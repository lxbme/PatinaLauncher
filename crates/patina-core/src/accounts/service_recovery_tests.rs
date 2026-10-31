//! Review regressions: compose mutations with fail-closed state and discard all
//! owners before retrying a partially completed refresh.
use super::super::credentials::CredentialRecord;
use super::*;
use crate::platform::credentials::WriteStage;

#[tokio::test]
async fn pending_revocation_survives_unrelated_remove_and_login() {
    for mutation in ["remove", "login"] {
        let f = Fixture::new().await;
        f.seed(NOW + 60).await;
        let other_id = Uuid::from_u128(2);
        if mutation == "remove" {
            super::super::testing::seed_verified(
                &f.service,
                "Other",
                other_id,
                "OTHER_SECRET_SENTINEL",
                NOW,
                NOW + 3600,
            )
            .await
            .unwrap();
        }
        f.chain("Original").await;
        Mock::given(path("/5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[]})))
            .with_priority(1)
            .mount(&f.server)
            .await;
        f.service.set_metadata_fault(WriteStage::Rename);
        assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
        let snapshot = f.service.snapshot().await.unwrap();
        assert!(
            snapshot
                .accounts()
                .iter()
                .find(|a| a.id == ID)
                .unwrap()
                .verification()
                .is_none()
        );
        if mutation == "remove" {
            f.service
                .remove(&other_id.simple().to_string())
                .await
                .unwrap();
        } else {
            f.server.reset().await;
            f.chain("NewOwner").await;
            Mock::given(path("/6"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(
                        json!({"id":other_id.simple().to_string(),"name":"NewOwner"}),
                    ),
                )
                .with_priority(1)
                .mount(&f.server)
                .await;
            assert!(matches!(
                f.login().await.last(),
                Some(LoginEvent::Done { .. })
            ));
        }
        let snapshot = f.service.snapshot().await.unwrap();
        assert!(
            snapshot
                .accounts()
                .iter()
                .find(|a| a.id == ID)
                .unwrap()
                .verification()
                .is_none(),
            "{mutation} republished revoked proof"
        );
        assert_eq!(snapshot.is_offline_allowed(), mutation == "login");
        drop(f.service);
        let reopened = Fixture::service(&f.data, &f.server, f.keyring.clone(), f.clock.clone());
        let snapshot = reopened.snapshot().await.unwrap();
        assert!(
            snapshot
                .accounts()
                .iter()
                .find(|a| a.id == ID)
                .unwrap()
                .verification()
                .is_none()
        );
        assert_eq!(snapshot.is_offline_allowed(), mutation == "login");
    }
}

#[tokio::test]
async fn refresh_metadata_failure_after_all_owners_drop_never_uses_stale_cached_identity() {
    for stage in [WriteStage::Rename, WriteStage::DirectorySync] {
        let f = Fixture::new().await;
        let account = f.seed(NOW + 60).await;
        f.clock.0.store(NOW + 1, Ordering::SeqCst);
        f.chain("Renamed").await;
        f.service.set_metadata_fault(stage);
        assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
        f.server.reset().await;
        f.response(1, 503, json!({})).await;
        // No original-owner resolve/reload retry: release every service clone.
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
        let record = f
            .keyring
            .read(account.token_ref.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(record.refresh_token().expose(), "ROTATED_SENTINEL");
        assert!(
            record.minecraft().is_none(),
            "uncommitted refresh must remain non-cache-eligible"
        );
        let snapshot = reopened.snapshot().await.unwrap();
        let account = snapshot.accounts().iter().find(|a| a.id == ID).unwrap();
        if stage == WriteStage::DirectorySync {
            assert_eq!(account.username, "Renamed");
            assert_eq!(account.skin_hash(), Some("abcdef"));
            assert_eq!(account.verification().unwrap().verified_at(), NOW + 1);
        } else {
            assert_eq!(account.username, "Original");
            assert_eq!(account.skin_hash(), None);
            assert_eq!(account.verification().unwrap().verified_at(), NOW);
        }
        let journal =
            std::fs::read_to_string(f.data.root().join("credentials/cleanup.json")).unwrap();
        assert!(!journal.contains("SENTINEL"));
        assert!(
            !std::fs::read_to_string(f.data.root().join("accounts.toml"))
                .unwrap()
                .contains("SENTINEL")
        );
    }
}

#[tokio::test]
async fn refresh_metadata_failure_across_process_restart_never_uses_stale_cached_identity() {
    for stage in ["rename", "directory-sync"] {
        let f = Fixture::new().await;
        f.chain("Renamed").await;
        // The writer process, not this otherwise unused service, owns the failing operation.
        drop(f.service);
        run_recovery_child(&f.data, &f.server, "write", stage).await;
        f.server.reset().await;
        Mock::given(path("/1"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&f.server)
            .await;
        run_recovery_child(&f.data, &f.server, "read", stage).await;
        let requests = f.server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(String::from_utf8_lossy(&requests[0].body).contains("ROTATED_SENTINEL"));
        let metadata = AccountStore::load(&f.data).unwrap();
        let account = metadata.current().unwrap();
        let bytes = std::fs::read(
            f.data
                .root()
                .join("credentials")
                .join(format!("{}.json", account.token_ref.as_ref().unwrap())),
        )
        .unwrap();
        let record = CredentialRecord::decode(&bytes).unwrap();
        assert_eq!(record.refresh_token().expose(), "ROTATED_SENTINEL");
        assert!(record.minecraft().is_none());
        assert!(
            !std::fs::read_to_string(f.data.root().join("credentials/cleanup.json"))
                .unwrap()
                .contains("SENTINEL")
        );
    }
}

async fn run_recovery_child(data: &DataDirs, server: &MockServer, mode: &str, stage: &str) {
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "accounts::service_tests::recovery_tests::refresh_recovery_process_helper",
            "--nocapture",
        ])
        .env("PATINA_TASK4_TEST_ROOT", data.root())
        .env("PATINA_TASK4_TEST_HTTP", server.uri())
        .env("PATINA_TASK4_RECOVERY_MODE", mode)
        .env("PATINA_TASK4_RECOVERY_STAGE", stage)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success(),
        "{mode}/{stage}"
    );
}

#[tokio::test]
#[ignore = "isolated subprocess used only by refresh_metadata_failure_across_process_restart_never_uses_stale_cached_identity"]
async fn refresh_recovery_process_helper() {
    let data = DataDirs::new(std::env::var("PATINA_TASK4_TEST_ROOT").unwrap());
    let base = std::env::var("PATINA_TASK4_TEST_HTTP").unwrap();
    let mode = std::env::var("PATINA_TASK4_RECOVERY_MODE").unwrap();
    let stage = std::env::var("PATINA_TASK4_RECOVERY_STAGE").unwrap();
    let stages = [
        AuthStage::DeviceCode,
        AuthStage::MicrosoftToken,
        AuthStage::XboxLive,
        AuthStage::Xsts,
        AuthStage::MinecraftToken,
        AuthStage::Entitlement,
        AuthStage::Profile,
    ];
    let http =
        AuthHttp::for_test(std::array::from_fn(|i| (stages[i], format!("{base}/{i}")))).unwrap();
    let clock = Arc::new(Clock(AtomicU64::new(NOW + 1)));
    let keyring = Arc::new(MemoryCredentials::default());
    keyring.set_error(Some(CredentialError::Unavailable));
    let service = AccountService::for_test(
        data.clone(),
        MsaClient::for_test(http, clock.clone()),
        credential_store(data.clone(), keyring),
        clock,
    );
    if mode == "write" {
        super::super::testing::seed_verified(
            &service,
            "Original",
            Uuid::parse_str(ID).unwrap(),
            "OLD_SECRET_SENTINEL",
            NOW,
            NOW + 60,
        )
        .await
        .unwrap();
        service.set_metadata_fault(if stage == "rename" {
            WriteStage::Rename
        } else {
            WriteStage::DirectorySync
        });
        let (_tx, cancel) = watch::channel(false);
        assert_eq!(
            service.resolve_launch(ID, cancel).await.unwrap_err(),
            AuthError::CredentialWrite
        );
    } else {
        assert_eq!(mode, "read");
        let (_tx, cancel) = watch::channel(false);
        assert_eq!(
            service.resolve_launch(ID, cancel).await.unwrap_err(),
            AuthError::Http {
                stage: AuthStage::MicrosoftToken,
                status: 503
            }
        );
        let snapshot = service.snapshot().await.unwrap();
        let account = snapshot.current().unwrap();
        if stage == "rename" {
            assert_eq!(account.username, "Original");
            assert_eq!(account.skin_hash(), None);
            assert_eq!(account.verification().unwrap().verified_at(), NOW);
        } else {
            assert_eq!(account.username, "Renamed");
            assert_eq!(account.skin_hash(), Some("abcdef"));
            assert_eq!(account.verification().unwrap().verified_at(), NOW + 1);
        }
    }
}

#[path = "service_recovery_fault_tests.rs"]
mod fault_tests;
