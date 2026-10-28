//! 概览 tab: a two-column stats grid (crypto_pro quote panel style), then
//! the recently updated mods. Sections are separated by 1 px dividers, not
//! boxes.

use gpui_kit::component::{StyledExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    Context, Div, ElementId, InteractiveElement, IntoElement, ParentElement, Rems,
    StatefulInteractiveElement, Styled, div, rems,
};

use super::DetailView;
use crate::data::{self, Instance};
use crate::theme::Palette;
use crate::ui::{
    GUTTER, TEXT_PRIMARY, TEXT_SECONDARY, icon_tile, section_title, stat_grid, stat_pair,
};

/// Readable measure of the overview's content; the dividers run full width.
const CONTENT_MAX_W: Rems = Rems(44.);
const GRID_ROW: Rems = Rems(1.625);
const MOD_ROW: Rems = Rems(2.25);

fn section(cx: &Context<DetailView>) -> Div {
    let p = Palette::get(cx);
    div()
        .w_full()
        .px(GUTTER)
        .pt_2()
        .pb_3()
        .border_b_1()
        .border_color(p.border)
}

pub fn render(inst: &Instance, cx: &mut Context<DetailView>) -> impl IntoElement {
    let p = *Palette::get(cx);
    let mods = if inst.mods == 0 {
        "—".to_string()
    } else {
        inst.mods.to_string()
    };
    let pairs = vec![
        stat_pair("游戏版本", inst.mc.to_string(), cx),
        stat_pair("加载器", inst.loader_full(), cx),
        stat_pair("Java", format!("{} · Temurin", inst.java), cx),
        stat_pair("内存", inst.memory.to_string(), cx),
        stat_pair("游玩时长", inst.playtime.to_string(), cx),
        stat_pair("Mods", mods, cx),
        stat_pair("存档", inst.saves.to_string(), cx),
        stat_pair("占用空间", inst.size.to_string(), cx),
    ];

    let recent = data::recent_mods(inst.loader);
    let rows: Vec<_> = recent
        .iter()
        .enumerate()
        .map(|(i, m)| {
            h_flex()
                .id(ElementId::NamedInteger("mod-row".into(), i as u64))
                .h(MOD_ROW)
                .mx(rems(-0.5))
                .px_2()
                .gap_2p5()
                .rounded(rems(0.375))
                .cursor_pointer()
                .hover(move |s| s.bg(p.hover))
                .active(move |s| s.bg(p.selected))
                .child(icon_tile(
                    m.name[..1].to_string(),
                    p.tint(m.tint),
                    rems(1.25),
                    rems(0.625),
                ))
                .child(
                    div()
                        .text_size(TEXT_PRIMARY)
                        .text_color(p.text)
                        .child(m.name),
                )
                .child(
                    div()
                        .text_size(TEXT_SECONDARY)
                        .text_color(p.muted)
                        .child(m.version),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(TEXT_SECONDARY)
                        .text_color(p.muted)
                        .child(m.date),
                )
        })
        .collect();
    let has_mods = !rows.is_empty();

    v_flex()
        .id("overview")
        .size_full()
        .overflow_y_scroll()
        .child(
            section(cx).child(
                v_flex()
                    .max_w(CONTENT_MAX_W)
                    .child(section_title("概况", cx))
                    .child(stat_grid(pairs, GRID_ROW)),
            ),
        )
        .child(
            section(cx).border_b_0().child(
                v_flex()
                    .max_w(CONTENT_MAX_W)
                    .child(
                        section_title("最近更新的 Mod", cx).when(has_mods, |d| {
                            d.child(
                                div()
                                    .id("mods-all")
                                    .text_size(TEXT_SECONDARY)
                                    .font_normal()
                                    .text_color(p.accent)
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .child(format!("查看全部 {}", inst.mods)),
                            )
                        }),
                    )
                    .children(rows)
                    .when(!has_mods, |d| {
                        d.child(
                            div()
                                .h(MOD_ROW)
                                .flex()
                                .items_center()
                                .text_size(TEXT_SECONDARY)
                                .text_color(p.muted)
                                .child("原版实例没有安装 Mod"),
                        )
                    }),
            ),
        )
}
