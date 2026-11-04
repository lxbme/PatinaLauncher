use crate::content::online::VersionRef;
use crate::loader::LoaderKind;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct PackFile {
    pub(super) path: String,
    pub(super) sha1: String,
    pub(super) sha512: String,
    pub(super) downloads: Vec<String>,
    pub(super) size: u64,
    pub(super) optional: bool,
}
impl PackFile {
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn is_optional(&self) -> bool {
        self.optional
    }
    pub fn size(&self) -> u64 {
        self.size
    }
}
/// Immutable, bounded archive contents. Confirmation never reopens a mutable input path.
#[derive(Debug, Clone)]
pub struct PackPreview {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) mc: String,
    pub(super) loader: Option<(LoaderKind, String)>,
    pub(super) files: Vec<PackFile>,
    pub(super) overrides: Arc<BTreeMap<String, Vec<u8>>>,
    pub(super) digest: String,
    pub(super) source: Option<VersionRef>,
    pub(super) project_icon: Option<String>,
}
impl PackPreview {
    pub fn with_project_icon(mut self, url: Option<String>) -> Self {
        self.project_icon = url;
        self
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn mc_version(&self) -> &str {
        &self.mc
    }
    pub fn loader(&self) -> Option<(LoaderKind, &str)> {
        self.loader.as_ref().map(|(k, v)| (*k, v.as_str()))
    }
    pub fn files(&self) -> &[PackFile] {
        &self.files
    }
    pub fn overrides(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.overrides
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn source(&self) -> Option<&VersionRef> {
        self.source.as_ref()
    }
}
