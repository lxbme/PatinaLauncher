use super::credentials::{CredentialBackend, CredentialError, CredentialRecord, CredentialStore};
use super::{error::AuthError, login::StorageKind};
use crate::paths::DataDirs;
use crate::platform::credentials::{FileCredentials, PrivateCredentialDir};
use std::sync::{Arc, Mutex};

const REF: &str = "c7137070-3696-4aa9-b592-7f21e0c10823";

#[derive(Default)]
struct Backend {
    record: Mutex<Option<CredentialRecord>>,
    error: Mutex<Option<CredentialError>>,
    write_error: Mutex<Option<CredentialError>>,
    deletes: Mutex<usize>,
}
impl CredentialBackend for Backend {
    fn read(&self, _: &str) -> Result<Option<CredentialRecord>, CredentialError> {
        if let Some(e) = *self.error.lock().unwrap() {
            return Err(e);
        }
        Ok(self.record.lock().unwrap().clone())
    }
    fn write(&self, _: &str, record: &CredentialRecord) -> Result<(), CredentialError> {
        if let Some(e) = *self.write_error.lock().unwrap() {
            return Err(e);
        }
        if let Some(e) = *self.error.lock().unwrap() {
            return Err(e);
        }
        *self.record.lock().unwrap() = Some(record.clone());
        Ok(())
    }
    fn delete(&self, _: &str) -> Result<(), CredentialError> {
        *self.deletes.lock().unwrap() += 1;
        if let Some(e) = *self.error.lock().unwrap() {
            return Err(e);
        }
        *self.record.lock().unwrap() = None;
        Ok(())
    }
}
fn record(secret: &str) -> CredentialRecord {
    CredentialRecord::decode(
        format!(
            r#"{{"version":1,"refresh_token":"{secret}","minecraft":null,"reauth_required":false}}"#
        )
        .as_bytes(),
    )
    .unwrap()
}
fn store(data: &DataDirs, keyring: Arc<Backend>) -> CredentialStore {
    CredentialStore::new(
        data.clone(),
        keyring,
        Arc::new(FileCredentials::new(data.clone())),
    )
}

// Collapsing NoEntry and Unavailable would erase actionable read failures.
#[test]
fn missing_is_not_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let backend = Arc::new(Backend::default());
    let store = store(&data, backend.clone());
    assert!(matches!(store.read(REF), Err(AuthError::CredentialMissing)));
    *backend.error.lock().unwrap() = Some(CredentialError::Unavailable);
    assert!(matches!(
        store.read(REF),
        Err(AuthError::StorageUnavailable)
    ));
    assert!(!tmp.path().join(format!("credentials/{REF}.json")).exists());
}

// Selecting today's available backend instead of the existing file revives stale rotations.
#[test]
fn fallback_remains_authoritative_when_keyring_returns() {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let backend = Arc::new(Backend::default());
    *backend.record.lock().unwrap() = Some(record("STALE_KEYRING_SENTINEL"));
    *backend.error.lock().unwrap() = Some(CredentialError::Unavailable);
    assert_eq!(
        store(&data, backend.clone())
            .write(REF, &record("NEW_FILE_SENTINEL"))
            .unwrap(),
        StorageKind::LocalFile
    );
    *backend.error.lock().unwrap() = None;
    let restarted = store(&data, backend.clone());
    assert_eq!(
        restarted.read(REF).unwrap().refresh_token().expose(),
        "NEW_FILE_SENTINEL"
    );
    assert_eq!(
        restarted.write(REF, &record("NEXT_FILE_SENTINEL")).unwrap(),
        StorageKind::LocalFile
    );
    assert_eq!(
        backend
            .record
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .refresh_token()
            .expose(),
        "STALE_KEYRING_SENTINEL"
    );
    PrivateCredentialDir::open(&data)
        .unwrap()
        .write(&format!("{REF}.json"), b"broken")
        .unwrap();
    assert!(matches!(
        restarted.read(REF),
        Err(AuthError::CredentialCorrupt)
    ));
    assert!(matches!(
        restarted.write(REF, &record("REPLACEMENT")),
        Err(AuthError::CredentialCorrupt)
    ));
}

#[test]
fn only_unavailable_write_falls_back() {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let backend = Arc::new(Backend::default());
    let store = store(&data, backend.clone());
    for error in [
        CredentialError::Corrupt,
        CredentialError::Ambiguous,
        CredentialError::Io,
        CredentialError::UnsafePath,
    ] {
        *backend.error.lock().unwrap() = Some(error);
        assert!(store.write(REF, &record("SYNTHETIC")).is_err());
        assert!(!tmp.path().join(format!("credentials/{REF}.json")).exists());
    }
    *backend.error.lock().unwrap() = None;
    assert_eq!(
        store.write(REF, &record("SYNTHETIC")).unwrap(),
        StorageKind::Keyring
    );
    assert!(store.write("../invalid", &record("SYNTHETIC")).is_err());
}

#[test]
fn cleanup_retries_both_backends() {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let key = Arc::new(Backend::default());
    let files = Arc::new(Backend::default());
    let store = CredentialStore::new(data, key.clone(), files.clone());
    *key.error.lock().unwrap() = Some(CredentialError::Unavailable);
    assert!(store.delete(REF).is_err());
    assert_eq!(*files.deletes.lock().unwrap(), 1);
    *key.error.lock().unwrap() = None;
    *files.error.lock().unwrap() = Some(CredentialError::Io);
    assert!(store.delete(REF).is_err());
    assert_eq!(*key.deletes.lock().unwrap(), 2);
    *files.error.lock().unwrap() = None;
    store.delete(REF).unwrap();
    store.delete(REF).unwrap();
}

#[test]
fn credential_schema_is_validated_and_errors_redacted() {
    for bytes in [b"{}".as_slice(), br#"{"version":2,"refresh_token":"SENTINEL","minecraft":null,"reauth_required":false}"#, br#"{"version":1,"refresh_token":"","minecraft":null,"reauth_required":false}"#, br#"{"version":1,"refresh_token":"SENTINEL","minecraft":{"access_token":"TOKEN","issued_at":20,"expires_at":10,"profile_uuid":"c7137070-3696-4aa9-b592-7f21e0c10823","xuid":null},"reauth_required":false}"#] {
        let error = CredentialRecord::decode(bytes).err().unwrap();
        assert_eq!(error, CredentialError::Corrupt);
        assert!(!format!("{error:?}").contains("SENTINEL"));
    }
    let value = record("SYNTHETIC");
    assert_eq!(
        CredentialRecord::decode(&value.encode().unwrap())
            .unwrap()
            .refresh_token()
            .expose(),
        "SYNTHETIC"
    );
}

#[test]
fn cleanup_journal_validates_nonsecret_references_and_is_durable() {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let store = store(&data, Arc::new(Backend::default()));
    assert!(store.journal().unwrap().is_empty());
    store.write_journal(&[REF.to_owned()]).unwrap();
    assert_eq!(store.journal().unwrap(), vec![REF]);
    assert!(store.write_journal(&["TOKEN_SENTINEL".into()]).is_err());
    assert_eq!(store.journal().unwrap(), vec![REF]);
    store.write_journal(&[]).unwrap();
    assert!(store.journal().unwrap().is_empty());
}

#[test]
fn persisted_session_round_trip_preserves_identity_and_validity() {
    let bytes = br#"{"version":1,"refresh_token":"REFRESH_SENTINEL","minecraft":{"access_token":"ACCESS_SENTINEL","issued_at":100,"expires_at":200,"profile_uuid":"c7137070-3696-4aa9-b592-7f21e0c10823","xuid":"123456"},"reauth_required":false}"#;
    let record = CredentialRecord::decode(bytes).unwrap();
    let encoded = record.encode().unwrap();
    let decoded = CredentialRecord::decode(&encoded).unwrap();
    let session = decoded.minecraft().unwrap();
    assert_eq!(session.issued_at(), 100);
    assert_eq!(session.expires_at(), 200);
    assert_eq!(session.profile_uuid().to_string(), REF);
    assert_eq!(session.xuid(), Some("123456"));
    assert_eq!(session.access_token().expose(), "ACCESS_SENTINEL");
    assert!(!decoded.reauth_required());
    assert!(!format!("{session:?}").contains("ACCESS_SENTINEL"));
    for (field, value) in [
        ("issued_at", serde_json::json!(201)),
        ("expires_at", serde_json::json!(100)),
        (
            "profile_uuid",
            serde_json::json!("00000000-0000-0000-0000-000000000000"),
        ),
        ("xuid", serde_json::json!("not-an-id")),
        ("access_token", serde_json::json!("")),
    ] {
        let mut value_json: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        value_json["minecraft"][field] = value;
        assert!(matches!(
            CredentialRecord::decode(&serde_json::to_vec(&value_json).unwrap()),
            Err(CredentialError::Corrupt)
        ));
    }
    let mut invalid: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    invalid["reauth_required"] = serde_json::json!(true);
    assert!(CredentialRecord::decode(&serde_json::to_vec(&invalid).unwrap()).is_err());
}

#[test]
fn file_read_error_never_uses_stale_keyring_or_creates_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let key = Arc::new(Backend::default());
    *key.record.lock().unwrap() = Some(record("STALE_SENTINEL"));
    let files = Arc::new(Backend::default());
    let store = CredentialStore::new(data, key, files.clone());
    for error in [
        CredentialError::Io,
        CredentialError::Unavailable,
        CredentialError::UnsafePath,
        CredentialError::Corrupt,
    ] {
        *files.error.lock().unwrap() = Some(error);
        assert!(store.read(REF).is_err());
        assert!(store.write(REF, &record("ROTATED_SENTINEL")).is_err());
        assert!(files.record.lock().unwrap().is_none());
    }
}

#[test]
fn both_backend_write_failures_are_not_success() {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let key = Arc::new(Backend::default());
    *key.error.lock().unwrap() = Some(CredentialError::Unavailable);
    let files = Arc::new(Backend::default());
    *files.write_error.lock().unwrap() = Some(CredentialError::Io);
    let store = CredentialStore::new(data, key, files.clone());
    assert!(matches!(
        store.write(REF, &record("NEW_SENTINEL")),
        Err(AuthError::CredentialWrite)
    ));
    assert!(files.record.lock().unwrap().is_none());
}

#[test]
fn production_construction_is_lazy_without_disk_or_keyring_probe() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("not-created");
    let _store = CredentialStore::production(DataDirs::new(&path));
    assert!(!path.exists());
}

#[test]
fn test_support_can_fail_writes_without_masking_existing_reads() {
    use super::testing::MemoryCredentials;
    let backend = MemoryCredentials::default();
    backend
        .seed(REF, &record("OLD_SENTINEL").encode().unwrap())
        .unwrap();
    backend.set_write_error(Some(CredentialError::Io));
    assert_eq!(
        backend.read(REF).unwrap().unwrap().refresh_token().expose(),
        "OLD_SENTINEL"
    );
    assert_eq!(
        backend.write(REF, &record("NEW_SENTINEL")),
        Err(CredentialError::Io)
    );
    backend.set_write_error(None);
    backend.write(REF, &record("NEW_SENTINEL")).unwrap();
    assert_eq!(
        backend.read(REF).unwrap().unwrap().refresh_token().expose(),
        "NEW_SENTINEL"
    );
}
