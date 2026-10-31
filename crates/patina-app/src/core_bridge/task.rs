//! Moving work between gpui and the app's tokio runtime.

use std::future::Future;
use std::time::Duration;

use gpui_kit::{App, AppContext as _, Context, Task, WeakEntity};
use rust_i18n::t;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::task::{AbortHandle, JoinHandle};

use super::Core;

/// Shortest time between two progress updates of a view (about 30 a second).
pub const PROGRESS_MIN_GAP: Duration = Duration::from_millis(33);

/// Why bridged work produced no result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeError {
    /// The work panicked (the panic hook has logged where).
    Panicked,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Panicked => f.write_str(&t!("core_bridge.panicked")),
        }
    }
}

impl std::error::Error for BridgeError {}

/// Runs `fut` on the app's tokio runtime and hands its output back to gpui,
/// or [`BridgeError::Panicked`] if it panicked.
///
/// Dropping the returned task aborts the work at its next await. If the
/// runtime is shut down (the app is quitting) the task never completes;
/// that is logged.
pub fn run<T: Send + 'static>(
    cx: &mut App,
    fut: impl Future<Output = T> + Send + 'static,
) -> Task<Result<T, BridgeError>> {
    let work = Core::global(cx).rt.spawn(fut);
    bring_back(cx, work)
}

/// Runs the blocking `f` on the runtime's blocking pool and hands its result
/// back to gpui. Use it for every blocking core call. As [`run`], except that
/// dropping the task cannot stop `f` once it has started.
pub fn run_blocking<T: Send + 'static>(
    cx: &mut App,
    f: impl FnOnce() -> T + Send + 'static,
) -> Task<Result<T, BridgeError>> {
    let work = Core::global(cx).rt.spawn_blocking(f);
    bring_back(cx, work)
}

/// Awaits `work` on gpui's background executor; aborts it if the gpui task
/// is dropped first.
fn bring_back<T: Send + 'static>(cx: &App, work: JoinHandle<T>) -> Task<Result<T, BridgeError>> {
    let abort = AbortOnDrop(work.abort_handle());
    cx.background_spawn(async move {
        let _abort = abort;
        match work.await {
            Ok(value) => Ok(value),
            Err(e) if e.is_panic() => {
                tracing::error!("core work panicked");
                Err(BridgeError::Panicked)
            }
            Err(_) => {
                // The runtime is shut down, so the app is quitting and nobody
                // needs the result. Pending tasks never hold up quitting.
                tracing::warn!("core work cancelled: the runtime is shut down");
                std::future::pending().await
            }
        }
    })
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        // A no-op once the work has finished.
        self.0.abort();
    }
}

/// Feeds the progress from `rx` to `entity` through `on`: only the newest
/// value, at most once every [`PROGRESS_MIN_GAP`], and always the last value
/// sent before the channel closed. `on` notifies if the view changed. Ends
/// when the channel closes or the entity is gone.
///
/// Store the returned task in the entity (replacing the previous one), not
/// `.detach()` it: dropping the task stops the forwarding, so it then ends
/// with the entity or when a newer operation replaces it.
pub fn forward_progress<E: 'static, P: 'static>(
    cx: &mut App,
    entity: WeakEntity<E>,
    mut rx: UnboundedReceiver<P>,
    on: impl Fn(&mut E, P, &mut Context<E>) + 'static,
) -> Task<()> {
    cx.spawn(async move |cx| {
        while let Some(mut latest) = rx.recv().await {
            // Everything queued meanwhile is superseded by its last value.
            let closed = loop {
                match rx.try_recv() {
                    Ok(value) => latest = value,
                    Err(TryRecvError::Empty) => break false,
                    Err(TryRecvError::Disconnected) => break true,
                }
            };
            if entity.update(cx, |e, cx| on(e, latest, cx)).is_err() || closed {
                return;
            }
            cx.background_executor().timer(PROGRESS_MIN_GAP).await;
        }
    })
}

/// Request numbering for async results. Take an id with [`Revision::start`]
/// when starting a request and apply its result only if
/// [`Revision::is_current`] still holds when it arrives: a newer request
/// makes every older result stale.
#[derive(Debug, Default)]
pub struct Revision(u64);

/// The id of one request, from [`Revision::start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevisionId(u64);

impl Revision {
    /// Starts a new request; results of earlier ones are stale from now on.
    pub fn start(&mut self) -> RevisionId {
        self.0 += 1;
        RevisionId(self.0)
    }

    pub fn is_current(&self, id: RevisionId) -> bool {
        id.0 == self.0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread::{self, ThreadId};

    use gpui_kit::{Entity, TestAppContext};
    use patina_core::paths::DataDirs;
    use patina_core::progress::{Phase, TaskProgress, Unit};
    use tokio::sync::oneshot;

    use super::*;
    use crate::core_bridge::testing::{self, FakeCatalog};

    /// A runtime of the app's kind and a `Core` on it, over an empty data
    /// dir. Keep both alive for the test.
    fn start_core(cx: &mut TestAppContext) -> (tokio::runtime::Runtime, tempfile::TempDir) {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = testing::core(
            rt.handle().clone(),
            DataDirs::new(dir.path()),
            Arc::new(FakeCatalog::new()),
        );
        cx.update(|cx| cx.set_global(core));
        // Results come back from the runtime's threads, not the test's.
        cx.executor().allow_parking();
        (rt, dir)
    }

    #[derive(Default)]
    struct Probe {
        value: Option<u64>,
        applied_on: Option<ThreadId>,
        rev: Revision,
        updates: usize,
        last: Option<TaskProgress>,
    }

    #[gpui_kit::test]
    async fn run_returns_on_gpui(cx: &mut TestAppContext) {
        let (_rt, _dir) = start_core(cx);
        let probe = cx.new(|_| Probe::default());
        let task = probe.update(cx, |_, cx| {
            let work = run(cx, async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                // Awaiting a tokio timer proves this is the runtime.
                (thread::current().name() == Some(crate::runtime::THREAD_NAME)) as u64
            });
            cx.spawn(async move |this, cx| {
                let value = work.await.unwrap();
                this.update(cx, |p, cx| {
                    p.value = Some(value);
                    p.applied_on = Some(thread::current().id());
                    cx.notify();
                })
                .unwrap();
            })
        });
        task.await;
        probe.read_with(cx, |p, _| {
            assert_eq!(p.value, Some(1), "ran on a runtime worker");
            assert_eq!(
                p.applied_on,
                Some(thread::current().id()),
                "applied on gpui"
            );
        });
    }

    #[gpui_kit::test]
    async fn run_blocking_runs_off_main_thread(cx: &mut TestAppContext) {
        let (_rt, _dir) = start_core(cx);
        let gpui_thread = cx.update(|_| thread::current().id());
        let task = cx.update(|cx| run_blocking(cx, || thread::current().id()));
        assert_ne!(task.await.unwrap(), gpui_thread);
    }

    #[gpui_kit::test]
    fn run_after_shutdown_never_completes(cx: &mut TestAppContext) {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = testing::core(
            rt.handle().clone(),
            DataDirs::new(dir.path()),
            Arc::new(FakeCatalog::new()),
        );
        cx.update(|cx| cx.set_global(core));
        rt.shutdown_timeout(Duration::from_secs(2));

        let probe = cx.new(|_| Probe::default());
        let finish = |task: Task<Result<u64, BridgeError>>,
                      probe: &Entity<Probe>,
                      cx: &mut TestAppContext| {
            probe.update(cx, |_, cx| {
                cx.spawn(async move |this, cx| {
                    let value = task.await.unwrap();
                    this.update(cx, |p, _| p.value = Some(value)).ok();
                })
                .detach()
            })
        };
        let task = cx.update(|cx| run(cx, async { 1 }));
        finish(task, &probe, cx);
        let task = cx.update(|cx| run_blocking(cx, || 2));
        finish(task, &probe, cx);
        cx.run_until_parked();
        probe.read_with(cx, |p, _| assert_eq!(p.value, None));
    }

    #[gpui_kit::test]
    async fn panicking_work_reports_panicked(cx: &mut TestAppContext) {
        let (_rt, _dir) = start_core(cx);
        let task = cx.update(|cx| run(cx, async { panic!("deliberate test panic") }));
        assert_eq!(task.await, Err::<(), _>(BridgeError::Panicked));
        let task = cx.update(|cx| run_blocking(cx, || panic!("deliberate test panic")));
        assert_eq!(task.await, Err::<(), _>(BridgeError::Panicked));
        // The runtime is still usable afterwards.
        let task = cx.update(|cx| run(cx, async { 7 }));
        assert_eq!(task.await, Ok(7));
    }

    fn progress(done: u64) -> TaskProgress {
        TaskProgress {
            phase: Phase::DownloadGameFiles,
            done,
            total: 1000,
            unit: Unit::Items,
        }
    }

    #[gpui_kit::test]
    fn progress_is_coalesced(cx: &mut TestAppContext) {
        let probe = cx.new(|_| Probe::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _forward = cx.update(|cx| {
            forward_progress(cx, probe.downgrade(), rx, |p: &mut Probe, value, cx| {
                p.updates += 1;
                p.last = Some(value);
                cx.notify();
            })
        });
        let updates = |cx: &mut TestAppContext| probe.read_with(cx, |p, _| p.updates);

        // 1000 values over 100 ms.
        for done in 1..=1000 {
            tx.send(progress(done)).unwrap();
            if done == 1 {
                cx.run_until_parked();
                assert_eq!(updates(cx), 1, "the first value shows at once");
            }
            if done % 10 == 0 {
                cx.executor().advance_clock(Duration::from_millis(1));
                cx.run_until_parked();
            }
        }
        drop(tx);
        cx.executor().advance_clock(PROGRESS_MIN_GAP);
        cx.run_until_parked();

        let (count, last) = probe.read_with(cx, |p, _| (p.updates, p.last));
        assert!((2..=10).contains(&count), "{count} updates");
        assert_eq!(last, Some(progress(1000)), "the last value is delivered");
    }

    #[gpui_kit::test]
    fn progress_stops_with_the_entity(cx: &mut TestAppContext) {
        let probe = cx.new(|_| Probe::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let forward = cx.update(|cx| {
            forward_progress(cx, probe.downgrade(), rx, |p: &mut Probe, value, _| {
                p.last = Some(value)
            })
        });
        drop(probe);
        cx.run_until_parked();
        tx.send(progress(1)).unwrap();
        cx.run_until_parked();
        // The forwarder has ended: the task is ready.
        assert!(forward.is_ready());
    }

    /// Starts a request whose result is whatever `answer` receives, applied
    /// only while the request is current.
    fn request(
        probe: &Entity<Probe>,
        answer: oneshot::Receiver<u64>,
        cx: &mut TestAppContext,
    ) -> Task<()> {
        probe.update(cx, |p, cx| {
            let id = p.rev.start();
            let work = run(cx, async move { answer.await.unwrap() });
            cx.spawn(async move |this, cx| {
                let value = work.await.unwrap();
                this.update(cx, |p, cx| {
                    if p.rev.is_current(id) {
                        p.value = Some(value);
                        cx.notify();
                    }
                })
                .unwrap();
            })
        })
    }

    #[gpui_kit::test]
    async fn stale_result_is_dropped(cx: &mut TestAppContext) {
        let (_rt, _dir) = start_core(cx);
        let probe = cx.new(|_| Probe::default());
        let (answer_old, old) = oneshot::channel();
        let (answer_new, new) = oneshot::channel();
        let old = request(&probe, old, cx);
        let new = request(&probe, new, cx);

        // The old request answers last-but-one: superseded, dropped.
        answer_old.send(1).unwrap();
        old.await;
        probe.read_with(cx, |p, _| assert_eq!(p.value, None));

        answer_new.send(2).unwrap();
        new.await;
        probe.read_with(cx, |p, _| assert_eq!(p.value, Some(2)));
    }

    #[test]
    fn revision_makes_older_ids_stale() {
        let mut rev = Revision::default();
        let a = rev.start();
        assert!(rev.is_current(a));
        let b = rev.start();
        assert!(!rev.is_current(a));
        assert!(rev.is_current(b));
        assert_ne!(a, b);
    }
}
