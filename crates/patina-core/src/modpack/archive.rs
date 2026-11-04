use super::{PackError, PackFile, PackPreview};
use crate::loader::LoaderKind;
use serde::Deserialize;
use sha2::{Digest, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read},
    path::Path,
    sync::Arc,
};

const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_PACK_DOWNLOAD_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub(super) const MAX_PACK_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// Archive expansion is bounded independently of compressed input size.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveLimits {
    entries: usize,
    expanded: u64,
    archive: u64,
}
impl Default for ArchiveLimits {
    fn default() -> Self {
        Self::new(20_000, 512 * 1024 * 1024, 256 * 1024 * 1024)
    }
}
impl ArchiveLimits {
    pub fn new(entries: usize, expanded: u64, archive: u64) -> Self {
        Self {
            entries,
            expanded,
            archive,
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    format_version: u32,
    game: String,
    version_id: String,
    name: String,
    files: Vec<File>,
    dependencies: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    path: String,
    hashes: BTreeMap<String, String>,
    downloads: Vec<String>,
    file_size: u64,
    env: Option<Env>,
}
#[derive(Deserialize)]
struct Env {
    client: Requirement,
    #[serde(rename = "server")]
    _server: Requirement,
}
#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Requirement {
    Required,
    Optional,
    Unsupported,
}

pub fn inspect_archive(path: &Path) -> Result<PackPreview, PackError> {
    let limits = ArchiveLimits::default();
    let mut b = Vec::new();
    std::fs::File::open(path)?
        .take(limits.archive + 1)
        .read_to_end(&mut b)?;
    inspect_bytes(b, limits)
}
pub fn inspect_bytes(bytes: Vec<u8>, limits: ArchiveLimits) -> Result<PackPreview, PackError> {
    if bytes.len() as u64 > limits.archive {
        return Err(PackError::Limit);
    }
    let digest = hex::encode(Sha512::digest(&bytes));
    let mut z = zip::ZipArchive::new(Cursor::new(bytes))?;
    if z.len() > limits.entries {
        return Err(PackError::Limit);
    }
    let mut entries = BTreeMap::new();
    let mut aliases = BTreeSet::new();
    let mut total = 0u64;
    for ix in 0..z.len() {
        let mut f = z.by_index(ix)?;
        let name = f.name()?.into_owned();
        if f.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err(PackError::UnsafePath(name));
        }
        let path = name.trim_end_matches('/');
        checked_path(path)?;
        if !aliases.insert(path.to_lowercase()) {
            return Err(PackError::UnsafePath(name));
        }
        total = total.checked_add(f.size()).ok_or(PackError::Limit)?;
        if total > limits.expanded {
            return Err(PackError::Limit);
        }
        if f.is_dir() {
            continue;
        }
        let mut b = Vec::new();
        f.by_ref().take(limits.expanded + 1).read_to_end(&mut b)?;
        if b.len() as u64 != f.size() {
            return Err(PackError::Invalid);
        }
        entries.insert(name, b);
    }
    let index = entries
        .remove("modrinth.index.json")
        .ok_or(PackError::Unsupported)?;
    if index.len() > MAX_MANIFEST_BYTES {
        return Err(PackError::Limit);
    }
    let m: Manifest = serde_json::from_slice(&index)?;
    if m.files.len() > limits.entries {
        return Err(PackError::Limit);
    }
    let mut download_bytes = 0u64;
    for f in &m.files {
        download_bytes = download_bytes
            .checked_add(f.file_size)
            .ok_or(PackError::Limit)?;
        if f.file_size > MAX_PACK_FILE_BYTES || download_bytes > MAX_PACK_DOWNLOAD_BYTES {
            return Err(PackError::Limit);
        }
    }
    if m.format_version != 1 || m.game != "minecraft" {
        return Err(PackError::Unsupported);
    }
    if m.name.trim().is_empty() || m.version_id.trim().is_empty() {
        return Err(PackError::Invalid);
    }
    let mc = m
        .dependencies
        .get("minecraft")
        .filter(|v| safe_version(v))
        .ok_or(PackError::Invalid)?
        .clone();
    let mut loader = None;
    for (k, v) in &m.dependencies {
        if k == "minecraft" {
            continue;
        }
        let kind = match k.as_str() {
            "fabric-loader" => LoaderKind::Fabric,
            "forge" => LoaderKind::Forge,
            "neoforge" => LoaderKind::Neoforge,
            _ => return Err(PackError::Unsupported),
        };
        if loader.is_some() || !safe_version(v) {
            return Err(PackError::Unsupported);
        }
        loader = Some((kind, v.clone()));
    }
    let mut files = Vec::new();
    let mut paths = BTreeSet::new();
    for f in m.files {
        checked_path(&f.path)?;
        check_target(&mut paths, &f.path)?;
        let sha1 = f
            .hashes
            .get("sha1")
            .filter(|h| valid_hash(h, 40))
            .ok_or(PackError::Invalid)?
            .to_lowercase();
        let sha512 = f
            .hashes
            .get("sha512")
            .filter(|h| valid_hash(h, 128))
            .ok_or(PackError::Invalid)?
            .to_lowercase();
        if f.downloads.is_empty() || f.downloads.iter().any(|u| !valid_download_url(u)) {
            return Err(PackError::Invalid);
        }
        let requirement = f.env.map(|e| e.client).unwrap_or(Requirement::Required);
        if requirement == Requirement::Unsupported {
            continue;
        }
        files.push(PackFile {
            path: f.path,
            sha1,
            sha512,
            downloads: f.downloads,
            size: f.file_size,
            optional: requirement == Requirement::Optional,
        });
    }
    let mut overrides = BTreeMap::new();
    for prefix in ["overrides/", "client-overrides/"] {
        let mut layer = BTreeSet::new();
        for (name, b) in &entries {
            if let Some(path) = name.strip_prefix(prefix) {
                checked_path(path)?;
                check_target(&mut layer, path)?;
                // Same exact path may override a manifest/earlier layer, never an alias or parent.
                let lower = path.to_lowercase();
                if paths.contains(&lower)
                    && !files.iter().any(|f| f.path == path)
                    && !overrides.contains_key(path)
                {
                    return Err(PackError::UnsafePath(path.into()));
                }
                if !paths.contains(&lower) {
                    check_target(&mut paths, path)?;
                }
                overrides.insert(path.to_string(), b.clone());
            }
        }
    }
    Ok(PackPreview {
        name: m.name,
        version: m.version_id,
        mc,
        loader,
        files,
        overrides: Arc::new(overrides),
        digest,
        source: None,
        project_icon: None,
    })
}
pub(super) fn valid_hash(h: &str, n: usize) -> bool {
    h.len() == n && h.bytes().all(|b| b.is_ascii_hexdigit())
}
pub(super) fn safe_version(v: &str) -> bool {
    !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
        && v != "."
        && v != ".."
}
pub(super) fn checked_path(path: &str) -> Result<(), PackError> {
    if path.is_empty()
        || path.len() > 1024
        || path.contains('\\')
        || path.split('/').any(|p| {
            p.is_empty()
                || p == "."
                || p == ".."
                || p.ends_with(['.', ' '])
                || p.chars().any(|c| c.is_control() || "<>:\"|?*".contains(c))
                || {
                    let s = p.split('.').next().unwrap_or("").to_ascii_uppercase();
                    matches!(s.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                        || ["COM", "LPT"].iter().any(|a| {
                            s.strip_prefix(a).is_some_and(|n| {
                                n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9')
                            })
                        })
                }
        })
    {
        return Err(PackError::UnsafePath(path.into()));
    }
    Ok(())
}
fn check_target(paths: &mut BTreeSet<String>, path: &str) -> Result<(), PackError> {
    let p = path.to_lowercase();
    if paths.iter().any(|old| {
        old == &p || old.starts_with(&format!("{p}/")) || p.starts_with(&format!("{old}/"))
    }) {
        return Err(PackError::UnsafePath(path.into()));
    }
    paths.insert(p);
    Ok(())
}
/// Official MR allowlist (2026-11-03); redirects are separately checked by the pack client.
pub(super) fn valid_download_url(s: &str) -> bool {
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    reqwest::Url::parse(s).is_ok_and(|u| {
        u.scheme() == "https"
            && u.username().is_empty()
            && u.password().is_none()
            && u.port().is_none()
            && matches!(
                u.host_str(),
                Some(
                    "cdn.modrinth.com" | "github.com" | "raw.githubusercontent.com" | "gitlab.com"
                )
            )
    })
}
