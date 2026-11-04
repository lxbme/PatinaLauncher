use super::{state::*, *};
use crate::{
    accounts::login::CancelToken,
    config::GlobalConfig,
    content::online::{ContentKind, ContentVersion, Platform, ProjectRef, VersionRef},
    instance::{Component, ComponentUid, Instance, InstanceStore},
    prepare::Launcher,
    progress::{Phase, ProgressSink, Unit, report},
};
use sha1::{Digest, Sha1};
use sha2::Sha512;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone)]
pub struct PackInstallChoices {
    pub(super) name: String,
    pub(super) optional: BTreeMap<String, bool>,
}
impl PackInstallChoices {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            optional: BTreeMap::new(),
        }
    }
    pub fn with_optional(mut self, path: impl Into<String>, selected: bool) -> Self {
        self.optional.insert(path.into(), selected);
        self
    }
}
#[derive(Debug, Clone)]
pub struct PackOutcome {
    pub(super) instance: Instance,
    pub(super) retained: usize,
}
impl PackOutcome {
    pub fn instance(&self) -> &Instance {
        &self.instance
    }
    pub fn retained_count(&self) -> usize {
        self.retained
    }
}
#[derive(Clone)]
pub struct PackService {
    pub(super) launcher: Arc<Launcher>,
    pub(super) global: GlobalConfig,
    pub(super) store: InstanceStore,
    downloads: Result<Arc<crate::download::Downloader>, String>,
}
/// Only disposable pre-apply downloads. Recovery material is stored separately inside the instance.
pub(super) struct Staging {
    pub(super) path: PathBuf,
}
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
impl PackService {
    pub fn new(launcher: Arc<Launcher>, global: GlobalConfig) -> Self {
        let store = InstanceStore::new(launcher.data.instances());
        let downloads = pack_http_client()
            .map(|client| {
                Arc::new(
                    crate::download::Downloader::new(client, crate::download::Mirror::official())
                        .with_transfer_settings(&launcher.downloader),
                )
            })
            .map_err(|e| e.to_string());
        Self {
            launcher,
            global,
            store,
            downloads,
        }
    }
    pub async fn preview_online(
        &self,
        version: ContentVersion,
        mut cancel: CancelToken,
        progress: ProgressSink,
    ) -> Result<PackPreview, PackError> {
        if version.reference().project().platform() != Platform::Modrinth {
            return Err(PackError::Unsupported);
        }
        let file = version
            .files()
            .iter()
            .find(|f| f.is_primary() && f.name().ends_with(".mrpack"))
            .ok_or(PackError::Unsupported)?;
        let url = file.download_url().ok_or(PackError::Invalid)?;
        if !archive::valid_download_url(url) {
            return Err(PackError::Invalid);
        }
        let sha1 = file
            .hashes()
            .sha1()
            .filter(|hash| archive::valid_hash(hash, 40))
            .ok_or(PackError::Invalid)?;
        let dest = self
            .launcher
            .data
            .cache()
            .join("modpacks/archives")
            .join(sha1.to_ascii_lowercase());
        report(
            &progress,
            Phase::DownloadPackArchive,
            0,
            file.size(),
            Unit::Bytes,
        );
        let archive_started = std::time::Instant::now();
        let job = crate::download::DownloadJob {
            url: self.download_url(url)?,
            dest: dest.clone(),
            sha1: file.hashes().sha1().map(str::to_string),
            size: Some(file.size()),
        };
        let download = crate::progress::run_bounded_downloads(
            self.downloads()?,
            vec![(job, Vec::new())],
            &progress,
            Phase::DownloadPackArchive,
            (256 * 1024 * 1024, 256 * 1024 * 1024),
        );
        tokio::pin!(download);
        tokio::select! {result=&mut download=>result.map_err(|e|PackError::Io(e.to_string()))?,_=cancelled(&mut cancel)=>return Err(PackError::Cancelled)};
        tracing::info!(
            elapsed_ms = archive_started.elapsed().as_millis(),
            "pack archive transfer/cache and SHA1 check complete"
        );
        let inspect_started = std::time::Instant::now();
        report(&progress, Phase::InspectPackArchive, 0, 1, Unit::Items);
        let hashes = file.hashes().clone();
        let reference = version.reference().clone();
        let preview = blocking(move || {
            let b = std::fs::read(dest)?;
            if hashes
                .sha1()
                .is_none_or(|h| !hex::encode(Sha1::digest(&b)).eq_ignore_ascii_case(h))
                || hashes
                    .sha512()
                    .is_some_and(|h| !hex::encode(Sha512::digest(&b)).eq_ignore_ascii_case(h))
            {
                return Err(PackError::Verification);
            }
            let mut p = inspect_bytes(b, ArchiveLimits::default())?;
            p.source = Some(reference);
            Ok(p)
        })
        .await?;
        tracing::info!(
            elapsed_ms = inspect_started.elapsed().as_millis(),
            files = preview.files().len(),
            overrides = preview.overrides().len(),
            "pack archive SHA512/decompression/inspection complete"
        );
        report(&progress, Phase::InspectPackArchive, 1, 1, Unit::Items);
        Ok(preview)
    }
    pub async fn install(
        &self,
        preview: PackPreview,
        choices: PackInstallChoices,
        cancel: CancelToken,
        progress: ProgressSink,
    ) -> Result<PackOutcome, PackError> {
        check_cancel(&cancel)?;
        if choices.name.trim().is_empty() {
            return Err(PackError::NameConflict);
        }
        let store = self.store.clone();
        let name = choices.name.clone();
        let mc = preview.mc.clone();
        let pending = state_for(
            &preview,
            choices.optional.clone(),
            Vec::new(),
            BTreeMap::new(),
            false,
        );
        let instance = blocking(move || {
            if store
                .list()?
                .instances
                .iter()
                .any(|i| i.config.name == name)
                || store
                    .load(&crate::instance::sanitize_dir_name(&name))
                    .is_ok()
            {
                return Err(PackError::NameConflict);
            }
            let i = store.create(&name, &mc)?;
            crate::fsutil::write_atomic(&i.dir.join(PENDING_FILE), b"installing")?;
            save_state(&i.dir, &pending)?;
            Ok(i)
        })
        .await?;
        self.install_into(instance, preview, choices, cancel, progress)
            .await
    }
    pub async fn retry_install(
        &self,
        id: String,
        mut preview: PackPreview,
        choices: PackInstallChoices,
        cancel: CancelToken,
        progress: ProgressSink,
    ) -> Result<PackOutcome, PackError> {
        let expected = preview.clone();
        let store = self.store.clone();
        let (i, source) = blocking(move || {
            let i = store.load(&id)?;
            let s = load_state(&i.dir)?.ok_or(PackError::InvalidState)?;
            if s.ready
                || s.version != expected.version
                || (expected.source.is_some() && s.source != expected.source)
                || s.archive_digest != expected.digest
            {
                return Err(PackError::InvalidState);
            }
            let marker = std::fs::read_to_string(i.dir.join(PENDING_FILE))?;
            if marker != "installing" {
                return Err(PackError::Recovery {
                    path: std::path::PathBuf::from(marker),
                    error: rust_i18n::t!("modpack.not_ready").to_string(),
                });
            }
            Ok((i, s.source))
        })
        .await?;
        preview.source = source;
        self.install_into(i, preview, choices, cancel, progress)
            .await
    }
    async fn install_into(
        &self,
        instance: Instance,
        preview: PackPreview,
        choices: PackInstallChoices,
        mut cancel: CancelToken,
        progress: ProgressSink,
    ) -> Result<PackOutcome, PackError> {
        let guard = self.store.lock(&instance.id)?;
        crate::launch::running::refuse_while_running(&instance.dir, &instance.id)?;
        let pending = state_for(
            &preview,
            choices.optional.clone(),
            Vec::new(),
            BTreeMap::new(),
            false,
        );
        let dir = instance.dir.clone();
        blocking(move || save_state(&dir, &pending)).await?;
        let (stage, mut state) = self
            .stage(&preview, &choices.optional, None, &mut cancel, &progress)
            .await?;
        let components = self
            .prepare_components(&preview, &stage, None, &progress, &mut cancel)
            .await?;
        state.components = components.clone();
        check_cancel(&cancel)?;
        let store = self.store.clone();
        let project_icon = preview.source.as_ref().and(preview.project_icon.clone());
        let mut outcome = blocking(move || {
            let _guard = guard;
            let old = load_state(&instance.dir)?.ok_or(PackError::InvalidState)?;
            let mut observed = BTreeMap::new();
            let mut changes = Vec::new();
            for (path, file) in &state.files {
                let current =
                    super::updates::fingerprint(&checked_game_path(&instance.game_dir(), path)?)?;
                if current.is_some() {
                    return Err(PackError::Collision(path.clone()));
                }
                observed.insert(path.clone(), None);
                changes.push(PackChange::install(path.clone(), file.sha512.clone()));
            }
            let metadata = super::updates::metadata_snapshot(&instance.dir)?;
            let plan = PackUpdatePlan {
                instance,
                old,
                target: state,
                preview,
                stage: Arc::new(stage),
                changes,
                observed,
                metadata,
                preserved: Vec::new(),
            };
            super::apply::apply(&store, plan, PackDecisions::default(), components, progress)
        })
        .await?;
        if let Some(url) = project_icon {
            if let Err(error) = self.install_project_icon(&outcome.instance.id, &url).await {
                tracing::debug!(%error,"optional project artwork unavailable; installation remains ready");
            }
            if let Ok(instance) = self.store.load(&outcome.instance.id) {
                outcome.instance = instance;
            }
        }
        Ok(outcome)
    }

    async fn install_project_icon(&self, id: &str, url: &str) -> Result<(), PackError> {
        use futures::StreamExt;
        let url = self.download_url(url)?;
        let response = self
            .downloads()?
            .client()
            .get(url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| PackError::Io(e.to_string()))?
            .error_for_status()
            .map_err(|e| PackError::Io(e.to_string()))?;
        if response.content_length().is_some_and(|n| n > 1024 * 1024) {
            return Err(PackError::Limit);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| PackError::Io(e.to_string()))?;
            if body.len() + chunk.len() > 1024 * 1024 {
                return Err(PackError::Limit);
            }
            body.extend_from_slice(&chunk);
        }
        let store = self.store.clone();
        let id = id.to_string();
        blocking(move || {
            store.set_project_icon_if_unset(&id, &body)?;
            Ok(())
        })
        .await
    }

    pub(super) fn staging(&self) -> Result<Staging, PackError> {
        let path = self
            .launcher
            .data
            .cache()
            .join("modpacks")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&path)?;
        Ok(Staging { path })
    }
    pub(super) async fn stage(
        &self,
        preview: &PackPreview,
        optional: &BTreeMap<String, bool>,
        existing: Option<PathBuf>,
        cancel: &mut CancelToken,
        progress: &ProgressSink,
    ) -> Result<(Staging, PackState), PackError> {
        let stage = self.staging()?;
        let mut selected = BTreeMap::new();
        let mut sources = Vec::new();
        let mut jobs = Vec::new();
        let mut reused_bytes = 0u64;
        let cache = self.launcher.data.cache().join("modpacks/downloads");
        for file in &preview.files {
            check_cancel(cancel)?;
            if file.optional {
                let enabled = optional.get(&file.path).copied().unwrap_or(false);
                selected.insert(file.path.clone(), enabled);
                if !enabled {
                    continue;
                }
            }
            let destination = stage.path.join("files").join(&file.path);
            let mut reused = false;
            if let Some(game) = &existing {
                let original = checked_game_path(game, &file.path)?;
                let dest = destination.clone();
                let file = file.clone();
                reused = blocking(move || {
                    if !verified_file(&original, &file)? {
                        return Ok(false);
                    }
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(original, &dest)?;
                    verified_file(&dest, &file)
                })
                .await?;
            }
            if reused {
                reused_bytes = reused_bytes
                    .checked_add(std::fs::metadata(&destination)?.len())
                    .ok_or(PackError::Limit)?;
                if reused_bytes > archive::MAX_PACK_DOWNLOAD_BYTES {
                    return Err(PackError::Limit);
                }
                sources.push((file.clone(), destination));
            } else {
                // Stable destinations let the existing Downloader retain completed bytes across retries.
                // Both author hashes are checked again before any cache bytes enter staging.
                let cached = cache.join(&file.sha512);
                jobs.push((
                    crate::download::DownloadJob {
                        url: self.download_url(&file.downloads[0])?,
                        dest: cached.clone(),
                        sha1: Some(file.sha1.clone()),
                        size: Some(file.size),
                    },
                    file.downloads[1..]
                        .iter()
                        .map(|url| self.download_url(url))
                        .collect::<Result<Vec<_>, _>>()?,
                ));
                sources.push((file.clone(), cached));
            }
        }
        let transfer_started = std::time::Instant::now();
        let job_count = jobs.len();
        let download = crate::progress::run_bounded_downloads(
            self.downloads()?,
            jobs,
            progress,
            Phase::DownloadContent,
            (
                archive::MAX_PACK_FILE_BYTES,
                archive::MAX_PACK_DOWNLOAD_BYTES - reused_bytes,
            ),
        );
        tokio::pin!(download);
        tokio::select! {result=&mut download=>{
            if let Err(error)=result {
                let path=match &error{crate::download::DownloadError::Failed{url,..}=>sources.iter().find(|(file,_)|file.downloads.iter().any(|source|self.download_url(source).is_ok_and(|mapped|mapped==*url))).map(|(file,_)|file.path.clone()).unwrap_or_default(),_=>String::new()};
                return Err(PackError::FileDownload{path,error:error.to_string()});
            }
        },_=cancelled(cancel)=>return Err(PackError::Cancelled)};
        tracing::info!(
            elapsed_ms = transfer_started.elapsed().as_millis(),
            jobs = job_count,
            "pack content transfer/cache and SHA1 checks complete"
        );
        check_cancel(cancel)?;
        let verify_started = std::time::Instant::now();
        let verification_total = (sources.len() + preview.overrides.len()) as u64;
        let source_count = sources.len() as u64;
        report(
            progress,
            Phase::VerifyPackFiles,
            0,
            verification_total,
            Unit::Items,
        );
        let verify_progress = progress.clone();
        let verify_cancel = cancel.clone();
        let stage_root = stage.path.join("files");
        let files = blocking(move || {
            let mut actual = 0u64;
            let mut files = BTreeMap::new();
            for (ix, (file, source)) in sources.into_iter().enumerate() {
                check_cancel(&verify_cancel)?;
                if !verified_file(&source, &file)? {
                    return Err(PackError::Verification);
                }
                actual = actual
                    .checked_add(std::fs::metadata(&source)?.len())
                    .ok_or(PackError::Limit)?;
                if actual > archive::MAX_PACK_DOWNLOAD_BYTES {
                    return Err(PackError::Limit);
                }
                check_cancel(&verify_cancel)?;
                let destination = stage_root.join(&file.path);
                if source != destination {
                    if let Some(parent) = destination.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(source, &destination)?;
                    if !verified_file(&destination, &file)? {
                        return Err(PackError::Verification);
                    }
                }
                files.insert(
                    file.path.clone(),
                    BaselineFile {
                        sha512: file.sha512.clone(),
                        source: manifest_source(&file),
                    },
                );
                report(
                    &verify_progress,
                    Phase::VerifyPackFiles,
                    ix as u64 + 1,
                    verification_total,
                    Unit::Items,
                );
            }
            Ok(files)
        })
        .await?;
        let mut files = files;
        let overrides = preview.overrides.clone();
        let root = stage.path.join("files");
        let verify_progress = progress.clone();
        let verify_cancel = cancel.clone();
        let override_hashes = blocking(move || {
            let mut hashes = BTreeMap::new();
            for (ix, (p, b)) in overrides.iter().enumerate() {
                check_cancel(&verify_cancel)?;
                let path = root.join(p);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, b)?;
                hashes.insert(
                    p.clone(),
                    BaselineFile {
                        sha512: hex::encode(Sha512::digest(b)),
                        source: None,
                    },
                );
                report(
                    &verify_progress,
                    Phase::VerifyPackFiles,
                    source_count + ix as u64 + 1,
                    verification_total,
                    Unit::Items,
                );
            }
            Ok(hashes)
        })
        .await?;
        files.extend(override_hashes);
        tracing::info!(
            elapsed_ms = verify_started.elapsed().as_millis(),
            files = files.len(),
            "pack content SHA512/staging/overrides complete"
        );
        Ok((
            stage,
            state_for(preview, selected, Vec::new(), files, false),
        ))
    }
    pub(super) async fn prepare_components(
        &self,
        p: &PackPreview,
        stage: &Staging,
        previous: Option<&crate::instance::InstanceConfig>,
        progress: &ProgressSink,
        cancel: &mut CancelToken,
    ) -> Result<Vec<Component>, PackError> {
        check_cancel(cancel)?;
        let prepare_started = std::time::Instant::now();
        let mut components = vec![Component {
            uid: ComponentUid::Minecraft,
            version: p.mc.clone(),
            profile: None,
        }];
        if let Some((kind, version)) = &p.loader {
            let vanilla = self.launcher.ensure_version(&p.mc).await?;
            let list =
                crate::loader::list_versions(&self.launcher.downloader, *kind, &p.mc).await?;
            let v = crate::loader::VersionPick::Exact(version.clone())
                .pick(&list)
                .ok_or(PackError::Unsupported)?;
            components.push(
                crate::loader::ops::install_version(&self.launcher, v, &vanilla, false, progress)
                    .await?,
            );
        }
        let config = crate::instance::InstanceConfig {
            components: components.clone(),
            ..previous.cloned().unwrap_or_default()
        };
        let temporary = Instance {
            id: "pack-preparation".into(),
            dir: stage.path.join("resources"),
            config,
        };
        let work = self
            .launcher
            .prepare_resources(&temporary, &self.global, progress.clone());
        tokio::pin!(work);
        tokio::select! {r=&mut work=>r?,_=cancelled(cancel)=>return Err(PackError::Cancelled)};
        check_cancel(cancel)?;
        tracing::info!(elapsed_ms=prepare_started.elapsed().as_millis(),minecraft=%p.mc,"pack game/loader resource preparation complete");
        Ok(components)
    }
    fn download_url(&self, url: &str) -> Result<String, PackError> {
        if !archive::valid_download_url(url) {
            return Err(PackError::Invalid);
        }
        let mapped = self.launcher.downloader.mirror().rewrite(url);
        if mapped == url {
            return Ok(mapped);
        }
        // Explicit fixture mappings may replace an author URL once, never add public fallbacks.
        let trusted_loopback = reqwest::Url::parse(&mapped).is_ok_and(|u| {
            u.scheme() == "http"
                && u.username().is_empty()
                && u.password().is_none()
                && u.host_str().is_some_and(|host| {
                    host == "localhost"
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                })
        });
        if trusted_loopback {
            Ok(mapped)
        } else {
            Err(PackError::Invalid)
        }
    }
    fn downloads(&self) -> Result<&crate::download::Downloader, PackError> {
        self.downloads
            .as_ref()
            .map(Arc::as_ref)
            .map_err(|e| PackError::Io(e.clone()))
    }
}
fn state_for(
    p: &PackPreview,
    optional: BTreeMap<String, bool>,
    components: Vec<Component>,
    files: BTreeMap<String, BaselineFile>,
    ready: bool,
) -> PackState {
    PackState {
        schema: 1,
        archive_digest: p.digest.clone(),
        name: p.name.clone(),
        version: p.version.clone(),
        source: p.source.clone(),
        components,
        files,
        optional,
        ready,
        retained: BTreeMap::new(),
        bindings: BTreeMap::new(),
    }
}
pub(super) fn manifest_source(f: &PackFile) -> Option<VersionRef> {
    if !f.path.starts_with("mods/") || f.path.split('/').count() != 2 {
        return None;
    }
    for u in &f.downloads {
        let u = reqwest::Url::parse(u).ok()?;
        if u.host_str() != Some("cdn.modrinth.com") {
            continue;
        }
        let s: Vec<_> = u.path_segments()?.collect();
        if s.len() == 5
            && s[0] == "data"
            && s[2] == "versions"
            && [s[1], s[3]]
                .iter()
                .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        {
            return Some(VersionRef::new(
                ProjectRef::new(Platform::Modrinth, s[1]),
                s[3],
            ));
        }
    }
    None
}
pub(super) fn save_pack_sources(
    i: &Instance,
    s: &PackState,
    replaced: Vec<String>,
) -> Result<(), PackError> {
    let mut records = Vec::new();
    for (path, f) in &s.files {
        if let Some(v) = &f.source {
            let disk = checked_game_path(&i.game_dir(), path)?;
            if super::updates::fingerprint(&disk)?.as_deref() == Some(f.sha512.as_str()) {
                records.push((
                    v.clone(),
                    ContentKind::Mod,
                    path.clone(),
                    crate::download::sha1_file(&disk)?,
                ));
            }
        }
    }

    let mut counts = std::collections::HashMap::new();
    for (version, _, _, _) in &records {
        *counts.entry(version.project().clone()).or_insert(0usize) += 1;
    }
    // Ambiguous metadata must not veto the author's exact file composition.
    records.retain(|(version, _, _, _)| counts.get(version.project()) == Some(&1));
    crate::content::online::record_pack_files(&i.dir, records, &replaced)?;
    Ok(())
}
pub(super) fn checked_game_path(root: &Path, relative: &str) -> Result<PathBuf, PackError> {
    archive::checked_path(relative)?;
    if root
        .parent()
        .is_some_and(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
    {
        return Err(PackError::UnsafePath(relative.into()));
    }
    let mut p = root.to_path_buf();
    for part in std::iter::once(None).chain(relative.split('/').map(Some)) {
        if let Some(part) = part {
            p.push(part)
        }
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(PackError::UnsafePath(relative.into()));
            }
            Ok(m) if p != root && p != root.join(relative) && !m.is_dir() => {
                return Err(PackError::Collision(relative.into()));
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(p)
}
pub(super) fn check_cancel(c: &CancelToken) -> Result<(), PackError> {
    if *c.borrow() || c.has_changed().is_err() {
        Err(PackError::Cancelled)
    } else {
        Ok(())
    }
}
pub(super) async fn cancelled(c: &mut CancelToken) {
    loop {
        if *c.borrow() || c.changed().await.is_err() {
            return;
        }
    }
}
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, PackError> + Send + 'static,
) -> Result<T, PackError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| PackError::Io(e.to_string()))?
}

fn verified_file(path: &Path, file: &PackFile) -> Result<bool, PackError> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() <= archive::MAX_PACK_FILE_BYTES => {
            Ok(crate::download::sha1_file(path)? == file.sha1
                && super::updates::fingerprint(path)?.as_deref() == Some(file.sha512.as_str()))
        }
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn pack_http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30))
        .user_agent("Patina-Launcher/0.1")
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.previous().len() >= 5 {
                return a.error("redirect limit");
            };
            let u = a.url();
            if u.scheme() == "https"
                && u.username().is_empty()
                && u.password().is_none()
                && u.port().is_none()
                && matches!(
                    u.host_str(),
                    Some(
                        "cdn.modrinth.com"
                            | "github.com"
                            | "raw.githubusercontent.com"
                            | "gitlab.com"
                            | "release-assets.githubusercontent.com"
                            | "objects.githubusercontent.com"
                    )
                )
            {
                a.follow()
            } else {
                a.error("unsafe redirect")
            }
        }))
        .build()
}
