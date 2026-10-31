//! A small window that tells one thing and closes: for the moments before
//! (or instead of) the main window, such as another Patina already running
//! or a failed start. Closing the last window quits the process
//! (`quit_with_last_window`).

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Context, Entity, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, SharedString, Styled,
    TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, size,
};
use rust_i18n::t;

use crate::fonts::Strong;

actions!(notice, [Dismiss]);

/// Key context of the [`Notice`] window.
const CONTEXT: &str = "Notice";

/// A title, a body and a button that closes the window. It takes focus when
/// it opens, so Enter and Escape close it too.
pub struct Notice {
    focus: FocusHandle,
    title: SharedString,
    body: SharedString,
}

impl Notice {
    fn new(
        title: SharedString,
        body: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self { focus, title, body }
    }
}

impl Render for Notice {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(|_: &Dismiss, window, _| window.remove_window())
            .size_full()
            .p_5()
            .gap_2()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .text_base()
                    .strong(FontWeight::SEMIBOLD, cx)
                    .child(self.title.clone()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(self.body.clone()),
            )
            .child(div().flex_1())
            .child(
                h_flex().justify_end().child(
                    Button::new("close")
                        .primary()
                        .label(t!("notice.close"))
                        .on_click(|_, window, _| window.remove_window()),
                ),
            )
    }
}

/// Opens a [`Notice`] of `size_rem` (width, height) at `ui_scale` and binds
/// its keys. Call after `gpui_kit::init`.
pub fn open(
    title: SharedString,
    body: SharedString,
    size_rem: (f32, f32),
    ui_scale: f32,
    cx: &mut App,
) -> gpui_kit::Result<(AnyWindowHandle, Entity<Notice>)> {
    cx.bind_keys([
        KeyBinding::new("enter", Dismiss, Some(CONTEXT)),
        KeyBinding::new("escape", Dismiss, Some(CONTEXT)),
    ]);
    // The window is sized in the theme's base font so it follows the scale;
    // before `theme::apply` that is the default theme's, which `apply`
    // keeps.
    let rem = Theme::global(cx).font_size * crate::scale::snap_scale(ui_scale);
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(t!("app.title").to_string().into()),
            ..Default::default()
        }),
        app_id: Some(crate::APP_ID.into()),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(rem * size_rem.0, rem * size_rem.1),
            cx,
        ))),
        is_resizable: false,
        is_minimizable: false,
        ..Default::default()
    };
    gpui_kit::open_window(options, cx, move |window, cx| {
        crate::style_window(ui_scale, window, cx);
        cx.new(|cx| Notice::new(title, body, window, cx))
    })
}

/// Opens the notice that Patina could not start, saying why (`error` is
/// localized). Call after `gpui_kit::init`.
pub fn open_startup_failure(
    error: &dyn std::fmt::Display,
    ui_scale: f32,
    cx: &mut App,
) -> gpui_kit::Result<(AnyWindowHandle, Entity<Notice>)> {
    let title = t!("startup_failed.title").to_string().into();
    // Roomier than the other notices: the reason can run to a few lines.
    open(title, error.to_string().into(), (28., 11.), ui_scale, cx)
}
