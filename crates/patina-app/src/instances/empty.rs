//! The right side when there is nothing to show: no instances at all (a
//! large "添加实例" button), or a list that could not be read.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, v_flex};
use gpui_kit::{
    App, FocusHandle, FontWeight, IntoElement, ParentElement, RenderOnce, SharedString, Styled,
    Window, div, rems,
};
use rust_i18n::t;

use super::AddInstance;
use crate::fonts::Strong;
use crate::layout::{TEXT_PRIMARY, TEXT_ROW};

/// No instances and no broken entries: what to do next, and the button
/// that does it. It dispatches [`AddInstance`] at `page_focus`, as the
/// list's ＋ does.
#[derive(IntoElement)]
pub struct EmptyState {
    page_focus: FocusHandle,
}

impl EmptyState {
    pub fn new(page_focus: FocusHandle) -> Self {
        Self { page_focus }
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let focus = self.page_focus;
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            // Sits a little above the optical centre, as the placeholders do.
            .pb(rems(3.))
            .child(
                div()
                    .text_size(TEXT_ROW)
                    .strong(FontWeight::MEDIUM, cx)
                    .text_color(theme.foreground)
                    .child(t!("instances.empty.title").to_string()),
            )
            .child(
                div()
                    .text_size(TEXT_PRIMARY)
                    .text_color(theme.muted_foreground)
                    .child(t!("instances.empty.body").to_string()),
            )
            .child(
                Button::new("add-instance-empty")
                    .primary()
                    .large()
                    .mt_2()
                    .icon(Icon::new(IconName::Plus))
                    .label(t!("instances.add").to_string())
                    .on_click(move |_, window, cx| focus.dispatch_action(&AddInstance, window, cx)),
            )
    }
}

/// The list could not be read at all (the first refresh failed): why.
#[derive(IntoElement)]
pub struct ListError {
    error: SharedString,
}

impl ListError {
    pub fn new(error: SharedString) -> Self {
        Self { error }
    }
}

impl RenderOnce for ListError {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .pb(rems(3.))
            .px(rems(2.))
            .child(
                div()
                    .text_size(TEXT_ROW)
                    .strong(FontWeight::MEDIUM, cx)
                    .text_color(theme.foreground)
                    .child(t!("instances.list_failed").to_string()),
            )
            .child(
                div()
                    .text_size(TEXT_PRIMARY)
                    .text_color(theme.muted_foreground)
                    .child(self.error),
            )
    }
}
