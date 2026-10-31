//! Account views read coherent service snapshots; only the service persists identities.

pub(crate) mod avatar;
#[cfg(feature = "dev-offline")]
mod fake_login;
mod login_dialog;
mod offline_dialog;
mod page;
mod switcher;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{AnyElement, App, Rems, Task, Window};
use patina_core::accounts::{Account, AccountKind, AccountSnapshot, OfflineOrigin};
use rust_i18n::t;

use crate::core_bridge::Core;
use crate::instances::{IconSource, instance_icon, tint_for};

#[cfg(feature = "dev-offline")]
pub use fake_login::{FAKE_CODE, FAKE_URI, FAKE_USERNAME, FakeLogin};
pub use login_dialog::{LoginDialog, LoginState, open as open_login_dialog};
#[cfg(feature = "dev-offline")]
pub use offline_dialog::open_development as open_dev_offline_dialog;
pub use offline_dialog::{OfflineForm, name_error, open as open_offline_dialog};
pub use page::{AccountsPage, current_tag_id, remove_id, row_id as account_row_id, set_current_id};
pub use switcher::{SWITCHER_ID, account_switcher};

/// A command that acts on a window (a menu item's or a dialog button's).
pub type WindowAction = dyn Fn(&mut Window, &mut App);

/// Reads the service's immutable, coherent snapshot off the UI thread.
pub fn read_accounts(cx: &mut App) -> Task<Result<AccountSnapshot, String>> {
    Core::read_accounts(cx)
}

/// Makes account `id` the current one (saved, then signalled); a failure
/// is reported as a notification on `window`.
pub fn switch_to(id: String, window: &mut Window, cx: &mut App) -> Task<()> {
    let service = Core::global(cx).accounts();
    let change = crate::core_bridge::run(cx, async move { service.set_current(&id).await });
    window.spawn(cx, async move |cx| {
        let result = change.await;
        if matches!(
            &result,
            Ok(Err(
                patina_core::accounts::error::AuthError::ReauthenticationRequired
                    | patina_core::accounts::error::AuthError::CredentialMissing
            ))
        ) {
            cx.update(|window, cx| {
                open_login_dialog(window, cx);
            })
            .ok();
            return;
        }
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => e.to_string(),
            Err(e) => e.to_string(),
        };
        tracing::warn!(%error, "switching the current account failed");
        let text = t!("accounts.switch_failed", error = error).to_string();
        cx.update(|window, cx| window.push_notification(Notification::error(text), cx))
            .ok();
    })
}

/// Read-only retained skin head, or the unchanged UUID-tinted letter tile.
pub fn avatar(account: &Account, size: Rems, cx: &App) -> AnyElement {
    use gpui_kit::{
        InteractiveElement, IntoElement, ObjectFit, ParentElement, StatefulInteractiveElement,
        Styled, StyledImage, TestSupportExt, div, img, rems,
    };
    if let Some(image) = avatar::cached_image(account, cx) {
        return div()
            .id(format!("account-head-{}", account.id))
            .test_support()
            .aria_label(account.username.clone())
            .flex_none()
            .size(size)
            .child(
                img(image)
                    .size_full()
                    .rounded(rems(size.0 * 0.22))
                    .object_fit(ObjectFit::Cover),
            )
            .into_any_element();
    }
    let ch = (account.username.chars().next())
        .and_then(|c| c.to_uppercase().next())
        .unwrap_or('?');
    let tint = tint_for(&account.uuid.to_string());
    instance_icon(&IconSource::Letter { ch, tint }, size, cx)
}

/// "微软账号" / "离线账号".
pub fn kind_label(account: &Account) -> String {
    match account.kind {
        AccountKind::Msa => t!("accounts.kind.msa"),
        AccountKind::Offline if account.offline_origin() == Some(OfflineOrigin::Development) => {
            t!("accounts.kind.development")
        }
        AccountKind::Offline => t!("accounts.kind.offline"),
    }
    .to_string()
}
