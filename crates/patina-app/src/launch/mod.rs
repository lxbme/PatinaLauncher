//! Long-running operations on an instance: the per-instance runner
//! ([`op_runner`]) and the view model of their phased progress
//! ([`progress`]). Plan 4b tasks 9 (add instance), 10 (launch) and 12 (the
//! versions tab) run their core work through [`InstancesModel::start_op`].
//!
//! [`InstancesModel::start_op`]: crate::instances::InstancesModel::start_op

pub mod op_runner;
pub mod progress;

pub use op_runner::{OpError, OpKind, OpState, RetryAction, busy_message};
pub use progress::{ProgressTracker, ProgressView, view_of};
