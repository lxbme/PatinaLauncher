use super::*;
use crate::{
    accounts::AuthenticatedLaunchError,
    instance::Instance,
    launch::{
        LaunchCommand,
        session::{GameSession, PendingLaunch},
    },
};

fn pending(
    f: &Fixture,
    account: &crate::launch::LaunchAccount,
) -> (PendingLaunch, std::path::PathBuf) {
    let dir = f.data.instances().join("service-spawn");
    std::fs::create_dir_all(dir.join("minecraft")).unwrap();
    let inst = Instance {
        id: "service-spawn".into(),
        dir: dir.clone(),
        config: Default::default(),
    };
    let command = LaunchCommand {
        program: f.data.root().join("deliberately-absent-executable"),
        args: vec![],
        cwd: dir.clone(),
    };
    (
        GameSession::prepare_launch_with_account_for_test(&inst, command, account).unwrap(),
        dir,
    )
}

#[tokio::test]
async fn mismatched_prepared_identity_and_same_uuid_stale_session_never_spawn() {
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
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
    let (_tx, cancel) = watch::channel(false);
    let other_grant = f.service.resolve_launch(&other.id, cancel).await.unwrap();
    let grant = f.resolve().await.unwrap();
    let (prepared, dir) = pending(&f, other_grant.account());
    let result = f.service.launch_pending(grant, prepared, None).await;
    assert!(matches!(
        result,
        Err(AuthenticatedLaunchError::Auth(AuthError::IdentityMismatch))
    ));
    assert!(!dir.join("patina-logs/current.log").exists());
    let grant = f.resolve().await.unwrap();
    let mut stale = grant.account().clone();
    stale.access_token = "STALE_SECRET_SENTINEL".into();
    let (prepared, dir) = pending(&f, &stale);
    let result = f.service.launch_pending(grant, prepared, None).await;
    assert!(matches!(
        result,
        Err(AuthenticatedLaunchError::Auth(AuthError::IdentityMismatch))
    ));
    assert!(!dir.join("patina-logs/current.log").exists());
}

#[tokio::test]
async fn removed_or_expired_grant_never_reaches_spawn_but_matching_grant_does() {
    for removed in [false, true] {
        let f = Fixture::new().await;
        f.seed(NOW + 3600).await;
        let grant = f.resolve().await.unwrap();
        let (prepared, dir) = pending(&f, grant.account());
        if removed {
            f.service.remove(ID).await.unwrap();
        } else {
            f.clock.0.store(NOW + 3600, Ordering::SeqCst);
        }
        let result = f.service.launch_pending(grant, prepared, None).await;
        if removed {
            assert!(matches!(
                result,
                Err(AuthenticatedLaunchError::Auth(AuthError::Account(
                    AccountError::NotFound(_)
                )))
            ));
        } else {
            assert!(matches!(
                result,
                Err(AuthenticatedLaunchError::Auth(AuthError::SessionExpired))
            ));
        }
        assert!(!dir.join("patina-logs/current.log").exists());
    }
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
    let grant = f.resolve().await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    // A nonexistent executable proves the matching path reaches the real spawn
    // boundary without starting any process/game. Spawn opens the current log.
    assert!(matches!(
        f.service.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Session(_))
    ));
    assert!(dir.join("patina-logs/current.log").exists());
}

#[tokio::test]
async fn cancelled_spawn_waiter_drops_pending_without_starting_after_lock_release() {
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
    let grant = f.resolve().await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    let lock = crate::platform::credentials::PrivateCredentialDir::open(&f.data)
        .unwrap()
        .try_lock()
        .unwrap()
        .unwrap();
    let service = f.service.clone();
    let waiter = tokio::spawn(async move { service.launch_pending(grant, prepared, None).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    waiter.abort();
    assert!(matches!(waiter.await,Err(error) if error.is_cancelled()));
    drop(lock);
    f.service.reload().await.unwrap();
    assert!(!dir.join("patina-logs/current.log").exists());
}

#[tokio::test]
async fn changed_metadata_identity_invalidates_a_prepared_grant() {
    let f = Fixture::new().await;
    let account = f.seed(NOW + 3600).await;
    let grant = f.resolve().await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
    let mut metadata = AccountStore::load(&f.data).unwrap();
    metadata.add_msa("Changed", account.uuid, account.token_ref);
    metadata.save().unwrap();
    assert!(matches!(
        f.service.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Auth(AuthError::IdentityMismatch))
    ));
    assert!(!dir.join("patina-logs/current.log").exists());
}

#[test]
fn authenticated_launch_error_redacts_legacy_session_payload() {
    let error = AuthenticatedLaunchError::Session(crate::launch::session::SessionError::Io {
        path: "SECRET_SENTINEL".into(),
        error: "SECRET_SENTINEL".into(),
    });
    assert!(!format!("{error:?} {error}").contains("SECRET_SENTINEL"));
    assert!(std::error::Error::source(&error).is_none());
}

#[tokio::test]
async fn unrelated_changes_preserve_grant_but_relogin_refresh_and_revocation_invalidate_it() {
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
    let grant = f.resolve().await.unwrap();
    let (prepared, dir) = pending(&f, grant.account());
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
    assert!(matches!(
        f.service.launch_pending(grant, prepared, None).await,
        Err(AuthenticatedLaunchError::Session(_))
    ));
    assert!(dir.join("patina-logs/current.log").exists());

    for mutation in ["relogin", "refresh", "revocation"] {
        let f = Fixture::new().await;
        f.seed(NOW + 3600).await;
        let grant = f.resolve().await.unwrap();
        let (prepared, dir) = pending(&f, grant.account());
        if mutation == "relogin" {
            f.seed(NOW + 3600).await;
        } else {
            f.clock.0.store(NOW + 3540, Ordering::SeqCst);
            f.chain("Original").await;
            if mutation == "revocation" {
                Mock::given(path("/5"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[]})))
                    .with_priority(1)
                    .mount(&f.server)
                    .await;
                assert_eq!(f.resolve().await.unwrap_err(), AuthError::NoEntitlement);
            } else {
                f.resolve().await.unwrap();
            }
        }
        let result = f.service.launch_pending(grant, prepared, None).await;
        assert!(
            matches!(
                result,
                Err(AuthenticatedLaunchError::Auth(
                    AuthError::IdentityMismatch | AuthError::SessionExpired
                ))
            ),
            "{mutation}"
        );
        assert!(!dir.join("patina-logs/current.log").exists(), "{mutation}");
    }
}
