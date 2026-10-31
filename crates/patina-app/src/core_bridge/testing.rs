//! Test doubles: a [`Core`] that never touches the network and a
//! [`FakeCatalog`] with canned version lists. Only in this crate's tests and
//! with the `test-support` feature (for `tests/`), never in a release build.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::BoxFuture;
use patina_core::content::online::{ContentSource, OnlineError, OnlinePage, OnlineQuery};
use patina_core::instance::InstanceStore;
use patina_core::loader::ops::{LoaderAction, OpOutcome};
use patina_core::loader::{LoaderError, LoaderKind, LoaderVersion, VersionPick};
use patina_core::meta::VersionManifest;
use patina_core::paths::DataDirs;
use patina_core::progress::ProgressSink;
use tokio::runtime::Handle;

use super::service::{Fakes, Startup};
use super::{Catalog, Core, LaunchBackend, VersionOps};

/// A [`Core`] over `data` that lists versions from `catalog`. It loads the
/// config and the accounts as the real one does and follows a configured
/// download source, but never speed-tests.
pub fn core(rt: Handle, data: DataDirs, catalog: Arc<dyn Catalog>) -> Core {
    let fakes = Fakes {
        catalog: Some(catalog),
        ..Fakes::default()
    };
    Core::start(rt, data, fakes, Startup::Offline).expect("the test core starts")
}

/// [`core`] whose loader and version changes go through `ops` (a
/// [`FakeVersionOps`]) instead of the network.
pub fn core_with_ops(
    rt: Handle,
    data: DataDirs,
    catalog: Arc<dyn Catalog>,
    ops: Arc<dyn VersionOps>,
) -> Core {
    let fakes = Fakes {
        catalog: Some(catalog),
        version_ops: Some(ops),
        ..Fakes::default()
    };
    Core::start(rt, data, fakes, Startup::Offline).expect("the test core starts")
}

/// [`core`] that prepares launches through `launch` (fake games).
pub fn core_with_launch(
    rt: Handle,
    data: DataDirs,
    catalog: Arc<dyn Catalog>,
    launch: Arc<dyn LaunchBackend>,
) -> Core {
    let fakes = Fakes {
        catalog: Some(catalog),
        launch_backend: Some(launch),
        ..Fakes::default()
    };
    Core::start(rt, data, fakes, Startup::Offline).expect("the test core starts")
}

/// [`core`] with fake games (`launch`) and recorded loader changes (`ops`).
pub fn core_with_launch_and_ops(
    rt: Handle,
    data: DataDirs,
    catalog: Arc<dyn Catalog>,
    launch: Arc<dyn LaunchBackend>,
    ops: Arc<dyn VersionOps>,
) -> Core {
    let fakes = Fakes {
        catalog: Some(catalog),
        launch_backend: Some(launch),
        version_ops: Some(ops),
        ..Fakes::default()
    };
    Core::start(rt, data, fakes, Startup::Offline).expect("the test core starts")
}

/// [`core`] whose online searches go to `source` (a [`FakeSource`]).
pub fn core_with_source(
    rt: Handle,
    data: DataDirs,
    catalog: Arc<dyn Catalog>,
    source: Arc<dyn ContentSource>,
) -> Core {
    let fakes = Fakes {
        catalog: Some(catalog),
        content_source: Some(source),
        ..Fakes::default()
    };
    Core::start(rt, data, fakes, Startup::Offline).expect("the test core starts")
}

type Answerer = Box<dyn Fn(&OnlineQuery) -> Result<OnlinePage, OnlineError> + Send + Sync>;
type Waiting = (
    OnlineQuery,
    futures::channel::oneshot::Sender<Result<OnlinePage, OnlineError>>,
);

enum SourceMode {
    Answer(Answerer),
    /// Every search waits for [`FakeSource::answer`].
    Gated(Mutex<Vec<Option<Waiting>>>),
}

/// An online source that records its queries. It answers with a canned
/// result ([`FakeSource::new`]), with a function of the query
/// ([`FakeSource::with`]) or when told to ([`FakeSource::gated`]).
pub struct FakeSource {
    mode: SourceMode,
    queries: Mutex<Vec<OnlineQuery>>,
}

impl FakeSource {
    pub fn new(result: Result<OnlinePage, OnlineError>) -> Self {
        Self::with(move |_| result.clone())
    }

    /// Answers each query with `f(query)`.
    pub fn with(
        f: impl Fn(&OnlineQuery) -> Result<OnlinePage, OnlineError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            mode: SourceMode::Answer(Box::new(f)),
            queries: Mutex::new(Vec::new()),
        }
    }

    /// A source whose searches stay in flight until [`FakeSource::answer`].
    pub fn gated() -> Self {
        Self {
            mode: SourceMode::Gated(Mutex::new(Vec::new())),
            queries: Mutex::new(Vec::new()),
        }
    }

    /// Answers the `n`th search (0-based, in call order) of a gated source.
    pub fn answer(&self, n: usize, result: Result<OnlinePage, OnlineError>) {
        let SourceMode::Gated(waiting) = &self.mode else {
            panic!("not a gated source");
        };
        let (_, tx) = waiting.lock().unwrap()[n].take().expect("answered once");
        tx.send(result).ok();
    }

    /// The queries so far, oldest first.
    pub fn queries(&self) -> Vec<OnlineQuery> {
        self.queries.lock().unwrap().clone()
    }
}

impl ContentSource for FakeSource {
    fn search(&self, q: OnlineQuery) -> BoxFuture<'static, Result<OnlinePage, OnlineError>> {
        self.queries.lock().unwrap().push(q.clone());
        match &self.mode {
            SourceMode::Answer(f) => futures::future::ready(f(&q)).boxed(),
            SourceMode::Gated(waiting) => {
                let (tx, rx) = futures::channel::oneshot::channel();
                waiting.lock().unwrap().push(Some((q, tx)));
                async move { rx.await.unwrap_or(Err(OnlineError::Unavailable)) }.boxed()
            }
        }
    }
}

/// Canned version lists. With no manifest set, asking for it fails; a loader
/// with no versions for a Minecraft version answers `NoVersions`.
pub struct FakeCatalog {
    manifest: Mutex<Result<VersionManifest, String>>,
    loaders: Vec<LoaderVersion>,
    manifest_requests: AtomicUsize,
    loader_requests: AtomicUsize,
}

impl FakeCatalog {
    pub fn new() -> Self {
        Self {
            manifest: Mutex::new(Err("no manifest in this fake".into())),
            loaders: Vec::new(),
            manifest_requests: AtomicUsize::new(0),
            loader_requests: AtomicUsize::new(0),
        }
    }

    pub fn with_manifest(mut self, manifest: VersionManifest) -> Self {
        self.manifest = Mutex::new(Ok(manifest));
        self
    }

    /// Changes the answer to later manifest requests.
    pub fn set_manifest(&self, manifest: Result<VersionManifest, String>) {
        *self.manifest.lock().unwrap() = manifest;
    }

    pub fn with_manifest_error(mut self, error: impl Into<String>) -> Self {
        self.manifest = Mutex::new(Err(error.into()));
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
        futures::future::ready(self.manifest.lock().unwrap().clone()).boxed()
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

/// One call a [`FakeVersionOps`] has seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpCall {
    InstallLoader {
        id: String,
        kind: LoaderKind,
        pick: VersionPick,
        replace_existing: bool,
    },
    SetMcVersion {
        id: String,
        mc: String,
        action: Option<LoaderAction>,
    },
    Reinstall {
        id: String,
    },
    RemoveLoader {
        id: String,
    },
}

/// Loader and version changes that only record their calls. Each call
/// answers with the next scripted result ([`FakeVersionOps::then`]); with
/// none left it succeeds, returning the instance as stored (unchanged).
pub struct FakeVersionOps {
    store: InstanceStore,
    calls: Mutex<Vec<OpCall>>,
    results: Mutex<VecDeque<Result<OpOutcome, LoaderError>>>,
}

impl FakeVersionOps {
    /// Fake operations over `store`, where the instances they answer for
    /// live.
    pub fn new(store: InstanceStore) -> Self {
        Self {
            store,
            calls: Mutex::new(Vec::new()),
            results: Mutex::new(VecDeque::new()),
        }
    }

    /// Queues the result of the next call that has no result queued yet.
    pub fn then(&self, result: Result<OpOutcome, LoaderError>) {
        self.results.lock().unwrap().push_back(result);
    }

    /// The calls so far, oldest first.
    pub fn calls(&self) -> Vec<OpCall> {
        self.calls.lock().unwrap().clone()
    }

    fn answer(&self, call: OpCall, id: &str) -> BoxFuture<'static, Result<OpOutcome, LoaderError>> {
        self.calls.lock().unwrap().push(call);
        let queued = self.results.lock().unwrap().pop_front();
        let result = queued.unwrap_or_else(|| {
            self.store
                .load(id)
                .map(|instance| OpOutcome {
                    instance,
                    notices: Vec::new(),
                })
                .map_err(|e| LoaderError::Instance(e.to_string()))
        });
        futures::future::ready(result).boxed()
    }
}

impl VersionOps for FakeVersionOps {
    fn install_loader(
        &self,
        id: String,
        kind: LoaderKind,
        pick: VersionPick,
        replace_existing: bool,
        _: ProgressSink,
    ) -> BoxFuture<'static, Result<OpOutcome, LoaderError>> {
        let call = OpCall::InstallLoader {
            id: id.clone(),
            kind,
            pick,
            replace_existing,
        };
        self.answer(call, &id)
    }

    fn set_mc_version(
        &self,
        id: String,
        mc: String,
        action: Option<LoaderAction>,
        _: ProgressSink,
    ) -> BoxFuture<'static, Result<OpOutcome, LoaderError>> {
        let call = OpCall::SetMcVersion {
            id: id.clone(),
            mc,
            action,
        };
        self.answer(call, &id)
    }

    fn reinstall_loader(
        &self,
        id: String,
        _: ProgressSink,
    ) -> BoxFuture<'static, Result<OpOutcome, LoaderError>> {
        self.answer(OpCall::Reinstall { id: id.clone() }, &id)
    }

    fn remove_loader(&self, id: String) -> BoxFuture<'static, Result<OpOutcome, LoaderError>> {
        self.answer(OpCall::RemoveLoader { id: id.clone() }, &id)
    }
}
