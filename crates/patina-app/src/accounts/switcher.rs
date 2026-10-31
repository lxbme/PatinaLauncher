//! The title bar's account button: it names the current account and opens a
//! menu of the accounts (the current one checked) to switch with one click,
//! then "管理账号…" for the accounts page. The menu is gpui-component's
//! dropdown menu, so arrow keys, Enter and Escape work in it, and the button
//! is a Tab stop that opens it with Enter or Space.
use patina_core::accounts::AccountSnapshot;

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{Icon, IconName, Sizable};
use gpui_kit::{
    Anchor, App, InteractiveElement, IntoElement, ParentElement, SharedString, Styled,
    TestSupportExt, div, rems,
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
    button.dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
        if let Some(snapshot) = &accounts {
            for account in snapshot.accounts() {
                let id = account.id.clone();
                let current = snapshot.current().is_some_and(|a| a.id == account.id);
                menu = menu.item(
                    PopupMenuItem::new(account.username.clone())
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
            PopupMenuItem::new(t!("accounts.switcher.manage").to_string())
                .on_click(move |_, window, cx| manage(window, cx)),
        )
    })
}
