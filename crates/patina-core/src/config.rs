//! Global configuration (`config.toml`).

use crate::jvm::{GcPreset, default_max_mem_mib};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}", rust_i18n::t!("errors.config.io", error = .0))]
    Io(String),
    #[error("{}", rust_i18n::t!("errors.config.parse", error = .0))]
    Parse(String),
    #[error("{}", rust_i18n::t!("errors.config.serialize", error = .0))]
    Serialize(String),
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e.to_string())
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Parse(e.to_string())
    }
}

impl From<toml::ser::Error> for ConfigError {
    fn from(e: toml::ser::Error) -> Self {
        ConfigError::Serialize(e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceChoice {
    #[default]
    Auto,
    Official,
    Bmclapi,
}

impl SourceChoice {
    /// Localized label for the settings UI.
    pub fn display_name(self) -> String {
        match self {
            SourceChoice::Auto => rust_i18n::t!("source.auto"),
            SourceChoice::Official => rust_i18n::t!("source.official"),
            SourceChoice::Bmclapi => rust_i18n::t!("source.bmclapi"),
        }
        .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadConfig {
    pub source: SourceChoice,
    /// Mirror id picked by the most recent speed test.
    pub auto_pick: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JavaSettings {
    /// `None` means use the managed Java.
    pub java_path: Option<PathBuf>,
    pub min_mem_mib: u32,
    /// `None` means automatic.
    pub max_mem_mib: Option<u32>,
    pub gc_preset: GcPreset,
    pub extra_jvm_args: String,
    pub low_mem_warning: bool,
}

impl Default for JavaSettings {
    fn default() -> Self {
        Self {
            java_path: None,
            min_mem_mib: 512,
            max_mem_mib: None,
            gc_preset: GcPreset::None,
            extra_jvm_args: String::new(),
            low_mem_warning: true,
        }
    }
}

impl JavaSettings {
    /// Explicit `max_mem_mib` wins; otherwise derive from physical memory.
    pub fn effective_max_mem(&self, total_mib: u64) -> u32 {
        self.max_mem_mib
            .unwrap_or_else(|| default_max_mem_mib(total_mib))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    pub download: DownloadConfig,
    pub java: JavaSettings,
}

impl GlobalConfig {
    /// Missing file yields the default config.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes to a temp file next to `path`, then renames.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let s = toml::to_string_pretty(self)?;
        crate::fsutil::write_atomic(path, s.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_errors_have_no_source() {
        use std::error::Error;
        let de = toml::from_str::<toml::Value>("x = [").unwrap_err();
        let errs: Vec<ConfigError> = vec![std::io::Error::other("boom").into(), de.into()];
        for e in errs {
            assert!(e.source().is_none(), "{e:?}");
        }
    }

    #[test]
    fn global_config_roundtrip_and_missing_default() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        let c = GlobalConfig::load(&p).unwrap();
        assert_eq!(c.download.source, SourceChoice::Auto);
        assert_eq!(c.java.min_mem_mib, 512);
        assert!(c.java.low_mem_warning);
        let mut c2 = c.clone();
        c2.download.source = SourceChoice::Bmclapi;
        c2.save(&p).unwrap();
        assert_eq!(
            GlobalConfig::load(&p).unwrap().download.source,
            SourceChoice::Bmclapi
        );
    }

    #[test]
    fn source_choice_display_names_are_localized() {
        use crate::i18n::{Language, locale_lock, set_language};
        let _locale = locale_lock();
        set_language(Language::ZhCn);
        assert_eq!(SourceChoice::Auto.display_name(), "自动（测速选择）");
        assert_eq!(SourceChoice::Official.display_name(), "优先使用官方源");
        assert_eq!(SourceChoice::Bmclapi.display_name(), "优先使用 BMCLAPI");
        for c in [
            SourceChoice::Auto,
            SourceChoice::Official,
            SourceChoice::Bmclapi,
        ] {
            assert!(!c.display_name().contains("固定"));
        }
        set_language(Language::En);
        assert_eq!(SourceChoice::Auto.display_name(), "Auto (fastest)");
        assert_eq!(SourceChoice::Official.display_name(), "Prefer official");
        assert_eq!(SourceChoice::Bmclapi.display_name(), "Prefer BMCLAPI");
        set_language(Language::ZhCn);
    }

    #[test]
    fn partial_file_loads_with_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        std::fs::write(&p, "[java]\nmax_mem_mib = 2048\n").unwrap();
        let c = GlobalConfig::load(&p).unwrap();
        assert_eq!(c.java.max_mem_mib, Some(2048));
        assert_eq!(c.java.min_mem_mib, 512);
        assert!(c.java.low_mem_warning);
    }

    #[test]
    fn effective_max_mem_explicit_wins_else_default() {
        let mut j = JavaSettings::default();
        assert_eq!(j.effective_max_mem(4096), 2730);
        j.max_mem_mib = Some(1024);
        assert_eq!(j.effective_max_mem(32768), 1024);
    }

    #[test]
    fn invalid_toml_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        std::fs::write(&p, "not = [valid").unwrap();
        assert!(matches!(GlobalConfig::load(&p), Err(ConfigError::Parse(_))));
    }
}
