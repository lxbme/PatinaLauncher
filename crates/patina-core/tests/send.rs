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

/// Never called: account operations must remain runtime-spawnable.
#[allow(dead_code)]
fn account_operations_are_send(
    service: &patina_core::accounts::AccountService,
    cancel: patina_core::accounts::login::CancelToken,
    grant: patina_core::accounts::LaunchGrant,
    pending: patina_core::launch::session::PendingLaunch,
) {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<patina_core::accounts::AccountService>();
    let _: &dyn patina_core::accounts::login::DeviceCodeLogin = service;
    assert_send(&service.snapshot());
    assert_send(&service.reload());
    assert_send(&service.set_current("id"));
    assert_send(&service.remove("id"));
    assert_send(&service.add_offline("Player"));
    assert_send(&service.resolve_launch("id", cancel.clone()));
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    assert_send(&patina_core::accounts::login::DeviceCodeLogin::start(
        service, events, cancel,
    ));
    assert_send(&service.launch_pending(grant, pending, None));
    #[cfg(feature = "dev-offline")]
    assert_send(&service.add_dev_offline("Player"));
}

#[test]
fn core_operation_futures_are_send() {
    // The checks above run at compile time; this keeps the file a test.
    let _ = (
        loader_ops_are_send as fn(_, _, _),
        prepare_launch_is_send as fn(_, _, _, _, _),
        account_operations_are_send as fn(_, _, _, _),
    );
}
