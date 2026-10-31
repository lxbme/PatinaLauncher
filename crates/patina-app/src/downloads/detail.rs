//! The detail column: the chosen hit, and the install button.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Disableable, v_flex};
use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, TestSupportExt as _, div, rems,
};
use patina_core::content::online::OnlineHit;
use rust_i18n::t;

use super::{DownloadsPage, Results};
use crate::fonts::Strong;
use crate::layout::{GUTTER, STRIP_H, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY, scroll_y};

impl DownloadsPage {
    pub(super) fn render_detail(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let model = self.model.read(cx);
        let hit: Option<OnlineHit> = match (model.results(), model.selected()) {
            (Results::Loaded(page), Some(id)) => page.hits.iter().find(|h| h.id == id).cloned(),
            _ => None,
        };
        let instance = model.target().and_then(|id| {
            self.instances
                .read(cx)
                .instances()
                .iter()
                .find(|i| i.id == id)
                .map(|i| i.config.name.clone())
        });
        let label = match &instance {
            Some(name) => t!("downloads.detail.install_to", instance = name).to_string(),
            None => t!("downloads.detail.choose_instance").to_string(),
        };
        let tip = t!("downloads.detail.install_soon").to_string();

        let body = match hit {
            Some(hit) => v_flex()
                .id("downloads-detail")
                .test_support()
                .gap_2()
                .p(GUTTER)
                .child(
                    div()
                        .text_size(TEXT_ROW)
                        .strong(FontWeight::MEDIUM, cx)
                        .child(hit.title),
                )
                .child(
                    div()
                        .text_size(TEXT_SECONDARY)
                        .text_color(muted)
                        .child(t!("downloads.results.by_author", author = &hit.author).to_string()),
                )
                .child(div().text_size(TEXT_PRIMARY).child(hit.summary))
                .into_any_element(),
            None => v_flex()
                .id("downloads-detail-empty")
                .test_support()
                .p(GUTTER)
                .text_size(TEXT_PRIMARY)
                .text_color(muted)
                .child(t!("downloads.detail.empty").to_string())
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .h(STRIP_H)
                    .px(GUTTER)
                    .border_b_1()
                    .border_color(border)
                    .text_size(TEXT_PRIMARY)
                    .strong(FontWeight::MEDIUM, cx)
                    .child(t!("downloads.detail.title").to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(scroll_y(body_div(body), &self.detail_scroll)),
            )
            .child(
                // Installing needs a real source: disabled for now, with the
                // reason on the wrapper (a disabled button shows no tooltip).
                div()
                    .id("downloads-install-wrap")
                    .flex_none()
                    .p(GUTTER)
                    .border_t_1()
                    .border_color(border)
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .child(
                        Button::new("downloads-install")
                            .primary()
                            .w_full()
                            .min_w(rems(0.))
                            .disabled(true)
                            .label(label),
                    ),
            )
    }
}

/// `body` as the scroll owner.
fn body_div(body: gpui_kit::AnyElement) -> gpui_kit::Stateful<gpui_kit::Div> {
    div().id("downloads-detail-scroll").child(body)
}
