//! In-process locks: one operation at a time per instance, and shared /
//! exclusive access to a installed loader or vanilla version, so a version a
//! running game uses is never rewritten.
//!
//! All locks are non-blocking try-locks and only cover this process;
//! cross-process exclusion is out of scope. Guards are owned and release on
//! drop. The registry mutex is never held across an `.await`.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

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
        let k = key(root, id);
        if !self.reg().instances.insert(k.clone()) {
            return Err(Busy {
                what: BusyKind::Instance(id.to_string()),
            });
        }
        Ok(InstanceGuard {
            locks: self.clone(),
            key: k,
        })
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
        Ok(VersionShared {
            locks: self.clone(),
            key: k,
        })
    }

    pub fn version_exclusive(
        &self,
        versions_dir: &Path,
        id: &str,
    ) -> Result<VersionExclusive, Busy> {
        let k = key(versions_dir, id);
        {
            let mut reg = self.reg();
            if reg.versions.contains_key(&k) {
                return Err(busy_version(id));
            }
            reg.versions.insert(k.clone(), Hold::Exclusive);
        }
        Ok(VersionExclusive {
            locks: self.clone(),
            key: k,
        })
    }

    /// Exclusive use of the processor outputs shared by every Forge/NeoForge
    /// build of Minecraft `mc`; `versions_dir` is the `versions` directory.
    pub fn processors(&self, versions_dir: &Path, mc: &str) -> Result<VersionExclusive, Busy> {
        self.version_exclusive(versions_dir, &processors_key(mc))
            .map_err(|_| Busy {
                what: BusyKind::Processors(mc.to_string()),
            })
    }
}

fn busy_version(id: &str) -> Busy {
    Busy {
        what: BusyKind::Version(id.to_string()),
    }
}

/// Holds an instance for one operation.
#[derive(Debug)]
pub struct InstanceGuard {
    locks: Locks,
    key: Key,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        self.locks.reg().instances.remove(&self.key);
    }
}

/// Shared use of a version (e.g. a running game).
#[derive(Debug)]
pub struct VersionShared {
    locks: Locks,
    key: Key,
}

impl Drop for VersionShared {
    fn drop(&mut self) {
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
}

impl Drop for VersionExclusive {
    fn drop(&mut self) {
        self.locks.reg().versions.remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Language, locale_lock, set_language};

    const R: &str = "/r";
    fn r() -> &'static Path {
        Path::new(R)
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
        let l = Locks::new();
        let g = l.instance(r(), "a").unwrap();
        assert!(l.instance(r(), "a").is_err());
        assert!(l.instance(r(), "b").is_ok());
        assert!(l.instance(Path::new("/other"), "a").is_ok());
        drop(g);
        assert!(l.instance(r(), "a").is_ok());
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
        let l = Locks::new();
        let s = l.version_shared(r(), "v").unwrap();
        let e = l.version_exclusive(r(), "v").unwrap_err();
        assert_eq!(e.what, BusyKind::Version("v".into()));
        assert!(l.version_exclusive(Path::new("/other"), "v").is_ok());
        drop(s);
        let x = l.version_exclusive(r(), "v").unwrap();
        assert!(l.version_exclusive(r(), "v").is_err());
        assert!(l.version_shared(r(), "v").is_err());
        drop(x);
        assert!(l.version_shared(r(), "v").is_ok());
        assert!(l.reg().versions.is_empty());
    }

    #[test]
    fn two_instances_share_a_version() {
        let l = Locks::new();
        let a = l.version_shared(r(), "v").unwrap();
        let b = l.version_shared(r(), "v").unwrap();
        drop(a);
        assert!(l.version_exclusive(r(), "v").is_err());
        drop(b);
        assert!(l.version_exclusive(r(), "v").is_ok());
    }

    #[test]
    fn busy_is_localized() {
        let _g = locale_lock();
        let l = Locks::new();
        let _i = l.instance(r(), "x").unwrap();
        let _v = l.version_shared(r(), "y").unwrap();
        let i = l.instance(r(), "x").unwrap_err();
        let v = l.version_exclusive(r(), "y").unwrap_err();
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
        let l = Locks::new();
        let held = l.processors(r(), "1.20.1").unwrap();
        let e = l.processors(r(), "1.20.1").unwrap_err();
        assert_eq!(e.what, BusyKind::Processors("1.20.1".into()));
        assert!(l.processors(r(), "1.21").is_ok());
        assert!(
            l.version_exclusive(r(), "1.20.1").is_ok(),
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
        assert!(l.processors(r(), "1.20.1").is_ok());
    }
}
