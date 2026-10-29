//! Data directory layout.

use crate::platform;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirs {
    root: PathBuf,
}

impl DataDirs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Order: `PATINA_DATA_DIR`, then `fallback`, then `./patina-data`.
    pub fn resolve(env: impl Fn(&str) -> Option<String>, fallback: Option<PathBuf>) -> Self {
        if let Some(v) = env("PATINA_DATA_DIR").filter(|v| !v.is_empty()) {
            return Self::new(v);
        }
        Self::new(fallback.unwrap_or_else(|| PathBuf::from("./patina-data")))
    }

    pub fn from_env() -> Self {
        Self::resolve(|k| std::env::var(k).ok(), platform::default_data_dir())
    }

    pub fn root(&self) -> PathBuf {
        self.root.clone()
    }
    pub fn instances(&self) -> PathBuf {
        self.root.join("instances")
    }
    pub fn libraries(&self) -> PathBuf {
        self.root.join("libraries")
    }
    pub fn assets(&self) -> PathBuf {
        self.root.join("assets")
    }
    /// Shared use of version `id` of this data dir (see [`crate::locks`]).
    pub fn lock_version_shared(
        &self,
        id: &str,
    ) -> Result<crate::locks::VersionShared, crate::locks::Busy> {
        crate::locks::Locks::global().version_shared(&self.versions(), id)
    }

    /// Exclusive access to version `id` of this data dir.
    pub fn lock_version_exclusive(
        &self,
        id: &str,
    ) -> Result<crate::locks::VersionExclusive, crate::locks::Busy> {
        crate::locks::Locks::global().version_exclusive(&self.versions(), id)
    }

    pub fn versions(&self) -> PathBuf {
        self.root.join("versions")
    }
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }
    pub fn runtimes(&self) -> PathBuf {
        self.root.join("runtimes")
    }
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn data_dirs_resolution_order() {
        let d = DataDirs::resolve(
            |k| (k == "PATINA_DATA_DIR").then(|| "/x/数据 目录".into()),
            Some("/fallback".into()),
        );
        assert_eq!(d.root(), Path::new("/x/数据 目录"));
        let d = DataDirs::resolve(|_| None, Some("/fallback".into()));
        assert_eq!(d.instances(), PathBuf::from("/fallback/instances"));
        assert_eq!(d.config_file(), PathBuf::from("/fallback/config.toml"));
        let d = DataDirs::resolve(|_| None, None);
        assert_eq!(d.root(), Path::new("./patina-data"));
    }
}
