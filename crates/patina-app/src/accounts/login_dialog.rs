//! One cancellable device-code attempt; the service commits before emitting Done.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit::{
    AppContext, ClipboardItem, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task,
    TestSupportExt, Window, div, rems,
};
use patina_core::accounts::login::{DeviceCode, LoginError, LoginEvent, StorageKind};
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
    /// The service has durably committed this identity.
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
    attempt: u64,
    local_storage: bool,
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
            attempt: 0,
            local_storage: false,
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
        let attempt = self.attempt;
        self.local_storage = false;
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
                if this
                    .update(cx, |this, cx| this.apply(attempt, event, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        cx.notify();
    }

    /// Cancels the attempt in flight (if any).
    pub fn cancel(&mut self) {
        self.attempt += 1;
        if let Some(cancel) = self.cancel.take() {
            cancel.send(true).ok();
        }
    }

    fn apply(&mut self, attempt: u64, event: LoginEvent, cx: &mut Context<Self>) {
        if attempt != self.attempt || self.state.is_finished() {
            return;
        }
        if let LoginEvent::Storage(kind) = event {
            self.local_storage = kind == StorageKind::LocalFile;
            cx.notify();
            return;
        }
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
            // Never persist event fields: the service already committed.
            (LoginEvent::Done { username, .. }, _) => LoginState::Done { username },
            (LoginEvent::Failed(error), _) => LoginState::Failed(error),
            // Future events may carry secrets; never log unknown payloads.
            _ => return,
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
            LoginState::Done { username } => vec![
                status(
                    "login-done",
                    t!("accounts.login.done", name = username).to_string(),
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
            .children(self.local_storage.then(|| {
                status(
                    "login-storage",
                    t!("accounts.login.local_storage").to_string(),
                    cx.theme().muted_foreground,
                )
            }))
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
        let retry = match &state {
            LoginState::Failed(LoginError::Expired | LoginError::Other(_)) => true,
            LoginState::Failed(LoginError::Auth(error)) => {
                use patina_core::accounts::error::AuthError;
                !matches!(
                    error,
                    AuthError::NoEntitlement
                        | AuthError::ProfileMissing
                        | AuthError::XboxRestriction(_)
                        | AuthError::IdentityMismatch
                        | AuthError::Cancelled
                )
            }
            _ => false,
        };
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
    let closing = dialog.downgrade();
    window.open_dialog(cx, move |d, window, _| {
        let title = SharedString::from(t!("accounts.login.title"));
        // Preserve the default width at 100%, scaling with code/actions at zoom.
        // Dialog still owns viewport clamping and overflow scrolling.
        d.width(rems(28.).to_pixels(window.rem_size()))
            .overlay_closable(false)
            .title(
                div()
                    .id("login-title")
                    .aria_label(title.clone())
                    .test_support()
                    .child(title),
            )
            .child(shown.clone())
            .footer(footer.clone())
            .on_close({
                let closing = closing.clone();
                move |_, _, cx| {
                    closing.update(cx, |d, _| d.cancel()).ok();
                }
            })
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

    #[gpui_kit::test]
    fn stale_and_post_terminal_events_cannot_replace_attempt(cx: &mut gpui_kit::TestAppContext) {
        let dialog = cx.new(|_| LoginDialog {
            state: LoginState::Preparing,
            attempt: 2,
            local_storage: false,
            cancel: None,
            copied: false,
            copied_reset: Task::ready(()),
            events: Task::ready(()),
        });
        dialog.update(cx, |dialog, cx| {
            dialog.apply(1, LoginEvent::Failed(LoginError::Expired), cx);
            assert_eq!(dialog.state(), &LoginState::Preparing);
            dialog.apply(2, LoginEvent::Failed(LoginError::Expired), cx);
            dialog.apply(
                2,
                LoginEvent::Done {
                    username: "Stale".into(),
                    uuid: patina_core::launch::offline_uuid("Stale"),
                },
                cx,
            );
            assert_eq!(dialog.state(), &LoginState::Failed(LoginError::Expired));
        });
    }

    #[gpui_kit::test]
    fn unknown_event_does_not_log_payload(cx: &mut gpui_kit::TestAppContext) {
        use patina_core::accounts::login::StorageKind;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
            type Writer = Self;
            fn make_writer(&'a self) -> Self {
                self.clone()
            }
        }

        // No login service or window: only exercise the real event handler.
        let dialog = cx.new(|_| LoginDialog {
            state: LoginState::Preparing,
            attempt: 0,
            local_storage: false,
            cancel: None,
            copied: false,
            copied_reset: Task::ready(()),
            events: Task::ready(()),
        });
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(capture.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("capture active");
            dialog.update(cx, |dialog, cx| {
                dialog.apply(0, LoginEvent::Storage(StorageKind::LocalFile), cx);
                assert_eq!(dialog.state(), &LoginState::Preparing);
            });
        });
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("capture active"));
        assert!(!output.contains("Storage") && !output.contains("LocalFile"));
    }

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
