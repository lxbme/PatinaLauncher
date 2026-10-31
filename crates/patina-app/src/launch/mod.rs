//! Long-running operations on an instance: the per-instance runner
//! ([`op_runner`]) and the view model of their phased progress
//! ([`progress`]); launching and tracking games ([`session`], with its
//! dialogs [`prelaunch_dialog`] and [`stop_dialog`]) and their log
//! ([`log_view`]). Plan 4b tasks 9 (add instance), 10 (launch) and 12 (the
//! versions tab) run their core work through [`InstancesModel::start_op`].
//!
//! [`InstancesModel::start_op`]: crate::instances::InstancesModel::start_op

pub mod log_view;
pub mod op_runner;
pub mod prelaunch_dialog;
pub mod progress;
pub mod session;
pub mod stop_dialog;

pub use log_view::{
    LOG_VIEW_MAX_BYTES, LOG_VIEW_MAX_LINES, LevelFilter, LogBottom, LogBuffer, LogPageDown,
    LogPageUp, LogTop, LogView, forward_logs, visible_rows,
};
pub use op_runner::{
    OpError, OpFix, OpKind, OpState, RetryAction, busy_message, instance_error_message,
    instance_error_reason,
};
pub use progress::{ProgressTracker, ProgressView, view_of};
pub use session::{AccountIssue, LaunchEvent, RunState};
