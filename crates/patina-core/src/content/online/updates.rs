//! User-triggered local identification and same-project compatible updates.
use super::{installed::*, *};
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct UpdateSelection {
    platform: Platform,
    kind: ContentKind,
    paths: Option<HashSet<String>>,
    channels: Vec<ReleaseChannel>,
}
impl UpdateSelection {
    pub fn new(platform: Platform, kind: ContentKind) -> Self {
        Self {
            platform,
            kind,
            paths: None,
            channels: vec![ReleaseChannel::Release],
        }
    }
    pub fn with_paths(mut self, paths: Vec<String>) -> Self {
        self.paths = Some(paths.into_iter().collect());
        self
    }
    pub fn with_channels(mut self, channels: Vec<ReleaseChannel>) -> Self {
        self.channels = channels;
        self
    }
}
#[derive(Debug, Clone)]
pub struct ContentUpdate {
    installed: SourceRecord,
    candidate: ContentVersion,
    request: InstallRequest,
    modified: bool,
}
impl ContentUpdate {
    pub fn installed(&self) -> &SourceRecord {
        &self.installed
    }
    pub fn candidate(&self) -> &ContentVersion {
        &self.candidate
    }
    pub fn request(&self) -> &InstallRequest {
        &self.request
    }
    pub fn is_modified(&self) -> bool {
        self.modified
    }
}
#[derive(Debug, Clone)]
pub struct ContentUpdatePlan {
    updates: Vec<ContentUpdate>,
    unknown: Vec<String>,
}
impl ContentUpdatePlan {
    pub fn updates(&self) -> &[ContentUpdate] {
        &self.updates
    }
    pub fn unknown_files(&self) -> &[String] {
        &self.unknown
    }
}
impl ContentService {
    pub async fn check_updates(
        &self,
        instance_id: String,
        selection: UpdateSelection,
    ) -> Result<ContentUpdatePlan, ContentError> {
        let store = self.store.clone();
        let scan = selection.clone();
        let (instance, original, mut sources, files) = super::service::blocking(move || {
            let _guard = store.lock(&instance_id)?;
            let instance = store.load(&instance_id)?;
            let original = load_sources(&instance.dir)?;
            let mut sources = original.clone();
            let game = instance.game_dir();
            for record in &mut sources.records {
                *record = effective_record(&game, record);
            }
            let directory = folder(scan.kind)?;
            let mut files = vec![];
            let entries = match std::fs::read_dir(game.join(directory)) {
                Ok(entries) => Some(entries),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            for entry in entries.into_iter().flatten() {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let lower = name.to_ascii_lowercase();
                if !(if scan.kind == ContentKind::Mod {
                    lower.ends_with(".jar") || lower.ends_with(".jar.disabled")
                } else {
                    lower.ends_with(".zip")
                }) {
                    continue;
                }
                let relative = format!("{directory}/{name}");
                if scan
                    .paths
                    .as_ref()
                    .is_some_and(|paths| !paths.contains(&relative))
                {
                    continue;
                }
                let path = super::service::checked_path(&game, &relative)?;
                let bytes = crate::content::read_file_capped(&path, 512 * 1024 * 1024)
                    .ok_or_else(|| ContentError::Verification(name.clone()))?;
                files.push((relative, FileIdentity::from_bytes(name, &bytes)));
            }
            files.sort_by(|a, b| a.0.cmp(&b.0));
            Ok((instance, original, sources, files))
        })
        .await?;
        let unknown: Vec<_> = files
            .iter()
            .filter(|(path, _)| !sources.records.iter().any(|r| r.path == *path))
            .cloned()
            .collect();
        let mut unrecognized = vec![];
        if !unknown.is_empty() {
            let identified = self
                .source
                .identify(
                    selection.platform,
                    unknown.iter().map(|(_, id)| id.clone()).collect(),
                )
                .await?;
            for (path, identity) in &unknown {
                let matched = identified
                    .iter()
                    .find(|f| f.identity() == identity)
                    .filter(|f| f.matches().len() == 1)
                    .and_then(|f| f.matches().first());
                let Some(version) = matched.filter(|v| {
                    v.reference().project().platform() == selection.platform
                        && v.files().iter().any(|f| {
                            f.hashes()
                                .sha1()
                                .is_some_and(|hash| hash.eq_ignore_ascii_case(identity.sha1()))
                        })
                }) else {
                    unrecognized.push(path.clone());
                    continue;
                };
                if sources
                    .records
                    .iter()
                    .any(|r| r.version.project() == version.reference().project())
                {
                    unrecognized.push(path.clone());
                    continue;
                }
                let project = self
                    .source
                    .project(version.reference().project().clone())
                    .await?;
                if project.kind() != selection.kind {
                    unrecognized.push(path.clone());
                    continue;
                }
                sources.records.push(SourceRecord {
                    version: version.reference().clone(),
                    kind: selection.kind,
                    path: path.clone(),
                    sha1: identity.sha1().into(),
                });
            }
        }
        let target = TargetCompatibility::new(
            selection.kind,
            instance.config.mc_version(),
            instance.config.loader().map(|(kind, _)| kind),
        )
        .with_channels(selection.channels);
        let mut updates = vec![];
        for record in sources.records.iter().filter(|r| r.kind == selection.kind) {
            let Some((_, identity)) = files.iter().find(|(path, _)| path == &record.path) else {
                continue;
            };
            let current = self
                .source
                .version(
                    record.version.project().platform(),
                    record.version.id().into(),
                )
                .await?;
            if current.reference() != &record.version {
                return Err(ContentError::InvalidMetadata);
            }
            let mut candidates = self
                .source
                .versions(record.version.project().clone(), target.clone())
                .await?;
            candidates.retain(|v| {
                v.reference().project() == record.version.project()
                    && v.reference() != &record.version
                    && v.published() > current.published()
                    && target.accepts(v)
            });
            candidates.sort_by_key(|v| std::cmp::Reverse(v.published()));
            if let Some(candidate) = candidates.into_iter().next() {
                updates.push(ContentUpdate {
                    installed: record.clone(),
                    request: InstallRequest::new(
                        &instance.id,
                        candidate.reference().clone(),
                        target.clone(),
                    ),
                    candidate,
                    modified: !record.sha1.eq_ignore_ascii_case(identity.sha1()),
                });
            }
        }
        if sources != original {
            let store = self.store.clone();
            let id = instance.id.clone();
            let expected = target.clone();
            super::service::blocking(move || {
                let _guard = store.lock(&id)?;
                let instance = store.load(&id)?;
                super::service::check_target(&instance, &expected)?;
                if load_sources(&instance.dir)? != original {
                    return Err(ContentError::TargetChanged);
                }
                for (path, identity) in files {
                    if crate::download::sha1_file(&super::service::checked_path(
                        &instance.game_dir(),
                        &path,
                    )?)? != identity.sha1()
                    {
                        return Err(ContentError::TargetChanged);
                    }
                }
                save_sources(&instance.dir, &sources)
            })
            .await?;
        }
        Ok(ContentUpdatePlan {
            updates,
            unknown: unrecognized,
        })
    }
}
