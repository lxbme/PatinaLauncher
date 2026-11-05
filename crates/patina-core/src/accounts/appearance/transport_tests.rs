use crate::accounts::{
    service_tests::{Fixture, ID, NOW},
    testing::{CredentialError, seed_verified},
};
use serde_json::json;
use tokio::sync::watch;
use wiremock::{Mock, ResponseTemplate, matchers::path};
fn profile() -> serde_json::Value {
    json!({"id":ID,"skins":[{"state":"ACTIVE","url":"https://textures.minecraft.net/texture/abcdef","variant":"CLASSIC"}],"capes":[]})
}
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
    Mock::given(path("/6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile()))
        .mount(&f.server)
        .await;
    f
}
#[tokio::test]
async fn appearance_texture_rejects_wrong_id_relogin_and_late_removed_download() {
    let f = fixture().await;
    let (_tx, c) = watch::channel(false);
    let p = f.service.appearance(ID, c.clone()).await.unwrap();
    assert!(
        f.service
            .appearance_texture("other", p.skin().unwrap().texture(), c.clone())
            .await
            .is_err()
    );
    Mock::given(path("/texture/abcdef"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("pixels")
                .set_delay(std::time::Duration::from_millis(150)),
        )
        .mount(&f.server)
        .await;
    let s = f.service.clone();
    let texture = p.skin().unwrap().texture().clone();
    let cancel = c.clone();
    let download = tokio::spawn(async move { s.appearance_texture(ID, &texture, cancel).await });
    while !f
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|r| r.url.path() == "/texture/abcdef")
    {
        tokio::task::yield_now().await;
    }
    seed_verified(
        &f.service,
        "Player_123",
        ID.parse().unwrap(),
        "NEW_FAKE_REFRESH",
        NOW,
        NOW + 3600,
    )
    .await
    .unwrap();
    assert!(download.await.unwrap().is_err());
    assert!(
        f.service
            .appearance_texture(ID, p.skin().unwrap().texture(), c)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn appearance_offline_and_unavailable_credential_store_do_not_fetch_profile() {
    let f = fixture().await;
    let offline = f.service.add_offline("Local_123").await.unwrap();
    let (_tx, c) = watch::channel(false);
    assert!(f.service.appearance(&offline.id, c.clone()).await.is_err());
    f.keyring.set_error(Some(CredentialError::Unavailable));
    assert!(f.service.appearance(ID, c).await.is_err());
    assert!(f.server.received_requests().await.unwrap().is_empty());
}
