//! Whole-UI scaling, `Ctrl +` / `Ctrl -` / `Ctrl 0` (80 % to 200 %).
//!
//! Same mechanism as crypto_pro's `scale.rs`: gpui scales every rem-based
//! length by the window's rem size, and gpui-component's root sets the rem
//! size from `Theme::font_size` before each frame. So scaling sets the
//! theme's font sizes to their unscaled base times the scale. Everything in
//! this UI is sized in rems so it follows; the few `px` values are 1–3 px
//! hairlines (dividers, the tab underline, progress lines).

use gpui_kit::component::Theme;
use gpui_kit::{App, Global, KeyBinding, Pixels, actions};

actions!(patina, [ZoomIn, ZoomOut, ZoomReset]);

pub const UI_SCALES: [f32; 8] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];
const DEFAULT_INDEX: usize = 2;

struct UiScale {
    index: usize,
    base_font: Pixels,
    base_mono: Pixels,
}

impl Global for UiScale {}

fn set_index(index: usize, cx: &mut App) {
    if !cx.has_global::<UiScale>() {
        let t = Theme::global(cx);
        let base = UiScale {
            index: DEFAULT_INDEX,
            base_font: t.font_size,
            base_mono: t.mono_font_size,
        };
        cx.set_global(base);
    }
    let ui = cx.global_mut::<UiScale>();
    ui.index = index.min(UI_SCALES.len() - 1);
    let k = UI_SCALES[ui.index];
    let (font, mono) = (ui.base_font * k, ui.base_mono * k);
    Theme::update(cx, |t| {
        t.font_size = font;
        t.mono_font_size = mono;
    });
    cx.refresh_windows();
}

fn index(cx: &App) -> usize {
    cx.try_global::<UiScale>()
        .map_or(DEFAULT_INDEX, |s| s.index)
}

/// Binds the zoom keys. `PATINA_SCALE` (e.g. `1.25`) sets the start scale,
/// handy for screenshots.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-=", ZoomIn, None),
        KeyBinding::new("ctrl-+", ZoomIn, None),
        KeyBinding::new("ctrl--", ZoomOut, None),
        KeyBinding::new("ctrl-0", ZoomReset, None),
    ]);
    cx.on_action(|_: &ZoomIn, cx| set_index(index(cx) + 1, cx));
    cx.on_action(|_: &ZoomOut, cx| set_index(index(cx).saturating_sub(1), cx));
    cx.on_action(|_: &ZoomReset, cx| set_index(DEFAULT_INDEX, cx));
}

/// Applies `PATINA_SCALE` if set (snapped to the nearest step).
pub fn apply_env(cx: &mut App) {
    let Some(s) = std::env::var("PATINA_SCALE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
    else {
        return;
    };
    let nearest = (0..UI_SCALES.len())
        .min_by(|&a, &b| {
            (UI_SCALES[a] - s)
                .abs()
                .total_cmp(&(UI_SCALES[b] - s).abs())
        })
        .unwrap_or(DEFAULT_INDEX);
    set_index(nearest, cx);
}
