//! The accounts page: a header with the count and "添加微软账号…" (and, in
//! development builds only, "添加离线账号…"), then one two-line row per
//! account: its letter avatar, name and "当前" tag, its kind, and
//! "设为当前" / "移除…" (which asks first).
//!
//! The page reads the accounts again whenever they change
//! ([`AccountsChanged`]) and re-reads `accounts.toml` each time it is shown,
//! so an edit made outside the app shows up. When the file cannot be read
//! the page says why and changes nothing (the file is never written over)
//! until "重新读取" succeeds.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, Context, ElementId, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Rems, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, TestSupportExt, Window, div, px, rems,
};
use patina_core::accounts::Account;
use rust_i18n::t;

use super::{AccountsSnapshot, WindowAction, avatar, kind_label, read_accounts, switch_to};
use crate::content::focus_first_button;
use crate::core_bridge::{AccountsChanged, Core, Revision};
use crate::fonts::Strong;
use crate::layout::{GUTTER, STRIP_H, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY, scroll_y};

/// Height of an account row (two lines).
const ROW_H: Rems = Rems(3.25);
/// The avatar tile in a row (32 px, as the instance list's icons).
const AVATAR: Rems = Rems(2.);

/// Element id of account `id`'s row.
pub fn row_id(id: &str) -> ElementId {
    ElementId::Name(format!("account-row-{id}").into())
}

/// Element id of account `id`'s "设为当前".
pub fn set_current_id(id: &str) -> ElementId {
    ElementId::Name(format!("account-set-current-{id}").into())
}

/// Element id of account `id`'s "移除…".
pub fn remove_id(id: &str) -> ElementId {
    ElementId::Name(format!("account-remove-{id}").into())
}

/// Element id of the "当前" tag on account `id`'s row.
pub fn current_tag_id(id: &str) -> ElementId {
    ElementId::Name(format!("account-current-{id}").into())
}

/// What the page has of the accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Accounts {
    Loading,
    /// They could not be read; the localized reason.
    Failed(String),
    Loaded(AccountsSnapshot),
}

/// The accounts page (an entity). See the module docs.
pub struct AccountsPage {
    accounts: Accounts,
    rev: Revision,
    load: Task<()>,
    /// Accounts with a change in flight; their buttons are disabled.
    changing: HashSet<String>,
    scroll: ScrollHandle,
    _accounts_changed: Subscription,
}

impl AccountsPage {
    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let _accounts_changed =
            cx.observe(&AccountsChanged::epoch(cx), |this, _, cx| this.refresh(cx));
        let mut this = Self {
            accounts: Accounts::Loading,
            rev: Revision::default(),
            load: Task::ready(()),
            changing: HashSet::new(),
            scroll: ScrollHandle::new(),
            _accounts_changed,
        };
        this.refresh(cx);
        this
    }

    /// The page came on show: reads `accounts.toml` again (the change
    /// signal then refreshes the page).
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        Core::reload_accounts(cx).detach();
    }

    /// The accounts on show, once read.
    pub fn snapshot(&self) -> Option<&AccountsSnapshot> {
        match &self.accounts {
            Accounts::Loaded(snapshot) => Some(snapshot),
            _ => None,
        }
    }

    /// Why the accounts could not be read, if they could not.
    pub fn error(&self) -> Option<&str> {
        match &self.accounts {
            Accounts::Failed(error) => Some(error),
            _ => None,
        }
    }

    /// Whether the accounts may be changed: only once they were read.
    pub fn is_editable(&self) -> bool {
        matches!(self.accounts, Accounts::Loaded(_))
    }

    /// Reads the accounts in the background; a newer read supersedes this
    /// one.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let id = self.rev.start();
        let read = read_accounts(cx);
        self.load = cx.spawn(async move |this, cx| {
            let accounts = match read.await {
                Ok(snapshot) => Accounts::Loaded(snapshot),
                Err(error) => {
                    tracing::warn!(%error, "the accounts page cannot read the accounts");
                    Accounts::Failed(error)
                }
            };
            this.update(cx, |this, cx| {
                if this.rev.is_current(id) && this.accounts != accounts {
                    this.accounts = accounts;
                    cx.notify();
                }
            })
            .ok();
        });
    }

    /// Makes account `id` the current one.
    pub fn set_current(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_editable() || !self.changing.insert(id.to_string()) {
            return;
        }
        let switched = switch_to(id.to_string(), window, cx);
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            switched.await;
            this.update(cx, |this, cx| this.changed(&id, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    /// Asks before removing account `id`: `移除"<名称>"？`, 移除 / 取消,
    /// with the keyboard on 取消.
    pub fn confirm_remove(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_editable() || self.changing.contains(id) {
            return;
        }
        let Some(account) = self.account(id).cloned() else {
            return;
        };
        let this = cx.weak_entity();
        let title = SharedString::from(t!("accounts.remove_title", name = account.username));
        // The footer's region: where the keyboard goes to reach 取消, and
        // how Enter tells whether a button has it.
        let footer_focus = cx.focus_handle();
        let first_draw = Cell::new(true);
        window.open_alert_dialog(cx, move |alert, window, cx| {
            focus_first_button(&footer_focus, &first_draw, window, cx);
            let remove: Rc<WindowAction> = {
                let (this, id) = (this.clone(), account.id.clone());
                Rc::new(move |window, cx| {
                    this.update(cx, |this, cx| this.remove(&id, window, cx))
                        .ok();
                })
            };
            let footer = DialogFooter::new()
                .child(
                    Button::new("account-remove-cancel")
                        .label(t!("accounts.cancel").to_string())
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child({
                    let remove = remove.clone();
                    Button::new("account-remove-ok")
                        .danger()
                        .label(t!("accounts.remove_confirm").to_string())
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            remove(window, cx);
                        })
                });
            let footer_focus = footer_focus.clone();
            let body = t!("accounts.remove_body").to_string();
            alert
                .title(
                    div()
                        .id("account-remove-title")
                        .aria_label(title.clone())
                        .test_support()
                        .child(title.clone()),
                )
                .description(
                    div()
                        .id("account-remove-body")
                        .aria_label(body.clone())
                        .test_support()
                        .child(body),
                )
                .footer(div().w_full().track_focus(&footer_focus).child(footer))
                // Enter removes from the dialog itself; on a button it is
                // that button's own key, so on 取消 it cancels.
                .on_ok(move |_, window, cx| {
                    if footer_focus.contains_focused(window, cx) {
                        cx.propagate();
                        return false;
                    }
                    remove(window, cx);
                    true
                })
        });
    }

    /// Removes account `id` (saved, then signalled).
    fn remove(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_editable() || !self.changing.insert(id.to_string()) {
            return;
        }
        let name = (self.account(id)).map_or_else(|| id.to_string(), |a| a.username.clone());
        let removing = id.to_string();
        let change = Core::update_accounts(cx, move |store| store.remove(&removing));
        let id = id.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let result = change.await;
            this.update_in(cx, |this, window, cx| {
                if let Err(error) = result {
                    tracing::warn!(%error, "removing an account failed");
                    let text = t!("accounts.remove_failed", name = name, error = error);
                    window.push_notification(Notification::error(text.to_string()), cx);
                }
                this.changed(&id, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// The change to account `id` has finished.
    fn changed(&mut self, id: &str, cx: &mut Context<Self>) {
        self.changing.remove(id);
        cx.notify();
    }

    fn account(&self, id: &str) -> Option<&Account> {
        self.snapshot()?.accounts.iter().find(|a| a.id == id)
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let editable = self.is_editable();
        let count = self.snapshot().map(|s| s.accounts.len());
        h_flex()
            .flex_none()
            .h(STRIP_H)
            .px(GUTTER)
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_size(TEXT_PRIMARY)
                            .strong(FontWeight::SEMIBOLD, cx)
                            .text_color(theme.foreground)
                            .child(t!("accounts.title").to_string()),
                    )
                    .children(count.map(|count| {
                        div()
                            .id("accounts-count")
                            .aria_label(count.to_string())
                            .test_support()
                            .text_size(TEXT_SECONDARY)
                            .text_color(theme.muted_foreground)
                            .child(count.to_string())
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .children(offline_entry(editable, cx))
                    .child(
                        Button::new("accounts-add-msa")
                            .outline()
                            .small()
                            .icon(Icon::new(IconName::Plus))
                            .label(t!("accounts.add_msa").to_string())
                            .disabled(!editable)
                            .on_click(|_, window, cx| {
                                super::open_login_dialog(window, cx);
                            }),
                    ),
            )
    }

    fn render_row(&self, account: &Account, current: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let busy = self.changing.contains(&account.id);
        let name = SharedString::from(account.username.clone());
        let title = h_flex()
            .min_w_0()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(TEXT_ROW)
                    .strong(FontWeight::MEDIUM, cx)
                    .text_color(theme.foreground)
                    .child(name.clone()),
            )
            .when(current, |title| {
                title.child(
                    div()
                        .id(current_tag_id(&account.id))
                        .test_support()
                        .flex_none()
                        .child(
                            Tag::primary()
                                .outline()
                                .xsmall()
                                .child(t!("accounts.current").to_string()),
                        ),
                )
            });
        let actions =
            h_flex()
                .flex_none()
                .gap_1()
                .when(!current, |actions| {
                    let id = account.id.clone();
                    actions.child(
                        Button::new(set_current_id(&account.id))
                            .outline()
                            .small()
                            .label(t!("accounts.set_current").to_string())
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_current(&id, window, cx)
                            })),
                    )
                })
                .child({
                    let id = account.id.clone();
                    Button::new(remove_id(&account.id))
                        .ghost()
                        .small()
                        .label(t!("accounts.remove").to_string())
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_remove(&id, window, cx)
                        }))
                });
        h_flex()
            .id(row_id(&account.id))
            .test_support()
            .aria_label(name)
            .aria_selected(current)
            .w_full()
            .h(ROW_H)
            .px(GUTTER)
            .gap_2p5()
            .border_b_1()
            .border_color(theme.border)
            .child(avatar(account, AVATAR, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    // 1px between the two lines, as the other lists have it.
                    .gap(px(1.))
                    .child(title)
                    .child(
                        div()
                            .text_size(TEXT_SECONDARY)
                            .text_color(theme.muted_foreground)
                            .child(kind_label(account.kind)),
                    ),
            )
            .child(actions)
            .into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.accounts {
            Accounts::Loading => div().into_any_element(),
            Accounts::Failed(error) => render_failed(error.clone(), cx).into_any_element(),
            Accounts::Loaded(snapshot) if snapshot.accounts.is_empty() => {
                render_empty(cx).into_any_element()
            }
            Accounts::Loaded(snapshot) => {
                let rows: Vec<_> = (snapshot.accounts.iter())
                    .map(|a| self.render_row(a, snapshot.is_current(&a.id), cx))
                    .collect();
                scroll_y(
                    v_flex().id("accounts-list").test_support().children(rows),
                    &self.scroll,
                )
                .into_any_element()
            }
        }
    }
}

/// "添加离线账号…", in development builds only.
#[cfg(feature = "dev-offline")]
fn offline_entry(editable: bool, _: &App) -> Option<Button> {
    Some(
        Button::new("accounts-add-offline")
            .ghost()
            .small()
            .label(t!("accounts.add_offline").to_string())
            .tooltip(t!("accounts.dev_only").to_string())
            .disabled(!editable)
            .on_click(|_, window, cx| super::open_offline_dialog(window, cx)),
    )
}

/// Release builds have no offline accounts.
#[cfg(not(feature = "dev-offline"))]
fn offline_entry(_: bool, _: &App) -> Option<Button> {
    None
}

/// No accounts yet: what to do next.
fn render_empty(cx: &App) -> impl IntoElement {
    let text = SharedString::from(t!("accounts.empty"));
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        // Sits a little above the optical centre, as the other empty
        // states do.
        .pb(rems(3.))
        .child(
            div()
                .id("accounts-empty")
                .aria_label(text.clone())
                .test_support()
                .text_size(TEXT_PRIMARY)
                .text_color(cx.theme().muted_foreground)
                .child(text),
        )
}

/// The accounts could not be read: why, that nothing will be written, and
/// "重新读取".
fn render_failed(error: String, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let error = SharedString::from(error);
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_3()
        .pb(rems(3.))
        .px(GUTTER)
        .child(
            Icon::new(IconName::TriangleAlert)
                .size(rems(1.5))
                .text_color(theme.warning),
        )
        .child(
            div()
                .id("accounts-error")
                .aria_label(error.clone())
                .test_support()
                .max_w(rems(32.))
                .text_size(TEXT_PRIMARY)
                .text_color(theme.foreground)
                .child(error),
        )
        .child(
            div()
                .max_w(rems(32.))
                .text_size(TEXT_SECONDARY)
                .text_color(theme.muted_foreground)
                .child(t!("accounts.failed_hint").to_string()),
        )
        .child(
            Button::new("accounts-reload")
                .outline()
                .small()
                .icon(Icon::new(IconName::RefreshCw))
                .label(t!("accounts.reload").to_string())
                .on_click(|_, _, cx| Core::reload_accounts(cx).detach()),
        )
}

impl Render for AccountsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("accounts")
            .size_full()
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(self.render_body(cx)))
    }
}
