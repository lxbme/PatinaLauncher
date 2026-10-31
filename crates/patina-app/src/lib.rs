//! Patina's desktop app (gpui-kit). `main.rs` runs the startup sequence;
//! this library holds the pieces so tests can boot the same app headless.

rust_i18n::i18n!("locales", fallback = "en");

pub mod assets;
pub mod content;
pub mod core_bridge;
pub mod disk_usage;
pub mod fonts;
pub mod i18n;
pub mod instances;
pub mod launch;
pub mod layout;
pub mod logging;
pub mod notice;
pub mod platform;
pub mod runtime;
pub mod scale;
pub mod settings;
pub mod shell;
pub mod single_instance;
pub mod theme;

use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Entity, TitlebarOptions, Window, WindowBounds,
    WindowOptions, px, size,
};
use rust_i18n::t;

use crate::settings::SettingsStore;
use crate::shell::RootView;

// Offline accounts exist only for development (`cargo app`); a build without
// the feature must not get them from patina-core through feature unification.
#[cfg(not(feature = "dev-offline"))]
const _: () = assert!(!patina_core::DEV_OFFLINE);

/// Application id (Wayland app id / X11 WM class) of every Patina window.
pub const APP_ID: &str = "patina";

/// Sets up the app-wide state the windows rely on: the settings, the CJK
/// font, the zoom keys, the shell's, instances page's and log's keys and
/// the accounts-changed signal. Call once, after
/// `gpui_kit::init` and before [`open_main_window`].
pub fn init(settings: SettingsStore, cx: &mut App) {
    cx.set_global(settings);
    fonts::init(cx);
    scale::init(cx);
    shell::init(cx);
    instances::init(cx);
    content::init(cx);
    launch::log_view::init(cx);
    core_bridge::AccountsChanged::init(cx);
    cx.on_app_quit(|cx| {
        SettingsStore::flush(cx);
        async {}
    })
    .detach();
}

/// Puts the theme and the UI scale on the app before a window's first frame
/// (see `theme::apply`).
pub(crate) fn style_window(ui_scale: f32, window: &mut Window, cx: &mut App) {
    theme::apply(window, cx);
    scale::apply_scale(ui_scale, cx);
}

/// The main window's options at `ui_scale`: 1280×800, at least the
/// scale's [`scale::min_window_size`] (960×600 at 100 %).
pub fn main_window_options(ui_scale: f32, cx: &App) -> WindowOptions {
    // Window geometry is in logical pixels by the platform's definition; the
    // content inside is rem-based and follows the UI scale, so the minimum
    // grows with it.
    let min = scale::min_window_size(ui_scale);
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(t!("app.title").to_string().into()),
            ..Default::default()
        }),
        app_id: Some(APP_ID.into()),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1280.), px(800.)).max(&min),
            cx,
        ))),
        window_min_size: Some(min),
        ..Default::default()
    }
}

/// Opens the main window (see [`main_window_options`]) at the saved UI
/// scale, lowered to the largest step that fits the display (the saved
/// setting is kept). Call after [`init`], with the
/// [`core_bridge::Core`] global set.
pub fn open_main_window(cx: &mut App) -> gpui_kit::Result<(AnyWindowHandle, Entity<RootView>)> {
    let ui_scale = scale::startup_scale(SettingsStore::get(cx).ui_scale, cx);
    let options = main_window_options(ui_scale, cx);
    gpui_kit::open_window(options, cx, move |window, cx| {
        style_window(ui_scale, window, cx);
        scale::follow_window(window, cx);
        cx.new(|cx| RootView::new(window, cx))
    })
}

/// Quits once the last window has closed.
pub fn quit_with_last_window(cx: &mut App) {
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    #[test]
    fn release_build_has_no_offline() {
        // The const assertion above covers builds without the feature; this
        // also checks the feature still reaches the core when enabled.
        assert_eq!(patina_core::DEV_OFFLINE, cfg!(feature = "dev-offline"));
    }
}
