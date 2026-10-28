//! Global configuration (`config.toml`).

use crate::jvm::{GcPreset, default_max_mem_mib};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("读取或写入配置文件失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("配置文件格式错误：{0}")]
    Parse(#[from] toml::de::Error),
    #[error("序列化配置失败：{0}")]
    Serialize(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceChoice {
    #[default]
    Auto,
    Official,
    Bmclapi,
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
        write_atomic(path, s.as_bytes())?;
        Ok(())
    }
}

pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

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
