//! The main window's root (layout A): title bar, then rail | page, then the
//! status bar.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{ActiveTheme, IconName, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Subscription, TestSupportExt, Window, actions, div,
};

use super::placeholder;
use super::{Nav, Rail, ShellState, StatusBar, TitleBarView};
use crate::fonts;

actions!(patina, [FocusSearch]);

/// Key context of the root view. `/` ([`FocusSearch`]) is bound in it
/// unless a text input has the keyboard (see `shell::init`).
pub const ROOT_CONTEXT: &str = "Patina";

/// The main window's content. Owns the shell's regions and the page state.
pub struct RootView {
    shell: Entity<ShellState>,
    title_bar: Entity<TitleBarView>,
    rail: Entity<Rail>,
    status_bar: Entity<StatusBar>,
    focus: FocusHandle,
    _observe: Subscription,
}

impl RootView {
    /// Builds the regions and takes the keyboard, so `/` works at once.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let shell = cx.new(|_| ShellState::new());
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            title_bar: cx.new(|cx| TitleBarView::new(shell.clone(), window, cx)),
            rail: cx.new(|cx| Rail::new(shell.clone(), cx)),
            status_bar: cx.new(|cx| StatusBar::new(window, cx)),
            _observe: cx.observe(&shell, |_, _, cx| cx.notify()),
            shell,
            focus,
        }
    }

    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    pub fn title_bar(&self) -> &Entity<TitleBarView> {
        &self.title_bar
    }

    pub fn status_bar(&self) -> &Entity<StatusBar> {
        &self.status_bar
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.title_bar
            .update(cx, |t, cx| t.focus_search(window, cx));
    }

    /// The page for `nav`. The instances page is mounted by the instance
    /// list (plan 4b, task 6); the others are placeholders until plan 4c.
    fn render_page(&self, nav: Nav) -> AnyElement {
        let page = div()
            .id(nav.page_id())
            .flex_1()
            .min_w_0()
            .h_full()
            .test_support();
        match nav {
            Nav::Instances => page.into_any_element(),
            Nav::Downloads => page
                .child(placeholder::page(Lucide::Download, nav.label()))
                .into_any_element(),
            Nav::Accounts => page
                .child(placeholder::page(IconName::CircleUser, nav.label()))
                .into_any_element(),
            Nav::Settings => page
                .child(placeholder::page(IconName::Settings, nav.label()))
                .into_any_element(),
        }
    }
}

impl Render for RootView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, fg) = (theme.background, theme.foreground);
        let nav = self.shell.read(cx).nav();
        v_flex()
            .key_context(ROOT_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::focus_search))
            .size_full()
            .bg(bg)
            .text_color(fg)
            .font_features(fonts::tnum())
            .child(self.title_bar.clone())
            .child(
                // Full-height columns: stretch them to the row's height.
                h_flex()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .child(self.rail.clone())
                    .child(self.render_page(nav)),
            )
            .child(self.status_bar.clone())
    }
}
