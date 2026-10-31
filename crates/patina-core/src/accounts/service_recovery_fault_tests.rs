//! Inverse commit windows and composition with unrelated/overriding mutations.
use super::super::super::credentials::{CredentialRecord, CredentialStore};
use super::*;
use crate::platform::credentials::{FileCredentials, PrivateCredentialDir};
use std::sync::{Mutex, atomic::AtomicUsize};

struct CacheWriteFault {
    data: DataDirs,
    files: FileCredentials,
    writes: AtomicUsize,
    stage: WriteStage,
}
impl CredentialBackend for CacheWriteFault {
    fn read(&self, reference: &str) -> Result<Option<CredentialRecord>, CredentialError> {
        self.files.read(reference)
    }
    fn delete(&self, reference: &str) -> Result<(), CredentialError> {
        self.files.delete(reference)
    }
    fn write(&self, reference: &str, record: &CredentialRecord) -> Result<(), CredentialError> {
        if self.writes.fetch_add(1, Ordering::SeqCst) == 2 {
            PrivateCredentialDir::open(&self.data)?.write_failing(
                &format!("{reference}.json"),
                &record.encode()?,
                self.stage,
            )
        } else {
            self.files.write(reference, record)
        }
    }
}
fn client(f: &Fixture) -> MsaClient {
    let stages = [
        AuthStage::DeviceCode,
        AuthStage::MicrosoftToken,
        AuthStage::XboxLive,
        AuthStage::Xsts,
        AuthStage::MinecraftToken,
        AuthStage::Entitlement,
        AuthStage::Profile,
    ];
    MsaClient::for_test(
        AuthHttp::for_test(std::array::from_fn(|i| {
            (stages[i], format!("{}/{i}", f.server.uri()))
        }))
        .unwrap(),
        f.clock.clone(),
    )
}

#[tokio::test]
async fn cache_write_failure_after_metadata_commit_keeps_identity_skin_and_proof_paired() {
    for stage in [WriteStage::FileSync, WriteStage::DirectorySync] {
        let mut f = Fixture::new().await;
        f.keyring.set_error(Some(CredentialError::Unavailable));
        let files = Arc::new(CacheWriteFault {
            data: f.data.clone(),
            files: FileCredentials::new(f.data.clone()),
            writes: AtomicUsize::new(0),
            stage,
        });
        f.service = AccountService::for_test(
            f.data.clone(),
            client(&f),
            CredentialStore::new(f.data.clone(), f.keyring.clone(), files.clone()),
            f.clock.clone(),
        );
        let old = f.seed(NOW + 60).await;
        f.clock.0.store(NOW + 1, Ordering::SeqCst);
        f.chain("Renamed").await;
        assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
        assert_eq!(files.writes.load(Ordering::SeqCst), 3);
        let store = AccountStore::load(&f.data).unwrap();
        let account = store.current().unwrap();
        assert_eq!(
            account.username, "Renamed",
            "metadata must precede the fresh cache write"
        );
        assert_eq!(account.skin_hash(), Some("abcdef"));
        assert_eq!(account.verification().unwrap().verified_at(), NOW + 1);
        assert_eq!(account.token_ref, old.token_ref);
        let snapshot = f.service.snapshot().await.unwrap();
        assert_eq!(
            snapshot.current().unwrap(),
            account,
            "publish committed metadata even when cache persistence fails"
        );
        f.server.reset().await;
        f.response(1, 503, json!({})).await;
        drop(f.service);
        let reopened = Fixture::service(&f.data, &f.server, f.keyring.clone(), f.clock.clone());
        let (_tx, cancel) = watch::channel(false);
        let result = reopened.resolve_launch(ID, cancel).await;
        if stage == WriteStage::DirectorySync {
            assert_eq!(result.unwrap().account().name(), "Renamed");
            assert!(f.server.received_requests().await.unwrap().is_empty());
        } else {
            assert_eq!(
                result.unwrap_err(),
                AuthError::Http {
                    stage: AuthStage::MicrosoftToken,
                    status: 503
                }
            );
        }
        let record = files
            .read(old.token_ref.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(record.refresh_token().expose(), "ROTATED_SENTINEL");
        assert_eq!(
            record.minecraft().is_some(),
            stage == WriteStage::DirectorySync
        );
        assert!(
            !std::fs::read_to_string(f.data.root().join("credentials/cleanup.json"))
                .unwrap()
                .contains("SENTINEL")
        );
    }
}

#[tokio::test]
async fn failed_mutation_does_not_retire_pending_revocation_before_durable_retry() {
    for mutation in ["remove", "login"] {
        let f = Fixture::new().await;
        f.seed(NOW + 60).await;
        let other = super::super::super::testing::seed_verified(
            &f.service,
            "Other",
            Uuid::from_u128(2),
            "OTHER_SECRET_SENTINEL",
            NOW,
            NOW + 3600,
        )
        .await
        .unwrap();
        f.chain("Original").await;
        Mock::given(path("/5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[]})))
            .with_priority(1)
            .mount(&f.server)
            .await;
        f.service.set_metadata_fault(WriteStage::Rename);
        assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
        f.service.set_metadata_fault(WriteStage::Rename);
        if mutation == "remove" {
            assert_eq!(
                f.service.remove(&other.id).await.unwrap_err(),
                AuthError::CredentialWrite
            );
        } else {
            assert_eq!(
                super::super::super::testing::seed_verified(
                    &f.service,
                    "Third",
                    Uuid::from_u128(3),
                    "THIRD_SECRET_SENTINEL",
                    NOW,
                    NOW + 3600
                )
                .await
                .unwrap_err(),
                AuthError::CredentialWrite
            );
        }
        assert!(
            AccountStore::load(&f.data)
                .unwrap()
                .accounts()
                .iter()
                .find(|a| a.id == ID)
                .unwrap()
                .verification()
                .is_some()
        );
        assert!(
            f.service
                .snapshot()
                .await
                .unwrap()
                .accounts()
                .iter()
                .find(|a| a.id == ID)
                .unwrap()
                .verification()
                .is_none()
        );
        f.service.remove(&other.id).await.unwrap();
        assert!(!f.service.snapshot().await.unwrap().is_offline_allowed());
        drop(f.service);
        let reopened = Fixture::service(&f.data, &f.server, f.keyring.clone(), f.clock.clone());
        assert!(!reopened.snapshot().await.unwrap().is_offline_allowed());
    }
}

struct ReferenceWriteFailure {
    records: Arc<MemoryCredentials>,
    blocked: Mutex<Option<String>>,
}
impl CredentialBackend for ReferenceWriteFailure {
    fn read(&self, reference: &str) -> Result<Option<CredentialRecord>, CredentialError> {
        self.records.read(reference)
    }
    fn delete(&self, reference: &str) -> Result<(), CredentialError> {
        self.records.delete(reference)
    }
    fn write(&self, reference: &str, record: &CredentialRecord) -> Result<(), CredentialError> {
        if self.blocked.lock().unwrap().as_deref() == Some(reference) {
            Err(CredentialError::Io)
        } else {
            self.records.write(reference, record)
        }
    }
}

#[tokio::test]
async fn remove_and_relogin_supersede_broken_secret_retry_without_saving_old_reference() {
    for mutation in ["remove", "relogin"] {
        let mut f = Fixture::new().await;
        let backend = Arc::new(ReferenceWriteFailure {
            records: f.keyring.clone(),
            blocked: Mutex::new(None),
        });
        f.service = AccountService::for_test(
            f.data.clone(),
            client(&f),
            CredentialStore::new(
                f.data.clone(),
                backend.clone(),
                Arc::new(MemoryCredentials::default()),
            ),
            f.clock.clone(),
        );
        let old = f.seed(NOW + 60).await;
        f.chain("Renamed").await;
        *backend.blocked.lock().unwrap() = old.token_ref.clone();
        assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
        if mutation == "remove" {
            assert_eq!(f.service.remove(ID).await.unwrap(), RemovalOutcome::Removed);
        } else {
            let new = super::super::super::testing::seed_verified(
                &f.service,
                "Replacement",
                old.uuid,
                "NEW_SECRET_SENTINEL",
                NOW,
                NOW + 3600,
            )
            .await
            .unwrap();
            assert_ne!(new.token_ref, old.token_ref);
            assert_eq!(f.resolve().await.unwrap().account().name(), "Replacement");
        }
        assert!(
            f.keyring
                .read(old.token_ref.as_ref().unwrap())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            f.refresh_count().await,
            1,
            "overriding mutation must not recover/exchange the broken old secret"
        );
        drop(f.service);
        let reopened = Fixture::service(&f.data, &f.server, f.keyring.clone(), f.clock.clone());
        let snapshot = reopened.snapshot().await.unwrap();
        assert_eq!(snapshot.accounts().is_empty(), mutation == "remove");
    }
}

#[tokio::test]
async fn own_reference_removal_and_verified_relogin_retire_pending_revocation() {
    for mutation in ["remove", "relogin"] {
        let f = Fixture::new().await;
        let old = f.seed(NOW + 60).await;
        f.chain("Original").await;
        Mock::given(path("/5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[]})))
            .with_priority(1)
            .mount(&f.server)
            .await;
        f.service.set_metadata_fault(WriteStage::Rename);
        assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
        if mutation == "remove" {
            f.service.remove(ID).await.unwrap();
        } else {
            let new = super::super::super::testing::seed_verified(
                &f.service,
                "Reverified",
                old.uuid,
                "NEW_SECRET_SENTINEL",
                NOW,
                NOW + 3600,
            )
            .await
            .unwrap();
            assert_ne!(new.token_ref, old.token_ref);
            assert!(new.verification().is_some());
        }
        assert_eq!(
            f.service.snapshot().await.unwrap().is_offline_allowed(),
            mutation == "relogin"
        );
        // A second owner must acquire the lock: the retired marker cannot keep
        // exclusion indefinitely after a durable removal/replacement.
        let snapshot =
            tokio::time::timeout(std::time::Duration::from_secs(2), f.restarted().snapshot())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(snapshot.is_offline_allowed(), mutation == "relogin");
        assert!(
            f.keyring
                .read(old.token_ref.as_ref().unwrap())
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn unrelated_mutation_during_metadata_retry_cannot_enable_stale_cache() {
    for mutation in ["remove", "login"] {
        let f = Fixture::new().await;
        f.seed(NOW + 60).await;
        let other = super::super::super::testing::seed_verified(
            &f.service,
            "Other",
            Uuid::from_u128(2),
            "OTHER_SECRET_SENTINEL",
            NOW,
            NOW + 3600,
        )
        .await
        .unwrap();
        f.chain("Renamed").await;
        f.service.set_metadata_fault(WriteStage::Rename);
        assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
        if mutation == "remove" {
            f.service.remove(&other.id).await.unwrap();
        } else {
            super::super::super::testing::seed_verified(
                &f.service,
                "Third",
                Uuid::from_u128(3),
                "THIRD_SECRET_SENTINEL",
                NOW,
                NOW + 3600,
            )
            .await
            .unwrap();
        }
        f.server.reset().await;
        f.response(1, 503, json!({})).await;
        drop(f.service);
        let reopened = Fixture::service(&f.data, &f.server, f.keyring.clone(), f.clock.clone());
        let (_tx, cancel) = watch::channel(false);
        assert_eq!(
            reopened.resolve_launch(ID, cancel).await.unwrap_err(),
            AuthError::Http {
                stage: AuthStage::MicrosoftToken,
                status: 503
            }
        );
    }
}

#[tokio::test]
async fn storage_only_retry_keeps_cache_disabled_until_metadata_commit() {
    for retry_stage in [WriteStage::Rename, WriteStage::DirectorySync] {
        let f = Fixture::new().await;
        let account = f.seed(NOW + 60).await;
        f.chain("Renamed").await;
        f.service.set_metadata_fault(WriteStage::Rename);
        assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
        f.service.set_metadata_fault(retry_stage);
        assert_eq!(
            f.service.reload().await.unwrap_err(),
            AuthError::CredentialWrite
        );
        let record = f
            .keyring
            .read(account.token_ref.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            record.minecraft().is_none(),
            "failed retry enabled the cache before durable metadata"
        );
        assert_eq!(record.refresh_token().expose(), "ROTATED_SENTINEL");
        assert_eq!(
            f.refresh_count().await,
            1,
            "metadata retry must be storage-only"
        );
        f.server.reset().await;
        f.response(1, 503, json!({})).await;
        drop(f.service);
        let reopened = Fixture::service(&f.data, &f.server, f.keyring.clone(), f.clock.clone());
        let (_tx, cancel) = watch::channel(false);
        assert_eq!(
            reopened.resolve_launch(ID, cancel).await.unwrap_err(),
            AuthError::Http {
                stage: AuthStage::MicrosoftToken,
                status: 503
            }
        );
    }
}
