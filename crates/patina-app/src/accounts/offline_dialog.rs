//! "添加离线账号（仅开发构建）": a player name, checked as it is typed
//! (3–16 letters, digits or underscores, the core's rule and wording), then
//! "添加". Development builds (`dev-offline`) only; release builds do not
//! contain this file.
//!
//! "添加" (also Enter in the name) is disabled while the name is invalid.
//! Adding closes the dialog at once; a failure is reported as a
//! notification. A name that is already there (any case) keeps its account.

#![cfg(feature = "dev-offline")]

use std::cell::Cell;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme, Disableable, WindowExt, v_flex};
use gpui_kit::{
    App, AppContext, Context, Entity, EventEmitter, Focusable, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, TestSupportExt, Window, div,
};
use patina_core::accounts::AccountError;
use rust_i18n::t;

use crate::core_bridge::Core;
use crate::fonts::Strong;
use crate::layout::{TEXT_PRIMARY, TEXT_SECONDARY};

/// Why `name` cannot be an offline player name, in the core's words; `None`
/// when it can, or while nothing is typed.
pub fn name_error(name: &str) -> Option<String> {
    let valid = (3..=16).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    (!valid && !name.is_empty()).then(|| AccountError::InvalidName(name.to_string()).to_string())
}

/// The dialog's form (an entity): the name input.
pub struct OfflineForm {
    name: Entity<InputState>,
    _changed: Subscription,
}

/// Emitted when whether the form can be submitted may have changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameChanged;

impl EventEmitter<NameChanged> for OfflineForm {}

impl OfflineForm {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx));
        let _changed = cx.subscribe(&name, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.emit(NameChanged);
                cx.notify();
            }
        });
        Self { name, _changed }
    }

    pub fn name_input(&self) -> &Entity<InputState> {
        &self.name
    }

    /// The name "添加" would add, or `None` while it is disabled.
    pub fn request(&self, cx: &App) -> Option<String> {
        let name = self.name.read(cx).value().trim().to_string();
        (!name.is_empty() && name_error(&name).is_none()).then_some(name)
    }

    /// Starts adding `form`'s account if the name is valid; returns whether
    /// it did (the dialog then closes).
    pub fn add(form: &Entity<Self>, window: &mut Window, cx: &mut App) -> bool {
        let Some(name) = form.read(cx).request(cx) else {
            return false;
        };
        let added = {
            let name = name.clone();
            Core::update_accounts(cx, move |store| store.add_offline(&name).map(|_| ()))
        };
        window
            .spawn(cx, async move |cx| {
                if let Err(error) = added.await {
                    tracing::warn!(%error, "adding an offline account failed");
                    let text = t!("accounts.offline.add_failed", name = name, error = error);
                    cx.update(|window, cx| {
                        window.push_notification(Notification::error(text.to_string()), cx)
                    })
                    .ok();
                }
            })
            .detach();
        true
    }
}

impl Render for OfflineForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let value = self.name.read(cx).value().trim().to_string();
        let error = name_error(&value).map(SharedString::from);
        v_flex()
            .id("offline-form")
            .test_support()
            .w_full()
            .gap_2()
            .text_size(TEXT_PRIMARY)
            .child(
                div()
                    .text_size(TEXT_SECONDARY)
                    .strong(FontWeight::MEDIUM, cx)
                    .text_color(theme.muted_foreground)
                    .child(t!("accounts.offline.name").to_string()),
            )
            .child(Input::new(&self.name).id("offline-name"))
            .children(error.map(|error| {
                div()
                    .id("offline-name-error")
                    .aria_label(error.clone())
                    .test_support()
                    .text_size(TEXT_SECONDARY)
                    .text_color(theme.danger)
                    .child(error)
            }))
    }
}

/// "取消" and "添加" (an entity, so "添加" follows the name).
struct OfflineFooter {
    form: Entity<OfflineForm>,
    _changed: Subscription,
}

impl OfflineFooter {
    fn new(form: Entity<OfflineForm>, cx: &mut Context<Self>) -> Self {
        let _changed = cx.subscribe(&form, |_, _, _: &NameChanged, cx| cx.notify());
        Self { form, _changed }
    }
}

impl Render for OfflineFooter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = self.form.read(cx).request(cx).is_some();
        let form = self.form.clone();
        DialogFooter::new()
            .child(
                Button::new("offline-cancel")
                    .label(t!("accounts.cancel").to_string())
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("offline-add")
                    .primary()
                    .label(t!("accounts.offline.add").to_string())
                    .disabled(!ready)
                    .on_click(move |_, window, cx| {
                        if OfflineForm::add(&form, window, cx) {
                            window.close_dialog(cx);
                        }
                    }),
            )
    }
}

/// Opens the dialog over `window`, with the keyboard in the name.
pub fn open(window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| OfflineForm::new(window, cx));
    let footer = cx.new(|cx| OfflineFooter::new(form.clone(), cx));
    // The builder runs again whenever the dialog layer is rebuilt; the name
    // takes the keyboard on the first run only.
    let first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            let name = form.read(cx).name_input().clone();
            name.update(cx, |input, cx| input.focus(window, cx));
        }
        let title = SharedString::from(t!("accounts.offline.title"));
        let form_ok = form.clone();
        dialog
            .title(
                div()
                    .id("offline-title")
                    .aria_label(title.clone())
                    .test_support()
                    .child(title),
            )
            .child(form.clone())
            .footer(footer.clone())
            // Enter adds only from the name; on a button it is that
            // button's own key.
            .on_ok(move |_, window, cx| {
                let name = form_ok.read(cx).name_input().focus_handle(cx);
                if !name.is_focused(window) {
                    cx.propagate();
                    return false;
                }
                OfflineForm::add(&form_ok, window, cx)
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_core_rule() {
        let _locale = crate::i18n::locale_lock();
        crate::i18n::set_language(crate::i18n::Language::En);
        assert_eq!(name_error(""), None, "nothing typed yet");
        for ok in ["abc", "Dev_One", &"a".repeat(16)] {
            assert_eq!(name_error(ok), None, "{ok}");
        }
        for bad in ["ab", &"a".repeat(17), "bad name", "名字名字", "a-b"] {
            let error = name_error(bad).expect(bad);
            assert_eq!(error, AccountError::InvalidName(bad.into()).to_string());
        }
    }
}
