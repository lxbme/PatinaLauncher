//! Launcher-owned vendor JDKs, separate from Mojang components. Only a verified
//! staged archive with a valid probe is published; no default is changed here.
use super::{JavaInstall, catalog::JavaRelease};
use crate::{
    config::JavaSettings,
    download::{DownloadJob, Downloader},
    instance::InstanceStore,
    progress::{Phase, ProgressSink, Unit, run_downloads},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
const MARKER: &str = ".patina-distribution.json";

#[derive(Debug, thiserror::Error)]
pub enum DistributionError {
    #[error("{}",rust_i18n::t!("java_distribution.io",error=.0))]
    Io(String),
    #[error("{}",rust_i18n::t!("java_distribution.invalid",error=.0))]
    Invalid(String),
    #[error("{}",rust_i18n::t!("java_distribution.canceled"))]
    Canceled,
    #[error("{}",rust_i18n::t!("java_distribution.referenced",objects=.0.join(", ")))]
    Referenced(Vec<String>),
    #[error(transparent)]
    Download(#[from] crate::download::DownloadError),
}
impl From<io::Error> for DistributionError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
fn invalid(s: impl ToString) -> DistributionError {
    DistributionError::Invalid(s.to_string())
}
fn check(cancel: &AtomicBool) -> Result<(), DistributionError> {
    if cancel.load(Ordering::Relaxed) {
        Err(DistributionError::Canceled)
    } else {
        Ok(())
    }
}
struct CancelOnDrop(Option<Arc<AtomicBool>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(true, Ordering::Relaxed);
        }
    }
}
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Marker {
    release: JavaRelease,
    java_relative: PathBuf,
    sha256: String,
}
#[derive(Clone, Debug)]
pub struct InstalledDistribution {
    release: JavaRelease,
    java: JavaInstall,
    sha256: String,
}
impl InstalledDistribution {
    pub fn release(&self) -> &JavaRelease {
        &self.release
    }
    pub fn java(&self) -> &JavaInstall {
        &self.java
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}
#[derive(Clone)]
pub struct DistributionStore {
    root: PathBuf,
}
impl DistributionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    fn installed(&self, id: &str) -> Result<InstalledDistribution, DistributionError> {
        if !crate::fsutil::is_safe_name(id) {
            return Err(invalid(rust_i18n::t!(
                "java_distribution.reason.invalid_id"
            )));
        }
        let dir = self.root.join(id);
        if fs::symlink_metadata(&dir)?.file_type().is_symlink() {
            return Err(invalid(rust_i18n::t!(
                "java_distribution.reason.linked_directory"
            )));
        }
        let marker: Marker =
            serde_json::from_slice(&fs::read(dir.join(MARKER))?).map_err(invalid)?;
        if marker.release.id() != id || !safe_relative(&marker.java_relative) {
            return Err(invalid(rust_i18n::t!(
                "java_distribution.reason.invalid_marker"
            )));
        }
        let java = super::probe(&dir.join(&marker.java_relative))
            .ok_or_else(|| invalid(rust_i18n::t!("java_distribution.reason.probe_failed")))?;
        if !java.path.starts_with(dir.canonicalize()?) {
            return Err(invalid(rust_i18n::t!(
                "java_distribution.reason.java_outside"
            )));
        }
        Ok(InstalledDistribution {
            release: marker.release,
            java,
            sha256: marker.sha256,
        })
    }
    pub fn list(&self) -> Result<Vec<InstalledDistribution>, DistributionError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries {
            let e = entry?;
            if let Some(id) = e.file_name().to_str().filter(|s| !s.starts_with('.'))
                && let Ok(i) = self.installed(id)
            {
                out.push(i);
            }
        }
        out.sort_by(|a, b| {
            b.java
                .major
                .cmp(&a.java.major)
                .then_with(|| a.release.id().cmp(&b.release.id()))
        });
        Ok(out)
    }
    pub fn merge_detected(
        &self,
        mut detected: Vec<JavaInstall>,
    ) -> Result<Vec<JavaInstall>, DistributionError> {
        detected.extend(self.list()?.into_iter().map(|i| i.java));
        let mut seen = std::collections::HashSet::new();
        detected.retain(|j| seen.insert(j.path.canonicalize().unwrap_or_else(|_| j.path.clone())));
        detected.sort_by(|a, b| b.major.cmp(&a.major).then_with(|| a.path.cmp(&b.path)));
        Ok(detected)
    }
    pub async fn install(
        &self,
        dl: &Downloader,
        release: &JavaRelease,
        progress: &ProgressSink,
        cancel: Arc<AtomicBool>,
    ) -> Result<InstalledDistribution, DistributionError> {
        let mut cancel_on_drop = CancelOnDrop(Some(cancel.clone()));
        check(&cancel)?;
        if !crate::fsutil::is_safe_name(&release.id()) {
            return Err(invalid(rust_i18n::t!(
                "java_distribution.reason.invalid_identity"
            )));
        }
        if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            return Err(invalid(rust_i18n::t!(
                "java_distribution.reason.unsupported_platform"
            )));
        }
        let lock = super::runtime::component_lock(&self.root, "vendor-store");
        let guard = lock.lock_owned().await;
        check(&cancel)?;
        if let Ok(installed) = self.installed(&release.id()) {
            cancel_on_drop.0 = None;
            return Ok(installed);
        }
        fs::create_dir_all(&self.root)?;
        let staging = Staging(self.root.join(format!(".install-{}", uuid::Uuid::new_v4())));
        fs::create_dir(&staging.0)?;
        let archive = staging.0.join("jdk.tar.gz");
        let checksum = match release.sha256() {
            Some(v) => v.to_string(),
            None => {
                let url = release.checksum_url().ok_or_else(|| {
                    invalid(rust_i18n::t!("java_distribution.reason.missing_checksum"))
                })?;
                let bytes = dl.fetch_bytes(url, None).await?;
                let body = String::from_utf8(bytes).map_err(invalid)?;
                let mut fields = body.split_whitespace();
                let sum = fields.next().ok_or_else(|| {
                    invalid(rust_i18n::t!("java_distribution.reason.empty_checksum"))
                })?;
                let filename = fields.next().unwrap_or("").trim_start_matches('*');
                if filename != release.url().rsplit('/').next().unwrap_or("") {
                    return Err(invalid(rust_i18n::t!(
                        "java_distribution.reason.wrong_checksum_file"
                    )));
                }
                sum.to_string()
            }
        };
        if checksum.len() != 64 || !checksum.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid(rust_i18n::t!(
                "java_distribution.reason.invalid_checksum"
            )));
        }
        let download = run_downloads(
            dl,
            vec![DownloadJob {
                url: release.url().into(),
                dest: archive.clone(),
                sha1: None,
                size: None,
            }],
            progress,
            Phase::DownloadRuntime,
            Unit::Bytes,
            0,
        );
        tokio::pin!(download);
        loop {
            tokio::select! {result=&mut download=>{result?;break;}, _=tokio::time::sleep(std::time::Duration::from_millis(80))=>check(&cancel)?,}
        }
        crate::progress::report(progress, Phase::Finalize, 0, 1, Unit::Items);
        let store = self.clone();
        let release = release.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let staging = staging;
            check(&cancel)?;
            let actual = sha256_file(&archive)?;
            if !actual.eq_ignore_ascii_case(&checksum) {
                return Err(invalid(rust_i18n::t!(
                    "java_distribution.reason.checksum_mismatch"
                )));
            }
            let unpacked = staging.0.join("unpacked");
            fs::create_dir(&unpacked)?;
            extract(&archive, &unpacked, &cancel)?;
            check(&cancel)?;
            let mut candidates = Vec::new();
            if unpacked.join("bin/java").is_file() {
                candidates.push(unpacked.join("bin/java"));
            }
            for entry in fs::read_dir(&unpacked)? {
                let path = entry?.path().join("bin/java");
                if path.is_file() {
                    candidates.push(path);
                }
            }
            if candidates.len() != 1 {
                return Err(invalid(rust_i18n::t!(
                    "java_distribution.reason.ambiguous_jdk"
                )));
            }
            let release_file = fs::read_to_string(
                candidates[0]
                    .parent()
                    .and_then(Path::parent)
                    .ok_or_else(|| invalid(rust_i18n::t!("java_distribution.reason.missing_home")))?
                    .join("release"),
            )?;
            let (_, arch) = super::parse_release_file(&release_file).ok_or_else(|| {
                invalid(rust_i18n::t!("java_distribution.reason.invalid_release"))
            })?;
            if !matches!(arch.as_deref(), Some("amd64" | "x86_64")) {
                return Err(invalid(rust_i18n::t!(
                    "java_distribution.reason.wrong_arch"
                )));
            }
            let java = super::probe(&candidates[0])
                .ok_or_else(|| invalid(rust_i18n::t!("java_distribution.reason.probe_failed")))?;
            if !java.path.starts_with(unpacked.canonicalize()?)
                || java.major != release.major()
                || !java.is_64bit
                || !version_matches(&java.version, &release)
            {
                return Err(invalid(rust_i18n::t!(
                    "java_distribution.reason.wrong_version"
                )));
            }
            let relative = java
                .path
                .strip_prefix(unpacked.canonicalize()?)
                .map_err(invalid)?
                .to_path_buf();
            let marker = Marker {
                release: release.clone(),
                java_relative: relative,
                sha256: actual,
            };
            fs::write(
                unpacked.join(MARKER),
                serde_json::to_vec_pretty(&marker).map_err(invalid)?,
            )?;
            check(&cancel)?;
            let final_dir = store.root.join(release.id());
            if final_dir.exists() {
                return Err(invalid(rust_i18n::t!(
                    "java_distribution.reason.destination_exists"
                )));
            }
            fs::rename(&unpacked, &final_dir)?;
            store.installed(&release.id())
        })
        .await
        .map_err(invalid)?;
        cancel_on_drop.0 = None;
        result
    }
    /// Re-read every persisted reference under instance locks immediately before
    /// removal. The caller retains the global config read lock for this operation.
    pub async fn remove(
        &self,
        id: &str,
        global: &JavaSettings,
        instances: &InstanceStore,
    ) -> Result<(), DistributionError> {
        let lock = super::runtime::component_lock(&self.root, "vendor-store");
        let _guard = lock.lock().await;
        let installed = self.installed(id)?;
        let _references = reference_guards(&installed.java.path, global, instances)?;
        // Recheck marker/ownership under the same store lock. Never remove a
        // caller-supplied Java path or a system-discovered installation.
        self.installed(id)?;
        fs::remove_dir_all(self.root.join(id))?;
        Ok(())
    }
}
/// Hold existing instance guards through deletion, sharing the vendor-store
/// protection with explicitly selectable Mojang runtimes.
pub(crate) fn reference_guards(
    path: &Path,
    global: &JavaSettings,
    instances: &InstanceStore,
) -> Result<Vec<crate::locks::InstanceGuard>, DistributionError> {
    let mut objects = Vec::new();
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let matches = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()) == canonical;
    if global.java_path.as_deref().is_some_and(matches) {
        objects.push(rust_i18n::t!("java_distribution.global_default").to_string());
    }
    let listed = instances.list().map_err(invalid)?;
    if !listed.broken.is_empty() {
        return Err(invalid(rust_i18n::t!(
            "java_distribution.reason.unreadable_instances"
        )));
    }
    let mut locks = Vec::new();
    for inst in listed.instances {
        match instances.lock(&inst.id) {
            Ok(lock) => locks.push(lock),
            Err(_) => {
                objects.push(inst.config.name.clone());
                continue;
            }
        }
        let inst = instances.load(&inst.id).map_err(invalid)?;
        if inst
            .config
            .java_override
            .as_ref()
            .and_then(|j| j.java_path.as_deref())
            .is_some_and(matches)
        {
            objects.push(inst.config.name.clone());
        }
        if let Some(running) = crate::launch::running_game(&inst)
            && crate::platform::process_executable(running.pid)
                .as_deref()
                .is_none_or(matches)
        {
            objects.push(
                rust_i18n::t!("java_distribution.running", name = inst.config.name).to_string(),
            );
        }
    }
    if !objects.is_empty() {
        objects.sort();
        objects.dedup();
        return Err(DistributionError::Referenced(objects));
    }
    Ok(locks)
}

fn version_matches(version: &str, release: &JavaRelease) -> bool {
    let parse = |s: &str| {
        s.split(|c: char| !c.is_ascii_digit())
            .filter_map(|n| n.parse::<u32>().ok())
            .collect::<Vec<_>>()
    };
    let mut actual = parse(version);
    if actual.first() == Some(&1) {
        actual.remove(0);
    }
    let expected = parse(release.version());
    // Temurin 8 uses jdk8uNNN-bNN release names; Java reports 1.8.0_NNN.
    if release.major() == 8 && release.version().contains('u') {
        return actual.get(2) == expected.get(1);
    }
    actual.iter().take(3).eq(expected.iter().take(3))
}
fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}
fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}
fn resolve_link(base: &Path, target: &Path) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for c in target.components() {
        match c {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(out)
}
// Resolve archive links component-by-component, not by normalizing `..`
// before following symlinks (which can make an escaping chain look safe).
fn resolve_archive_path(
    path: &Path,
    links: &std::collections::HashMap<PathBuf, PathBuf>,
) -> Option<PathBuf> {
    let mut pending = path
        .components()
        .map(|c| PathBuf::from(c.as_os_str()))
        .collect::<std::collections::VecDeque<_>>();
    let mut out = PathBuf::new();
    let mut followed = 0;
    while let Some(part) = pending.pop_front() {
        if part == Path::new(".") {
            continue;
        }
        if part == Path::new("..") {
            if !out.pop() {
                return None;
            }
            continue;
        }
        if !safe_relative(&part) {
            return None;
        }
        out.push(&part);
        if let Some(target) = links.get(&out) {
            followed += 1;
            if followed > 40 {
                return None;
            }
            out.pop();
            for c in target.components().rev() {
                pending.push_front(PathBuf::from(c.as_os_str()));
            }
        }
    }
    Some(out)
}
fn extract(archive: &Path, root: &Path, cancel: &AtomicBool) -> Result<(), DistributionError> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(archive)?));
    tar.set_preserve_permissions(true);
    let mut links = Vec::new();
    for entry in tar.entries()? {
        check(cancel)?;
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !safe_relative(&path) {
            return Err(invalid(rust_i18n::t!(
                "java_distribution.reason.unsafe_path"
            )));
        }
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            let target = entry
                .link_name()?
                .ok_or_else(|| invalid(rust_i18n::t!("java_distribution.reason.missing_link")))?
                .into_owned();
            let base = if kind.is_symlink() {
                path.parent().unwrap_or(Path::new(""))
            } else {
                Path::new("")
            };
            let resolved = resolve_link(base, &target)
                .ok_or_else(|| invalid(rust_i18n::t!("java_distribution.reason.unsafe_link")))?;
            links.push((path, target, resolved, kind.is_hard_link()));
        } else if kind.is_file() || kind.is_dir() {
            if !entry.unpack_in(root)? {
                return Err(invalid(rust_i18n::t!(
                    "java_distribution.reason.unsafe_path"
                )));
            }
        } else {
            return Err(invalid(rust_i18n::t!(
                "java_distribution.reason.unsupported_entry"
            )));
        }
    }
    let symlinks = links
        .iter()
        .filter(|(_, _, _, hard)| !*hard)
        .map(|(path, target, _, _)| (resolve_link(Path::new(""), path).unwrap(), target.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    for (path, target, resolved, hard) in &links {
        // Do not materialize entries through an archive-provided directory link.
        if path.ancestors().skip(1).any(|p| symlinks.contains_key(p)) {
            return Err(invalid(rust_i18n::t!(
                "java_distribution.reason.linked_parent"
            )));
        }
        let reference = if *hard {
            resolved.clone()
        } else {
            path.parent().unwrap_or(Path::new("")).join(target)
        };
        resolve_archive_path(&reference, &symlinks)
            .ok_or_else(|| invalid(rust_i18n::t!("java_distribution.reason.unsafe_link_chain")))?;
    }
    // Links are created after all regular files, preventing writes through an
    // archive-supplied directory symlink. Every target stays inside this root.
    for (path, target, resolved, hard) in links {
        check(cancel)?;
        let dest = root.join(path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        if hard {
            fs::hard_link(root.join(resolved), dest)?;
        } else {
            crate::platform::symlink(&target, &dest)?;
        }
    }
    Ok(())
}
