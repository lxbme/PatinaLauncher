//! The per-instance operation runner: state types and the helpers around
//! [`InstancesModel::start_op`](crate::instances::InstancesModel::start_op).

use std::rc::Rc;

use patina_core::instance::InstanceError;
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

/// The message for an instance store error: a busy lock (an operation
/// holds the instance, its game runs) in the app's words with the instance
/// by `display_name` (its id when that has none), everything else as the
/// core words it.
pub fn instance_error_message(
    error: &InstanceError,
    display_name: impl Fn(&str) -> Option<String>,
) -> String {
    match error {
        InstanceError::Busy(busy) => busy_message(&busy.what, |id| {
            display_name(id).unwrap_or_else(|| id.to_string())
        }),
        error => error.to_string(),
    }
}

/// The reason an instance store error gives, for a text that names the
/// instance already ("Couldn’t delete “X”: …"): a busy instance or its
/// running game without the name (the versions tab's reasons), everything
/// else as [`instance_error_message`] words it.
pub fn instance_error_reason(error: &InstanceError) -> String {
    match error {
        InstanceError::Busy(busy) => match &busy.what {
            BusyKind::Instance(_) => t!("versions.busy_operation").to_string(),
            BusyKind::Running(_) => t!("versions.busy_running").to_string(),
            _ => instance_error_message(error, |_| None),
        },
        error => error.to_string(),
    }
}

impl OpError {
    /// The message to show for this error: busy locks in the app's words,
    /// everything else as the core words it.
    /// `display_name` maps an instance id to its display name, when listed.
    pub fn message(&self, display_name: impl Fn(&str) -> Option<String>) -> String {
        let name_of = |id: &str| display_name(id).unwrap_or_else(|| id.to_string());
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

#[cfg(test)]
mod tests {
    use patina_core::locks::Busy;

    use super::*;

    #[test]
    fn instance_errors_name_busy_instances_by_display_name() {
        let _locale = crate::i18n::locale_lock();
        crate::i18n::set_language(crate::i18n::Language::En);
        let name = |id: &str| (id == "pack-1").then(|| "My Pack".to_string());
        let busy = |what| InstanceError::Busy(Busy { what });
        assert_eq!(
            instance_error_message(&busy(BusyKind::Running("pack-1".into())), name),
            "“My Pack” is running; stop the game first"
        );
        assert_eq!(
            instance_error_message(&busy(BusyKind::Instance("pack-1".into())), name),
            "“My Pack” is busy with another operation"
        );
        // Without a display name (a broken entry) the id stands in.
        assert_eq!(
            instance_error_message(&busy(BusyKind::Instance("Broken".into())), name),
            "“Broken” is busy with another operation"
        );
        let other = InstanceError::NotFound("gone".into());
        assert_eq!(instance_error_message(&other, name), other.to_string());

        // As a reason after the name: the name is not repeated.
        assert_eq!(
            instance_error_reason(&busy(BusyKind::Running("pack-1".into()))),
            "The game is running"
        );
        assert_eq!(
            instance_error_reason(&busy(BusyKind::Instance("pack-1".into()))),
            "An operation is in progress"
        );
        assert_eq!(
            instance_error_reason(&busy(BusyKind::Version("1.21.1".into()))),
            "Version 1.21.1 is in use by another operation"
        );
        assert_eq!(instance_error_reason(&other), other.to_string());
    }
}
