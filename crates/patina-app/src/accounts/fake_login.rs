//! A stand-in Microsoft sign-in for development builds only (`dev-offline`),
//! so the device-code dialog can be walked through before plan 3 brings the
//! real flow. It never reaches the network and the dialog adds nothing when
//! it finishes. Release builds do not contain this file.

#![cfg(feature = "dev-offline")]

use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Either};
use patina_core::accounts::login::{
    CancelToken, DeviceCode, DeviceCodeLogin, LoginError, LoginEvent,
};
use tokio::sync::mpsc::UnboundedSender;

/// The code it shows.
pub const FAKE_CODE: &str = "DEV-0000";
/// Where it says to enter the code.
pub const FAKE_URI: &str = "https://microsoft.com/link";
/// The name its `Done` reports.
pub const FAKE_USERNAME: &str = "DevPlayer";

/// Sends `Code` ([`FAKE_CODE`] at [`FAKE_URI`]) and `Waiting`, then `Done`
/// after `wait` (2 s by default). Cancelling (the token's value or its
/// sender dropped) ends it with `Failed(Cancelled)` at once.
#[derive(Debug, Clone, Copy)]
pub struct FakeLogin {
    wait: Duration,
}

impl FakeLogin {
    /// A fake whose approval takes `wait`.
    pub fn new(wait: Duration) -> Self {
        Self { wait }
    }
}

impl Default for FakeLogin {
    fn default() -> Self {
        Self::new(Duration::from_secs(2))
    }
}

impl DeviceCodeLogin for FakeLogin {
    fn start(
        &self,
        events: UnboundedSender<LoginEvent>,
        mut cancel: CancelToken,
    ) -> BoxFuture<'static, ()> {
        let wait = self.wait;
        async move {
            // A closed receiver only means nobody is listening any more.
            events
                .send(LoginEvent::Code(DeviceCode {
                    user_code: FAKE_CODE.into(),
                    verification_uri: FAKE_URI.into(),
                    expires_in: Duration::from_secs(900),
                }))
                .ok();
            events.send(LoginEvent::Waiting).ok();
            let approved = Box::pin(tokio::time::sleep(wait));
            let event =
                match futures::future::select(approved, Box::pin(cancelled(&mut cancel))).await {
                    Either::Left(_) => LoginEvent::Done {
                        username: FAKE_USERNAME.into(),
                        // Any stable id: nothing stores it.
                        uuid: patina_core::launch::offline_uuid(FAKE_USERNAME),
                    },
                    Either::Right(_) => LoginEvent::Failed(LoginError::Cancelled),
                };
            events.send(event).ok();
        }
        .boxed()
    }
}

/// Resolves once `cancel` says so or its sender is gone.
async fn cancelled(cancel: &mut CancelToken) {
    while !*cancel.borrow_and_update() {
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::{mpsc, watch};

    use super::*;

    fn code() -> LoginEvent {
        LoginEvent::Code(DeviceCode {
            user_code: FAKE_CODE.into(),
            verification_uri: FAKE_URI.into(),
            expires_in: Duration::from_secs(900),
        })
    }

    #[test]
    fn walks_code_waiting_done() {
        let rt = crate::runtime::start();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_cancel, token) = watch::channel(false);
        rt.block_on(FakeLogin::new(Duration::from_millis(10)).start(tx, token));
        assert_eq!(rx.try_recv().ok(), Some(code()));
        assert_eq!(rx.try_recv().ok(), Some(LoginEvent::Waiting));
        assert!(matches!(
            rx.try_recv(),
            Ok(LoginEvent::Done { username, .. }) if username == FAKE_USERNAME
        ));
        assert!(rx.try_recv().is_err(), "nothing after the end");
    }

    #[test]
    fn cancelling_ends_it_at_once() {
        let rt = crate::runtime::start();
        // By the token's value…
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (cancel, token) = watch::channel(false);
        cancel.send(true).unwrap();
        let started = std::time::Instant::now();
        rt.block_on(FakeLogin::new(Duration::from_secs(60)).start(tx, token));
        assert!(started.elapsed() < Duration::from_secs(5));
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            events.last(),
            Some(&LoginEvent::Failed(LoginError::Cancelled))
        );
        // …and by its sender going away.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (cancel, token) = watch::channel(false);
        drop(cancel);
        rt.block_on(FakeLogin::new(Duration::from_secs(60)).start(tx, token));
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            events.last(),
            Some(&LoginEvent::Failed(LoginError::Cancelled))
        );
    }
}
