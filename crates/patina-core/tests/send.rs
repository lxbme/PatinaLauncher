//! Compile-time checks that the core's long-running operations produce
//! `Send` futures, so the app can run them on its multi-threaded runtime
//! (and abort them by dropping the task). A regression fails to compile.

use patina_core::config::GlobalConfig;
use patina_core::instance::{Instance, InstanceStore};
use patina_core::launch::LaunchAccount;
use patina_core::launch::session::GameSession;
use patina_core::loader::ops::{install_loader, reinstall_loader, set_mc_version};
use patina_core::loader::{LoaderKind, VersionPick};
use patina_core::prepare::Launcher;
use patina_core::progress::ProgressSink;

fn assert_send<T: Send>(_: &T) {}

/// Never called: type-checking the body is the test.
#[allow(dead_code)]
fn loader_ops_are_send(launcher: &Launcher, store: &InstanceStore, progress: ProgressSink) {
    let install = install_loader(
        launcher,
        store,
        "id",
        LoaderKind::Forge,
        VersionPick::Exact("1".into()),
        false,
        progress.clone(),
    );
    assert_send(&install);
    assert_send(&reinstall_loader(launcher, store, "id", progress.clone()));
    assert_send(&set_mc_version(
        launcher, store, "id", "1.21.1", None, progress,
    ));
}

/// Never called: type-checking the body is the test.
#[allow(dead_code)]
fn prepare_launch_is_send(
    launcher: &Launcher,
    inst: &Instance,
    global: &GlobalConfig,
    account: &LaunchAccount,
    progress: ProgressSink,
) {
    assert_send(&GameSession::prepare_launch(
        launcher, inst, global, account, progress,
    ));
}

#[test]
fn core_operation_futures_are_send() {
    // The checks above run at compile time; this keeps the file a test.
    let _ = (
        loader_ops_are_send as fn(_, _, _),
        prepare_launch_is_send as fn(_, _, _, _, _),
    );
}
