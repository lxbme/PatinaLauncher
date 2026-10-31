use super::keyring::*;
use crate::accounts::credentials::{CredentialBackend, CredentialError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[test]
fn keyring_constructor_is_lazy_and_invalid_reference_never_connects() {
    let backend = Keyring::new();
    assert!(matches!(
        backend.read("not-a-reference"),
        Err(CredentialError::Corrupt)
    ));
}

// If the timeout merely abandons blocking work, this drop witness stays false.
#[test]
fn deadline_drops_underlying_future_before_returning() {
    struct Witness(Arc<AtomicBool>);
    impl Drop for Witness {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let witness = Witness(dropped.clone());
    let result: Result<(), CredentialError> = run_bounded(Duration::from_millis(10), async move {
        let _witness = witness;
        std::future::pending().await
    });
    assert_eq!(result, Err(CredentialError::Unavailable));
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn error_mapping_does_not_hide_corrupt_or_ambiguous_results() {
    use oo7::zbus;
    assert_eq!(
        map_zbus(zbus::Error::InputOutput(Arc::new(std::io::Error::from(
            std::io::ErrorKind::TimedOut
        )))),
        CredentialError::Unavailable
    );
    assert_eq!(
        map_zbus(zbus::Error::Failure("SECRET_SENTINEL".into())),
        CredentialError::Corrupt
    );
    assert_eq!(
        require_no_prompt("/prompt/1"),
        Err(CredentialError::Unavailable)
    );
    assert_eq!(require_no_prompt("/"), Ok(()));
    assert_eq!(
        select_item(vec!["/item/1".into(), "/item/2".into()], vec![]),
        Err(CredentialError::Ambiguous)
    );
    assert_eq!(
        select_item(vec![], vec!["/item/1".into()]),
        Err(CredentialError::Unavailable)
    );
    assert_eq!(select_item(vec![], vec![]), Ok(None));
}

// An isolated peer socket exercises real D-Bus encoding and maintained crypto;
// no session address, real keyring, daemon process or network is consulted.
mod peer {
    use super::*;
    use oo7::{
        Key,
        dbus::api::DBusSecretInner,
        zbus::{
            self,
            zvariant::{OwnedObjectPath, OwnedValue},
        },
    };
    use std::{
        collections::HashMap,
        sync::{Mutex, atomic::AtomicUsize},
    };
    const REF: &str = "c7137070-3696-4aa9-b592-7f21e0c10823";
    const PATH: &str = "/test/item";
    const SESSION: &str = "/test/session";
    const PAYLOAD: &[u8] = br#"{"version":1,"refresh_token":"SYNTHETIC_PEER_SENTINEL","minecraft":null,"reauth_required":false}"#;
    #[derive(Default)]
    struct State {
        mode: Mutex<&'static str>,
        key: Mutex<Option<Key>>,
        saved: Mutex<Vec<u8>>,
        prompts: AtomicUsize,
        unlocks: AtomicUsize,
        encrypted_sessions: AtomicUsize,
    }
    fn path(s: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(s.to_owned()).unwrap()
    }
    struct Service(Arc<State>);
    #[zbus::interface(name = "org.freedesktop.Secret.Service")]
    impl Service {
        fn search_items(
            &self,
            attributes: HashMap<String, String>,
        ) -> (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) {
            assert_eq!(attributes.get("service").unwrap(), "patina-launcher");
            assert_eq!(attributes.get("username").unwrap(), REF);
            match *self.0.mode.lock().unwrap() {
                "missing" | "create-prompt" | "create" | "write-hang" => (vec![], vec![]),
                "locked" => (vec![], vec![path(PATH)]),
                "ambiguous" => (vec![path(PATH), path("/test/second")], vec![]),
                _ => (vec![path(PATH)], vec![]),
            }
        }
        fn open_session(
            &self,
            algorithm: &str,
            input: OwnedValue,
        ) -> (OwnedValue, OwnedObjectPath) {
            assert_eq!(algorithm, "dh-ietf1024-sha256-aes128-cbc-pkcs7");
            self.0.encrypted_sessions.fetch_add(1, Ordering::SeqCst);
            let private = Key::generate_private_key().unwrap();
            let public = Key::generate_public_key(&private).unwrap();
            let remote = Key::try_from(input).unwrap();
            *self.0.key.lock().unwrap() = Some(Key::generate_aes_key(&private, &remote).unwrap());
            (public.into(), path(SESSION))
        }
        fn read_alias(&self, alias: &str) -> OwnedObjectPath {
            assert_eq!(alias, "default");
            path("/test/collection")
        }
        fn unlock(
            &self,
            _objects: Vec<OwnedObjectPath>,
        ) -> (Vec<OwnedObjectPath>, OwnedObjectPath) {
            self.0.unlocks.fetch_add(1, Ordering::SeqCst);
            (vec![], path("/test/prompt"))
        }
    }
    struct Collection(Arc<State>);
    fn save(state: &State, secret: DBusSecretInner) {
        assert_eq!(secret.0.as_str(), SESSION);
        assert_eq!(secret.1.len(), 16);
        assert!(
            !secret
                .2
                .windows(b"SYNTHETIC_PEER_SENTINEL".len())
                .any(|w| w == b"SYNTHETIC_PEER_SENTINEL")
        );
        let key = state.key.lock().unwrap();
        let decoded = oo7::crypto::decrypt(&secret.2, key.as_ref().unwrap(), &secret.1).unwrap();
        assert_eq!(decoded.as_slice(), PAYLOAD);
        *state.saved.lock().unwrap() = decoded.to_vec();
    }
    #[zbus::interface(name = "org.freedesktop.Secret.Collection")]
    impl Collection {
        #[zbus(property)]
        fn locked(&self) -> bool {
            false
        }
        async fn create_item(
            &self,
            properties: HashMap<String, OwnedValue>,
            secret: DBusSecretInner,
            replace: bool,
        ) -> (OwnedObjectPath, OwnedObjectPath) {
            assert!(replace);
            assert!(properties.contains_key("org.freedesktop.Secret.Item.Attributes"));
            save(&self.0, secret);
            let mode = *self.0.mode.lock().unwrap();
            if mode == "write-hang" {
                std::future::pending::<()>().await;
            }
            if mode == "create-prompt" {
                (path("/"), path("/test/prompt"))
            } else {
                (path(PATH), path("/"))
            }
        }
    }
    struct Item(Arc<State>);
    #[zbus::interface(name = "org.freedesktop.Secret.Item")]
    impl Item {
        fn get_secret(&self, session: OwnedObjectPath) -> DBusSecretInner {
            assert_eq!(session.as_str(), SESSION);
            let key = self.0.key.lock().unwrap();
            let iv = if *self.0.mode.lock().unwrap() == "bad-iv" {
                vec![0; 15]
            } else {
                oo7::crypto::generate_iv().unwrap()
            };
            let ciphertext = oo7::crypto::encrypt(PAYLOAD, key.as_ref().unwrap(), [0; 16]).unwrap();
            if iv.len() != 16 {
                return DBusSecretInner(session, iv, ciphertext, oo7::ContentType::Blob);
            }
            let ciphertext = oo7::crypto::encrypt(PAYLOAD, key.as_ref().unwrap(), &iv).unwrap();
            DBusSecretInner(session, iv, ciphertext, oo7::ContentType::Blob)
        }
        fn set_secret(&self, secret: DBusSecretInner) {
            save(&self.0, secret);
        }
        fn delete(&self) -> OwnedObjectPath {
            if *self.0.mode.lock().unwrap() == "delete-prompt" {
                path("/test/prompt")
            } else {
                self.0.saved.lock().unwrap().clear();
                path("/")
            }
        }
    }
    struct Prompt(Arc<State>);
    #[zbus::interface(name = "org.freedesktop.Secret.Prompt")]
    impl Prompt {
        fn prompt(&self, _window: &str) {
            self.0.prompts.fetch_add(1, Ordering::SeqCst);
        }
    }
    async fn connections(state: Arc<State>) -> (zbus::Connection, zbus::Connection) {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let server = zbus::connection::Builder::unix_stream(a)
            .p2p()
            .server(zbus::Guid::generate())
            .unwrap()
            .serve_at("/org/freedesktop/secrets", Service(state.clone()))
            .unwrap()
            .serve_at("/test/collection", Collection(state.clone()))
            .unwrap()
            .serve_at(PATH, Item(state.clone()))
            .unwrap()
            .serve_at("/test/prompt", Prompt(state))
            .unwrap()
            .build();
        let client = zbus::connection::Builder::unix_stream(b).p2p().build();
        let (a, b) = tokio::join!(server, client);
        (a.unwrap(), b.unwrap())
    }
    #[tokio::test]
    async fn encrypted_read_write_and_delete_never_prompt() {
        let state = Arc::new(State::default());
        *state.mode.lock().unwrap() = "create";
        let (_server, client) = connections(state.clone()).await;
        write_connected(&client, REF, PAYLOAD).await.unwrap();
        assert_eq!(*state.saved.lock().unwrap(), PAYLOAD);
        *state.mode.lock().unwrap() = "read";
        assert_eq!(
            read_connected(&client, REF)
                .await
                .unwrap()
                .unwrap()
                .refresh_token()
                .expose(),
            "SYNTHETIC_PEER_SENTINEL"
        );
        write_connected(&client, REF, PAYLOAD).await.unwrap();
        delete_connected(&client, REF).await.unwrap();
        assert!(state.saved.lock().unwrap().is_empty());
        assert!(state.encrypted_sessions.load(Ordering::SeqCst) >= 3);
        assert_eq!(state.prompts.load(Ordering::SeqCst), 0);
        assert_eq!(state.unlocks.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn prompts_locked_ambiguous_and_corrupt_are_not_absence() {
        let state = Arc::new(State::default());
        let (_server, client) = connections(state.clone()).await;
        for (mode, expected) in [
            ("locked", CredentialError::Unavailable),
            ("ambiguous", CredentialError::Ambiguous),
            ("bad-iv", CredentialError::Corrupt),
        ] {
            *state.mode.lock().unwrap() = mode;
            assert_eq!(read_connected(&client, REF).await.err().unwrap(), expected);
        }
        *state.mode.lock().unwrap() = "create-prompt";
        assert_eq!(
            write_connected(&client, REF, PAYLOAD).await,
            Err(CredentialError::Unavailable)
        );
        *state.mode.lock().unwrap() = "delete-prompt";
        assert_eq!(
            delete_connected(&client, REF).await,
            Err(CredentialError::Unavailable)
        );
        *state.mode.lock().unwrap() = "missing";
        assert!(read_connected(&client, REF).await.unwrap().is_none());
        delete_connected(&client, REF).await.unwrap();
        assert_eq!(state.prompts.load(Ordering::SeqCst), 0);
        assert_eq!(state.unlocks.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn lost_write_acknowledgement_is_not_rollback() {
        let state = Arc::new(State::default());
        *state.mode.lock().unwrap() = "write-hang";
        let (_server, client) = connections(state.clone()).await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(200),
                write_connected(&client, REF, PAYLOAD)
            )
            .await
            .is_err()
        );
        assert_eq!(*state.saved.lock().unwrap(), PAYLOAD);
        assert_eq!(state.prompts.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn wrapped_access_denied_and_interactive_auth_are_unavailable() {
    use oo7::zbus::{Error, fdo};
    for error in [
        fdo::Error::AccessDenied("PRIVATE_REMOTE_TEXT".into()),
        fdo::Error::InteractiveAuthorizationRequired("PRIVATE_REMOTE_TEXT".into()),
        fdo::Error::NoServer("PRIVATE_REMOTE_TEXT".into()),
        fdo::Error::SpawnExecFailed("PRIVATE_REMOTE_TEXT".into()),
    ] {
        assert_eq!(
            map_zbus(Error::FDO(Box::new(error))),
            CredentialError::Unavailable
        );
    }
    assert_eq!(
        map_zbus(Error::FDO(Box::new(fdo::Error::InvalidArgs(
            "PRIVATE_REMOTE_TEXT".into()
        )))),
        CredentialError::Corrupt
    );
}

#[test]
fn stalled_fake_bus_connections_are_closed_on_every_deadline() {
    use std::io::Read;
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("fake-bus");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = oo7::zbus::Address::new(oo7::zbus::address::Transport::Unix(
        oo7::zbus::address::transport::Unix::new(oo7::zbus::address::transport::UnixSocket::File(
            socket,
        )),
    ));
    for _ in 0..4 {
        assert!(matches!(
            run_bounded(Duration::from_millis(30), connect_address(&address)),
            Err(CredentialError::Unavailable)
        ));
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut auth_bytes = Vec::new();
        // EOF, not a timeout: neither runtime nor connection reader retains the socket.
        peer.read_to_end(&mut auth_bytes).unwrap();
    }
}

#[test]
fn malformed_transport_data_is_not_write_unavailability() {
    for kind in [
        std::io::ErrorKind::InvalidData,
        std::io::ErrorKind::InvalidInput,
    ] {
        let error =
            oo7::zbus::Error::InputOutput(Arc::new(std::io::Error::new(kind, "REMOTE_SENTINEL")));
        assert_eq!(map_zbus(error), CredentialError::Corrupt);
    }
}
