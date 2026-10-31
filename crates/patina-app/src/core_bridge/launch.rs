//! [`LaunchBackend`]: how the app gets a game ready to start. Injectable so
//! UI tests launch fake games instead of Minecraft.

use std::sync::{Arc, RwLock as StdRwLock};

use futures::FutureExt;
use futures::future::BoxFuture;
use patina_core::accounts::{AccountError, AccountStore};
use patina_core::instance::Instance;
use patina_core::launch::{GameSession, LaunchAccount, PendingLaunch, SessionError};
use patina_core::prepare::Launcher;
use patina_core::progress::ProgressSink;
use tokio::sync::RwLock;

use super::service::{LoadedConfig, current};

/// Prepares launches. The app reaches it through `Core.launch_backend`; the
/// futures do file and network I/O, so poll them on the runtime
/// ([`run`](super::run)).
pub trait LaunchBackend: Send + Sync + 'static {
    /// The launch identity of account `id` in `accounts`. Called on the
    /// blocking pool (the account store does file I/O).
    fn launch_account(
        &self,
        accounts: &AccountStore,
        id: &str,
    ) -> Result<LaunchAccount, AccountError> {
        accounts.launch_account(id)
    }

    /// Takes the instance's locks and prepares `inst` for `account` without
    /// starting it ([`GameSession::prepare_launch`]); dropping the result
    /// cancels the launch.
    fn prepare(
        &self,
        inst: Instance,
        account: LaunchAccount,
        progress: ProgressSink,
    ) -> BoxFuture<'static, Result<PendingLaunch, SessionError>>;
}

/// The real backend: the launcher current when a launch is prepared, and
/// the config as loaded.
pub(super) struct CoreLaunchBackend {
    pub(super) launcher: Arc<StdRwLock<Arc<Launcher>>>,
    pub(super) config: Arc<RwLock<LoadedConfig>>,
}

impl LaunchBackend for CoreLaunchBackend {
    fn prepare(
        &self,
        inst: Instance,
        account: LaunchAccount,
        progress: ProgressSink,
    ) -> BoxFuture<'static, Result<PendingLaunch, SessionError>> {
        let (launcher, config) = (self.launcher.clone(), self.config.clone());
        async move {
            let global = config.read().await.config.clone();
            // Read after the config: once it is loaded the launcher follows
            // its download source.
            let launcher = current(&launcher);
            GameSession::prepare_launch(&launcher, &inst, &global, &account, progress).await
        }
        .boxed()
    }
}
