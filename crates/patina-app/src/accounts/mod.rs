//! Accounts: the accounts page ([`AccountsPage`]), the "add Microsoft
//! account" device-code dialog ([`LoginDialog`]), the title bar's account
//! switcher ([`account_switcher`]) and, in development builds only
//! (`dev-offline`), the "add offline account" dialog and the fake sign-in.
//!
//! Every view reads the accounts through [`read_accounts`] when
//! [`AccountsChanged`](crate::core_bridge::AccountsChanged) says they
//! changed, and every change goes through
//! [`Core::update_accounts`](crate::core_bridge::Core::update_accounts),
//! which saves and then signals. Signing in only walks the dialog for now:
//! the real flow and storing its account arrive with plan 3.

#[cfg(feature = "dev-offline")]
mod fake_login;
mod login_dialog;
#[cfg(feature = "dev-offline")]
mod offline_dialog;
mod page;
mod switcher;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{AnyElement, App, AppContext as _, Rems, Task, Window};
use patina_core::accounts::{Account, AccountKind};
use rust_i18n::t;

use crate::core_bridge::Core;
use crate::instances::{IconSource, instance_icon, tint_for};

#[cfg(feature = "dev-offline")]
pub use fake_login::{FAKE_CODE, FAKE_URI, FAKE_USERNAME, FakeLogin};
pub use login_dialog::{LoginDialog, LoginState, open as open_login_dialog};
#[cfg(feature = "dev-offline")]
pub use offline_dialog::{OfflineForm, name_error, open as open_offline_dialog};
pub use page::{AccountsPage, current_tag_id, remove_id, row_id as account_row_id, set_current_id};
pub use switcher::{SWITCHER_ID, account_switcher};

/// A command that acts on a window (a menu item's or a dialog button's).
pub type WindowAction = dyn Fn(&mut Window, &mut App);

/// The accounts as one read saw them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountsSnapshot {
    pub accounts: Vec<Account>,
    /// The id of the current account.
    pub current: Option<String>,
}

impl AccountsSnapshot {
    /// The current account.
    pub fn current(&self) -> Option<&Account> {
        let id = self.current.as_deref()?;
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn is_current(&self, id: &str) -> bool {
        self.current.as_deref() == Some(id)
    }
}

/// Reads the accounts on the blocking pool. Resolves to the localized
/// reason when they could not be loaded (the file is then left alone).
pub fn read_accounts(cx: &mut App) -> Task<Result<AccountsSnapshot, String>> {
    let read = Core::with_accounts(cx, |loaded| match loaded {
        Ok(store) => Ok(AccountsSnapshot {
            accounts: store.accounts().to_vec(),
            current: store.current().map(|a| a.id.clone()),
        }),
        Err(e) => Err(e.to_string()),
    });
    cx.background_spawn(async move { read.await.unwrap_or_else(|e| Err(e.to_string())) })
}

/// Makes account `id` the current one (saved, then signalled); a failure
/// is reported as a notification on `window`.
pub fn switch_to(id: String, window: &mut Window, cx: &mut App) -> Task<()> {
    let change = Core::update_accounts(cx, move |store| store.set_current(&id));
    window.spawn(cx, async move |cx| {
        if let Err(error) = change.await {
            tracing::warn!(%error, "switching the current account failed");
            let text = t!("accounts.switch_failed", error = error).to_string();
            cx.update(|window, cx| window.push_notification(Notification::error(text), cx))
                .ok();
        }
    })
}

/// The account's avatar: the first letter of its name on a tile whose tint
/// follows its uuid (the skin's head arrives with plan 3).
pub fn avatar(account: &Account, size: Rems, cx: &App) -> AnyElement {
    let ch = (account.username.chars().next())
        .and_then(|c| c.to_uppercase().next())
        .unwrap_or('?');
    let tint = tint_for(&account.uuid.to_string());
    instance_icon(&IconSource::Letter { ch, tint }, size, cx)
}

/// "微软账号" / "离线账号".
pub fn kind_label(kind: AccountKind) -> String {
    match kind {
        AccountKind::Msa => t!("accounts.kind.msa"),
        AccountKind::Offline => t!("accounts.kind.offline"),
    }
    .to_string()
}
