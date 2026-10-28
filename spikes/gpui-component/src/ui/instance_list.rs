//! Instance column (crypto_pro watchlist style): title row with `+`, loader
//! filter chips, then two-line rows with no dividers. Clicking a row
//! selects it; the detail pane follows the shared state.

use gpui_kit::component::{IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    Context, ElementId, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px, rems,
};

use crate::data::{Instance, Loader};
use crate::state::AppState;
use crate::theme::Palette;
use crate::ui::{
    GUTTER, STRIP_H, Strong, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY, chip, icon_button, icon_tile,
    loader_tag,
};

/// Column width at UI scale 1.0 (300 px): the five loader chips fit.
pub const LIST_W: f32 = 18.75;
/// Row height (two lines), 48 px.
const ROW_H: f32 = 3.;

/// Loader filter chip; session-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Only(Loader),
}

impl Filter {
    const CHIPS: [Filter; 5] = [
        Filter::All,
        Filter::Only(Loader::Fabric),
        Filter::Only(Loader::Forge),
        Filter::Only(Loader::NeoForge),
        Filter::Only(Loader::Vanilla),
    ];

    fn accepts(self, l: Loader) -> bool {
        match self {
            Filter::All => true,
            Filter::Only(x) => x == l,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Filter::All => "全部",
            Filter::Only(l) => l.label(),
        }
    }
}

pub struct InstanceList {
    state: Entity<AppState>,
    filter: Filter,
    _observe: Subscription,
}

impl InstanceList {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            filter: Filter::All,
            _observe,
        }
    }

    fn render_row(
        &self,
        ix: usize,
        inst: &Instance,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let p = *Palette::get(cx);
        let state = self.state.clone();
        h_flex()
            .id(ElementId::NamedInteger("inst-row".into(), ix as u64))
            .w_full()
            .h(rems(ROW_H))
            .px(GUTTER)
            .gap_2p5()
            .cursor_pointer()
            .when(selected, |r| r.bg(p.selected))
            .when(!selected, |r| {
                r.hover(move |s| s.bg(p.hover))
                    .active(move |s| s.bg(p.selected))
            })
            .on_click(move |_, _, cx| state.update(cx, |s, cx| s.select(ix, cx)))
            .child(icon_tile(
                inst.initial,
                p.tint(inst.tint),
                rems(1.75),
                TEXT_PRIMARY,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_size(TEXT_ROW)
                                    .text_color(p.text)
                                    .child(inst.name),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .text_size(TEXT_SECONDARY)
                                    .text_color(p.muted)
                                    .child(inst.last_played),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .child(loader_tag(inst.loader, cx))
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_size(TEXT_SECONDARY)
                                    .text_color(p.muted)
                                    .child(inst.summary()),
                            ),
                    ),
            )
    }

    fn render_chips(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        h_flex()
            .flex_none()
            .h(rems(2.25))
            .px(GUTTER)
            .gap_1()
            .overflow_hidden()
            .children(Filter::CHIPS.into_iter().map(|f| {
                let this = this.clone();
                chip(
                    ElementId::Name(format!("inst-chip-{f:?}").into()),
                    f.label(),
                    None,
                    f == self.filter,
                    move |_, _, cx| {
                        let _ = this.update(cx, |v, cx| {
                            v.filter = f;
                            cx.notify();
                        });
                    },
                    cx,
                )
            }))
    }
}

impl Render for InstanceList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::get(cx);
        let (instances, selected) = {
            let s = self.state.read(cx);
            (s.instances.clone(), s.selected)
        };
        let rows: Vec<_> = instances
            .iter()
            .enumerate()
            .filter(|(_, i)| self.filter.accepts(i.loader))
            .map(|(ix, i)| self.render_row(ix, i, ix == selected, cx))
            .collect();
        let empty = rows.is_empty();
        v_flex()
            .flex_none()
            .w(rems(LIST_W))
            .h_full()
            .overflow_hidden()
            .bg(p.bg)
            .border_r_1()
            .border_color(p.border)
            .child(
                h_flex()
                    .flex_none()
                    .h(STRIP_H)
                    .pl(GUTTER)
                    .pr(rems(0.5))
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_size(TEXT_PRIMARY)
                                    .strong(FontWeight::SEMIBOLD)
                                    .text_color(p.text)
                                    .child("实例"),
                            )
                            .child(
                                div()
                                    .text_size(TEXT_SECONDARY)
                                    .text_color(p.muted)
                                    .child(instances.len().to_string()),
                            ),
                    )
                    .child(icon_button("inst-new", IconName::Plus, "新建实例")),
            )
            .child(self.render_chips(cx))
            .child(
                v_flex()
                    .id("inst-rows")
                    .flex_1()
                    .min_h_0()
                    .pt_1()
                    .overflow_y_scroll()
                    .children(rows)
                    .when(empty, |d| {
                        d.child(
                            div()
                                .px(GUTTER)
                                .py_3()
                                .text_size(TEXT_SECONDARY)
                                .text_color(p.muted)
                                .child("没有匹配的实例"),
                        )
                    }),
            )
    }
}
