//! Platform-independent identities and immutable catalog metadata.
use super::{ContentKind, Platform};
use crate::loader::LoaderKind;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectRef {
    platform: Platform,
    id: String,
}
impl ProjectRef {
    pub fn new(platform: Platform, id: impl Into<String>) -> Self {
        Self {
            platform,
            id: id.into(),
        }
    }
    pub fn platform(&self) -> Platform {
        self.platform
    }
    pub fn id(&self) -> &str {
        &self.id
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionRef {
    project: ProjectRef,
    id: String,
}
impl VersionRef {
    pub fn new(project: ProjectRef, id: impl Into<String>) -> Self {
        Self {
            project,
            id: id.into(),
        }
    }
    pub fn project(&self) -> &ProjectRef {
        &self.project
    }
    pub fn id(&self) -> &str {
        &self.id
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseChannel {
    Release,
    Beta,
    Alpha,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetCompatibility {
    kind: ContentKind,
    mc_version: String,
    loader: Option<LoaderKind>,
    channels: Vec<ReleaseChannel>,
}
impl TargetCompatibility {
    pub fn new(
        kind: ContentKind,
        mc_version: impl Into<String>,
        loader: Option<LoaderKind>,
    ) -> Self {
        Self {
            kind,
            mc_version: mc_version.into(),
            loader: loader.filter(|_| kind == ContentKind::Mod),
            channels: vec![ReleaseChannel::Release],
        }
    }
    pub fn with_channels(mut self, channels: Vec<ReleaseChannel>) -> Self {
        self.channels = channels;
        self
    }
    pub fn kind(&self) -> ContentKind {
        self.kind
    }
    pub fn mc_version(&self) -> &str {
        &self.mc_version
    }
    pub fn loader(&self) -> Option<LoaderKind> {
        self.loader
    }
    pub fn channels(&self) -> &[ReleaseChannel] {
        &self.channels
    }
    pub fn accepts(&self, version: &ContentVersion) -> bool {
        self.channels.contains(&version.channel)
            && version.game_versions.iter().any(|v| v == &self.mc_version)
            && (self.kind != ContentKind::Mod
                || self.loader.is_some_and(|l| version.loaders.contains(&l)))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentProject {
    pub(super) reference: ProjectRef,
    pub(super) kind: ContentKind,
    pub(super) title: String,
    pub(super) description: String,
    pub(super) authors: Vec<String>,
    pub(super) url: String,
    pub(super) support_urls: Vec<String>,
    pub(super) client_supported: bool,
}
impl ContentProject {
    pub fn new(
        reference: ProjectRef,
        kind: ContentKind,
        title: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            reference,
            kind,
            title: title.into(),
            description: String::new(),
            authors: vec![],
            url: url.into(),
            support_urls: vec![],
            client_supported: true,
        }
    }
    pub fn with_description(mut self, value: impl Into<String>) -> Self {
        self.description = value.into();
        self
    }
    pub fn with_authors(mut self, value: Vec<String>) -> Self {
        self.authors = value;
        self
    }
    pub fn reference(&self) -> &ProjectRef {
        &self.reference
    }
    pub fn kind(&self) -> ContentKind {
        self.kind
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn authors(&self) -> &[String] {
        &self.authors
    }
    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn support_urls(&self) -> &[String] {
        &self.support_urls
    }
    pub fn is_client_supported(&self) -> bool {
        self.client_supported
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyRelation {
    Required,
    Optional,
    Incompatible,
    Embedded,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub(super) project: Option<ProjectRef>,
    pub(super) version: Option<String>,
    pub(super) relation: DependencyRelation,
}
impl Dependency {
    pub fn new(
        project: Option<ProjectRef>,
        version: Option<String>,
        relation: DependencyRelation,
    ) -> Self {
        Self {
            project,
            version,
            relation,
        }
    }
    pub fn project(&self) -> Option<&ProjectRef> {
        self.project.as_ref()
    }
    pub fn version_id(&self) -> Option<&str> {
        self.version.as_deref()
    }
    pub fn relation(&self) -> DependencyRelation {
        self.relation
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileHashes {
    pub(super) sha1: Option<String>,
    pub(super) sha512: Option<String>,
    pub(super) md5: Option<String>,
}
impl FileHashes {
    pub fn new(sha1: Option<String>, sha512: Option<String>, md5: Option<String>) -> Self {
        Self { sha1, sha512, md5 }
    }
    pub fn sha1(&self) -> Option<&str> {
        self.sha1.as_deref()
    }
    pub fn sha512(&self) -> Option<&str> {
        self.sha512.as_deref()
    }
    pub fn md5(&self) -> Option<&str> {
        self.md5.as_deref()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentFile {
    pub(super) name: String,
    pub(super) size: u64,
    pub(super) hashes: FileHashes,
    pub(super) download_url: Option<String>,
    pub(super) page_url: String,
    pub(super) primary: bool,
}
impl ContentFile {
    pub fn new(
        name: impl Into<String>,
        size: u64,
        hashes: FileHashes,
        download_url: Option<String>,
        page_url: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            size,
            hashes,
            download_url,
            page_url: page_url.into(),
            primary: true,
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn hashes(&self) -> &FileHashes {
        &self.hashes
    }
    pub fn download_url(&self) -> Option<&str> {
        self.download_url.as_deref()
    }
    pub fn page_url(&self) -> &str {
        &self.page_url
    }
    pub fn is_primary(&self) -> bool {
        self.primary
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentVersion {
    pub(super) reference: VersionRef,
    pub(super) name: String,
    pub(super) channel: ReleaseChannel,
    pub(super) published: u64,
    pub(super) game_versions: Vec<String>,
    pub(super) loaders: Vec<LoaderKind>,
    pub(super) files: Vec<ContentFile>,
    pub(super) dependencies: Vec<Dependency>,
}
impl ContentVersion {
    pub fn new(reference: VersionRef, name: impl Into<String>, channel: ReleaseChannel) -> Self {
        Self {
            reference,
            name: name.into(),
            channel,
            published: 0,
            game_versions: vec![],
            loaders: vec![],
            files: vec![],
            dependencies: vec![],
        }
    }
    pub fn with_compatibility(
        mut self,
        game_versions: Vec<String>,
        loaders: Vec<LoaderKind>,
    ) -> Self {
        self.game_versions = game_versions;
        self.loaders = loaders;
        self
    }
    pub fn with_files(mut self, files: Vec<ContentFile>) -> Self {
        self.files = files;
        self
    }
    pub fn with_dependencies(mut self, dependencies: Vec<Dependency>) -> Self {
        self.dependencies = dependencies;
        self
    }
    pub fn with_published(mut self, published: u64) -> Self {
        self.published = published;
        self
    }
    pub fn reference(&self) -> &VersionRef {
        &self.reference
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn channel(&self) -> ReleaseChannel {
        self.channel
    }
    pub fn published(&self) -> u64 {
        self.published
    }
    pub fn game_versions(&self) -> &[String] {
        &self.game_versions
    }
    pub fn loaders(&self) -> &[LoaderKind] {
        &self.loaders
    }
    pub fn files(&self) -> &[ContentFile] {
        &self.files
    }
    pub fn dependencies(&self) -> &[Dependency] {
        &self.dependencies
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentCategory {
    pub(super) id: String,
    pub(super) name: String,
}
impl ContentCategory {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
        }
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    name: String,
    sha1: String,
    fingerprint: u32,
}
impl FileIdentity {
    /// Computes only the identifiers sent to catalogs; file bytes never leave this process.
    pub fn from_bytes(name: impl Into<String>, bytes: &[u8]) -> Self {
        let normalized: Vec<_> = bytes
            .iter()
            .copied()
            .filter(|b| !matches!(b, 9 | 10 | 13 | 32))
            .collect();
        Self {
            name: name.into(),
            sha1: hex::encode(Sha1::digest(bytes)),
            fingerprint: murmur2::murmur2(&normalized, 1),
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn sha1(&self) -> &str {
        &self.sha1
    }
    pub fn fingerprint(&self) -> u32 {
        self.fingerprint
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifiedFile {
    pub(super) identity: FileIdentity,
    pub(super) matches: Vec<ContentVersion>,
}
impl IdentifiedFile {
    pub fn new(identity: FileIdentity, matches: Vec<ContentVersion>) -> Self {
        Self { identity, matches }
    }
    pub fn identity(&self) -> &FileIdentity {
        &self.identity
    }
    pub fn matches(&self) -> &[ContentVersion] {
        &self.matches
    }
}
