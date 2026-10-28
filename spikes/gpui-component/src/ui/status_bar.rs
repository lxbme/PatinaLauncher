//! Status bar: download source and latency, Java runtimes, version. Small,
//! muted, `header` fill.

use gpui_kit::component::h_flex;
use gpui_kit::{App, IntoElement, ParentElement, Styled, div, px, rems};

use crate::theme::Palette;
use crate::ui::{GUTTER, TEXT_SECONDARY};

pub const STATUS_H: f32 = 1.625;

pub fn render(cx: &App) -> impl IntoElement {
    let p = *Palette::get(cx);
    let sep = || div().text_color(p.border).child("|");
    h_flex()
        .flex_none()
        .w_full()
        .h(rems(STATUS_H))
        .px(GUTTER)
        .gap_2p5()
        .bg(p.header)
        .border_t_1()
        .border_color(p.border)
        .whitespace_nowrap()
        .overflow_hidden()
        .text_size(TEXT_SECONDARY)
        .text_color(p.muted)
        .child(
            h_flex()
                .gap_1p5()
                .child(div().size(px(6.)).rounded_full().bg(p.green))
                .child("BMCLAPI · 38 ms"),
        )
        .child(sep())
        .child("Java 运行时 3 个")
        .child(sep())
        .child("实例 8 个 · 共 25.1 GB")
        .child(div().flex_1())
        .child("Patina 0.1.0-proto")
}
