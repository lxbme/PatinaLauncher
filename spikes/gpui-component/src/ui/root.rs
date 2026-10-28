//! Window root (layout A): title bar, then rail | instance column | detail,
//! then the status bar. Other rail sections show a placeholder.

use gpui_kit::component::{IconName, h_flex, v_flex};
use gpui_kit::{
    AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement, KeyBinding,
    ParentElement, Render, Styled, Subscription, Window, actions, div,
};

use crate::state::{AppState, Nav};
use crate::theme::Palette;
use crate::ui::detail::DetailView;
use crate::ui::instance_list::InstanceList;
use crate::ui::rail::Rail;
use crate::ui::title_bar::TitleBarView;
use crate::ui::{empty_state, status_bar, tnum};

actions!(patina, [FocusSearch]);

/// Key context of the root view; `/` focuses the search unless an input
/// has the keyboard (as in crypto_pro).
const ROOT_CONTEXT: &str = "Patina";

pub fn init(cx: &mut gpui_kit::App) {
    cx.bind_keys([KeyBinding::new(
        "/",
        FocusSearch,
        Some(&format!("{ROOT_CONTEXT} && !Input")),
    )]);
}

pub struct RootView {
    state: Entity<AppState>,
    title_bar: Entity<TitleBarView>,
    rail: Entity<Rail>,
    list: Entity<InstanceList>,
    detail: Entity<DetailView>,
    focus: FocusHandle,
    _observe: Subscription,
}

impl RootView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|_| AppState::new());
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            title_bar: cx.new(|cx| TitleBarView::new(state.clone(), window, cx)),
            rail: cx.new(|cx| Rail::new(state.clone(), cx)),
            list: cx.new(|cx| InstanceList::new(state.clone(), cx)),
            detail: cx.new(|cx| DetailView::new(state.clone(), cx)),
            _observe: cx.observe(&state, |_, _, cx| cx.notify()),
            state,
            focus,
        }
    }

    fn render_section(&self, nav: Nav, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::assets::IconName as Lucide;
        let (icon, text): (gpui_kit::component::Icon, &str) = match nav {
            Nav::Downloads => (Lucide::Download.into(), "下载队列：3 项进行中（原型未实现此页）"),
            Nav::Accounts => (IconName::CircleUser.into(), "账号管理（原型未实现此页）"),
            _ => (IconName::Settings.into(), "全局设置（原型未实现此页）"),
        };
        div()
            .flex_1()
            .h_full()
            .bg(Palette::get(cx).bg)
            .child(empty_state(icon, text, None, cx))
    }
}

impl Render for RootView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::get(cx);
        let nav = self.state.read(cx).nav;
        let main = if nav == Nav::Instances {
            h_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(self.list.clone())
                .child(self.detail.clone())
                .into_any_element()
        } else {
            self.render_section(nav, cx).into_any_element()
        };
        let title_bar = self.title_bar.clone();
        v_flex()
            .key_context(ROOT_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(move |_, _: &FocusSearch, window, cx| {
                title_bar.update(cx, |t, cx| t.focus_search(window, cx))
            }))
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .font_features(tnum())
            .child(self.title_bar.clone())
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.rail.clone())
                    .child(main),
            )
            .child(status_bar::render(cx))
    }
}
