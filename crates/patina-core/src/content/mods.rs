//! Mod jar metadata (Fabric, Quilt, NeoForge, Forge, legacy `mcmod.info`)
//! and enabling, disabling and deleting mods.
//!
//! Jars are untrusted: no declared size is believed, every entry is read
//! through a hard cap, and no input may panic.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde_json::Value;
use zip::ZipArchive;

use crate::fsutil::is_safe_rel;

/// Largest accepted icon, in bytes.
pub const ICON_MAX_BYTES: usize = 1024 * 1024;
/// Largest metadata entry (after decompression), in bytes.
pub const ENTRY_MAX_BYTES: usize = 4 * 1024 * 1024;
/// A jar with more entries than this is rejected outright.
pub const MAX_ZIP_ENTRIES: usize = 65536;
/// At most this many icon paths are tried for a size-to-path icon map.
const MAX_ICON_CANDIDATES: usize = 4;

const DISABLED_SUFFIX: &str = ".disabled";
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModFormat {
    Fabric,
    Quilt,
    Forge,
    NeoForge,
    LegacyForge,
    Unknown,
}

/// Metadata files in default priority order.
const CANDIDATES: [(ModFormat, &str); 5] = [
    (ModFormat::Fabric, "fabric.mod.json"),
    (ModFormat::Quilt, "quilt.mod.json"),
    (ModFormat::NeoForge, "META-INF/neoforge.mods.toml"),
    (ModFormat::Forge, "META-INF/mods.toml"),
    (ModFormat::LegacyForge, "mcmod.info"),
];

/// Whether `format` should be tried before the rest when the instance runs
/// `prefer` (a loader accepts its own format and its compatible ones).
fn preferred_by(prefer: ModFormat, format: ModFormat) -> bool {
    use ModFormat::*;
    matches!(
        (prefer, format),
        (Fabric | Quilt, Fabric | Quilt)
            | (NeoForge, NeoForge | Forge)
            | (Forge, Forge | LegacyForge)
            | (LegacyForge, LegacyForge)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModMeta {
    pub id: Option<String>,
    pub name: String,
    pub version: Option<String>,
    pub authors: Vec<String>,
    pub description: Option<String>,
    pub format: ModFormat,
    /// Raw PNG bytes, at most [`ICON_MAX_BYTES`].
    pub icon: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModEntry {
    pub file_name: String,
    pub path: PathBuf,
    pub enabled: bool,
    pub size: u64,
    /// Modification time in unix seconds (0 when unknown).
    pub modified: u64,
    pub meta: Option<ModMeta>,
}

/// Splits a mod file name into its stem and whether it is enabled:
/// `x.jar` -> `("x", true)`, `x.jar.disabled` -> `("x", false)`, matched
/// case-insensitively. Anything else is not a mod file.
pub fn split_mod_name(name: &str) -> Option<(&str, bool)> {
    let lower = name.to_ascii_lowercase();
    let (cut, enabled) = if lower.ends_with(".jar") {
        (4, true)
    } else if lower.ends_with(".jar.disabled") {
        (4 + DISABLED_SUFFIX.len(), false)
    } else {
        return None;
    };
    let stem = name.get(..name.len() - cut)?;
    (!stem.is_empty()).then_some((stem, enabled))
}

/// Lists `*.jar` and `*.jar.disabled` in `dir` (symlinks are followed),
/// sorted by file name (case-insensitive, ties by exact name). A missing
/// directory yields an empty list; a jar whose metadata cannot be read gets
/// `meta: None`. `prefer` is the instance's loader, see [`read_mod_meta`].
///
/// This is blocking (it opens every jar): the GUI must call it through
/// `tokio::task::spawn_blocking`.
pub fn scan_mods(dir: &Path, prefer: Option<ModFormat>) -> io::Result<Vec<ModEntry>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some((_, enabled)) = split_mod_name(&file_name) else {
            continue;
        };
        let path = entry.path();
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        if !md.is_file() {
            continue;
        }
        let modified = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        let meta = read_mod_meta(&path, prefer);
        out.push(ModEntry {
            file_name,
            path,
            enabled,
            size: md.len(),
            modified,
            meta,
        });
    }
    out.sort_by_cached_key(|e| (e.file_name.to_lowercase(), e.file_name.clone()));
    Ok(out)
}

/// Reads the metadata of one jar. Candidate metadata files are tried in the
/// order Fabric, Quilt, NeoForge, Forge, legacy; `prefer` (the instance's
/// loader) moves its own and compatible formats (Fabric/Quilt, NeoForge ->
/// Forge, Forge -> legacy) first. The first candidate that is present and
/// parses wins. No candidate file at all yields [`ModFormat::Unknown`] named
/// after the file; candidates that are all unparsable (or an unreadable jar)
/// yield `None`.
pub fn read_mod_meta(jar: &Path, prefer: Option<ModFormat>) -> Option<ModMeta> {
    let stem = jar_stem(jar);
    let mut ar = ZipArchive::new(File::open(jar).ok()?).ok()?;
    if ar.len() > MAX_ZIP_ENTRIES {
        return None;
    }
    let mut present: Vec<(ModFormat, &str)> = CANDIDATES
        .iter()
        .copied()
        .filter(|(_, entry)| has(&mut ar, entry))
        .collect();
    if present.is_empty() {
        let mut m = blank(ModFormat::Unknown);
        m.name = stem;
        return Some(m);
    }
    if let Some(p) = prefer {
        present.sort_by_key(|(f, _)| !preferred_by(p, *f));
    }
    present
        .into_iter()
        .find_map(|(format, entry)| match format {
            ModFormat::Fabric => parse_fabric(&mut ar, entry, &stem),
            ModFormat::Quilt => parse_quilt(&mut ar, entry, &stem),
            ModFormat::NeoForge | ModFormat::Forge => {
                parse_mods_toml(&mut ar, entry, format, &stem)
            }
            ModFormat::LegacyForge => parse_mcmod_info(&mut ar, entry, &stem),
            ModFormat::Unknown => None,
        })
}

/// Adds or removes the `.disabled` suffix. Returns the (new) path; a mod
/// already in the requested state is left alone. Fails with `InvalidInput`
/// for a name that is not a mod file and `AlreadyExists` when the target
/// name is taken.
///
/// The existence check and the rename are not atomic: a file created in
/// between would be replaced. Only the launcher writes to a mods folder, so
/// this is accepted.
pub fn set_mod_enabled(path: &Path, enabled: bool) -> io::Result<PathBuf> {
    let name = path.file_name().and_then(|n| n.to_str());
    let Some((_, is_enabled)) = name.and_then(split_mod_name) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a mod file name",
        ));
    };
    if is_enabled == enabled {
        return Ok(path.to_path_buf());
    }
    let name = name.unwrap_or_default();
    let new_name = if enabled {
        name.get(..name.len() - DISABLED_SUFFIX.len())
            .unwrap_or(name)
            .to_owned()
    } else {
        format!("{name}{DISABLED_SUFFIX}")
    };
    let target = path.with_file_name(new_name);
    if target.symlink_metadata().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "target mod file already exists",
        ));
    }
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// Deletes one mod file. A directory (or a missing path) is an error and is
/// left untouched.
pub fn delete_mod(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)
}

// ---------------------------------------------------------------------------

type Jar = ZipArchive<File>;

fn jar_stem(jar: &Path) -> String {
    let name = jar.file_name().and_then(|n| n.to_str()).unwrap_or("");
    split_mod_name(name).map_or_else(
        || {
            jar.file_stem()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
        },
        |(stem, _)| stem.to_owned(),
    )
}

fn has(ar: &mut Jar, name: &str) -> bool {
    ar.by_name(name).is_ok()
}

/// Reads one entry fully, refusing more than `limit` decompressed bytes
/// whatever the zip header declares.
fn read_capped(ar: &mut Jar, name: &str, limit: usize) -> Option<Vec<u8>> {
    let f = ar.by_name(name).ok()?;
    if f.is_dir() {
        return None;
    }
    let mut buf = Vec::new();
    f.take(limit as u64 + 1).read_to_end(&mut buf).ok()?;
    (buf.len() <= limit).then_some(buf)
}

fn read_icon(ar: &mut Jar, path: &str) -> Option<Vec<u8>> {
    let rel = path.trim().trim_start_matches('/');
    if !is_safe_rel(rel) {
        return None;
    }
    let bytes = read_capped(ar, rel, ICON_MAX_BYTES)?;
    bytes.starts_with(&PNG_SIGNATURE).then_some(bytes)
}

/// Paths of a size-to-path icon map, largest size first, without duplicates,
/// at most [`MAX_ICON_CANDIDATES`] (a hostile map may have thousands).
fn icon_candidates(map: &serde_json::Map<String, Value>) -> Vec<&str> {
    let mut sized: Vec<(u64, &str)> = map
        .iter()
        .filter_map(|(k, p)| Some((k.trim().parse().unwrap_or(0), p.as_str()?)))
        .collect();
    sized.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
    let mut out: Vec<&str> = Vec::new();
    for (_, p) in sized {
        if out.len() == MAX_ICON_CANDIDATES {
            break;
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// An icon given as one path or as a size-to-path map.
fn read_icon_value(ar: &mut Jar, v: Option<&Value>) -> Option<Vec<u8>> {
    match v? {
        Value::String(p) => read_icon(ar, p),
        Value::Object(map) => icon_candidates(map)
            .into_iter()
            .find_map(|p| read_icon(ar, p)),
        _ => None,
    }
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Sets `id`, and `name` falling back to the id, then the file stem.
fn finish(mut meta: ModMeta, id: Option<String>, name: Option<String>, stem: &str) -> ModMeta {
    meta.name = name
        .or_else(|| id.clone())
        .unwrap_or_else(|| stem.to_owned());
    meta.id = id;
    meta
}

fn blank(format: ModFormat) -> ModMeta {
    ModMeta {
        id: None,
        name: String::new(),
        version: None,
        authors: Vec::new(),
        description: None,
        format,
        icon: None,
    }
}

fn read_json(ar: &mut Jar, name: &str) -> Option<Value> {
    let bytes = read_capped(ar, name, ENTRY_MAX_BYTES)?;
    let bytes = bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF][..])
        .unwrap_or(&bytes);
    serde_json::from_slice(bytes).ok()
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn parse_fabric(ar: &mut Jar, entry: &str, stem: &str) -> Option<ModMeta> {
    let v = read_json(ar, entry)?;
    let mut m = blank(ModFormat::Fabric);
    m.version = non_empty(str_of(&v, "version"));
    m.description = non_empty(str_of(&v, "description"));
    if let Some(list) = v.get("authors").and_then(Value::as_array) {
        m.authors = list
            .iter()
            .filter_map(|a| match a {
                Value::String(s) => non_empty(Some(s)),
                Value::Object(_) => non_empty(str_of(a, "name")),
                _ => None,
            })
            .collect();
    }
    m.icon = read_icon_value(ar, v.get("icon"));
    let id = non_empty(str_of(&v, "id"));
    let name = non_empty(str_of(&v, "name"));
    Some(finish(m, id, name, stem))
}

fn parse_quilt(ar: &mut Jar, entry: &str, stem: &str) -> Option<ModMeta> {
    let v = read_json(ar, entry)?;
    let ql = v.get("quilt_loader")?;
    let mut m = blank(ModFormat::Quilt);
    m.version = non_empty(str_of(ql, "version"));
    let md = ql.get("metadata");
    if let Some(md) = md {
        m.description = non_empty(str_of(md, "description"));
        if let Some(c) = md.get("contributors").and_then(Value::as_object) {
            m.authors = c.keys().filter_map(|k| non_empty(Some(k))).collect();
        }
        m.icon = read_icon_value(ar, md.get("icon"));
    }
    let id = non_empty(str_of(ql, "id"));
    let name = non_empty(md.and_then(|md| str_of(md, "name")));
    Some(finish(m, id, name, stem))
}

/// `Implementation-Version` from the main section of `MANIFEST.MF` (up to
/// the first blank line), with 72-byte continuation lines unfolded.
fn manifest_implementation_version(ar: &mut Jar) -> Option<String> {
    let bytes = read_capped(ar, "META-INF/MANIFEST.MF", ENTRY_MAX_BYTES)?;
    let text = String::from_utf8_lossy(&bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            break;
        }
        match (line.strip_prefix(' '), lines.last_mut()) {
            (Some(rest), Some(prev)) => prev.push_str(rest),
            _ => lines.push(line.to_owned()),
        }
    }
    lines.iter().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("Implementation-Version")
            .then(|| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

/// A TOML value that is a string or an array of strings.
fn toml_strings(v: Option<&toml::Value>) -> Vec<String> {
    match v {
        Some(toml::Value::String(s)) => non_empty(Some(s)).into_iter().collect(),
        Some(toml::Value::Array(a)) => a.iter().filter_map(|x| non_empty(x.as_str())).collect(),
        _ => Vec::new(),
    }
}

fn parse_mods_toml(ar: &mut Jar, entry: &str, format: ModFormat, stem: &str) -> Option<ModMeta> {
    let bytes = read_capped(ar, entry, ENTRY_MAX_BYTES)?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let root: toml::Table = toml::from_str(text).ok()?;
    let first = root.get("mods")?.as_array()?.first()?.as_table()?;
    let get = |key: &str| first.get(key).and_then(toml::Value::as_str);
    let top = |key: &str| root.get(key).and_then(toml::Value::as_str);

    let mut m = blank(format);
    m.version = non_empty(get("version")).and_then(|v| {
        if v.contains("${file.jarVersion}") {
            let jar_version = manifest_implementation_version(ar)?;
            Some(v.replace("${file.jarVersion}", &jar_version))
        } else {
            Some(v)
        }
    });
    m.description = non_empty(get("description"));
    m.authors = toml_strings(first.get("authors"));
    if m.authors.is_empty() {
        m.authors = toml_strings(root.get("authors"));
    }
    if let Some(p) = get("logoFile").or_else(|| top("logoFile")) {
        m.icon = read_icon(ar, p);
    }
    let id = non_empty(get("modId"));
    let name = non_empty(get("displayName"));
    Some(finish(m, id, name, stem))
}

fn parse_mcmod_info(ar: &mut Jar, entry: &str, stem: &str) -> Option<ModMeta> {
    let v = read_json(ar, entry)?;
    let first = match &v {
        Value::Array(a) => a.first()?,
        Value::Object(_) => v.get("modList")?.as_array()?.first()?,
        _ => return None,
    };
    let mut m = blank(ModFormat::LegacyForge);
    m.version = non_empty(str_of(first, "version"));
    m.description = non_empty(str_of(first, "description"));
    m.authors = ["authorList", "authors"]
        .iter()
        .find_map(|k| first.get(*k).and_then(Value::as_array))
        .map(|a| a.iter().filter_map(|x| non_empty(x.as_str())).collect())
        .unwrap_or_default();
    if let Some(p) = str_of(first, "logoFile") {
        m.icon = read_icon(ar, p);
    }
    let id = non_empty(str_of(first, "modid"));
    let name = non_empty(str_of(first, "name"));
    Some(finish(m, id, name, stem))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::forge::test_support::write_zip;

    fn png(extra: usize) -> Vec<u8> {
        let mut v = PNG_SIGNATURE.to_vec();
        v.extend(std::iter::repeat_n(7u8, extra));
        v
    }

    #[test]
    fn parses_fabric_quilt_forge_neoforge_legacy() {
        let d = tempfile::tempdir().unwrap();
        let icon = png(16);
        let p = |n: &str| d.path().join(n);

        write_zip(
            &p("fabric.jar"),
            &[
                (
                    "fabric.mod.json",
                    br#"{"id":"fab","name":"Fab Mod","version":"1.2",
                        "authors":["Ann",{"name":"Bob"}],"description":"d",
                        "icon":{"16":"small.png","64":"mid.png","128":"big.png"}}"#,
                ),
                ("small.png", b"nope"),
                ("mid.png", &icon),
                ("big.png", b"not a png at all"),
            ],
        );
        write_zip(
            &p("quilt.jar"),
            &[
                (
                    "quilt.mod.json",
                    br#"{"quilt_loader":{"id":"q","version":"2","metadata":
                        {"name":"Quilt Mod","contributors":{"Cy":"Owner"},"icon":{"32":"/q.png"}}}}"#,
                ),
                ("q.png", &icon),
            ],
        );
        write_zip(
            &p("neo.jar"),
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    b"logoFile=\"n.png\"\n[[mods]]\nmodId=\"neo\"\nversion=\"3\"\ndisplayName=\"Neo Mod\"\nauthors=\"Di\"\n",
                ),
                ("n.png", &icon),
            ],
        );
        write_zip(
            &p("forge.jar"),
            &[
                (
                    "META-INF/mods.toml",
                    b"[[mods]]\nmodId=\"fg\"\nversion=\"4\"\ndisplayName=\"Forge Mod\"\nlogoFile=\"f.png\"\n",
                ),
                ("f.png", &icon),
            ],
        );
        write_zip(
            &p("legacy.jar"),
            &[
                (
                    "mcmod.info",
                    b"\xEF\xBB\xBF{\"modListVersion\":2,\"modList\":[{\"modid\":\"lg\",\"name\":\"Legacy Mod\",\"version\":\"5\",\"authorList\":[\"Ed\"],\"logoFile\":\"l.png\"}]}",
                ),
                ("l.png", &icon),
            ],
        );
        write_zip(&p("plain.jar"), &[("a.txt", b"x")]);

        let m = |n: &str| read_mod_meta(&p(n), None).unwrap();
        let f = m("fabric.jar");
        assert_eq!((f.format, f.name.as_str()), (ModFormat::Fabric, "Fab Mod"));
        assert_eq!(f.id.as_deref(), Some("fab"));
        assert_eq!(f.version.as_deref(), Some("1.2"));
        assert_eq!(f.authors, ["Ann", "Bob"]);
        assert_eq!(f.icon.as_deref(), Some(&icon[..]));
        let q = m("quilt.jar");
        assert_eq!((q.format, q.name.as_str()), (ModFormat::Quilt, "Quilt Mod"));
        assert_eq!(q.id.as_deref(), Some("q"));
        assert_eq!(q.version.as_deref(), Some("2"));
        assert_eq!(q.authors, ["Cy"]);
        assert!(q.icon.is_some());
        let n = m("neo.jar");
        assert_eq!(
            (n.format, n.name.as_str()),
            (ModFormat::NeoForge, "Neo Mod")
        );
        assert_eq!(n.id.as_deref(), Some("neo"));
        assert_eq!(n.version.as_deref(), Some("3"));
        assert!(n.icon.is_some());
        let g = m("forge.jar");
        assert_eq!((g.format, g.name.as_str()), (ModFormat::Forge, "Forge Mod"));
        assert_eq!(g.id.as_deref(), Some("fg"));
        assert!(g.icon.is_some());
        let l = m("legacy.jar");
        assert_eq!(
            (l.format, l.name.as_str()),
            (ModFormat::LegacyForge, "Legacy Mod")
        );
        assert_eq!(l.id.as_deref(), Some("lg"));
        assert_eq!(l.version.as_deref(), Some("5"));
        assert_eq!(l.authors, ["Ed"]);
        assert!(l.icon.is_some());
        let u = m("plain.jar");
        assert_eq!((u.format, u.name.as_str()), (ModFormat::Unknown, "plain"));
        assert_eq!(u.id, None);
    }

    #[test]
    fn jar_version_placeholder_resolved() {
        let d = tempfile::tempdir().unwrap();
        let jar = d.path().join("x.jar");
        write_zip(
            &jar,
            &[
                (
                    "META-INF/mods.toml",
                    b"[[mods]]\nmodId=\"x\"\nversion=\"${file.jarVersion}\"\n",
                ),
                (
                    "META-INF/MANIFEST.MF",
                    b"Manifest-Version: 1.0\r\nImplementation-Title: x\r\nImplementation-Version: 1.\r\n 2.3\r\n\r\n",
                ),
            ],
        );
        let m = read_mod_meta(&jar, None).unwrap();
        assert_eq!(m.version.as_deref(), Some("1.2.3"));
        assert_eq!(m.name, "x");

        let jar2 = d.path().join("y.jar");
        write_zip(
            &jar2,
            &[(
                "META-INF/mods.toml",
                b"[[mods]]\nmodId=\"y\"\nversion=\"${file.jarVersion}\"\n",
            )],
        );
        assert_eq!(read_mod_meta(&jar2, None).unwrap().version, None);
    }

    #[test]
    fn enable_disable_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let jar = d.path().join("m.jar");
        std::fs::write(&jar, b"x").unwrap();
        assert_eq!(set_mod_enabled(&jar, true).unwrap(), jar);
        let off = set_mod_enabled(&jar, false).unwrap();
        assert_eq!(off, d.path().join("m.jar.disabled"));
        assert!(!jar.exists() && off.exists());
        assert_eq!(set_mod_enabled(&off, false).unwrap(), off);
        let listed = scan_mods(d.path(), None).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].enabled);
        std::fs::write(&jar, b"y").unwrap();
        let err = set_mod_enabled(&off, true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_file(&jar).unwrap();
        assert_eq!(set_mod_enabled(&off, true).unwrap(), jar);
        delete_mod(&jar).unwrap();
        assert!(!jar.exists());
        assert!(
            scan_mods(&d.path().join("missing"), None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn malformed_and_hostile_jars_fall_back() {
        let d = tempfile::tempdir().unwrap();
        let p = |n: &str| d.path().join(n);
        std::fs::create_dir(p("dir.jar")).unwrap();
        std::fs::write(p("notes.txt"), b"x").unwrap();
        write_zip(&p("badjson.jar"), &[("fabric.mod.json", b"{not json,}")]);
        std::fs::write(p("notzip.jar"), b"this is not a zip").unwrap();
        let big = png(ICON_MAX_BYTES);
        write_zip(
            &p("bigicon.jar"),
            &[
                ("fabric.mod.json", br#"{"id":"b","icon":"i.png"}"#),
                ("i.png", &big),
            ],
        );
        write_zip(
            &p("escape.jar"),
            &[("fabric.mod.json", br#"{"id":"e","icon":"../x.png"}"#)],
        );
        // Highly compressible 8 MiB entry: far over ENTRY_MAX_BYTES.
        let bomb = vec![b' '; ENTRY_MAX_BYTES * 2];
        write_zip(&p("bomb.jar"), &[("fabric.mod.json", &bomb)]);
        write_zip(
            &p("Alpha.jar"),
            &[("mcmod.info", b"[{\"modid\":\"a\",\"name\":\"A\"},]")],
        );

        let list = scan_mods(d.path(), None).unwrap();
        let names: Vec<_> = list.iter().map(|e| e.file_name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Alpha.jar",
                "badjson.jar",
                "bigicon.jar",
                "bomb.jar",
                "escape.jar",
                "notzip.jar"
            ]
        );
        let meta = |n: &str| list.iter().find(|e| e.file_name == n).unwrap().meta.clone();
        assert!(meta("Alpha.jar").is_none());
        assert!(meta("badjson.jar").is_none());
        assert!(meta("bomb.jar").is_none());
        assert!(meta("notzip.jar").is_none());
        let b = meta("bigicon.jar").unwrap();
        assert_eq!(b.name, "b");
        assert!(b.icon.is_none());
        assert!(meta("escape.jar").unwrap().icon.is_none());
    }

    #[test]
    fn random_and_truncated_zips_never_panic() {
        let d = tempfile::tempdir().unwrap();
        let good = d.path().join("good.jar");
        write_zip(
            &good,
            &[("fabric.mod.json", br#"{"id":"g"}"#), ("a", b"abc")],
        );
        let bytes = std::fs::read(&good).unwrap();
        let mut seed = 0x1234_5678_u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as usize
        };
        let f = d.path().join("fuzz.jar");
        for cut in 0..bytes.len() {
            std::fs::write(&f, &bytes[..cut]).unwrap();
            let _ = read_mod_meta(&f, None);
        }
        for _ in 0..200 {
            let mut b = bytes.clone();
            for _ in 0..4 {
                let i = next() % b.len();
                b[i] = next() as u8;
            }
            std::fs::write(&f, &b).unwrap();
            let _ = read_mod_meta(&f, None);
        }
        let rnd: Vec<u8> = (0..4096).map(|_| next() as u8).collect();
        std::fs::write(&f, rnd).unwrap();
        assert!(read_mod_meta(&f, None).is_none());
    }

    #[test]
    fn multi_loader_jar_follows_preference() {
        let d = tempfile::tempdir().unwrap();
        let jar = d.path().join("arch.jar");
        write_zip(
            &jar,
            &[
                ("fabric.mod.json", br#"{"id":"fab","name":"F"}"#),
                (
                    "META-INF/mods.toml",
                    b"[[mods]]\nmodId=\"fg\"\ndisplayName=\"G\"\n",
                ),
            ],
        );
        let fmt = |p| read_mod_meta(&jar, p).unwrap().format;
        assert_eq!(fmt(Some(ModFormat::Forge)), ModFormat::Forge);
        assert_eq!(fmt(Some(ModFormat::NeoForge)), ModFormat::Forge);
        assert_eq!(fmt(Some(ModFormat::Fabric)), ModFormat::Fabric);
        assert_eq!(fmt(Some(ModFormat::Quilt)), ModFormat::Fabric);
        assert_eq!(fmt(None), ModFormat::Fabric);

        let broken = d.path().join("broken.jar");
        write_zip(
            &broken,
            &[
                ("fabric.mod.json", b"{oops"),
                ("META-INF/mods.toml", b"[[mods]]\nmodId=\"fg\"\n"),
            ],
        );
        assert_eq!(
            read_mod_meta(&broken, None).unwrap().format,
            ModFormat::Forge
        );
    }

    #[test]
    fn icon_candidates_are_capped_and_deduped() {
        let mut map = serde_json::Map::new();
        for i in 0..10_000 {
            map.insert(i.to_string(), Value::String("same.png".into()));
        }
        assert_eq!(icon_candidates(&map), ["same.png"]);
        let mut map = serde_json::Map::new();
        for i in 0..10_000 {
            map.insert(i.to_string(), Value::String(format!("{i}.png")));
        }
        let c = icon_candidates(&map);
        assert_eq!(c, ["9999.png", "9998.png", "9997.png", "9996.png"]);

        let d = tempfile::tempdir().unwrap();
        let jar = d.path().join("m.jar");
        let icons: String = (0..10_000)
            .map(|i| format!("\"{i}\":\"x.png\""))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(r#"{{"id":"m","icon":{{{icons}}}}}"#);
        write_zip(
            &jar,
            &[("fabric.mod.json", json.as_bytes()), ("x.png", b"zz")],
        );
        let m = read_mod_meta(&jar, None).unwrap();
        assert_eq!(m.id.as_deref(), Some("m"));
        assert!(m.icon.is_none());
    }

    /// Stored zip whose local and central headers declare `declared` as the
    /// uncompressed size of every entry.
    fn zip_with_declared_size(path: &Path, entries: &[(&str, &[u8])], declared: u32) {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap();
        let mut b = std::fs::read(path).unwrap();
        for i in 0..b.len().saturating_sub(30) {
            let off = match b.get(i..i + 4) {
                Some([0x50, 0x4b, 0x01, 0x02]) => 24,
                Some([0x50, 0x4b, 0x03, 0x04]) => 22,
                _ => continue,
            };
            b[i + off..i + off + 4].copy_from_slice(&declared.to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn lying_zip_headers_do_not_panic_or_allocate() {
        let d = tempfile::tempdir().unwrap();
        let entries: [(&str, &[u8]); 2] = [
            ("fabric.mod.json", br#"{"id":"h","icon":"i.png"}"#),
            ("i.png", b"\x89PNG\r\n\x1a\n0123456789"),
        ];
        for (name, declared) in [("huge.jar", 1u32 << 30), ("short.jar", 3)] {
            let jar = d.path().join(name);
            zip_with_declared_size(&jar, &entries, declared);
            // Whatever the zip layer decides (error, or the real few bytes),
            // reading is capped by `take`, so the declared size is never
            // allocated; the test passing quickly without panic is the check.
            let _ = read_mod_meta(&jar, None);
        }
        let jar = d.path().join("huge2.jar");
        zip_with_declared_size(&jar, &[("fabric.mod.json", &vec![b' '; 1024])], 1 << 30);
        let _ = read_mod_meta(&jar, None);
    }

    #[test]
    fn manifest_main_section_and_folding() {
        let d = tempfile::tempdir().unwrap();
        let toml = b"[[mods]]\nmodId=\"x\"\nversion=\"${file.jarVersion}\"\n";
        let version = "1.2.3-a-very-long-version-string-that-forces-a-fold-0123456789";
        // Fold at 72 bytes like the JDK jar tool: continuation lines start
        // with one space.
        let line = format!("Implementation-Version: {version}");
        let (head, tail) = line.split_at(72);
        let mf = format!("Manifest-Version: 1.0\r\n{head}\r\n {tail}\r\n\r\n");
        assert!(head.len() == 72);
        let jar = d.path().join("fold.jar");
        write_zip(
            &jar,
            &[
                ("META-INF/mods.toml", toml),
                ("META-INF/MANIFEST.MF", mf.as_bytes()),
            ],
        );
        assert_eq!(
            read_mod_meta(&jar, None).unwrap().version.as_deref(),
            Some(version)
        );

        let later = d.path().join("later.jar");
        write_zip(
            &later,
            &[
                ("META-INF/mods.toml", toml),
                (
                    "META-INF/MANIFEST.MF",
                    b"Manifest-Version: 1.0\r\n\r\nName: a/b.class\r\nImplementation-Version: 9\r\n",
                ),
            ],
        );
        assert_eq!(read_mod_meta(&later, None).unwrap().version, None);
    }

    #[test]
    fn toml_authors_array_and_names() {
        let d = tempfile::tempdir().unwrap();
        let jar = d.path().join("a.jar");
        write_zip(
            &jar,
            &[(
                "META-INF/mods.toml",
                b"[[mods]]\nmodId=\"a\"\nauthors=[\"A\",\"B\"]\n",
            )],
        );
        let m = read_mod_meta(&jar, None).unwrap();
        assert_eq!(m.authors, ["A", "B"]);
        assert_eq!(m.name, "a");

        assert_eq!(split_mod_name("x.JAR"), Some(("x", true)));
        assert_eq!(split_mod_name("x.Jar.DISABLED"), Some(("x", false)));
        assert_eq!(split_mod_name("x.zip"), None);
        assert_eq!(split_mod_name(".jar"), None);

        let up = d.path().join("x.JAR.DISABLED");
        std::fs::write(&up, b"x").unwrap();
        assert_eq!(set_mod_enabled(&up, true).unwrap(), d.path().join("x.JAR"));
        let err = set_mod_enabled(&d.path().join("x.txt"), true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn delete_refuses_directories_and_scan_follows_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("d.jar");
        std::fs::create_dir(&dir).unwrap();
        assert!(delete_mod(&dir).is_err());
        assert!(dir.is_dir());

        let real = d.path().join("real.dat");
        write_zip(&real, &[("a", b"x")]);
        std::os::unix::fs::symlink(&real, d.path().join("link.jar")).unwrap();
        std::os::unix::fs::symlink(d.path().join("gone"), d.path().join("dead.jar")).unwrap();
        let names: Vec<_> = scan_mods(d.path(), None)
            .unwrap()
            .into_iter()
            .map(|e| e.file_name)
            .collect();
        assert_eq!(names, ["link.jar"]);
    }
}
