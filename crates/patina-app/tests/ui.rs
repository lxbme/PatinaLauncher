//! UI integration tests: the real app bootstrap in headless windows.

use gpui_kit::component::Theme;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, Pixels, TestAppContext, px};
use patina_app::settings::{self, AppSettings, SettingsStore};
use patina_app::single_instance;
use patina_app::theme::{self, ProductColors};

/// Boots the app as `main` does, with its settings in `dir`, and opens the
/// main window.
fn boot(cx: &mut TestAppContext, dir: &std::path::Path, start: AppSettings) -> AnyWindowHandle {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        patina_app::init(SettingsStore::new(dir.to_path_buf(), start), cx);
        patina_app::open_main_window(cx)
            .expect("open the main window")
            .0
    })
}

fn font_size(cx: &mut TestAppContext) -> Pixels {
    cx.update(|cx| Theme::global(cx).font_size)
}

#[gpui_kit::test]
fn app_boots_headless(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let handle = boot(cx, tmp.path(), AppSettings::default());
    cx.update_window(handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update(|cx| {
        let t = Theme::global(cx);
        assert!(t.mode.is_dark(), "Patina has a dark theme only");
        // Patina's tokens, not the stock dark theme's.
        assert_eq!(t.background, theme::background());
        assert_eq!(t.progress_bar, ProductColors::get(cx).signature());
        assert_eq!(cx.windows().len(), 1);
    });
}

#[gpui_kit::test]
fn zoom_keys_scale_the_ui_and_persist(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let mut start = AppSettings::default();
    start.ui_scale = 1.25;
    let handle = boot(cx, tmp.path(), start);
    // The saved scale is applied before the first frame.
    let base = font_size(cx) / 1.25;
    assert_eq!(base, px(16.));

    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(handle, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    };
    let scale = |cx: &mut TestAppContext| cx.update(|cx| SettingsStore::get(cx).ui_scale);

    press(cx, "ctrl-=");
    assert_eq!(scale(cx), 1.5);
    assert_eq!(font_size(cx), base * 1.5);

    press(cx, "ctrl-0");
    assert_eq!(scale(cx), 1.0);
    assert_eq!(font_size(cx), base);

    press(cx, "ctrl--");
    press(cx, "ctrl--");
    press(cx, "ctrl--");
    assert_eq!(scale(cx), 0.8, "clamped at the smallest step");
    assert_eq!(font_size(cx), base * 0.8);

    press(cx, "ctrl-+");
    assert_eq!(scale(cx), 0.9);

    // Quitting (which runs the app's quit handlers) writes the change.
    cx.quit();
    assert_eq!(settings::load(tmp.path()).ui_scale, 0.9);
}

/// Opens the already-running notice as the second instance does.
fn open_notice(cx: &mut TestAppContext) -> AnyWindowHandle {
    cx.update(gpui_kit::init);
    let handle = cx.update(|cx| {
        patina_app::fonts::init(cx);
        single_instance::open_notice(1.0, cx)
            .expect("open the notice")
            .0
    });
    cx.update_window(handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    handle
}

fn window_count(cx: &mut TestAppContext) -> usize {
    cx.run_until_parked();
    cx.update(|cx| cx.windows().len())
}

#[gpui_kit::test]
fn already_running_notice_closes_on_click(cx: &mut TestAppContext) {
    let handle = open_notice(cx);
    cx.update_window(handle, |_, window, cx| {
        let close = window.find("close");
        assert!(close.visible());
        assert!(close.bounds().size.width > px(0.));
        window.click("close", cx);
    })
    .unwrap();
    assert_eq!(window_count(cx), 0);
}

#[gpui_kit::test]
fn already_running_notice_closes_on_escape(cx: &mut TestAppContext) {
    let handle = open_notice(cx);
    // Other keys leave it open.
    cx.update_window(handle, |_, window, cx| window.press("a", cx))
        .unwrap();
    assert_eq!(window_count(cx), 1);
    // The notice has focus from the start: no click before the key.
    cx.update_window(handle, |_, window, cx| window.press("escape", cx))
        .unwrap();
    assert_eq!(window_count(cx), 0);
}

#[gpui_kit::test]
fn already_running_notice_closes_on_enter(cx: &mut TestAppContext) {
    let handle = open_notice(cx);
    cx.update_window(handle, |_, window, cx| window.press("enter", cx))
        .unwrap();
    assert_eq!(window_count(cx), 0);
}
