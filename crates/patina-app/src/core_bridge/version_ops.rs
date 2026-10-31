//! [`VersionOps`]: the loader and Minecraft-version changes of an instance.
//! Injectable so UI tests record the calls instead of installing anything.

use std::sync::{Arc, RwLock as StdRwLock};

use futures::FutureExt;
use futures::future::BoxFuture;
use patina_core::instance::InstanceStore;
use patina_core::loader::ops::{
    LoaderAction, OpOutcome, install_loader, reinstall_loader, remove_loader, set_mc_version,
};
use patina_core::loader::{LoaderError, LoaderKind, VersionPick};
use patina_core::prepare::Launcher;
use patina_core::progress::ProgressSink;

use super::service::current;

/// Changes what an instance runs. The app reaches it through
/// `Core.version_ops`; the futures do file and network I/O, so poll them on
/// the runtime ([`run`](super::run)). Each method mirrors the operation of
/// the same name in `patina_core::loader::ops`.
pub trait VersionOps: Send + Sync + 'static {
    fn install_loader(
        &self,
        id: String,
        kind: LoaderKind,
        pick: VersionPick,
        replace_existing: bool,
        progress: ProgressSink,
    ) -> BoxFuture<'static, Result<OpOutcome, LoaderError>>;

    fn set_mc_version(
        &self,
        id: String,
        mc: String,
        action: Option<LoaderAction>,
        progress: ProgressSink,
    ) -> BoxFuture<'static, Result<OpOutcome, LoaderError>>;

    fn reinstall_loader(
        &self,
        id: String,
        progress: ProgressSink,
    ) -> BoxFuture<'static, Result<OpOutcome, LoaderError>>;

    /// Blocking in the core; runs on the blocking pool here.
    fn remove_loader(&self, id: String) -> BoxFuture<'static, Result<OpOutcome, LoaderError>>;
}

/// The real operations: on the launcher current when one starts.
pub(super) struct CoreVersionOps {
    pub(super) launcher: Arc<StdRwLock<Arc<Launcher>>>,
    pub(super) store: InstanceStore,
}

impl VersionOps for CoreVersionOps {
    fn install_loader(
        &self,
        id: String,
        kind: LoaderKind,
        pick: VersionPick,
        replace_existing: bool,
        progress: ProgressSink,
    ) -> BoxFuture<'static, Result<OpOutcome, LoaderError>> {
        let (launcher, store) = (current(&self.launcher), self.store.clone());
        async move {
            install_loader(
                &launcher,
                &store,
                &id,
                kind,
                pick,
                replace_existing,
                progress,
            )
            .await
        }
        .boxed()
    }

    fn set_mc_version(
        &self,
        id: String,
        mc: String,
        action: Option<LoaderAction>,
        progress: ProgressSink,
    ) -> BoxFuture<'static, Result<OpOutcome, LoaderError>> {
        let (launcher, store) = (current(&self.launcher), self.store.clone());
        async move { set_mc_version(&launcher, &store, &id, &mc, action, progress).await }.boxed()
    }

    fn reinstall_loader(
        &self,
        id: String,
        progress: ProgressSink,
    ) -> BoxFuture<'static, Result<OpOutcome, LoaderError>> {
        let (launcher, store) = (current(&self.launcher), self.store.clone());
        async move { reinstall_loader(&launcher, &store, &id, progress).await }.boxed()
    }

    fn remove_loader(&self, id: String) -> BoxFuture<'static, Result<OpOutcome, LoaderError>> {
        let store = self.store.clone();
        async move {
            match tokio::task::spawn_blocking(move || remove_loader(&store, &id)).await {
                Ok(result) => result,
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                // The runtime is shutting down; so is the caller.
                Err(_) => std::future::pending().await,
            }
        }
        .boxed()
    }
}
