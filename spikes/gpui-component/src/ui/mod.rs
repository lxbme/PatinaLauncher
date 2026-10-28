//! Window UI: root layout, one module per region, plus the small building
//! blocks the regions share (text sizes, chips, tabs, tags, stats grid).
//! Most helpers are crypto_pro's (`crates/app/src/ui/mod.rs`), retuned for
//! Patina's tokens.

pub mod detail;
pub mod instance_list;
pub mod rail;
pub mod root;
pub mod status_bar;
pub mod title_bar;

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{Icon, IconNamed, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    App, ClickEvent, Div, ElementId, FontFeatures, FontWeight, Hsla, InteractiveElement,
    ParentElement, Rems, SharedString, Stateful, StatefulInteractiveElement, Styled,
    Window, div, px, rems,
};

use crate::data::Loader;
use crate::theme::Palette;

/// Text sizes at UI scale 1.0, in rem so they follow the scale: primary
/// 13 px, secondary labels 12 px, row titles 14 px.
pub const TEXT_PRIMARY: Rems = Rems(0.8125);
pub const TEXT_SECONDARY: Rems = Rems(0.75);
pub const TEXT_ROW: Rems = Rems(0.875);
/// Panel gutter (12 px).
pub const GUTTER: Rems = Rems(0.75);
/// Height of every panel's top strip (title bar, column headers, tab bar),
/// so their bottom dividers line up across columns.
pub const STRIP_H: Rems = Rems(2.5);

/// Family for weighted (medium / semibold) text that contains CJK.
///
/// gpui's Linux text system picks CJK glyphs from a fallback face at
/// Regular weight whatever weight was asked for, so bold Chinese next to
/// bold Latin looks thin. Naming the CJK family as the primary font makes
/// gpui select its real Medium / Bold faces (its Latin glyphs are a close
/// humanist sans). Plain-weight text keeps the UI font with `tnum`.
pub const CJK_FAMILY: &str = "Noto Sans CJK SC";

/// `font_weight` plus `CJK_FAMILY`; see there.
pub trait Strong: Styled + Sized {
    fn strong(self, weight: FontWeight) -> Self {
        self.font_weight(weight).font_family(CJK_FAMILY)
    }
}

impl<T: Styled> Strong for T {}

/// OpenType tabular figures, set on the root so every digit in the UI has
/// the same advance and nothing jitters.
pub fn tnum() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// Pill-outline filter chip (crypto_pro watchlist chips): muted outline,
/// the selected one accent outline and accent text.
pub fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    count: Option<usize>,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    let p = *Palette::get(cx);
    div()
        .id(id)
        .flex_none()
        .h(rems(1.5))
        .px(rems(0.4375))
        .flex()
        .items_center()
        .gap_1()
        .rounded_full()
        .border_1()
        .cursor_pointer()
        .whitespace_nowrap()
        .text_size(TEXT_SECONDARY)
        .when(selected, |d| {
            d.border_color(p.accent)
                .text_color(p.accent)
                .strong(FontWeight::MEDIUM)
        })
        .when(!selected, |d| {
            d.border_color(p.border)
                .text_color(p.muted)
                .hover(move |s| s.bg(p.raised).text_color(p.text))
                .active(move |s| s.bg(p.selected))
        })
        .child(label.into())
        .when_some(count, |d, n| {
            d.child(
                div()
                    .text_color(if selected { p.accent.opacity(0.7) } else { p.muted.opacity(0.7) })
                    .child(n.to_string()),
            )
        })
        .on_click(on_click)
}

/// Underline tab (crypto_pro `underline_tab`): muted text, the selected one
/// primary with a 2 px teal bar on the strip's bottom divider.
pub fn underline_tab(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    count: Option<u32>,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    let p = *Palette::get(cx);
    div()
        .id(id)
        .flex_none()
        .h_full()
        .px_0p5()
        .flex()
        .items_center()
        .gap_1()
        .relative()
        .cursor_pointer()
        .whitespace_nowrap()
        .text_size(TEXT_PRIMARY)
        .text_color(if selected { p.text } else { p.muted })
        .when(selected, |d| d.strong(FontWeight::MEDIUM))
        .when(!selected, |d| d.hover(move |s| s.text_color(p.text)))
        .child(label.into())
        .when_some(count, |d, n| {
            d.child(
                div()
                    .text_size(TEXT_SECONDARY)
                    .font_weight(FontWeight::NORMAL)
                    .text_color(p.muted)
                    .child(n.to_string()),
            )
        })
        .when(selected, |d| {
            d.child(
                div()
                    .absolute()
                    .bottom(px(-1.))
                    .left_0()
                    .right_0()
                    .h(px(2.))
                    .bg(p.teal),
            )
        })
        .on_click(on_click)
}

/// Loader as coloured text (`Fabric`), like crypto_pro's `market_tag`.
pub fn loader_tag(l: Loader, cx: &App) -> Div {
    div()
        .flex_none()
        .whitespace_nowrap()
        .text_size(TEXT_SECONDARY)
        .text_color(Palette::get(cx).loader(l))
        .child(l.label())
}

/// Loader badge: the tag on a faint fill of its colour (`market_badge`).
pub fn loader_badge(text: impl Into<SharedString>, l: Loader, cx: &App) -> Div {
    let color = Palette::get(cx).loader(l);
    div()
        .flex_none()
        .px_1p5()
        .rounded_sm()
        .bg(color.opacity(0.14))
        .text_color(color)
        .text_size(TEXT_SECONDARY)
        .line_height(rems(1.125))
        .whitespace_nowrap()
        .child(text.into())
}

/// Square placeholder icon: a tinted block with an initial.
pub fn icon_tile(initial: impl Into<SharedString>, color: Hsla, size: Rems, text: Rems) -> Div {
    div()
        .flex_none()
        .size(size)
        .flex()
        .items_center()
        .justify_center()
        .rounded(rems(size.0 * 0.22))
        .bg(color.opacity(0.16))
        .border_1()
        .border_color(color.opacity(0.22))
        .text_size(text)
        .strong(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(initial.into())
}

/// Ghost icon button with a tooltip and a pointer cursor (the title bar
/// gear, the instance column's `+`).
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
) -> Button {
    Button::new(id)
        .ghost()
        .small()
        .icon(icon)
        .tooltip(tooltip)
        .cursor_pointer()
}

/// Ghost button with an icon and a label (log toolbar).
pub fn text_button(
    id: impl Into<ElementId>,
    icon: impl IconNamed,
    label: impl Into<SharedString>,
) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(Icon::new(icon).xsmall())
        .label(label)
        .px_2()
        .h(rems(1.5))
        .text_size(TEXT_SECONDARY)
        .cursor_pointer()
}

/// One label / value pair of a two-column stats grid (crypto_pro
/// `stat_pair`): muted label left, value right-aligned.
pub fn stat_pair(label: &'static str, value: String, cx: &App) -> Div {
    let p = Palette::get(cx);
    h_flex()
        .flex_1()
        .min_w_0()
        .justify_between()
        .gap_2()
        .child(
            div()
                .whitespace_nowrap()
                .text_size(TEXT_SECONDARY)
                .text_color(p.muted)
                .child(label),
        )
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_size(TEXT_PRIMARY)
                .text_color(p.text)
                .child(value),
        )
}

/// Lays `pairs` out two per row, rows `row_h` high (`stat_grid`).
pub fn stat_grid(pairs: Vec<Div>, row_h: Rems) -> Div {
    let mut grid = v_flex().gap(rems(0.1875));
    let mut it = pairs.into_iter();
    while let Some(a) = it.next() {
        let b = it.next().unwrap_or_else(|| div().flex_1());
        grid = grid.child(h_flex().h(row_h).gap_10().child(a).child(b));
    }
    grid
}

/// Section title row: 13 px semibold, optional trailing element.
pub fn section_title(title: &'static str, cx: &App) -> Div {
    h_flex()
        .h(rems(2.))
        .justify_between()
        .text_size(TEXT_PRIMARY)
        .strong(FontWeight::SEMIBOLD)
        .text_color(Palette::get(cx).text)
        .child(title)
}

/// Centered empty state: muted icon on a faint disc, one line, optional
/// action.
pub fn empty_state(
    icon: impl Into<Icon>,
    text: impl Into<SharedString>,
    action: Option<Button>,
    cx: &App,
) -> Div {
    let p = Palette::get(cx);
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_3()
        .pb(rems(3.))
        .child(
            div()
                .size(rems(3.5))
                .rounded_full()
                .bg(p.raised.opacity(0.6))
                .flex()
                .items_center()
                .justify_center()
                .text_color(p.muted)
                .child(icon.into().size(rems(1.5))),
        )
        .child(
            div()
                .text_size(TEXT_PRIMARY)
                .text_color(p.muted)
                .child(text.into()),
        )
        .children(action)
}

/// Secondary button (raised fill) used next to filled actions.
pub fn secondary_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id)
        .outline()
        .small()
        .label(label)
        .px_3()
        .text_size(TEXT_PRIMARY)
        .cursor_pointer()
}
