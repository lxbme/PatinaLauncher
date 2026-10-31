//! Real second-process lock/reload proof; child uses only supplied temp data,
//! loopback endpoints and an unavailable fake keyring (never a native provider).
use super::*;

#[tokio::test]
async fn second_process_reloads_first_process_session_without_stale_refresh() {
    let f = Fixture::new().await;
    f.keyring.set_error(Some(CredentialError::Unavailable));
    let account = f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    Mock::given(path("/1")).respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(2)).set_body_json(json!({"access_token":"MS_SECRET_SENTINEL","refresh_token":"ROTATED_SENTINEL","expires_in":3600}))).with_priority(1).mount(&f.server).await;
    let service = f.service.clone();
    let (_tx, cancel) = watch::channel(false);
    let refresh = tokio::spawn(async move { service.resolve_launch(ID, cancel).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while f.refresh_count().await == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "accounts::service_tests::process_tests::service_process_helper",
            "--nocapture",
        ])
        .env("PATINA_TASK4_TEST_ROOT", f.data.root())
        .env("PATINA_TASK4_TEST_HTTP", f.server.uri())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !f.data.root().join("child-started").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !refresh.is_finished(),
        "child must overlap the first process's locked refresh"
    );
    assert_eq!(refresh.await.unwrap().unwrap().account().name(), "Renamed");
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(
        std::fs::read_to_string(f.data.root().join("child-result")).unwrap(),
        "Renamed"
    );
    assert_eq!(f.refresh_count().await, 1);
    let bytes = std::fs::read(
        f.data
            .root()
            .join("credentials")
            .join(format!("{}.json", account.token_ref.unwrap())),
    )
    .unwrap();
    let record = super::super::credentials::CredentialRecord::decode(&bytes).unwrap();
    assert_eq!(record.refresh_token().expose(), "ROTATED_SENTINEL");
}

#[tokio::test]
#[ignore = "isolated child invoked only by second_process_reloads_first_process_session_without_stale_refresh"]
async fn service_process_helper() {
    let root = std::env::var("PATINA_TASK4_TEST_ROOT").unwrap();
    let base = std::env::var("PATINA_TASK4_TEST_HTTP").unwrap();
    let data = DataDirs::new(root);
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
    let clock = Arc::new(Clock(AtomicU64::new(NOW)));
    let keyring = Arc::new(MemoryCredentials::default());
    keyring.set_error(Some(CredentialError::Unavailable));
    let service = AccountService::for_test(
        data.clone(),
        MsaClient::for_test(http, clock.clone()),
        credential_store(data.clone(), keyring),
        clock,
    );
    std::fs::write(data.root().join("child-started"), b"ready").unwrap();
    let (_tx, cancel) = watch::channel(false);
    let grant = service.resolve_launch(ID, cancel).await.unwrap();
    std::fs::write(data.root().join("child-result"), grant.account().name()).unwrap();
}
