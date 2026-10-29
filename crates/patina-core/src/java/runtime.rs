//! Launcher-managed Java runtimes (Mojang `java-runtime` components).

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use serde::Deserialize;

use crate::download::{DownloadError, DownloadJob, Downloader};
use crate::fsutil::{is_safe_name, is_safe_rel};
use crate::meta::FileRef;
use crate::platform;
use crate::progress::{Phase, ProgressSink, Unit, run_downloads};

pub const RUNTIME_INDEX_URL: &str = "https://piston-meta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";

const MARKER: &str = ".patina-installed";

pub fn platform_key() -> Option<&'static str> {
    platform::mojang_runtime_key()
}

pub type RuntimeIndex = HashMap<String, HashMap<String, Vec<RuntimeEntry>>>;

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeEntry {
    pub manifest: FileRef,
    pub version: RuntimeVersion,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeVersion {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeManifest {
    pub files: BTreeMap<String, RuntimeFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RuntimeFile {
    File {
        #[serde(default)]
        executable: bool,
        downloads: RuntimeDownloads,
    },
    Directory,
    Link {
        target: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeDownloads {
    pub raw: FileRef,
}

#[derive(Debug, Default)]
pub struct InstallPlan {
    pub jobs: Vec<DownloadJob>,
    pub dirs: Vec<PathBuf>,
    pub links: Vec<(PathBuf, String)>,
    pub executables: Vec<PathBuf>,
}

/// A link target is safe if, resolved lexically against the link's parent
/// (relative to the runtime root), it stays inside the root.
fn link_target_stays_inside(rel_link: &str, target: &str) -> bool {
    let mut stack: Vec<&std::ffi::OsStr> = Path::new(rel_link)
        .parent()
        .map(|p| p.iter().collect())
        .unwrap_or_default();
    for c in Path::new(target).components() {
        match c {
            Component::Normal(n) => stack.push(n),
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

pub fn plan_install(m: &RuntimeManifest, dest: &Path) -> InstallPlan {
    let mut plan = InstallPlan::default();
    for (rel, file) in &m.files {
        if !is_safe_rel(rel) {
            continue;
        }
        let path = dest.join(rel);
        match file {
            RuntimeFile::Directory => plan.dirs.push(path),
            RuntimeFile::File {
                executable,
                downloads,
            } => {
                if let Some(parent) = path.parent() {
                    plan.dirs.push(parent.to_path_buf());
                }
                plan.jobs.push(DownloadJob {
                    url: downloads.raw.url.clone(),
                    dest: path.clone(),
                    sha1: Some(downloads.raw.sha1.clone()),
                    size: Some(downloads.raw.size),
                });
                if *executable {
                    plan.executables.push(path);
                }
            }
            RuntimeFile::Link { target } => {
                if !link_target_stays_inside(rel, target) {
                    continue;
                }
                if let Some(parent) = path.parent() {
                    plan.dirs.push(parent.to_path_buf());
                }
                plan.links.push((path, target.clone()));
            }
        }
    }
    plan.dirs.sort();
    plan.dirs.dedup();
    plan
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("{}", rust_i18n::t!("errors.runtime.unavailable", component = .component))]
    Unavailable { component: String },
    #[error("{}", rust_i18n::t!("errors.runtime.invalid_component", component = format!("{:?}", .0)))]
    InvalidComponent(String),
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub struct ManagedRuntimes {
    root: PathBuf,
}

/// Per-component install/remove lock, shared by every `ManagedRuntimes` of
/// this process (callers create a fresh handle per operation).
type ComponentLock = Arc<tokio::sync::Mutex<()>>;
type LockMap = HashMap<(PathBuf, String), Weak<tokio::sync::Mutex<()>>>;

fn component_lock(root: &Path, component: &str) -> ComponentLock {
    static LOCKS: OnceLock<StdMutex<LockMap>> = OnceLock::new();
    let key = (
        crate::fsutil::path_key(root).unwrap_or_else(|_| root.to_path_buf()),
        component.to_string(),
    );
    let mut map = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(lock) = map.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    map.retain(|_, w| w.strong_count() > 0);
    let lock = ComponentLock::default();
    map.insert(key, Arc::downgrade(&lock));
    lock
}

/// Creates `link` → `target`; an existing correct link is kept, anything else
/// at that path is replaced.
fn ensure_symlink(target: &Path, link: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(link) {
        Ok(m) if m.file_type().is_symlink() => {
            if std::fs::read_link(link)? == target {
                return Ok(());
            }
            std::fs::remove_file(link)?;
        }
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(link)?,
        Ok(_) => std::fs::remove_file(link)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    platform::symlink(target, link)
}

impl ManagedRuntimes {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn home(&self, component: &str) -> PathBuf {
        self.root.join(component)
    }

    pub fn java_bin(&self, component: &str) -> PathBuf {
        platform::java_bin_in_home(&self.home(component))
    }

    pub fn is_installed(&self, component: &str) -> bool {
        is_safe_name(component) && self.home(component).join(MARKER).is_file()
    }

    /// Downloads (or re-downloads) `component` and returns its `java`.
    ///
    /// Installs and removals of one component are serialized in this process.
    /// A caller that had to wait for another install of the same component
    /// reuses that result when it left the component installed, instead of
    /// downloading it again.
    pub async fn install(
        &self,
        dl: &Downloader,
        component: &str,
        progress: &ProgressSink,
    ) -> Result<PathBuf, RuntimeError> {
        if !is_safe_name(component) {
            return Err(RuntimeError::InvalidComponent(component.to_string()));
        }
        let lock = component_lock(&self.root, component);
        let _guard = match lock.try_lock() {
            Ok(g) => g,
            Err(_) => {
                let g = lock.lock().await;
                if self.is_installed(component) {
                    return Ok(self.java_bin(component));
                }
                g
            }
        };
        self.install_locked(dl, component, progress).await
    }

    async fn install_locked(
        &self,
        dl: &Downloader,
        component: &str,
        progress: &ProgressSink,
    ) -> Result<PathBuf, RuntimeError> {
        // A failed (re)install must not leave a stale marker behind.
        match tokio::fs::remove_file(self.home(component).join(MARKER)).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let unavailable = || RuntimeError::Unavailable {
            component: component.to_string(),
        };
        let key = platform_key().ok_or_else(unavailable)?;
        let bytes = dl.fetch_bytes(RUNTIME_INDEX_URL, None).await?;
        let index: RuntimeIndex = serde_json::from_slice(&bytes)?;
        let entry = index
            .get(key)
            .and_then(|c| c.get(component))
            .and_then(|v| v.first())
            .ok_or_else(unavailable)?;

        let mbytes = dl
            .fetch_bytes(&entry.manifest.url, Some(&entry.manifest.sha1))
            .await?;
        let manifest: RuntimeManifest = serde_json::from_slice(&mbytes)?;

        let home = self.home(component);
        let plan = plan_install(&manifest, &home);
        tokio::fs::create_dir_all(&home).await?;
        for d in &plan.dirs {
            tokio::fs::create_dir_all(d).await?;
        }
        run_downloads(
            dl,
            plan.jobs,
            progress,
            Phase::DownloadRuntime,
            Unit::Bytes,
            0,
        )
        .await?;
        for e in &plan.executables {
            platform::set_executable(e)?;
        }
        for (link, target) in &plan.links {
            ensure_symlink(Path::new(target), link)?;
        }
        tokio::fs::write(home.join(MARKER), &entry.version.name).await?;
        Ok(self.java_bin(component))
    }

    /// (component, version name) for each installed component, sorted by name.
    pub fn list_installed(&self) -> Vec<(String, String)> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let v = std::fs::read_to_string(e.path().join(MARKER)).ok()?;
                Some((name, v.trim().to_string()))
            })
            .collect();
        out.sort();
        out
    }

    pub async fn remove(&self, component: &str) -> io::Result<()> {
        if !is_safe_name(component) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                rust_i18n::t!(
                    "errors.runtime.invalid_component",
                    component = format!("{component:?}")
                )
                .to_string(),
            ));
        }
        // Waits for a running install of the same component.
        let lock = component_lock(&self.root, component);
        let _guard = lock.lock().await;
        match tokio::fs::remove_dir_all(self.home(component)).await {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::Mirror;
    use sha1::{Digest, Sha1};
    use std::time::Duration;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sha(b: &[u8]) -> String {
        hex::encode(Sha1::digest(b))
    }

    #[test]
    fn plans_install() {
        let m: RuntimeManifest =
            serde_json::from_str(include_str!("../../tests/fixtures/runtime_manifest.json"))
                .unwrap();
        let p = plan_install(&m, Path::new("/R/jre-legacy"));
        assert_eq!(p.jobs.len(), 3);
        assert!(
            p.executables
                .contains(&PathBuf::from("/R/jre-legacy/bin/java"))
        );
        assert_eq!(
            p.links,
            vec![(
                PathBuf::from("/R/jre-legacy/lib/alias.so"),
                "libjli.so".to_string()
            )]
        );
    }

    #[test]
    fn rejects_escaping_paths() {
        let json = r#"{"files":{
            "../evil":{"type":"directory"},
            "/abs":{"type":"directory"},
            "a/../../b":{"type":"link","target":"x"},
            "ok":{"type":"directory"}}}"#;
        let m: RuntimeManifest = serde_json::from_str(json).unwrap();
        let p = plan_install(&m, Path::new("/R/c"));
        assert_eq!(p.dirs, vec![PathBuf::from("/R/c/ok")]);
        assert!(p.links.is_empty());
    }

    #[test]
    fn rejects_escaping_link_targets() {
        let json = r#"{"files":{
            "lib/a.so":{"type":"link","target":"../../../etc/passwd"},
            "lib/b.so":{"type":"link","target":"/etc/passwd"},
            "lib/c.so":{"type":"link","target":"../lib/real.so"}}}"#;
        let m: RuntimeManifest = serde_json::from_str(json).unwrap();
        let p = plan_install(&m, Path::new("/R/c"));
        assert_eq!(
            p.links,
            vec![(PathBuf::from("/R/c/lib/c.so"), "../lib/real.so".to_string())]
        );
    }

    #[tokio::test]
    async fn unsafe_component_names_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("runtimes");
        std::fs::create_dir_all(&root).unwrap();
        let rt = ManagedRuntimes::new(root.clone());
        for c in ["..", "", "a/b", "/abs", "."] {
            let e = rt.remove(c).await.unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            assert!(!rt.is_installed(c));
        }
        assert!(root.is_dir() && tmp.path().is_dir());
    }

    #[tokio::test]
    async fn invalid_component_install_errors() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let rt = ManagedRuntimes::new(tmp.path().to_path_buf());
        let r = rt.install(&downloader(&server), "../x", &None).await;
        assert!(matches!(r, Err(RuntimeError::InvalidComponent(_))));
    }

    #[test]
    fn platform_key_on_linux_x64() {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(platform_key(), Some("linux"));
        }
    }

    fn downloader(server: &MockServer) -> Downloader {
        let uri = server.uri();
        let mirror = Mirror {
            id: "fake".into(),
            rules: vec![
                ("https://piston-meta.mojang.com".into(), uri.clone()),
                ("https://piston-data.mojang.com".into(), uri),
            ],
        };
        Downloader::new(reqwest::Client::new(), mirror).with_retry(1, Duration::ZERO)
    }

    async fn serve(server: &MockServer, p: &str, body: Vec<u8>) {
        Mock::given(method("GET"))
            .and(wiremock::matchers::path(p))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(server)
            .await;
    }

    /// Serves a one-component ("jre-legacy") fake runtime index and files.
    async fn serve_fake_runtime(server: &MockServer) {
        let files: [(&str, &[u8]); 3] = [
            ("java", b"#!/bin/sh\n"),
            ("jli", b"jli-lib"),
            ("release", b"JAVA_VERSION=8"),
        ];
        let fr = |name: &str, body: &[u8]| {
            serde_json::json!({"downloads":{"raw":{"sha1":sha(body),"size":body.len(),
              "url":format!("https://piston-data.mojang.com/o/{name}")}}})
        };
        let mut f_java = fr("java", files[0].1);
        f_java["type"] = "file".into();
        f_java["executable"] = true.into();
        let mut f_jli = fr("jli", files[1].1);
        f_jli["type"] = "file".into();
        let mut f_rel = fr("release", files[2].1);
        f_rel["type"] = "file".into();
        let manifest = serde_json::json!({"files":{
            "bin":{"type":"directory"},
            "bin/java":f_java,
            "lib/libjli.so":f_jli,
            "lib/alias.so":{"type":"link","target":"libjli.so"},
            "release":f_rel}})
        .to_string()
        .into_bytes();
        let index = serde_json::json!({"linux":{"jre-legacy":[{
            "manifest":{"url":"https://piston-meta.mojang.com/m.json","sha1":sha(&manifest),"size":manifest.len()},
            "version":{"name":"8u202"}}]}})
        .to_string()
        .into_bytes();
        let idx_path = RUNTIME_INDEX_URL
            .strip_prefix("https://piston-meta.mojang.com")
            .unwrap();
        serve(server, idx_path, index).await;
        serve(server, "/m.json", manifest).await;
        for (n, b) in files {
            serve(server, &format!("/o/{n}"), b.to_vec()).await;
        }
    }

    #[tokio::test]
    async fn runtime_install_reports_bytes() {
        let server = MockServer::start().await;
        serve_fake_runtime(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let rt = ManagedRuntimes::new(tmp.path().join("runtimes"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        rt.install(&downloader(&server), "jre-legacy", &Some(tx))
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Ok(p) = rx.try_recv() {
            events.push(p);
        }
        assert!(!events.is_empty());
        assert!(
            events
                .iter()
                .all(|p| p.phase == Phase::DownloadRuntime && p.unit == Unit::Bytes),
            "{events:?}"
        );
        let last = events.last().unwrap();
        assert!(last.total > 0 && last.done == last.total, "{last:?}");
        assert!(events.windows(2).all(|w| w[0].done <= w[1].done));
    }

    #[tokio::test]
    async fn installs_from_fake_mojang() {
        let server = MockServer::start().await;
        serve_fake_runtime(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let rt = ManagedRuntimes::new(tmp.path().join("runtimes"));
        let dl = downloader(&server);
        assert!(!rt.is_installed("jre-legacy"));
        let bin = rt.install(&dl, "jre-legacy", &None).await.unwrap();
        assert_eq!(bin, rt.java_bin("jre-legacy"));
        use std::os::unix::fs::PermissionsExt;
        assert!(bin.is_file());
        assert!(std::fs::metadata(&bin).unwrap().permissions().mode() & 0o111 != 0);
        let alias = rt.home("jre-legacy").join("lib/alias.so");
        assert!(
            std::fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&alias).unwrap(), b"jli-lib");
        assert!(rt.is_installed("jre-legacy"));
        assert_eq!(
            rt.list_installed(),
            vec![("jre-legacy".to_string(), "8u202".to_string())]
        );
        // reinstall over existing link works
        rt.install(&dl, "jre-legacy", &None).await.unwrap();
        // a failing reinstall (index lacks the component) clears the marker
        let dead = MockServer::start().await;
        let idx = RUNTIME_INDEX_URL
            .strip_prefix("https://piston-meta.mojang.com")
            .unwrap();
        serve(&dead, idx, br#"{"linux":{}}"#.to_vec()).await;
        assert!(
            rt.install(&downloader(&dead), "jre-legacy", &None)
                .await
                .is_err()
        );
        assert!(!rt.is_installed("jre-legacy"));
        rt.install(&dl, "jre-legacy", &None).await.unwrap();
        assert!(rt.is_installed("jre-legacy"));
        rt.remove("jre-legacy").await.unwrap();
        assert!(!rt.is_installed("jre-legacy"));
        rt.remove("jre-legacy").await.unwrap();
    }

    fn hits(reqs: &[wiremock::Request], path: &str) -> usize {
        reqs.iter().filter(|r| r.url.path() == path).count()
    }

    /// Regression: two launches needing the same not-yet-installed runtime
    /// raced on the marker and the symlinks (EEXIST). The second install now
    /// waits for the first and reuses its result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_installs_of_one_component_download_once() {
        let server = MockServer::start().await;
        serve_fake_runtime(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("runtimes");
        let dl = downloader(&server);
        // Separate handles, as two launches would create them.
        let (a, b) = (
            ManagedRuntimes::new(root.clone()),
            ManagedRuntimes::new(root.clone()),
        );
        let (ra, rb) = tokio::join!(
            a.install(&dl, "jre-legacy", &None),
            b.install(&dl, "jre-legacy", &None)
        );
        assert_eq!(ra.unwrap(), a.java_bin("jre-legacy"));
        assert_eq!(rb.unwrap(), a.java_bin("jre-legacy"));
        assert!(a.is_installed("jre-legacy"));
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(hits(&reqs, "/m.json"), 1, "manifest fetched once");
        assert_eq!(hits(&reqs, "/o/java"), 1, "files downloaded once");

        // A link that points elsewhere is replaced on reinstall.
        let alias = a.home("jre-legacy").join("lib/alias.so");
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink("release", &alias).unwrap();
        a.install(&dl, "jre-legacy", &None).await.unwrap();
        assert_eq!(std::fs::read_link(&alias).unwrap(), Path::new("libjli.so"));
    }

    /// `remove` waits for a running install instead of deleting under it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remove_waits_for_a_running_install() {
        let server = MockServer::start().await;
        serve_fake_runtime(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let rt = ManagedRuntimes::new(tmp.path().join("runtimes"));
        let dl = downloader(&server);
        let (installed, removed) = tokio::join!(rt.install(&dl, "jre-legacy", &None), async {
            tokio::task::yield_now().await;
            rt.remove("jre-legacy").await
        });
        installed.unwrap();
        removed.unwrap();
        // Either order leaves a consistent state: installed with every file,
        // or fully removed.
        let home = rt.home("jre-legacy");
        if rt.is_installed("jre-legacy") {
            assert!(rt.java_bin("jre-legacy").is_file());
        } else {
            assert!(!home.exists());
        }
    }

    #[tokio::test]
    async fn unknown_component_is_unavailable() {
        let server = MockServer::start().await;
        let idx_path = RUNTIME_INDEX_URL
            .strip_prefix("https://piston-meta.mojang.com")
            .unwrap();
        serve(
            &server,
            idx_path,
            br#"{"linux":{"minecraft-java-exe":[]}}"#.to_vec(),
        )
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let rt = ManagedRuntimes::new(tmp.path().to_path_buf());
        let dl = downloader(&server);
        for c in ["nope", "minecraft-java-exe"] {
            let r = rt.install(&dl, c, &None).await;
            assert!(matches!(r, Err(RuntimeError::Unavailable { .. })));
        }
    }
}
