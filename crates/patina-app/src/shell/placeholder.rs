//! Stand-in pages for destinations that a later version fills in.

use gpui_kit::component::{ActiveTheme, Icon, v_flex};
use gpui_kit::{
    App, FontWeight, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div,
    rems,
};
use rust_i18n::t;

use crate::fonts::Strong;
use crate::layout::{TEXT_PRIMARY, TEXT_ROW};

/// A page that only says it is coming: `icon` on a faint disc, the page's
/// `title` and one muted line.
pub fn page(icon: impl Into<Icon>, title: impl Into<SharedString>) -> Placeholder {
    Placeholder {
        icon: icon.into(),
        title: title.into(),
    }
}

/// See [`page`].
#[derive(IntoElement)]
pub struct Placeholder {
    icon: Icon,
    title: SharedString,
}

impl RenderOnce for Placeholder {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            // Sits a little above the optical centre, as the empty states do.
            .pb(rems(3.))
            .child(
                div()
                    .size(rems(3.5))
                    .rounded_full()
                    .bg(theme.muted.opacity(0.6))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.muted_foreground)
                    .child(self.icon.size(rems(1.5))),
            )
            .child(
                div()
                    .text_size(TEXT_ROW)
                    .strong(FontWeight::MEDIUM, cx)
                    .text_color(theme.foreground)
                    .child(self.title),
            )
            .child(
                div()
                    .text_size(TEXT_PRIMARY)
                    .text_color(theme.muted_foreground)
                    .child(t!("shell.placeholder.coming_later").to_string()),
            )
    }
}
