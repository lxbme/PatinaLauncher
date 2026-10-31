//! The main window's root (layout A): title bar, then rail | page, then the
//! status bar.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use futures::future::Either;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{ActiveTheme, IconName, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Subscription, TestSupportExt, WeakEntity, Window, actions, div,
};

use super::placeholder;
use super::{Nav, OpenDownloads, Rail, ShellState, StatusBar, TitleBarView};
use crate::fonts;
use crate::instances::{InstancesChanged, InstancesPage};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use rust_i18n::t;

use crate::launch::LaunchEvent;
use crate::settings::{AfterLaunch, SettingsStore};

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
    instances: Entity<InstancesPage>,
    focus: FocusHandle,
    _observe: Subscription,
    _instances_changed: Subscription,
    _launch_events: Subscription,
}

impl RootView {
    /// Builds the regions and takes the keyboard, so `/` works at once.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let shell = cx.new(|_| ShellState::new());
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let instances = cx.new(|cx| InstancesPage::new(window, cx));
        let model = instances.read(cx).model().clone();
        // This app's own instance operations change the status bar's
        // count and size at once, without waiting for its throttle.
        let _instances_changed = cx.subscribe(&model, |this, _, _: &InstancesChanged, cx| {
            this.status_bar.update(cx, |s, cx| s.refresh_now(cx));
        });
        let _launch_events = cx.subscribe_in(&model, window, Self::on_launch_event);
        let page = instances.downgrade();
        let closing = Rc::new(Cell::new(false));
        window.on_window_should_close(cx, move |window, cx| {
            close_after_saving(&page, &closing, window, cx)
        });
        Self {
            title_bar: cx.new(|cx| {
                TitleBarView::new(shell.clone(), instances.clone(), focus.clone(), window, cx)
            }),
            rail: cx.new(|cx| Rail::new(shell.clone(), cx)),
            status_bar: cx.new(|cx| StatusBar::new(window, cx)),
            instances,
            _observe: cx.observe(&shell, |this, shell, cx| {
                let shown = shell.read(cx).nav() == Nav::Instances;
                this.instances
                    .update(cx, |page, cx| page.set_shown(shown, cx));
                cx.notify();
            }),
            _instances_changed,
            _launch_events,
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

    pub fn instances(&self) -> &Entity<InstancesPage> {
        &self.instances
    }

    /// The launch flow's requests for the shell and the window.
    fn on_launch_event(
        &mut self,
        _: &Entity<crate::instances::InstancesModel>,
        event: &LaunchEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            LaunchEvent::ShowAccounts => {
                self.shell.update(cx, |s, cx| s.navigate(Nav::Accounts, cx));
            }
            LaunchEvent::Spawned {
                warnings,
                record_saved,
                ..
            } => {
                for warning in warnings {
                    window.push_notification(Notification::warning(warning.clone()), cx);
                }
                match SettingsStore::get(cx).after_launch {
                    AfterLaunch::Keep => {}
                    AfterLaunch::Minimize => window.minimize_window(),
                    // The game keeps running: quitting drops its session,
                    // which keeps the running record for the next start to
                    // reattach.
                    AfterLaunch::Close if *record_saved => cx.quit(),
                    // Without the record nothing could find the game again.
                    AfterLaunch::Close => {
                        let text = t!("launch.close_skipped").to_string();
                        window.push_notification(Notification::warning(text), cx);
                    }
                }
            }
            LaunchEvent::ConfirmWarnings { .. } => {}
        }
    }

    /// Shows the downloads page. Its target and query are not used yet: the
    /// downloads page takes them over once it exists (plan 4c, Task 6).
    fn open_downloads(&mut self, _: &OpenDownloads, _: &mut Window, cx: &mut Context<Self>) {
        self.shell
            .update(cx, |s, cx| s.navigate(Nav::Downloads, cx));
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.title_bar
            .update(cx, |t, cx| t.focus_search(window, cx));
    }

    /// The page for `nav`. Every page but the instances page is a
    /// placeholder until plan 4c.
    fn render_page(&self, nav: Nav) -> AnyElement {
        let page = div()
            .id(nav.page_id())
            .flex_1()
            .min_w_0()
            .h_full()
            .test_support();
        match nav {
            Nav::Instances => page.child(self.instances.clone()).into_any_element(),
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

/// How long closing the window waits for the settings form's save: the
/// runtime's shutdown budget.
const CLOSE_BUDGET: Duration = Duration::from_secs(2);

/// Whether the window may close now. With a settings form open, what was
/// typed into it is saved first (waiting at most [`CLOSE_BUDGET`]), then
/// the window closes by itself.
fn close_after_saving(
    page: &WeakEntity<InstancesPage>,
    closing: &Rc<Cell<bool>>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if closing.get() {
        return true;
    }
    let form = page
        .upgrade()
        .and_then(|page| page.read(cx).detail().read(cx).settings_view().cloned());
    let Some(form) = form else {
        return true;
    };
    closing.set(true);
    let saved = form.update(cx, |form, cx| form.flush(window, cx));
    window
        .spawn(cx, async move |cx| {
            let budget = cx.background_executor().timer(CLOSE_BUDGET);
            if let Either::Right(_) = futures::future::select(Box::pin(saved), budget).await {
                tracing::warn!("closing before the settings were saved");
            }
            cx.update(|window, _| window.remove_window()).ok();
        })
        .detach();
    false
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
            .on_action(cx.listener(Self::open_downloads))
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
