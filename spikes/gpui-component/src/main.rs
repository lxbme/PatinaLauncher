//! Patina UI prototype (layout A in crypto_pro's visual language). Static
//! fake data; see `ui/` for one module per window region.
//!
//! `PATINA_TAB=log` opens on the 日志 tab and `PATINA_SCALE=1.25` starts
//! scaled (both for screenshots); `Ctrl +` / `Ctrl -` / `Ctrl 0` zoom.

mod assets;
mod data;
mod scale;
mod state;
mod theme;
mod ui;

use gpui_kit::*;

fn main() {
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            scale::init(cx);
            ui::root::init(cx);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Patina".into()),
                    ..Default::default()
                }),
                app_id: Some("patina-proto".into()),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(800.)),
                    cx,
                ))),
                window_min_size: Some(size(px(960.), px(600.))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                // Before the first frame, so the window never flashes the
                // default theme or scale.
                theme::apply(window, cx);
                scale::apply_env(cx);
                cx.new(|cx| ui::root::RootView::new(window, cx))
            })
            .expect("failed to open window");
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}
