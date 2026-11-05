use crate::accounts::{
    appearance::*,
    service_tests::{Fixture, ID, NOW},
    testing::seed_verified,
};
use image::{DynamicImage, ImageFormat, RgbaImage};
use serde_json::json;
use std::io::Cursor;
use tokio::sync::watch;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};
fn profile() -> serde_json::Value {
    json!({"id":ID,"skins":[{"state":"ACTIVE","url":"https://textures.minecraft.net/texture/abcdef","variant":"CLASSIC"}],"capes":[{"id":"cape-one","state":"INACTIVE","url":"https://textures.minecraft.net/texture/cdefab","alias":"One"}]})
}
fn skin() -> SkinUpload {
    let mut out = Cursor::new(vec![]);
    DynamicImage::ImageRgba8(RgbaImage::new(64, 64))
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    SkinUpload::from_png(out.into_inner(), SkinModel::Slim).unwrap()
}
async fn fixture() -> (Fixture, AppearanceProfile) {
    let f = Fixture::new().await;
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
        .and(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile()))
        .mount(&f.server)
        .await;
    let (_tx, c) = watch::channel(false);
    let p = f.service.appearance(ID, c).await.unwrap();
    (f, p)
}
#[tokio::test]
async fn appearance_apply_changed_only_multipart_skin_then_owned_cape() {
    let (f, p) = fixture().await;
    let upload = skin();
    let png = upload.bytes().to_vec();
    Mock::given(path("/6/skins"))
        .and(method("POST"))
        .and(move |r: &wiremock::Request| {
            let body = &r.body;
            let text = String::from_utf8_lossy(body);
            r.headers.get("content-type").is_some_and(|v| {
                v.to_str()
                    .unwrap()
                    .starts_with("multipart/form-data; boundary=")
            }) && text.contains("name=\"variant\"")
                && text.contains("slim")
                && text.contains("filename=\"skin.png\"")
                && text.contains("image/png")
                && body.windows(png.len()).any(|w| w == png)
        })
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(path("/6/capes/active"))
        .and(method("PUT"))
        .and(|r: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()
                == json!({"capeId":"cape-one"})
        })
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&f.server)
        .await;
    let changes = AppearanceChanges::new(&p)
        .with_skin(upload)
        .with_cape(CapeChange::Select(p.capes()[0].id().clone()));
    let (_tx, c) = watch::channel(false);
    let out = f.service.apply_appearance(ID, changes, c).await.unwrap();
    assert_eq!(out.skin(), &FieldOutcome::Confirmed);
    assert_eq!(out.cape(), &FieldOutcome::Confirmed);
    let requests = f.server.received_requests().await.unwrap();
    let writes: Vec<_> = requests
        .iter()
        .filter(|r| r.method != "GET")
        .map(|r| r.url.path())
        .collect();
    assert_eq!(writes, ["/6/skins", "/6/capes/active"]);
}
#[tokio::test]
async fn appearance_skin_failure_stops_cape_and_partial_success_is_retained() {
    for (skin_status, cape_count) in [(403, 0), (200, 1)] {
        let (f, p) = fixture().await;
        Mock::given(path("/6/skins"))
            .and(method("POST"))
            .respond_with(ResponseTemplate::new(skin_status).set_body_string("REMOTE_SECRET"))
            .expect(1)
            .mount(&f.server)
            .await;
        Mock::given(path("/6/capes/active"))
            .respond_with(ResponseTemplate::new(429))
            .expect(cape_count)
            .mount(&f.server)
            .await;
        let changes = AppearanceChanges::new(&p)
            .with_skin(skin())
            .with_cape(CapeChange::Select(p.capes()[0].id().clone()));
        let (_tx, c) = watch::channel(false);
        let out = f.service.apply_appearance(ID, changes, c).await.unwrap();
        if skin_status == 200 {
            assert_eq!(out.skin(), &FieldOutcome::Confirmed);
            assert!(matches!(out.cape(), FieldOutcome::Failed(_)));
        } else {
            assert!(matches!(out.skin(), FieldOutcome::Failed(_)));
            assert_eq!(out.cape(), &FieldOutcome::NotRequested);
        }
        assert!(!format!("{out:?}").contains("REMOTE_SECRET"));
    }
}
#[tokio::test]
async fn appearance_unknown_reencoded_skin_reconciliation_never_retries_write() {
    let (f, p) = fixture().await;
    // A malformed/5xx acknowledgement may follow a remotely accepted write.
    Mock::given(path("/6/skins"))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(path("/6/capes/active"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&f.server)
        .await;
    let changes = AppearanceChanges::new(&p)
        .with_skin(skin())
        .with_cape(CapeChange::Select(p.capes()[0].id().clone()));
    let (_tx, c) = watch::channel(false);
    let out = f
        .service
        .apply_appearance(ID, changes, c.clone())
        .await
        .unwrap();
    assert!(matches!(out.skin(), FieldOutcome::Uncertain(_)));
    assert_eq!(out.cape(), &FieldOutcome::NotRequested);
    let reconciled = f.service.reconcile_appearance(&out, c).await.unwrap();
    assert!(matches!(reconciled.skin(), FieldOutcome::Uncertain(_)));
}
#[tokio::test]
async fn appearance_apply_stale_identity_is_rejected_before_any_write() {
    let (f, p) = fixture().await;
    f.service.remove(ID).await.unwrap();
    let (_tx, c) = watch::channel(false);
    assert!(
        f.service
            .apply_appearance(ID, AppearanceChanges::new(&p).with_skin(skin()), c)
            .await
            .is_err()
    );
    assert!(
        f.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method == "GET")
    );
}

#[tokio::test]
async fn appearance_cape_only_disable_and_unchanged_do_not_upload_skin() {
    for disable in [false, true] {
        let (f, _) = fixture().await;
        f.server.reset().await;
        let mut value = profile();
        value["capes"][0]["state"] = json!(if disable { "ACTIVE" } else { "INACTIVE" });
        Mock::given(path("/6"))
            .respond_with(ResponseTemplate::new(200).set_body_json(value))
            .mount(&f.server)
            .await;
        let (_tx, c) = watch::channel(false);
        let p = f.service.appearance(ID, c.clone()).await.unwrap();
        Mock::given(path("/6/capes/active"))
            .and(method(if disable { "DELETE" } else { "PUT" }))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&f.server)
            .await;
        let change = if disable {
            CapeChange::Disable
        } else {
            CapeChange::Select(p.capes()[0].id().clone())
        };
        let out = f
            .service
            .apply_appearance(ID, AppearanceChanges::new(&p).with_cape(change), c.clone())
            .await
            .unwrap();
        assert_eq!(out.skin(), &FieldOutcome::NotRequested);
        assert_eq!(out.cape(), &FieldOutcome::Confirmed);
        let clean = f
            .service
            .apply_appearance(ID, AppearanceChanges::new(&p), c)
            .await
            .unwrap();
        assert_eq!(clean.skin(), &FieldOutcome::NotRequested);
        assert_eq!(clean.cape(), &FieldOutcome::NotRequested);
        assert!(
            !f.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path().ends_with("/skins"))
        );
    }
}
#[tokio::test]
async fn appearance_fresh_cape_ownership_is_required_before_writing() {
    let (f, p) = fixture().await;
    f.server.reset().await;
    let mut fresh = profile();
    fresh["capes"] = json!([]);
    Mock::given(path("/6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fresh))
        .mount(&f.server)
        .await;
    let (_tx, c) = watch::channel(false);
    let changes =
        AppearanceChanges::new(&p).with_cape(CapeChange::Select(p.capes()[0].id().clone()));
    assert!(f.service.apply_appearance(ID, changes, c).await.is_err());
    assert!(
        f.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method == "GET")
    );
}
#[tokio::test]
async fn appearance_serializes_writes_and_reconcile_uses_updated_metadata_identity() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let (f, p) = fixture().await;
    f.server.reset().await;
    let uploaded = Arc::new(AtomicBool::new(false));
    let state = uploaded.clone();
    Mock::given(path("/6"))
        .respond_with(move |_: &wiremock::Request| {
            let mut v = profile();
            if state.load(Ordering::SeqCst) {
                v["skins"][0]["url"] = json!("https://textures.minecraft.net/texture/123abc");
                v["skins"][0]["variant"] = json!("SLIM");
            }
            ResponseTemplate::new(200).set_body_json(v)
        })
        .mount(&f.server)
        .await;
    let state = uploaded.clone();
    Mock::given(path("/6/skins"))
        .respond_with(move |_: &wiremock::Request| {
            state.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(150))
        })
        .expect(1)
        .mount(&f.server)
        .await;
    let (_tx, c) = watch::channel(false);
    let service = f.service.clone();
    let changes = AppearanceChanges::new(&p).with_skin(skin());
    let cancel = c.clone();
    let operation =
        tokio::spawn(async move { service.apply_appearance(ID, changes, cancel).await });
    while !uploaded.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    let duplicate = f
        .service
        .apply_appearance(ID, AppearanceChanges::new(&p).with_skin(skin()), c.clone())
        .await;
    assert!(matches!(
        duplicate,
        Err(AppearanceError::Auth(
            crate::accounts::error::AuthError::Busy
        ))
    ));
    let out = operation.await.unwrap().unwrap();
    assert_eq!(out.skin(), &FieldOutcome::Confirmed);
    assert_eq!(
        f.service.snapshot().await.unwrap().accounts()[0].skin_hash(),
        Some("123abc")
    );
    let reread = f.service.reconcile_appearance(&out, c).await.unwrap();
    assert!(
        reread.profile().is_some(),
        "own acknowledged metadata must not invalidate later reconciliation"
    );
}
#[tokio::test]
async fn appearance_skin_ack_readback_failure_stays_confirmed_not_retryable() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let (f, p) = fixture().await;
    f.server.reset().await;
    let reads = Arc::new(AtomicUsize::new(0));
    Mock::given(path("/6"))
        .respond_with(move |_: &wiremock::Request| {
            if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200).set_body_json(profile())
            } else {
                ResponseTemplate::new(500)
            }
        })
        .mount(&f.server)
        .await;
    Mock::given(path("/6/skins"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&f.server)
        .await;
    let (_tx, c) = watch::channel(false);
    let out = f
        .service
        .apply_appearance(ID, AppearanceChanges::new(&p).with_skin(skin()), c)
        .await
        .unwrap();
    assert_eq!(out.skin(), &FieldOutcome::Confirmed);
    assert!(out.profile().is_none());
    assert!(out.refresh_error().is_some());
}

#[tokio::test]
async fn appearance_timeout_after_remote_reencoding_stays_unknown_and_stops_cape() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let (f, p) = fixture().await;
    f.server.reset().await;
    let accepted = Arc::new(AtomicBool::new(false));
    let state = accepted.clone();
    Mock::given(path("/6"))
        .respond_with(move |_: &wiremock::Request| {
            let mut value = profile();
            if state.load(Ordering::SeqCst) {
                value["skins"][0]["url"] = json!("https://textures.minecraft.net/texture/123abc");
                value["skins"][0]["variant"] = json!("SLIM");
            }
            ResponseTemplate::new(200).set_body_json(value)
        })
        .mount(&f.server)
        .await;
    Mock::given(path("/6/skins"))
        .respond_with(move |_: &wiremock::Request| {
            accepted.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(16))
        })
        .expect(1)
        .mount(&f.server)
        .await;
    let changes = AppearanceChanges::new(&p)
        .with_skin(skin())
        .with_cape(CapeChange::Select(p.capes()[0].id().clone()));
    let (_tx, c) = watch::channel(false);
    let out = f
        .service
        .apply_appearance(ID, changes, c.clone())
        .await
        .unwrap();
    assert!(matches!(out.skin(), FieldOutcome::Uncertain(_)));
    assert_eq!(out.cape(), &FieldOutcome::NotRequested);
    assert_eq!(
        out.profile().unwrap().skin().unwrap().texture().hash(),
        "123abc"
    );
    let again = f.service.reconcile_appearance(&out, c).await.unwrap();
    assert!(again.is_uncertain());
    let writes = f
        .server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method != "GET")
        .count();
    assert_eq!(writes, 1);
}
#[tokio::test]
async fn appearance_dropped_waiter_retains_ack_but_does_not_send_next_cape() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let (f, p) = fixture().await;
    f.server.reset().await;
    let accepted = Arc::new(AtomicBool::new(false));
    let state = accepted.clone();
    Mock::given(path("/6"))
        .respond_with(move |_: &wiremock::Request| {
            let mut value = profile();
            if state.load(Ordering::SeqCst) {
                value["skins"][0]["url"] = json!("https://textures.minecraft.net/texture/123abc");
            }
            ResponseTemplate::new(200).set_body_json(value)
        })
        .mount(&f.server)
        .await;
    let state = accepted.clone();
    Mock::given(path("/6/skins"))
        .respond_with(move |_: &wiremock::Request| {
            state.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(150))
        })
        .expect(1)
        .mount(&f.server)
        .await;
    let changes = AppearanceChanges::new(&p)
        .with_skin(skin())
        .with_cape(CapeChange::Select(p.capes()[0].id().clone()));
    let (_tx, c) = watch::channel(false);
    let service = f.service.clone();
    let worker = tokio::spawn(async move { service.apply_appearance(ID, changes, c).await });
    while !accepted.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    worker.abort();
    let _ = worker.await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if f.service.snapshot().await.unwrap().accounts()[0].skin_hash() == Some("123abc") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !f.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path() == "/6/capes/active")
    );
}

#[tokio::test]
async fn appearance_launch_selection_change_preserves_target_and_draft_apply() {
    let (f, p) = fixture().await;
    let other = seed_verified(
        &f.service,
        "Other",
        "1123456789abcdef0123456789abcdef".parse().unwrap(),
        "FAKE_OTHER",
        NOW,
        NOW + 3600,
    )
    .await
    .unwrap();
    f.service.set_current(&other.id).await.unwrap();
    Mock::given(path("/6/skins"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&f.server)
        .await;
    let (_tx, c) = watch::channel(false);
    let out = f
        .service
        .apply_appearance(ID, AppearanceChanges::new(&p).with_skin(skin()), c)
        .await
        .unwrap();
    assert_eq!(out.skin(), &FieldOutcome::Confirmed);
    assert_eq!(
        f.service.snapshot().await.unwrap().current().unwrap().id,
        other.id
    );
}

#[tokio::test]
async fn appearance_skin_http_rejections_and_server_failure_never_replay_or_continue_cape() {
    for status in [401, 403, 429, 500] {
        let (f, p) = fixture().await;
        Mock::given(path("/6/skins"))
            .and(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string("REMOTE_SECRET_SENTINEL"))
            .expect(1)
            .mount(&f.server)
            .await;
        Mock::given(path("/6/capes/active"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&f.server)
            .await;
        let (_tx, c) = watch::channel(false);
        let changes = AppearanceChanges::new(&p)
            .with_skin(skin())
            .with_cape(CapeChange::Select(p.capes()[0].id().clone()));
        let out = f.service.apply_appearance(ID, changes, c).await.unwrap();
        assert_eq!(
            matches!(out.skin(), FieldOutcome::Uncertain(_)),
            status == 500
        );
        assert_eq!(matches!(out.skin(), FieldOutcome::Failed(_)), status != 500);
        assert_eq!(out.cape(), &FieldOutcome::NotRequested);
        assert!(!format!("{out:?}").contains("REMOTE_SECRET_SENTINEL"));
        assert!(
            !f.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path() == "/1"),
            "no automatic refresh/replay after write rejection"
        );
    }
}
#[tokio::test]
async fn appearance_ambiguous_cape_ack_reconciles_owned_id_with_one_read_not_replay() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let (f, p) = fixture().await;
    f.server.reset().await;
    let applied = Arc::new(AtomicBool::new(false));
    let state = applied.clone();
    Mock::given(path("/6"))
        .respond_with(move |_: &wiremock::Request| {
            let mut value = profile();
            if state.load(Ordering::SeqCst) {
                value["capes"][0]["state"] = json!("ACTIVE");
            }
            ResponseTemplate::new(200).set_body_json(value)
        })
        .mount(&f.server)
        .await;
    Mock::given(path("/6/capes/active"))
        .and(method("PUT"))
        .respond_with(move |_: &wiremock::Request| {
            applied.store(true, Ordering::SeqCst);
            ResponseTemplate::new(502)
        })
        .expect(1)
        .mount(&f.server)
        .await;
    let (_tx, c) = watch::channel(false);
    let changes =
        AppearanceChanges::new(&p).with_cape(CapeChange::Select(p.capes()[0].id().clone()));
    let out = f
        .service
        .apply_appearance(ID, changes, c.clone())
        .await
        .unwrap();
    assert_eq!(out.skin(), &FieldOutcome::NotRequested);
    assert_eq!(out.cape(), &FieldOutcome::Confirmed);
    assert_eq!(
        out.profile().unwrap().active_cape().unwrap().id(),
        p.capes()[0].id()
    );
    let out = f.service.reconcile_appearance(&out, c).await.unwrap();
    assert_eq!(out.cape(), &FieldOutcome::Confirmed);
}
