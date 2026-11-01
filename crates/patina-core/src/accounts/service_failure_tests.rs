//! Faults at actual transaction boundaries, not synthetic terminal events.
use super::super::{credentials::CredentialStore, service::RemovalOutcome};
use super::*;
use crate::platform::credentials::{PrivateCredentialDir, WriteStage};

#[tokio::test]
async fn metadata_failure_before_and_after_rename_preserves_referenced_credentials() {
    for stage in [
        WriteStage::Create,
        WriteStage::Write,
        WriteStage::FileSync,
        WriteStage::Rename,
        WriteStage::DirectorySync,
    ] {
        let f = Fixture::new().await;
        let old = f.seed(NOW + 3600).await;
        let before = *f.service.changes().borrow();
        f.service.set_metadata_fault(stage);
        let result = super::super::testing::seed_verified(
            &f.service,
            "Replacement",
            Uuid::parse_str(ID).unwrap(),
            "NEW_SECRET_SENTINEL",
            NOW,
            NOW + 3600,
        )
        .await;
        assert!(matches!(result, Err(AuthError::CredentialWrite)));
        assert_eq!(*f.service.changes().borrow(), before);
        let snapshot = f.restarted().snapshot().await.unwrap();
        let active = snapshot.current().unwrap();
        let record = f
            .keyring
            .read(active.token_ref.as_ref().unwrap())
            .unwrap()
            .unwrap();
        if stage == WriteStage::DirectorySync {
            assert_eq!(active.username, "Replacement");
            assert_eq!(record.refresh_token().expose(), "NEW_SECRET_SENTINEL");
            assert!(
                f.keyring
                    .read(old.token_ref.as_ref().unwrap())
                    .unwrap()
                    .is_none()
            );
        } else {
            assert_eq!(active.username, "Original");
            assert_eq!(record.refresh_token().expose(), "OLD_SECRET_SENTINEL");
        }
        assert!(
            PrivateCredentialDir::open(&f.data)
                .unwrap()
                .read("cleanup.json")
                .unwrap()
                .unwrap()
                == b"[]"
        );
    }
}

#[tokio::test]
async fn logout_journal_failure_and_precommit_metadata_failure_leave_account_usable() {
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
    let journal = f.data.root().join("credentials/cleanup.json");
    std::fs::remove_file(&journal).unwrap();
    std::fs::create_dir(&journal).unwrap();
    assert!(f.service.remove(ID).await.is_err());
    assert!(f.service.snapshot().await.unwrap().current().is_some());
    std::fs::remove_dir(&journal).unwrap();
    f.service.set_metadata_fault(WriteStage::Rename);
    assert!(f.service.remove(ID).await.is_err());
    assert!(f.resolve().await.is_ok());
    assert_eq!(f.refresh_count().await, 0);
}

#[tokio::test]
async fn committed_removal_reports_cleanup_pending_and_restart_retries_both_backends() {
    let f = Fixture::new().await;
    let account = f.seed(NOW + 3600).await;
    f.keyring.set_error(Some(CredentialError::Unavailable));
    assert_eq!(
        f.service.remove(ID).await.unwrap(),
        RemovalOutcome::CleanupPending
    );
    assert!(f.service.snapshot().await.unwrap().accounts().is_empty());
    assert!(matches!(
        f.resolve().await,
        Err(AuthError::Account(AccountError::NotFound(_)))
    ));
    f.keyring.set_error(None);
    assert!(
        f.restarted()
            .snapshot()
            .await
            .unwrap()
            .accounts()
            .is_empty()
    );
    assert!(
        f.keyring
            .read(account.token_ref.as_ref().unwrap())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn rollback_delete_failure_leaves_journal_for_restart_without_deleting_active_record() {
    let f = Fixture::new().await;
    let old = f.seed(NOW + 3600).await;
    f.service.set_metadata_fault(WriteStage::Rename);
    assert!(
        super::super::testing::seed_verified(
            &f.service,
            "Replacement",
            Uuid::parse_str(ID).unwrap(),
            "NEW_SECRET_SENTINEL",
            NOW,
            NOW + 3600
        )
        .await
        .is_err()
    );
    f.keyring.set_error(Some(CredentialError::Unavailable));
    let snapshot = f.restarted().snapshot().await.unwrap();
    assert_eq!(snapshot.current().unwrap().token_ref, old.token_ref);
    let journal = PrivateCredentialDir::open(&f.data)
        .unwrap()
        .read("cleanup.json")
        .unwrap()
        .unwrap();
    assert_ne!(journal, b"[]");
    f.keyring.set_error(None);
    f.service.reload().await.unwrap();
    assert!(
        f.keyring
            .read(old.token_ref.as_ref().unwrap())
            .unwrap()
            .is_some()
    );
    assert_eq!(
        PrivateCredentialDir::open(&f.data)
            .unwrap()
            .read("cleanup.json")
            .unwrap()
            .unwrap(),
        b"[]"
    );
}

#[tokio::test]
async fn corrupt_reload_is_not_reset_and_diagnostics_never_include_legacy_payload() {
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
    std::fs::write(
        f.data.root().join("accounts.toml"),
        "SECRET_SENTINEL not [valid",
    )
    .unwrap();
    let error = f.service.reload().await.unwrap_err();
    assert_eq!(error, AuthError::CredentialCorrupt);
    assert!(f.service.snapshot().await.is_err());
    assert!(f.service.remove(ID).await.is_err());
    assert_eq!(
        std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap(),
        "SECRET_SENTINEL not [valid"
    );
    let wrapped = AuthError::Account(AccountError::Parse("SECRET_SENTINEL".into()));
    assert!(!format!("{wrapped:?} {wrapped}").contains("SECRET_SENTINEL"));
}

#[tokio::test]
async fn rotation_write_failure_retries_storage_before_exchange_and_blocks_other_owner() {
    let mut f = Fixture::new().await;
    let files = Arc::new(MemoryCredentials::default());
    let stages = [
        AuthStage::DeviceCode,
        AuthStage::MicrosoftToken,
        AuthStage::XboxLive,
        AuthStage::Xsts,
        AuthStage::MinecraftToken,
        AuthStage::Entitlement,
        AuthStage::Profile,
    ];
    let http = AuthHttp::for_test(std::array::from_fn(|i| {
        (stages[i], format!("{}/{i}", f.server.uri()))
    }))
    .unwrap();
    f.service = AccountService::for_test(
        f.data.clone(),
        MsaClient::for_test(http, f.clock.clone()),
        CredentialStore::new(f.data.clone(), f.keyring.clone(), files.clone()),
        f.clock.clone(),
    );
    let account = f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    f.keyring
        .set_write_error(Some(CredentialError::Unavailable));
    files.set_write_error(Some(CredentialError::Io));
    assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
    assert_eq!(f.refresh_count().await, 1);
    // Another owner cannot read/exchange the durable stale token while the
    // first owner holds the private unsaved rotation and retained file lock.
    let second = f.restarted();
    let (tx, cancel) = watch::channel(false);
    let other = tokio::spawn(async move { second.resolve_launch(ID, cancel).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!other.is_finished());
    tx.send(true).unwrap();
    assert_eq!(other.await.unwrap().unwrap_err(), AuthError::Cancelled);
    assert_eq!(f.refresh_count().await, 1);
    // A failed storage-only retry must not send a second refresh.
    assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
    assert_eq!(f.refresh_count().await, 1);
    files.set_write_error(None);
    f.service.reload().await.unwrap();
    assert_eq!(
        files
            .read(account.token_ref.as_ref().unwrap())
            .unwrap()
            .unwrap()
            .refresh_token()
            .expose(),
        "ROTATED_SENTINEL"
    );
    assert_eq!(f.refresh_count().await, 1);
    assert!(f.resolve().await.is_ok());
    let requests = f.server.received_requests().await.unwrap();
    let refreshes: Vec<_> = requests.iter().filter(|r| r.url.path() == "/1").collect();
    assert_eq!(refreshes.len(), 2);
    assert!(String::from_utf8_lossy(&refreshes[1].body).contains("ROTATED_SENTINEL"));
}

#[tokio::test]
async fn invalid_grant_preserves_history_but_explicit_negative_and_mismatch_revoke_it() {
    for (stage, status, body, expected, keep) in [
        (
            1,
            400,
            json!({"error":"invalid_grant"}),
            AuthError::ReauthenticationRequired,
            true,
        ),
        (5, 200, json!({"items":[]}), AuthError::NoEntitlement, false),
        (6, 404, json!({}), AuthError::ProfileMissing, false),
        (
            6,
            200,
            json!({"id":"fedcba9876543210fedcba9876543210","name":"Different"}),
            AuthError::IdentityMismatch,
            false,
        ),
        (
            6,
            200,
            json!({"id":"invalid","name":"Invalid"}),
            AuthError::InvalidResponse {
                stage: AuthStage::Profile,
            },
            true,
        ),
        (
            4,
            403,
            json!({}),
            AuthError::Http {
                stage: AuthStage::MinecraftToken,
                status: 403,
            },
            true,
        ),
    ] {
        let f = Fixture::new().await;
        let account = f.seed(NOW + 60).await;
        f.chain("Renamed").await;
        Mock::given(path(format!("/{stage}")))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .with_priority(1)
            .mount(&f.server)
            .await;
        assert_eq!(f.resolve().await.unwrap_err(), expected);
        let snapshot = f.restarted().snapshot().await.unwrap();
        assert_eq!(snapshot.current().unwrap().verification().is_some(), keep);
        if expected == AuthError::ReauthenticationRequired {
            let record = f
                .keyring
                .read(account.token_ref.as_ref().unwrap())
                .unwrap()
                .unwrap();
            assert!(record.reauth_required());
            assert!(record.minecraft().is_none());
            assert_eq!(f.resolve().await.unwrap_err(), expected);
            assert_eq!(f.refresh_count().await, 1);
        }
    }
}

#[tokio::test]
async fn expiry_margin_clock_rollback_restart_missing_and_omitted_rotation() {
    for (expires, now, requests) in [
        (NOW + 61, NOW, 0),
        (NOW + 60, NOW, 1),
        (NOW + 59, NOW, 1),
        (NOW + 3600, NOW - 1, 1),
        (NOW + 3600, NOW + 3600, 1),
    ] {
        let f = Fixture::new().await;
        f.seed(expires).await;
        f.chain("Renamed").await;
        f.clock.0.store(now, Ordering::SeqCst);
        let (_tx, cancel) = watch::channel(false);
        assert!(f.restarted().resolve_launch(ID, cancel).await.is_ok());
        assert_eq!(
            f.refresh_count().await,
            requests,
            "expiry {expires} now {now}"
        );
    }
    let f = Fixture::new().await;
    let account = f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    Mock::given(path("/1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token":"MS_SECRET_SENTINEL","expires_in":3600})),
        )
        .with_priority(1)
        .mount(&f.server)
        .await;
    assert!(f.resolve().await.is_ok());
    assert_eq!(
        f.keyring
            .read(account.token_ref.as_ref().unwrap())
            .unwrap()
            .unwrap()
            .refresh_token()
            .expose(),
        "OLD_SECRET_SENTINEL"
    );
    f.keyring
        .delete(account.token_ref.as_ref().unwrap())
        .unwrap();
    assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialMissing);
}

#[tokio::test]
async fn failed_revocation_is_fail_closed_and_retried_without_network() {
    let f = Fixture::new().await;
    f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    Mock::given(path("/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[]})))
        .with_priority(1)
        .mount(&f.server)
        .await;
    f.service.set_metadata_fault(WriteStage::Rename);
    assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
    assert!(!f.service.snapshot().await.unwrap().is_offline_allowed());
    let before_retry = f.service.snapshot().await.unwrap();
    let changes = f.service.changes();
    f.service.reload().await.unwrap();
    assert_eq!(f.service.snapshot().await.unwrap(), before_retry);
    assert!(!changes.has_changed().unwrap());
    assert!(!f.restarted().snapshot().await.unwrap().is_offline_allowed());
    assert_eq!(f.refresh_count().await, 1);
}

#[tokio::test]
async fn refresh_does_not_change_unrelated_current_and_watch_revision_counts_bursts() {
    let f = Fixture::new().await;
    f.seed(NOW + 60).await;
    let other = super::super::testing::seed_verified(
        &f.service,
        "Other",
        Uuid::from_u128(2),
        "OTHER_SECRET_SENTINEL",
        NOW,
        NOW + 3600,
    )
    .await
    .unwrap();
    let before = *f.service.changes().borrow();
    let mut changes = f.service.changes();
    f.service.set_current(ID).await.unwrap();
    f.service.set_current(&other.id).await.unwrap();
    changes.changed().await.unwrap();
    assert_eq!(*changes.borrow_and_update(), before + 2);
    f.chain("Renamed").await;
    f.resolve().await.unwrap();
    assert_eq!(
        f.service.snapshot().await.unwrap().current().unwrap().id,
        other.id
    );
    assert_eq!(*changes.borrow(), before + 3);
    let offline = f.service.add_offline("Entitled").await.unwrap();
    let snapshot = f.service.snapshot().await.unwrap();
    assert!(snapshot.is_launchable(&offline.id));
    assert_eq!(snapshot.current().unwrap().id, other.id);
    assert_eq!(*changes.borrow(), before + 4);
}

#[tokio::test]
async fn corrupt_cleanup_journal_prevents_refresh_and_cannot_be_silently_discarded() {
    let f = Fixture::new().await;
    f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    std::fs::write(
        f.data.root().join("credentials/cleanup.json"),
        b"SECRET_SENTINEL invalid journal",
    )
    .unwrap();
    assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialCorrupt);
    assert_eq!(f.refresh_count().await, 0);
    assert_eq!(
        std::fs::read(f.data.root().join("credentials/cleanup.json")).unwrap(),
        b"SECRET_SENTINEL invalid journal"
    );
}

#[tokio::test]
async fn production_constructor_is_lazy_and_pre_cancelled_login_has_no_requests() {
    let f = Fixture::new().await;
    let missing = f.data.root().join("not-created");
    let _service = AccountService::production(DataDirs::new(&missing)).unwrap();
    assert!(!missing.exists());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_cancel, token) = watch::channel(true);
    f.service.start(tx, token).await;
    assert_eq!(
        rx.recv().await,
        Some(LoginEvent::Failed(
            super::super::login::LoginError::Cancelled
        ))
    );
    assert_eq!(rx.recv().await, None);
    assert!(f.server.received_requests().await.unwrap().is_empty());
    assert!(!f.data.root().join("credentials").exists());
}

#[tokio::test]
async fn postrename_sync_failure_never_emits_done_but_restart_preserves_visible_login() {
    let f = Fixture::new().await;
    f.chain("Renamed").await;
    f.service.set_metadata_fault(WriteStage::DirectorySync);
    let events = f.login().await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, LoginEvent::Done { .. }))
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, LoginEvent::Failed(_)))
            .count(),
        1
    );
    assert_eq!(*f.service.changes().borrow(), 0);
    let snapshot = f.restarted().snapshot().await.unwrap();
    assert_eq!(snapshot.current().unwrap().username, "Renamed");
    assert!(
        f.keyring
            .read(snapshot.current().unwrap().token_ref.as_ref().unwrap())
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn corruption_reload_publishes_an_error_revision_instead_of_stale_success() {
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
    let mut changes = f.service.changes();
    let before = *changes.borrow_and_update();
    std::fs::write(
        f.data.root().join("accounts.toml"),
        b"SECRET_SENTINEL invalid",
    )
    .unwrap();
    assert_eq!(
        f.service.reload().await.unwrap_err(),
        AuthError::CredentialCorrupt
    );
    assert_eq!(*changes.borrow(), before + 1);
    assert!(f.service.snapshot().await.is_err());
    assert_eq!(
        f.service.reload().await.unwrap_err(),
        AuthError::CredentialCorrupt
    );
    assert_eq!(
        *changes.borrow(),
        before + 1,
        "identical error is not another snapshot change"
    );
}

#[tokio::test]
async fn automatic_file_login_reports_storage_then_durable_done_without_keyring() {
    let f = Fixture::new().await;
    f.keyring.set_error(Some(CredentialError::Unavailable));
    f.chain("Renamed").await;
    let events = f.login().await;
    assert!(matches!(
        events.as_slice(),
        [
            LoginEvent::Code(_),
            LoginEvent::Waiting,
            LoginEvent::Storage(super::super::login::StorageKind::LocalFile),
            LoginEvent::Done { .. }
        ]
    ));
    let snapshot = f.restarted().snapshot().await.unwrap();
    let account = snapshot.current().unwrap();
    let bytes = std::fs::read(
        f.data
            .root()
            .join("credentials")
            .join(format!("{}.json", account.token_ref.as_ref().unwrap())),
    )
    .unwrap();
    assert_eq!(
        super::super::credentials::CredentialRecord::decode(&bytes)
            .unwrap()
            .refresh_token()
            .expose(),
        "ROTATED_SENTINEL"
    );
    assert!(account.verification().is_some());
}

#[tokio::test]
async fn legacy_missing_reference_requires_reauthentication_without_blessing_proof() {
    let f = Fixture::new().await;
    let mut store = AccountStore::load(&f.data).unwrap();
    store.add_msa("Legacy", Uuid::parse_str(ID).unwrap(), None);
    store.save().unwrap();
    assert_eq!(
        f.resolve().await.unwrap_err(),
        AuthError::ReauthenticationRequired
    );
    assert!(!f.service.snapshot().await.unwrap().is_offline_allowed());
    assert!(f.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn failed_full_session_write_retries_verified_metadata_with_the_record() {
    let f = Fixture::new().await;
    f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    let keyring = f.keyring.clone();
    Mock::given(path("/6"))
        .respond_with(move |_: &wiremock::Request| {
            keyring.set_write_error(Some(CredentialError::Io));
            ResponseTemplate::new(200).set_body_json(json!({"id":ID,"name":"Renamed"}))
        })
        .with_priority(1)
        .mount(&f.server)
        .await;
    assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
    f.keyring.set_write_error(None);
    assert_eq!(f.resolve().await.unwrap().account().name(), "Renamed");
    assert_eq!(
        f.restarted()
            .snapshot()
            .await
            .unwrap()
            .current()
            .unwrap()
            .username,
        "Renamed"
    );
    assert_eq!(f.refresh_count().await, 1);
}

#[tokio::test]
async fn identical_selection_and_reload_do_not_publish_changes() {
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
    let before = f.service.snapshot().await.unwrap();
    let changes = f.service.changes();
    f.service.set_current(ID).await.unwrap();
    f.service.reload().await.unwrap();
    assert_eq!(f.service.snapshot().await.unwrap(), before);
    assert!(!changes.has_changed().unwrap());
    assert_eq!(
        f.restarted()
            .snapshot()
            .await
            .unwrap()
            .current()
            .unwrap()
            .id,
        ID
    );
}

#[tokio::test]
async fn unchanged_metadata_refresh_still_notifies_credential_transition() {
    let f = Fixture::new().await;
    f.chain("Player_123").await;
    f.login().await;
    let before = f.service.snapshot().await.unwrap();
    let reference = before.current().unwrap().token_ref.as_ref().unwrap();
    // Expire only the cached session, keeping the clock/profile/proof identical.
    let record = super::super::credentials::CredentialRecord::new(
        super::super::secret::SecretString::new("ROTATED_SENTINEL".into()),
        None,
        false,
    )
    .unwrap();
    f.keyring.write(reference, &record).unwrap();
    let changes = f.service.changes();
    assert!(f.resolve().await.is_ok());
    let after = f.service.snapshot().await.unwrap();
    assert_eq!(after.accounts(), before.accounts());
    assert_eq!(after.current(), before.current());
    assert!(changes.has_changed().unwrap());
    assert_eq!(after.revision(), before.revision() + 1);
}

#[tokio::test]
async fn first_selection_change_after_restart_notifies_without_prior_snapshot() {
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
    super::super::testing::seed_verified(
        &f.service,
        "Other",
        Uuid::from_u128(2),
        "OTHER_SECRET_SENTINEL",
        NOW,
        NOW + 3600,
    )
    .await
    .unwrap();
    let restarted = f.restarted();
    let changes = restarted.changes();
    restarted.set_current(ID).await.unwrap();
    assert!(changes.has_changed().unwrap());
    assert_eq!(*changes.borrow(), 1);
    assert_eq!(
        restarted.snapshot().await.unwrap().current().unwrap().id,
        ID
    );
}
