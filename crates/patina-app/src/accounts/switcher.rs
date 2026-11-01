//! The title bar's account button: it names the current account and opens a
//! menu of the accounts (the current one checked) to switch with one click,
//! then "管理账号…" for the accounts page. The menu is gpui-component's
//! dropdown menu, so arrow keys, Enter and Escape work in it, and the button
//! is a Tab stop that opens it with Enter or Space.
use patina_core::accounts::AccountSnapshot;

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme, Icon, IconName, Side, Sizable, h_flex, v_flex};
use gpui_kit::{
    Anchor, App, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, div, rems,
};
use rust_i18n::t;

use super::{WindowAction, switch_to};

/// Element id of the account button.
pub const SWITCHER_ID: &str = "title-account";

/// The account button. `label` is what it says (`None`: only its icon);
/// `accounts` the accounts to list (`None` when they could not be read);
/// `manage` shows the accounts page.
pub fn account_switcher(
    label: Option<SharedString>,
    accounts: Option<Rc<AccountSnapshot>>,
    manage: Rc<WindowAction>,
    cx: &App,
) -> impl IntoElement {
    let name = t!("accounts.switcher.tooltip").to_string();
    let button = Button::new(SWITCHER_ID)
        .ghost()
        .small()
        .tooltip(name.clone())
        .dropdown_caret(true);
    let button = match accounts.as_ref().and_then(|snapshot| snapshot.current()) {
        Some(account) => {
            let head = super::avatar::cached_image(account, cx).is_some();
            button.child(
                div()
                    .id(if head {
                        "switcher-head"
                    } else {
                        "switcher-letter"
                    })
                    .test_support()
                    .flex_none()
                    .child(super::avatar(account, rems(1.), cx)),
            )
        }
        None => button.icon(Icon::new(IconName::CircleUser)),
    };
    let button = match label {
        Some(label) => button.label(label),
        None => button.accessibility_label(name),
    };
    button.dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, _| {
        let mut menu = menu
            .min_w(rems(15.).to_pixels(window.rem_size()))
            .max_w(rems(22.).to_pixels(window.rem_size()))
            .check_side(Side::Right);
        if let Some(snapshot) = &accounts {
            if let Some(current) = snapshot.current() {
                let current = current.clone();
                menu = menu
                    .item(
                        PopupMenuItem::element(move |_, cx| {
                            v_flex()
                                .min_w_0()
                                .py_1()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(t!("accounts.switcher.current").to_string()),
                                )
                                .child(
                                    div()
                                        .id("account-menu-current")
                                        .test_support()
                                        .aria_label(current.username.clone())
                                        .text_sm()
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .text_color(cx.theme().foreground)
                                        .truncate()
                                        .child(current.username.clone()),
                                )
                        })
                        .disabled(true),
                    )
                    .separator();
            }
            for account in snapshot.accounts() {
                let id = account.id.clone();
                let current = snapshot.current().is_some_and(|a| a.id == account.id);
                let account_row = account.clone();
                menu = menu.item(
                    PopupMenuItem::element(move |_, cx| {
                        h_flex()
                            .min_w_0()
                            .flex_1()
                            .gap_2()
                            .py_1()
                            .child(
                                div()
                                    .id(format!("account-menu-avatar-{}", account_row.id))
                                    .test_support()
                                    .flex_none()
                                    .child(super::avatar(&account_row, rems(1.5), cx)),
                            )
                            .child(
                                div()
                                    .id(format!("account-menu-name-{}", account_row.id))
                                    .min_w_0()
                                    .truncate()
                                    .aria_label(account_row.username.clone())
                                    .test_support()
                                    .child(account_row.username.clone()),
                            )
                    })
                    .checked(current)
                    .disabled(
                        account.kind == patina_core::accounts::AccountKind::Offline
                            && !snapshot.is_launchable(&account.id),
                    )
                    .on_click(move |_, window, cx| {
                        if !current {
                            switch_to(id.clone(), window, cx).detach();
                        }
                    }),
                );
            }
            if !snapshot.accounts().is_empty() {
                menu = menu.separator();
            }
        }
        let manage = manage.clone();
        menu.item(
            PopupMenuItem::element(|_, _| {
                h_flex()
                    .gap_2()
                    .py_1()
                    .child(
                        div()
                            .size_6()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::Settings).small()),
                    )
                    .child(
                        div()
                            .id("account-menu-manage")
                            .test_support()
                            .aria_label(t!("accounts.switcher.manage").to_string())
                            .child(t!("accounts.switcher.manage").to_string()),
                    )
            })
            .on_click(move |_, window, cx| manage(window, cx)),
        )
    })
}
