//! Patina's desktop app (gpui-kit). `main.rs` runs the startup sequence;
//! this library holds the pieces so tests can boot the same app headless.

rust_i18n::i18n!("locales", fallback = "en");

pub mod assets;
pub mod core_bridge;
pub mod disk_usage;
pub mod fonts;
pub mod i18n;
pub mod instances;
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
/// font, the zoom keys, the shell's keys and the accounts-changed signal. Call once, after
/// `gpui_kit::init` and before [`open_main_window`].
pub fn init(settings: SettingsStore, cx: &mut App) {
    cx.set_global(settings);
    fonts::init(cx);
    scale::init(cx);
    shell::init(cx);
    instances::init(cx);
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

/// Opens the main window (1280×800, at least 960×600). Call after [`init`],
/// with the [`core_bridge::Core`] global set.
pub fn open_main_window(cx: &mut App) -> gpui_kit::Result<(AnyWindowHandle, Entity<RootView>)> {
    // Window geometry is in logical pixels by the platform's definition; the
    // content inside is rem-based and follows the UI scale.
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(t!("app.title").to_string().into()),
            ..Default::default()
        }),
        app_id: Some(APP_ID.into()),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1280.), px(800.)),
            cx,
        ))),
        window_min_size: Some(size(px(960.), px(600.))),
        ..Default::default()
    };
    gpui_kit::open_window(options, cx, |window, cx| {
        let ui_scale = SettingsStore::get(cx).ui_scale;
        style_window(ui_scale, window, cx);
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
