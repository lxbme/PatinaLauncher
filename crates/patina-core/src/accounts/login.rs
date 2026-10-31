//! Microsoft device-code login interface. The real flow arrives with plan 3;
//! until then [`UnavailableLogin`] reports that sign-in is not available.
//! Persisting a finished login into the `AccountStore` is plan 3's job.

use futures::future::BoxFuture;
use std::time::Duration;
use tokio::sync::{mpsc::UnboundedSender, watch};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    #[error("{}", rust_i18n::t!("errors.login.unavailable"))]
    Unavailable,
    #[error("{}", rust_i18n::t!("errors.login.expired"))]
    Expired,
    #[error("{}", rust_i18n::t!("errors.login.cancelled"))]
    Cancelled,
    #[error("{}", rust_i18n::t!("errors.login.other", error = .0))]
    Other(String),
}

/// Progress of one login attempt. Non-exhaustive: later versions may carry
/// more data (for example a token handle in `Done`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoginEvent {
    /// A code to show the user. May be sent again (refresh after expiry);
    /// the UI replaces the code it shows.
    Code(DeviceCode),
    /// Polling for the user's approval is in progress.
    Waiting,
    Done {
        username: String,
        uuid: Uuid,
    },
    Failed(LoginError),
}

/// Signals cancellation: the login flow stops when the value becomes `true`
/// or the sender is dropped.
pub type CancelToken = watch::Receiver<bool>;

pub trait DeviceCodeLogin: Send + Sync {
    /// Runs one login attempt, reporting progress on `events`. The future ends
    /// after a terminal event (`Done` or `Failed`).
    ///
    /// Cancellation: the attempt is cancelled when the watch value becomes
    /// `true` or the sender is dropped (`changed().await` returns `Err`). On
    /// cancel the implementation sends `Failed(Cancelled)` (ignoring a closed
    /// event receiver) and returns.
    fn start(
        &self,
        events: UnboundedSender<LoginEvent>,
        cancel: CancelToken,
    ) -> BoxFuture<'static, ()>;
}

/// Stand-in used until the real flow exists: fails immediately.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableLogin;

impl DeviceCodeLogin for UnavailableLogin {
    fn start(
        &self,
        events: UnboundedSender<LoginEvent>,
        _cancel: CancelToken,
    ) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            // A closed receiver just means nobody is listening any more.
            let _ = events.send(LoginEvent::Failed(LoginError::Unavailable));
        })
    }
}

const _: fn() = || {
    fn assert_obj(_: std::sync::Arc<dyn DeviceCodeLogin>) {}
    assert_obj(std::sync::Arc::new(UnavailableLogin));
};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unavailable_login_fails_immediately() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel) = watch::channel(false);
        let login: std::sync::Arc<dyn DeviceCodeLogin> = std::sync::Arc::new(UnavailableLogin);
        login.start(tx, cancel).await;
        assert_eq!(
            rx.recv().await,
            Some(LoginEvent::Failed(LoginError::Unavailable))
        );
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn unavailable_login_ends_with_cancelled_token() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_tx, cancel) = watch::channel(false);
        cancel_tx.send(true).unwrap();
        UnavailableLogin.start(tx, cancel).await;
        assert_eq!(
            rx.recv().await,
            Some(LoginEvent::Failed(LoginError::Unavailable))
        );
        // Dropped sender counts as cancelled too.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_tx, cancel) = watch::channel(false);
        drop(cancel_tx);
        UnavailableLogin.start(tx, cancel).await;
    }

    #[test]
    fn login_errors_localized() {
        use crate::i18n::{Language, set_language};
        let _locale = crate::i18n::locale_lock();
        let msgs = |lang| {
            set_language(lang);
            [
                LoginError::Unavailable.to_string(),
                LoginError::Expired.to_string(),
                LoginError::Cancelled.to_string(),
                LoginError::Other("boom".into()).to_string(),
            ]
        };
        let zh = msgs(Language::ZhCn);
        let en = msgs(Language::En);
        set_language(Language::ZhCn);
        for (z, e) in zh.iter().zip(&en) {
            assert!(
                z.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{z}"
            );
            assert!(e.is_ascii() && !e.starts_with("errors."), "{e}");
            assert!(!z.starts_with("errors."), "{z}");
        }
        assert!(zh[3].contains("boom") && en[3].contains("boom"));
    }
}
