//! Historical policy through the real service, isolated HTTP and temporary storage.
use super::{
    AccountError, AccountService, AccountStore, AuthenticatedLaunchError,
    error::{AuthError, AuthStage},
    login::{DeviceCodeLogin, LoginEvent},
    service_tests::{Fixture, ID, NOW},
};
use crate::{
    instance::Instance,
    launch::{
        LaunchCommand,
        session::{GameSession, PendingLaunch},
    },
};
use serde_json::json;
use std::sync::atomic::Ordering;
use tokio::sync::{mpsc, watch};
use wiremock::{Mock, ResponseTemplate, matchers::path};

#[cfg(not(feature = "dev-offline"))]
const _: () = assert!(!crate::DEV_OFFLINE);

async fn login(f: &Fixture) -> super::Account {
    f.chain("Owner").await;
    assert!(matches!(
        f.login().await.last(),
        Some(LoginEvent::Done { .. })
    ));
    f.service
        .snapshot()
        .await
        .unwrap()
        .current()
        .unwrap()
        .clone()
}
async fn resolve(service: &AccountService, id: &str) -> Result<super::LaunchGrant, AuthError> {
    let (_tx, cancel) = watch::channel(false);
    service.resolve_launch(id, cancel).await
}
fn pending(
    f: &Fixture,
    account: &crate::launch::LaunchAccount,
) -> (PendingLaunch, std::path::PathBuf) {
    let dir = f.data.instances().join("policy-spawn");
    std::fs::create_dir_all(dir.join("minecraft")).unwrap();
    let instance = Instance {
        id: "policy-spawn".into(),
        dir: dir.clone(),
        config: Default::default(),
    };
    let command = LaunchCommand {
        program: f.data.root().join("deliberately-absent-executable"),
        args: vec![],
        cwd: dir.clone(),
    };
    (
        GameSession::prepare_launch_with_account_for_test(&instance, command, account).unwrap(),
        dir,
    )
}

#[tokio::test]
async fn offline_names_and_collisions() {
    let f = Fixture::new().await;
    assert_eq!(
        f.service.add_offline("Notch").await.unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    login(&f).await;
    for name in ["", "ab", "abcdefghijklmnopq", "bad name", "名字名字", "a-b"] {
        assert!(matches!(
            f.service.add_offline(name).await,
            Err(AuthError::Account(AccountError::InvalidName(_)))
        ));
    }
    for name in ["abc", "abcdefghijklmnop", "Under_score", "Notch"] {
        let account = f.service.add_offline(name).await.unwrap();
        assert_eq!(account.uuid, crate::launch::offline_uuid(name));
    }
    let notch = f.service.add_offline("Notch").await.unwrap();
    assert_eq!(
        notch.uuid.to_string(),
        "b50ad385-829d-3141-a216-7e7d7539ba7f"
    );
    assert_eq!(f.service.add_offline("nOTCH").await.unwrap(), notch);
    let text = std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap();
    assert!(text.contains("offline_origin = \"entitled\""));
    assert_eq!(
        f.service.snapshot().await.unwrap().current().unwrap().id,
        ID
    );
    let grant = resolve(&f.service, &notch.id).await.unwrap();
    assert_eq!(grant.account().uuid(), notch.uuid);
    assert_eq!(grant.account().name(), "Notch");
    assert_eq!(
        grant.account().user_type(),
        crate::launch::account::UserType::Legacy
    );
}

#[tokio::test]
async fn last_owner_removal_blocks_existing_offline() {
    let f = Fixture::new().await;
    let owner = login(&f).await;
    let offline = f.service.add_offline("Notch").await.unwrap();
    f.service.set_current(&offline.id).await.unwrap();
    let grant = resolve(&f.service, &offline.id).await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    let revision = f.service.snapshot().await.unwrap().revision();
    f.service.remove(&owner.id).await.unwrap();
    let snapshot = f.service.snapshot().await.unwrap();
    assert!(snapshot.revision() > revision);
    assert!(!snapshot.is_offline_allowed());
    assert!(snapshot.accounts().iter().any(|a| a.id == offline.id));
    assert!(!snapshot.is_launchable(&offline.id));
    assert!(snapshot.current().is_none());
    assert_eq!(
        f.service.set_current(&offline.id).await.unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    assert_eq!(
        resolve(&f.service, &offline.id).await.unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    assert!(matches!(
        f.service.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Auth(AuthError::OfflineNotAllowed))
    ));
    assert!(!dir.join("patina-logs/current.log").exists());
    let restarted = f.restarted().snapshot().await.unwrap();
    assert!(!restarted.is_offline_allowed());
    assert!(restarted.current().is_none());
    assert_eq!(restarted.accounts(), snapshot.accounts());
    f.service.remove(&offline.id).await.unwrap();
    assert!(f.service.snapshot().await.unwrap().accounts().is_empty());
}

#[tokio::test]
async fn another_owner_preserves_offline() {
    let f = Fixture::new().await;
    login(&f).await;
    let offline = f.service.add_offline("Notch").await.unwrap();
    let grant = resolve(&f.service, &offline.id).await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    // A second real profile, not an invented entitled default.
    Mock::given(path("/6"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":"fedcba9876543210fedcba9876543210", "name":"Other"})),
        )
        .with_priority(1)
        .mount(&f.server)
        .await;
    assert!(matches!(
        f.login().await.last(),
        Some(LoginEvent::Done { .. })
    ));
    f.service.set_current(&offline.id).await.unwrap();
    f.service.remove(ID).await.unwrap();
    let snapshot = f.service.snapshot().await.unwrap();
    assert!(snapshot.is_offline_allowed());
    assert_eq!(snapshot.current().unwrap().id, offline.id);
    assert!(snapshot.is_launchable(&offline.id));
    assert!(matches!(
        f.service.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Session(_))
    ));
    assert!(dir.join("patina-logs/current.log").exists());
    assert!(f.restarted().snapshot().await.unwrap().is_offline_allowed());
}

#[tokio::test]
async fn offline_policy_error_matrix_and_restart() {
    let cases = [
        (
            1,
            400,
            json!({"error":"invalid_grant"}),
            AuthError::ReauthenticationRequired,
            true,
        ),
        (
            5,
            500,
            json!({}),
            AuthError::Http {
                stage: AuthStage::Entitlement,
                status: 500,
            },
            true,
        ),
        (
            5,
            429,
            json!({}),
            AuthError::Http {
                stage: AuthStage::Entitlement,
                status: 429,
            },
            true,
        ),
        (
            5,
            403,
            json!({}),
            AuthError::Http {
                stage: AuthStage::Entitlement,
                status: 403,
            },
            true,
        ),
        (
            6,
            403,
            json!({}),
            AuthError::Http {
                stage: AuthStage::Profile,
                status: 403,
            },
            true,
        ),
        (
            6,
            401,
            json!({}),
            AuthError::Http {
                stage: AuthStage::Profile,
                status: 401,
            },
            true,
        ),
        (5, 200, json!({"items":[]}), AuthError::NoEntitlement, false),
        (
            5,
            200,
            json!({"items":[{"name":"unrelated"}]}),
            AuthError::NoEntitlement,
            false,
        ),
        (
            5,
            200,
            json!({}),
            AuthError::InvalidResponse {
                stage: AuthStage::Entitlement,
            },
            true,
        ),
        (6, 404, json!({}), AuthError::ProfileMissing, false),
        (
            6,
            200,
            json!({"id":ID}),
            AuthError::InvalidResponse {
                stage: AuthStage::Profile,
            },
            true,
        ),
        (
            6,
            200,
            json!({"id":"fedcba9876543210fedcba9876543210","name":"Other"}),
            AuthError::IdentityMismatch,
            false,
        ),
    ];
    for (stage, status, body, expected, keep) in cases {
        let f = Fixture::new().await;
        login(&f).await;
        let offline = f.service.add_offline("Notch").await.unwrap();
        f.service.set_current(&offline.id).await.unwrap();
        let grant = resolve(&f.service, &offline.id).await.unwrap();
        let (prepared, dir) = pending(&f, grant.account());
        f.clock.0.store(NOW + 3540, Ordering::SeqCst);
        Mock::given(path(format!("/{stage}")))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .with_priority(1)
            .mount(&f.server)
            .await;
        assert_eq!(f.resolve().await.unwrap_err(), expected);
        let snapshot = f.service.snapshot().await.unwrap();
        assert_eq!(snapshot.is_offline_allowed(), keep, "{expected:?}");
        assert_eq!(snapshot.is_launchable(&offline.id), keep);
        assert_eq!(
            snapshot.current().unwrap().id,
            if keep { &offline.id } else { ID }
        );
        assert_eq!(
            f.restarted().snapshot().await.unwrap().is_offline_allowed(),
            keep
        );
        assert_eq!(resolve(&f.service, &offline.id).await.is_ok(), keep);
        let result = f.service.launch_pending(grant, prepared, None).await;
        if keep {
            assert!(matches!(result, Err(AuthenticatedLaunchError::Session(_))));
        } else {
            assert!(matches!(
                result,
                Err(AuthenticatedLaunchError::Auth(AuthError::OfflineNotAllowed))
            ));
            assert!(!dir.join("patina-logs/current.log").exists());
        }
    }
}

#[tokio::test]
async fn expired_missing_and_unavailable_credentials_do_not_lease_historical_proof() {
    use super::credentials::{CredentialBackend, CredentialError};
    for unavailable in [false, true] {
        let f = Fixture::new().await;
        let owner = login(&f).await;
        let offline = f.service.add_offline("Notch").await.unwrap();
        f.clock
            .0
            .store(NOW + 20 * 365 * 24 * 3600, Ordering::SeqCst);
        if unavailable {
            f.keyring.set_error(Some(CredentialError::Unavailable));
        } else {
            f.keyring.delete(owner.token_ref.as_ref().unwrap()).unwrap();
        }
        assert!(f.resolve().await.is_err());
        let restarted = f.restarted();
        assert!(restarted.snapshot().await.unwrap().is_offline_allowed());
        assert!(resolve(&restarted, &offline.id).await.is_ok());
        assert!(restarted.add_offline("Another").await.is_ok());
    }
}

#[tokio::test]
async fn fake_done_cannot_verify() {
    struct FakeDone;
    impl DeviceCodeLogin for FakeDone {
        fn start(
            &self,
            events: mpsc::UnboundedSender<LoginEvent>,
            _: super::login::CancelToken,
        ) -> futures::future::BoxFuture<'static, ()> {
            Box::pin(async move {
                let _ = events.send(LoginEvent::Done {
                    username: "Fake".into(),
                    uuid: uuid::Uuid::parse_str(ID).unwrap(),
                });
            })
        }
    }
    let f = Fixture::new().await;
    let mut store = AccountStore::load(&f.data).unwrap();
    store.add_msa(
        "Fake",
        uuid::Uuid::parse_str(ID).unwrap(),
        Some("11111111-1111-1111-1111-111111111111".into()),
    );
    store.save().unwrap();
    let (events, mut received) = mpsc::unbounded_channel();
    let (_tx, cancel) = watch::channel(false);
    FakeDone.start(events, cancel).await;
    assert!(matches!(
        received.recv().await,
        Some(LoginEvent::Done { .. })
    ));
    let snapshot = f.service.snapshot().await.unwrap();
    assert!(!snapshot.is_offline_allowed());
    assert!(snapshot.accounts()[0].verification().is_none());
    assert_eq!(
        f.service.add_offline("Notch").await.unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    assert!(!f.restarted().snapshot().await.unwrap().is_offline_allowed());
}

#[tokio::test]
async fn legacy_dev_entries_never_promote() {
    for marker in ["", "offline_origin = \"development\"\n"] {
        let f = Fixture::new().await;
        let off = crate::launch::offline_uuid("Legacy");
        let id = format!("offline-{}", off.simple());
        std::fs::write(f.data.root().join("accounts.toml"), format!("current = \"{id}\"\n[[accounts]]\nid = \"{id}\"\nkind = \"offline\"\nusername = \"Legacy\"\nuuid = \"{off}\"\n{marker}")).unwrap();
        let before: toml::Value =
            toml::from_str(&std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap())
                .unwrap();
        let snapshot = f.service.snapshot().await.unwrap();
        assert_eq!(
            snapshot.accounts().len(),
            usize::from(cfg!(feature = "dev-offline"))
        );
        assert!(!snapshot.is_offline_allowed());
        AccountStore::load(&f.data).unwrap().save().unwrap();
        let after: toml::Value =
            toml::from_str(&std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap())
                .unwrap();
        assert_eq!(
            before, after,
            "hidden legacy metadata and stored current round trip unchanged"
        );
        login(&f).await;
        assert_eq!(
            f.service.add_offline("lEGACY").await.unwrap_err(),
            AuthError::OfflineConflict
        );
        let snapshot = f.restarted().snapshot().await.unwrap();
        assert_eq!(
            snapshot.accounts().iter().any(|a| a.id == id),
            cfg!(feature = "dev-offline")
        );
        let after: toml::Value =
            toml::from_str(&std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap())
                .unwrap();
        let legacy = after["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"].as_str() == Some(&id))
            .unwrap();
        assert_eq!(legacy, &before["accounts"][0]);
        #[cfg(feature = "dev-offline")]
        {
            f.service.remove(&id).await.unwrap();
            let entitled = f.service.add_offline("Legacy").await.unwrap();
            assert_eq!(entitled.id, id);
            assert_eq!(
                f.service.add_dev_offline("LEGACY").await.unwrap_err(),
                AuthError::OfflineConflict
            );
        }
    }
}

#[cfg(feature = "dev-offline")]
#[tokio::test]
async fn developer_bypass_is_separate_from_normal_creation_and_entitlement() {
    let f = Fixture::new().await;
    let dev = f.service.add_dev_offline("Developer").await.unwrap();
    assert!(!f.service.snapshot().await.unwrap().is_offline_allowed());
    assert_eq!(
        f.service.add_offline("Normal").await.unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    assert!(resolve(&f.service, &dev.id).await.is_ok());
    login(&f).await;
    assert_eq!(
        f.service.add_offline("DEVELOPER").await.unwrap_err(),
        AuthError::OfflineConflict
    );
    f.service.remove(ID).await.unwrap();
    assert!(resolve(&f.service, &dev.id).await.is_ok());
    assert!(!f.restarted().snapshot().await.unwrap().is_offline_allowed());
    let text = std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap();
    assert!(text.contains("offline_origin = \"development\""));
}

#[tokio::test]
async fn incomplete_fresh_auth_never_establishes_history() {
    for (stage, status, body) in [
        (2, 500, json!({})), // Microsoft-only success is insufficient.
        (5, 403, json!({})),
        (5, 200, json!({"items":[]})),
        (5, 200, json!({"items":"malformed"})),
        (6, 404, json!({})),
        (6, 200, json!({"id":ID})),
    ] {
        let f = Fixture::new().await;
        f.chain("Owner").await;
        Mock::given(path(format!("/{stage}")))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .with_priority(1)
            .mount(&f.server)
            .await;
        let events = f.login().await;
        assert!(matches!(events.last(), Some(LoginEvent::Failed(_))));
        assert!(!events.iter().any(|e| matches!(e, LoginEvent::Done { .. })));
        let snapshot = f.restarted().snapshot().await.unwrap();
        assert!(snapshot.accounts().is_empty());
        assert!(!snapshot.is_offline_allowed());
        assert_eq!(
            f.service.add_offline("Notch").await.unwrap_err(),
            AuthError::OfflineNotAllowed
        );
    }
}

#[tokio::test]
async fn timeout_preserves_historical_policy_and_offline_spawn() {
    let f = Fixture::new().await;
    login(&f).await;
    let offline = f.service.add_offline("Notch").await.unwrap();
    f.service.set_current(&offline.id).await.unwrap();
    f.clock.0.store(NOW + 3540, Ordering::SeqCst);
    Mock::given(path("/5"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(60))
                .set_body_json(json!({"items":[]})),
        )
        .with_priority(1)
        .mount(&f.server)
        .await;
    let service = f.service.clone();
    let (_tx, cancel) = watch::channel(false);
    let refresh = tokio::spawn(async move { service.resolve_launch(ID, cancel).await });
    // Wait for the real request before advancing its production timeout. No
    // socket IO or blocking-pool progress is assumed from virtual time alone.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if f.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path() == "/5")
                .count()
                == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(16)).await;
    let result = refresh.await.unwrap();
    tokio::time::resume();
    assert_eq!(
        result.unwrap_err(),
        AuthError::Timeout {
            stage: AuthStage::Entitlement
        }
    );
    let restarted = f.restarted();
    let snapshot = restarted.snapshot().await.unwrap();
    assert!(snapshot.is_offline_allowed());
    assert_eq!(snapshot.current().unwrap().id, offline.id);
    let grant = resolve(&restarted, &offline.id).await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    assert!(matches!(
        restarted.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Session(_))
    ));
    assert!(dir.join("patina-logs/current.log").exists());
}

#[tokio::test]
async fn proof_loss_with_failed_save_stays_closed_across_offline_mutations() {
    use crate::platform::credentials::WriteStage;
    let f = Fixture::new().await;
    login(&f).await;
    let offline = f.service.add_offline("Notch").await.unwrap();
    f.service.set_current(&offline.id).await.unwrap();
    let grant = resolve(&f.service, &offline.id).await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    f.clock.0.store(NOW + 3540, Ordering::SeqCst);
    Mock::given(path("/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[]})))
        .with_priority(1)
        .mount(&f.server)
        .await;
    f.service.set_metadata_fault(WriteStage::Rename);
    assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
    let snapshot = f.service.snapshot().await.unwrap();
    assert!(!snapshot.is_offline_allowed());
    assert_eq!(snapshot.current().unwrap().id, ID);
    assert!(
        AccountStore::load(&f.data)
            .unwrap()
            .accounts()
            .iter()
            .find(|a| a.id == ID)
            .unwrap()
            .verification()
            .is_some(),
        "pre-rename failure leaves stale disk proof until retry"
    );
    #[cfg(feature = "dev-offline")]
    {
        let dev = f.service.add_dev_offline("Developer").await.unwrap();
        assert!(resolve(&f.service, &dev.id).await.is_ok());
        assert!(!f.service.snapshot().await.unwrap().is_offline_allowed());
    }
    assert_eq!(
        f.service.add_offline("Another").await.unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    assert_eq!(
        f.service.set_current(&offline.id).await.unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    assert!(matches!(
        f.service.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Auth(AuthError::OfflineNotAllowed))
    ));
    assert!(!dir.join("patina-logs/current.log").exists());
    assert!(!f.restarted().snapshot().await.unwrap().is_offline_allowed());
}

#[tokio::test]
async fn reload_revalidates_stale_service_grants_after_another_owner_removes_proof() {
    let f = Fixture::new().await;
    login(&f).await;
    let offline = f.service.add_offline("Notch").await.unwrap();
    f.service.set_current(&offline.id).await.unwrap();
    let stale = f.service.snapshot().await.unwrap();
    let grant = resolve(&f.service, &offline.id).await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    f.restarted().remove(ID).await.unwrap();
    assert!(stale.is_launchable(&offline.id));
    assert!(matches!(
        f.service.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Auth(AuthError::OfflineNotAllowed))
    ));
    assert!(!dir.join("patina-logs/current.log").exists());
    f.service.reload().await.unwrap();
    let snapshot = f.service.snapshot().await.unwrap();
    assert!(!snapshot.is_offline_allowed());
    assert!(!snapshot.is_launchable(&offline.id));
    assert!(snapshot.current().is_none());
    assert!(snapshot.revision() > stale.revision());
}

#[cfg(not(feature = "dev-offline"))]
#[tokio::test]
async fn hidden_stored_current_survives_policy_repair_until_explicit_selection() {
    let f = Fixture::new().await;
    login(&f).await;
    let offline = f.service.add_offline("Notch").await.unwrap();
    let legacy = crate::launch::offline_uuid("Legacy");
    let id = format!("offline-{}", legacy.simple());
    let file = f.data.root().join("accounts.toml");
    let text = std::fs::read_to_string(&file).unwrap().replace(
        &format!("current = \"{ID}\""),
        &format!("current = \"{id}\""),
    );
    std::fs::write(&file, format!("{text}\n[[accounts]]\nid = \"{id}\"\nkind = \"offline\"\nusername = \"Legacy\"\nuuid = \"{legacy}\"\n")).unwrap();
    f.service.reload().await.unwrap();
    f.service.add_offline("Another").await.unwrap();
    assert_eq!(
        f.service.snapshot().await.unwrap().current().unwrap().id,
        ID
    );
    let raw = || toml::from_str::<toml::Value>(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(raw()["current"].as_str(), Some(id.as_str()));
    let hidden_before = raw()["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"].as_str() == Some(&id))
        .unwrap()
        .clone();
    // Policy clears the visible selection, not the separately retained hidden choice.
    f.service.remove(ID).await.unwrap();
    assert!(f.service.snapshot().await.unwrap().current().is_none());
    assert_eq!(raw()["current"].as_str(), Some(id.as_str()));
    assert_eq!(
        raw()["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"].as_str() == Some(&id))
            .unwrap(),
        &hidden_before
    );
    login(&f).await;
    f.service.set_current(&offline.id).await.unwrap();
    assert_eq!(raw()["current"].as_str(), Some(offline.id.as_str()));
}

#[cfg(feature = "dev-offline")]
#[tokio::test]
async fn developer_grant_cannot_turn_into_an_entitled_grant_after_recreation() {
    let f = Fixture::new().await;
    let dev = f.service.add_dev_offline("Notch").await.unwrap();
    let grant = resolve(&f.service, &dev.id).await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    login(&f).await;
    f.service.remove(&dev.id).await.unwrap();
    let entitled = f.service.add_offline("Notch").await.unwrap();
    assert_eq!(dev.id, entitled.id);
    assert!(matches!(
        f.service.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Auth(AuthError::IdentityMismatch))
    ));
    assert!(!dir.join("patina-logs/current.log").exists());
    let snapshot = f.restarted().snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .accounts()
            .iter()
            .find(|a| a.id == entitled.id)
            .unwrap()
            .offline_origin(),
        Some(super::OfflineOrigin::Entitled)
    );
    f.service.remove(ID).await.unwrap();
    assert_eq!(
        resolve(&f.service, &entitled.id).await.unwrap_err(),
        AuthError::OfflineNotAllowed
    );
}

#[tokio::test]
async fn queued_offline_operations_recheck_proof_after_shared_lock_acquisition() {
    let f = Fixture::new().await;
    login(&f).await;
    let offline = f.service.add_offline("Notch").await.unwrap();
    let grant = resolve(&f.service, &offline.id).await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    let lock = crate::platform::credentials::PrivateCredentialDir::open(&f.data)
        .unwrap()
        .try_lock()
        .unwrap()
        .unwrap();
    let service = f.service.clone();
    let create = tokio::spawn(async move { service.add_offline("Another").await });
    let service = f.service.clone();
    let id = offline.id.clone();
    let select = tokio::spawn(async move { service.set_current(&id).await });
    let service = f.service.clone();
    let id = offline.id.clone();
    let (_tx, cancel) = watch::channel(false);
    let resolve = tokio::spawn(async move { service.resolve_launch(&id, cancel).await });
    let service = f.service.clone();
    let spawn = tokio::spawn(async move { service.launch_pending(grant, prepared, None).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(
        !create.is_finished()
            && !select.is_finished()
            && !resolve.is_finished()
            && !spawn.is_finished()
    );
    // Simulate another lock-owning writer committing removal before waiters enter.
    let mut metadata = AccountStore::load_durable(&f.data).unwrap();
    metadata.remove(ID).unwrap();
    metadata.save_durable(&f.data, None).unwrap();
    drop(lock);
    assert_eq!(
        create.await.unwrap().unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    assert_eq!(
        select.await.unwrap().unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    assert_eq!(
        resolve.await.unwrap().unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    assert!(matches!(
        spawn.await.unwrap(),
        Err(AuthenticatedLaunchError::Auth(AuthError::OfflineNotAllowed))
    ));
    assert!(!dir.join("patina-logs/current.log").exists());
    f.service.reload().await.unwrap();
    let snapshot = f.service.snapshot().await.unwrap();
    assert!(snapshot.current().is_none());
    assert_eq!(snapshot.accounts().len(), 1);
    assert_eq!(snapshot.accounts()[0].id, offline.id);
}

#[tokio::test]
async fn offline_grant_preserves_exact_prepared_identity_across_selection() {
    let f = Fixture::new().await;
    login(&f).await;
    let first = f.service.add_offline("First").await.unwrap();
    let second = f.service.add_offline("Second").await.unwrap();
    let grant = resolve(&f.service, &first.id).await.unwrap();
    let other = resolve(&f.service, &second.id).await.unwrap();
    let (prepared, dir) = pending(&f, other.account());
    assert!(matches!(
        f.service.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Auth(AuthError::IdentityMismatch))
    ));
    assert!(!dir.join("patina-logs/current.log").exists());
    let grant = resolve(&f.service, &first.id).await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    f.service.set_current(&second.id).await.unwrap();
    assert!(matches!(
        f.service.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Session(_))
    ));
    assert!(dir.join("patina-logs/current.log").exists());
}

#[tokio::test]
async fn historical_proof_does_not_require_a_credential_reference() {
    let f = Fixture::new().await;
    login(&f).await;
    let mut metadata = AccountStore::load(&f.data).unwrap();
    metadata.accounts[0].token_ref = None;
    metadata.save().unwrap();
    let restarted = f.restarted();
    let snapshot = restarted.snapshot().await.unwrap();
    assert!(snapshot.is_offline_allowed());
    assert!(!snapshot.is_launchable(ID));
    assert!(snapshot.current().is_none());
    assert_eq!(snapshot.accounts().len(), 1);
    assert_eq!(snapshot.accounts()[0].id, ID);
    assert!(snapshot.accounts()[0].verification().is_some());
    let offline = restarted.add_offline("Notch").await.unwrap();
    let second = restarted.add_offline("Second").await.unwrap();
    assert!(resolve(&restarted, &offline.id).await.is_ok());
    assert_eq!(
        restarted.set_current(ID).await.unwrap_err(),
        AuthError::ReauthenticationRequired
    );
    assert_eq!(
        restarted.snapshot().await.unwrap().current().unwrap().id,
        offline.id,
        "rejected selection keeps the prior valid current"
    );
    restarted.remove(&offline.id).await.unwrap();
    assert_eq!(
        restarted.snapshot().await.unwrap().current().unwrap().id,
        second.id,
        "fallback skips the visible Msa without a reference"
    );
    restarted.remove(&second.id).await.unwrap();
    assert!(restarted.snapshot().await.unwrap().current().is_none());
    assert!(restarted.snapshot().await.unwrap().is_offline_allowed());
    // Authenticated re-sign-in repairs the reference and explicitly selects Msa.
    assert!(matches!(
        f.login().await.last(),
        Some(LoginEvent::Done { .. })
    ));
    restarted.reload().await.unwrap();
    let snapshot = restarted.snapshot().await.unwrap();
    assert_eq!(snapshot.current().unwrap().id, ID);
    assert!(snapshot.current().unwrap().token_ref.is_some());
    assert!(snapshot.is_launchable(ID));
}
