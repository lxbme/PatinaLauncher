//! The launch flow of [`InstancesModel`] and the games it follows: a child
//! module of the model, so it shares the model's private state.

use std::rc::Rc;

use futures::FutureExt as _;
use gpui_kit::{Context, Task};
use patina_core::instance::Instance;
use patina_core::launch::{
    GameSession, LaunchAccount, PendingLaunch, Reattach, SessionExit, SessionWarning,
};
use rust_i18n::t;

use super::{Entry, InstancesModel, LaunchFlow, OnFail, Track};
use crate::core_bridge::{BridgeError, Core, run, run_blocking};
use crate::launch::session::{
    AccountIssue, ELSEWHERE_POLL, reattach_all, resolve_account, stop_elsewhere,
};
use crate::launch::{
    LaunchEvent, OpError, OpFix, OpKind, OpState, RetryAction, RunState, forward_logs,
};

/// The launch flow and the games it follows. Transitions of an instance's
/// [`RunState`]:
///
/// - idle or `Exited` → `Starting` ([`InstancesModel::launch`]): the
///   account lookup, the prepare operation, the warnings (if any), the
///   spawn operation;
/// - `Starting` → `Running` once the game is spawned, or back to idle when
///   any step fails or is cancelled;
/// - `Running` → idle when the game exits normally (also after a force
///   stop), `Exited` when it crashed;
/// - idle → `Running { reattached }` / `HeldElsewhere` when a game of an
///   earlier launcher session is found (after the first list load, and
///   after a launch found the instance busy);
/// - `HeldElsewhere` → idle once the game is gone, or `Running` when its
///   session became free and this app took it over.
impl InstancesModel {
    fn next_token(&mut self) -> u64 {
        self.next_op += 1;
        self.next_op
    }

    /// Launches healthy instance `id` (see the type docs); returns whether
    /// the launch started. A busy instance ([`InstancesModel::is_busy`])
    /// refuses.
    pub fn launch(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        if self.is_busy(id) || !matches!(self.entry(id), Some(Entry::Healthy(_))) {
            return false;
        }
        // A failure from an earlier attempt goes; so does a crash notice.
        self.ops.remove(id);
        let token = self.next_token();
        let core = Core::global(cx);
        let (backend, store) = (core.launch_backend.clone(), core.store.clone());
        let account = Core::with_accounts(cx, move |accounts| {
            resolve_account(backend.as_ref(), accounts)
        });
        // The instance as it is on disk now, not as last listed.
        let instance = {
            let id = id.to_string();
            run_blocking(cx, move || store.load(&id))
        };
        let key = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let account = account.await;
            let instance = instance.await;
            this.update(cx, |this, cx| {
                this.account_ready(&key, token, account, instance, cx)
            })
            .ok();
        });
        self.flows.insert(
            id.to_string(),
            LaunchFlow {
                token,
                pending: None,
                _task: task,
            },
        );
        self.running.insert(id.to_string(), RunState::Starting);
        cx.notify();
        true
    }

    /// Whether the launch flow `token` of `id` is still the current one.
    fn flow_is(&self, id: &str, token: u64) -> bool {
        self.flows.get(id).is_some_and(|f| f.token == token)
    }

    /// Ends launch flow `token` of `id` (dropping whatever it still holds)
    /// and leaves `Starting`; a state set meanwhile by someone else stays.
    fn end_flow(&mut self, id: &str, token: u64, cx: &mut Context<Self>) {
        if !self.flow_is(id, token) {
            return;
        }
        self.flows.remove(id);
        if matches!(self.running.get(id), Some(RunState::Starting)) {
            self.running.remove(id);
        }
        cx.notify();
    }

    /// Retries a launch of `id`.
    fn relaunch(id: &str) -> RetryAction {
        let id = id.to_string();
        Rc::new(move |this: &mut Self, cx: &mut Context<Self>| {
            this.launch(&id, cx);
        })
    }

    /// Ends the flow and shows why it failed.
    fn fail_launch(
        &mut self,
        id: &str,
        token: u64,
        message: String,
        fix: Option<OpFix>,
        cx: &mut Context<Self>,
    ) {
        tracing::warn!(id, %message, "launch failed");
        self.end_flow(id, token, cx);
        let retry = fix.is_none().then(|| Self::relaunch(id));
        let op_token = self.next_token();
        self.insert_failed(id, op_token, OpKind::Launch, message, retry, fix);
        cx.notify();
    }

    fn account_ready(
        &mut self,
        id: &str,
        token: u64,
        account: Result<Result<LaunchAccount, AccountIssue>, BridgeError>,
        instance: Result<Result<Instance, patina_core::instance::InstanceError>, BridgeError>,
        cx: &mut Context<Self>,
    ) {
        if !self.flow_is(id, token) {
            return;
        }
        let account = match account {
            Ok(Ok(account)) => account,
            Ok(Err(issue)) => {
                let fix = issue.wants_account().then_some(OpFix::AddAccount);
                return self.fail_launch(id, token, issue.message(), fix, cx);
            }
            Err(e) => return self.fail_launch(id, token, e.to_string(), None, cx),
        };
        let instance = match instance {
            Ok(Ok(instance)) => instance,
            Ok(Err(e)) => return self.fail_launch(id, token, e.to_string(), None, cx),
            Err(e) => return self.fail_launch(id, token, e.to_string(), None, cx),
        };
        let backend = Core::global(cx).launch_backend.clone();
        let key = id.to_string();
        let started = self.start_op_with(
            id,
            OpKind::Launch,
            move |progress| {
                let prepare = backend.prepare(instance, account, progress);
                async move { prepare.await.map_err(OpError::Session) }.boxed()
            },
            {
                let key = key.clone();
                move |this, pending, cx| this.prepared(&key, token, pending, cx)
            },
            Some(Self::relaunch(id)),
            Self::on_launch_failed(key, token),
            cx,
        );
        if !started {
            self.end_flow(id, token, cx);
        }
    }

    /// Cleanup for a failed prepare or spawn of flow `token`: ends the flow,
    /// and when the instance turned out to be held or running, looks for
    /// its game.
    fn on_launch_failed(id: String, token: u64) -> OnFail {
        Box::new(move |this, error, cx| {
            this.end_flow(&id, token, cx);
            if error.is_some_and(OpError::may_be_running) {
                this.follow_game(&id, false, cx);
            }
        })
    }

    fn prepared(&mut self, id: &str, token: u64, pending: PendingLaunch, cx: &mut Context<Self>) {
        // A flow that ended meanwhile drops `pending`: nothing starts.
        if !self.flow_is(id, token) {
            return;
        }
        if pending.warnings().is_empty() {
            self.spawn_game(id, token, pending, cx);
            return;
        }
        if let Some(flow) = self.flows.get_mut(id) {
            flow.pending = Some(pending);
        }
        cx.emit(LaunchEvent::ConfirmWarnings { id: id.to_string() });
    }

    /// The warnings of `id`'s prepared launch that waits for an answer, as
    /// the core words them.
    pub fn pending_warnings(&self, id: &str) -> Option<Vec<String>> {
        let pending = self.flows.get(id)?.pending.as_ref()?;
        Some(pending.warnings().iter().map(ToString::to_string).collect())
    }

    /// Starts `id`'s prepared launch despite its warnings.
    pub fn confirm_launch(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(flow) = self.flows.get_mut(id) else {
            return;
        };
        let token = flow.token;
        if let Some(pending) = flow.pending.take() {
            self.spawn_game(id, token, pending, cx);
        }
    }

    /// Cancels `id`'s prepared launch: nothing starts and its locks are
    /// released. Does nothing once it was confirmed.
    pub fn cancel_launch(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(flow) = self.flows.get(id) else {
            return;
        };
        if flow.pending.is_some() {
            let token = flow.token;
            self.end_flow(id, token, cx);
        }
    }

    fn spawn_game(&mut self, id: &str, token: u64, pending: PendingLaunch, cx: &mut Context<Self>) {
        let key = id.to_string();
        let started = self.start_op_with(
            id,
            OpKind::Launch,
            move |progress| {
                async move { pending.launch(progress).await.map_err(OpError::Session) }.boxed()
            },
            {
                let key = key.clone();
                move |this, session: GameSession, cx| {
                    if this.flow_is(&key, token) {
                        this.flows.remove(&key);
                    }
                    // What went wrong while starting it (the prepare
                    // warnings were answered already).
                    let warnings: Vec<_> = (session.warnings().iter())
                        .filter(|w| !matches!(w, SessionWarning::Prepare(_)))
                        .collect();
                    let record_saved = !warnings
                        .iter()
                        .any(|w| matches!(w, SessionWarning::RecordNotSaved { .. }));
                    let warnings = warnings.iter().map(ToString::to_string).collect();
                    this.follow_session(&key, session, false, cx);
                    this.after_spawn(&key, warnings, record_saved, cx);
                }
            },
            Some(Self::relaunch(id)),
            Self::on_launch_failed(key, token),
            cx,
        );
        if !started {
            self.end_flow(id, token, cx);
        }
    }

    /// Records the play time, then reports the start ([`LaunchEvent::Spawned`],
    /// which applies the after-launch window behaviour, so a quit comes only
    /// after the play time is saved).
    fn after_spawn(
        &mut self,
        id: &str,
        warnings: Vec<String>,
        record_saved: bool,
        cx: &mut Context<Self>,
    ) {
        for warning in &warnings {
            tracing::warn!(id, %warning, "game started with a warning");
        }
        let store = Core::global(cx).store.clone();
        let key = id.to_string();
        let touched = {
            let id = key.clone();
            run_blocking(cx, move || store.touch_played(&id))
        };
        cx.spawn(async move |this, cx| {
            match touched.await {
                Ok(Err(error)) => tracing::warn!(%error, "recording the play time failed"),
                Err(error) => tracing::warn!(%error, "recording the play time failed"),
                Ok(Ok(())) => {}
            }
            this.update(cx, |this, cx| {
                this.refresh(cx);
                cx.emit(LaunchEvent::Spawned {
                    id: key,
                    warnings,
                    record_saved,
                });
            })
            .ok();
        })
        .detach();
    }

    /// Follows `session` as `id`'s game: forwards its log into a fresh log
    /// buffer and waits for it to end on the runtime.
    fn follow_session(
        &mut self,
        id: &str,
        mut session: GameSession,
        reattached: bool,
        cx: &mut Context<Self>,
    ) {
        let token = self.next_token();
        let (pid, killer, logs) = (session.pid(), session.killer(), session.logs());
        // The previous run's forwarding stops before its buffer is reused.
        self.log_forwards.remove(id);
        let buffer = self.begin_log(id, cx);
        let forward = forward_logs(cx, buffer.downgrade(), logs);
        self.log_forwards.insert(id.to_string(), forward);
        self.running.insert(
            id.to_string(),
            RunState::Running {
                pid,
                killer,
                reattached,
            },
        );
        let wait = run(cx, session.wait());
        let key = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let exit = wait.await;
            this.update(cx, |this, cx| this.session_ended(&key, token, exit, cx))
                .ok();
        });
        self.tracks
            .insert(id.to_string(), Track { token, _task: task });
        cx.notify();
    }

    fn session_ended(
        &mut self,
        id: &str,
        token: u64,
        exit: Result<SessionExit, BridgeError>,
        cx: &mut Context<Self>,
    ) {
        if self.tracks.get(id).is_none_or(|t| t.token != token) {
            return;
        }
        self.tracks.remove(id);
        self.stopping.remove(id);
        match exit {
            Ok(exit) if exit.crashed => {
                tracing::warn!(id, code = ?exit.code, "the game crashed");
                self.set_crash_log(id, exit.saved_log.clone(), cx);
                self.running.insert(id.to_string(), RunState::Exited(exit));
            }
            Ok(exit) => {
                tracing::info!(id, code = ?exit.code, "the game exited");
                self.running.remove(id);
            }
            Err(error) => {
                tracing::warn!(id, %error, "waiting for the game failed");
                self.running.remove(id);
            }
        }
        cx.notify();
    }

    /// Looks for the games of an earlier launcher session, once the list
    /// first loaded.
    pub(super) fn reattach_on_startup(&mut self, cx: &mut Context<Self>) {
        let instances: Vec<Instance> = (self.outcome.instances.iter())
            .filter(|i| !self.running.contains_key(&i.id))
            .cloned()
            .collect();
        let job = run_blocking(cx, move || reattach_all(instances));
        self.reattach_task = cx.spawn(async move |this, cx| {
            let found = match job.await {
                Ok(found) => found,
                Err(error) => {
                    tracing::warn!(%error, "looking for running games failed");
                    return;
                }
            };
            this.update(cx, |this, cx| {
                for (id, found) in found {
                    // Launched or found meanwhile: that one stands. A
                    // session attached here is dropped again, which keeps
                    // the game and its record.
                    if this.running.contains_key(&id) || this.tracks.contains_key(&id) {
                        continue;
                    }
                    let token = this.next_token();
                    // Registers the poll that `apply_found` checks for.
                    this.tracks.insert(
                        id.clone(),
                        Track {
                            token,
                            _task: Task::ready(()),
                        },
                    );
                    if this.apply_found(&id, token, Ok(found), cx) {
                        this.follow_game(&id, true, cx);
                    }
                }
            })
            .ok();
        });
    }

    /// Follows `id`'s game through its running record when this app holds
    /// no session for it: takes the session over when it is free, shows it
    /// as running elsewhere while another process holds it, and stops once
    /// it is gone. With `delay` the first look waits one poll interval.
    pub(super) fn follow_game(&mut self, id: &str, delay: bool, cx: &mut Context<Self>) {
        if matches!(self.running.get(id), Some(RunState::Running { .. })) {
            return;
        }
        let Some(Entry::Healthy(inst)) = self.entry(id) else {
            return;
        };
        let inst = inst.clone();
        let token = self.next_token();
        let key = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            if delay {
                cx.background_executor().timer(ELSEWHERE_POLL).await;
            }
            loop {
                let inst = inst.clone();
                let Ok(look) = this.update(cx, |_, cx| {
                    run_blocking(cx, move || GameSession::reattach(&inst))
                }) else {
                    return;
                };
                let found = look.await;
                let again = this.update(cx, |this, cx| this.apply_found(&key, token, found, cx));
                if !matches!(again, Ok(true)) {
                    return;
                }
                cx.background_executor().timer(ELSEWHERE_POLL).await;
            }
        });
        self.tracks
            .insert(id.to_string(), Track { token, _task: task });
    }

    /// Applies what a look for `id`'s game found; returns whether to look
    /// again later (the game runs elsewhere).
    fn apply_found(
        &mut self,
        id: &str,
        token: u64,
        found: Result<Reattach, BridgeError>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.tracks.get(id).is_none_or(|t| t.token != token) {
            // Stale: an attached session is dropped again, which keeps the
            // game running and its record in place.
            return false;
        }
        match found {
            Ok(Reattach::Attached(session)) => {
                tracing::info!(id, pid = session.pid(), "reattached to a running game");
                self.stopping.remove(id);
                self.drop_failed_launch(id);
                self.follow_session(id, session, true, cx);
                false
            }
            Ok(Reattach::HeldElsewhere(info)) => {
                if self.drop_failed_launch(id) {
                    cx.notify();
                }
                if !matches!(
                    self.running.get(id),
                    Some(RunState::HeldElsewhere { pid }) if *pid == info.pid
                ) {
                    let state = RunState::HeldElsewhere { pid: info.pid };
                    self.running.insert(id.to_string(), state);
                    cx.notify();
                }
                true
            }
            Ok(Reattach::NotRunning) | Err(_) => {
                if let Err(error) = found {
                    tracing::warn!(id, %error, "looking for a running game failed");
                }
                self.tracks.remove(id);
                self.stopping.remove(id);
                if matches!(self.running.get(id), Some(RunState::HeldElsewhere { .. })) {
                    self.running.remove(id);
                }
                cx.notify();
                false
            }
        }
    }

    /// Removes a failed launch from `id`'s header once its game turned out
    /// to run: the run status replaces it (retrying could only fail again).
    fn drop_failed_launch(&mut self, id: &str) -> bool {
        let failed_launch = matches!(
            self.op(id),
            Some(OpState::Failed {
                kind: OpKind::Launch,
                ..
            })
        );
        if failed_launch {
            self.ops.remove(id);
        }
        failed_launch
    }

    /// Force-stops `id`'s running game (SIGTERM, then SIGKILL; the core
    /// signals only that game). The exit that follows is not a crash. Ask
    /// first (`stop_dialog`).
    pub fn force_stop(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.stopping.contains_key(id) {
            return;
        }
        let stop = match self.running.get(id) {
            Some(RunState::Running { killer, .. }) => {
                let killer = killer.clone();
                run(cx, async move { killer.kill().await })
            }
            Some(RunState::HeldElsewhere { .. }) => {
                let Some(Entry::Healthy(inst)) = self.entry(id) else {
                    return;
                };
                run(cx, stop_elsewhere(inst.clone()))
            }
            _ => return,
        };
        let key = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let result = match stop.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |this, cx| this.stop_done(&key, result, cx))
                .ok();
        });
        self.stopping.insert(id.to_string(), task);
        cx.notify();
    }

    fn stop_done(&mut self, id: &str, result: Result<(), String>, cx: &mut Context<Self>) {
        let Err(error) = result else {
            // Stopping lasts until the game is seen gone.
            return;
        };
        tracing::warn!(id, %error, "force stop failed");
        self.stopping.remove(id);
        let token = self.next_token();
        let message = t!("launch.stop_failed", error = error).to_string();
        self.insert_failed(id, token, OpKind::Stop, message, None, None);
        cx.notify();
    }

    /// Removes the crash notice of `id`'s last run from the header (the
    /// log tab keeps its banner).
    pub fn dismiss_exit(&mut self, id: &str, cx: &mut Context<Self>) {
        if matches!(self.running.get(id), Some(RunState::Exited(_))) {
            self.running.remove(id);
            cx.notify();
        }
    }

    /// Applies the fix offered by `id`'s failed operation.
    pub fn apply_fix(&mut self, id: &str, cx: &mut Context<Self>) {
        let fix = match self.op(id) {
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
}
