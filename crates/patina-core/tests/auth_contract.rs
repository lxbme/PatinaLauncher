//! Public account boundary contracts. Only loopback HTTP, temporary files and fake
//! credentials; this target must never construct a production credential adapter.
use patina_core::{
    accounts::{
        AccountService, AuthenticatedLaunchError, OfflineOrigin,
        error::{AuthError, AuthStage},
        login::{DeviceCodeLogin, LoginEvent, StorageKind},
        testing::{
            AuthClock, AuthHttp, CredentialError, MemoryCredentials, MsaClient, credential_store,
        },
    },
    config::{GlobalConfig, SourceChoice},
    instance::InstanceStore,
    launch::{GameSession, LaunchCommand, PendingLaunch},
    paths::DataDirs,
};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, watch};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, body_string_contains, header, method, path},
};

const ID: &str = "0123456789abcdef0123456789abcdef";
const NOW: u64 = 1_800_000_000;
const STAGES: [AuthStage; 7] = [
    AuthStage::DeviceCode,
    AuthStage::MicrosoftToken,
    AuthStage::XboxLive,
    AuthStage::Xsts,
    AuthStage::MinecraftToken,
    AuthStage::Entitlement,
    AuthStage::Profile,
];
const SECRETS: [&str; 9] = [
    "DEVICE_SECRET_SENTINEL",
    "USER_SECRET_SENTINEL",
    "MS_SECRET_SENTINEL",
    "REFRESH_SECRET_SENTINEL",
    "ROTATED_SECRET_SENTINEL",
    "XBL_SECRET_SENTINEL",
    "XSTS_SECRET_SENTINEL",
    "HASH_SECRET_SENTINEL",
    "MC_SECRET_SENTINEL",
];

struct Clock(AtomicU64);
impl AuthClock for Clock {
    fn unix_seconds(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    data: DataDirs,
    server: MockServer,
    clock: Arc<Clock>,
    keyring: Arc<MemoryCredentials>,
    service: AccountService,
}
impl Fixture {
    async fn new(file_fallback: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(temp.path());
        let server = MockServer::start().await;
        let clock = Arc::new(Clock(AtomicU64::new(NOW)));
        let keyring = Arc::new(MemoryCredentials::default());
        if file_fallback {
            keyring.set_error(Some(CredentialError::Unavailable));
        }
        let service = Self::service(&data, &server, clock.clone(), keyring.clone());
        Self {
            _temp: temp,
            data,
            server,
            clock,
            keyring,
            service,
        }
    }
    fn service(
        data: &DataDirs,
        server: &MockServer,
        clock: Arc<Clock>,
        keyring: Arc<MemoryCredentials>,
    ) -> AccountService {
        let http = AuthHttp::for_test(std::array::from_fn(|i| {
            (STAGES[i], format!("{}/{i}", server.uri()))
        }))
        .unwrap();
        AccountService::for_test(
            data.clone(),
            MsaClient::for_test(http, clock.clone()),
            credential_store(data.clone(), keyring),
            clock,
        )
    }
    fn restart(&self) -> AccountService {
        Self::service(
            &self.data,
            &self.server,
            self.clock.clone(),
            self.keyring.clone(),
        )
    }
    async fn chain(&self, refresh: bool, expected_refresh: &str) {
        self.chain_until(refresh, expected_refresh, None).await;
    }
    async fn chain_until(
        &self,
        refresh: bool,
        expected_refresh: &str,
        terminal: Option<(usize, ResponseTemplate)>,
    ) {
        let responses = [
            json!({"device_code": SECRETS[0], "user_code": SECRETS[1], "verification_uri":"https://microsoft.com/link", "expires_in":120, "interval":1}),
            json!({"access_token": SECRETS[2], "refresh_token": if refresh { SECRETS[4] } else { SECRETS[3] }, "expires_in":3600}),
            json!({"Token":SECRETS[5], "NotAfter":"2030-01-01T00:00:00Z", "DisplayClaims":{"xui":[{"uhs":SECRETS[7],"xid":"123456"}]}}),
            json!({"Token":SECRETS[6], "NotAfter":"2030-01-01T00:00:00Z", "DisplayClaims":{"xui":[{"uhs":SECRETS[7],"xid":"123456"}]}}),
            json!({"access_token":SECRETS[8], "expires_in":3600, "token_type":"Bearer"}),
            json!({"items":[{"name":"game_minecraft"}]}),
            json!({"id":ID,"name":"Player_123"}),
        ];
        for (i, response) in responses
            .into_iter()
            .enumerate()
            .take(terminal.as_ref().map_or(7, |(stage, _)| stage + 1))
            .skip(usize::from(refresh))
        {
            let mock =
                Mock::given(path(format!("/{i}"))).and(method(if i >= 5 { "GET" } else { "POST" }));
            let mock = if i <= 1 {
                mock.and(header("content-type", "application/x-www-form-urlencoded"))
                    .and(body_string_contains(format!(
                        "client_id={}",
                        patina_core::accounts::msa_client_id()
                    )))
            } else if i <= 4 {
                mock.and(header("content-type", "application/json"))
            } else {
                mock
            };
            let mock = match i {
                0 => mock.and(body_string_contains("scope=XboxLive.signin+offline_access"))
                    .and(body_string_contains(format!("client_id={}", patina_core::accounts::msa_client_id()))),
                1 if refresh => mock.and(body_string_contains("grant_type=refresh_token"))
                    .and(body_string_contains(format!("refresh_token={expected_refresh}")))
                    .and(body_string_contains("scope=XboxLive.signin+offline_access")),
                1 => mock.and(body_string_contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"))
                    .and(body_string_contains(format!("device_code={}", SECRETS[0]))),
                2 => mock.and(body_json(json!({"Properties":{"AuthMethod":"RPS","SiteName":"user.auth.xboxlive.com","RpsTicket":format!("d={}",SECRETS[2])},"RelyingParty":"http://auth.xboxlive.com","TokenType":"JWT"}))),
                3 => mock.and(body_json(json!({"Properties":{"SandboxId":"RETAIL","UserTokens":[SECRETS[5]]},"RelyingParty":"rp://api.minecraftservices.com/","TokenType":"JWT"}))),
                4 => mock.and(body_json(json!({"identityToken":format!("XBL3.0 x={};{}", SECRETS[7], SECRETS[6])}))),
                _ => mock.and(header("authorization", format!("Bearer {}", SECRETS[8]))),
            };
            let response = terminal
                .as_ref()
                .filter(|(stage, _)| *stage == i)
                .map_or_else(
                    || ResponseTemplate::new(200).set_body_json(response),
                    |(_, response)| response.clone(),
                );
            mock.respond_with(response)
                .expect(1)
                .mount(&self.server)
                .await;
        }
    }
    async fn login(&self) -> Vec<LoginEvent> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_cancel, cancel) = watch::channel(false);
        self.service.start(tx, cancel).await;
        let mut events = vec![];
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    }
    async fn resolve(
        service: &AccountService,
        id: &str,
    ) -> Result<patina_core::accounts::LaunchGrant, AuthError> {
        let (_cancel, cancel) = watch::channel(false);
        service.resolve_launch(id, cancel).await
    }
    fn pending(
        &self,
        account: &patina_core::launch::LaunchAccount,
        program: &str,
    ) -> PendingLaunch {
        let inst = InstanceStore::new(self.data.instances())
            .create("Contract fixture", "1.20.1")
            .unwrap();
        let command = LaunchCommand {
            program: program.into(),
            args: if program == "/bin/true" {
                vec![]
            } else {
                vec!["--accessToken".into(), SECRETS[8].into()]
            },
            cwd: inst.dir.clone(),
        };
        secret_free(&format!("{command:?}"));
        GameSession::prepare_launch_with_account_for_test(&inst, command, account).unwrap()
    }
    async fn assert_requests(&self, first_stage: usize) {
        let requests = self.server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 7 - first_stage);
        for (request, stage) in requests.iter().zip(first_stage..7) {
            assert_eq!(request.url.path(), format!("/{stage}"));
            assert!(request.url.query().is_none());
            assert_eq!(
                request.method.as_str(),
                if stage >= 5 { "GET" } else { "POST" }
            );
            assert_eq!(request.headers.contains_key("authorization"), stage >= 5);
            if stage >= 5 {
                assert!(request.body.is_empty());
            }
        }
        self.server.verify().await;
    }
}
fn secret_free(text: &str) {
    for secret in SECRETS {
        assert!(!text.contains(secret), "secret sentinel leaked");
    }
}
fn saved(events: &[LoginEvent]) {
    assert!(
        matches!(events.last(), Some(LoginEvent::Done { username, .. }) if username == "Player_123"),
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, LoginEvent::Done { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn account_service_contract_survives_restart() {
    let f = Fixture::new(true).await;
    f.chain(false, "").await;
    let events = f.login().await;
    saved(&events);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LoginEvent::Storage(StorageKind::LocalFile)))
    );
    f.assert_requests(0).await;
    let offline = f.service.add_offline("LocalPlayer").await.unwrap();
    f.service.set_current(&offline.id).await.unwrap();
    let restarted = f.restart();
    let snapshot = restarted.snapshot().await.unwrap();
    assert_eq!(snapshot.current().unwrap().id, offline.id);
    assert!(snapshot.is_offline_allowed());
    assert_eq!(snapshot.accounts().len(), 2);
    // Cached credential survives loss of all in-process service state.
    assert_eq!(
        Fixture::resolve(&restarted, ID)
            .await
            .unwrap()
            .account()
            .name(),
        "Player_123"
    );
    f.assert_requests(0).await;
    f.server.reset().await;
    f.clock.0.store(NOW + 3600, Ordering::SeqCst);
    f.chain(true, SECRETS[3]).await;
    Fixture::resolve(&restarted, ID).await.unwrap();
    f.assert_requests(1).await;
    assert_eq!(
        restarted.snapshot().await.unwrap().current().unwrap().id,
        offline.id
    );
    // Backend return cannot supersede the file-pinned rotated credential.
    f.keyring.set_error(None);
    f.server.reset().await;
    f.clock.0.store(NOW + 7200, Ordering::SeqCst);
    f.chain(true, SECRETS[4]).await;
    Fixture::resolve(&f.restart(), ID).await.unwrap();
    f.assert_requests(1).await;
    secret_free(&std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap());
}

#[tokio::test]
async fn auth_never_uses_mirror_fallback() {
    let f = Fixture::new(false).await;
    let mirror = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&mirror)
        .await;
    let mut config = GlobalConfig::default();
    config.download.source = SourceChoice::Bmclapi;
    config.save(&f.data.root().join("config.toml")).unwrap();
    // Redirect the bearer-bearing entitlement request, not just the initial code.
    f.chain_until(
        false,
        "",
        Some((
            5,
            ResponseTemplate::new(302).insert_header("location", format!("{}/token", mirror.uri())),
        )),
    )
    .await;
    let events = f.login().await;
    assert!(matches!(events.last(), Some(LoginEvent::Failed(_))));
    assert!(f.service.snapshot().await.unwrap().accounts().is_empty());
    assert!(mirror.received_requests().await.unwrap().is_empty());
    assert_eq!(f.server.received_requests().await.unwrap().len(), 6);
    f.server.verify().await;
    f.server.reset().await;
    Mock::given(path("/0"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error":SECRETS[2]})))
        .expect(1)
        .mount(&f.server)
        .await;
    let events = f.login().await;
    assert!(matches!(events.last(), Some(LoginEvent::Failed(_))));
    assert!(mirror.received_requests().await.unwrap().is_empty());
    assert_eq!(f.server.received_requests().await.unwrap().len(), 1);
    secret_free(&format!("{events:?}"));
}

#[tokio::test]
async fn release_gated_offline_does_not_enable_dev_bypass() {
    #[cfg(not(feature = "dev-offline"))]
    const _: () = assert!(!patina_core::DEV_OFFLINE);
    let f = Fixture::new(true).await;
    assert_eq!(
        f.service.add_offline("LocalPlayer").await,
        Err(AuthError::OfflineNotAllowed)
    );
    f.chain(false, "").await;
    saved(&f.login().await);
    let offline = f.service.add_offline("LocalPlayer").await.unwrap();
    assert_eq!(offline.offline_origin(), Some(OfflineOrigin::Entitled));
    let restarted = f.restart();
    let grant = Fixture::resolve(&restarted, &offline.id).await.unwrap();
    let pending = f.pending(grant.account(), "/bin/true");
    // Harmless fixture executable: no game, output or credential arguments.
    let session = restarted
        .launch_pending(grant, pending, None)
        .await
        .unwrap();
    let exit = session.wait().await;
    assert_eq!(exit.code, Some(0));
    assert!(!exit.crashed);
    let grant = Fixture::resolve(&restarted, &offline.id).await.unwrap();
    let pending = f.pending(grant.account(), "/bin/true");
    f.service.remove(ID).await.unwrap();
    assert!(matches!(
        restarted.launch_pending(grant, pending, None).await,
        Err(AuthenticatedLaunchError::Auth(AuthError::OfflineNotAllowed))
    ));
    restarted.reload().await.unwrap();
    assert_eq!(
        Fixture::resolve(&restarted, &offline.id).await.unwrap_err(),
        AuthError::OfflineNotAllowed
    );
    assert!(
        restarted
            .snapshot()
            .await
            .unwrap()
            .accounts()
            .iter()
            .any(|a| a.id == offline.id)
    );
    assert_eq!(
        restarted.add_offline("Another").await,
        Err(AuthError::OfflineNotAllowed)
    );
}

#[cfg(feature = "dev-offline")]
#[tokio::test]
async fn developer_feature_coexists_without_promoting_production_offline() {
    let f = Fixture::new(false).await;
    let dev = f.service.add_dev_offline("Developer").await.unwrap();
    assert_eq!(dev.offline_origin(), Some(OfflineOrigin::Development));
    assert!(Fixture::resolve(&f.service, &dev.id).await.is_ok());
    assert_eq!(
        f.service.add_offline("Normal").await,
        Err(AuthError::OfflineNotAllowed)
    );
}

#[tokio::test]
async fn missing_credentials_and_unavailable_backend_are_distinct() {
    let f = Fixture::new(false).await;
    f.chain(false, "").await;
    saved(&f.login().await);
    f.keyring.set_error(Some(CredentialError::Unavailable));
    assert_eq!(
        Fixture::resolve(&f.restart(), ID).await.unwrap_err(),
        AuthError::StorageUnavailable
    );
    let missing = Fixture::service(
        &f.data,
        &f.server,
        f.clock.clone(),
        Arc::new(MemoryCredentials::default()),
    );
    assert_eq!(
        Fixture::resolve(&missing, ID).await.unwrap_err(),
        AuthError::CredentialMissing
    );
    assert!(missing.snapshot().await.unwrap().is_offline_allowed());
    f.assert_requests(0).await;
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
#[tokio::test]
async fn launch_identity_debug_and_metadata_are_secret_free() {
    let capture = Capture::default();
    // Global capture includes AccountService's owned spawned transaction tasks.
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .without_time()
            .with_writer(capture.clone())
            .finish(),
    )
    .unwrap();
    tracing::info!("auth contract capture active");
    let f = Fixture::new(true).await;
    f.chain(false, "").await;
    let events = f.login().await;
    saved(&events);
    f.server.verify().await;
    f.server.reset().await;
    f.clock.0.store(NOW + 3600, Ordering::SeqCst);
    f.chain(true, SECRETS[3]).await;
    let grant = Fixture::resolve(&f.service, ID).await.unwrap();
    f.assert_requests(1).await;
    secret_free(&format!("{grant:?}{:?}{events:?}", grant.account()));
    let pending = f.pending(grant.account(), "/nonexistent/patina-contract-executable");
    let error = f
        .service
        .launch_pending(grant, pending, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, AuthenticatedLaunchError::Session(_)));
    secret_free(&format!("{error:?}{error}"));
    secret_free(&std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap());
    // Storage error diagnostics must not serialize the record they failed to save.
    f.server.reset().await;
    f.chain(false, "").await;
    f.keyring.set_error(Some(CredentialError::Corrupt));
    let events = f.login().await;
    assert!(matches!(events.last(), Some(LoginEvent::Failed(_))));
    secret_free(&format!("{events:?}"));
    let trace = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(trace.contains("auth contract capture active"));
    secret_free(&trace);
}

#[tokio::test]
async fn invalid_entitlement_never_commits_the_protocol_success() {
    let f = Fixture::new(false).await;
    // Actual negative input to the real service, not a fabricated terminal event.
    f.chain_until(
        false,
        "",
        Some((
            5,
            ResponseTemplate::new(200).set_body_json(json!({"items":[]})),
        )),
    )
    .await;
    let events = f.login().await;
    assert!(matches!(
        events.last(),
        Some(LoginEvent::Failed(
            patina_core::accounts::login::LoginError::Auth(AuthError::NoEntitlement)
        ))
    ));
    assert!(f.service.snapshot().await.unwrap().accounts().is_empty());
    assert_eq!(
        f.service.add_offline("LocalPlayer").await,
        Err(AuthError::OfflineNotAllowed)
    );
    assert!(!f.data.root().join("accounts.toml").exists());
}

#[tokio::test]
async fn fallback_rotation_remains_authoritative_over_stale_keyring() {
    let f = Fixture::new(false).await;
    f.chain(false, "").await;
    let events = f.login().await;
    saved(&events);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LoginEvent::Storage(StorageKind::Keyring)))
    );
    f.assert_requests(0).await;
    f.server.reset().await;
    f.keyring
        .set_write_error(Some(CredentialError::Unavailable));
    f.clock.0.store(NOW + 3600, Ordering::SeqCst);
    f.chain(true, SECRETS[3]).await;
    Fixture::resolve(&f.service, ID).await.unwrap();
    f.assert_requests(1).await;
    f.server.reset().await;
    // The fake keyring still has the initial record; only the private file has
    // the rotated refresh token. The next real request must use that rotation.
    f.keyring.set_write_error(None);
    f.clock.0.store(NOW + 7200, Ordering::SeqCst);
    f.chain(true, SECRETS[4]).await;
    Fixture::resolve(&f.restart(), ID).await.unwrap();
    f.assert_requests(1).await;
}

fn skin_profile(hash: &str, name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": ID,
        "name": name,
        "skins": [{"state": "ACTIVE", "url": format!("https://textures.minecraft.net/texture/{hash}")}],
    }))
}

#[tokio::test]
async fn invalid_optional_skin_login_commits_done_and_survives_restart() {
    for length in [140_000, 65] {
        let f = Fixture::new(true).await;
        f.chain_until(
            false,
            "",
            Some((6, skin_profile(&"a".repeat(length), "Player_123"))),
        )
        .await;
        saved(&f.login().await);
        f.assert_requests(0).await;
        f.server.reset().await;

        for service in [&f.service, &f.restart()] {
            service.reload().await.unwrap();
            let snapshot = service.snapshot().await.unwrap();
            assert_eq!(snapshot.accounts().len(), 1);
            let account = snapshot.current().unwrap();
            assert_eq!(account.id, ID);
            assert_eq!(account.username, "Player_123");
            assert!(account.skin_hash().is_none());
            assert_eq!(account.verification().unwrap().profile_uuid(), account.uuid);
            assert!(snapshot.is_offline_allowed());
            assert_eq!(
                Fixture::resolve(service, ID)
                    .await
                    .unwrap()
                    .account()
                    .name(),
                "Player_123"
            );
        }
        let metadata = std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap();
        assert!(!metadata.contains("skin_hash"));
        secret_free(&metadata);
        assert!(f.server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn invalid_optional_skin_refresh_preserves_rotation_without_trapping_recovery() {
    let f = Fixture::new(true).await;
    f.chain_until(
        false,
        "",
        Some((6, skin_profile(&"b".repeat(64), "Player_123"))),
    )
    .await;
    saved(&f.login().await);
    let initial = f.service.snapshot().await.unwrap();
    let initial_account = initial.current().unwrap();
    assert_eq!(
        initial_account.skin_hash(),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    );
    f.assert_requests(0).await;
    f.server.reset().await;

    f.clock.0.store(NOW + 3600, Ordering::SeqCst);
    f.chain_until(
        true,
        SECRETS[3],
        Some((6, skin_profile(&"a".repeat(140_000), "UpdatedPlayer"))),
    )
    .await;
    let grant = Fixture::resolve(&f.service, ID).await.unwrap();
    assert_eq!(grant.account().name(), "UpdatedPlayer");
    f.assert_requests(1).await;
    f.server.reset().await;

    // Recovery and ordinary retry must not retain an unsavable metadata intent.
    // A separate service also proves the operation lock was released and the
    // verified metadata/session cache were durably committed, not just published.
    let restarted = f.restart();
    for service in [&f.service, &restarted] {
        service.reload().await.unwrap();
        let snapshot = service.snapshot().await.unwrap();
        let account = snapshot.current().unwrap();
        assert_eq!(account.id, ID);
        assert_eq!(account.uuid, initial_account.uuid);
        assert_eq!(account.token_ref, initial_account.token_ref);
        assert_eq!(account.username, "UpdatedPlayer");
        assert!(account.skin_hash().is_none());
        assert_eq!(account.verification().unwrap().verified_at(), NOW + 3600);
        assert!(snapshot.is_offline_allowed());
        assert_eq!(
            Fixture::resolve(service, ID)
                .await
                .unwrap()
                .account()
                .name(),
            "UpdatedPlayer"
        );
    }
    assert!(f.server.received_requests().await.unwrap().is_empty());
    let metadata = std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap();
    assert!(!metadata.contains("skin_hash"));
    secret_free(&metadata);

    // At the next expiry the real refresh request must use the durable rotation.
    f.clock.0.store(NOW + 7200, Ordering::SeqCst);
    f.chain(true, SECRETS[4]).await;
    Fixture::resolve(&restarted, ID).await.unwrap();
    f.assert_requests(1).await;
}
