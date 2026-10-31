//! Patina, the desktop app. Startup order matters; see the steps below.

use std::time::Duration;

use patina_app::assets::AppAssets;
use patina_app::settings::{self, SettingsStore};
use patina_app::single_instance::{self, AlreadyRunning};
use patina_app::{i18n, logging, runtime};
use patina_core::paths::DataDirs;

/// Temp files older than this are leftovers of a crashed run.
const TEMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Longest the runtime may take to wind down after the last window closed.
/// A blocking task still running then is abandoned rather than keeping a
/// windowless process (and its single-instance lock) alive.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(2);

fn main() {
    let data = DataDirs::from_env();

    // Before logging: a second instance must not touch the first one's log.
    let guard = match single_instance::acquire(&data) {
        Ok(guard) => guard,
        Err(AlreadyRunning) => return show_already_running(&data),
    };

    logging::init(&data);
    if let Some(e) = guard.unguarded_reason() {
        tracing::warn!(error = %e, "single-instance lock unavailable; running unguarded");
    }

    let settings = settings::load(&data.root());
    i18n::set_language(settings.language);

    let rt = runtime::start();
    let root = data.root();
    rt.spawn_blocking(
        move || match patina_core::fsutil::sweep_owned_temps(&root, TEMP_MAX_AGE) {
            Ok(0) => {}
            Ok(n) => tracing::info!(removed = n, "swept stale temp files"),
            Err(e) => tracing::warn!(error = %e, "temp sweep failed"),
        },
    );

    let store = SettingsStore::new(data.root(), settings);
    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            patina_app::init(store, cx);
            patina_app::open_main_window(cx).expect("failed to open the main window");
            patina_app::quit_with_last_window(cx);
            cx.activate(true);
        });

    // Only after gpui has returned: views may still have had work on it.
    rt.shutdown_timeout(RUNTIME_SHUTDOWN);
    // Last: the next Patina may start once this one is done with the data dir.
    drop(guard);
}

/// The second-instance path: say that Patina is already running, then quit.
/// Reads the settings for language and scale but changes nothing on disk.
fn show_already_running(data: &DataDirs) {
    let settings = settings::peek(&data.root());
    i18n::set_language(settings.language);
    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            patina_app::fonts::init(cx);
            single_instance::open_notice(settings.ui_scale, cx)
                .expect("failed to open the notice window");
            patina_app::quit_with_last_window(cx);
            cx.activate(true);
        });
}
