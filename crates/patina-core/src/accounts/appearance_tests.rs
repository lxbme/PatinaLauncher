//! Real account owner and request transport, fake credentials, temporary data, loopback only.
use super::service_tests::NOW;
use super::{
    appearance::*,
    service_tests::{Fixture, ID},
    testing::seed_verified,
};
use serde_json::json;
use tokio::sync::watch;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

async fn fixture() -> Fixture {
    let mut f = Fixture::new().await;
    f.service
        .set_skin_endpoint_for_test(&f.server.uri())
        .unwrap();
    seed_verified(
        &f.service,
        "Player_123",
        ID.parse().unwrap(),
        "FAKE_REFRESH",
        NOW,
        NOW + 3600,
    )
    .await
    .unwrap();
    f
}
fn profile() -> serde_json::Value {
    json!({"id":ID,"name":"Player_123","skins":[{"id":"skin-id","state":"ACTIVE","url":"https://textures.minecraft.net/texture/abcdef","variant":"CLASSIC"}],"capes":[]})
}
#[tokio::test]
async fn appearance_read_and_texture_are_isolated_from_launch_selection_and_credentials() {
    let f = fixture().await;
    let original = f.service.snapshot().await.unwrap();
    Mock::given(method("GET"))
        .and(path("/6"))
        .and(|r: &wiremock::Request| r.headers.contains_key("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile()))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/texture/abcdef"))
        .and(|r: &wiremock::Request| {
            !r.headers.contains_key("authorization") && !r.headers.contains_key("cookie")
        })
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"texture fixture".to_vec()))
        .expect(1)
        .mount(&f.server)
        .await;
    let (_tx, cancel) = watch::channel(false);
    let p = f.service.appearance(ID, cancel.clone()).await.unwrap();
    let bytes = f
        .service
        .appearance_texture(ID, p.skin().unwrap().texture(), cancel)
        .await
        .unwrap();
    assert_eq!(bytes, b"texture fixture");
    assert_eq!(f.service.snapshot().await.unwrap(), original);
    assert!(!format!("{p:?}").contains("MC_SECRET"));
}
#[tokio::test]
async fn appearance_read_failure_is_not_an_empty_cape_list() {
    for status in [401, 403, 429, 500] {
        let f = fixture().await;
        Mock::given(path("/6"))
            .respond_with(ResponseTemplate::new(status).set_body_string("REMOTE_SECRET_SENTINEL"))
            .expect(1)
            .mount(&f.server)
            .await;
        let (_tx, cancel) = watch::channel(false);
        let error = f.service.appearance(ID, cancel).await.unwrap_err();
        assert!(!format!("{error:?} {error}").contains("REMOTE_SECRET_SENTINEL"));
        assert!(matches!(error, AppearanceError::Auth(_)));
        assert_eq!(
            f.server.received_requests().await.unwrap().len(),
            1,
            "no auth replay"
        );
    }
}
#[tokio::test]
async fn appearance_deleted_or_relogged_identity_rejects_late_profile_and_texture() {
    let f = fixture().await;
    Mock::given(path("/6"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(profile())
                .set_delay(std::time::Duration::from_millis(150)),
        )
        .mount(&f.server)
        .await;
    let (_tx, cancel) = watch::channel(false);
    let service = f.service.clone();
    let c = cancel.clone();
    let load = tokio::spawn(async move { service.appearance(ID, c).await });
    while f.server.received_requests().await.unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    f.service.remove(ID).await.unwrap();
    assert!(load.await.unwrap().is_err());
    assert!(f.service.snapshot().await.unwrap().accounts().is_empty());
}
#[tokio::test]
async fn appearance_unavailable_credentials_and_cancellation_never_send() {
    let f = Fixture::new().await;
    let (_tx, cancel) = watch::channel(false);
    assert!(f.service.appearance(ID, cancel.clone()).await.is_err());
    let f = fixture().await;
    let (tx, cancel) = watch::channel(false);
    tx.send(true).unwrap();
    assert!(f.service.appearance(ID, cancel).await.is_err());
    assert!(f.server.received_requests().await.unwrap().is_empty());
}
