//! [`InstanceActivity`]: what the instances are doing — their operations
//! and the progress of those, the games being launched, followed and
//! stopped, and the games' logs. An entity of its own beside
//! [`InstancesModel`], so the ~30 Hz progress ticks of an operation notify
//! only the views that show activity (the detail header, the tabs), never
//! the observers of the list and the selection.
//!
//! [`InstancesModel`]: super::InstancesModel

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use futures::future::BoxFuture;
use gpui_kit::{AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use patina_core::instance::ListOutcome;
use patina_core::launch::PendingLaunch;
use patina_core::progress::{ProgressSink, TaskProgress};

use super::model::{Entry, display_name_in, find_entry};
use crate::core_bridge::{forward_progress, run};
use crate::launch::{
    LaunchEvent, LogBuffer, OpError, OpFix, OpKind, OpState, ProgressTracker, RetryAction, RunState,
};

mod launch;

/// Identifies one operation when it finishes.
struct OpMeta {
    id: String,
    token: u64,
    kind: OpKind,
    retry: Option<RetryAction>,
}

/// An operation of one instance: what the header shows, plus the tasks that
/// keep it going (dropping the entry cancels them).
struct OpEntry {
    state: OpState,
    /// Tells this operation from an earlier one of the same instance.
    token: u64,
    tracker: ProgressTracker,
    _forward: Task<()>,
    _job: Task<()>,
}

/// A launch in progress (the instance's state is [`RunState::Starting`]):
/// the account lookup, then the prepare and spawn operations. Dropping it
/// cancels what is left, including a prepared launch waiting for the
/// warnings to be confirmed (which releases its locks).
struct LaunchFlow {
    token: u64,
    /// Prepared, waiting for the pre-launch warnings to be answered.
    pending: Option<PendingLaunch>,
    _task: Task<()>,
}

/// Follows a game: its session's `wait`, or the poll of a game another
/// process holds. Dropping it stops following (a session is then dropped,
/// which keeps its running record: the game goes on).
struct Track {
    token: u64,
    _task: Task<()>,
}

/// What happens after an operation fails or its instance is gone (`None`).
type OnFail =
    Box<dyn FnOnce(&mut InstanceActivity, Option<&OpError>, &mut Context<InstanceActivity>)>;

/// What the activity asks of the instances model; the model handles these
/// (and re-emits the launch events and [`InstancesChanged`] for the shell).
///
/// [`InstancesChanged`]: super::InstancesChanged
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityEvent {
    /// The instances on disk may have changed: read the list again.
    Refresh,
    /// One of this app's operations changed an instance on disk.
    Changed,
    /// A request of the launch flow for a window or the shell.
    Launch(LaunchEvent),
}

/// The activity of the instances (an entity, owned by the instances
/// model). It works on the list as the model last loaded it
/// ([`InstanceActivity::list_loaded`]).
pub struct InstanceActivity {
    /// The list as last loaded, shared with the model.
    list: Rc<ListOutcome>,
    /// Whether a list load has landed (successfully or not).
    loaded: bool,
    running: HashMap<String, RunState>,
    /// Launches in progress, by instance id.
    flows: HashMap<String, LaunchFlow>,
    /// Games being followed, by instance id.
    tracks: HashMap<String, Track>,
    /// The log forwarding of each instance's latest session.
    log_forwards: HashMap<String, Task<()>>,
    /// Force stops in flight (until the game is gone or the stop failed).
    stopping: HashMap<String, Task<()>>,
    /// Whether the games of an earlier launcher session were looked for.
    reattach_started: bool,
    reattach_task: Task<()>,
    /// Operations by instance id; see [`InstanceActivity::start_op`].
    ops: HashMap<String, OpEntry>,
    next_op: u64,
    /// The game output of each instance that ran (or is running) in this
    /// session. Entities of their own, so a log batch notifies only the log
    /// view.
    logs: HashMap<String, Entity<LogBuffer>>,
    /// How many of this app's operations (other than launches) changed each
    /// instance: the overview walks the disk again after one.
    op_changes: HashMap<String, u64>,
    /// The crash log saved after the last run of an instance, if it crashed.
    crash_logs: HashMap<String, PathBuf>,
}

impl EventEmitter<ActivityEvent> for InstanceActivity {}

/// Observes `activity` for what `read` takes from it: notifies the observer
/// only when that changed, so a view showing (say) whether an instance is
/// busy is not re-rendered by every progress tick.
pub(crate) fn observe_activity<V: 'static, T: PartialEq + 'static>(
    activity: &Entity<InstanceActivity>,
    cx: &mut Context<V>,
    read: impl Fn(&InstanceActivity) -> T + 'static,
) -> Subscription {
    let mut last = read(activity.read(cx));
    cx.observe(activity, move |_, activity, cx| {
        let now = read(activity.read(cx));
        if now != last {
            last = now;
            cx.notify();
        }
    })
}

impl Default for InstanceActivity {
    fn default() -> Self {
        Self::new()
    }
}

impl InstanceActivity {
    pub fn new() -> Self {
        Self {
            list: Rc::default(),
            loaded: false,
            running: HashMap::new(),
            flows: HashMap::new(),
            tracks: HashMap::new(),
            log_forwards: HashMap::new(),
            stopping: HashMap::new(),
            reattach_started: false,
            reattach_task: Task::ready(()),
            ops: HashMap::new(),
            next_op: 0,
            logs: HashMap::new(),
            op_changes: HashMap::new(),
            crash_logs: HashMap::new(),
        }
    }

    // --- the list, as the model loaded it ----------------------------------

    fn entry(&self, id: &str) -> Option<Entry<'_>> {
        find_entry(&self.list, id)
    }

    /// The display name of healthy instance `id`.
    pub fn display_name(&self, id: &str) -> Option<String> {
        display_name_in(&self.list, id)
    }

    /// The ids whose game is known (a state, or followed): a refresh looks
    /// for games of the others only.
    pub(super) fn known_ids(&self) -> impl Iterator<Item = &String> {
        self.running.keys().chain(self.tracks.keys())
    }

    /// Whether the startup look for running games has begun, so a refresh
    /// may look for games of idle instances too.
    pub(super) fn reattach_started(&self) -> bool {
        self.reattach_started
    }

    /// Takes the list the model loaded: forgets what belonged to instances
    /// that are gone, looks for the games of an earlier launcher session
    /// after the first load, and follows `found` (idle instances whose game
    /// a refresh found running).
    pub(super) fn list_loaded(
        &mut self,
        list: Rc<ListOutcome>,
        found: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let before = self.sizes();
        // A failure left on the header of an instance that is gone has
        // nothing to hang on to. (Running ones end by themselves.)
        self.ops
            .retain(|id, e| e.state.is_running() || find_entry(&list, id).is_some());
        // Nor does the log of an instance that is gone.
        let running = &self.running;
        let kept = |id: &String| running.contains_key(id) || find_entry(&list, id).is_some();
        self.logs.retain(|id, _| kept(id));
        self.op_changes.retain(|id, _| kept(id));
        self.crash_logs.retain(|id, _| kept(id));
        // A crash notice has nothing to hang on to either; a game that runs
        // stays (it ends by itself).
        self.running.retain(|id, state| {
            !matches!(state, RunState::Exited(_)) || find_entry(&list, id).is_some()
        });
        self.list = list;
        self.loaded = true;
        if self.sizes() != before {
            cx.notify();
        }
        if !std::mem::replace(&mut self.reattach_started, true) {
            self.reattach_on_startup(cx);
        }
        for id in found {
            let idle = !self.running.contains_key(&id) && !self.tracks.contains_key(&id);
            if idle {
                self.follow_game(&id, false, cx);
            }
        }
    }

    /// A list load failed: the previous list stands.
    pub(super) fn list_failed(&mut self) {
        self.loaded = true;
    }

    fn sizes(&self) -> [usize; 5] {
        [
            self.ops.len(),
            self.logs.len(),
            self.op_changes.len(),
            self.crash_logs.len(),
            self.running.len(),
        ]
    }

    // --- run states and logs -----------------------------------------------

    /// What the game of instance `id` is doing; `None` when idle.
    pub fn run_state(&self, id: &str) -> Option<&RunState> {
        self.running.get(id)
    }

    /// Whether a game of instance `id` runs (here or in another Patina
    /// process).
    pub fn is_running(&self, id: &str) -> bool {
        self.running.get(id).is_some_and(RunState::is_running)
    }

    /// The instances whose game runs, here or elsewhere.
    pub fn running_ids(&self) -> impl Iterator<Item = &str> {
        (self.running.iter())
            .filter(|(_, state)| state.is_running())
            .map(|(id, _)| id.as_str())
    }

    /// Whether instance `id` must not be launched or changed now: an
    /// operation runs on it, or its game is starting or running.
    pub fn is_busy(&self, id: &str) -> bool {
        self.is_op_running(id) || self.running.get(id).is_some_and(RunState::is_busy)
    }

    /// Why instance `id` takes no change now (its versions, name or
    /// deletion), or `None`: an operation is in flight, or its game runs.
    pub fn busy_reason(&self, id: &str) -> Option<String> {
        if self.is_op_running(id) {
            Some(rust_i18n::t!("versions.busy_operation").to_string())
        } else if self.is_busy(id) {
            Some(rust_i18n::t!("versions.busy_running").to_string())
        } else {
            None
        }
    }

    /// Whether this app has work under way that quitting would cut short:
    /// an operation (install, version change, launch…), a launch flow or a
    /// force stop, on any instance. Games that run are not work: they go
    /// on without the launcher.
    pub fn has_work_in_flight(&self) -> bool {
        self.ops.values().any(|e| e.state.is_running())
            || !self.flows.is_empty()
            || !self.stopping.is_empty()
    }

    /// Whether a game of any instance is starting or running (here or in
    /// another Patina process).
    pub fn any_game_busy(&self) -> bool {
        self.running.values().any(RunState::is_busy)
    }

    /// Whether this app follows a game of `id`: its session, or a look
    /// through its running record (in flight or polling).
    pub fn is_following(&self, id: &str) -> bool {
        self.tracks.contains_key(id)
    }

    /// Whether a force stop of `id`'s game is in flight.
    pub fn is_stopping(&self, id: &str) -> bool {
        self.stopping.contains_key(id)
    }

    /// Sets what instance `id` is doing (`None`: idle) without following a
    /// game; for tests and previews. The launch flow sets it itself.
    pub fn set_run_state(&mut self, id: &str, state: Option<RunState>, cx: &mut Context<Self>) {
        match state {
            Some(state) => self.running.insert(id.to_string(), state),
            None => self.running.remove(id),
        };
        cx.notify();
    }

    /// The log buffer of instance `id`, created empty on first use. It keeps
    /// the last run's output until [`InstanceActivity::begin_log`].
    pub fn log_buffer(&mut self, id: &str, cx: &mut Context<Self>) -> Entity<LogBuffer> {
        if let Some(buffer) = self.logs.get(id) {
            return buffer.clone();
        }
        let buffer = cx.new(|_| LogBuffer::new());
        self.logs.insert(id.to_string(), buffer.clone());
        buffer
    }

    /// Whether instance `id` has a log buffer.
    pub fn has_log(&self, id: &str) -> bool {
        self.logs.contains_key(id)
    }

    /// Starts a new run's log: empties the buffer and forgets the previous
    /// crash log. Returns the buffer to forward the session's lines into.
    pub fn begin_log(&mut self, id: &str, cx: &mut Context<Self>) -> Entity<LogBuffer> {
        let buffer = self.log_buffer(id, cx);
        buffer.update(cx, |b, cx| {
            b.clear();
            cx.notify();
        });
        self.set_crash_log(id, None, cx);
        buffer
    }

    /// The crash log saved after the last run of `id`.
    pub fn crash_log(&self, id: &str) -> Option<&Path> {
        self.crash_logs.get(id).map(PathBuf::as_path)
    }

    /// Records (or clears) the saved crash log of `id`.
    pub fn set_crash_log(&mut self, id: &str, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let changed = match path {
            Some(path) => self.crash_logs.insert(id.to_string(), path.clone()) != Some(path),
            None => self.crash_logs.remove(id).is_some(),
        };
        if changed {
            cx.notify();
        }
    }

    // --- operations --------------------------------------------------------

    /// The operation of instance `id`: running, or failed and not dismissed.
    pub fn op(&self, id: &str) -> Option<&OpState> {
        self.ops.get(id).map(|e| &e.state)
    }

    /// The retry action of `id`'s failed operation.
    pub(super) fn retry_of(&self, id: &str) -> Option<RetryAction> {
        match self.op(id) {
            Some(OpState::Failed { retry, .. }) => retry.clone(),
            _ => None,
        }
    }

    /// How many of this app's operations other than launches have finished
    /// on instance `id` (a counter to compare, not a total of all time).
    pub fn op_changes(&self, id: &str) -> u64 {
        self.op_changes.get(id).copied().unwrap_or(0)
    }

    /// Whether an operation of instance `id` is running.
    pub fn is_op_running(&self, id: &str) -> bool {
        self.op(id).is_some_and(OpState::is_running)
    }

    /// Runs `f` as operation `kind` of instance `id`; see
    /// [`InstancesModel::start_op`](super::InstancesModel::start_op), which
    /// is how views start one. `on_done` runs on the activity.
    pub fn start_op<T: Send + 'static>(
        &mut self,
        id: &str,
        kind: OpKind,
        f: impl FnOnce(ProgressSink) -> BoxFuture<'static, Result<T, OpError>>,
        on_done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
        retry: Option<RetryAction>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.is_busy(id) {
            return false;
        }
        self.start_op_with(id, kind, f, on_done, retry, Box::new(|_, _, _| {}), cx)
    }

    /// [`InstanceActivity::start_op`] that also runs `on_fail` when the work
    /// fails (before the failure lands on the header) or its result is
    /// dropped because the instance is gone (with `None`).
    #[allow(clippy::too_many_arguments)]
    fn start_op_with<T: Send + 'static>(
        &mut self,
        id: &str,
        kind: OpKind,
        f: impl FnOnce(ProgressSink) -> BoxFuture<'static, Result<T, OpError>>,
        on_done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
        retry: Option<RetryAction>,
        on_fail: OnFail,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.is_op_running(id) {
            return false;
        }
        self.next_op += 1;
        let token = self.next_op;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<TaskProgress>();
        let work = run(cx, f(Some(tx)));
        let key = id.to_string();
        let this = cx.weak_entity();
        let _forward = forward_progress(cx, this, rx, {
            let key = key.clone();
            move |this, tick, cx| this.apply_progress(&key, token, &tick, cx)
        });
        let _job = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let result = match work.await {
                    Ok(result) => result,
                    Err(error) => Err(OpError::Message(error.to_string())),
                };
                let meta = OpMeta {
                    id: key,
                    token,
                    kind,
                    retry,
                };
                this.update(cx, |this, cx| {
                    this.finish_op(meta, result, on_done, on_fail, cx)
                })
                .ok();
            }
        });
        self.ops.insert(
            key,
            OpEntry {
                state: OpState::Running {
                    kind,
                    progress: None,
                },
                token,
                tracker: ProgressTracker::default(),
                _forward,
                _job,
            },
        );
        cx.notify();
        true
    }

    fn apply_progress(
        &mut self,
        id: &str,
        token: u64,
        tick: &TaskProgress,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.ops.get_mut(id).filter(|e| e.token == token) else {
            return;
        };
        let view = entry.tracker.update(tick);
        if let OpState::Running { progress, .. } = &mut entry.state
            && progress.as_ref() != Some(&view)
        {
            *progress = Some(view);
            cx.notify();
        }
    }

    fn finish_op<T>(
        &mut self,
        meta: OpMeta,
        result: Result<T, OpError>,
        on_done: impl FnOnce(&mut Self, T, &mut Context<Self>),
        on_fail: OnFail,
        cx: &mut Context<Self>,
    ) {
        let OpMeta {
            id,
            token,
            kind,
            retry,
        } = meta;
        let id = id.as_str();
        if self.ops.get(id).is_none_or(|e| e.token != token) {
            return;
        }
        // Stop the progress forwarding; the finished entry goes with it.
        self.ops.remove(id);
        // The instance was deleted while the operation ran: nothing to show.
        let gone = self.loaded && self.entry(id).is_none();
        if gone {
            tracing::debug!(
                id,
                "dropping the result of an operation on a deleted instance"
            );
            on_fail(self, None, cx);
        } else {
            match result {
                Ok(value) => {
                    on_done(self, value, cx);
                    // A launch changes no instance on disk worth a new
                    // count or size: no status-bar disk scan per launch.
                    if kind != OpKind::Launch {
                        *self.op_changes.entry(id.to_string()).or_default() += 1;
                        cx.emit(ActivityEvent::Changed);
                    }
                }
                Err(error) => {
                    let message = error.message(|id| self.display_name(id));
                    tracing::warn!(id, ?kind, %message, "instance operation failed");
                    on_fail(self, Some(&error), cx);
                    let fix = error.fix();
                    self.insert_failed(id, token, kind, message, retry, fix);
                }
            }
        }
        cx.emit(ActivityEvent::Refresh);
        cx.notify();
    }

    /// Puts a failed operation on `id`'s header.
    fn insert_failed(
        &mut self,
        id: &str,
        token: u64,
        kind: OpKind,
        message: String,
        retry: Option<RetryAction>,
        fix: Option<OpFix>,
    ) {
        self.ops.insert(
            id.to_string(),
            OpEntry {
                state: OpState::Failed {
                    kind,
                    message,
                    retry,
                    fix,
                },
                token,
                tracker: ProgressTracker::default(),
                _forward: Task::ready(()),
                _job: Task::ready(()),
            },
        );
    }

    /// Removes the failed operation of `id` from the header.
    pub fn dismiss_op(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.ops.get(id).is_some_and(|e| !e.state.is_running()) {
            self.ops.remove(id);
            cx.notify();
        }
    }
}
