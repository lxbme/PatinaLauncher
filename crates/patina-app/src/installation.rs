//! Shared presentation for content and pack installation decisions; workflow stays with each owner.
use crate::fonts::Strong;
use gpui_kit::component::{ActiveTheme, Sizable, alert::Alert, h_flex, label::Label, v_flex};
use gpui_kit::{
    App, Div, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, div, rems,
};
use rust_i18n::t;

pub(crate) fn identity(
    id: &'static str,
    title: String,
    version: String,
    cx: &App,
) -> impl IntoElement + use<> {
    v_flex()
        .id(id)
        .test_support()
        .gap_1()
        .min_w_0()
        .child(Label::new(title).text_lg().strong(FontWeight::SEMIBOLD, cx))
        .child(
            Label::new(version)
                .text_sm()
                .text_color(cx.theme().muted_foreground),
        )
}
pub(crate) fn metadata(label: String, value: String, cx: &App) -> Div {
    h_flex()
        .items_start()
        .gap_3()
        .min_w_0()
        .child(
            Label::new(label)
                .w_20()
                .flex_none()
                .text_sm()
                .text_color(cx.theme().muted_foreground),
        )
        .child(div().flex_1().min_w_0().text_sm().child(value))
}
pub(crate) fn section_label(text: String, cx: &App) -> Label {
    Label::new(text).text_sm().strong(FontWeight::MEDIUM, cx)
}
pub(crate) fn failure(id: &'static str, error: SharedString) -> impl IntoElement + use<> {
    div()
        .id(id)
        .test_support()
        .max_h(rems(8.))
        .overflow_y_scroll()
        .child(
            Alert::error(SharedString::from(format!("{id}-alert")), error)
                .title(t!("installation.failed").to_string())
                .small(),
        )
}
pub(crate) fn files(count: usize, bytes: u64) -> String {
    t!(
        "installation.file_size",
        count = count,
        size = format!("{:.1}", bytes as f64 / 1024. / 1024.)
    )
    .to_string()
}

/// Current files only. The run snapshot survives last-value bridge coalescing.
pub(crate) fn activity(
    snapshot: Option<&patina_core::download::TransferSnapshot>,
    name: impl Fn(&patina_core::download::FileActivity) -> String,
    window: &gpui_kit::Window,
    cx: &App,
) -> Option<gpui_kit::AnyElement> {
    use gpui_kit::component::{progress::Progress, scroll::ScrollableElement, tooltip::Tooltip};
    let snapshot = snapshot.filter(|s| !s.files.is_empty())?;
    let height = (snapshot.files.len().min(3) as f32 * 3.).min(
        (f32::from(window.viewport_size().height) / f32::from(window.rem_size()) - 24.).max(3.),
    );
    let rows = snapshot.files.iter().map(|file| {
        let id = SharedString::from(format!("install-file:{}:{}", snapshot.run, file.id));
        let full = name(file);
        let tooltip = full.clone();
        let known = file.total_bytes.filter(|n| *n > 0);
        let active = file.state == patina_core::download::FileActivityState::Downloading;
        v_flex()
            .id(id.clone())
            .test_support()
            .h(rems(3.))
            .flex_none()
            .gap_1()
            .min_w_0()
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .id(SharedString::from(format!("{id}-name")))
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(full)
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip.clone()).build(window, cx)
                            }),
                    )
                    .children(active.then(|| {
                        Label::new(match known {
                            Some(total) => t!(
                                "installation.file_amount",
                                done = file.done_bytes / 1024,
                                total = total / 1024
                            )
                            .to_string(),
                            None => t!("installation.received", done = file.done_bytes / 1024)
                                .to_string(),
                        })
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                    }))
                    .child(
                        Label::new(file.state.display_name())
                            .text_xs()
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            .child(
                Progress::new(SharedString::from(format!("{id}-bar")))
                    .loading(!active || known.is_none())
                    .value(known.map_or(0., |total| {
                        (file.done_bytes as f64 / total as f64 * 100.).clamp(0., 100.) as f32
                    })),
            )
    });
    Some(
        v_flex()
            .id("install-file-activity")
            .test_support()
            .gap_2()
            .min_w_0()
            .children((snapshot.bytes_per_second > 0).then(|| {
                Label::new(
                    t!(
                        "installation.network_speed",
                        speed = format!("{:.1}", snapshot.bytes_per_second as f64 / 1024. / 1024.)
                    )
                    .to_string(),
                )
                .text_sm()
                .text_color(cx.theme().muted_foreground)
            }))
            .child(
                div()
                    .id("install-file-list")
                    .test_support()
                    .h(rems(height))
                    .min_h_0()
                    .child(
                        v_flex()
                            .h_full()
                            .children(rows)
                            .overflow_y_scrollbar()
                            .id("install-file-scroll"),
                    ),
            )
            .into_any_element(),
    )
}
