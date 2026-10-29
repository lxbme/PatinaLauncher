//! Instance storage: one directory per instance with `instance.toml`.

use crate::config::JavaSettings;
use crate::fsutil::write_atomic;
use crate::loader::LoaderKind;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("{}", rust_i18n::t!("errors.instance.io", error = .0))]
    Io(#[from] std::io::Error),
    #[error("{}", rust_i18n::t!("errors.instance.parse", error = .0))]
    Parse(#[from] toml::de::Error),
    #[error("{}", rust_i18n::t!("errors.instance.serialize", error = .0))]
    Serialize(#[from] toml::ser::Error),
    #[error("{}", rust_i18n::t!("errors.instance.not_found", id = .0))]
    NotFound(String),
    #[error("{}", rust_i18n::t!("errors.instance.invalid_id", id = .0))]
    InvalidId(String),
    #[error("{}", rust_i18n::t!("errors.instance.invalid_config", reason = .0))]
    InvalidConfig(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentUid {
    Minecraft,
    Forge,
    Neoforge,
    Fabric,
}

impl ComponentUid {
    fn loader_kind(self) -> Option<LoaderKind> {
        match self {
            ComponentUid::Minecraft => None,
            ComponentUid::Forge => Some(LoaderKind::Forge),
            ComponentUid::Neoforge => Some(LoaderKind::Neoforge),
            ComponentUid::Fabric => Some(LoaderKind::Fabric),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    pub uid: ComponentUid,
    pub version: String,
    /// Id of the loader version JSON (`versions/<profile>/<profile>.json`).
    /// Always `None` for the minecraft component.
    #[serde(default)]
    pub profile: Option<String>,
}

impl Component {
    fn minecraft(version: &str) -> Self {
        Component {
            uid: ComponentUid::Minecraft,
            version: version.to_string(),
            profile: None,
        }
    }
}

/// On-disk shape; `mc_version` is only read, for plan-1 files.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    name: String,
    mc_version: String,
    components: Vec<Component>,
    java_override: Option<JavaSettings>,
    created_at: u64,
    last_played: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct InstanceConfig {
    pub name: String,
    /// Invariant: first is minecraft, at most one loader.
    pub components: Vec<Component>,
    /// `None` means use the global settings.
    pub java_override: Option<JavaSettings>,
    /// Unix seconds.
    pub created_at: u64,
    /// Unix seconds.
    pub last_played: Option<u64>,
}

impl InstanceConfig {
    fn from_raw(raw: RawConfig) -> Result<Self, InstanceError> {
        let mut components = raw.components;
        if components.is_empty() {
            components.push(Component::minecraft(&raw.mc_version));
        }
        let cfg = InstanceConfig {
            name: raw.name,
            components,
            java_override: raw.java_override,
            created_at: raw.created_at,
            last_played: raw.last_played,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), InstanceError> {
        let bad = |reason: String| Err(InstanceError::InvalidConfig(reason));
        match self.components.first() {
            Some(c) if c.uid == ComponentUid::Minecraft => {
                if c.version.trim().is_empty() {
                    return bad(
                        rust_i18n::t!("errors.instance.config_reason.empty_version").to_string()
                    );
                }
                if c.profile.is_some() {
                    return bad(
                        rust_i18n::t!("errors.instance.config_reason.minecraft_profile")
                            .to_string(),
                    );
                }
            }
            _ => {
                return bad(rust_i18n::t!("errors.instance.config_reason.no_minecraft").to_string());
            }
        }
        let rest = &self.components[1..];
        if rest.iter().any(|c| c.uid == ComponentUid::Minecraft) {
            return bad(
                rust_i18n::t!("errors.instance.config_reason.duplicate_minecraft").to_string(),
            );
        }
        if rest
            .iter()
            .filter(|c| c.uid.loader_kind().is_some())
            .count()
            > 1
        {
            return bad(
                rust_i18n::t!("errors.instance.config_reason.multiple_loaders").to_string(),
            );
        }
        Ok(())
    }

    pub fn mc_version(&self) -> &str {
        self.components
            .first()
            .map(|c| c.version.as_str())
            .unwrap_or("")
    }

    pub fn loader(&self) -> Option<(LoaderKind, &Component)> {
        self.components
            .iter()
            .find_map(|c| c.uid.loader_kind().map(|k| (k, c)))
    }

    pub fn set_mc_version(&mut self, v: &str) {
        match self.components.first_mut() {
            Some(c) if c.uid == ComponentUid::Minecraft => c.version = v.to_string(),
            _ => self.components.insert(0, Component::minecraft(v)),
        }
    }

    /// Replaces the loader component. Rejects a non-loader uid.
    pub fn set_loader(&mut self, comp: Option<Component>) -> Result<(), InstanceError> {
        if let Some(c) = &comp
            && c.uid.loader_kind().is_none()
        {
            return Err(InstanceError::InvalidConfig(
                rust_i18n::t!("errors.instance.config_reason.not_a_loader").to_string(),
            ));
        }
        self.components.retain(|e| e.uid.loader_kind().is_none());
        self.components.extend(comp);
        Ok(())
    }
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

/// Longest directory name `create` derives from a display name, before any
/// ` (n)` dedup suffix. Well under the usual 255-byte file name limit.
const MAX_DIR_NAME_BYTES: usize = 200;

/// Safe directory name for a display name, at most [`MAX_DIR_NAME_BYTES`] bytes.
pub fn sanitize_dir_name(name: &str) -> String {
    clean_dir_name(name, MAX_DIR_NAME_BYTES)
}

fn clean_dir_name(name: &str, max_bytes: usize) -> String {
    let replaced: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let is_trim = |c: char| c == ' ' || c == '.';
    let mut trimmed = replaced.trim_matches(is_trim);
    if trimmed.len() > max_bytes {
        let mut end = max_bytes;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        trimmed = trimmed[..end].trim_end_matches(is_trim);
    }
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
        // No length cap here: ids may carry a dedup suffix past the cap.
        if id.is_empty() || id != clean_dir_name(id, usize::MAX) {
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
            config: InstanceConfig::from_raw(toml::from_str(&text)?)?,
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
                components: vec![Component::minecraft(mc_version)],
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
        inst.config.validate()?;
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

    fn write_raw(tmp: &std::path::Path, id: &str, body: &str) {
        std::fs::create_dir_all(tmp.join(id)).unwrap();
        std::fs::write(tmp.join(id).join("instance.toml"), body).unwrap();
    }

    #[test]
    fn legacy_instance_config_migrates() {
        let tmp = tempfile::tempdir().unwrap();
        write_raw(
            tmp.path(),
            "old",
            "name = \"Old\"\nmc_version = \"1.20.1\"\ncreated_at = 5\n",
        );
        let store = InstanceStore::new(tmp.path());
        let inst = store.load("old").unwrap();
        assert_eq!(inst.config.mc_version(), "1.20.1");
        assert!(inst.config.loader().is_none());
        store.save(&inst).unwrap();
        let text = std::fs::read_to_string(tmp.path().join("old/instance.toml")).unwrap();
        assert!(text.contains("[[components]]"), "{text}");
        assert!(!text.contains("mc_version"), "{text}");
        assert_eq!(store.load("old").unwrap(), inst);
    }

    #[test]
    fn loader_component_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let mut inst = store.create("L", "1.21.1").unwrap();
        inst.config.java_override = Some(JavaSettings::default());
        let fabric = Component {
            uid: ComponentUid::Fabric,
            version: "0.16.10".into(),
            profile: Some("fabric-loader-0.16.10-1.21.1".into()),
        };
        inst.config.set_loader(Some(fabric.clone())).unwrap();
        store.save(&inst).unwrap();
        let re = store.load("L").unwrap();
        assert_eq!(re, inst);
        assert_eq!(re.config.mc_version(), "1.21.1");
        let (kind, c) = re.config.loader().unwrap();
        assert_eq!(kind, LoaderKind::Fabric);
        assert_eq!(c, &fabric);
        let mut re = re;
        re.config.set_loader(None).unwrap();
        assert!(re.config.loader().is_none());
        re.config.set_mc_version("1.21.4");
        assert_eq!(re.config.mc_version(), "1.21.4");
    }

    #[test]
    fn missing_or_empty_mc_version_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        write_raw(tmp.path(), "a", "name = \"A\"\n");
        write_raw(tmp.path(), "b", "name = \"B\"\nmc_version = \"\"\n");
        for id in ["a", "b"] {
            assert!(matches!(
                store.load(id),
                Err(InstanceError::InvalidConfig(_))
            ));
        }
    }

    #[test]
    fn components_win_over_legacy_field() {
        let tmp = tempfile::tempdir().unwrap();
        write_raw(
            tmp.path(),
            "m",
            "mc_version = \"1.8\"\n[[components]]\nuid = \"minecraft\"\nversion = \"1.21\"\n",
        );
        let inst = InstanceStore::new(tmp.path()).load("m").unwrap();
        assert_eq!(inst.config.mc_version(), "1.21");
    }

    #[test]
    fn set_loader_keeps_minecraft_first_and_rejects_non_loaders() {
        let mut cfg = InstanceStore::new(tempfile::tempdir().unwrap().path())
            .create("x", "1.20")
            .unwrap()
            .config;
        let forge = Component {
            uid: ComponentUid::Forge,
            version: "47.1".into(),
            profile: Some("p".into()),
        };
        cfg.set_loader(Some(forge)).unwrap();
        cfg.set_mc_version("1.20.1");
        assert_eq!(cfg.components[0].uid, ComponentUid::Minecraft);
        assert_eq!(cfg.mc_version(), "1.20.1");
        assert!(cfg.loader().is_some());
        let mc = Component {
            uid: ComponentUid::Minecraft,
            version: "1".into(),
            profile: None,
        };
        assert!(matches!(
            cfg.set_loader(Some(mc)),
            Err(InstanceError::InvalidConfig(_))
        ));
        assert!(cfg.loader().is_some());
    }

    #[test]
    fn save_refuses_invalid_config_and_keeps_old_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let mut inst = store.create("S", "1.20").unwrap();
        let path = inst.dir.join("instance.toml");
        let before = std::fs::read_to_string(&path).unwrap();
        inst.config.components.clear();
        assert!(matches!(
            store.save(&inst),
            Err(InstanceError::InvalidConfig(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn invalid_components_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let mc = "[[components]]\nuid = \"minecraft\"\nversion = \"1.20.1\"\n";
        let loader = |u: &str| format!("[[components]]\nuid = \"{u}\"\nversion = \"1\"\n");
        let cases = [
            format!("{mc}{}{}", loader("fabric"), loader("forge")),
            format!("{}{mc}", loader("fabric")),
            format!("{mc}{mc}"),
            format!("{mc}profile = \"x\"\n"),
        ];
        for (i, body) in cases.iter().enumerate() {
            let id = format!("bad{i}");
            write_raw(tmp.path(), &id, body);
            assert!(
                matches!(store.load(&id), Err(InstanceError::InvalidConfig(_))),
                "case {i}"
            );
        }
    }

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
    fn long_names_are_capped_on_a_char_boundary() {
        let name = "长".repeat(120);
        let id = sanitize_dir_name(&name);
        assert!(id.len() <= 200, "{} bytes", id.len());
        assert!(!id.is_empty() && name.starts_with(&id));
        assert_eq!(sanitize_dir_name(&id), id);
        // A cut that lands on a trailing space or dot is trimmed again.
        let spaced = format!("{} x", "a".repeat(199));
        assert_eq!(sanitize_dir_name(&spaced), "a".repeat(199));

        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let a = store.create(&name, "1.21.4").unwrap();
        let b = store.create(&name, "1.21.4").unwrap();
        assert_eq!(a.id, id);
        assert_eq!(b.id, format!("{id} (2)"));
        assert_eq!(store.load(&b.id).unwrap().config.name, name);
        assert_eq!(store.list().unwrap().len(), 2);
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
