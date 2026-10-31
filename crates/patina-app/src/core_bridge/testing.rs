//! Test doubles: a [`Core`] that never touches the network and a
//! [`FakeCatalog`] with canned version lists. Only in this crate's tests and
//! with the `test-support` feature (for `tests/`), never in a release build.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::FutureExt;
use futures::future::BoxFuture;
use patina_core::loader::{LoaderError, LoaderKind, LoaderVersion};
use patina_core::meta::VersionManifest;
use patina_core::paths::DataDirs;
use tokio::runtime::Handle;

use super::service::Startup;
use super::{Catalog, Core};

/// A [`Core`] over `data` that lists versions from `catalog`. It loads the
/// config and the accounts as the real one does and follows a configured
/// download source, but never speed-tests.
pub fn core(rt: Handle, data: DataDirs, catalog: Arc<dyn Catalog>) -> Core {
    Core::start(rt, data, Some(catalog), Startup::Offline).expect("the test core starts")
}

/// Canned version lists. With no manifest set, asking for it fails; a loader
/// with no versions for a Minecraft version answers `NoVersions`.
pub struct FakeCatalog {
    manifest: Result<VersionManifest, String>,
    loaders: Vec<LoaderVersion>,
    manifest_requests: AtomicUsize,
    loader_requests: AtomicUsize,
}

impl FakeCatalog {
    pub fn new() -> Self {
        Self {
            manifest: Err("no manifest in this fake".into()),
            loaders: Vec::new(),
            manifest_requests: AtomicUsize::new(0),
            loader_requests: AtomicUsize::new(0),
        }
    }

    pub fn with_manifest(mut self, manifest: VersionManifest) -> Self {
        self.manifest = Ok(manifest);
        self
    }

    pub fn with_manifest_error(mut self, error: impl Into<String>) -> Self {
        self.manifest = Err(error.into());
        self
    }

    /// Adds loader versions; each answers for its own `kind` and `mc`.
    pub fn with_loader_versions(
        mut self,
        versions: impl IntoIterator<Item = LoaderVersion>,
    ) -> Self {
        self.loaders.extend(versions);
        self
    }

    pub fn manifest_requests(&self) -> usize {
        self.manifest_requests.load(Ordering::SeqCst)
    }

    pub fn loader_requests(&self) -> usize {
        self.loader_requests.load(Ordering::SeqCst)
    }
}

impl Default for FakeCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl Catalog for FakeCatalog {
    fn manifest(&self) -> BoxFuture<'static, Result<VersionManifest, String>> {
        self.manifest_requests.fetch_add(1, Ordering::SeqCst);
        futures::future::ready(self.manifest.clone()).boxed()
    }

    fn loader_versions(
        &self,
        kind: LoaderKind,
        mc: String,
    ) -> BoxFuture<'static, Result<Vec<LoaderVersion>, LoaderError>> {
        self.loader_requests.fetch_add(1, Ordering::SeqCst);
        let list: Vec<_> = (self.loaders.iter())
            .filter(|v| v.kind == kind && v.mc == mc)
            .cloned()
            .collect();
        let result = if list.is_empty() {
            Err(LoaderError::NoVersions { kind, mc })
        } else {
            Ok(list)
        };
        futures::future::ready(result).boxed()
    }
}
