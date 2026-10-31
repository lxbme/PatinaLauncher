//! Service tests exercise real transactions against temporary data and loopback HTTP.
use super::credentials::{CredentialBackend, CredentialError};
use super::service::RemovalOutcome;
use super::{
    AccountError, AccountService, AccountStore,
    error::{AuthError, AuthStage},
    login::{DeviceCodeLogin, LoginEvent},
    testing::{AuthClock, AuthHttp, MemoryCredentials, MsaClient, credential_store},
};
use crate::paths::DataDirs;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const NOW: u64 = 1_800_000_000;
const ID: &str = "0123456789abcdef0123456789abcdef";
#[derive(Default)]
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
    keyring: Arc<MemoryCredentials>,
    clock: Arc<Clock>,
    service: AccountService,
}
impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(temp.path());
        let server = MockServer::start().await;
        let keyring = Arc::new(MemoryCredentials::default());
        let clock = Arc::new(Clock(AtomicU64::new(NOW)));
        let service = Self::service(&data, &server, keyring.clone(), clock.clone());
        Self {
            _temp: temp,
            data,
            server,
            keyring,
            clock,
            service,
        }
    }
    fn service(
        data: &DataDirs,
        server: &MockServer,
        keyring: Arc<MemoryCredentials>,
        clock: Arc<Clock>,
    ) -> AccountService {
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
        AccountService::for_test(
            data.clone(),
            MsaClient::for_test(http, clock.clone()),
            credential_store(data.clone(), keyring),
            clock,
        )
    }
    fn restarted(&self) -> AccountService {
        Self::service(
            &self.data,
            &self.server,
            self.keyring.clone(),
            self.clock.clone(),
        )
    }
    async fn response(&self, stage: usize, status: u16, body: serde_json::Value) {
        Mock::given(path(format!("/{stage}")))
            .and(method(if stage >= 5 { "GET" } else { "POST" }))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&self.server)
            .await;
    }
    async fn chain(&self, name: &str) {
        self.response(0,200,json!({"device_code":"DEVICE_SECRET_SENTINEL", "user_code":"ABCD-EFGH", "verification_uri":"https://microsoft.com/link", "expires_in":120, "interval":1})).await;
        self.response(1,200,json!({"access_token":"MS_SECRET_SENTINEL", "refresh_token":"ROTATED_SENTINEL", "expires_in":3600})).await;
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
        self.response(6,200,json!({"id":ID,"name":name,"skins":[{"state":"ACTIVE","url":"https://textures.minecraft.net/texture/abcdef"}]})).await;
    }
    async fn seed(&self, expires: u64) -> super::Account {
        super::testing::seed_verified(
            &self.service,
            "Original",
            Uuid::parse_str(ID).unwrap(),
            "OLD_SECRET_SENTINEL",
            NOW,
            expires,
        )
        .await
        .unwrap()
    }
    async fn login(&self) -> Vec<LoginEvent> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_cancel, token) = watch::channel(false);
        self.service.start(tx, token).await;
        let mut events = vec![];
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        events
    }
    async fn resolve(&self) -> Result<super::LaunchGrant, AuthError> {
        let (_cancel, token) = watch::channel(false);
        self.service.resolve_launch(ID, token).await
    }
    async fn refresh_count(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/1")
            .count()
    }
}

#[tokio::test]
async fn done_means_durable_current_account() {
    let f = Fixture::new().await;
    f.chain("Player_123").await;
    let events = f.login().await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, LoginEvent::Done { .. }))
            .count(),
        1,
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, LoginEvent::Failed(_)))
            .count(),
        0
    );
    let snapshot = f.restarted().snapshot().await.unwrap();
    let account = snapshot.current().unwrap();
    assert_eq!(account.uuid, Uuid::parse_str(ID).unwrap());
    assert_eq!(account.username, "Player_123");
    assert!(account.verification().is_some());
    assert_eq!(account.skin_hash(), Some("abcdef"));
    let metadata = std::fs::read_to_string(f.data.root().join("accounts.toml")).unwrap();
    assert!(!metadata.contains("SECRET_SENTINEL"));
    assert!(!metadata.contains("ROTATED_SENTINEL"));
    assert!(matches!(events.last(), Some(LoginEvent::Done { .. })));
}

#[tokio::test]
async fn same_uuid_relogin_replaces_reference() {
    let f = Fixture::new().await;
    let old = f.seed(NOW + 3600).await;
    f.chain("Renamed").await;
    assert!(matches!(
        f.login().await.last(),
        Some(LoginEvent::Done { .. })
    ));
    let snapshot = f.service.snapshot().await.unwrap();
    assert_eq!(snapshot.accounts().len(), 1);
    let new = snapshot.current().unwrap();
    assert_eq!(new.id, old.id);
    assert_ne!(new.token_ref, old.token_ref);
    assert_eq!(new.username, "Renamed");
    assert!(
        f.keyring
            .read(old.token_ref.as_ref().unwrap())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn concurrent_launches_share_refresh() {
    let f = Fixture::new().await;
    f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    let (a, b) = tokio::join!(f.resolve(), f.resolve());
    assert_eq!(a.unwrap().account().name(), "Renamed");
    assert_eq!(b.unwrap().account().name(), "Renamed");
    assert_eq!(f.refresh_count().await, 1);
}

#[tokio::test]
async fn rotation_persists_before_downstream_failure() {
    let f = Fixture::new().await;
    let account = f.seed(NOW + 60).await;
    f.chain("Player_123").await;
    Mock::given(path("/3"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&f.server)
        .await;
    assert!(matches!(
        f.resolve().await,
        Err(AuthError::Http {
            stage: AuthStage::Xsts,
            status: 500
        })
    ));
    let record = f
        .keyring
        .read(account.token_ref.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(record.refresh_token().expose(), "ROTATED_SENTINEL");
    assert!(
        f.restarted().snapshot().await.unwrap().accounts()[0]
            .verification()
            .is_some()
    );
}

#[tokio::test]
async fn transaction_failure_recovery_matrix() {
    let f = Fixture::new().await;
    f.keyring.set_write_error(Some(CredentialError::Io));
    let epoch = *f.service.changes().borrow();
    assert!(
        super::testing::seed_verified(
            &f.service,
            "Original",
            Uuid::parse_str(ID).unwrap(),
            "SECRET_SENTINEL",
            NOW,
            NOW + 3600
        )
        .await
        .is_err()
    );
    assert_eq!(*f.service.changes().borrow(), epoch);
    assert!(
        f.restarted()
            .snapshot()
            .await
            .unwrap()
            .accounts()
            .is_empty()
    );
}

#[tokio::test]
async fn logout_cannot_be_undone_by_refresh() {
    let f = Fixture::new().await;
    let a = f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    Mock::given(path("/1")).respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(100)).set_body_json(json!({"access_token":"MS_SECRET_SENTINEL","refresh_token":"ROTATED_SENTINEL","expires_in":3600}))).with_priority(1).mount(&f.server).await;
    let service = f.service.clone();
    let (_tx, cancel) = watch::channel(false);
    let refresh = tokio::spawn(async move { service.resolve_launch(ID, cancel).await });
    while f.refresh_count().await == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        f.service.remove(&a.id).await.unwrap(),
        RemovalOutcome::Removed
    );
    let _ = refresh.await.unwrap();
    assert!(matches!(
        f.resolve().await,
        Err(AuthError::Account(AccountError::NotFound(_)))
    ));
    assert!(
        f.restarted()
            .snapshot()
            .await
            .unwrap()
            .accounts()
            .is_empty()
    );
}

#[tokio::test]
async fn transient_auth_errors_preserve_credentials() {
    let f = Fixture::new().await;
    let account = f.seed(NOW + 60).await;
    f.response(1, 503, json!({"error_description":"SECRET_SENTINEL"}))
        .await;
    let error = f.resolve().await.unwrap_err();
    assert_eq!(
        error,
        AuthError::Http {
            stage: AuthStage::MicrosoftToken,
            status: 503
        }
    );
    assert!(!format!("{error:?} {error}").contains("SECRET_SENTINEL"));
    let record = f
        .keyring
        .read(account.token_ref.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(record.refresh_token().expose(), "OLD_SECRET_SENTINEL");
    assert!(!record.reauth_required());
    assert!(
        AccountStore::load(&f.data).unwrap().accounts()[0]
            .verification()
            .is_some()
    );
}

#[path = "service_concurrency_tests.rs"]
mod concurrency_tests;
#[path = "service_failure_tests.rs"]
mod failure_tests;
#[path = "service_launch_tests.rs"]
mod launch_tests;
#[path = "service_process_tests.rs"]
mod process_tests;
