//! 日志 tab: a toolbar (level filter chips, 复制, 清空) over the game log.
//! Lines use the mono font with tabular digits: muted timestamp, a
//! coloured level badge (INFO muted, WARN amber, ERROR red), thread, text.

use gpui_kit::component::{IconName, StyledExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    ClipboardItem, Context, ElementId, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, rems,
};

use super::DetailView;
use crate::data::{self, Level, LogLine};
use crate::theme::Palette;
use crate::ui::{GUTTER, TEXT_SECONDARY, chip, empty_state, secondary_button, text_button};

const LINE_H: f32 = 1.375;

fn level_color(l: Level, p: &Palette) -> gpui_kit::Hsla {
    match l {
        Level::Info => p.muted,
        Level::Warn => p.amber,
        Level::Error => p.red,
    }
}

fn plain_text(lines: &[&LogLine]) -> String {
    lines
        .iter()
        .map(|l| format!("[{}] [{}/{}] {}", l.time, l.thread, l.level.label(), l.msg))
        .collect::<Vec<_>>()
        .join("\n")
}

impl DetailView {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::get(cx);
        let count = |f: Option<Level>| {
            self.log
                .iter()
                .filter(|l| f.is_none_or(|lv| l.level == lv))
                .count()
        };
        let this = cx.weak_entity();
        let filters = [None, Some(Level::Info), Some(Level::Warn), Some(Level::Error)];
        let shown: Vec<&LogLine> = self
            .log
            .iter()
            .filter(|l| self.log_filter.is_none_or(|lv| l.level == lv))
            .collect();
        let copy_text = plain_text(&shown);
        h_flex()
            .flex_none()
            .h(rems(2.5))
            .px(GUTTER)
            .gap_1()
            .border_b_1()
            .border_color(p.border)
            .children(filters.into_iter().map(|f| {
                let this = this.clone();
                chip(
                    ElementId::Name(format!("log-chip-{f:?}").into()),
                    f.map_or("全部", Level::label),
                    Some(count(f)),
                    f == self.log_filter,
                    move |_, _, cx| {
                        let _ = this.update(cx, |v, cx| {
                            v.log_filter = f;
                            cx.notify();
                        });
                    },
                    cx,
                )
            }))
            .child(div().flex_1())
            .child(
                div()
                    .mr_2()
                    .text_size(TEXT_SECONDARY)
                    .text_color(p.muted)
                    .child("latest.log · 实时"),
            )
            .child(
                text_button("log-copy", IconName::Copy, "复制")
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()))
                    }),
            )
            .child(
                text_button("log-clear", gpui_kit::assets::IconName::Trash, "清空").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.log.clear();
                        cx.notify();
                    }),
                ),
            )
    }

    fn render_line(&self, ix: usize, l: &LogLine, cx: &Context<Self>) -> impl IntoElement + use<> {
        let p = *Palette::get(cx);
        let color = level_color(l.level, &p);
        let tint = match l.level {
            Level::Info => None,
            Level::Warn => Some(p.amber.opacity(0.05)),
            Level::Error => Some(p.red.opacity(0.07)),
        };
        h_flex()
            .id(ElementId::NamedInteger("log-line".into(), ix as u64))
            .min_h(rems(LINE_H))
            .px(GUTTER)
            .gap_2p5()
            .when_some(tint, |d, t| d.bg(t))
            .hover(move |s| s.bg(p.hover))
            .child(
                div()
                    .flex_none()
                    .text_color(p.muted.opacity(0.85))
                    .child(l.time),
            )
            .child(
                div()
                    .flex_none()
                    .w(rems(3.))
                    .flex()
                    .justify_center()
                    .rounded_sm()
                    .bg(color.opacity(0.14))
                    .text_color(color)
                    .text_size(rems(0.625))
                    .line_height(rems(1.))
                    .font_semibold()
                    .child(l.level.label()),
            )
            .child(
                div()
                    .flex_none()
                    .w(rems(8.5))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(p.muted)
                    .child(l.thread),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(match l.level {
                        Level::Info => p.text,
                        _ => color,
                    })
                    .child(l.msg),
            )
    }

    pub(super) fn render_log(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::get(cx);
        let mono = gpui_kit::component::Theme::global(cx).mono_font_family.clone();
        let lines: Vec<_> = self
            .log
            .iter()
            .enumerate()
            .filter(|(_, l)| self.log_filter.is_none_or(|lv| l.level == lv))
            .map(|(i, l)| self.render_line(i, l, cx).into_any_element())
            .collect();
        let empty = self.log.is_empty();
        v_flex()
            .size_full()
            .child(self.render_toolbar(cx))
            .when(empty, |d| {
                let restore = secondary_button("log-restore", "重新载入").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.log = data::launch_log();
                        cx.notify();
                    },
                ));
                d.child(empty_state(
                    IconName::FileText,
                    "日志已清空，下次启动时会重新记录",
                    Some(restore),
                    cx,
                ))
            })
            .when(!empty, |d| {
                d.child(
                    v_flex()
                        .id("log-lines")
                        .flex_1()
                        .min_h_0()
                        .py_1p5()
                        .overflow_y_scroll()
                        .font_family(mono)
                        .text_size(TEXT_SECONDARY)
                        .text_color(p.text)
                        .children(lines),
                )
            })
    }
}
