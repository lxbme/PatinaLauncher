//! The title bar's account button: it names the current account and opens a
//! menu of the accounts (the current one checked) to switch with one click,
//! then "管理账号…" for the accounts page. The menu is gpui-component's
//! dropdown menu, so arrow keys, Enter and Escape work in it, and the button
//! is a Tab stop that opens it with Enter or Space.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{Icon, IconName, Sizable};
use gpui_kit::{Anchor, IntoElement, SharedString};
use rust_i18n::t;

use super::{AccountsSnapshot, WindowAction, switch_to};

/// Element id of the account button.
pub const SWITCHER_ID: &str = "title-account";

/// The account button. `label` is what it says (`None`: only its icon);
/// `accounts` the accounts to list (`None` when they could not be read);
/// `manage` shows the accounts page.
pub fn account_switcher(
    label: Option<SharedString>,
    accounts: Option<Rc<AccountsSnapshot>>,
    manage: Rc<WindowAction>,
) -> impl IntoElement {
    let name = t!("accounts.switcher.tooltip").to_string();
    let button = Button::new(SWITCHER_ID)
        .ghost()
        .small()
        .icon(Icon::new(IconName::CircleUser))
        .tooltip(name.clone())
        .dropdown_caret(true);
    let button = match label {
        Some(label) => button.label(label),
        None => button.accessibility_label(name),
    };
    button.dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
        if let Some(snapshot) = &accounts {
            for account in &snapshot.accounts {
                let id = account.id.clone();
                let current = snapshot.is_current(&account.id);
                menu = menu.item(
                    PopupMenuItem::new(account.username.clone())
                        .checked(current)
                        .on_click(move |_, window, cx| {
                            if !current {
                                switch_to(id.clone(), window, cx).detach();
                            }
                        }),
                );
            }
            if !snapshot.accounts.is_empty() {
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
