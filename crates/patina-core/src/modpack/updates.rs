use super::{service::*, state::*, *};
use crate::{
    content::online::load_sources,
    instance::{Component, Instance},
};
use sha2::{Digest, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackDecision {
    KeepLocal,
    UsePack,
}
#[derive(Debug, Clone, Default)]
pub struct PackDecisions(BTreeMap<String, PackDecision>);
impl PackDecisions {
    pub fn set(&mut self, id: impl Into<String>, decision: PackDecision) {
        self.0.insert(id.into(), decision);
    }
    pub fn get(&self, id: &str) -> Option<PackDecision> {
        self.0.get(id).copied()
    }
    pub fn decision(&self, c: &PackChange) -> PackDecision {
        self.get(c.id()).unwrap_or_else(|| c.default_decision())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackChangeKind {
    Unchanged,
    FollowPack,
    Preserve,
    Conflict,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackConflictReason {
    BothChanged,
    DeletedLocally,
    RemovedByPack,
    NewPathCollision,
}
#[derive(Debug, Clone)]
pub struct PackChange {
    pub(super) id: String,
    pub(super) current_path: String,
    pub(super) target_path: Option<String>,
    pub(super) kind: PackChangeKind,
    pub(super) reason: Option<PackConflictReason>,
    pub(super) current: Option<String>,
    pub(super) target: Option<String>,
}
impl PackChange {
    pub(super) fn install(path: String, hash: String) -> Self {
        Self {
            id: path.clone(),
            current_path: path.clone(),
            target_path: Some(path),
            kind: PackChangeKind::FollowPack,
            reason: None,
            current: None,
            target: Some(hash),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn current_path(&self) -> &str {
        &self.current_path
    }
    pub fn target_path(&self) -> Option<&str> {
        self.target_path.as_deref()
    }
    pub fn kind(&self) -> PackChangeKind {
        self.kind
    }
    pub fn reason(&self) -> Option<PackConflictReason> {
        self.reason
    }
    pub fn is_pack_deletion(&self) -> bool {
        self.target.is_none()
    }
    pub fn default_decision(&self) -> PackDecision {
        match self.kind {
            PackChangeKind::Conflict | PackChangeKind::Preserve => PackDecision::KeepLocal,
            _ => PackDecision::UsePack,
        }
    }
}
/// A plan owns validated new content and the precise disk/metadata observations it depends on.
#[derive(Clone)]
pub struct PackUpdatePlan {
    pub(super) instance: Instance,
    pub(super) old: PackState,
    pub(super) target: PackState,
    pub(super) preview: PackPreview,
    pub(super) stage: Arc<Staging>,
    pub(super) changes: Vec<PackChange>,
    pub(super) observed: BTreeMap<String, Option<String>>,
    pub(super) metadata: BTreeMap<String, Option<Vec<u8>>>,
    pub(super) preserved: Vec<String>,
}
impl PackUpdatePlan {
    pub fn instance(&self) -> &Instance {
        &self.instance
    }
    pub fn old_state(&self) -> &PackState {
        &self.old
    }
    pub fn old_components(&self) -> &[Component] {
        &self.old.components
    }
    pub fn preview(&self) -> &PackPreview {
        &self.preview
    }
    pub fn changes(&self) -> &[PackChange] {
        &self.changes
    }
    pub fn preserved_paths(&self) -> &[String] {
        &self.preserved
    }
    pub fn optional_choices(&self) -> &BTreeMap<String, bool> {
        &self.target.optional
    }
    pub fn conflict_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| c.kind == PackChangeKind::Conflict)
            .count()
    }
    pub fn normal_change_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| c.kind == PackChangeKind::FollowPack)
            .count()
    }
}
impl PackService {
    pub async fn plan_update(
        &self,
        id: String,
        preview: PackPreview,
        optional: BTreeMap<String, bool>,
    ) -> Result<PackUpdatePlan, PackError> {
        let store = self.store.clone();
        let p = preview.clone();
        let (instance, old) = blocking(move || {
            let _g = store.lock(&id)?;
            let i = store.load(&id)?;
            crate::launch::running::refuse_while_running(&i.dir, &id)?;
            ensure_ready(&i.dir)?;
            let s = load_state(&i.dir)?.ok_or(PackError::NoBaseline)?;
            match (&s.source, &p.source) {
                (Some(a), Some(b)) if a.project() == b.project() => {}
                (None, None) => {}
                _ => return Err(PackError::SourceChanged),
            }
            Ok((i, s))
        })
        .await?;
        let mut choices = old.optional.clone();
        choices.extend(optional);
        for file in preview
            .files
            .iter()
            .filter(|f| f.optional && !choices.contains_key(&f.path))
            .collect::<Vec<_>>()
        {
            if let Some(source) = manifest_source(file) {
                let owned: Vec<_> = old
                    .files
                    .iter()
                    .filter(|(_, f)| {
                        f.source
                            .as_ref()
                            .is_some_and(|v| v.project() == source.project())
                    })
                    .collect();
                if owned.len() == 1
                    && let Some(choice) = old.optional.get(owned[0].0)
                {
                    choices.insert(file.path.clone(), *choice);
                }
            }
        }
        let (_tx, mut cancel) = tokio::sync::watch::channel(false);
        let (stage, target) = self
            .stage(
                &preview,
                &choices,
                Some(instance.game_dir()),
                &mut cancel,
                &None,
            )
            .await?;
        let store = self.store.clone();
        blocking(move || {
            let _g = store.lock(&instance.id)?;
            crate::launch::running::refuse_while_running(&instance.dir, &instance.id)?;
            ensure_ready(&instance.dir)?;
            if load_state(&instance.dir)?.as_ref() != Some(&old) {
                return Err(PackError::StalePlan);
            }
            let sources = load_sources(&instance.dir)?;
            let game = instance.game_dir();
            let mut observed = BTreeMap::new();
            let mut changes = Vec::new();
            let mut consumed = BTreeSet::new();
            let mut actual_paths = BTreeSet::new();
            for (path, baseline) in &old.files {
                let mut actual = old
                    .bindings
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| path.clone());
                if let Some(source) = &baseline.source {
                    let records: Vec<_> = sources
                        .records()
                        .iter()
                        .filter(|r| r.version().project() == source.project())
                        .collect();
                    if records.len() > 1 {
                        return Err(PackError::Collision(path.clone()));
                    }
                    if let Some(record) = records.first() {
                        let verified = checked_game_path(&game, record.path())?;
                        if crate::download::sha1_file(&verified).is_ok_and(|h| h == record.sha1()) {
                            actual = record.path().to_string();
                        }
                    }
                }
                if !actual_paths.insert(actual.clone()) {
                    return Err(PackError::Collision(actual));
                }
                let matches: Vec<_> = if let Some(file) = target.files.get(path) {
                    vec![(path, file)]
                } else {
                    target
                        .files
                        .iter()
                        .filter(|(_, f)| {
                            baseline.source.as_ref().is_some_and(|old| {
                                f.source
                                    .as_ref()
                                    .is_some_and(|new| old.project() == new.project())
                            })
                        })
                        .collect()
                };
                if matches.len() > 1 {
                    return Err(PackError::Collision(path.clone()));
                }
                let next = matches.first().map(|(p, f)| ((*p).clone(), (*f).clone()));
                if let Some((p, _)) = &next
                    && !consumed.insert(p.clone())
                {
                    return Err(PackError::Collision(p.clone()));
                }
                let current = observe(&game, &actual, &mut observed)?;
                if let Some((new_path, _)) = &next {
                    let at_target = observe(&game, new_path, &mut observed)?;
                    if new_path != &actual && at_target.is_some() {
                        return Err(PackError::Collision(new_path.clone()));
                    }
                }
                changes.push(classify(
                    path.clone(),
                    actual,
                    Some(&baseline.sha512),
                    current,
                    next,
                ));
            }
            for (path, f) in &target.files {
                if consumed.contains(path) {
                    continue;
                }
                let current = observe(&game, path, &mut observed)?;
                changes.push(classify(
                    path.clone(),
                    path.clone(),
                    None,
                    current,
                    Some((path.clone(), f.clone())),
                ));
            }
            let touched: BTreeSet<_> = changes
                .iter()
                .map(|c| c.current_path.clone())
                .chain(changes.iter().filter_map(|c| c.target_path.clone()))
                .collect();
            let mut all = Vec::new();
            walk_files(&game, &game, &mut all)?;
            let preserved = all
                .into_iter()
                .filter(|p| !touched.contains(p))
                .chain(
                    changes
                        .iter()
                        .filter(|c| c.kind == PackChangeKind::Preserve)
                        .map(|c| c.current_path.clone()),
                )
                .collect();
            let metadata = metadata_snapshot(&instance.dir)?;
            Ok(PackUpdatePlan {
                instance,
                old,
                target,
                preview,
                stage: Arc::new(stage),
                changes,
                observed,
                metadata,
                preserved,
            })
        })
        .await
    }
}
fn classify(
    id: String,
    current_path: String,
    old: Option<&String>,
    current: Option<String>,
    next: Option<(String, BaselineFile)>,
) -> PackChange {
    let target = next.as_ref().map(|(_, f)| f.sha512.clone());
    let target_path = next.map(|(p, _)| p);
    let (kind, reason) = if current == target {
        (PackChangeKind::Unchanged, None)
    } else if current.is_none() && old.is_some() && target.is_some() {
        (
            PackChangeKind::Conflict,
            Some(PackConflictReason::DeletedLocally),
        )
    } else if current.as_ref() == old {
        (PackChangeKind::FollowPack, None)
    } else if target.as_ref() == old {
        (PackChangeKind::Preserve, None)
    } else {
        (
            PackChangeKind::Conflict,
            Some(if current.is_none() {
                PackConflictReason::DeletedLocally
            } else if old.is_none() {
                PackConflictReason::NewPathCollision
            } else if target.is_none() {
                PackConflictReason::RemovedByPack
            } else {
                PackConflictReason::BothChanged
            }),
        )
    };
    PackChange {
        id,
        current_path,
        target_path,
        kind,
        reason,
        current,
        target,
    }
}
pub(super) fn fingerprint(path: &Path) -> Result<Option<String>, PackError> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => {
            let mut f = std::fs::File::open(path)?;
            let mut hash = Sha512::new();
            use std::io::Read;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let n = f.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            Ok(Some(hex::encode(hash.finalize())))
        }
        Ok(_) => Err(PackError::Collision(path.display().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn observe(
    game: &Path,
    p: &str,
    observed: &mut BTreeMap<String, Option<String>>,
) -> Result<Option<String>, PackError> {
    let value = fingerprint(&checked_game_path(game, p)?)?;
    observed.insert(p.into(), value.clone());
    Ok(value)
}
pub(super) fn metadata_snapshot(
    dir: &Path,
) -> Result<BTreeMap<String, Option<Vec<u8>>>, PackError> {
    let mut out = BTreeMap::new();
    for name in [
        "instance.toml",
        STATE_FILE,
        "content-sources.toml",
        PENDING_FILE,
    ] {
        let path = dir.join(name);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(PackError::UnsafePath(name.into()));
        }
        let bytes = match std::fs::read(path) {
            Ok(b) if b.len() <= 16 * 1024 * 1024 => Some(b),
            Ok(_) => return Err(PackError::Limit),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        out.insert(name.into(), bytes);
    }
    Ok(out)
}
fn walk_files(root: &Path, dir: &Path, files: &mut Vec<String>) -> Result<(), PackError> {
    for entry in std::fs::read_dir(dir)? {
        let e = entry?;
        let t = e.file_type()?;
        if files.len() >= 20_000 {
            return Err(PackError::Limit);
        }
        if t.is_dir() {
            walk_files(root, &e.path(), files)?
        } else if let Ok(p) = e.path().strip_prefix(root) {
            files.push(p.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}
