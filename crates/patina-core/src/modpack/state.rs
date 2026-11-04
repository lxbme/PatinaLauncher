use super::{
    PackError,
    archive::{checked_path, valid_hash},
};
use crate::{content::online::VersionRef, instance::Component};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read, path::Path};
pub(super) const STATE_FILE: &str = "modpack-state.json";
pub(super) const PENDING_FILE: &str = "modpack-pending";
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineFile {
    pub(super) sha512: String,
    pub(super) source: Option<VersionRef>,
}
impl BaselineFile {
    pub fn sha512(&self) -> &str {
        &self.sha512
    }
    pub fn source(&self) -> Option<&VersionRef> {
        self.source.as_ref()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackState {
    pub(super) schema: u32,
    #[serde(default)]
    pub(super) archive_digest: String,
    pub(super) name: String,
    pub(super) version: String,
    pub(super) source: Option<VersionRef>,
    pub(super) components: Vec<Component>,
    /// Always the author's target, never overwritten with retained player bytes.
    pub(super) files: BTreeMap<String, BaselineFile>,
    pub(super) optional: BTreeMap<String, bool>,
    pub(super) ready: bool,
    #[serde(default)]
    pub(super) bindings: BTreeMap<String, String>,
    pub(super) retained: BTreeMap<String, Option<String>>,
}
impl PackState {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn source(&self) -> Option<&VersionRef> {
        self.source.as_ref()
    }
    pub fn components(&self) -> &[Component] {
        &self.components
    }
    pub fn files(&self) -> &BTreeMap<String, BaselineFile> {
        &self.files
    }
    pub fn optional_choices(&self) -> &BTreeMap<String, bool> {
        &self.optional
    }
    pub fn is_ready(&self) -> bool {
        self.ready
    }
}
pub fn load_state(dir: &Path) -> Result<Option<PackState>, PackError> {
    if std::fs::symlink_metadata(dir.join(STATE_FILE)).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(PackError::InvalidState);
    }
    let mut f = match std::fs::File::open(dir.join(STATE_FILE)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut b = Vec::new();
    f.by_ref().take(MAX_STATE_BYTES + 1).read_to_end(&mut b)?;
    if b.len() as u64 > MAX_STATE_BYTES {
        return Err(PackError::InvalidState);
    }
    let s: PackState = serde_json::from_slice(&b).map_err(|_| PackError::InvalidState)?;
    if s.schema != 1
        || s.files.len() > 20_000
        || (s.ready && s.components.is_empty())
        || s.files
            .iter()
            .any(|(p, f)| checked_path(p).is_err() || !valid_hash(&f.sha512, 128))
        || s.bindings.iter().any(|(a, b)| {
            !s.files.contains_key(a) || checked_path(a).is_err() || checked_path(b).is_err()
        })
        || s.optional
            .keys()
            .chain(s.retained.keys())
            .any(|p| checked_path(p).is_err())
    {
        return Err(PackError::InvalidState);
    }
    Ok(Some(s))
}
pub(super) fn save_state(dir: &Path, state: &PackState) -> Result<(), PackError> {
    crate::fsutil::write_atomic(&dir.join(STATE_FILE), &serde_json::to_vec_pretty(state)?)?;
    Ok(())
}
/// Core launch barrier, including corrupted state and interrupted application after restart.
pub fn ensure_ready(dir: &Path) -> Result<(), PackError> {
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(PackError::InvalidState);
    }
    if std::fs::symlink_metadata(dir.join(PENDING_FILE)).is_ok() {
        return Err(PackError::NotReady);
    }
    if load_state(dir)?.is_some_and(|s| !s.ready) {
        return Err(PackError::NotReady);
    }
    Ok(())
}
