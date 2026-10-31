//! Whole-UI zoom (`Ctrl +` / `Ctrl -` / `Ctrl 0`), 80 % to 200 %.
//!
//! gpui scales every rem-based length (text sizes, `h_8()`, paddings, ...) by
//! the window's rem size, and gpui-component's root sets each window's rem
//! size from `Theme::font_size` before every frame (calling
//! `Window::set_rem_size` directly would be undone on the next frame). So
//! zoom goes through the theme: [`apply_scale`] sets `Theme::font_size` and
//! `mono_font_size` to their unscaled base times the scale. Size the UI in
//! rems and it follows; custom-painted views derive pixel sizes from
//! `window.rem_size()`.

use gpui_kit::component::Theme;
use gpui_kit::{App, Global, KeyBinding, Pixels, actions};

use crate::settings::SettingsStore;

actions!(patina, [ZoomIn, ZoomOut, ZoomReset]);

/// The zoom steps; every stored or requested scale snaps to one of these.
pub const UI_SCALES: [f32; 8] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];
pub const DEFAULT_SCALE: f32 = 1.0;

/// Index of the step nearest to `s`; non-finite input → the default step.
fn nearest_index(s: f32) -> usize {
    let default = UI_SCALES
        .iter()
        .position(|&x| x == DEFAULT_SCALE)
        .expect("the default scale is a step");
    if !s.is_finite() {
        return default;
    }
    let mut best = 0;
    for (i, &x) in UI_SCALES.iter().enumerate() {
        if (x - s).abs() < (UI_SCALES[best] - s).abs() {
            best = i;
        }
    }
    best
}

/// Nearest step of [`UI_SCALES`]; non-finite input → [`DEFAULT_SCALE`].
pub fn snap_scale(s: f32) -> f32 {
    UI_SCALES[nearest_index(s)]
}

/// One step larger, clamped at the last step (unknown values snap first).
pub fn next_scale(s: f32) -> f32 {
    UI_SCALES[(nearest_index(s) + 1).min(UI_SCALES.len() - 1)]
}

/// One step smaller, clamped at the first step (unknown values snap first).
pub fn prev_scale(s: f32) -> f32 {
    UI_SCALES[nearest_index(s).saturating_sub(1)]
}

/// The theme's unscaled font sizes, captured on the first [`apply_scale`].
struct UnscaledFonts {
    font: Pixels,
    mono: Pixels,
}

impl Global for UnscaledFonts {}

/// Scales the whole UI of every window to `scale` (snapped to a step).
///
/// Call after `theme::apply`: the first call takes the theme's font sizes
/// as the unscaled base.
pub fn apply_scale(scale: f32, cx: &mut App) {
    let scale = snap_scale(scale);
    if !cx.has_global::<UnscaledFonts>() {
        let theme = Theme::global(cx);
        let base = UnscaledFonts {
            font: theme.font_size,
            mono: theme.mono_font_size,
        };
        cx.set_global(base);
    }
    let base = cx.global::<UnscaledFonts>();
    let (font, mono) = (base.font * scale, base.mono * scale);
    // `update` refreshes every window, so the new rem size shows at once.
    Theme::update(cx, |t| {
        t.font_size = font;
        t.mono_font_size = mono;
    });
}

/// Binds the zoom keys and routes them to [`set_scale`]. Needs the
/// [`SettingsStore`] global.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-=", ZoomIn, None),
        KeyBinding::new("ctrl-+", ZoomIn, None),
        KeyBinding::new("ctrl--", ZoomOut, None),
        KeyBinding::new("ctrl-0", ZoomReset, None),
    ]);
    cx.on_action(|_: &ZoomIn, cx| zoom(next_scale, cx));
    cx.on_action(|_: &ZoomOut, cx| zoom(prev_scale, cx));
    cx.on_action(|_: &ZoomReset, cx| zoom(|_| DEFAULT_SCALE, cx));
}

fn zoom(step: fn(f32) -> f32, cx: &mut App) {
    let scale = step(SettingsStore::get(cx).ui_scale);
    set_scale(scale, cx);
}

/// Applies `scale` (snapped) and saves it as the `ui_scale` setting. The
/// zoom keys and any later scale control share this, so they stay in step.
pub fn set_scale(scale: f32, cx: &mut App) {
    let scale = snap_scale(scale);
    if SettingsStore::get(cx).ui_scale == scale {
        return;
    }
    SettingsStore::update(cx, |s| s.ui_scale = scale);
    tracing::info!(scale, "ui scale changed");
    apply_scale(scale, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_snaps_to_steps() {
        for s in UI_SCALES {
            assert_eq!(snap_scale(s), s);
        }
        assert_eq!(snap_scale(1.24), 1.25);
        assert_eq!(snap_scale(1.02), 1.0);
        assert_eq!(snap_scale(9.0), 2.0);
        assert_eq!(snap_scale(0.1), 0.8);
        assert_eq!(snap_scale(-3.0), 0.8);
        assert_eq!(snap_scale(f32::NAN), DEFAULT_SCALE);
        assert_eq!(snap_scale(f32::INFINITY), DEFAULT_SCALE);

        assert_eq!(next_scale(1.0), 1.1);
        assert_eq!(next_scale(2.0), 2.0);
        assert_eq!(prev_scale(1.0), 0.9);
        assert_eq!(prev_scale(0.8), 0.8);
        assert_eq!(prev_scale(2.0), 1.75);
        // Unknown values snap to the nearest step first.
        assert_eq!(next_scale(1.02), 1.1);
        assert_eq!(prev_scale(1.3), 1.1);
        assert_eq!(next_scale(5.0), 2.0);
        assert_eq!(prev_scale(0.1), 0.8);
    }
}
