//! Locks: one operation at a time per instance, and shared / exclusive
//! access to an installed loader or vanilla version, so a version a running
//! game uses is never rewritten.
//!
//! Each lock has two layers, taken in this order and released together:
//!
//! 1. an in-process registry (fast path; also covers names that have no
//!    directory on disk);
//! 2. an OS file lock (`flock` on Linux via std's [`File::try_lock`]) on a
//!    lock file, so other Patina processes (the app, `patina-dev`) see it:
//!    - instance: `<instance dir>/.patina.lock`, only while that directory
//!      exists (a lock never creates an instance directory);
//!    - version: `<versions>/<id>/.patina.lock` (directory created as needed);
//!    - processors: `<versions>/.processors-<mc>.lock`.
//!
//! A name that is not a safe single path component has no directory and so
//! no lock file; only the in-process layer applies to it. Lock files are
//! never truncated or deleted.
//!
//! So a version directory may exist holding nothing but `.patina.lock` (a
//! lock taken for a version that was never installed, or whose install
//! failed). A version counts as installed only when `<versions>/<id>/<id>.json`
//! exists (and, for loaders, passes the install check); never test for the
//! directory itself.
//!
//! All locks are try-locks: anything that keeps the file lock from being
//! taken (held elsewhere, or an I/O error, which is logged) is [`Busy`].
//! **Lock calls may block** for up to 20 ms: a held file lock is retried
//! that long first, because a child being spawned briefly shares the open
//! lock files of this process. Guards are owned and release on drop. The
//! registry mutex is never held across an `.await` or a file operation.

use std::collections::{HashMap, HashSet};
use std::fs::{File, TryLockError};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::fsutil::is_safe_name;

/// Lock file inside an instance directory and a version directory.
pub const LOCK_FILE: &str = ".patina.lock";

/// How long a held file lock is retried before it counts as busy (see
/// [`lock_file`]).
const RELEASE_GRACE: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusyKind {
    Instance(String),
    Version(String),
    /// Forge/NeoForge processors for this Minecraft version are running.
    Processors(String),
}

/// A lock could not be taken because someone else holds it.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{}", match .what {
    BusyKind::Instance(id) => rust_i18n::t!("errors.locks.instance_busy", id = id),
    BusyKind::Version(id) => rust_i18n::t!("errors.locks.version_busy", id = id),
    BusyKind::Processors(mc) => rust_i18n::t!("errors.locks.processors_busy", mc = mc),
})]
pub struct Busy {
    pub what: BusyKind,
}

/// Lock key: the absolute root directory the name lives in, plus the name,
/// so two data dirs (or test temp dirs) never collide.
type Key = (PathBuf, String);

#[derive(Debug, Clone, Copy)]
enum Hold {
    Shared(NonZeroUsize),
    Exclusive,
}

#[derive(Default, Debug)]
struct Registry {
    instances: HashSet<Key>,
    versions: HashMap<Key, Hold>,
}

/// Lock ordering (to avoid deadlock; all locks are try-locks today, but keep
/// the order): instance, then version profile (`<profile id>`), then
/// `processors:<mc>`.
fn processors_key(mc: &str) -> String {
    format!("processors:{mc}")
}

#[derive(Clone, Default, Debug)]
pub struct Locks {
    inner: Arc<Mutex<Registry>>,
}

/// Same path normalization as the download destination locks.
fn key(root: &Path, id: &str) -> Key {
    let root = crate::fsutil::path_key(root).unwrap_or_else(|_| root.to_path_buf());
    (root, id.to_string())
}

/// Where a lock file lives, and whether its directory may be created.
enum LockFile {
    /// No on-disk counterpart: in-process lock only.
    None,
    /// The directory must already exist; if it vanished meanwhile, the lock
    /// is in-process only.
    InExisting(PathBuf),
    /// The directory is created as needed.
    Create(PathBuf),
}

fn instance_lock_file(root: &Path, id: &str) -> LockFile {
    if !is_safe_name(id) {
        return LockFile::None;
    }
    let dir = root.join(id);
    if dir.is_dir() {
        LockFile::InExisting(dir.join(LOCK_FILE))
    } else {
        LockFile::None
    }
}

fn version_lock_file(versions_dir: &Path, id: &str) -> LockFile {
    if is_safe_name(id) {
        LockFile::Create(versions_dir.join(id).join(LOCK_FILE))
    } else {
        LockFile::None
    }
}

fn processors_lock_file(versions_dir: &Path, mc: &str) -> LockFile {
    if is_safe_name(mc) {
        LockFile::Create(versions_dir.join(format!(".processors-{mc}.lock")))
    } else {
        LockFile::None
    }
}

/// Takes the file layer of a lock. `Ok(None)` when there is no lock file;
/// `Err(())` when it is busy or cannot be locked (logged).
fn lock_file(at: LockFile, shared: bool) -> Result<Option<File>, ()> {
    let (path, create_dir) = match at {
        LockFile::None => return Ok(None),
        LockFile::InExisting(p) => (p, false),
        LockFile::Create(p) => (p, true),
    };
    if create_dir
        && let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        tracing::warn!("cannot create lock directory {}: {e}", dir.display());
        return Err(());
    }
    let file = match File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if !create_dir && e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!("{} is gone; in-process lock only", path.display());
            return Ok(None);
        }
        Err(e) => {
            tracing::warn!("cannot open lock file {}: {e}", path.display());
            return Err(());
        }
    };
    let try_once = || {
        if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        }
    };
    // While any thread of this process spawns a child, the child shares
    // every open file of the process until its exec closes them, so a lock
    // just released here can look held for a moment. Ride that out briefly.
    let deadline = Instant::now() + RELEASE_GRACE;
    let mut pause = Duration::from_micros(250);
    let r = loop {
        match try_once() {
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(8));
            }
            r => break r,
        }
    };
    match r {
        // The file may have been unlinked (its directory deleted) between
        // open and lock; a lock on an orphaned file guards nothing.
        Ok(()) if still_at(&file, &path) => Ok(Some(file)),
        Ok(()) => {
            tracing::debug!("lock file {} was replaced or removed", path.display());
            Err(())
        }
        Err(TryLockError::WouldBlock) => Err(()),
        Err(TryLockError::Error(e)) => {
            tracing::warn!("cannot lock {}: {e}", path.display());
            Err(())
        }
    }
}

/// Whether `path` still names the open `file`.
fn still_at(file: &File, path: &Path) -> bool {
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => crate::platform::same_file(&a, &b),
        _ => false,
    }
}

impl Locks {
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-wide registry.
    pub fn global() -> &'static Locks {
        static GLOBAL: OnceLock<Locks> = OnceLock::new();
        GLOBAL.get_or_init(Locks::new)
    }

    fn reg(&self) -> MutexGuard<'_, Registry> {
        // The registry is always left consistent, so a poisoned lock is usable.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `root` is the instances directory.
    pub fn instance(&self, root: &Path, id: &str) -> Result<InstanceGuard, Busy> {
        let busy = || Busy {
            what: BusyKind::Instance(id.to_string()),
        };
        let k = key(root, id);
        if !self.reg().instances.insert(k.clone()) {
            return Err(busy());
        }
        let mut guard = InstanceGuard {
            locks: self.clone(),
            key: k,
            file: None,
        };
        // On failure the guard drops and releases the in-process hold.
        guard.file = lock_file(instance_lock_file(root, id), false).map_err(|()| busy())?;
        Ok(guard)
    }

    /// `versions_dir` is the `versions` directory.
    pub fn version_shared(&self, versions_dir: &Path, id: &str) -> Result<VersionShared, Busy> {
        let k = key(versions_dir, id);
        {
            let mut reg = self.reg();
            let next = match reg.versions.get(&k) {
                None => NonZeroUsize::MIN,
                Some(Hold::Shared(n)) => n.saturating_add(1),
                Some(Hold::Exclusive) => return Err(busy_version(id)),
            };
            reg.versions.insert(k.clone(), Hold::Shared(next));
        }
        let mut guard = VersionShared {
            locks: self.clone(),
            key: k,
            file: None,
        };
        guard.file =
            lock_file(version_lock_file(versions_dir, id), true).map_err(|()| busy_version(id))?;
        Ok(guard)
    }

    pub fn version_exclusive(
        &self,
        versions_dir: &Path,
        id: &str,
    ) -> Result<VersionExclusive, Busy> {
        self.exclusive(versions_dir, id, version_lock_file(versions_dir, id))
            .ok_or_else(|| busy_version(id))
    }

    /// Exclusive use of the processor outputs shared by every Forge/NeoForge
    /// build of Minecraft `mc`; `versions_dir` is the `versions` directory.
    pub fn processors(&self, versions_dir: &Path, mc: &str) -> Result<VersionExclusive, Busy> {
        self.exclusive(
            versions_dir,
            &processors_key(mc),
            processors_lock_file(versions_dir, mc),
        )
        .ok_or_else(|| Busy {
            what: BusyKind::Processors(mc.to_string()),
        })
    }

    /// `None` when busy.
    fn exclusive(&self, versions_dir: &Path, id: &str, at: LockFile) -> Option<VersionExclusive> {
        let k = key(versions_dir, id);
        {
            let mut reg = self.reg();
            if reg.versions.contains_key(&k) {
                return None;
            }
            reg.versions.insert(k.clone(), Hold::Exclusive);
        }
        let mut guard = VersionExclusive {
            locks: self.clone(),
            key: k,
            file: None,
        };
        guard.file = lock_file(at, false).ok()?;
        Some(guard)
    }
}

fn busy_version(id: &str) -> Busy {
    Busy {
        what: BusyKind::Version(id.to_string()),
    }
}

// Each guard drops its file (releasing the OS lock) before it leaves the
// registry, so a thread that gets past the registry never finds the file
// still locked by this process.

/// Holds an instance for one operation.
#[derive(Debug)]
pub struct InstanceGuard {
    locks: Locks,
    key: Key,
    file: Option<File>,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        drop(self.file.take());
        self.locks.reg().instances.remove(&self.key);
    }
}

/// Shared use of a version (e.g. a running game).
#[derive(Debug)]
pub struct VersionShared {
    locks: Locks,
    key: Key,
    file: Option<File>,
}

impl Drop for VersionShared {
    fn drop(&mut self) {
        drop(self.file.take());
        let mut reg = self.locks.reg();
        if let Some(Hold::Shared(n)) = reg.versions.get(&self.key).copied() {
            match NonZeroUsize::new(n.get() - 1) {
                Some(m) => {
                    reg.versions.insert(self.key.clone(), Hold::Shared(m));
                }
                None => {
                    reg.versions.remove(&self.key);
                }
            }
        }
    }
}

/// Exclusive access to a version, to (re)install its files.
#[derive(Debug)]
pub struct VersionExclusive {
    locks: Locks,
    key: Key,
    file: Option<File>,
}

impl Drop for VersionExclusive {
    fn drop(&mut self) {
        drop(self.file.take());
        self.locks.reg().versions.remove(&self.key);
    }
}

/// Takes a file lock for a test acting as another process, polling for up to
/// 2 s: right after this process released the lock, a child being spawned
/// by another test may still share the open file (see [`RELEASE_GRACE`]).
#[cfg(test)]
pub(crate) fn test_lock(file: &File, shared: bool) -> Result<(), TryLockError> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let r = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match r {
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            r => return r,
        }
    }
}

/// Whether `dir` (missing counts) holds no files other than lock files, at
/// any depth: what a failed install may leave behind.
#[cfg(test)]
pub(crate) fn only_lock_files(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return !dir.exists();
    };
    rd.flatten().all(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            only_lock_files(&e.path())
        } else {
            name == LOCK_FILE || (name.starts_with(".processors-") && name.ends_with(".lock"))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Language, locale_lock, set_language};
    use crate::instance::InstanceStore;
    use crate::paths::DataDirs;
    use std::fs::{File, TryLockError};
    use std::io::{BufRead, BufReader};
    use std::time::Duration;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// A second open file description of `path`: to the OS, as good as a
    /// handle held by another process.
    fn other_handle(path: &Path) -> File {
        File::options().read(true).write(true).open(path).unwrap()
    }

    fn would_block(r: Result<(), TryLockError>) -> bool {
        matches!(r, Err(TryLockError::WouldBlock))
    }

    fn assert_send_static<T: Send + 'static>() {}

    #[test]
    fn guards_are_send_and_static() {
        assert_send_static::<InstanceGuard>();
        assert_send_static::<VersionShared>();
        assert_send_static::<VersionExclusive>();
        assert_send_static::<Locks>();
    }

    #[test]
    fn instance_is_exclusive_and_released_on_drop() {
        let t = tmp();
        let r = t.path();
        let l = Locks::new();
        let g = l.instance(r, "a").unwrap();
        assert!(l.instance(r, "a").is_err());
        assert!(l.instance(r, "b").is_ok());
        assert!(l.instance(Path::new("/other"), "a").is_ok());
        drop(g);
        assert!(l.instance(r, "a").is_ok());
        assert!(l.reg().instances.is_empty());
    }

    #[test]
    fn spellings_of_one_root_share_a_lock() {
        let l = Locks::new();
        let _g = l.instance(Path::new("/r/instances"), "a").unwrap();
        for other in ["/r/./instances", "/r/x/../instances", "/r/instances/"] {
            assert!(l.instance(Path::new(other), "a").is_err(), "{other}");
        }
    }

    #[test]
    fn install_of_version_in_use_is_busy() {
        let (t, o) = (tmp(), tmp());
        let (r, other) = (t.path(), o.path());
        let l = Locks::new();
        let s = l.version_shared(r, "v").unwrap();
        let e = l.version_exclusive(r, "v").unwrap_err();
        assert_eq!(e.what, BusyKind::Version("v".into()));
        assert!(l.version_exclusive(other, "v").is_ok());
        drop(s);
        let x = l.version_exclusive(r, "v").unwrap();
        assert!(l.version_exclusive(r, "v").is_err());
        assert!(l.version_shared(r, "v").is_err());
        drop(x);
        assert!(l.version_shared(r, "v").is_ok());
        assert!(l.reg().versions.is_empty());
    }

    #[test]
    fn two_instances_share_a_version() {
        let t = tmp();
        let r = t.path();
        let l = Locks::new();
        let a = l.version_shared(r, "v").unwrap();
        let b = l.version_shared(r, "v").unwrap();
        drop(a);
        assert!(l.version_exclusive(r, "v").is_err());
        drop(b);
        assert!(l.version_exclusive(r, "v").is_ok());
    }

    #[test]
    fn busy_is_localized() {
        let _g = locale_lock();
        let t = tmp();
        let r = t.path();
        let l = Locks::new();
        let _i = l.instance(r, "x").unwrap();
        let _v = l.version_shared(r, "y").unwrap();
        let i = l.instance(r, "x").unwrap_err();
        let v = l.version_exclusive(r, "y").unwrap_err();
        set_language(Language::En);
        let (ie, ve) = (i.to_string(), v.to_string());
        assert!(ie.contains('x') && !ie.contains("errors.locks"), "{ie}");
        assert!(ve.contains('y') && !ve.contains("errors.locks"), "{ve}");
        set_language(Language::ZhCn);
        assert_ne!(i.to_string(), ie);
        assert_ne!(v.to_string(), ve);
    }

    #[test]
    fn processors_busy_names_the_minecraft_version_not_the_key() {
        let _g = locale_lock();
        let t = tmp();
        let r = t.path();
        let l = Locks::new();
        let held = l.processors(r, "1.20.1").unwrap();
        let e = l.processors(r, "1.20.1").unwrap_err();
        assert_eq!(e.what, BusyKind::Processors("1.20.1".into()));
        assert!(l.processors(r, "1.21").is_ok());
        assert!(
            l.version_exclusive(r, "1.20.1").is_ok(),
            "separate from the version"
        );
        for lang in [Language::En, Language::ZhCn] {
            set_language(lang);
            let msg = e.to_string();
            assert!(msg.contains("1.20.1"), "{msg}");
            assert!(
                !msg.contains("processors:") && !msg.contains("errors."),
                "{msg}"
            );
        }
        drop(held);
        assert!(l.processors(r, "1.20.1").is_ok());
    }

    #[test]
    fn file_lock_blocks_second_holder() {
        let t = tmp();
        let store = InstanceStore::new(t.path());
        std::fs::create_dir(t.path().join("a")).unwrap();
        let g = store.lock("a").unwrap();
        let other = other_handle(&t.path().join("a").join(LOCK_FILE));
        assert!(would_block(other.try_lock()), "held by the guard");
        assert!(would_block(other.try_lock_shared()));
        drop(g);
        test_lock(&other, false).expect("released with the guard");
        // Now "another process" holds it: this process sees the instance busy.
        let e = store.lock("a").unwrap_err();
        assert_eq!(e.what, BusyKind::Instance("a".into()));
        drop(other);
        drop(store.lock("a").expect("free again"));
    }

    #[test]
    fn shared_version_file_lock_allows_readers_blocks_writer() {
        let t = tmp();
        let versions = t.path().join("versions");
        let l = Locks::new();
        let s = l.version_shared(&versions, "v").unwrap();
        let path = versions.join("v").join(LOCK_FILE);
        let other = other_handle(&path);
        other.try_lock_shared().expect("readers share the version");
        other.unlock().unwrap();
        assert!(would_block(other.try_lock()), "a writer is blocked");
        drop(s);
        test_lock(&other, false).expect("released with the guard");
        // Another process installs the version: both kinds are busy here,
        // and the failed attempts leave no in-process hold behind.
        let e = l.version_shared(&versions, "v").unwrap_err();
        assert_eq!(e.what, BusyKind::Version("v".into()));
        assert!(l.version_exclusive(&versions, "v").is_err());
        assert!(l.reg().versions.is_empty());
        other.unlock().unwrap();
        // Another process reads it: shared is fine, exclusive is busy.
        test_lock(&other, true).unwrap();
        let a = l.version_shared(&versions, "v").unwrap();
        let b = l.version_shared(&versions, "v").unwrap();
        drop((a, b));
        assert!(l.version_exclusive(&versions, "v").is_err());
        assert!(l.reg().versions.is_empty());
        drop(other);
        assert!(l.version_exclusive(&versions, "v").is_ok());
    }

    #[test]
    fn processors_file_lock_is_per_minecraft_version() {
        let t = tmp();
        let data = DataDirs::new(t.path().to_path_buf());
        let held = data.lock_processors("1.20.1").unwrap();
        let path = data.versions().join(".processors-1.20.1.lock");
        let other = other_handle(&path);
        assert!(would_block(other.try_lock()));
        drop(held);
        test_lock(&other, false).unwrap();
        let e = data.lock_processors("1.20.1").unwrap_err();
        assert_eq!(e.what, BusyKind::Processors("1.20.1".into()));
        assert!(data.lock_processors("1.21").is_ok());
    }

    #[test]
    fn lock_of_missing_instance_creates_nothing() {
        let t = tmp();
        let store = InstanceStore::new(t.path());
        let g = store.lock("ghost").unwrap();
        assert!(store.lock("ghost").is_err(), "still exclusive in-process");
        drop(g);
        assert!(!t.path().join("ghost").exists());
    }

    #[test]
    fn version_lock_creates_its_directory_and_keeps_the_file() {
        let t = tmp();
        let versions = t.path().join("versions");
        let l = Locks::new();
        drop(l.version_exclusive(&versions, "v").unwrap());
        let path = versions.join("v").join(LOCK_FILE);
        assert!(path.is_file());
        // The content is never truncated.
        std::fs::write(&path, b"keep").unwrap();
        drop(l.version_shared(&versions, "v").unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }

    #[test]
    fn names_that_are_not_a_directory_get_no_lock_file() {
        let t = tmp();
        let l = Locks::new();
        for id in ["", "..", "a/b"] {
            let _v = l.version_shared(t.path(), id).unwrap();
            assert!(l.version_exclusive(t.path(), id).is_err(), "{id:?}");
            let _i = l.instance(t.path(), id).unwrap();
        }
        assert!(l.processors(t.path(), "../x").is_ok());
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_lock_on_a_replaced_or_removed_file_is_not_held() {
        let t = tmp();
        let path = t.path().join(LOCK_FILE);
        let file = File::create(&path).unwrap();
        assert!(still_at(&file, &path));
        std::fs::remove_file(&path).unwrap();
        assert!(!still_at(&file, &path), "removed");
        File::create(&path).unwrap();
        assert!(!still_at(&file, &path), "another file now");
    }

    #[test]
    fn sweep_keeps_lock_files() {
        let t = tmp();
        let data = DataDirs::new(t.path().to_path_buf());
        std::fs::create_dir_all(data.instances().join("a")).unwrap();
        let store = InstanceStore::new(data.instances());
        drop(store.lock("a").unwrap());
        drop(data.lock_version_exclusive("1.20.1").unwrap());
        drop(data.lock_processors("1.20.1").unwrap());
        let files = [
            data.instances().join("a").join(LOCK_FILE),
            data.versions().join("1.20.1").join(LOCK_FILE),
            data.versions().join(".processors-1.20.1.lock"),
        ];
        for f in &files {
            assert!(f.is_file(), "{}", f.display());
            let old = std::time::SystemTime::now() - Duration::from_secs(3600);
            File::options()
                .write(true)
                .open(f)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        crate::fsutil::sweep_owned_temps(t.path(), Duration::ZERO).unwrap();
        for f in &files {
            assert!(f.is_file(), "swept {}", f.display());
        }
    }

    const CHILD_ENV: &str = "PATINA_LOCK_CHILD";
    const CHILD_READY: &str = "patina-lock-child: locked";

    /// Not a test on its own: re-executed by
    /// `instance_lock_is_seen_by_another_process` as the other process. With
    /// `PATINA_LOCK_CHILD=<instances dir>` it locks instance `a` there and
    /// holds it until stdin closes; without it, it does nothing.
    #[test]
    #[ignore = "helper process for instance_lock_is_seen_by_another_process"]
    fn cross_process_lock_child() {
        let Some(root) = std::env::var_os(CHILD_ENV) else {
            return;
        };
        let _g = Locks::global()
            .instance(Path::new(&root), "a")
            .expect("child takes the lock");
        println!("{CHILD_READY}");
        let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new());
    }

    #[test]
    fn instance_lock_is_seen_by_another_process() {
        use std::process::{Command, Stdio};
        let t = tmp();
        std::fs::create_dir(t.path().join("a")).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "locks::tests::cross_process_lock_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, t.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // libtest may print "test <name> ... " on the same line first.
                if line.contains(CHILD_READY) {
                    let _ = tx.send(());
                }
            }
        });
        let ready = rx.recv_timeout(Duration::from_secs(60));
        let store = InstanceStore::new(t.path());
        let held = ready.is_ok().then(|| store.lock("a"));
        // Closing stdin lets the child exit; it is our own child either way.
        drop(child.stdin.take());
        if ready.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        assert!(ready.is_ok(), "child never took the lock");
        let e = held.unwrap().unwrap_err();
        assert_eq!(e.what, BusyKind::Instance("a".into()));
        assert!(status.success(), "{status:?}");
        drop(store.lock("a").expect("released when the child exited"));
    }
}
