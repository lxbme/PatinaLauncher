//! The per-instance operation runner: state types and the helpers around
//! [`InstancesModel::start_op`](crate::instances::InstancesModel::start_op).

use std::rc::Rc;

use patina_core::launch::SessionError;
use patina_core::loader::LoaderError;
use patina_core::locks::BusyKind;
use patina_core::prepare::PrepareError;
use rust_i18n::t;

use super::progress::ProgressView;
use crate::instances::InstancesModel;

/// What an operation does to an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    InstallLoader,
    ChangeVersion,
    RemoveLoader,
    Reinstall,
    Launch,
    /// Force-stopping a running game.
    Stop,
}

/// A fix the failed header offers in place of retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpFix {
    /// No usable account: go to the accounts page.
    AddAccount,
    /// The loader's files are gone: reinstall it, then launch again.
    ReinstallLoader,
}

impl OpFix {
    /// The fix button's label.
    pub fn label(self) -> String {
        match self {
            OpFix::AddAccount => t!("launch.add_account"),
            OpFix::ReinstallLoader => t!("launch.reinstall"),
        }
        .to_string()
    }
}

/// Starts the same operation again (the failed header's retry button).
pub type RetryAction =
    Rc<dyn Fn(&mut InstancesModel, &mut gpui_kit::Context<InstancesModel>) + 'static>;

/// The operation of one instance, as the header shows it. There is at most
/// one per instance; a finished operation leaves nothing behind.
#[derive(Clone)]
pub enum OpState {
    Running {
        kind: OpKind,
        /// `None` until the first progress tick.
        progress: Option<ProgressView>,
    },
    Failed {
        kind: OpKind,
        /// Localized, shown as is.
        message: String,
        /// Re-runs the operation, when it can be.
        retry: Option<RetryAction>,
        /// Offered instead of retry when it fixes the cause.
        fix: Option<OpFix>,
    },
}

impl OpState {
    pub fn kind(&self) -> OpKind {
        match self {
            OpState::Running { kind, .. } | OpState::Failed { kind, .. } => *kind,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, OpState::Running { .. })
    }
}

impl std::fmt::Debug for OpState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpState::Running { kind, progress } => f
                .debug_struct("Running")
                .field("kind", kind)
                .field("progress", progress)
                .finish(),
            OpState::Failed {
                kind,
                message,
                retry,
                fix,
            } => f
                .debug_struct("Failed")
                .field("kind", kind)
                .field("message", message)
                .field("retry", &retry.is_some())
                .field("fix", fix)
                .finish(),
        }
    }
}

/// Why an operation's core work failed.
#[derive(Debug)]
pub enum OpError {
    Loader(LoaderError),
    Session(SessionError),
    /// Already localized.
    Message(String),
}

impl From<LoaderError> for OpError {
    fn from(e: LoaderError) -> Self {
        OpError::Loader(e)
    }
}

impl From<SessionError> for OpError {
    fn from(e: SessionError) -> Self {
        OpError::Session(e)
    }
}

impl From<String> for OpError {
    fn from(e: String) -> Self {
        OpError::Message(e)
    }
}

/// The localized text of a busy lock: the instance by its display name
/// (`name_of` maps an id to it), the others by their id.
pub fn busy_message(what: &BusyKind, name_of: impl Fn(&str) -> String) -> String {
    match what {
        BusyKind::Instance(id) => t!("busy.instance", name = name_of(id)).to_string(),
        BusyKind::Version(id) => t!("busy.version", id = id).to_string(),
        BusyKind::Processors(mc) => t!("busy.processors", mc = mc).to_string(),
        BusyKind::Running(id) => t!("busy.running", name = name_of(id)).to_string(),
    }
}

impl OpError {
    /// The message to show for this error: busy locks in the app's words,
    /// everything else as the core words it.
    pub fn message(&self, model: &InstancesModel) -> String {
        let name_of = |id: &str| model.display_name(id).unwrap_or_else(|| id.to_string());
        match self {
            OpError::Loader(LoaderError::Busy(busy)) => busy_message(&busy.what, name_of),
            OpError::Session(SessionError::Busy(busy)) => busy_message(&busy.what, name_of),
            OpError::Loader(e) => e.to_string(),
            OpError::Session(e) => e.to_string(),
            OpError::Message(m) => m.clone(),
        }
    }

    /// The fix to offer for this error, if one applies.
    pub fn fix(&self) -> Option<OpFix> {
        match self {
            OpError::Session(SessionError::Prepare(PrepareError::LoaderNeedsReinstall {
                ..
            })) => Some(OpFix::ReinstallLoader),
            _ => None,
        }
    }

    /// Whether the game may already run elsewhere (its instance is held or
    /// it runs), so the run state is worth checking.
    pub fn may_be_running(&self) -> bool {
        matches!(
            self,
            OpError::Session(SessionError::Busy(_) | SessionError::AlreadyRunning(_))
        )
    }
}
