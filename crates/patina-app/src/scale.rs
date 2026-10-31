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
//!
//! The layout is designed for a 960×600 window at 100 %, so the main
//! window's minimum grows with the scale ([`min_window_size`]), and a step
//! whose minimum would not fit the window's display is not offered: the zoom
//! keys stop at the largest step that fits ([`largest_fitting`]).

use gpui_kit::component::Theme;
use gpui_kit::{
    AnyWindowHandle, App, Global, KeyBinding, Pixels, PlatformDisplay, Size, Window, actions, px,
    size,
};

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

/// The main window's minimum size at 100 % (logical pixels, the
/// platform's window geometry unit).
pub const MIN_WINDOW: Size<Pixels> = Size {
    width: px(960.),
    height: px(600.),
};

/// The main window's minimum size at `scale`: the 100 % minimum scaled, so
/// the rem-based layout has the room it was designed for.
pub fn min_window_size(scale: f32) -> Size<Pixels> {
    let scale = snap_scale(scale);
    size(MIN_WINDOW.width * scale, MIN_WINDOW.height * scale)
}

/// The largest step whose [`min_window_size`] fits in `display` (the
/// display's usable size). 100 % is always offered, even on a display too
/// small for it: that is the layout's baseline.
pub fn largest_fitting(display: Size<Pixels>) -> f32 {
    UI_SCALES
        .iter()
        .copied()
        .filter(|&s| {
            let min = min_window_size(s);
            min.width <= display.width && min.height <= display.height
        })
        .fold(DEFAULT_SCALE, f32::max)
}

/// The zoom state of this process (a gpui `Global`).
struct ScaleState {
    /// The theme's unscaled font sizes, captured on the first
    /// [`apply_scale`].
    base: Option<(Pixels, Pixels)>,
    /// The step the theme shows now; it can be below the saved setting when
    /// that did not fit the display.
    applied: f32,
    /// The largest step the main window's display can take.
    max: f32,
    /// The windows whose minimum follows the scale (the main window).
    windows: Vec<AnyWindowHandle>,
}

impl Default for ScaleState {
    fn default() -> Self {
        Self {
            base: None,
            applied: DEFAULT_SCALE,
            max: UI_SCALES[UI_SCALES.len() - 1],
            windows: Vec::new(),
        }
    }
}

impl Global for ScaleState {}

/// Scales the whole UI of every window to `scale` (snapped to a step).
/// Does not save it and does not check the display limit; see
/// [`set_scale`] for a user's change.
///
/// Call after `theme::apply`: the first call takes the theme's font sizes
/// as the unscaled base.
pub fn apply_scale(scale: f32, cx: &mut App) {
    let scale = snap_scale(scale);
    let base = match cx.default_global::<ScaleState>().base {
        Some(base) => base,
        None => {
            let theme = Theme::global(cx);
            let base = (theme.font_size, theme.mono_font_size);
            cx.global_mut::<ScaleState>().base = Some(base);
            base
        }
    };
    cx.global_mut::<ScaleState>().applied = scale;
    let (font, mono) = (base.0 * scale, base.1 * scale);
    // `update` refreshes every window, so the new rem size shows at once.
    Theme::update(cx, |t| {
        t.font_size = font;
        t.mono_font_size = mono;
    });
}

/// The step the UI shows now.
pub fn applied_scale(cx: &App) -> f32 {
    cx.try_global::<ScaleState>()
        .map_or(DEFAULT_SCALE, |s| s.applied)
}

/// The largest step the main window's display can take now.
pub fn max_scale(cx: &App) -> f32 {
    cx.try_global::<ScaleState>()
        .map_or(UI_SCALES[UI_SCALES.len() - 1], |s| s.max)
}

/// The usable size of `display` (`None` → no limit is known).
fn usable(display: Option<std::rc::Rc<dyn PlatformDisplay>>) -> Option<Size<Pixels>> {
    display.map(|d| d.visible_bounds().size)
}

/// The display a new window most likely opens on, for the limit before the
/// window exists: the primary display, else the only one, else the
/// smallest (Wayland names no primary display; the smallest is the one
/// whose limit is safe on any of them).
fn display_for_new_window(cx: &App) -> Option<Size<Pixels>> {
    if let Some(primary) = usable(cx.primary_display()) {
        return Some(primary);
    }
    let area = |s: &Size<Pixels>| f32::from(s.width) * f32::from(s.height);
    cx.displays()
        .into_iter()
        .map(|d| d.visible_bounds().size)
        .min_by(|a, b| area(a).total_cmp(&area(b)))
}

/// Sets the display limit from `display` (`None` → no limit).
fn set_limit(display: Option<Size<Pixels>>, cx: &mut App) {
    let max = display.map_or(UI_SCALES[UI_SCALES.len() - 1], largest_fitting);
    cx.default_global::<ScaleState>().max = max;
}

/// The step the main window opens at: the saved `scale`, limited to the
/// display the window will likely open on. Sets that limit; the saved
/// setting itself stays as it is.
pub fn startup_scale(scale: f32, cx: &mut App) -> f32 {
    set_limit(display_for_new_window(cx), cx);
    snap_scale(scale).min(max_scale(cx))
}

/// Makes `window`'s minimum follow the scale: takes the limit from its
/// display (now and whenever it moves to another one) and grows it when a
/// larger step needs more room. Call once, right after opening the window
/// with [`min_window_size`] of the scale it opened at.
pub fn follow_window(window: &mut Window, cx: &mut App) {
    cx.default_global::<ScaleState>()
        .windows
        .push(window.window_handle());
    limit_to_display(window, cx);
    window
        .observe_window_display(|window, cx| {
            limit_to_display(window, cx);
            let scale = applied_scale(cx);
            grow_to_min(scale, window);
        })
        .detach();
}

/// Takes the limit from `window`'s display; a scale above it is lowered
/// (shown, not saved).
fn limit_to_display(window: &Window, cx: &mut App) {
    let display = usable(window.display(cx)).or_else(|| display_for_new_window(cx));
    set_limit(display, cx);
    let (applied, max) = (applied_scale(cx), max_scale(cx));
    if applied > max {
        tracing::info!(applied, max, "ui scale lowered to fit the display");
        apply_scale(max, cx);
    }
}

/// Grows `window` to the minimum of `scale` when it is smaller (a maximized
/// or fullscreen window is left to the platform).
///
/// gpui sets a window's platform minimum only when it opens, so a step
/// taken later cannot raise it; growing the window gives the step its room
/// now, and the next start opens with the new minimum.
fn grow_to_min(scale: f32, window: &mut Window) {
    if window.is_maximized() || window.is_fullscreen() {
        return;
    }
    let min = min_window_size(scale);
    let now = window.viewport_size();
    if now.width < min.width || now.height < min.height {
        window.resize(now.max(&min));
    }
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
    let scale = step(applied_scale(cx));
    set_scale(scale, cx);
}

/// Applies `scale` (snapped, and limited to the largest step that fits the
/// display) and saves it as the `ui_scale` setting; the main window grows
/// to the step's minimum. The zoom keys and any later scale control share
/// this, so they stay in step. A step that changes nothing saves nothing.
pub fn set_scale(scale: f32, cx: &mut App) {
    let scale = snap_scale(scale).min(max_scale(cx));
    if cx.has_global::<ScaleState>() && applied_scale(cx) == scale {
        return;
    }
    SettingsStore::update(cx, |s| s.ui_scale = scale);
    tracing::info!(scale, "ui scale changed");
    apply_scale(scale, cx);
    // The zoom keys run while their window is being updated; resize it
    // once that is done.
    let windows = cx.global::<ScaleState>().windows.clone();
    cx.defer(move |cx| {
        for handle in windows {
            handle
                .update(cx, |_, window, _| grow_to_min(scale, window))
                .ok();
        }
    });
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

    #[test]
    fn the_minimum_grows_with_the_scale() {
        assert_eq!(min_window_size(1.0), size(px(960.), px(600.)));
        assert_eq!(min_window_size(1.5), size(px(1440.), px(900.)));
        assert_eq!(min_window_size(2.0), size(px(1920.), px(1200.)));
        // Unknown values snap first.
        assert_eq!(min_window_size(1.02), MIN_WINDOW);
    }

    #[test]
    fn only_steps_that_fit_the_display_are_offered() {
        let display = |w: f32, h: f32| size(px(w), px(h));
        assert_eq!(largest_fitting(display(3840., 2160.)), 2.0);
        assert_eq!(largest_fitting(display(1920., 1200.)), 2.0);
        // 1920×1080: 200 % needs 1200 rows.
        assert_eq!(largest_fitting(display(1920., 1080.)), 1.75);
        assert_eq!(largest_fitting(display(1366., 768.)), 1.25);
        assert_eq!(largest_fitting(display(1280., 720.)), 1.1);
        // 100 % stays offered on a display too small for it.
        assert_eq!(largest_fitting(display(800., 480.)), 1.0);
    }
}
