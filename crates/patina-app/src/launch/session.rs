//! The run state of an instance's game and the pieces of launching that do
//! not depend on the model: account lookup, reattaching, stopping a game
//! another process holds. [`InstanceActivity`] owns the state and its
//! transitions (`launch`, `force_stop`, ...).
//!
//! [`InstanceActivity`]: crate::instances::InstanceActivity

use std::time::Duration;

use patina_core::accounts::AccountError;
use patina_core::instance::Instance;
use patina_core::launch::{GameSession, KillHandle, LaunchAccount, Reattach, SessionExit};
use rust_i18n::t;

use crate::core_bridge::{LaunchBackend, LoadedAccounts};

/// How often a game running in another Patina process is checked: whether
/// it still runs, or its session became free to take over.
pub const ELSEWHERE_POLL: Duration = Duration::from_secs(2);

/// What an instance's game is doing. No entry means idle.
#[derive(Clone)]
pub enum RunState {
    /// Being launched: the account lookup, preparing, the pre-launch
    /// warnings, starting the process.
    Starting,
    /// The game runs and this app holds its session.
    Running {
        pid: u32,
        /// Stops it (identity-checked; signals only this game).
        killer: KillHandle,
        /// Picked up from an earlier launcher session rather than started
        /// by this one.
        reattached: bool,
    },
    /// The game runs, but another Patina process holds its session. It can
    /// be stopped ([`KillHandle::for_running`]); its log is that process's.
    HeldElsewhere { pid: u32 },
    /// The last run ended abnormally; kept until dismissed or relaunched.
    /// A normal exit leaves no state behind.
    Exited(SessionExit),
}

impl RunState {
    /// Whether a game process is up (here or in another Patina process).
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            RunState::Running { .. } | RunState::HeldElsewhere { .. }
        )
    }

    /// Whether the instance must not be launched or changed now.
    pub fn is_busy(&self) -> bool {
        !matches!(self, RunState::Exited(_))
    }
}

impl std::fmt::Debug for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunState::Starting => f.write_str("Starting"),
            RunState::Running {
                pid, reattached, ..
            } => f
                .debug_struct("Running")
                .field("pid", pid)
                .field("reattached", reattached)
                .finish_non_exhaustive(),
            RunState::HeldElsewhere { pid } => {
                f.debug_struct("HeldElsewhere").field("pid", pid).finish()
            }
            RunState::Exited(exit) => f.debug_tuple("Exited").field(exit).finish(),
        }
    }
}

/// Requests from the launch flow that need a window or the shell; emitted
/// by the instances model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchEvent {
    /// The prepared launch of `id` has warnings: ask before starting it
    /// (see `prelaunch_dialog`).
    ConfirmWarnings { id: String },
    /// A game of this app was started (not reattached) and its play time
    /// recorded: show `warnings` (from starting it, localized) and apply
    /// the after-launch window setting, except quitting when the running
    /// record was not saved (a restart could not find the game again).
    Spawned {
        id: String,
        warnings: Vec<String>,
        record_saved: bool,
    },
    /// Show the accounts page (no usable account to launch with).
    ShowAccounts,
}

/// Why there is no account to launch with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountIssue {
    NoAccount,
    MsaUnavailable,
    /// Already localized.
    Failed(String),
}

impl AccountIssue {
    pub fn message(&self) -> String {
        match self {
            AccountIssue::NoAccount => t!("launch.no_account").to_string(),
            AccountIssue::MsaUnavailable => t!("launch.msa_unavailable").to_string(),
            AccountIssue::Failed(m) => m.clone(),
        }
    }

    /// Whether adding an account on the accounts page fixes it.
    pub fn wants_account(&self) -> bool {
        matches!(self, AccountIssue::NoAccount | AccountIssue::MsaUnavailable)
    }
}

/// The launch identity of the current account. Blocking (the account store
/// does file I/O): run it on the blocking pool.
pub fn resolve_account(
    backend: &dyn LaunchBackend,
    accounts: &LoadedAccounts,
) -> Result<LaunchAccount, AccountIssue> {
    let store = (accounts.as_ref()).map_err(|e| AccountIssue::Failed(e.to_string()))?;
    let current = store.current().ok_or(AccountIssue::NoAccount)?;
    backend
        .launch_account(store, &current.id)
        .map_err(|e| match e {
            AccountError::MsaUnavailable => AccountIssue::MsaUnavailable,
            e => AccountIssue::Failed(e.to_string()),
        })
}

/// Looks for the games of `instances` that still run ([`GameSession::reattach`]
/// each). Blocking, and `reattach` needs a tokio context: run it on the
/// runtime's blocking pool.
pub fn reattach_all(instances: Vec<Instance>) -> Vec<(String, Reattach)> {
    instances
        .into_iter()
        .map(|inst| {
            let found = GameSession::reattach(&inst);
            (inst.id, found)
        })
        .filter(|(_, found)| !matches!(found, Reattach::NotRunning))
        .collect()
}

/// Stops the game of `inst` that another Patina process holds, through its
/// running record. `Ok` also when it is already gone. Run it on the runtime.
pub async fn stop_elsewhere(inst: Instance) -> std::io::Result<()> {
    let handle = tokio::task::spawn_blocking(move || KillHandle::for_running(&inst))
        .await
        .map_err(std::io::Error::other)?;
    match handle {
        Some(handle) => handle.kill().await,
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::future::BoxFuture;
    use patina_core::accounts::AccountStore;
    use patina_core::launch::{PendingLaunch, SessionError};
    use patina_core::paths::DataDirs;
    use patina_core::progress::ProgressSink;

    use super::*;
    use crate::core_bridge::LoadError;

    /// Resolves accounts as the real backend does; never prepares.
    struct AccountsOnly;

    impl LaunchBackend for AccountsOnly {
        fn prepare(
            &self,
            _: Instance,
            _: LaunchAccount,
            _: ProgressSink,
        ) -> BoxFuture<'static, Result<PendingLaunch, SessionError>> {
            unreachable!("these tests only look up accounts")
        }
    }

    fn load(data: &DataDirs) -> LoadedAccounts {
        AccountStore::load(data).map_err(|e| LoadError::Failed(Arc::new(e)))
    }

    #[test]
    fn account_issues_name_their_way_out() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDirs::new(dir.path());
        let none = resolve_account(&AccountsOnly, &load(&data)).unwrap_err();
        assert_eq!(none, AccountIssue::NoAccount);
        assert!(none.wants_account());

        std::fs::write(
            dir.path().join("accounts.toml"),
            "current = \"msa-1\"\n\n[[accounts]]\nid = \"msa-1\"\nkind = \"msa\"\n\
             username = \"Steve\"\nuuid = \"00000000-0000-0000-0000-000000000001\"\n",
        )
        .unwrap();
        let msa = resolve_account(&AccountsOnly, &load(&data)).unwrap_err();
        assert_eq!(msa, AccountIssue::MsaUnavailable);
        assert!(msa.wants_account());

        // An account store that is not there cannot be fixed by adding one.
        let failed = resolve_account(&AccountsOnly, &Err(LoadError::NotLoaded)).unwrap_err();
        assert!(matches!(failed, AccountIssue::Failed(_)));
        assert!(!failed.wants_account());
    }

    #[test]
    fn only_a_crash_notice_leaves_the_instance_free() {
        let exited = RunState::Exited(SessionExit {
            code: Some(1),
            crashed: true,
            saved_log: None,
        });
        assert!(!exited.is_busy() && !exited.is_running());
        let elsewhere = RunState::HeldElsewhere { pid: 0 };
        assert!(elsewhere.is_busy() && elsewhere.is_running());
        assert!(RunState::Starting.is_busy() && !RunState::Starting.is_running());
    }
}
