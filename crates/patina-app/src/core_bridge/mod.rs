//! The bridge between the UI and patina-core.
//!
//! [`Core`] is a gpui global holding the core's services and a handle to the
//! app's tokio runtime. Views never call the core on the gpui main thread:
//!
//! - async core work goes through [`run`], blocking core calls (the stores,
//!   scans, `reattach`, ...) through [`run_blocking`]; both return a gpui
//!   [`Task`](gpui_kit::Task) with the result (or [`BridgeError`]), and
//!   dropping that task cancels the work if it has not started (or, for
//!   async work, at its next await);
//! - the config and the accounts are reached through [`Core::read_config`],
//!   [`Core::update_config`] and [`Core::with_accounts`]; the accounts change
//!   only through [`Core::update_accounts`] and [`Core::reload_accounts`],
//!   which then call [`AccountsChanged::bump`] so views re-read them;
//!   config changes and speed tests signal [`ConfigChanged`] likewise;
//! - a stream of progress reaches a view through [`forward_progress`], at most
//!   about 30 times a second;
//! - launches are prepared through `Core.launch_backend` ([`LaunchBackend`]),
//!   which UI tests replace with fake games;
//! - loader and version changes go through `Core.version_ops`
//!   ([`VersionOps`]), likewise injectable;
//! - every result is checked against the request that is current when it
//!   arrives ([`Revision`], or the id of the instance it is for) and dropped
//!   if it is stale.

mod accounts_changed;
mod config_changed;
mod launch;
mod service;
pub(crate) use service::back_up;
mod speed;
mod task;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
mod version_ops;

pub use accounts_changed::{AccountsChanged, AccountsEpoch};
pub use config_changed::{ConfigChanged, ConfigEpoch};
pub use launch::LaunchBackend;
pub use service::{
    Catalog, ConfigSaveError, ConfigSaved, Core, DetectJava, InitError, LoadError, LoadedAccounts,
    LoadedConfig, ProbeJava, SpeedTested, config_reason,
};
pub use speed::{MirrorProbe, SpeedTest, SpeedTestResult, latency_text, mirror_name};
pub use task::{
    BridgeError, PROGRESS_MIN_GAP, Revision, RevisionId, forward_progress, run, run_blocking,
};
pub use version_ops::VersionOps;
