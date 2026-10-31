//! [`InstancesModel`]: the instance list as the instances page knows it, the
//! selection, which instances are running, and the launch flow that gets
//! them there.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use futures::channel::oneshot;
use futures::future::BoxFuture;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, SharedString, Subscription, Task, Window,
};
use patina_core::instance::{BrokenInstance, Instance, ListOutcome};
use patina_core::launch::{PendingLaunch, running_game};
use patina_core::progress::{ProgressSink, TaskProgress};

use crate::core_bridge::{Core, Revision, forward_progress, run, run_blocking};
use crate::launch::{
    LaunchEvent, LogBuffer, OpError, OpFix, OpKind, OpState, ProgressTracker, RetryAction, RunState,
};

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
type OnFail = Box<dyn FnOnce(&mut InstancesModel, Option<&OpError>, &mut Context<InstancesModel>)>;

mod launch;

/// One entry of the list: an instance that loads, or a directory whose
/// config does not.
#[derive(Debug, Clone, Copy)]
pub enum Entry<'a> {
    Healthy(&'a Instance),
    Broken(&'a BrokenInstance),
}

impl Entry<'_> {
    pub fn id(&self) -> &str {
        match self {
            Entry::Healthy(i) => &i.id,
            Entry::Broken(b) => &b.id,
        }
    }
}

/// Emitted after this app changed the instances on disk (created, deleted,
/// repaired), so views that summarise them (the status bar) re-read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstancesChanged;

/// The instances page's domain state (an entity).
///
/// The list is read from the core's `InstanceStore` in the background
/// ([`InstancesModel::refresh`]): when the model is created, whenever the
/// window becomes active (another program may have changed the directory)
/// and after each of this app's own operations. A newer refresh makes the
/// results of older ones stale.
///
/// The selection is an instance id (the directory name, which healthy and
/// broken entries share), so it survives refreshes and reordering; when the
/// selected entry is gone the first entry is selected.
pub struct InstancesModel {
    outcome: ListOutcome,
    loaded: bool,
    refreshing: bool,
    /// Why the last refresh failed; the previous list stays on show.
    error: Option<SharedString>,
    /// Bumped by every refresh that succeeds, so dependants can tell a new
    /// list from other changes.
    generation: u64,
    selected: Option<String>,
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
    /// Operations by instance id; see [`InstancesModel::start_op`].
    ops: HashMap<String, OpEntry>,
    next_op: u64,
    /// The game output of each instance that ran (or is running) in this
    /// session. Entities of their own, so a log batch notifies only the log
    /// view, never the model's other observers.
    logs: HashMap<String, Entity<LogBuffer>>,
    /// The crash log saved after the last run of an instance, if it crashed.
    crash_logs: HashMap<String, PathBuf>,
    /// Broken entries being deleted, so a second request is ignored.
    deleting: HashSet<String>,
    revision: Revision,
    task: Task<()>,
    /// Told when the next refresh lands ([`InstancesModel::refresh_and_wait`]).
    refresh_waiters: Vec<oneshot::Sender<()>>,
    _activation: Subscription,
}

impl EventEmitter<InstancesChanged> for InstancesModel {}
impl EventEmitter<LaunchEvent> for InstancesModel {}

impl InstancesModel {
    /// Starts loading the list, and reloads it whenever `window` becomes
    /// active.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.refresh(cx);
            }
        });
        let mut this = Self {
            outcome: ListOutcome::default(),
            loaded: false,
            refreshing: false,
            error: None,
            generation: 0,
            selected: None,
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
            crash_logs: HashMap::new(),
            deleting: HashSet::new(),
            revision: Revision::default(),
            task: Task::ready(()),
            refresh_waiters: Vec::new(),
            _activation,
        };
        this.refresh(cx);
        this
    }

    /// The listed instances, most recently played first.
    pub fn instances(&self) -> &[Instance] {
        &self.outcome.instances
    }

    /// Directories whose config failed to load, by id.
    pub fn broken(&self) -> &[BrokenInstance] {
        &self.outcome.broken
    }

    /// Whether the first refresh has finished (successfully or not).
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Whether a refresh is in flight.
    pub fn is_refreshing(&self) -> bool {
        self.refreshing
    }

    /// Why the last refresh failed, if it did.
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    /// Counts the refreshes that brought a new list.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether there is nothing at all to list, healthy or broken.
    pub fn is_empty(&self) -> bool {
        self.outcome.instances.is_empty() && self.outcome.broken.is_empty()
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The selected entry, if the selection is set.
    pub fn selected_entry(&self) -> Option<Entry<'_>> {
        self.entry(self.selected.as_deref()?)
    }

    /// The entry `id`, healthy or broken.
    pub fn entry(&self, id: &str) -> Option<Entry<'_>> {
        find_entry(&self.outcome, id)
    }

    /// Selects entry `id` if it is listed; notifies only on a change.
    pub fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(id) || self.entry(id).is_none() {
            return;
        }
        self.selected = Some(id.to_string());
        cx.notify();
    }

    /// What the game of instance `id` is doing; `None` when idle.
    pub fn run_state(&self, id: &str) -> Option<&RunState> {
        self.running.get(id)
    }

    /// Whether a game of instance `id` runs (here or in another Patina
    /// process).
    pub fn is_running(&self, id: &str) -> bool {
        self.running.get(id).is_some_and(RunState::is_running)
    }

    /// Whether instance `id` must not be launched or changed now: an
    /// operation runs on it, or its game is starting or running. The
    /// versions and settings tabs disable their changes on it.
    pub fn is_busy(&self, id: &str) -> bool {
        self.is_op_running(id) || self.running.get(id).is_some_and(RunState::is_busy)
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
    /// the last run's output until [`InstancesModel::begin_log`].
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

    /// The display name of healthy instance `id`.
    pub fn display_name(&self, id: &str) -> Option<String> {
        match self.entry(id)? {
            Entry::Healthy(i) => Some(i.config.name.clone()),
            Entry::Broken(_) => None,
        }
    }

    /// The operation of instance `id`: running, or failed and not dismissed.
    pub fn op(&self, id: &str) -> Option<&OpState> {
        self.ops.get(id).map(|e| &e.state)
    }

    /// Whether an operation of instance `id` is running.
    pub fn is_op_running(&self, id: &str) -> bool {
        self.op(id).is_some_and(OpState::is_running)
    }

    /// Runs `f` as operation `kind` of instance `id` in the background and
    /// returns whether it started; an instance with an operation running
    /// rejects a second one (returns `false`, `f` is not called). A failed
    /// operation left on the header is replaced.
    ///
    /// `f` gets the progress sink to hand to the core and returns the core
    /// work, which runs on the app's runtime; `T` is its result (the loader
    /// operations produce an `OpOutcome`, launching something else). The
    /// progress reaches only the `ops` entry of `id`, with the bar held from
    /// going back within a phase.
    ///
    /// When the work succeeds `on_done` runs on the model with `T`, then the
    /// list is refreshed and [`InstancesChanged`] is emitted. When it fails
    /// the entry becomes [`OpState::Failed`] with the localized message
    /// ([`OpError::message`]) and `retry`, and the list is refreshed. A result
    /// is dropped, `on_done` not called, if the instance has left the list in
    /// the meantime or a newer operation replaced this one. "Left the list"
    /// is judged against the list as loaded when the work ends, so a caller
    /// that has just created `id` on disk must refresh (and wait for it)
    /// before starting an operation on it.
    pub fn start_op<T: Send + 'static>(
        &mut self,
        id: &str,
        kind: OpKind,
        f: impl FnOnce(ProgressSink) -> BoxFuture<'static, Result<T, OpError>>,
        on_done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
        retry: Option<RetryAction>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.start_op_with(id, kind, f, on_done, retry, Box::new(|_, _, _| {}), cx)
    }

    /// [`InstancesModel::start_op`] that also runs `on_fail` when the work
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
                    cx.emit(InstancesChanged);
                }
                Err(error) => {
                    let message = error.message(self);
                    tracing::warn!(id, ?kind, %message, "instance operation failed");
                    on_fail(self, Some(&error), cx);
                    let fix = error.fix();
                    self.insert_failed(id, token, kind, message, retry, fix);
                }
            }
        }
        self.refresh(cx);
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

    /// Starts the failed operation of `id` again through its retry action.
    pub fn retry_op(&mut self, id: &str, cx: &mut Context<Self>) {
        let retry = match self.ops.get(id).map(|e| &e.state) {
            Some(OpState::Failed { retry, .. }) => retry.clone(),
            _ => None,
        };
        if let Some(retry) = retry {
            retry(self, cx);
        }
    }

    /// Removes the failed operation of `id` from the header.
    pub fn dismiss_op(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.ops.get(id).is_some_and(|e| !e.state.is_running()) {
            self.ops.remove(id);
            cx.notify();
        }
    }

    /// Whether broken entry `id` is being deleted.
    pub fn is_deleting(&self, id: &str) -> bool {
        self.deleting.contains(id)
    }

    /// Re-reads the list in the background; keeps the selection when its
    /// entry is still listed and otherwise selects the first entry.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let id = self.revision.start();
        let store = Core::global(cx).store.clone();
        // Once the startup look is done, also look (lock-free) for games of
        // idle instances, such as ones another Patina process started.
        let probe = self.reattach_started;
        let known: HashSet<String> = (self.running.keys())
            .chain(self.tracks.keys())
            .cloned()
            .collect();
        let list = run_blocking(cx, move || {
            let outcome = store.list();
            let running: Vec<String> = match &outcome {
                Ok(outcome) if probe => (outcome.instances.iter())
                    .filter(|i| !known.contains(&i.id) && running_game(i).is_some())
                    .map(|i| i.id.clone())
                    .collect(),
                _ => Vec::new(),
            };
            (outcome, running)
        });
        self.refreshing = true;
        // Replacing the task drops the previous refresh.
        self.task = cx.spawn(async move |this, cx| {
            let result = list.await;
            this.update(cx, |this, cx| {
                if !this.revision.is_current(id) {
                    return;
                }
                let result =
                    result.map(|(outcome, running)| outcome.map(|outcome| (outcome, running)));
                match result {
                    Ok(Ok((outcome, running))) => {
                        this.apply(outcome);
                        if !std::mem::replace(&mut this.reattach_started, true) {
                            this.reattach_on_startup(cx);
                        }
                        for id in running {
                            let idle =
                                !this.running.contains_key(&id) && !this.tracks.contains_key(&id);
                            if idle {
                                this.follow_game(&id, false, cx);
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "listing the instances failed");
                        this.error = Some(error.to_string().into());
                    }
                    Err(error) => this.error = Some(error.to_string().into()),
                }
                this.refreshing = false;
                this.loaded = true;
                for waiter in this.refresh_waiters.drain(..) {
                    waiter.send(()).ok();
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Refreshes as [`InstancesModel::refresh`] does; the returned future
    /// resolves once a refresh started no earlier than this call has landed
    /// (successfully or not), or the model is gone.
    pub fn refresh_and_wait(&mut self, cx: &mut Context<Self>) -> impl Future<Output = ()> + use<> {
        let (tx, rx) = oneshot::channel();
        self.refresh_waiters.push(tx);
        self.refresh(cx);
        async move {
            rx.await.ok();
        }
    }

    fn apply(&mut self, outcome: ListOutcome) {
        self.selected = reselect(self.selected.as_deref(), &outcome);
        // A failure left on the header of an instance that is gone has
        // nothing to hang on to. (Running ones end by themselves.)
        self.ops
            .retain(|id, e| e.state.is_running() || find_entry(&outcome, id).is_some());
        // Nor does the log of an instance that is gone.
        let running = &self.running;
        let kept = |id: &String| running.contains_key(id) || find_entry(&outcome, id).is_some();
        self.logs.retain(|id, _| kept(id));
        self.crash_logs.retain(|id, _| kept(id));
        // A crash notice has nothing to hang on to either; a game that runs
        // stays (it ends by itself).
        self.running.retain(|id, state| {
            !matches!(state, RunState::Exited(_)) || find_entry(&outcome, id).is_some()
        });
        self.outcome = outcome;
        self.error = None;
        self.generation += 1;
    }

    /// Deletes the directory of broken entry `broken` in the background,
    /// then refreshes. Resolves to the error message if that failed. A
    /// second request for an entry being deleted resolves at once.
    pub fn delete_broken(
        &mut self,
        broken: BrokenInstance,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), SharedString>> {
        let id = broken.id.clone();
        if !self.deleting.insert(id.clone()) {
            return Task::ready(Ok(()));
        }
        cx.notify();
        let store = Core::global(cx).store.clone();
        let job = run_blocking(cx, move || store.delete_broken(&broken));
        cx.spawn(async move |this, cx| {
            let result = match job.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(SharedString::from(error.to_string())),
                Err(error) => Err(SharedString::from(error.to_string())),
            };
            if let Err(error) = &result {
                tracing::warn!(%error, id, "deleting a broken instance failed");
            }
            this.update(cx, |this, cx| {
                this.deleting.remove(&id);
                if result.is_ok() {
                    cx.emit(InstancesChanged);
                }
                // Either way the directory may have changed (repaired,
                // removed by someone else).
                this.refresh(cx);
                cx.notify();
            })
            .ok();
            result
        })
    }
}

fn find_entry<'a>(outcome: &'a ListOutcome, id: &str) -> Option<Entry<'a>> {
    let healthy = outcome.instances.iter().find(|i| i.id == id);
    healthy.map(Entry::Healthy).or_else(|| {
        let broken = outcome.broken.iter().find(|b| b.id == id);
        broken.map(Entry::Broken)
    })
}

/// The selection after the list became `outcome`: `previous` if it is still
/// listed, else the first healthy entry, else the first broken one.
pub(crate) fn reselect(previous: Option<&str>, outcome: &ListOutcome) -> Option<String> {
    if let Some(id) = previous
        && find_entry(outcome, id).is_some()
    {
        return Some(id.to_string());
    }
    let first = outcome.instances.first().map(|i| &i.id);
    first
        .or_else(|| outcome.broken.first().map(|b| &b.id))
        .cloned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use patina_core::instance::{InstanceConfig, InstanceError};

    use super::*;

    fn healthy(id: &str) -> Instance {
        Instance {
            id: id.to_string(),
            dir: PathBuf::from(id),
            config: InstanceConfig {
                name: id.to_string(),
                ..Default::default()
            },
        }
    }

    fn broken(id: &str) -> BrokenInstance {
        BrokenInstance {
            id: id.to_string(),
            dir: PathBuf::from(id),
            error: InstanceError::Parse("bad".into()),
        }
    }

    fn outcome(ok: &[&str], bad: &[&str]) -> ListOutcome {
        ListOutcome {
            instances: ok.iter().map(|id| healthy(id)).collect(),
            broken: bad.iter().map(|id| broken(id)).collect(),
        }
    }

    #[test]
    fn reselect_keeps_a_listed_entry_and_falls_back_to_the_first() {
        let list = outcome(&["b", "a"], &["x"]);
        assert_eq!(reselect(Some("a"), &list).as_deref(), Some("a"));
        // A broken entry can stay selected.
        assert_eq!(reselect(Some("x"), &list).as_deref(), Some("x"));
        assert_eq!(reselect(Some("gone"), &list).as_deref(), Some("b"));
        assert_eq!(reselect(None, &list).as_deref(), Some("b"));
        // Only broken entries left: the first of them.
        let only_broken = outcome(&[], &["x", "y"]);
        assert_eq!(reselect(Some("a"), &only_broken).as_deref(), Some("x"));
        assert_eq!(reselect(Some("a"), &outcome(&[], &[])), None);
    }
}
