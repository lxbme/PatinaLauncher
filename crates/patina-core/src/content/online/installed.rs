//! Minimal per-instance provenance; never infer ownership from a display name.
use super::*;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRecord {
    pub(super) version: VersionRef,
    pub(super) kind: ContentKind,
    pub(super) path: String,
    pub(super) sha1: String,
}
impl SourceRecord {
    pub fn version(&self) -> &VersionRef {
        &self.version
    }
    pub fn kind(&self) -> ContentKind {
        self.kind
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn sha1(&self) -> &str {
        &self.sha1
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSources {
    schema_version: u32,
    pub(super) records: Vec<SourceRecord>,
}
impl Default for ContentSources {
    fn default() -> Self {
        Self {
            schema_version: 1,
            records: vec![],
        }
    }
}
impl ContentSources {
    pub fn records(&self) -> &[SourceRecord] {
        &self.records
    }
}
pub fn load_sources(instance_dir: &Path) -> Result<ContentSources, ContentError> {
    let path = instance_dir.join("content-sources.toml");
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ContentSources::default()),
        Err(e) => return Err(e.into()),
    };
    let mut text = String::new();
    (&mut file)
        .take(4 * 1024 * 1024 + 1)
        .read_to_string(&mut text)?;
    if text.len() > 4 * 1024 * 1024 {
        return Err(ContentError::InvalidSources);
    }
    let sources: ContentSources =
        toml::from_str(&text).map_err(|_| ContentError::InvalidSources)?;
    let mut projects = std::collections::HashSet::new();
    let mut paths = std::collections::HashSet::new();
    if sources.schema_version != 1
        || sources.records.iter().any(|r| {
            !valid_content_path(r.kind, &r.path)
                || r.sha1.len() != 40
                || !r.sha1.bytes().all(|b| b.is_ascii_hexdigit())
                || !projects.insert(r.version.project())
                || !paths.insert(&r.path)
        })
    {
        return Err(ContentError::InvalidSources);
    }
    Ok(sources)
}
pub(super) fn save_sources(dir: &Path, sources: &ContentSources) -> Result<(), ContentError> {
    let text = toml::to_string(sources).map_err(|_| ContentError::InvalidSources)?;
    crate::fsutil::write_atomic(&dir.join("content-sources.toml"), text.as_bytes())?;
    Ok(())
}
pub(super) fn folder(kind: ContentKind) -> Result<&'static str, ContentError> {
    match kind {
        ContentKind::Mod => Ok("mods"),
        ContentKind::ResourcePack => Ok("resourcepacks"),
        ContentKind::Shader => Ok("shaderpacks"),
        ContentKind::Modpack => Err(ContentError::Unsupported),
    }
}
pub(super) fn valid_content_path(kind: ContentKind, path: &str) -> bool {
    let Some((dir, name)) = path.split_once('/') else {
        return false;
    };
    folder(kind).is_ok_and(|f| f == dir) && crate::fsutil::is_safe_name(name)
}
pub(super) fn effective_record(root: &Path, record: &SourceRecord) -> SourceRecord {
    let mut record = record.clone();
    if record.kind == ContentKind::Mod && !root.join(&record.path).exists() {
        let alternate = if let Some(path) = record.path.strip_suffix(".disabled") {
            path.to_string()
        } else {
            format!("{}.disabled", record.path)
        };
        if root.join(&alternate).is_file() {
            record.path = alternate;
        }
    }
    record
}

/// Registers only verified manifest files; this never resolves dependencies or fetches metadata.
pub(crate) fn record_pack_files(
    dir: &Path,
    records: Vec<(VersionRef, ContentKind, String, String)>,
    replaced: &[String],
) -> Result<(), ContentError> {
    let mut sources = load_sources(dir)?;
    sources
        .records
        .retain(|r| !replaced.contains(&r.path) && dir.join("minecraft").join(&r.path).is_file());
    let mut projects = std::collections::HashSet::new();
    for (version, kind, path, sha1) in records {
        if !valid_content_path(kind, &path)
            || sha1.len() != 40
            || !projects.insert(version.project().clone())
        {
            return Err(ContentError::InvalidSources);
        }
        sources
            .records
            .retain(|r| r.version.project() != version.project() && r.path != path);
        sources.records.push(SourceRecord {
            version,
            kind,
            path,
            sha1,
        });
    }
    save_sources(dir, &sources)
}
