//! The main window's root (layout A): title bar, then rail | page, then the
//! status bar.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use futures::future::Either;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Subscription, TestSupportExt, WeakEntity, Window, actions, div,
};

use super::{Nav, OpenDownloads, Rail, ShellState, StatusBar, TitleBarView};
use crate::accounts::AccountsPage;
use crate::downloads::DownloadsPage;
use crate::fonts;
use crate::instances::{InstancesChanged, InstancesPage};
use crate::settings_page::SettingsPage;
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
    downloads: Entity<DownloadsPage>,
    accounts: Entity<AccountsPage>,
    settings: Entity<SettingsPage>,
    focus: FocusHandle,
    _observe: Subscription,
    _focus_lost: Subscription,
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
        let downloads = cx.new(|cx| DownloadsPage::new(model.clone(), window, cx));
        let accounts = cx.new(|cx| AccountsPage::new(window, cx));
        let settings = cx.new(|cx| SettingsPage::new(model.clone(), window, cx));
        // This app's own instance operations change the status bar's
        // count and size at once, without waiting for its throttle.
        let _instances_changed = cx.subscribe(&model, |this, _, _: &InstancesChanged, cx| {
            this.status_bar.update(cx, |s, cx| s.refresh_now(cx));
        });
        let _launch_events = cx.subscribe_in(&model, window, Self::on_launch_event);
        // When the focused element leaves the window (its page was switched
        // away), the keyboard would reach nothing and `/` would be dead:
        // give it to the closest ancestor still drawn, else to the root.
        let _focus_lost = cx.on_focus_lost(window, |this, window, cx| {
            let target = window
                .focus_lost_restore_target(cx)
                .unwrap_or_else(|| this.focus.clone());
            target.focus(window, cx);
        });
        let page = instances.downgrade();
        let settings_page = settings.downgrade();
        let closing = Rc::new(Cell::new(false));
        window.on_window_should_close(cx, move |window, cx| {
            close_after_saving(&page, &settings_page, &closing, window, cx)
        });
        Self {
            title_bar: cx.new(|cx| {
                TitleBarView::new(shell.clone(), instances.clone(), focus.clone(), window, cx)
            }),
            rail: cx.new(|cx| Rail::new(shell.clone(), cx)),
            status_bar: cx.new(|cx| StatusBar::new(window, cx)),
            instances,
            downloads,
            accounts,
            settings,
            _observe: cx.observe(&shell, |this, shell, cx| {
                let nav = shell.read(cx).nav();
                this.instances
                    .update(cx, |page, cx| page.set_shown(nav == Nav::Instances, cx));
                match nav {
                    Nav::Downloads => this.downloads.update(cx, |page, cx| page.shown(cx)),
                    Nav::Accounts => this.accounts.update(cx, |page, cx| page.shown(cx)),
                    Nav::Settings => this.settings.update(cx, |page, cx| page.shown(cx)),
                    Nav::Instances => {}
                }
                cx.notify();
            }),
            _focus_lost,
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

    pub fn downloads(&self) -> &Entity<DownloadsPage> {
        &self.downloads
    }

    pub fn accounts(&self) -> &Entity<AccountsPage> {
        &self.accounts
    }

    pub fn settings(&self) -> &Entity<SettingsPage> {
        &self.settings
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
                // Work that quitting would cut short: operations, launches,
                // stops, and a managed runtime being deleted.
                let busy = {
                    let model = self.instances.read(cx).model();
                    model.read(cx).activity().read(cx).has_work_in_flight()
                        || self.settings.read(cx).is_removing_runtime()
                };
                let setting = SettingsStore::get(cx).after_launch;
                match after_launch_step(setting, *record_saved, busy) {
                    AfterLaunchStep::Stay => {}
                    AfterLaunchStep::Minimize => window.minimize_window(),
                    AfterLaunchStep::Quit => cx.quit(),
                    AfterLaunchStep::Skip(key) => {
                        let text = t!(key).to_string();
                        window.push_notification(Notification::warning(text), cx);
                    }
                }
            }
            LaunchEvent::ConfirmWarnings { .. } => {}
        }
    }

    /// Shows the downloads page, aimed at the action's target instance and
    /// searching its words.
    fn open_downloads(
        &mut self,
        action: &OpenDownloads,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (target, query) = (action.target.as_deref(), action.query.as_deref());
        self.downloads
            .update(cx, |page, cx| page.open(target, query, window, cx));
        self.shell
            .update(cx, |s, cx| s.navigate(Nav::Downloads, cx));
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.title_bar
            .update(cx, |t, cx| t.focus_search(window, cx));
    }

    /// The page for `nav`.
    fn render_page(&self, nav: Nav) -> AnyElement {
        let page = div()
            .id(nav.page_id())
            .flex_1()
            .min_w_0()
            .h_full()
            .test_support();
        match nav {
            Nav::Instances => page.child(self.instances.clone()).into_any_element(),
            Nav::Downloads => page.child(self.downloads.clone()).into_any_element(),
            Nav::Accounts => page.child(self.accounts.clone()).into_any_element(),
            Nav::Settings => page.child(self.settings.clone()).into_any_element(),
        }
    }
}

/// What the window does once a game has started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterLaunchStep {
    Stay,
    Minimize,
    Quit,
    /// The setting says close, but quitting now would lose something: stay,
    /// and say why (the locale key).
    Skip(&'static str),
}

/// What the `setting` asks for once a game started, given whether its
/// running record was saved and whether other work is under way.
fn after_launch_step(setting: AfterLaunch, record_saved: bool, busy: bool) -> AfterLaunchStep {
    match setting {
        AfterLaunch::Keep => AfterLaunchStep::Stay,
        AfterLaunch::Minimize => AfterLaunchStep::Minimize,
        // Without the record nothing could find the game again.
        AfterLaunch::Close if !record_saved => AfterLaunchStep::Skip("launch.close_skipped"),
        // Quitting would cut an install, a launch or a stop short.
        AfterLaunch::Close if busy => AfterLaunchStep::Skip("launch.close_skipped_busy"),
        // The game keeps running: quitting drops its session, which keeps
        // the running record for the next start to reattach.
        AfterLaunch::Close => AfterLaunchStep::Quit,
    }
}

/// How long closing the window waits for the settings form's save: the
/// runtime's shutdown budget.
const CLOSE_BUDGET: Duration = Duration::from_secs(2);

/// Whether the window may close now. Instance and global settings validate
/// focused inputs and drain queued saves together (at most [`CLOSE_BUDGET`]),
/// then the window closes by itself.
fn close_after_saving(
    page: &WeakEntity<InstancesPage>,
    settings: &WeakEntity<SettingsPage>,
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
    let settings = settings.upgrade();
    if form.is_none() && settings.is_none() {
        return true;
    }
    closing.set(true);
    let instance_saved = form.map(|form| form.update(cx, |form, cx| form.flush(window, cx)));
    let global_saved = settings.map(|page| page.update(cx, |page, cx| page.flush(window, cx)));
    let saved = async move {
        futures::join!(
            async move {
                if let Some(saved) = instance_saved {
                    saved.await;
                }
            },
            async move {
                if let Some(saved) = global_saved {
                    saved.await;
                }
            }
        );
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_after_launch_waits_for_other_work() {
        use AfterLaunchStep::*;
        assert_eq!(after_launch_step(AfterLaunch::Keep, true, true), Stay);
        assert_eq!(
            after_launch_step(AfterLaunch::Minimize, true, true),
            Minimize
        );
        assert_eq!(after_launch_step(AfterLaunch::Close, true, false), Quit);
        assert_eq!(
            after_launch_step(AfterLaunch::Close, true, true),
            Skip("launch.close_skipped_busy")
        );
        assert_eq!(
            after_launch_step(AfterLaunch::Close, false, true),
            Skip("launch.close_skipped")
        );
        assert_eq!(
            after_launch_step(AfterLaunch::Close, false, false),
            Skip("launch.close_skipped")
        );
    }
}
