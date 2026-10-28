//! Title bar (client-side, `header` fill): app mark, a centred search pill
//! (`/` focuses it), then account, download activity, the gear and the
//! window controls.
//!
//! The bar is gpui-component's `TitleBar` (as in crypto_pro's
//! `title_bar.rs`): on a client-side decorated window (niri) it drags the
//! window and draws the window controls; under server decorations it is a
//! plain bar.

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Icon, IconName, Sizable, StyledExt, TitleBar, h_flex, v_flex};
use gpui_kit::{
    AppContext, Context, Decorations, Div, Entity, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, div, px, rems,
};

use crate::state::{AppState, Nav};
use crate::theme::Palette;
use crate::ui::{GUTTER, STRIP_H, TEXT_PRIMARY, TEXT_SECONDARY, icon_button};

/// Placeholder logo: teal rounded square with a white monogram.
fn brand_mark(p: &Palette) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(rems(1.25))
        .rounded(rems(0.3125))
        .bg(p.teal)
        .text_size(rems(0.6875))
        .font_bold()
        .text_color(gpui_kit::white())
        .child("P")
}

pub struct TitleBarView {
    state: Entity<AppState>,
    search: Entity<InputState>,
}

impl TitleBarView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("按 / 搜索实例、Mod")
                .clean_on_escape()
        });
        Self { state, search }
    }

    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Account: square avatar placeholder, name, account kind.
    fn account(&self, p: &Palette) -> impl IntoElement {
        let (hover, active) = (p.raised, p.selected);
        h_flex()
            .id("title-account")
            .flex_none()
            .h(rems(1.75))
            .px_1p5()
            .gap_2()
            .whitespace_nowrap()
            .rounded(rems(0.375))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .active(move |s| s.bg(active))
            .child(
                // A blocky Steve-ish face: skin with a hair band on top.
                v_flex()
                    .size(rems(1.25))
                    .rounded(rems(0.1875))
                    .overflow_hidden()
                    .bg(gpui_kit::rgb(0xc69070))
                    .child(div().h(rems(0.375)).w_full().bg(gpui_kit::rgb(0x3f2a1d))),
            )
            .child(
                div()
                    .text_size(TEXT_PRIMARY)
                    .font_medium()
                    .text_color(p.text)
                    .child("Steve"),
            )
            .child(
                div()
                    .text_size(TEXT_SECONDARY)
                    .text_color(p.muted)
                    .child("微软账号"),
            )
    }

    /// Download activity: icon, `下载 3 项 · 42%`, a hairline progress bar.
    fn downloads(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let (hover, active) = (p.raised, p.selected);
        let state = self.state.clone();
        h_flex()
            .id("title-downloads")
            .flex_none()
            .h(rems(1.75))
            .px_2()
            .gap_1p5()
            .whitespace_nowrap()
            .rounded(rems(0.375))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .active(move |s| s.bg(active))
            .on_click(cx.listener(move |_, _, _, cx| {
                state.update(cx, |s, cx| s.navigate(Nav::Downloads, cx))
            }))
            .text_color(p.teal)
            .child(Icon::new(gpui_kit::assets::IconName::Download).xsmall())
            .child(
                div()
                    .text_size(TEXT_SECONDARY)
                    .text_color(p.muted)
                    .child("下载 3 项"),
            )
            .child(
                div()
                    .w(rems(3.))
                    .h(px(3.))
                    .rounded_full()
                    .bg(p.border)
                    .child(
                        div()
                            .h_full()
                            .w(gpui_kit::relative(0.42))
                            .rounded_full()
                            .bg(p.teal),
                    ),
            )
            .child(
                div()
                    .text_size(TEXT_SECONDARY)
                    .text_color(p.text)
                    .child("42%"),
            )
    }
}

impl Render for TitleBarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let client = matches!(window.window_decorations(), Decorations::Client { .. });
        let p = *Palette::get(cx);
        // Clicks on the controls must not start a window move.
        let no_drag =
            |d: Div| d.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        let state = self.state.clone();
        TitleBar::new()
            .h(STRIP_H)
            .pl(GUTTER)
            .pr(rems(if client { 0.5 } else { 0. }))
            .bg(p.header)
            .border_color(p.border)
            .child(
                h_flex()
                    .size_full()
                    .gap_3()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(brand_mark(&p))
                            .child(
                                div()
                                    .text_size(TEXT_PRIMARY)
                                    .font_semibold()
                                    .text_color(p.text)
                                    .child("Patina"),
                            ),
                    )
                    .child(
                        no_drag(div())
                            .flex_shrink(1.)
                            .w(rems(20.))
                            .min_w(rems(9.))
                            .child(
                            Input::new(&self.search)
                                .small()
                                .cleanable(true)
                                .rounded_full()
                                .bg(p.bg)
                                .prefix(Icon::new(IconName::Search).xsmall().text_color(p.muted)),
                        ),
                    )
                    .child(
                        // No `min_w_0`: the right group keeps its content
                        // width and the search pill gives way instead.
                        no_drag(h_flex())
                            .flex_1()
                            .justify_end()
                            .gap_1()
                            .pr(rems(if client { 0.5 } else { 0. }))
                            .child(self.downloads(&p, cx))
                            .child(div().w(px(1.)).h(rems(1.)).mx_1().bg(p.border))
                            .child(self.account(&p))
                            .child(
                                icon_button("title-settings", IconName::Settings, "设置")
                                    .ml_1()
                                    .on_click(move |_, _, cx| {
                                        state.update(cx, |s, cx| s.navigate(Nav::Settings, cx))
                                    }),
                            ),
                    ),
            )
    }
}
