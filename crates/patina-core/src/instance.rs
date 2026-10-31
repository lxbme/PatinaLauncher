//! Instance storage: one directory per instance with `instance.toml`.

use crate::config::JavaSettings;
use crate::fsutil::{is_safe_name, write_atomic};
use crate::loader::LoaderKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, thiserror::Error)]
pub enum InstanceError {
    #[error("{}", rust_i18n::t!("errors.instance.io", error = .0))]
    Io(String),
    #[error("{}", rust_i18n::t!("errors.instance.parse", error = .0))]
    Parse(String),
    #[error("{}", rust_i18n::t!("errors.instance.serialize", error = .0))]
    Serialize(String),
    #[error("{}", rust_i18n::t!("errors.instance.not_found", id = .0))]
    NotFound(String),
    #[error("{}", rust_i18n::t!("errors.instance.invalid_id", id = .0))]
    InvalidId(String),
    #[error("{}", rust_i18n::t!("errors.instance.invalid_config", reason = .0))]
    InvalidConfig(String),
    #[error("{}", rust_i18n::t!("errors.instance.invalid_icon", reason = .0))]
    InvalidIcon(String),
    #[error("{}", rust_i18n::t!("errors.instance.not_broken", id = .0))]
    NotBroken(String),
    #[error(transparent)]
    Busy(#[from] crate::locks::Busy),
}

impl From<std::io::Error> for InstanceError {
    fn from(e: std::io::Error) -> Self {
        InstanceError::Io(e.to_string())
    }
}

impl From<toml::de::Error> for InstanceError {
    fn from(e: toml::de::Error) -> Self {
        InstanceError::Parse(e.to_string())
    }
}

impl From<toml::ser::Error> for InstanceError {
    fn from(e: toml::ser::Error) -> Self {
        InstanceError::Serialize(e.to_string())
    }
}

/// Instance icon. Serialized as `"builtin:<name>"` or `"custom"` (the image
/// is `icon.png` in the instance directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceIcon {
    Builtin(String),
    Custom,
}

impl Serialize for InstanceIcon {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            InstanceIcon::Builtin(n) => s.serialize_str(&format!("builtin:{n}")),
            InstanceIcon::Custom => s.serialize_str("custom"),
        }
    }
}

impl InstanceIcon {
    /// Parses the on-disk form; `None` for anything unknown or unsafe.
    fn parse(text: &str) -> Option<Self> {
        if text == "custom" {
            return Some(InstanceIcon::Custom);
        }
        let name = text.strip_prefix("builtin:")?;
        is_safe_name(name).then(|| InstanceIcon::Builtin(name.to_string()))
    }
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

/// Game window size in pixels, passed as `--width` / `--height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

impl WindowSize {
    pub const MIN_WIDTH: u32 = 320;
    pub const MIN_HEIGHT: u32 = 240;
    pub const MAX: u32 = 16384;

    fn is_valid(&self) -> bool {
        (Self::MIN_WIDTH..=Self::MAX).contains(&self.width)
            && (Self::MIN_HEIGHT..=Self::MAX).contains(&self.height)
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
    /// Cosmetic: an unparseable value is dropped on load, not an error.
    icon: Option<String>,
    window: Option<WindowSize>,
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
    pub icon: Option<InstanceIcon>,
    /// `None` leaves the window size to the game.
    pub window: Option<WindowSize>,
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
            icon: raw.icon.and_then(|text| {
                let icon = InstanceIcon::parse(&text);
                if icon.is_none() {
                    tracing::warn!("ignoring invalid instance icon {text:?}");
                }
                icon
            }),
            window: raw.window,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), InstanceError> {
        let bad = |reason: String| Err(InstanceError::InvalidConfig(reason));
        if let Some(InstanceIcon::Builtin(n)) = &self.icon
            && !is_safe_name(n)
        {
            return bad(rust_i18n::t!("errors.instance.config_reason.bad_icon_name").to_string());
        }
        if let Some(w) = &self.window
            && !w.is_valid()
        {
            return bad(rust_i18n::t!(
                "errors.instance.config_reason.bad_window_size",
                min_width = WindowSize::MIN_WIDTH,
                min_height = WindowSize::MIN_HEIGHT,
                max = WindowSize::MAX
            )
            .to_string());
        }
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
    /// The custom icon file, only when the icon is custom and the file exists.
    pub fn icon_path(&self) -> Option<PathBuf> {
        let p = self.dir.join(ICON_FILE);
        (self.config.icon == Some(InstanceIcon::Custom) && p.is_file()).then_some(p)
    }
}

const CONFIG_FILE: &str = "instance.toml";
/// Held (`flock`) across a config's read, change and write by
/// [`InstanceStore::update`], so concurrent changes of different fields (in
/// this process or another) do not overwrite each other. Never deleted.
pub const CONFIG_LOCK_FILE: &str = ".patina-config.lock";
const ICON_FILE: &str = "icon.png";
const ICON_SIZE: u32 = 128;
const MAX_ICON_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ICON_SOURCE_SIDE: u32 = 8192;
const MAX_ICON_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// An instance directory whose entry could not be loaded.
#[derive(Debug, Clone)]
pub struct BrokenInstance {
    pub id: String,
    pub dir: PathBuf,
    pub error: InstanceError,
}

#[derive(Debug, Default)]
pub struct ListOutcome {
    pub instances: Vec<Instance>,
    pub broken: Vec<BrokenInstance>,
}

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

    /// Takes the lock for instance `id` of this store, which other Patina
    /// processes see too (see [`crate::locks`]).
    pub fn lock(&self, id: &str) -> Result<crate::locks::InstanceGuard, crate::locks::Busy> {
        crate::locks::Locks::global().instance(&self.root, id)
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

    /// Instances sorted by `last_played` descending (never played last), then
    /// name; `broken` sorted by id. Only a failure to read the root itself is
    /// an error; a bad directory name or config lands in `broken`.
    pub fn list(&self) -> Result<ListOutcome, InstanceError> {
        let rd = match std::fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ListOutcome::default());
            }
            Err(e) => return Err(e.into()),
        };
        let mut out = ListOutcome::default();
        for entry in rd {
            let entry = entry?;
            let dir = entry.path();
            if !dir.join(CONFIG_FILE).is_file() {
                continue;
            }
            let name = entry.file_name();
            let id = name.to_string_lossy().into_owned();
            let result = match name.to_str() {
                Some(_) => self.load(&id),
                None => Err(InstanceError::InvalidId(id.clone())),
            };
            match result {
                Ok(inst) => out.instances.push(inst),
                Err(error) => out.broken.push(BrokenInstance { id, dir, error }),
            }
        }
        out.instances.sort_by(|a, b| {
            b.config
                .last_played
                .cmp(&a.config.last_played)
                .then_with(|| a.config.name.cmp(&b.config.name))
                .then_with(|| a.id.cmp(&b.id))
        });
        out.broken.sort_by(|a, b| a.id.cmp(&b.id));
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
                icon: None,
                window: None,
            },
        };
        std::fs::create_dir_all(inst.game_dir())?;
        self.save(&inst)?;
        Ok(inst)
    }

    /// Healthy instances whose display name or id (both are matched) contains
    /// `query`, case-insensitively, sorted by name. A blank query matches
    /// nothing.
    pub fn search_instances<'a>(outcome: &'a ListOutcome, query: &str) -> Vec<&'a Instance> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<&Instance> = outcome
            .instances
            .iter()
            .filter(|i| {
                i.config.name.to_lowercase().contains(&q) || i.id.to_lowercase().contains(&q)
            })
            .collect();
        hits.sort_by_cached_key(|i| (i.config.name.to_lowercase(), i.id.clone()));
        hits
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

    /// Reads instance `id`'s config as it is on disk now, changes it with
    /// `f` and saves it, holding the config lock ([`CONFIG_LOCK_FILE`]) from
    /// the read to the write: another update of the same instance, in this
    /// process or another, waits, so neither loses the other's change. Use
    /// it (not [`InstanceStore::load`] then [`InstanceStore::save`]) for
    /// every change of an existing instance's config. Blocking; `f` runs
    /// under the lock and must be quick.
    ///
    /// This is not the instance lock ([`InstanceStore::lock`]): it is held
    /// only for the write, also while a game or an operation holds the
    /// instance.
    pub fn update(
        &self,
        id: &str,
        f: impl FnOnce(&mut InstanceConfig),
    ) -> Result<Instance, InstanceError> {
        Self::check_id(id)?;
        let dir = self.root.join(id);
        // The lock file is never created for an instance that is not there.
        if !dir.join(CONFIG_FILE).is_file() {
            return Err(InstanceError::NotFound(id.to_string()));
        }
        let _lock = lock_config(&dir)?;
        let mut inst = self.read(id)?;
        f(&mut inst.config);
        self.save(&inst)?;
        Ok(inst)
    }

    /// Changes the display name only; the directory id stays.
    pub fn rename(&self, id: &str, new_name: &str) -> Result<Instance, InstanceError> {
        self.update(id, |config| config.name = new_name.to_string())
    }

    /// Records that the instance was just played, keeping changes made since
    /// the caller loaded it. Takes no instance lock: it is called while the
    /// game session holds the instance.
    pub fn touch_played(&self, id: &str) -> Result<(), InstanceError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.update(id, |config| config.last_played = Some(now))
            .map(drop)
    }

    pub fn set_builtin_icon(&self, id: &str, name: &str) -> Result<Instance, InstanceError> {
        let icon = InstanceIcon::Builtin(name.to_string());
        let inst = self.update(id, |config| config.icon = Some(icon))?;
        remove_icon_file(&inst.dir)?;
        Ok(inst)
    }

    /// Crops `src` (png, jpeg or webp, at most 8 MiB) to a centered square,
    /// scales it to 128x128 and stores it as the instance's `icon.png`.
    pub fn set_custom_icon(&self, id: &str, src: &Path) -> Result<Instance, InstanceError> {
        let dir = self.load(id)?.dir;
        let png = render_icon(src)?;
        write_atomic(&dir.join(ICON_FILE), &png)?;
        self.update(id, |config| config.icon = Some(InstanceIcon::Custom))
    }

    pub fn clear_icon(&self, id: &str) -> Result<Instance, InstanceError> {
        let inst = self.update(id, |config| config.icon = None)?;
        remove_icon_file(&inst.dir)?;
        Ok(inst)
    }

    /// Removes a directory reported in [`ListOutcome::broken`]. The entry is
    /// only partly trusted: anything that is not a real (non-symlink) direct
    /// child of the instances root is refused, and so is a directory that
    /// loads as a healthy instance again (repaired since it was listed).
    pub fn delete_broken(&self, b: &BrokenInstance) -> Result<(), InstanceError> {
        let refuse = || InstanceError::InvalidId(b.id.clone());
        let _lock = self.lock(&b.id)?;
        if !std::fs::symlink_metadata(&b.dir)?.is_dir() {
            return Err(refuse());
        }
        let name = match b.dir.file_name() {
            Some(n) if n != ".." && n != "." => n,
            _ => return Err(refuse()),
        };
        if let Some(name) = name.to_str().filter(|n| is_safe_name(n))
            && self.read(name).is_ok()
        {
            return Err(InstanceError::NotBroken(b.id.clone()));
        }
        let parent = b.dir.parent().ok_or_else(refuse)?;
        if std::fs::canonicalize(parent)? != std::fs::canonicalize(&self.root)? {
            return Err(refuse());
        }
        std::fs::remove_dir_all(&b.dir)?;
        Ok(())
    }

    /// Deletes instance `id` and its directory, once a config update in
    /// flight ([`InstanceStore::update`]) is written. Refuses while another
    /// operation holds the instance, and while its game runs
    /// ([`BusyKind::Running`](crate::locks::BusyKind::Running)).
    pub fn delete(&self, id: &str) -> Result<(), InstanceError> {
        Self::check_id(id)?;
        let _lock = self.lock(id)?;
        let dir = self.root.join(id);
        if !dir.join(CONFIG_FILE).is_file() {
            return Err(InstanceError::NotFound(id.to_string()));
        }
        // After the instance lock, as the loader operations take them: a
        // config update in flight finishes first, and one that comes later
        // finds no instance.
        let _config = lock_config(&dir)?;
        crate::launch::running::refuse_while_running(&dir, id)?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}

/// Takes the config lock of instance directory `dir` (see
/// [`InstanceStore::update`]), waiting while someone else holds it: every
/// holder only reads, changes and writes one small file. Released when the
/// returned file is dropped.
fn lock_config(dir: &Path) -> Result<std::fs::File, InstanceError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(CONFIG_LOCK_FILE))?;
    file.lock()?;
    Ok(file)
}

fn remove_icon_file(dir: &Path) -> Result<(), InstanceError> {
    match std::fs::remove_file(dir.join(ICON_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn render_icon(src: &Path) -> Result<Vec<u8>, InstanceError> {
    use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, imageops};
    use std::io::{Cursor, Read};
    let bad = |key: &str| InstanceError::InvalidIcon(rust_i18n::t!(key).to_string());
    let decode_err = |_| bad("errors.instance.icon_reason.decode");
    let mut bytes = Vec::new();
    std::fs::File::open(src)?
        .take(MAX_ICON_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ICON_SOURCE_BYTES {
        return Err(bad("errors.instance.icon_reason.too_large"));
    }
    let format = match image::guess_format(&bytes) {
        Ok(f @ (ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP)) => f,
        _ => return Err(bad("errors.instance.icon_reason.format")),
    };
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_ICON_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_ICON_SOURCE_SIDE);
    limits.max_alloc = Some(MAX_ICON_DECODE_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(decode_err)?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(decode_err)?;
    img.apply_orientation(orientation);
    let side = img.width().min(img.height());
    let view = imageops::crop_imm(
        &img,
        (img.width() - side) / 2,
        (img.height() - side) / 2,
        side,
        side,
    );
    let icon = imageops::resize(&*view, ICON_SIZE, ICON_SIZE, imageops::FilterType::Lanczos3);
    let mut out = Cursor::new(Vec::new());
    icon.write_to(&mut out, ImageFormat::Png)
        .map_err(|_| bad("errors.instance.icon_reason.encode"))?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_errors_have_no_source() {
        use std::error::Error;
        let de = toml::from_str::<toml::Value>("x = [").unwrap_err();
        let errs: Vec<InstanceError> = vec![std::io::Error::other("boom").into(), de.into()];
        for e in errs {
            assert!(e.source().is_none(), "{e:?}");
        }
    }

    use crate::config::JavaSettings;

    #[test]
    fn concurrent_updates_of_different_fields_both_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let id = store.create("Pack", "1.21.1").unwrap().id;
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        let slow = {
            let (store, id) = (store.clone(), id.clone());
            std::thread::spawn(move || {
                store
                    .update(&id, |config| {
                        // Read; the other update starts meanwhile.
                        read_tx.send(()).unwrap();
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        config.name = "Renamed".into();
                    })
                    .unwrap();
            })
        };
        read_rx.recv().unwrap();
        store
            .update(&id, |config| config.last_played = Some(42))
            .unwrap();
        slow.join().unwrap();
        let config = store.load(&id).unwrap().config;
        assert_eq!(config.name, "Renamed");
        assert_eq!(config.last_played, Some(42), "neither update is lost");
        assert!(tmp.path().join(&id).join(CONFIG_LOCK_FILE).is_file());
    }

    #[test]
    fn delete_waits_for_a_config_update_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let id = store.create("Pack", "1.21.1").unwrap().id;
        let dir = tmp.path().join(&id);
        // A writer holds the config lock (between its read and its write).
        let held = lock_config(&dir).unwrap();
        let deleting = {
            let (store, id) = (store.clone(), id.clone());
            std::thread::spawn(move || store.delete(&id))
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            dir.join(CONFIG_FILE).is_file(),
            "not removed under the writer"
        );
        drop(held);
        deleting.join().unwrap().unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn update_of_a_missing_instance_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        assert!(matches!(
            store.update("gone", |c| c.last_played = Some(1)),
            Err(InstanceError::NotFound(_))
        ));
        assert!(!tmp.path().join("gone").exists());
    }

    fn write_raw(tmp: &std::path::Path, id: &str, body: &str) {
        std::fs::create_dir_all(tmp.join(id)).unwrap();
        std::fs::write(tmp.join(id).join("instance.toml"), body).unwrap();
    }

    #[test]
    fn window_size_round_trips_and_validates() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let mut inst = store.create("W", "1.20.1").unwrap();
        assert_eq!(store.load(&inst.id).unwrap().config.window, None);
        let ok = WindowSize {
            width: 1280,
            height: 720,
        };
        inst.config.window = Some(ok);
        store.save(&inst).unwrap();
        assert_eq!(store.load(&inst.id).unwrap().config.window, Some(ok));
        for (width, height) in [(0, 0), (20000, 100), (319, 240), (320, 239), (320, 16385)] {
            inst.config.window = Some(WindowSize { width, height });
            assert!(
                matches!(store.save(&inst), Err(InstanceError::InvalidConfig(_))),
                "{width}x{height}"
            );
        }
        inst.config.window = Some(WindowSize {
            width: 16384,
            height: 240,
        });
        store.save(&inst).unwrap();
        write_raw(
            tmp.path(),
            "raw",
            "name = \"R\"\nmc_version = \"1.20.1\"\n[window]\nwidth = 0\nheight = 0\n",
        );
        assert!(matches!(
            store.load("raw"),
            Err(InstanceError::InvalidConfig(_))
        ));
    }

    #[test]
    fn search_instances_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        store.create("zeta Pack", "1.20.1").unwrap();
        store.create("Alpha pack", "1.20.1").unwrap();
        store.create("Other", "1.20.1").unwrap();
        let out = store.list().unwrap();
        let names = |q: &str| -> Vec<String> {
            InstanceStore::search_instances(&out, q)
                .into_iter()
                .map(|i| i.config.name.clone())
                .collect()
        };
        assert_eq!(names(" PACK "), ["Alpha pack", "zeta Pack"]);
        assert_eq!(names("oth"), ["Other"]);
        assert!(names("  ").is_empty());
        assert!(names("nomatch").is_empty());
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
    fn touch_played_keeps_changes_made_since_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let stale = store.create("Old", "1.20.1").unwrap();
        store.rename(&stale.id, "New").unwrap();
        let _held = store.lock(&stale.id).unwrap(); // as a running game holds it
        store.touch_played(&stale.id).unwrap();
        let now = store.load(&stale.id).unwrap();
        assert_eq!(now.config.name, "New");
        assert!(now.config.last_played.is_some_and(|t| t > 0));
        assert!(matches!(
            store.touch_played("missing"),
            Err(InstanceError::NotFound(_))
        ));
        assert!(matches!(
            store.touch_played("../x"),
            Err(InstanceError::InvalidId(_))
        ));
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
        assert_eq!(store.list().unwrap().instances.len(), 2);
    }

    #[test]
    fn sanitize_edge_cases() {
        assert_eq!(sanitize_dir_name("  ..  "), "instance");
        assert_eq!(sanitize_dir_name("a:b*c?\"<>|\\"), "a_b_c______");
        assert_eq!(sanitize_dir_name("CON"), "_CON");
        assert_eq!(sanitize_dir_name("tab\there"), "tab_here");
    }

    #[test]
    fn delete_refuses_while_instance_is_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let a = store.create("Busy", "1.8.9").unwrap();
        let guard = store.lock(&a.id).unwrap();
        assert!(matches!(store.delete(&a.id), Err(InstanceError::Busy(_))));
        let broken = BrokenInstance {
            id: a.id.clone(),
            dir: a.dir.clone(),
            error: InstanceError::NotFound(a.id.clone()),
        };
        assert!(matches!(
            store.delete_broken(&broken),
            Err(InstanceError::Busy(_))
        ));
        assert!(a.dir.is_dir());
        drop(guard);
        store.delete(&a.id).unwrap();
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

    #[cfg(target_os = "linux")]
    #[test]
    fn delete_refuses_while_the_game_runs() {
        use crate::launch::running::FakeGame;
        use crate::locks::BusyKind;
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path().to_path_buf());
        let a = store.create("Running", "1.21.1").unwrap();
        let game = FakeGame::start(&a.dir, &a.id);
        match store.delete(&a.id) {
            Err(InstanceError::Busy(b)) => assert_eq!(b.what, BusyKind::Running(a.id.clone())),
            other => panic!("expected Busy(Running), got {other:?}"),
        }
        assert!(a.dir.is_dir(), "nothing deleted");
        drop(game);
        // The game is gone: its stale record no longer holds the instance.
        store.delete(&a.id).unwrap();
        assert!(!a.dir.exists());
    }

    #[test]
    fn delete_removes_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let a = store.create("X", "1.20").unwrap();
        store.delete(&a.id).unwrap();
        assert!(!a.dir.exists());
        assert!(store.list().unwrap().instances.is_empty());
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
        let ids: Vec<_> = store
            .list()
            .unwrap()
            .instances
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ids, ["C", "B", "A"]);
    }

    #[test]
    fn list_ignores_dirs_without_config_and_name_tiebreak() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        std::fs::create_dir(tmp.path().join("junk")).unwrap();
        store.create("b", "1").unwrap();
        store.create("a", "1").unwrap();
        let ids: Vec<_> = store
            .list()
            .unwrap()
            .instances
            .into_iter()
            .map(|i| i.id)
            .collect();
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
    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, _| image::Rgba([(x % 256) as u8, 9, 9, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn list_reports_broken_instances_without_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        store.create("good", "1.20").unwrap();
        write_raw(tmp.path(), "torn", "name = [");
        write_raw(tmp.path(), "bad:name", "name = \"x\"\nmc_version = \"1\"\n");
        let out = store.list().unwrap();
        assert_eq!(out.instances.len(), 1);
        assert_eq!(out.broken.len(), 2);
        let torn = out.broken.iter().find(|b| b.id == "torn").unwrap();
        assert!(matches!(torn.error, InstanceError::Parse(_)));
        assert_eq!(torn.dir, tmp.path().join("torn"));
        let bad = out.broken.iter().find(|b| b.id == "bad:name").unwrap();
        assert!(matches!(bad.error, InstanceError::InvalidId(_)));
    }

    #[test]
    fn custom_icon_is_cropped_and_resized() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path().join("inst"));
        let a = store.create("I", "1.20").unwrap();
        assert!(a.icon_path().is_none());
        let src = tmp.path().join("src.png");
        std::fs::write(&src, png_bytes(300, 200)).unwrap();
        let inst = store.set_custom_icon("I", &src).unwrap();
        let p = inst.icon_path().unwrap();
        let img = image::open(&p).unwrap().to_rgba8();
        assert_eq!((img.width(), img.height()), (128, 128));
        // The source red channel is x: a centered 200px crop spans x = 50..250.
        let left = img.get_pixel(0, 64)[0];
        let right = img.get_pixel(127, 64)[0];
        assert!((48..=60).contains(&left), "left {left}");
        assert!((238..=252).contains(&right), "right {right}");
        assert_eq!(
            store.load("I").unwrap().config.icon,
            Some(InstanceIcon::Custom)
        );
        let cleared = store.clear_icon("I").unwrap();
        assert!(cleared.config.icon.is_none() && !p.exists());
        assert!(cleared.icon_path().is_none());
    }

    #[test]
    fn icon_rejects_oversized_or_unknown_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path().join("inst"));
        store.create("I", "1.20").unwrap();
        let write = |name: &str, data: &[u8]| {
            let p = tmp.path().join(name);
            std::fs::write(&p, data).unwrap();
            p
        };
        let mut big = png_bytes(8, 8);
        big.resize(8 * 1024 * 1024 + 1, 0);
        let cases = [
            (write("big.png", &big), "too_large"),
            (
                write("a.gif", b"GIF89a\x01\x00\x01\x00\x00\x00\x00;"),
                "format",
            ),
            (write("junk.png", b"not an image"), "format"),
            (write("wide.png", &png_bytes(20000, 1)), "decode"),
        ];
        for (p, key) in cases {
            let want = match key {
                "too_large" => rust_i18n::t!("errors.instance.icon_reason.too_large"),
                "format" => rust_i18n::t!("errors.instance.icon_reason.format"),
                _ => rust_i18n::t!("errors.instance.icon_reason.decode"),
            }
            .to_string();
            match store.set_custom_icon("I", &p) {
                Err(InstanceError::InvalidIcon(got)) => assert_eq!(got, want, "{p:?}"),
                other => panic!("{p:?}: {other:?}"),
            }
        }
        // Truncated PNG: right format, undecodable.
        let trunc = write("trunc.png", &png_bytes(64, 64)[..40]);
        match store.set_custom_icon("I", &trunc) {
            Err(InstanceError::InvalidIcon(got)) => assert_eq!(
                got,
                rust_i18n::t!("errors.instance.icon_reason.decode").to_string()
            ),
            other => panic!("{other:?}"),
        }
        assert!(
            store
                .set_custom_icon("I", &tmp.path().join("missing.png"))
                .is_err()
        );
        assert!(store.load("I").unwrap().config.icon.is_none());
    }

    #[test]
    fn builtin_icon_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        store.create("B", "1.20").unwrap();
        let inst = store.set_builtin_icon("B", "grass").unwrap();
        assert_eq!(
            inst.config.icon,
            Some(InstanceIcon::Builtin("grass".into()))
        );
        let text = std::fs::read_to_string(inst.dir.join("instance.toml")).unwrap();
        assert!(text.contains("icon = \"builtin:grass\""), "{text}");
        assert_eq!(store.load("B").unwrap(), inst);
        assert!(inst.icon_path().is_none());
        for bad in ["", "../x", "a/b", "."] {
            assert!(
                matches!(
                    store.set_builtin_icon("B", bad),
                    Err(InstanceError::InvalidConfig(_))
                ),
                "{bad:?}"
            );
        }
        assert_eq!(store.load("B").unwrap(), inst);
    }

    #[test]
    fn invalid_icon_on_disk_loads_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        for (i, icon) in ["weird", "builtin:", "builtin:../x", ""].iter().enumerate() {
            write_raw(
                tmp.path(),
                &format!("i{i}"),
                &format!("mc_version = \"1\"\nicon = \"{icon}\"\n"),
            );
        }
        let out = store.list().unwrap();
        assert!(out.broken.is_empty(), "{:?}", out.broken);
        assert_eq!(out.instances.len(), 4);
        assert!(out.instances.iter().all(|i| i.config.icon.is_none()));
    }

    #[test]
    fn broken_instances_can_be_deleted_safely() {
        use std::os::unix::ffi::OsStrExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("inst");
        let store = InstanceStore::new(&root);
        store.create("good", "1.20").unwrap();
        write_raw(&root, "torn", "name = [");
        let weird = root.join(std::ffi::OsStr::from_bytes(b"bad\xff"));
        std::fs::create_dir(&weird).unwrap();
        std::fs::write(weird.join("instance.toml"), "x").unwrap();
        let out = store.list().unwrap();
        assert_eq!(out.broken.len(), 2);
        for b in &out.broken {
            store.delete_broken(b).unwrap();
            assert!(!b.dir.exists());
        }
        assert_eq!(store.list().unwrap().instances.len(), 1);
        assert!(store.list().unwrap().broken.is_empty());

        // Refuses a path outside the root, a nested path and a symlink.
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let fake = |dir: PathBuf| BrokenInstance {
            id: "x".into(),
            dir,
            error: InstanceError::NotFound("x".into()),
        };
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        for dir in [
            outside.clone(),
            root.join("good/minecraft"),
            root.join("link"),
            root.join("../outside"),
        ] {
            assert!(
                matches!(
                    store.delete_broken(&fake(dir.clone())),
                    Err(InstanceError::InvalidId(_))
                ),
                "{dir:?}"
            );
        }
        assert!(outside.exists() && root.join("good/minecraft").exists());
    }

    /// A stale broken entry (repaired since it was listed, or forged) never
    /// deletes a healthy instance.
    #[test]
    fn delete_broken_refuses_a_healthy_instance() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        write_raw(tmp.path(), "torn", "name = [");
        let out = store.list().unwrap();
        let b = out.broken.into_iter().next().unwrap();
        // Repaired before the user confirmed the deletion.
        let good = store.create("good", "1.20").unwrap();
        std::fs::copy(good.dir.join(CONFIG_FILE), b.dir.join(CONFIG_FILE)).unwrap();
        assert!(matches!(
            store.delete_broken(&b),
            Err(InstanceError::NotBroken(id)) if id == "torn"
        ));
        assert!(b.dir.join(CONFIG_FILE).is_file());
        let forged = BrokenInstance {
            id: good.id.clone(),
            dir: good.dir.clone(),
            error: InstanceError::NotFound(good.id.clone()),
        };
        assert!(matches!(
            store.delete_broken(&forged),
            Err(InstanceError::NotBroken(_))
        ));
        assert!(good.dir.is_dir());
    }
}
