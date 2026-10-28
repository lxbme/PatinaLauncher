//! Instance storage: one directory per instance with `instance.toml`.

use crate::config::{JavaSettings, write_atomic};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("实例配置格式错误：{0}")]
    Parse(#[from] toml::de::Error),
    #[error("序列化实例配置失败：{0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("找不到实例：{0}")]
    NotFound(String),
    #[error("无效的实例 ID：{0}")]
    InvalidId(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InstanceConfig {
    pub name: String,
    pub mc_version: String,
    /// `None` means use the global settings.
    pub java_override: Option<JavaSettings>,
    /// Unix seconds.
    pub created_at: u64,
    /// Unix seconds.
    pub last_played: Option<u64>,
}

pub fn effective_java<'a>(global: &'a JavaSettings, inst: &'a InstanceConfig) -> &'a JavaSettings {
    inst.java_override.as_ref().unwrap_or(global)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub id: String,
    pub dir: PathBuf,
    pub config: InstanceConfig,
}

impl Instance {
    pub fn game_dir(&self) -> PathBuf {
        self.dir.join("minecraft")
    }
    pub fn natives_dir(&self) -> PathBuf {
        self.dir.join("natives")
    }
}

const CONFIG_FILE: &str = "instance.toml";

pub fn sanitize_dir_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = replaced.trim_matches(|c| c == ' ' || c == '.');
    if trimmed.is_empty() {
        return "instance".to_string();
    }
    if is_reserved(trimmed) {
        return format!("_{trimmed}");
    }
    trimmed.to_string()
}

fn is_reserved(s: &str) -> bool {
    // Windows treats "CON.txt" as reserved too, so only look at the stem.
    let stem = s.split('.').next().unwrap_or(s).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|p| {
            stem.strip_prefix(p)
                .is_some_and(|d| d.len() == 1 && matches!(d.as_bytes()[0], b'1'..=b'9'))
        })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct InstanceStore {
    root: PathBuf,
}

impl InstanceStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn check_id(id: &str) -> Result<(), InstanceError> {
        if id.is_empty() || id != sanitize_dir_name(id) {
            return Err(InstanceError::InvalidId(id.to_string()));
        }
        Ok(())
    }

    fn read(&self, id: &str) -> Result<Instance, InstanceError> {
        let dir = self.root.join(id);
        let text = match std::fs::read_to_string(dir.join(CONFIG_FILE)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(InstanceError::NotFound(id.to_string()));
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Instance {
            id: id.to_string(),
            dir,
            config: toml::from_str(&text)?,
        })
    }

    /// Sorted by `last_played` descending (never played last), then name.
    pub fn list(&self) -> Result<Vec<Instance>, InstanceError> {
        let rd = match std::fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in rd {
            let entry = entry?;
            if !entry.path().join(CONFIG_FILE).is_file() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            out.push(self.read(&id)?);
        }
        out.sort_by(|a, b| {
            b.config
                .last_played
                .cmp(&a.config.last_played)
                .then_with(|| a.config.name.cmp(&b.config.name))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    pub fn create(&self, name: &str, mc_version: &str) -> Result<Instance, InstanceError> {
        std::fs::create_dir_all(&self.root)?;
        let base = sanitize_dir_name(name);
        let mut n = 1u32;
        let (id, dir) = loop {
            let id = if n == 1 {
                base.clone()
            } else {
                format!("{base} ({n})")
            };
            let dir = self.root.join(&id);
            match std::fs::create_dir(&dir) {
                Ok(()) => break (id, dir),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e.into()),
            }
        };
        let inst = Instance {
            id,
            dir,
            config: InstanceConfig {
                name: name.to_string(),
                mc_version: mc_version.to_string(),
                java_override: None,
                created_at: now_secs(),
                last_played: None,
            },
        };
        std::fs::create_dir_all(inst.game_dir())?;
        self.save(&inst)?;
        Ok(inst)
    }

    pub fn load(&self, id: &str) -> Result<Instance, InstanceError> {
        Self::check_id(id)?;
        self.read(id)
    }

    pub fn save(&self, inst: &Instance) -> Result<(), InstanceError> {
        Self::check_id(&inst.id)?;
        let s = toml::to_string_pretty(&inst.config)?;
        write_atomic(&self.root.join(&inst.id).join(CONFIG_FILE), s.as_bytes())?;
        Ok(())
    }

    /// Changes the display name only; the directory id stays.
    pub fn rename(&self, id: &str, new_name: &str) -> Result<Instance, InstanceError> {
        let mut inst = self.load(id)?;
        inst.config.name = new_name.to_string();
        self.save(&inst)?;
        Ok(inst)
    }

    pub fn delete(&self, id: &str) -> Result<(), InstanceError> {
        Self::check_id(id)?;
        let dir = self.root.join(id);
        if !dir.join(CONFIG_FILE).is_file() {
            return Err(InstanceError::NotFound(id.to_string()));
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::JavaSettings;

    #[test]
    fn create_sanitizes_and_dedups() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let a = store.create("我的 生存/服", "1.21.4").unwrap();
        assert_eq!(a.id, "我的 生存_服");
        assert_eq!(a.config.name, "我的 生存/服");
        assert!(a.dir.join("instance.toml").is_file());
        assert!(a.game_dir().is_dir());
        let b = store.create("我的 生存/服", "1.21.4").unwrap();
        assert_eq!(b.id, "我的 生存_服 (2)");
    }

    #[test]
    fn sanitize_edge_cases() {
        assert_eq!(sanitize_dir_name("  ..  "), "instance");
        assert_eq!(sanitize_dir_name("a:b*c?\"<>|\\"), "a_b_c______");
        assert_eq!(sanitize_dir_name("CON"), "_CON");
        assert_eq!(sanitize_dir_name("tab\there"), "tab_here");
    }

    #[test]
    fn rename_changes_display_name_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let a = store.create("Old", "1.8.9").unwrap();
        let r = store.rename(&a.id, "新名字").unwrap();
        assert_eq!(r.id, "Old");
        assert_eq!(store.load("Old").unwrap().config.name, "新名字");
    }

    #[test]
    fn delete_removes_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let a = store.create("X", "1.20").unwrap();
        store.delete(&a.id).unwrap();
        assert!(!a.dir.exists());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_sorted_by_last_played_then_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let mut a = store.create("A", "1.20").unwrap();
        let mut b = store.create("B", "1.20").unwrap();
        let mut c = store.create("C", "1.20").unwrap();
        a.config.last_played = None;
        b.config.last_played = Some(100);
        c.config.last_played = Some(200);
        for i in [&a, &b, &c] {
            store.save(i).unwrap();
        }
        let ids: Vec<_> = store.list().unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, ["C", "B", "A"]);
    }

    #[test]
    fn list_ignores_dirs_without_config_and_name_tiebreak() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        std::fs::create_dir(tmp.path().join("junk")).unwrap();
        store.create("b", "1").unwrap();
        store.create("a", "1").unwrap();
        let ids: Vec<_> = store.list().unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn load_rejects_path_traversal_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        assert!(store.load("../x").is_err());
        assert!(matches!(
            store.load("nope"),
            Err(InstanceError::NotFound(_))
        ));
    }

    #[test]
    fn effective_java_prefers_override() {
        let g = JavaSettings::default();
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let mut i = store.create("J", "1.20").unwrap();
        assert_eq!(effective_java(&g, &i.config), &g);
        let o = JavaSettings {
            min_mem_mib: 1024,
            ..Default::default()
        };
        i.config.java_override = Some(o.clone());
        store.save(&i).unwrap();
        let re = store.load("J").unwrap();
        assert_eq!(effective_java(&g, &re.config), &o);
    }
}
