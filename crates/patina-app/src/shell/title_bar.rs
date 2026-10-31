//! The title bar (client-side, `title_bar` fill): mark and name, a centred
//! global search box (`/` focuses it; see [`GlobalSearch`]), then download
//! activity, the current account, settings and the window controls.
//!
//! The bar is gpui-component's `TitleBar`, as in crypto_pro and the
//! prototype: on a client-side decorated window it drags the window and draws
//! the window controls; under server decorations it is a plain bar.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, TitleBar, h_flex};
use gpui_kit::{
    App, AppContext, ClickEvent, Context, Decorations, Div, Entity, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Rems, Render, SharedString,
    Styled, Subscription, Task, Window, div, rems,
};
use rust_i18n::t;

use super::{Nav, ShellState};
use crate::core_bridge::{AccountsChanged, Core, Revision};
use crate::fonts::Strong;
use crate::instances::InstancesPage;
use crate::layout::{GUTTER, STRIP_H, TEXT_PRIMARY};
use crate::search::GlobalSearch;
use crate::theme::ProductColors;

/// What the account button says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AccountLabel {
    /// The accounts have not been read yet; the button shows only its icon.
    #[default]
    Loading,
    /// No current account, or the accounts could not be read.
    SignedOut,
    /// The current account's name.
    SignedIn(SharedString),
}

/// The title bar; owns the global search and the account label.
pub struct TitleBarView {
    shell: Entity<ShellState>,
    search: Entity<GlobalSearch>,
    account: AccountLabel,
    account_rev: Revision,
    account_load: Task<()>,
    _accounts_changed: Subscription,
}

impl TitleBarView {
    /// Reads the current account now and whenever the accounts change
    /// ([`AccountsChanged`]); the search finds `page`'s instances and
    /// their mods, and gives the keyboard to `home_focus` (the shell's)
    /// when it has nowhere else to return it.
    pub fn new(
        shell: Entity<ShellState>,
        page: Entity<InstancesPage>,
        home_focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| GlobalSearch::new(shell.clone(), page, home_focus, window, cx));
        let _accounts_changed = cx.observe(&AccountsChanged::epoch(cx), |this, _, cx| {
            this.refresh_account(cx);
        });
        let mut this = Self {
            shell,
            search,
            account: AccountLabel::default(),
            account_rev: Revision::default(),
            account_load: Task::ready(()),
            _accounts_changed,
        };
        this.refresh_account(cx);
        this
    }

    /// Moves the keyboard to the search box.
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    pub fn search(&self) -> &Entity<GlobalSearch> {
        &self.search
    }

    pub fn account(&self) -> &AccountLabel {
        &self.account
    }

    /// Reads the current account's name in the background; a newer read
    /// supersedes this one.
    pub fn refresh_account(&mut self, cx: &mut Context<Self>) {
        let id = self.account_rev.start();
        let read = Core::with_accounts(cx, |accounts| match accounts {
            Ok(store) => Ok(store.current().map(|a| a.username.clone())),
            Err(e) => Err(e.to_string()),
        });
        self.account_load = cx.spawn(async move |this, cx| {
            let label = match read.await {
                Ok(Ok(Some(name))) => AccountLabel::SignedIn(name.into()),
                Ok(Ok(None)) => AccountLabel::SignedOut,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "accounts unavailable; title bar shows signed out");
                    AccountLabel::SignedOut
                }
                Err(error) => {
                    tracing::warn!(%error, "reading the current account failed");
                    AccountLabel::SignedOut
                }
            };
            this.update(cx, |this, cx| {
                if this.account_rev.is_current(id) && this.account != label {
                    this.account = label;
                    cx.notify();
                }
            })
            .ok();
        });
    }

    fn render_brand(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .child(
                // Placeholder mark: a teal rounded square with a monogram.
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(rems(1.25))
                    .rounded(rems(0.3125))
                    .bg(ProductColors::get(cx).signature())
                    .text_size(rems(0.6875))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.primary_foreground)
                    .child("P"),
            )
            .child(
                div()
                    .text_size(TEXT_PRIMARY)
                    .strong(FontWeight::SEMIBOLD, cx)
                    .text_color(theme.foreground)
                    .child(t!("app.title").to_string()),
            )
    }

    fn render_search(&self) -> impl IntoElement {
        no_drag(div())
            .flex_shrink(1.)
            .w(rems(20.))
            .min_w(rems(9.))
            .child(self.search.clone())
    }

    fn render_actions(&self, client: bool) -> impl IntoElement {
        let downloads = Nav::Downloads.label();
        let account = match &self.account {
            AccountLabel::SignedIn(name) => Some(name.clone()),
            AccountLabel::SignedOut => Some(t!("shell.title.signed_out").to_string().into()),
            AccountLabel::Loading => None,
        };
        let settings = Nav::Settings.label();
        // No `min_w_0`: the group keeps its content width and the search
        // box gives way instead.
        no_drag(h_flex())
            .flex_1()
            .justify_end()
            .gap_1()
            .pr(controls_gap(client))
            .child(
                // Download activity: a placeholder until the download queue
                // exists (plan 4c); opens the downloads page.
                Button::new("title-downloads")
                    .ghost()
                    .small()
                    .icon(Icon::new(Lucide::Download))
                    .tooltip(downloads.clone())
                    .accessibility_label(downloads)
                    .on_click(go(Nav::Downloads, self.shell.clone())),
            )
            .child(Separator::vertical().h(rems(1.)).mx_1())
            .child({
                let button = Button::new("title-account")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::CircleUser))
                    .tooltip(Nav::Accounts.label())
                    .on_click(go(Nav::Accounts, self.shell.clone()));
                match account {
                    Some(name) => button.label(name),
                    None => button.accessibility_label(Nav::Accounts.label()),
                }
            })
            .child(
                Button::new("title-settings")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::Settings))
                    .tooltip(settings.clone())
                    .accessibility_label(settings)
                    .on_click(go(Nav::Settings, self.shell.clone())),
            )
    }
}

/// Clicks on the bar's controls must not start a window move.
fn no_drag(d: Div) -> Div {
    d.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// Room before the window controls, which only a client-decorated window
/// draws.
fn controls_gap(client: bool) -> Rems {
    rems(if client { 0.5 } else { 0. })
}

/// A click handler that shows `nav`.
fn go(nav: Nav, shell: Entity<ShellState>) -> impl Fn(&ClickEvent, &mut Window, &mut App) {
    move |_, _, cx| shell.update(cx, |s, cx| s.navigate(nav, cx))
}

impl Render for TitleBarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let client = matches!(window.window_decorations(), Decorations::Client { .. });
        let theme = cx.theme();
        let (bg, border) = (theme.title_bar, theme.title_bar_border);
        TitleBar::new()
            .h(STRIP_H)
            .pl(GUTTER)
            .pr(controls_gap(client))
            .bg(bg)
            .border_color(border)
            .child(
                h_flex()
                    .size_full()
                    .gap_3()
                    .child(self.render_brand(cx))
                    .child(self.render_search())
                    .child(self.render_actions(client)),
            )
    }
}
