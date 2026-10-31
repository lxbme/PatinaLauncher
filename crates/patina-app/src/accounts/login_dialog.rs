//! The "添加微软账号" dialog: one device-code sign-in through `Core.login`
//! ([`DeviceCodeLogin`](patina_core::accounts::login::DeviceCodeLogin)).
//!
//! It walks 准备中 → the code (large, monospaced, with "复制") and
//! "打开 microsoft.com/link" → 等待中 → done or failed. "取消", Escape and
//! closing the dialog cancel the attempt: "取消" sets the cancel token,
//! closing drops it (the dialog's [`LoginDialog`] goes, and with it the
//! token's sender), which the interface also counts as cancelling.
//!
//! Nothing is stored yet: a finished sign-in only says so (development
//! builds walk a fake flow); plan 3 adds the account here. Release builds
//! have no flow at all and say "微软登录暂未开放".

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit::{
    AppContext, ClipboardItem, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task,
    TestSupportExt, Window, div, rems,
};
use patina_core::accounts::login::{DeviceCode, LoginError, LoginEvent};
use rust_i18n::t;
use tokio::sync::{mpsc, watch};

use crate::core_bridge::Core;
use crate::layout::TEXT_PRIMARY;

/// How long "复制" says "已复制".
const COPIED_FOR: std::time::Duration = std::time::Duration::from_secs(2);

/// Where a sign-in attempt is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    /// Waiting for the first code.
    Preparing,
    /// A code to enter; `waiting` once the flow polls for the approval.
    Code {
        code: DeviceCode,
        waiting: bool,
    },
    /// Signed in as `username`. Nothing was added (see the module docs).
    Done {
        username: String,
    },
    Failed(LoginError),
}

impl LoginState {
    /// Whether the attempt is over.
    pub fn is_finished(&self) -> bool {
        matches!(self, LoginState::Done { .. } | LoginState::Failed(_))
    }
}

/// One sign-in attempt and what the dialog shows of it (an entity). The
/// attempt runs on the app's runtime and ends when this is dropped.
pub struct LoginDialog {
    state: LoginState,
    /// Cancels the attempt in flight: `true`, or dropping it.
    cancel: Option<watch::Sender<bool>>,
    /// Whether "复制" just copied the code.
    copied: bool,
    copied_reset: Task<()>,
    events: Task<()>,
}

impl LoginDialog {
    /// Starts an attempt at once.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            state: LoginState::Preparing,
            cancel: None,
            copied: false,
            copied_reset: Task::ready(()),
            events: Task::ready(()),
        };
        this.start(cx);
        this
    }

    pub fn state(&self) -> &LoginState {
        &self.state
    }

    /// Starts a new attempt, cancelling the one in flight.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.cancel();
        let (events, mut rx) = mpsc::unbounded_channel();
        let (cancel, token) = watch::channel(false);
        let core = Core::global(cx);
        // Not tied to this view's tasks: it ends through the token, as the
        // interface asks, and then reports `Failed(Cancelled)` to nobody.
        core.rt.spawn(core.login.start(events, token));
        self.cancel = Some(cancel);
        self.state = LoginState::Preparing;
        self.copied = false;
        self.events = cx.spawn(async move |this, cx| {
            while let Some(event) = rx.recv().await {
                if this.update(cx, |this, cx| this.apply(event, cx)).is_err() {
                    return;
                }
            }
        });
        cx.notify();
    }

    /// Cancels the attempt in flight (if any).
    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.send(true).ok();
        }
    }

    fn apply(&mut self, event: LoginEvent, cx: &mut Context<Self>) {
        let next = match (event, &self.state) {
            // A new code replaces the one on show.
            (LoginEvent::Code(code), _) => LoginState::Code {
                code,
                waiting: false,
            },
            (LoginEvent::Waiting, LoginState::Code { code, .. }) => LoginState::Code {
                code: code.clone(),
                waiting: true,
            },
            (LoginEvent::Waiting, _) => return,
            // Plan 3 stores the account here.
            (LoginEvent::Done { username, .. }, _) => LoginState::Done { username },
            (LoginEvent::Failed(error), _) => LoginState::Failed(error),
            (event, _) => {
                tracing::debug!(?event, "ignoring a sign-in event this dialog does not know");
                return;
            }
        };
        if next.is_finished() {
            self.cancel = None;
        }
        if self.state != next {
            self.state = next;
            cx.notify();
        }
    }

    /// Copies the code on show; "复制" says "已复制" for a moment.
    fn copy_code(&mut self, cx: &mut Context<Self>) {
        let LoginState::Code { code, .. } = &self.state else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(code.user_code.clone()));
        self.copied = true;
        self.copied_reset = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn open_link(&self, cx: &mut Context<Self>) {
        if let LoginState::Code { code, .. } = &self.state {
            cx.open_url(&code.verification_uri);
        }
    }

    fn render_code(
        &self,
        code: &DeviceCode,
        waiting: bool,
        cx: &mut Context<Self>,
    ) -> Vec<gpui_kit::AnyElement> {
        let theme = cx.theme();
        let site = site_of(&code.verification_uri);
        let copy_label = if self.copied {
            t!("accounts.login.copied")
        } else {
            t!("accounts.login.copy")
        }
        .to_string();
        let user_code = SharedString::from(code.user_code.clone());
        let mut parts = vec![
            note(t!("accounts.login.enter_code", site = site).to_string(), cx).into_any_element(),
            h_flex()
                .gap_3()
                .child(
                    div()
                        .id("login-code")
                        .aria_label(user_code.clone())
                        .test_support()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(rems(1.75))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(user_code),
                )
                .child(
                    Button::new("login-copy")
                        .outline()
                        .small()
                        .icon(Icon::new(if self.copied {
                            IconName::Check
                        } else {
                            IconName::Copy
                        }))
                        .label(copy_label)
                        .on_click(cx.listener(|this, _, _, cx| this.copy_code(cx))),
                )
                .into_any_element(),
            div()
                .child(
                    Button::new("login-open")
                        .primary()
                        .icon(Icon::new(IconName::ExternalLink))
                        .label(t!("accounts.login.open", site = site).to_string())
                        .on_click(cx.listener(|this, _, _, cx| this.open_link(cx))),
                )
                .into_any_element(),
        ];
        if waiting {
            parts.push(
                h_flex()
                    .id("login-waiting")
                    .test_support()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child(note(t!("accounts.login.waiting").to_string(), cx))
                    .into_any_element(),
            );
        }
        parts
    }
}

/// `https://microsoft.com/link` as `microsoft.com/link`.
fn site_of(uri: &str) -> &str {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    rest.trim_end_matches('/')
}

/// A line of muted body text.
fn note(text: String, cx: &gpui_kit::App) -> gpui_kit::Div {
    div()
        .text_size(TEXT_PRIMARY)
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// A status line with `id` (its text also as its label, for tests).
fn status(id: &'static str, text: String, color: gpui_kit::Hsla) -> impl IntoElement {
    let text = SharedString::from(text);
    div()
        .id(id)
        .aria_label(text.clone())
        .test_support()
        .text_size(TEXT_PRIMARY)
        .text_color(color)
        .child(text)
}

impl Render for LoginDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, danger) = (theme.foreground, theme.danger);
        let body: Vec<gpui_kit::AnyElement> = match &self.state.clone() {
            LoginState::Preparing => vec![
                h_flex()
                    .id("login-preparing")
                    .test_support()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child(note(t!("accounts.login.preparing").to_string(), cx))
                    .into_any_element(),
            ],
            LoginState::Code { code, waiting } => self.render_code(code, *waiting, cx),
            LoginState::Done { .. } => vec![
                status(
                    "login-done",
                    t!("accounts.login.done_not_added").to_string(),
                    fg,
                )
                .into_any_element(),
            ],
            LoginState::Failed(LoginError::Unavailable) => vec![
                status(
                    "login-failed",
                    t!("accounts.login.unavailable").to_string(),
                    fg,
                )
                .into_any_element(),
            ],
            LoginState::Failed(error) => {
                vec![status("login-failed", error.to_string(), danger).into_any_element()]
            }
        };
        v_flex()
            .id("login-body")
            .test_support()
            .w_full()
            .gap_3()
            .text_size(TEXT_PRIMARY)
            .children(body)
    }
}

/// The footer: "取消" while the attempt runs; "关闭" (and "重试" after a
/// failure that a new attempt may fix) once it is over.
struct LoginFooter {
    dialog: Entity<LoginDialog>,
    _changed: Subscription,
}

impl LoginFooter {
    fn new(dialog: Entity<LoginDialog>, cx: &mut Context<Self>) -> Self {
        let _changed = cx.observe(&dialog, |_, _, cx| cx.notify());
        Self { dialog, _changed }
    }
}

impl Render for LoginFooter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.dialog.read(cx).state().clone();
        let dialog = self.dialog.clone();
        let footer = DialogFooter::new();
        if !state.is_finished() {
            return footer.child(
                Button::new("login-cancel")
                    .label(t!("accounts.login.cancel").to_string())
                    .on_click(move |_, window, cx| {
                        dialog.update(cx, |d, _| d.cancel());
                        window.close_dialog(cx);
                    }),
            );
        }
        let retry = matches!(
            state,
            LoginState::Failed(LoginError::Expired | LoginError::Other(_))
        );
        let footer = footer.child(
            Button::new("login-close")
                .label(t!("accounts.login.close").to_string())
                .on_click(|_, window, cx| window.close_dialog(cx)),
        );
        if !retry {
            return footer;
        }
        footer.child(
            Button::new("login-retry")
                .primary()
                .label(t!("accounts.login.retry").to_string())
                .on_click(move |_, _, cx| dialog.update(cx, |d, cx| d.start(cx))),
        )
    }
}

/// Opens the dialog over `window` and starts signing in.
pub fn open(window: &mut Window, cx: &mut gpui_kit::App) -> Entity<LoginDialog> {
    let dialog = cx.new(LoginDialog::new);
    let footer = cx.new(|cx| LoginFooter::new(dialog.clone(), cx));
    let shown = dialog.clone();
    window.open_dialog(cx, move |d, _, _| {
        let title = SharedString::from(t!("accounts.login.title"));
        d.title(
            div()
                .id("login-title")
                .aria_label(title.clone())
                .test_support()
                .child(title),
        )
        .child(shown.clone())
        .footer(footer.clone())
        // Enter has nothing to commit: on a button it presses that button.
        .on_ok(|_, _, cx| {
            cx.propagate();
            false
        })
    });
    dialog
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_drops_the_scheme() {
        assert_eq!(site_of("https://microsoft.com/link"), "microsoft.com/link");
        assert_eq!(
            site_of("https://www.microsoft.com/link/"),
            "www.microsoft.com/link"
        );
        assert_eq!(site_of("microsoft.com/link"), "microsoft.com/link");
    }
}
