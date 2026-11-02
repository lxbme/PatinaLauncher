//! Dependency planning and staged application through the existing instance lock/downloader.
use super::{installed::*, *};
use crate::accounts::login::CancelToken;
use crate::download::{DownloadJob, Downloader, sha1_file};
use crate::instance::{Instance, InstanceStore};
use crate::progress::{Phase, ProgressSink, Unit};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error(transparent)]
    Online(#[from] OnlineError),
    #[error(transparent)]
    Instance(#[from] crate::instance::InstanceError),
    #[error(transparent)]
    Busy(#[from] crate::locks::Busy),
    #[error(transparent)]
    Download(#[from] crate::download::DownloadError),
    #[error("{}",rust_i18n::t!("errors.content_online.io",error=.0))]
    Io(String),
    #[error("{}",rust_i18n::t!("errors.content_online.invalid_sources"))]
    InvalidSources,
    #[error("{}",rust_i18n::t!("errors.content_online.invalid_metadata"))]
    InvalidMetadata,
    #[error("{}",rust_i18n::t!("errors.content_online.target_changed"))]
    TargetChanged,
    #[error("{}",rust_i18n::t!("errors.content_online.unsupported"))]
    Unsupported,
    #[error("{}",rust_i18n::t!("errors.content_online.cancelled"))]
    Cancelled,
    #[error("{}",rust_i18n::t!("errors.content_online.no_compatible",project=.0))]
    NoCompatible(String),
    #[error("{}",rust_i18n::t!("errors.content_online.incompatible",project=.0))]
    Incompatible(String),
    #[error("{}",rust_i18n::t!("errors.content_online.cycle",project=.0))]
    Cycle(String),
    #[error("{}",rust_i18n::t!("errors.content_online.conflict",path=.0))]
    Conflict(String),
    #[error("{}",rust_i18n::t!("errors.content_online.manual_required",file=.0))]
    ManualRequired(String),
    #[error("{}",rust_i18n::t!("errors.content_online.verification",file=.0))]
    Verification(String),
}
impl From<std::io::Error> for ContentError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
#[derive(Clone)]
pub struct ContentService {
    pub(super) store: InstanceStore,
    pub(super) source: Arc<dyn ContentSource>,
    downloader: Arc<Downloader>,
}
impl ContentService {
    pub fn new(
        store: InstanceStore,
        source: Arc<dyn ContentSource>,
        downloader: Arc<Downloader>,
    ) -> Self {
        Self {
            store,
            source,
            downloader,
        }
    }
    /// Optional decoration uses the credential-free download client, never the CF API client.
    pub async fn fetch_icon(&self, url: String) -> Option<Vec<u8>> {
        use futures::StreamExt;
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return None;
        }
        let response = self
            .downloader
            .client()
            .get(url)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        if response.content_length().is_some_and(|n| n > 256 * 1024) {
            return None;
        }
        let mut stream = response.bytes_stream();
        let mut bytes = vec![];
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            if bytes.len() + chunk.len() > 256 * 1024 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        Some(bytes)
    }
    pub async fn validate_manual_file(
        file: ContentFile,
        path: PathBuf,
    ) -> Result<(), ContentError> {
        blocking(move || verify_file(&path, &file)).await
    }
    pub async fn plan_install(
        &self,
        request: InstallRequest,
    ) -> Result<ContentInstallPlan, ContentError> {
        folder(request.target.kind())?;
        let store = self.store.clone();
        let req = request.clone();
        let (sources, instance_loader) = blocking(move || {
            let _guard = store.lock(&req.instance)?;
            let instance = store.load(&req.instance)?;
            check_target(&instance, &req.target)?;
            crate::launch::running::refuse_while_running(&instance.dir, &instance.id)?;
            Ok((
                load_sources(&instance.dir)?,
                instance.config.loader().map(|(kind, _)| kind),
            ))
        })
        .await?;
        let root = self
            .source
            .version(
                request.version.project().platform(),
                request.version.id().to_string(),
            )
            .await?;
        if root.reference() != &request.version {
            return Err(ContentError::InvalidMetadata);
        }
        let mut queue = VecDeque::from([(
            root,
            Vec::<ProjectRef>::new(),
            Vec::<ProjectRef>::new(),
            false,
        )]);
        let mut items: Vec<PlannedContent> = vec![];
        let mut incompatible = vec![];
        let mut unavailable = vec![];
        while let Some((version, conditions, mut ancestors, dependency)) = queue.pop_front() {
            let project_ref = version.reference().project().clone();
            if ancestors.contains(&project_ref) {
                return Err(ContentError::Cycle(project_ref.id().into()));
            }
            if ancestors.len() >= 64 || items.len() >= 256 {
                return Err(ContentError::InvalidMetadata);
            }
            let existing_ix = items
                .iter()
                .position(|i| i.version.reference().project() == &project_ref);
            if let Some(ix) = existing_ix {
                let existing = &mut items[ix];
                if existing.version.reference() != version.reference() {
                    return Err(ContentError::Incompatible(project_ref.id().into()));
                }
                if existing.conditions.contains(&conditions) {
                    continue;
                }
                existing.conditions.push(conditions.clone());
            }
            let project = self.source.project(project_ref.clone()).await?;
            if project.reference() != &project_ref {
                return Err(ContentError::InvalidMetadata);
            }
            if !project.is_client_supported() {
                if !conditions.is_empty() {
                    unavailable.push(super::plan::UnavailableDependency {
                        conditions,
                        project: project_ref.id().into(),
                    });
                    continue;
                }
                return Err(ContentError::NoCompatible(project_ref.id().into()));
            }
            if !dependency && project.kind() != request.target.kind() {
                return Err(ContentError::InvalidMetadata);
            }
            let compatibility = TargetCompatibility::new(
                project.kind(),
                request.target.mc_version(),
                instance_loader,
            )
            .with_channels(request.target.channels().to_vec());
            if !compatibility.accepts(&version) {
                if !conditions.is_empty() {
                    unavailable.push(super::plan::UnavailableDependency {
                        conditions,
                        project: project_ref.id().into(),
                    });
                    continue;
                }
                return Err(ContentError::NoCompatible(project_ref.id().into()));
            }
            let file = version
                .files()
                .iter()
                .find(|f| f.is_primary())
                .or_else(|| version.files().first())
                .ok_or(ContentError::InvalidMetadata)?
                .clone();
            let name = file.name().to_ascii_lowercase();
            let extension_ok = if project.kind() == ContentKind::Mod {
                name.ends_with(".jar")
            } else {
                name.ends_with(".zip")
            };
            if !extension_ok || !crate::fsutil::is_safe_name(file.name()) {
                return Err(ContentError::InvalidMetadata);
            }
            let destination = format!("{}/{}", folder(project.kind())?, file.name());
            ancestors.push(project_ref.clone());
            for dependency in version.dependencies() {
                if dependency.relation() == DependencyRelation::Embedded {
                    continue;
                }
                if dependency.relation() == DependencyRelation::Incompatible {
                    if let Some(project) = dependency.project() {
                        incompatible.push(super::plan::Incompatibility {
                            project: project.clone(),
                            conditions: conditions.clone(),
                            installed: false,
                        });
                    } else if let Some(id) = dependency.version_id() {
                        let v = self
                            .source
                            .version(project_ref.platform(), id.into())
                            .await?;
                        incompatible.push(super::plan::Incompatibility {
                            project: v.reference().project().clone(),
                            conditions: conditions.clone(),
                            installed: false,
                        });
                    } else {
                        return Err(ContentError::InvalidMetadata);
                    }
                    continue;
                }
                let mut activation = conditions.clone();
                let optional = dependency.relation() == DependencyRelation::Optional;
                // A project-only recommendation back to a validated ancestor is already
                // satisfied by that selected version, not a new optional activation path.
                // Explicit version constraints and required cycles still go through validation.
                if optional
                    && dependency.version_id().is_none()
                    && dependency.project().is_some_and(|p| ancestors.contains(p))
                {
                    continue;
                }
                let candidate = if let Some(id) = dependency.version_id() {
                    let v = self
                        .source
                        .version(project_ref.platform(), id.into())
                        .await?;
                    if dependency
                        .project()
                        .is_some_and(|p| p != v.reference().project())
                    {
                        return Err(ContentError::InvalidMetadata);
                    }
                    Some(v)
                } else if let Some(reference) = dependency.project() {
                    let dep_project = self.source.project(reference.clone()).await?;
                    let target = TargetCompatibility::new(
                        dep_project.kind(),
                        request.target.mc_version(),
                        instance_loader,
                    )
                    .with_channels(request.target.channels().to_vec());
                    let mut candidates = self
                        .source
                        .versions(reference.clone(), target.clone())
                        .await?;
                    candidates.retain(|v| target.accepts(v));
                    candidates.sort_by_key(|v| std::cmp::Reverse(v.published()));
                    // Keep an already installed compatible dependency instead of implicitly updating it.
                    let installed = sources
                        .records()
                        .iter()
                        .find(|r| r.version.project() == reference);
                    let reused = installed
                        .and_then(|r| candidates.iter().find(|v| v.reference() == &r.version))
                        .cloned();
                    reused.or_else(|| candidates.into_iter().next())
                } else {
                    return Err(ContentError::InvalidMetadata);
                };
                let Some(candidate) = candidate else {
                    if optional {
                        continue;
                    }
                    let missing = dependency.project().map_or("?", ProjectRef::id).to_string();
                    if !conditions.is_empty() {
                        unavailable.push(super::plan::UnavailableDependency {
                            conditions: conditions.clone(),
                            project: missing,
                        });
                        continue;
                    }
                    return Err(ContentError::NoCompatible(missing));
                };
                if optional {
                    activation.push(candidate.reference().project().clone());
                }
                queue.push_back((candidate, activation, ancestors.clone(), true));
            }
            if existing_ix.is_some() {
                continue;
            }
            items.push(PlannedContent {
                title: project.title().to_string(),
                version,
                kind: project.kind(),
                file,
                destination,
                old: None,
                reusable: false,
                conditions: vec![conditions],
                dependency,
            });
        }
        let store = self.store.clone();
        blocking(move || {
            let _guard = store.lock(&request.instance)?;
            let instance = store.load(&request.instance)?;
            check_target(&instance, &request.target)?;
            if instance.config.loader().map(|(kind, _)| kind) != instance_loader {
                return Err(ContentError::TargetChanged);
            }
            if load_sources(&instance.dir)? != sources {
                return Err(ContentError::TargetChanged);
            }
            let game = instance.game_dir();
            let mut observed = HashMap::new();
            let mut conflicts = vec![];
            let mut destinations = HashSet::new();
            for item in &mut items {
                let old = sources
                    .records()
                    .iter()
                    .find(|r| r.version.project() == item.version.reference().project())
                    .map(|r| effective_record(&game, r));
                let disabled = old.as_ref().is_some_and(|r| r.path.ends_with(".disabled"));
                if disabled {
                    item.destination.push_str(".disabled");
                }
                if !destinations.insert(item.destination.clone()) {
                    return Err(ContentError::InvalidMetadata);
                }
                let destination = observe(&game, &item.destination, &mut observed)?;
                if let Some(old) = &old {
                    let fingerprint = observe(&game, &old.path, &mut observed)?;
                    item.reusable = old.version == *item.version.reference()
                        && fingerprint.as_deref() == Some(old.sha1.as_str());
                    if fingerprint.is_some() && fingerprint.as_deref() != Some(old.sha1.as_str()) {
                        conflicts.push(ContentConflict {
                            project: old.version.project().clone(),
                            path: old.path.clone(),
                            reason: ConflictReason::Modified,
                        });
                    }
                    if disabled && item.dependency {
                        conflicts.push(ContentConflict {
                            project: old.version.project().clone(),
                            path: old.path.clone(),
                            reason: ConflictReason::DisabledRequired,
                        });
                    }
                }
                if destination.is_some() && old.as_ref().is_none_or(|r| r.path != item.destination)
                {
                    conflicts.push(ContentConflict {
                        project: item.version.reference().project().clone(),
                        path: item.destination.clone(),
                        reason: ConflictReason::ForeignFile,
                    });
                }
                item.old = old;
            }
            for conflict in &mut incompatible {
                conflict.installed = sources
                    .records()
                    .iter()
                    .filter(|r| r.version.project() == &conflict.project)
                    .map(|r| effective_record(&game, r))
                    .any(|r| !r.path.ends_with(".disabled") && game.join(r.path).is_file());
                if conflict.conditions.is_empty()
                    && (conflict.installed
                        || items.iter().any(|i| {
                            i.version.reference().project() == &conflict.project && !i.is_optional()
                        }))
                {
                    return Err(ContentError::Incompatible(conflict.project.id().into()));
                }
            }
            Ok(ContentInstallPlan {
                incompatibilities: incompatible,
                unavailable,
                loader: instance_loader,
                request,
                items,
                sources,
                observed,
                conflicts,
            })
        })
        .await
    }

    pub async fn apply(
        &self,
        plan: ContentInstallPlan,
        choices: InstallChoices,
        mut cancel: CancelToken,
        progress: ProgressSink,
    ) -> Result<InstallOutcome, ContentError> {
        if *cancel.borrow() || cancel.has_changed().is_err() {
            return Err(ContentError::Cancelled);
        }
        for missing in &plan.unavailable {
            if missing
                .conditions
                .iter()
                .all(|p| choices.optional.contains(p))
            {
                return Err(ContentError::NoCompatible(missing.project.clone()));
            }
        }
        for conflict in &plan.incompatibilities {
            if conflict
                .conditions
                .iter()
                .all(|p| choices.optional.contains(p))
                && (conflict.installed
                    || plan.items.iter().any(|i| {
                        i.version.reference().project() == &conflict.project
                            && i.is_selected(&choices)
                    }))
            {
                return Err(ContentError::Incompatible(conflict.project.id().into()));
            }
        }
        for conflict in &plan.conflicts {
            if !plan.items.iter().any(|i| {
                i.version.reference().project() == &conflict.project && i.is_selected(&choices)
            }) {
                continue;
            }
            let approved = if conflict.reason == ConflictReason::DisabledRequired {
                choices.enable.contains(&conflict.project)
            } else {
                choices.replace.contains(&conflict.path)
            };
            if !approved {
                return Err(ContentError::Conflict(conflict.path.clone()));
            }
        }
        let selected: Vec<_> = plan
            .items
            .iter()
            .filter(|i| i.is_selected(&choices))
            .cloned()
            .collect();
        for item in &selected {
            if !item.reusable
                && item.file.download_url().is_none()
                && !choices.manual.contains_key(item.version.reference())
            {
                return Err(ContentError::ManualRequired(item.file.name().into()));
            }
        }
        let store = self.store.clone();
        let request = plan.request.clone();
        let expected_loader = plan.loader;
        let (guard, instance, mut staging) = blocking(move || {
            let guard = store.lock(&request.instance)?;
            let instance = store.load(&request.instance)?;
            check_target(&instance, &request.target)?;
            if instance.config.loader().map(|(kind, _)| kind) != expected_loader {
                return Err(ContentError::TargetChanged);
            }
            crate::launch::running::refuse_while_running(&instance.dir, &instance.id)?;
            let path =
                crate::fsutil::unique_sibling(&instance.dir.join(".content-install"), ".tmp");
            std::fs::create_dir(&path)?;
            Ok((guard, instance, Staging { path, keep: false }))
        })
        .await?;
        let mut jobs = vec![];
        for (ix, item) in selected.iter().enumerate() {
            if item.reusable {
                continue;
            }
            let destination = staging.path.join(ix.to_string());
            if let Some(manual) = choices.manual.get(item.version.reference()) {
                let manual = manual.clone();
                let file = item.file.clone();
                blocking(move || {
                    if !std::fs::metadata(&manual)?.is_file() {
                        return Err(ContentError::Verification(file.name().into()));
                    }
                    std::fs::copy(manual, &destination)?;
                    verify_file(&destination, &file)
                })
                .await?;
            } else {
                let url = item
                    .file
                    .download_url()
                    .ok_or_else(|| ContentError::ManualRequired(item.file.name().into()))?;
                if !url.starts_with("https://") && !url.starts_with("http://") {
                    return Err(ContentError::InvalidMetadata);
                }
                jobs.push(DownloadJob {
                    url: url.into(),
                    dest: destination,
                    sha1: item.file.hashes().sha1().map(str::to_owned),
                    size: Some(item.file.size()),
                });
            }
        }
        {
            let download = crate::progress::run_downloads(
                &self.downloader,
                jobs,
                &progress,
                Phase::DownloadContent,
                Unit::Bytes,
                0,
            );
            tokio::pin!(download);
            tokio::select! { result=&mut download=>result?, _=async { loop { if cancel.changed().await.is_err() || *cancel.borrow() { break; } } }=>return Err(ContentError::Cancelled) }
        }
        if *cancel.borrow() || cancel.has_changed().is_err() {
            return Err(ContentError::Cancelled);
        }
        blocking(move || {
            let _guard = guard;
            let current = self_store_load(&instance)?;
            check_target(&current, &plan.request.target)?;
            if current.config.loader().map(|(kind, _)| kind) != plan.loader {
                return Err(ContentError::TargetChanged);
            }
            crate::launch::running::refuse_while_running(&instance.dir, &instance.id)?;
            if load_sources(&instance.dir)? != plan.sources {
                return Err(ContentError::TargetChanged);
            }
            let game = instance.game_dir();
            for (path, expected) in &plan.observed {
                if fingerprint(&checked_path(&game, path)?)? != *expected {
                    return Err(ContentError::TargetChanged);
                }
            }
            for (ix, item) in selected.iter().enumerate() {
                if !item.reusable {
                    verify_file(&staging.path.join(ix.to_string()), &item.file)?;
                }
            }
            if *cancel.borrow() || cancel.has_changed().is_err() {
                return Err(ContentError::Cancelled);
            }
            crate::progress::report(
                &progress,
                Phase::Finalize,
                0,
                selected.len() as u64,
                Unit::Items,
            );
            let result = commit(&instance, &plan, &selected, &choices, &mut staging);
            if result.is_ok() {
                crate::progress::report(
                    &progress,
                    Phase::Finalize,
                    selected.len() as u64,
                    selected.len() as u64,
                    Unit::Items,
                );
            }
            result
        })
        .await
    }
}
fn self_store_load(instance: &Instance) -> Result<Instance, ContentError> {
    let parent = instance.dir.parent().ok_or(ContentError::InvalidMetadata)?;
    Ok(InstanceStore::new(parent).load(&instance.id)?)
}
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ContentError> + Send + 'static,
) -> Result<T, ContentError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| ContentError::Io("content worker failed".into()))?
}
pub(super) fn check_target(
    instance: &Instance,
    target: &TargetCompatibility,
) -> Result<(), ContentError> {
    if instance.config.mc_version() != target.mc_version()
        || (target.kind() == ContentKind::Mod
            && instance.config.loader().map(|(l, _)| l) != target.loader())
    {
        return Err(ContentError::TargetChanged);
    }
    Ok(())
}
pub(super) fn checked_path(game: &Path, relative: &str) -> Result<PathBuf, ContentError> {
    if !crate::fsutil::is_safe_rel(relative) || relative.split('/').count() != 2 {
        return Err(ContentError::InvalidMetadata);
    }
    let mut path = game.to_path_buf();
    for component in std::iter::once(None).chain(relative.split('/').map(Some)) {
        if let Some(component) = component {
            path.push(component);
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&path)
            && metadata.file_type().is_symlink()
        {
            return Err(ContentError::Conflict(relative.into()));
        }
    }
    Ok(path)
}
fn fingerprint(path: &Path) -> Result<Option<String>, ContentError> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => Ok(Some(sha1_file(path)?)),
        Ok(_) => Err(ContentError::Conflict(path.display().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn observe(
    game: &Path,
    path: &str,
    observed: &mut HashMap<String, Option<String>>,
) -> Result<Option<String>, ContentError> {
    let value = fingerprint(&checked_path(game, path)?)?;
    observed.insert(path.into(), value.clone());
    Ok(value)
}
fn verify_file(path: &Path, file: &ContentFile) -> Result<(), ContentError> {
    use sha1::Digest;
    if std::fs::metadata(path)?.len() != file.size() {
        return Err(ContentError::Verification(file.name().into()));
    }
    let hashes = file.hashes();
    if hashes.sha1().is_none() && hashes.sha512().is_none() && hashes.md5().is_none() {
        return Err(ContentError::Verification(file.name().into()));
    }
    if let Some(expected) = hashes.sha1()
        && !sha1_file(path)?.eq_ignore_ascii_case(expected)
    {
        return Err(ContentError::Verification(file.name().into()));
    }
    if hashes.sha512().is_some() || hashes.md5().is_some() {
        use std::io::Read;
        let mut sha512 = sha2::Sha512::new();
        let mut md5 = md5::Md5::new();
        let mut input = std::fs::File::open(path)?;
        let mut buffer = [0; 65536];
        loop {
            let len = input.read(&mut buffer)?;
            if len == 0 {
                break;
            }
            sha512.update(&buffer[..len]);
            md5.update(&buffer[..len]);
        }
        if hashes
            .sha512()
            .is_some_and(|s| !hex::encode(sha512.finalize()).eq_ignore_ascii_case(s))
            || hashes
                .md5()
                .is_some_and(|s| !hex::encode(md5.finalize()).eq_ignore_ascii_case(s))
        {
            return Err(ContentError::Verification(file.name().into()));
        }
    }
    Ok(())
}
struct Staging {
    path: PathBuf,
    keep: bool,
}
impl Drop for Staging {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
fn commit(
    instance: &Instance,
    plan: &ContentInstallPlan,
    items: &[PlannedContent],
    choices: &InstallChoices,
    staging: &mut Staging,
) -> Result<InstallOutcome, ContentError> {
    let game = instance.game_dir();
    let mut sources = plan.sources.clone();
    for record in &mut sources.records {
        *record = effective_record(&game, record);
    }
    let mut backups: Vec<(PathBuf, PathBuf)> = vec![];
    let mut written = vec![];
    let mut applied = vec![];
    let result = (|| {
        let mut backed = HashSet::new();
        for (ix, item) in items.iter().enumerate() {
            let enable =
                item.dependency && choices.enable.contains(item.version.reference().project());
            let destination = if enable {
                item.destination
                    .strip_suffix(".disabled")
                    .unwrap_or(&item.destination)
                    .to_string()
            } else {
                item.destination.clone()
            };
            let dest = checked_path(&game, &destination)?;
            if destination != item.destination && dest.exists() {
                return Err(ContentError::Conflict(destination));
            }
            if item.reusable && !enable {
                if let Some(old) = &item.old {
                    applied.push(old.clone());
                }
                continue;
            }
            for relative in item
                .old
                .iter()
                .map(|r| r.path.as_str())
                .chain(std::iter::once(destination.as_str()))
            {
                let path = checked_path(&game, relative)?;
                if path.exists() && backed.insert(path.clone()) {
                    let backup = staging.path.join(format!("backup-{}", backups.len()));
                    std::fs::rename(&path, &backup)?;
                    backups.push((path, backup));
                }
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if item.reusable {
                let old = item.old.as_ref().ok_or(ContentError::InvalidMetadata)?;
                let backup = backups
                    .iter()
                    .find(|(path, _)| path == &game.join(&old.path))
                    .ok_or(ContentError::InvalidMetadata)?;
                std::fs::copy(&backup.1, &dest)?;
            } else {
                std::fs::rename(staging.path.join(ix.to_string()), &dest)?;
            }
            written.push(dest.clone());
            applied.push(SourceRecord {
                version: item.version.reference().clone(),
                kind: item.kind,
                path: destination,
                sha1: sha1_file(&dest)?,
            });
        }
        for record in &applied {
            sources
                .records
                // An explicitly approved path overwrite also displaces that path's prior owner.
                .retain(|r| {
                    r.version.project() != record.version.project() && r.path != record.path
                });
            sources.records.push(record.clone());
        }
        save_sources(&instance.dir, &sources)?;
        Ok(())
    })();
    if let Err(error) = result {
        for path in written.iter().rev() {
            let _ = std::fs::remove_file(path);
        }
        for (path, backup) in backups.iter().rev() {
            if let Err(restore) = std::fs::rename(backup, path) {
                staging.keep = true;
                return Err(ContentError::Io(format!(
                    "{error}; restore failed: {restore}; backup: {}",
                    staging.path.display()
                )));
            }
        }
        return Err(error);
    }
    Ok(InstallOutcome { applied })
}
