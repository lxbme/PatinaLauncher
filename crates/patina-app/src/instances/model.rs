//! [`InstancesModel`]: the instance list as the instances page knows it and
//! the selection. What the instances are doing (operations, launches,
//! games, logs) is the [`InstanceActivity`] it owns; the model delegates
//! to it, so callers keep one handle.

use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, SharedString, Subscription, Task, Window,
};
use patina_core::instance::{BrokenInstance, Instance, ListOutcome};
use patina_core::launch::running_game;
use patina_core::progress::ProgressSink;

use super::activity::{ActivityEvent, InstanceActivity};
use crate::core_bridge::{Core, Revision, run_blocking};
use crate::launch::{
    LaunchEvent, LogBuffer, OpError, OpFix, OpKind, OpState, RetryAction, RunState,
    instance_error_reason,
};

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

/// The instances page's domain state (an entity): the list and the
/// selection.
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
///
/// The model notifies only when the list, the selection or the refresh
/// state change. Run states, operations and their progress live in
/// [`InstancesModel::activity`], which notifies on its own: views that show
/// them observe it too.
pub struct InstancesModel {
    outcome: Rc<ListOutcome>,
    loaded: bool,
    refreshing: bool,
    /// Why the last refresh failed; the previous list stays on show.
    error: Option<SharedString>,
    /// Bumped by every refresh that succeeds, so dependants can tell a new
    /// list from other changes.
    generation: u64,
    selected: Option<String>,
    activity: Entity<InstanceActivity>,
    /// Broken entries being deleted, so a second request is ignored.
    deleting: HashSet<String>,
    revision: Revision,
    task: Task<()>,
    /// Told when the next refresh lands ([`InstancesModel::refresh_and_wait`]).
    refresh_waiters: Vec<oneshot::Sender<()>>,
    _activation: Subscription,
    _activity_events: Subscription,
}

/// Emitted when a refresh after the first load failed, with why; the list
/// on show is the previous one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshFailed(pub SharedString);

impl EventEmitter<InstancesChanged> for InstancesModel {}
impl EventEmitter<RefreshFailed> for InstancesModel {}
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
        let activity = cx.new(|_| InstanceActivity::new());
        let _activity_events =
            cx.subscribe(
                &activity,
                |this, _, event: &ActivityEvent, cx| match event {
                    ActivityEvent::Refresh => this.refresh(cx),
                    ActivityEvent::Changed => cx.emit(InstancesChanged),
                    ActivityEvent::Launch(event) => cx.emit(event.clone()),
                },
            );
        let mut this = Self {
            outcome: Rc::default(),
            loaded: false,
            refreshing: false,
            error: None,
            generation: 0,
            selected: None,
            activity,
            deleting: HashSet::new(),
            revision: Revision::default(),
            task: Task::ready(()),
            refresh_waiters: Vec::new(),
            _activation,
            _activity_events,
        };
        this.refresh(cx);
        this
    }

    /// What the instances are doing: observe it to follow run states,
    /// operations and their progress.
    pub fn activity(&self) -> &Entity<InstanceActivity> {
        &self.activity
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

    /// The list as last read, healthy and broken entries (shared, so a
    /// search can keep it while the list moves on).
    pub fn outcome(&self) -> &Rc<ListOutcome> {
        &self.outcome
    }

    /// The display name of healthy instance `id`.
    pub fn display_name(&self, id: &str) -> Option<String> {
        display_name_in(&self.outcome, id)
    }

    // --- the activity, delegated -------------------------------------------

    /// What the game of instance `id` is doing; `None` when idle.
    pub fn run_state<'a>(&self, id: &str, cx: &'a App) -> Option<&'a RunState> {
        self.activity.read(cx).run_state(id)
    }

    /// Whether a game of instance `id` runs (here or in another Patina
    /// process).
    pub fn is_running(&self, id: &str, cx: &App) -> bool {
        self.activity.read(cx).is_running(id)
    }

    /// Whether instance `id` must not be launched or changed now: an
    /// operation runs on it, or its game is starting or running. The
    /// versions and settings tabs disable their changes on it.
    pub fn is_busy(&self, id: &str, cx: &App) -> bool {
        self.activity.read(cx).is_busy(id)
    }

    /// Why instance `id` takes no change now, or `None`; see
    /// [`InstanceActivity::busy_reason`].
    pub fn busy_reason(&self, id: &str, cx: &App) -> Option<String> {
        self.activity.read(cx).busy_reason(id)
    }

    /// Whether this app follows a game of `id`.
    pub fn is_following(&self, id: &str, cx: &App) -> bool {
        self.activity.read(cx).is_following(id)
    }

    /// Whether a force stop of `id`'s game is in flight.
    pub fn is_stopping(&self, id: &str, cx: &App) -> bool {
        self.activity.read(cx).is_stopping(id)
    }

    /// The operation of instance `id`: running, or failed and not dismissed.
    pub fn op<'a>(&self, id: &str, cx: &'a App) -> Option<&'a OpState> {
        self.activity.read(cx).op(id)
    }

    /// Whether an operation of instance `id` is running.
    pub fn is_op_running(&self, id: &str, cx: &App) -> bool {
        self.activity.read(cx).is_op_running(id)
    }

    /// How many of this app's operations other than launches have finished
    /// on instance `id` (a counter to compare).
    pub fn op_changes(&self, id: &str, cx: &App) -> u64 {
        self.activity.read(cx).op_changes(id)
    }

    /// Whether instance `id` has a log buffer.
    pub fn has_log(&self, id: &str, cx: &App) -> bool {
        self.activity.read(cx).has_log(id)
    }

    /// The crash log saved after the last run of `id`.
    pub fn crash_log<'a>(&self, id: &str, cx: &'a App) -> Option<&'a Path> {
        self.activity.read(cx).crash_log(id)
    }

    /// The warnings of `id`'s prepared launch that waits for an answer.
    pub fn pending_warnings(&self, id: &str, cx: &App) -> Option<Vec<String>> {
        self.activity.read(cx).pending_warnings(id)
    }

    /// Sets what instance `id` is doing (`None`: idle) without following a
    /// game; for tests and previews.
    pub fn set_run_state(&mut self, id: &str, state: Option<RunState>, cx: &mut Context<Self>) {
        self.activity
            .update(cx, |a, cx| a.set_run_state(id, state, cx));
    }

    /// The log buffer of instance `id`, created empty on first use.
    pub fn log_buffer(&mut self, id: &str, cx: &mut Context<Self>) -> Entity<LogBuffer> {
        self.activity.update(cx, |a, cx| a.log_buffer(id, cx))
    }

    /// Starts a new run's log; see [`InstanceActivity::begin_log`].
    pub fn begin_log(&mut self, id: &str, cx: &mut Context<Self>) -> Entity<LogBuffer> {
        self.activity.update(cx, |a, cx| a.begin_log(id, cx))
    }

    /// Records (or clears) the saved crash log of `id`.
    pub fn set_crash_log(
        &mut self,
        id: &str,
        path: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.activity
            .update(cx, |a, cx| a.set_crash_log(id, path, cx));
    }

    /// Runs `f` as operation `kind` of instance `id` in the background and
    /// returns whether it started; an instance that is busy (an operation
    /// runs, or its game starts or runs, here or elsewhere) rejects it
    /// (returns `false`, `f` is not called). A failed
    /// operation left on the header is replaced.
    ///
    /// `f` gets the progress sink to hand to the core and returns the core
    /// work, which runs on the app's runtime; `T` is its result (the loader
    /// operations produce an `OpOutcome`, launching something else). The
    /// progress reaches only the activity's entry of `id` (the model is not
    /// notified), with the bar held from going back within a phase.
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
        let model = cx.weak_entity();
        let on_done =
            move |_: &mut InstanceActivity, value: T, cx: &mut Context<InstanceActivity>| {
                // The activity is being updated, and `on_done` may use it
                // through the model: run it once the update is over.
                cx.defer(move |cx| {
                    model.update(cx, |m, cx| on_done(m, value, cx)).ok();
                });
            };
        self.activity
            .update(cx, |a, cx| a.start_op(id, kind, f, on_done, retry, cx))
    }

    /// Starts the failed operation of `id` again through its retry action.
    pub fn retry_op(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(retry) = self.activity.read(cx).retry_of(id) {
            retry(self, cx);
        }
    }

    /// Removes the failed operation of `id` from the header.
    pub fn dismiss_op(&mut self, id: &str, cx: &mut Context<Self>) {
        self.activity.update(cx, |a, cx| a.dismiss_op(id, cx));
    }

    /// Launches healthy instance `id`; returns whether the launch started.
    /// A busy instance ([`InstancesModel::is_busy`]) refuses. See
    /// [`InstanceActivity::launch`].
    pub fn launch(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        self.activity.update(cx, |a, cx| a.launch(id, cx))
    }

    /// Starts `id`'s prepared launch despite its warnings.
    pub fn confirm_launch(&mut self, id: &str, cx: &mut Context<Self>) {
        self.activity.update(cx, |a, cx| a.confirm_launch(id, cx));
    }

    /// Cancels `id`'s prepared launch: nothing starts and its locks are
    /// released. Does nothing once it was confirmed.
    pub fn cancel_launch(&mut self, id: &str, cx: &mut Context<Self>) {
        self.activity.update(cx, |a, cx| a.cancel_launch(id, cx));
    }

    /// Force-stops `id`'s running game; see [`InstanceActivity::force_stop`].
    pub fn force_stop(&mut self, id: &str, cx: &mut Context<Self>) {
        self.activity.update(cx, |a, cx| a.force_stop(id, cx));
    }

    /// Removes the crash notice of `id`'s last run from the header (the
    /// log tab keeps its banner).
    pub fn dismiss_exit(&mut self, id: &str, cx: &mut Context<Self>) {
        self.activity.update(cx, |a, cx| a.dismiss_exit(id, cx));
    }

    /// Applies the fix offered by `id`'s failed operation.
    pub fn apply_fix(&mut self, id: &str, cx: &mut Context<Self>) {
        let fix = match self.op(id, cx) {
            Some(OpState::Failed { fix: Some(fix), .. }) => *fix,
            _ => return,
        };
        match fix {
            OpFix::AddAccount => {
                self.dismiss_op(id, cx);
                cx.emit(LaunchEvent::ShowAccounts);
            }
            OpFix::ReinstallLoader => {
                self.reinstall_and_launch(id, cx);
            }
        }
    }

    /// Reinstalls `id`'s loader (through `Core.version_ops`, as the versions
    /// tab's repair does), then launches it again once the list shows the
    /// reinstalled instance.
    pub fn reinstall_and_launch(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let ops = Core::global(cx).version_ops.clone();
        let key = id.to_string();
        let retry: RetryAction = {
            let key = key.clone();
            Rc::new(move |this: &mut Self, cx: &mut Context<Self>| {
                this.reinstall_and_launch(&key, cx);
            })
        };
        self.start_op(
            id,
            OpKind::Reinstall,
            {
                let key = key.clone();
                move |progress| {
                    async move {
                        ops.reinstall_loader(key, progress)
                            .await
                            .map_err(OpError::Loader)
                    }
                    .boxed()
                }
            },
            move |this, _, cx| {
                let listed = this.refresh_and_wait(cx);
                cx.spawn(async move |this, cx| {
                    listed.await;
                    this.update(cx, |this, cx| this.launch(&key, cx)).ok();
                })
                .detach();
            },
            Some(retry),
            cx,
        )
    }

    // --- the list ------------------------------------------------------------

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
        let activity = self.activity.read(cx);
        let probe = activity.reattach_started();
        let known: HashSet<String> = activity.known_ids().cloned().collect();
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
                        let list = this.outcome.clone();
                        this.activity
                            .update(cx, |a, cx| a.list_loaded(list, running, cx));
                    }
                    Ok(Err(error)) => this.list_failed(error.to_string().into(), cx),
                    Err(error) => this.list_failed(error.to_string().into(), cx),
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

    /// A refresh failed: the previous list stays. After the first load the
    /// page is told ([`RefreshFailed`]) once per run of failures (or when
    /// the reason changes), since every window activation refreshes.
    fn list_failed(&mut self, error: SharedString, cx: &mut Context<Self>) {
        tracing::warn!(%error, "listing the instances failed");
        if self.loaded && self.error.as_ref() != Some(&error) {
            cx.emit(RefreshFailed(error.clone()));
        }
        self.error = Some(error);
        self.activity.update(cx, |a, _| a.list_failed());
    }

    fn apply(&mut self, outcome: ListOutcome) {
        self.selected = reselect(self.selected.as_deref(), &outcome);
        self.outcome = Rc::new(outcome);
        self.error = None;
        self.generation += 1;
    }

    /// Deletes healthy instance `id` and its directory in the background.
    /// Resolves to the localized reason if that failed, worded to follow
    /// the instance's name ([`instance_error_reason`]).
    pub fn delete(&mut self, id: &str, cx: &mut Context<Self>) -> Task<Result<(), SharedString>> {
        let store = Core::global(cx).store.clone();
        let job = {
            let id = id.to_string();
            run_blocking(cx, move || store.delete(&id))
        };
        cx.spawn(async move |_, _| match job.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(instance_error_reason(&error).into()),
            Err(error) => Err(error.to_string().into()),
        })
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
                // Shown after the entry's id.
                Ok(Err(error)) => Err(SharedString::from(instance_error_reason(&error))),
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

/// The display name of healthy instance `id` of `outcome`.
pub(super) fn display_name_in(outcome: &ListOutcome, id: &str) -> Option<String> {
    match find_entry(outcome, id)? {
        Entry::Healthy(i) => Some(i.config.name.clone()),
        Entry::Broken(_) => None,
    }
}

pub(super) fn find_entry<'a>(outcome: &'a ListOutcome, id: &str) -> Option<Entry<'a>> {
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
