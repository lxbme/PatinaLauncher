//! Global rail (far left, 48 px): 实例 / 下载 / 账号, 设置 at the bottom.
//! The active item gets a `raised` fill and an accent bar on the rail's
//! left edge; every item has hover / pressed fills and a tooltip.

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px, rems,
};

use crate::state::{AppState, Nav};
use crate::theme::Palette;

/// Rail width at UI scale 1.0 (48 px).
pub const RAIL_W: f32 = 3.;

pub struct Rail {
    state: Entity<AppState>,
    _observe: Subscription,
}

impl Rail {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self { state, _observe }
    }

    fn item(
        &self,
        nav: Nav,
        icon: impl Into<Icon>,
        tip: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = *Palette::get(cx);
        let on = self.state.read(cx).nav == nav;
        let state = self.state.clone();
        div()
            .relative()
            .w_full()
            .h(rems(2.5))
            .flex()
            .items_center()
            .justify_center()
            .when(on, |d| {
                d.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(rems(0.75))
                        .h(rems(1.))
                        .w(px(3.))
                        .rounded_r(px(2.))
                        .bg(p.accent),
                )
            })
            .child(
                div()
                    .id(tip)
                    .size(rems(2.25))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(rems(0.4375))
                    .cursor_pointer()
                    .when(on, |d| d.bg(p.raised).text_color(p.text))
                    .when(!on, |d| {
                        d.text_color(p.muted)
                            .hover(move |s| s.bg(p.raised.opacity(0.55)).text_color(p.text))
                            .active(move |s| s.bg(p.raised))
                    })
                    .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                    .on_click(move |_, _, cx| state.update(cx, |s, cx| s.navigate(nav, cx)))
                    .child(icon.into().size(rems(1.125))),
            )
    }
}

impl Render for Rail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::get(cx);
        use gpui_kit::assets::IconName as Lucide;
        v_flex()
            .flex_none()
            .w(rems(RAIL_W))
            .h_full()
            .py_1()
            .bg(p.header)
            .border_r_1()
            .border_color(p.border)
            .child(self.item(Nav::Instances, Lucide::Boxes, "实例", cx))
            .child(self.item(Nav::Downloads, Lucide::Download, "下载", cx))
            .child(self.item(Nav::Accounts, IconName::CircleUser, "账号", cx))
            .child(div().flex_1())
            .child(self.item(Nav::Settings, IconName::Settings, "设置", cx))
    }
}
