//! CLI runners against the real protocol, service transactions and temporary credentials.
use std::sync::Arc;

use patina_core::accounts::error::{AuthError, AuthStage};
use patina_core::accounts::login::LoginEvent;
use patina_core::accounts::testing::{
    AuthClock, AuthHttp, CredentialError, MemoryCredentials, MsaClient, credential_store,
};
use patina_core::accounts::{AccountService, OfflineOrigin};
use patina_core::paths::DataDirs;
use patina_dev::accounts::{login_event_message, run_account_command_with_output};
use patina_dev::cli::AccountCmd;
use serde_json::json;
use tokio::sync::watch;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const ID: &str = "0123456789abcdef0123456789abcdef";
struct Clock;
impl AuthClock for Clock {
    fn unix_seconds(&self) -> u64 {
        1_800_000_000
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    data: DataDirs,
    server: MockServer,
    keyring: Arc<MemoryCredentials>,
    service: Arc<AccountService>,
}
impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(temp.path());
        let server = MockServer::start().await;
        let keyring = Arc::new(MemoryCredentials::default());
        let service = Self::service(&data, &server, keyring.clone());
        Self {
            _temp: temp,
            data,
            server,
            keyring,
            service,
        }
    }
    fn service(
        data: &DataDirs,
        server: &MockServer,
        keyring: Arc<MemoryCredentials>,
    ) -> Arc<AccountService> {
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
            (stages[i], format!("{}/{i}", server.uri()))
        }))
        .unwrap();
        Arc::new(AccountService::for_test(
            data.clone(),
            MsaClient::for_test(http, Arc::new(Clock)),
            credential_store(data.clone(), keyring),
            Arc::new(Clock),
        ))
    }
    async fn response(&self, stage: usize, status: u16, body: serde_json::Value) {
        Mock::given(path(format!("/{stage}")))
            .and(method(if stage >= 5 { "GET" } else { "POST" }))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&self.server)
            .await;
    }
    async fn code(&self) {
        self.response(0, 200, json!({"device_code":"DEVICE_SECRET_SENTINEL", "user_code":"ABCD-EFGH", "verification_uri":"https://microsoft.com/link", "expires_in":120, "interval":1})).await;
    }
    async fn chain(&self) {
        self.code().await;
        self.response(1,200,json!({"access_token":"MS_SECRET_SENTINEL","refresh_token":"REFRESH_SECRET_SENTINEL","expires_in":3600})).await;
        for (stage, token) in [(2, "XBL_SECRET_SENTINEL"), (3, "XSTS_SECRET_SENTINEL")] {
            self.response(stage,200,json!({"Token":token,"NotAfter":"2030-01-01T00:00:00Z","DisplayClaims":{"xui":[{"uhs":"HASH_SECRET_SENTINEL","xid":"123456"}]}})).await;
        }
        self.response(
            4,
            200,
            json!({"access_token":"MC_SECRET_SENTINEL","expires_in":3600,"token_type":"Bearer"}),
        )
        .await;
        self.response(5, 200, json!({"items":[{"name":"game_minecraft"}]}))
            .await;
        self.response(6, 200, json!({"id":ID,"name":"Player_123"}))
            .await;
    }
    async fn run(&self, command: AccountCmd) -> (Result<(), AuthError>, String) {
        let (_tx, cancel) = watch::channel(false);
        let output = std::cell::RefCell::new(String::new());
        let result = run_account_command_with_output(
            self.service.clone(),
            command,
            |event| {
                if let Some(line) = login_event_message(&event) {
                    output.borrow_mut().push_str(&line);
                }
            },
            cancel,
            |line| {
                output.borrow_mut().push_str(line);
                output.borrow_mut().push('\n');
            },
        )
        .await;
        (result, output.into_inner())
    }
    async fn login(&self) {
        self.chain().await;
        self.run(AccountCmd::Login).await.0.unwrap();
    }
}

#[tokio::test]
async fn account_commands_use_auth_service() {
    let f = Fixture::new().await;
    f.login().await;
    let restarted = Fixture::service(&f.data, &f.server, f.keyring.clone());
    restarted.reload().await.unwrap();
    let snapshot = restarted.snapshot().await.unwrap();
    assert_eq!(snapshot.current().unwrap().id, ID);
    assert!(snapshot.is_offline_allowed());
    let (result, list) = f.run(AccountCmd::List).await;
    result.unwrap();
    assert!(list.contains(ID) && list.contains("Player_123") && list.contains('*'));
    f.run(AccountCmd::AddOffline {
        name: "Notch".into(),
    })
    .await
    .0
    .unwrap();
    let snapshot = f.service.snapshot().await.unwrap();
    let offline = snapshot
        .accounts()
        .iter()
        .find(|a| a.username == "Notch")
        .unwrap();
    assert_eq!(
        offline.uuid.to_string(),
        "b50ad385-829d-3141-a216-7e7d7539ba7f"
    );
    assert_eq!(offline.offline_origin(), Some(OfflineOrigin::Entitled));
    f.run(AccountCmd::Use {
        id: offline.id.clone(),
    })
    .await
    .0
    .unwrap();
    restarted.reload().await.unwrap();
    assert_eq!(
        restarted.snapshot().await.unwrap().current().unwrap().id,
        offline.id
    );
    f.run(AccountCmd::Remove { id: ID.into() }).await.0.unwrap();
    restarted.reload().await.unwrap();
    let snapshot = restarted.snapshot().await.unwrap();
    assert_eq!(snapshot.accounts().len(), 1);
    assert!(!snapshot.is_offline_allowed());
    assert!(snapshot.current().is_none());
}

#[tokio::test]
async fn cli_login_cancel_has_no_saved_account() {
    let f = Fixture::new().await;
    f.code().await;
    let (tx, cancel) = watch::channel(false);
    let mut terminal = false;
    let result = run_account_command_with_output(
        f.service.clone(),
        AccountCmd::Login,
        |event| {
            if matches!(event, LoginEvent::Code(_)) {
                tx.send(true).unwrap();
            }
            if matches!(event, LoginEvent::Failed(_)) {
                terminal = true;
            }
        },
        cancel,
        |_| panic!("login status belongs to the event sink"),
    )
    .await;
    assert_eq!(result, Err(AuthError::Cancelled));
    assert!(terminal);
    assert!(f.service.snapshot().await.unwrap().accounts().is_empty());
    assert!(!f.data.root().join("accounts.toml").exists());
    assert_eq!(f.server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn cli_release_offline_is_gated() {
    let f = Fixture::new().await;
    assert_eq!(
        f.run(AccountCmd::AddOffline {
            name: "Notch".into()
        })
        .await
        .0,
        Err(AuthError::OfflineNotAllowed)
    );
    assert!(f.service.snapshot().await.unwrap().accounts().is_empty());
}

#[tokio::test]
async fn cli_logout_reports_pending_cleanup() {
    let f = Fixture::new().await;
    f.login().await;
    f.keyring.set_error(Some(CredentialError::Unavailable));
    let (result, output) = f.run(AccountCmd::Remove { id: ID.into() }).await;
    result.unwrap();
    assert!(output.contains("cleanup pending"), "{output}");
    assert!(f.service.snapshot().await.unwrap().accounts().is_empty());
    assert!(!f.service.snapshot().await.unwrap().is_offline_allowed());
}

#[tokio::test]
async fn cli_output_has_no_secrets() {
    let f = Fixture::new().await;
    f.chain().await;
    let (result, output) = f.run(AccountCmd::Login).await;
    result.unwrap();
    assert!(output.contains("ABCD-EFGH") && output.contains("https://microsoft.com/link"));
    assert!(output.contains("Player_123"));
    assert!(!output.contains("SECRET_SENTINEL"), "{output}");
    let (_, list) = f.run(AccountCmd::List).await;
    assert!(!list.contains("ABCD-EFGH") && !list.contains("SECRET_SENTINEL"));
    f.server.reset().await;
    f.response(0, 503, json!({"error":"REMOTE_SECRET_SENTINEL"}))
        .await;
    let (result, output) = f.run(AccountCmd::Login).await;
    assert!(matches!(
        result,
        Err(AuthError::Http {
            stage: AuthStage::DeviceCode,
            status: 503
        })
    ));
    assert!(!format!("{result:?}{output}").contains("SECRET_SENTINEL"));
    assert_eq!(
        f.service.snapshot().await.unwrap().current().unwrap().id,
        ID
    );
}

#[cfg(feature = "dev-offline")]
#[tokio::test]
async fn cli_developer_offline_is_distinct_and_collision_is_not_promotion() {
    let f = Fixture::new().await;
    f.run(AccountCmd::AddDevOffline {
        name: "Notch".into(),
    })
    .await
    .0
    .unwrap();
    assert_eq!(
        f.run(AccountCmd::AddOffline {
            name: "Other".into()
        })
        .await
        .0,
        Err(AuthError::OfflineNotAllowed)
    );
    f.login().await;
    assert_eq!(
        f.run(AccountCmd::AddOffline {
            name: "nOtCh".into()
        })
        .await
        .0,
        Err(AuthError::OfflineConflict)
    );
    let snapshot = f.service.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .accounts()
            .iter()
            .find(|a| a.username == "Notch")
            .unwrap()
            .offline_origin(),
        Some(OfflineOrigin::Development)
    );
}

fn pending(
    f: &Fixture,
    account: &patina_core::launch::LaunchAccount,
) -> patina_core::launch::PendingLaunch {
    use patina_core::instance::InstanceStore;
    use patina_core::launch::{GameSession, LaunchCommand};
    let store = InstanceStore::new(f.data.instances());
    let inst = store.create("CLI launch", "1.20.1").unwrap();
    GameSession::prepare_launch_with_account_for_test(
        &inst,
        LaunchCommand {
            program: f.data.root().join("no-such-executable"),
            args: vec!["MC_SECRET_SENTINEL".into()],
            cwd: inst.dir.clone(),
        },
        account,
    )
    .unwrap()
}

#[tokio::test]
async fn cli_stored_launch_captures_identity_through_preparation() {
    use patina_core::accounts::AuthenticatedLaunchError;
    let f = Fixture::new().await;
    f.login().await;
    let offline = f.service.add_offline("Other").await.unwrap();
    let f = &f;
    let (_tx, cancel) = watch::channel(false);
    let result = patina_dev::accounts::launch_stored_account(
        f.service.clone(),
        patina_dev::accounts::resolve_stored_account(&f.service, None, cancel)
            .await
            .unwrap(),
        |account| async move {
            assert_eq!(account.name(), "Player_123");
            let prepared = pending(f, &account);
            f.service.set_current(&offline.id).await.unwrap();
            Ok(prepared)
        },
        None,
    )
    .await;
    // A missing executable reaches real spawn without starting a game. A
    // re-resolved current would reject the prepared Microsoft identity instead.
    assert!(matches!(result, Err(AuthenticatedLaunchError::Session(_))));
    assert!(!format!("{:?}", result.err()).contains("SECRET_SENTINEL"));
}

#[tokio::test]
async fn cli_stored_launch_revalidates_after_preparation() {
    use patina_core::accounts::AuthenticatedLaunchError;
    let f = Fixture::new().await;
    f.login().await;
    let f = &f;
    let (_tx, cancel) = watch::channel(false);
    let result = patina_dev::accounts::launch_stored_account(
        f.service.clone(),
        patina_dev::accounts::resolve_stored_account(&f.service, Some(ID), cancel)
            .await
            .unwrap(),
        |account| async move {
            let prepared = pending(f, &account);
            f.service.remove(ID).await.unwrap();
            Ok(prepared)
        },
        None,
    )
    .await;
    assert!(matches!(result, Err(AuthenticatedLaunchError::Auth(_))));
    assert!(!f.data.instances().read_dir().unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .join(patina_core::launch::running::CURRENT_LOG)
            .exists()
    }));
}

#[tokio::test]
async fn cli_cancel_stops_an_active_poll() {
    let f = Fixture::new().await;
    f.code().await;
    let (cancel_tx, cancel) = watch::channel(false);
    Mock::given(path("/1"))
        .and(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            // The actual request, rather than a wall-clock delay, triggers the
            // same sender action as Ctrl-C in main.
            cancel_tx.send(true).unwrap();
            ResponseTemplate::new(400).set_body_json(json!({"error":"authorization_pending"}))
        })
        .mount(&f.server)
        .await;
    let result = run_account_command_with_output(
        f.service.clone(),
        AccountCmd::Login,
        |_| {},
        cancel,
        |_| {},
    )
    .await;
    assert_eq!(result, Err(AuthError::Cancelled));
    assert_eq!(f.server.received_requests().await.unwrap().len(), 2);
    assert!(f.service.snapshot().await.unwrap().accounts().is_empty());
}

#[tokio::test]
async fn cli_logout_cleans_both_credential_backends_and_retries_pending_cleanup() {
    let f = Fixture::new().await;
    f.keyring.set_error(Some(CredentialError::Unavailable));
    f.login().await;
    let reference = f
        .service
        .snapshot()
        .await
        .unwrap()
        .current()
        .unwrap()
        .token_ref
        .clone()
        .unwrap();
    // Simulate an old keyring copy alongside the authoritative file fallback.
    f.keyring.seed(&reference, br#"{"version":1,"refresh_token":"STALE_SECRET_SENTINEL","minecraft":null,"reauth_required":false}"#).unwrap();
    assert!(f.keyring.contains_reference(&reference));
    let file = f
        .data
        .root()
        .join("credentials")
        .join(format!("{reference}.json"));
    assert!(file.exists());
    let (result, output) = f.run(AccountCmd::Remove { id: ID.into() }).await;
    result.unwrap();
    assert!(output.contains("cleanup pending"));
    assert!(!file.exists());
    assert!(f.keyring.contains_reference(&reference));
    f.keyring.set_error(None);
    let restarted = Fixture::service(&f.data, &f.server, f.keyring.clone());
    restarted.reload().await.unwrap();
    assert!(!f.keyring.contains_reference(&reference));
    assert!(restarted.snapshot().await.unwrap().accounts().is_empty());
}

#[tokio::test]
async fn cli_hidden_legacy_collision_is_not_promoted() {
    let f = Fixture::new().await;
    std::fs::write(
        f.data.root().join("accounts.toml"),
        r#"
[[accounts]]
id = "offline-b50ad385829d3141a2167e7d7539ba7f"
kind = "offline"
username = "Notch"
uuid = "b50ad385-829d-3141-a216-7e7d7539ba7f"
"#,
    )
    .unwrap();
    f.login().await;
    assert_eq!(
        f.run(AccountCmd::AddOffline {
            name: "nOtCh".into()
        })
        .await
        .0,
        Err(AuthError::OfflineConflict)
    );
    let raw = std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap();
    assert!(!raw.contains("entitled"));
    assert!(raw.contains("Notch"));
}

#[tokio::test]
async fn cli_failed_login_preserves_noncurrent_owner_selection() {
    let f = Fixture::new().await;
    f.login().await;
    let other = f.service.add_offline("Other").await.unwrap();
    f.run(AccountCmd::Use {
        id: other.id.clone(),
    })
    .await
    .0
    .unwrap();
    f.server.reset().await;
    f.response(0, 503, json!({"error":"REMOTE_SECRET_SENTINEL"}))
        .await;
    assert!(f.run(AccountCmd::Login).await.0.is_err());
    assert_eq!(
        f.service.snapshot().await.unwrap().current().unwrap().id,
        other.id
    );
    let restarted = Fixture::service(&f.data, &f.server, f.keyring.clone());
    assert_eq!(
        restarted.snapshot().await.unwrap().current().unwrap().id,
        other.id
    );
}

#[tokio::test]
async fn cli_cancellation_after_committed_storage_keeps_truthful_success() {
    let f = Fixture::new().await;
    f.chain().await;
    let (tx, cancel) = watch::channel(false);
    let mut done = 0;
    let result = run_account_command_with_output(
        f.service.clone(),
        AccountCmd::Login,
        |event| match event {
            LoginEvent::Storage(_) => {
                let _ = tx.send(true);
            }
            LoginEvent::Done { .. } => done += 1,
            LoginEvent::Failed(_) => panic!("a committed login cannot become cancelled"),
            _ => {}
        },
        cancel,
        |_| {},
    )
    .await;
    result.unwrap();
    assert_eq!(done, 1);
    let restarted = Fixture::service(&f.data, &f.server, f.keyring.clone());
    assert_eq!(
        restarted.snapshot().await.unwrap().current().unwrap().id,
        ID
    );
}
