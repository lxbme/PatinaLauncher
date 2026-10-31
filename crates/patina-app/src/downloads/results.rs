//! The results column: the sort dropdown and the area under the search box
//! (a placeholder while the source is unavailable, the hits when it
//! answers).

use gpui_kit::component::button::Button;
use gpui_kit::component::select::Select;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt as _, Window, div, rems,
};
use patina_core::content::online::OnlineHit;
use rust_i18n::t;

use super::{DownloadsPage, HITS_CONTEXT, Results};
use crate::fonts::{self, Strong};
use crate::layout::{GUTTER, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY, scroll_y};

/// Element id of the row of hit `id`.
pub fn hit_row_id(id: &str) -> ElementId {
    ElementId::Name(SharedString::from(format!("download-hit-{id}")))
}

impl DownloadsPage {
    pub(super) fn render_sort(&self) -> impl IntoElement {
        div().flex_none().w(rems(10.)).child(
            Select::new(&self.sort)
                .id("downloads-sort")
                .small()
                .w_full()
                .accessibility_label(t!("downloads.filters.sort").to_string()),
        )
    }

    pub(super) fn render_results(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let model = self.model.read(cx);
        let selected = model.selected().map(str::to_string);
        let searching = model.is_searching();
        let status = |id: &'static str, title: String, hint: Option<String>| {
            v_flex()
                .id(id)
                .test_support()
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
                        .text_color(fg)
                        .child(title),
                )
                .children(
                    hint.map(|hint| div().text_size(TEXT_PRIMARY).text_color(muted).child(hint)),
                )
        };
        let results = model.results().clone();
        let body = match &results {
            Results::Idle | Results::Unavailable => status(
                "downloads-unavailable",
                t!("downloads.results.unavailable_title").to_string(),
                Some(t!("downloads.results.unavailable_hint").to_string()),
            )
            .into_any_element(),
            Results::Loading => h_flex()
                .id("downloads-loading")
                .test_support()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .pb(rems(3.))
                .text_size(TEXT_PRIMARY)
                .text_color(muted)
                .child(Spinner::new().small())
                .child(t!("downloads.results.loading").to_string())
                .into_any_element(),
            Results::Failed(error) => status(
                "downloads-failed",
                t!("downloads.results.failed").to_string(),
                Some(error.to_string()),
            )
            .child(
                Button::new("downloads-retry")
                    .outline()
                    .small()
                    .label(t!("downloads.results.retry").to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model.update(cx, |m, cx| m.retry(cx));
                    })),
            )
            .into_any_element(),
            Results::Loaded(page) if page.hits.is_empty() => status(
                "downloads-empty",
                t!("downloads.results.empty").to_string(),
                None,
            )
            .into_any_element(),
            Results::Loaded(page) => {
                // The ring shows only while the keyboard drives the list.
                let ring = self.hits_focus.is_focused(window) && window.last_input_was_keyboard();
                let rows: Vec<AnyElement> = page
                    .hits
                    .iter()
                    .map(|hit| {
                        let is_selected = selected.as_deref() == Some(hit.id.as_str());
                        self.hit_row(hit, is_selected, ring, cx)
                    })
                    .collect();
                scroll_y(
                    v_flex()
                        .id("downloads-hits")
                        // Before `track_focus`, so tests see the focus.
                        .test_support()
                        .key_context(HITS_CONTEXT)
                        .track_focus(&self.hits_focus)
                        .children(rows),
                    &self.results_scroll,
                )
                .into_any_element()
            }
        };
        // The previous answer stays while a new one is awaited.
        div()
            .relative()
            .size_full()
            .child(body)
            .when(searching && !matches!(results, Results::Loading), |d| {
                d.child(
                    div()
                        .id("downloads-searching")
                        .test_support()
                        .absolute()
                        .top_2()
                        .right_3()
                        .child(Spinner::new().small()),
                )
            })
            .into_any_element()
    }

    /// A hit as two lines: title and author, then the summary; its download
    /// count on the right.
    fn hit_row(
        &self,
        hit: &OnlineHit,
        selected: bool,
        ring: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let ring_color = theme.ring;
        let (hover, active) = (theme.list_hover, theme.list_active);
        let id = hit.id.clone();
        let focus = self.hits_focus.clone();
        div()
            .id(hit_row_id(&hit.id))
            .test_support()
            .border_b_1()
            .border_color(border)
            .when(selected, |d| d.bg(active))
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                focus.focus(window, cx);
                this.model.update(cx, |m, cx| m.select(Some(&id), cx));
            }))
            .child(
                h_flex()
                    .gap_3()
                    .px(GUTTER)
                    .py_2()
                    .items_start()
                    // A transparent border keeps the rows from moving when
                    // the ring appears.
                    .border_1()
                    .border_color(if ring && selected {
                        ring_color
                    } else {
                        gpui_kit::transparent_black()
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_baseline()
                                    .child(
                                        div()
                                            .text_size(TEXT_ROW)
                                            .strong(FontWeight::MEDIUM, cx)
                                            .child(hit.title.clone()),
                                    )
                                    .child(
                                        div().text_size(TEXT_SECONDARY).text_color(muted).child(
                                            t!("downloads.results.by_author", author = &hit.author)
                                                .to_string(),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(TEXT_PRIMARY)
                                    .text_color(muted)
                                    .line_clamp(2)
                                    .child(hit.summary.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(TEXT_SECONDARY)
                            .text_color(muted)
                            .font_features(fonts::tnum())
                            .child(
                                t!(
                                    "downloads.results.downloads",
                                    count = group_thousands(hit.downloads)
                                )
                                .to_string(),
                            ),
                    ),
            )
            .into_any_element()
    }
}

/// `n` with a comma between thousands ("1,234,567"): the grouping both
/// interface languages use.
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::group_thousands;

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1234), "1,234");
        assert_eq!(group_thousands(1_000_000), "1,000,000");
        assert_eq!(group_thousands(12_345_678), "12,345,678");
    }
}
