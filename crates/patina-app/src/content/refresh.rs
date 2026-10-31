//! What the content tabs do to the instance's files and folder: open the
//! folder, and run mutations under the instance lock.

use std::path::{Path, PathBuf};

use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::{App, Entity, Task, Window};
use patina_core::instance::InstanceStore;
use patina_core::locks::{BusyKind, InstanceGuard};
use rust_i18n::t;

use crate::core_bridge::{Core, run_blocking};
use crate::instances::InstancesModel;
use crate::launch::{RunState, busy_message};

/// Opens `dir` in the system file manager, creating it first when it does
/// not exist yet (a fresh instance has no `mods` folder). The creation runs
/// on the blocking pool.
pub fn open_folder(dir: PathBuf, window: &mut Window, cx: &mut App) {
    let open = open_folder_with(dir, cx, |dir, cx| cx.open_with_system(dir));
    window
        .spawn(cx, async move |cx| {
            if let Err(error) = open.await {
                cx.update(|window, cx| window.push_notification(Notification::error(error), cx))
                    .ok();
            }
        })
        .detach();
}

/// [`open_folder`] with the opener injected, for tests (the headless
/// platform cannot open anything). The returned task finishes after
/// `opener` ran, or returns the localized failure to show to the user.
pub fn open_folder_with(
    dir: PathBuf,
    cx: &mut App,
    opener: impl FnOnce(&Path, &mut App) + 'static,
) -> Task<Result<(), String>> {
    let create = {
        let dir = dir.clone();
        run_blocking(cx, move || std::fs::create_dir_all(&dir))
    };
    cx.spawn(async move |cx| match create.await {
        Ok(Ok(())) => {
            cx.update(|cx| opener(&dir, cx));
            Ok(())
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, dir = %dir.display(), "creating the folder failed");
            Err(t!("content.open_failed", path = dir.display(), error = error).to_string())
        }
        Err(error) => {
            Err(t!("content.open_failed", path = dir.display(), error = error).to_string())
        }
    })
}

/// Takes the lock of instance `id` on the calling (blocking) thread; when it
/// is held the localized reason names the instance by `name_of`. Use it
/// inside a `run_blocking` closure; most mutations want
/// [`with_instance_lock`].
pub fn lock_blocking(
    store: &InstanceStore,
    id: &str,
    name_of: impl Fn(&str) -> String,
) -> Result<InstanceGuard, String> {
    store
        .lock(id)
        .map_err(|busy| busy_message(&busy.what, name_of))
}

/// Why the app itself keeps instance `id` from being changed, or `None`.
fn busy_cause(model: &InstancesModel, id: &str, cx: &App) -> Option<BusyKind> {
    if model.is_op_running(id, cx) {
        return Some(BusyKind::Instance(id.to_string()));
    }
    match model.run_state(id, cx) {
        // The game is still being set up: an operation on the instance.
        Some(RunState::Starting) => Some(BusyKind::Instance(id.to_string())),
        Some(state) if state.is_busy() => Some(BusyKind::Running(id.to_string())),
        _ => None,
    }
}

/// Runs `mutate` (a change to the instance's files) on the blocking pool
/// while holding the instance's lock, so it cannot overlap a launch or an
/// operation, in this app or another Patina process.
///
/// Before that the app's own busy state is checked: an operation in flight
/// or a starting/running game refuses at once. The task then already holds
/// `Err(reason)` and nothing ran. Reasons are localized and name the
/// instance by its display name, ready to show in a notification. A lock
/// that is held elsewhere gives the same kind of reason from the blocking
/// side. `Err` also reports a panic of `mutate`.
///
/// Afterwards the caller reloads its tab (`ContentTab::mutated`).
pub fn with_instance_lock<T: Send + 'static>(
    model: &Entity<InstancesModel>,
    id: &str,
    cx: &mut App,
    mutate: impl FnOnce() -> T + Send + 'static,
) -> Task<Result<T, String>> {
    let name = model.read(cx).display_name(id);
    let name_of = move |id: &str| name.clone().unwrap_or_else(|| id.to_string());
    if let Some(what) = busy_cause(model.read(cx), id, cx) {
        return Task::ready(Err(busy_message(&what, name_of)));
    }
    let store = Core::global(cx).store.clone();
    let dir = Core::global(cx).data.instances().join(id);
    let id = id.to_string();
    let work = run_blocking(cx, move || {
        let _guard = lock_blocking(&store, &id, &name_of)?;
        patina_core::launch::running::refuse_while_running(&dir, &id)
            .map_err(|busy| busy_message(&busy.what, name_of))?;
        Ok(mutate())
    });
    cx.spawn(async move |_| match work.await {
        Ok(result) => result,
        Err(error) => Err(error.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::FutureExt as _;
    use gpui_kit::{
        AppContext as _, Context, IntoElement, ParentElement as _, Render, TestAppContext, Window,
        div,
    };
    use patina_core::paths::DataDirs;

    use super::*;
    use crate::core_bridge::testing::{self, FakeCatalog};
    use crate::launch::{OpError, OpKind};

    #[test]
    fn lock_blocking_names_the_instance_when_held() {
        let dir = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(dir.path());
        let held = store.lock("pack-1").unwrap();
        let reason = lock_blocking(&store, "pack-1", |_| "My pack".into()).unwrap_err();
        assert!(reason.contains("My pack"), "{reason}");
        drop(held);
        assert!(lock_blocking(&store, "pack-1", |_| String::new()).is_ok());
    }

    struct Host {
        model: Entity<InstancesModel>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child("")
        }
    }

    struct Fixture {
        _rt: tokio::runtime::Runtime,
        _dir: tempfile::TempDir,
        store: InstanceStore,
        id: String,
        model: Entity<InstancesModel>,
    }

    async fn fixture(cx: &mut TestAppContext) -> Fixture {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = testing::core(
            rt.handle().clone(),
            DataDirs::new(dir.path()),
            Arc::new(FakeCatalog::new().with_manifest_error("offline")),
        );
        let store = core.store.clone();
        let id = store.create("My pack", "1.21").unwrap().id;
        cx.update(|cx| cx.set_global(core));
        cx.executor().allow_parking();
        let window = cx.add_window(|window, cx| Host {
            model: cx.new(|cx| InstancesModel::new(window, cx)),
        });
        let model = window.read_with(cx, |h, _| h.model.clone()).unwrap();
        model.update(cx, |m, cx| m.refresh_and_wait(cx)).await;
        Fixture {
            _rt: rt,
            _dir: dir,
            store,
            id,
            model,
        }
    }

    #[gpui_kit::test]
    #[allow(clippy::await_holding_lock)] // the locale must not change mid-test
    async fn busy_instance_refuses_with_its_display_name(cx: &mut TestAppContext) {
        let _locale = crate::i18n::locale_lock();
        crate::i18n::set_language(crate::i18n::Language::En);
        let f = fixture(cx).await;
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // A running game (held by another process).
        f.model.update(cx, |m, cx| {
            m.set_run_state(&f.id, Some(RunState::HeldElsewhere { pid: 0 }), cx)
        });
        let flag = ran.clone();
        let reason = cx
            .update(|cx| {
                with_instance_lock(&f.model, &f.id, cx, move || {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst)
                })
            })
            .await
            .unwrap_err();
        assert!(
            reason.contains("My pack") && reason.contains("stop the game"),
            "{reason}"
        );
        f.model.update(cx, |m, cx| m.set_run_state(&f.id, None, cx));

        // An operation in flight.
        f.model.update(cx, |m, cx| {
            m.start_op(
                &f.id,
                OpKind::ChangeVersion,
                |_| futures::future::pending::<Result<(), OpError>>().boxed(),
                |_, _, _| {},
                None,
                cx,
            )
        });
        let flag = ran.clone();
        let result = cx
            .update(|cx| {
                with_instance_lock(&f.model, &f.id, cx, move || {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst)
                })
            })
            .await;
        let reason = result.unwrap_err();
        assert!(
            reason.contains("My pack") && reason.contains("another operation"),
            "{reason}"
        );

        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        // Nothing took the lock for the refused calls.
        drop(f.store.lock(&f.id).expect("the lock is free"));
    }

    #[gpui_kit::test]
    async fn mutation_holds_the_lock_and_a_panic_releases_it(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        // While it runs the lock is held.
        let (store, id) = (f.store.clone(), f.id.clone());
        let held = cx
            .update(|cx| with_instance_lock(&f.model, &f.id, cx, move || store.lock(&id).is_err()))
            .await;
        assert_eq!(held, Ok(true));
        assert!(f.store.lock(&f.id).is_ok());

        // A panicking mutation is an Err, and the lock is free afterwards.
        let result = cx
            .update(|cx| {
                with_instance_lock(&f.model, &f.id, cx, || -> () { panic!("mutation failed") })
            })
            .await;
        assert!(result.is_err());
        assert!(f.store.lock(&f.id).is_ok());
    }
}
