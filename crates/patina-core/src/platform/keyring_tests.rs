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
        peer_public: Mutex<Option<Vec<u8>>>,
        public_padding: AtomicUsize,
        get_secret_calls: AtomicUsize,
        set_secret_calls: AtomicUsize,
        create_item_calls: AtomicUsize,
        saved: Mutex<Vec<u8>>,
        label: Mutex<Option<String>>,
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
            if let Some(public) = self.0.peer_public.lock().unwrap().as_ref() {
                return (Key::new(public.clone()).into(), path(SESSION));
            }
            let private = Key::generate_private_key().unwrap();
            let public = Key::generate_public_key(&private).unwrap();
            let remote = Key::try_from(input).unwrap();
            *self.0.key.lock().unwrap() = Some(Key::generate_aes_key(&private, &remote).unwrap());
            let mut encoded = vec![0; self.0.public_padding.load(Ordering::SeqCst)];
            encoded.extend_from_slice(public.as_ref());
            (Key::new(encoded).into(), path(SESSION))
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
            mut properties: HashMap<String, OwnedValue>,
            secret: DBusSecretInner,
            replace: bool,
        ) -> zbus::fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
            self.0.create_item_calls.fetch_add(1, Ordering::SeqCst);
            if self.0.peer_public.lock().unwrap().is_some() {
                return Err(zbus::fdo::Error::Failed(
                    "unexpected secret operation".into(),
                ));
            }
            assert!(replace);
            assert!(properties.contains_key("org.freedesktop.Secret.Item.Attributes"));
            *self.0.label.lock().unwrap() = Some(
                String::try_from(
                    properties
                        .remove("org.freedesktop.Secret.Item.Label")
                        .unwrap(),
                )
                .unwrap(),
            );
            save(&self.0, secret);
            let mode = *self.0.mode.lock().unwrap();
            if mode == "write-hang" {
                std::future::pending::<()>().await;
            }
            if mode == "create-prompt" {
                Ok((path("/"), path("/test/prompt")))
            } else {
                Ok((path(PATH), path("/")))
            }
        }
    }
    struct Item(Arc<State>);
    #[zbus::interface(name = "org.freedesktop.Secret.Item")]
    impl Item {
        fn get_secret(&self, session: OwnedObjectPath) -> zbus::fdo::Result<DBusSecretInner> {
            self.0.get_secret_calls.fetch_add(1, Ordering::SeqCst);
            if self.0.peer_public.lock().unwrap().is_some() {
                return Err(zbus::fdo::Error::Failed(
                    "unexpected secret operation".into(),
                ));
            }
            assert_eq!(session.as_str(), SESSION);
            let key = self.0.key.lock().unwrap();
            let iv = if *self.0.mode.lock().unwrap() == "bad-iv" {
                vec![0; 15]
            } else {
                oo7::crypto::generate_iv().unwrap()
            };
            let saved = self.0.saved.lock().unwrap();
            let ciphertext =
                oo7::crypto::encrypt(saved.as_slice(), key.as_ref().unwrap(), [0; 16]).unwrap();
            if iv.len() != 16 {
                return Ok(DBusSecretInner(
                    session,
                    iv,
                    ciphertext,
                    oo7::ContentType::Blob,
                ));
            }
            let ciphertext =
                oo7::crypto::encrypt(saved.as_slice(), key.as_ref().unwrap(), &iv).unwrap();
            Ok(DBusSecretInner(
                session,
                iv,
                ciphertext,
                oo7::ContentType::Blob,
            ))
        }
        fn set_secret(&self, secret: DBusSecretInner) -> zbus::fdo::Result<()> {
            self.0.set_secret_calls.fetch_add(1, Ordering::SeqCst);
            if self.0.peer_public.lock().unwrap().is_some() {
                return Err(zbus::fdo::Error::Failed(
                    "unexpected secret operation".into(),
                ));
            }
            save(&self.0, secret);
            Ok(())
        }
        fn delete(&self) -> OwnedObjectPath {
            if *self.0.mode.lock().unwrap() == "delete-prompt" {
                path("/test/prompt")
            } else {
                self.0.saved.lock().unwrap().clear();
                *self.0.mode.lock().unwrap() = "missing";
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
    // Independent fixture: RFC 2409 section 6.2, not the production bound.
    fn oakley_group_2_prime() -> Vec<u8> {
        hex::decode(concat!(
            "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1",
            "29024E088A67CC74020BBEA63B139B22514A08798E3404DD",
            "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
            "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
            "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE65381",
            "FFFFFFFFFFFFFFFF",
        ))
        .unwrap()
    }
    fn padded(value: &[u8], zeros: usize) -> Vec<u8> {
        let mut encoded = vec![0; zeros];
        encoded.extend_from_slice(value);
        encoded
    }

    #[tokio::test]
    async fn invalid_dh_public_values_never_reach_secrets_or_file_fallback() {
        use crate::accounts::{
            credentials::{CredentialRecord, CredentialStore},
            error::AuthError,
            testing::MemoryCredentials,
        };
        use crate::paths::DataDirs;
        let prime = oakley_group_2_prime();
        let mut minus_one = prime.clone();
        *minus_one.last_mut().unwrap() = 0xfe;
        let mut plus_one = prime.clone();
        for byte in plus_one.iter_mut().rev() {
            let (next, carry) = byte.overflowing_add(1);
            *byte = next;
            if !carry {
                break;
            }
        }
        let cases = [
            ("empty", vec![]),
            ("zero", vec![0]),
            ("padded zero", vec![0; 129]),
            ("one", vec![1]),
            ("padded one", padded(&[1], 128)),
            ("p minus one", minus_one.clone()),
            ("padded p minus one", padded(&minus_one, 1)),
            ("p", prime.clone()),
            ("padded p", padded(&prime, 1)),
            ("p plus one", plus_one),
            ("maximum 1024-bit value", vec![0xff; 128]),
            (
                "oversized significant value",
                [vec![1], vec![0; 128]].concat(),
            ),
            (
                "padded oversized significant value",
                [vec![0, 1], vec![0; 128]].concat(),
            ),
        ];
        for (case, public) in cases {
            let state = Arc::new(State::default());
            *state.peer_public.lock().unwrap() = Some(public);
            let (_server, client) = connections(state.clone()).await;
            assert_eq!(
                read_connected(&client, REF).await.err(),
                Some(CredentialError::Corrupt),
                "{case}: read classification"
            );
            assert_eq!(
                write_connected(&client, REF, PAYLOAD).await,
                Err(CredentialError::Corrupt),
                "{case}: update classification"
            );
            *state.mode.lock().unwrap() = "create";
            let error = write_connected(&client, REF, PAYLOAD)
                .await
                .expect_err("invalid peer must fail");
            assert_eq!(
                error,
                CredentialError::Corrupt,
                "{case}: create classification"
            );
            assert_eq!(
                state.encrypted_sessions.load(Ordering::SeqCst),
                3,
                "{case}: actual OpenSession calls"
            );
            assert_eq!(
                state.get_secret_calls.load(Ordering::SeqCst),
                0,
                "{case}: GetSecret reached"
            );
            assert_eq!(
                state.set_secret_calls.load(Ordering::SeqCst),
                0,
                "{case}: SetSecret reached"
            );
            assert_eq!(
                state.create_item_calls.load(Ordering::SeqCst),
                0,
                "{case}: CreateItem reached"
            );
            assert_eq!(state.prompts.load(Ordering::SeqCst), 0);
            assert_eq!(state.unlocks.load(Ordering::SeqCst), 0);

            // Pass the actual adapter failure through the unchanged store policy.
            let tmp = tempfile::tempdir().unwrap();
            let keyring = Arc::new(MemoryCredentials::default());
            keyring.set_write_error(Some(error));
            let files = Arc::new(MemoryCredentials::default());
            let store = CredentialStore::new(DataDirs::new(tmp.path()), keyring, files.clone());
            assert_eq!(
                store.write(REF, &CredentialRecord::decode(PAYLOAD).unwrap()),
                Err(AuthError::CredentialCorrupt),
                "{case}: fallback must not succeed"
            );
            assert!(
                files.read(REF).unwrap().is_none(),
                "{case}: fallback file backend was written"
            );
        }
    }

    #[tokio::test]
    async fn dh_public_range_boundaries_accept_unsigned_leading_zero_encodings() {
        let mut minus_two = oakley_group_2_prime();
        *minus_two.last_mut().unwrap() = 0xfd;
        for public in [
            vec![2],
            padded(&[2], 128),
            minus_two.clone(),
            padded(&minus_two, 1),
        ] {
            let state = Arc::new(State::default());
            *state.peer_public.lock().unwrap() = Some(public);
            let (_server, client) = connections(state.clone()).await;
            // The fixture deliberately refuses GetSecret for overridden keys.
            // Reaching it proves range acceptance, not a successful exchange.
            assert_eq!(
                read_connected(&client, REF).await.err(),
                Some(CredentialError::Corrupt)
            );
            assert_eq!(state.get_secret_calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn valid_padded_dh_public_values_complete_encrypted_exchange() {
        for zeros in [0, 1, 128] {
            let state = Arc::new(State::default());
            state.public_padding.store(zeros, Ordering::SeqCst);
            *state.mode.lock().unwrap() = "create";
            let (_server, client) = connections(state.clone()).await;
            write_connected(&client, REF, PAYLOAD).await.unwrap();
            *state.mode.lock().unwrap() = "read";
            assert!(read_connected(&client, REF).await.unwrap().is_some());
            assert_eq!(state.create_item_calls.load(Ordering::SeqCst), 1);
            assert_eq!(state.get_secret_calls.load(Ordering::SeqCst), 1);
            assert_eq!(state.prompts.load(Ordering::SeqCst), 0);
            assert_eq!(state.unlocks.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn encrypted_read_write_and_delete_never_prompt() {
        tokio::time::timeout(Duration::from_secs(5), encrypted_crud())
            .await
            .expect("fake peer CRUD deadline");
    }

    async fn encrypted_crud() {
        let state = Arc::new(State::default());
        *state.mode.lock().unwrap() = "create";
        let (_server, client) = connections(state.clone()).await;
        write_connected(&client, REF, PAYLOAD).await.unwrap();
        assert!(matches!(
            state.label.lock().unwrap().as_deref(),
            Some("Patina credentials" | "Patina 凭据")
        ));
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
        let rotated = br#"{"version":1,"refresh_token":"SYNTHETIC_ROTATED_SENTINEL","minecraft":null,"reauth_required":true}"#;
        write_connected(&client, REF, rotated).await.unwrap();
        let record = read_connected(&client, REF).await.unwrap().unwrap();
        assert_eq!(
            record.refresh_token().expose(),
            "SYNTHETIC_ROTATED_SENTINEL"
        );
        assert!(record.reauth_required());
        assert_eq!(*state.saved.lock().unwrap(), rotated);
        assert_eq!(state.create_item_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.set_secret_calls.load(Ordering::SeqCst), 1);
        delete_connected(&client, REF).await.unwrap();
        assert!(state.saved.lock().unwrap().is_empty());
        assert!(read_connected(&client, REF).await.unwrap().is_none());
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
