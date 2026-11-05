use super::*;
use image::{DynamicImage, ImageFormat, RgbaImage};
use patina_core::accounts::{
    AccountService,
    error::AuthStage,
    testing::{AuthClock, AuthHttp, MemoryCredentials, MsaClient, credential_store, seed_verified},
};
use patina_core::paths::DataDirs;
use std::io::Cursor;
use std::sync::Arc;
use tokio::sync::watch;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
const ID: &str = "0123456789abcdef0123456789abcdef";
struct Clock;
impl AuthClock for Clock {
    fn unix_seconds(&self) -> u64 {
        1_800_000_000
    }
}
async fn fixture() -> (
    tempfile::TempDir,
    MockServer,
    AccountService,
    AccountSnapshot,
    AppearanceProfile,
) {
    let temp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(temp.path());
    let server = MockServer::start().await;
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
    let service = AccountService::for_test(
        data.clone(),
        MsaClient::for_test(http, Arc::new(Clock)),
        credential_store(data, Arc::new(MemoryCredentials::default())),
        Arc::new(Clock),
    );
    seed_verified(
        &service,
        "Steve",
        ID.parse().unwrap(),
        "FAKE",
        1_800_000_000,
        1_800_003_600,
    )
    .await
    .unwrap();
    Mock::given(path("/6"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id":ID,"skins":[{"state":"ACTIVE","url":"https://textures.minecraft.net/texture/abcdef","variant":"CLASSIC"}],"capes":[{"id":"cape-one","state":"INACTIVE","url":"https://textures.minecraft.net/texture/abcdef","alias":"One"}]})),
        )
        .mount(&server)
        .await;
    let (_tx, c) = watch::channel(false);
    let p = service.appearance(ID, c).await.unwrap();
    let snapshot = service.snapshot().await.unwrap();
    (temp, server, service, snapshot, p)
}
fn skin(height: u32) -> SkinUpload {
    let mut out = Cursor::new(vec![]);
    DynamicImage::ImageRgba8(RgbaImage::new(64, height))
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    SkinUpload::from_png(out.into_inner(), SkinModel::Classic).unwrap()
}
#[tokio::test]
async fn appearance_model_selects_msa_without_changing_launch_current() {
    let (_temp, _server, service, snapshot, p) = fixture().await;
    let original = snapshot.current().unwrap().id.clone();
    let mut model = AppearanceModel::default();
    model.set_accounts(snapshot);
    assert_eq!(model.selected_id(), Some(ID));
    let ticket = model.ticket().unwrap();
    assert!(model.accept_profile(&ticket, Ok(p)));
    assert_eq!(model.leave_policy(), LeavePolicy::Allow);
    model.set_side(PreviewSide::Back);
    model.set_outer(false);
    assert!(!model.has_unapplied_changes());
    let second = seed_verified(
        &service,
        "Alex",
        "1123456789abcdef0123456789abcdef".parse().unwrap(),
        "FAKE2",
        1_800_000_000,
        1_800_003_600,
    )
    .await
    .unwrap();
    service.set_current(&original).await.unwrap();
    model.set_accounts(service.snapshot().await.unwrap());
    assert!(model.select_account(&second.id));
    assert_eq!(
        service.snapshot().await.unwrap().current().unwrap().id,
        original
    );
    assert!(!model.accept_profile(&ticket, Err("late".into())));
}
#[tokio::test]
async fn appearance_model_validates_immutable_skin_and_preview_controls_stay_clean() {
    let (_temp, _server, _service, snapshot, p) = fixture().await;
    let mut model = AppearanceModel::default();
    model.set_accounts(snapshot);
    let ticket = model.ticket().unwrap();
    model.accept_profile(&ticket, Ok(p));
    assert!(
        model.set_model(SkinModel::Slim).is_err(),
        "no placeholder/model-only upload"
    );
    let selected = skin(32);
    let bytes = selected.bytes().to_vec();
    model.choose_skin(selected, "legacy.png".into());
    assert_eq!(model.draft_skin().unwrap().bytes(), bytes);
    assert!(model.set_model(SkinModel::Slim).is_err());
    assert_eq!(model.leave_policy(), LeavePolicy::ConfirmDiscard);
    model.set_side(PreviewSide::Back);
    model.set_outer(false);
    assert!(model.has_unapplied_changes());
    model.discard();
    assert!(!model.has_unapplied_changes());
    assert_eq!(model.leave_policy(), LeavePolicy::Allow);
}
#[tokio::test]
async fn appearance_model_apply_wait_is_finite_unknown_cannot_blind_retry() {
    let (_temp, _server, _service, snapshot, p) = fixture().await;
    let mut model = AppearanceModel::default();
    model.set_accounts(snapshot);
    let ticket = model.ticket().unwrap();
    model.accept_profile(&ticket, Ok(p));
    model.choose_skin(skin(64), "skin.png".into());
    assert!(model.begin_apply().is_some());
    assert_eq!(model.leave_policy(), LeavePolicy::WaitForApply);
    assert!(!model.select_account("other"));
    assert!(!model.choose_skin(skin(64), "new.png".into()));
    model.apply_wait_expired();
    assert_eq!(model.leave_policy(), LeavePolicy::ConfirmUncertain);
    assert!(!model.is_applying());
    assert!(model.begin_apply().is_none());
}
#[tokio::test]
async fn appearance_model_deleted_or_relogged_target_invalidates_late_results() {
    let (_temp, _server, service, snapshot, p) = fixture().await;
    let mut model = AppearanceModel::default();
    model.set_accounts(snapshot);
    let ticket = model.ticket().unwrap();
    model.accept_profile(&ticket, Ok(p.clone()));
    model.choose_skin(skin(64), "skin.png".into());
    service.remove(ID).await.unwrap();
    model.set_accounts(service.snapshot().await.unwrap());
    assert_eq!(model.selected_id(), None);
    assert!(!model.has_unapplied_changes());
    assert!(!model.accept_profile(&ticket, Ok(p)));
    assert!(model.begin_apply().is_none());
}

#[tokio::test]
async fn appearance_model_partial_success_discards_only_unconfirmed_draft() {
    let (_temp, server, service, snapshot, p) = fixture().await;
    Mock::given(path("/6/skins"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(path("/6/capes/active"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let mut model = AppearanceModel::default();
    model.set_accounts(snapshot);
    let ticket = model.ticket().unwrap();
    model.accept_profile(&ticket, Ok(p.clone()));
    model.choose_skin(skin(64), "skin.png".into());
    model.set_cape(Some(p.capes()[0].id().clone()));
    let (ticket, changes) = model.begin_apply().unwrap();
    let (_tx, c) = watch::channel(false);
    let out = service.apply_appearance(ID, changes, c).await.unwrap();
    assert!(model.finish_apply(&ticket, Ok(out)));
    assert!(model.draft_skin().is_none());
    assert!(model.skin().is_some());
    assert!(model.has_unapplied_changes());
    model.discard();
    assert!(!model.has_unapplied_changes());
    assert!(
        model.skin().is_some(),
        "discard must retain acknowledged skin"
    );
}
#[tokio::test]
async fn appearance_model_uncertain_write_cannot_apply_until_resolved_or_left() {
    let (_temp, server, service, snapshot, p) = fixture().await;
    Mock::given(path("/6/skins"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let mut model = AppearanceModel::default();
    model.set_accounts(snapshot);
    let ticket = model.ticket().unwrap();
    model.accept_profile(&ticket, Ok(p));
    model.choose_skin(skin(64), "skin.png".into());
    let (ticket, changes) = model.begin_apply().unwrap();
    let (_tx, c) = watch::channel(false);
    let out = service.apply_appearance(ID, changes, c).await.unwrap();
    model.finish_apply(&ticket, Ok(out));
    assert_eq!(model.leave_policy(), LeavePolicy::ConfirmUncertain);
    assert!(model.begin_apply().is_none());
    assert!(model.begin_reconcile().is_some());
    assert_eq!(model.leave_policy(), LeavePolicy::ConfirmUncertain);
}

#[tokio::test]
async fn appearance_model_failed_reconciliation_never_unlocks_uncertain_write() {
    let (_temp, server, service, snapshot, p) = fixture().await;
    Mock::given(path("/6/skins"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let mut model = AppearanceModel::default();
    model.set_accounts(snapshot);
    let ticket = model.ticket().unwrap();
    model.accept_profile(&ticket, Ok(p));
    model.choose_skin(skin(64), "skin.png".into());
    let (ticket, changes) = model.begin_apply().unwrap();
    let (_tx, c) = watch::channel(false);
    let out = service.apply_appearance(ID, changes, c).await.unwrap();
    model.finish_apply(&ticket, Ok(out));
    let (ticket, _) = model.begin_reconcile().unwrap();
    model.finish_apply(&ticket, Err("connection unavailable".into()));
    assert_eq!(model.leave_policy(), LeavePolicy::ConfirmUncertain);
    assert!(model.begin_apply().is_none());
}

#[tokio::test]
async fn appearance_model_confirmed_write_failed_readback_can_retry_only_read() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (_temp, server, service, snapshot, p) = fixture().await;
    server.reset().await;
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    Mock::given(path("/6"))
        .respond_with(move |_: &wiremock::Request| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id":ID,"skins":[],"capes":[]}))
            } else {
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    Mock::given(path("/6/skins"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let mut model = AppearanceModel::default();
    model.set_accounts(snapshot);
    let ticket = model.ticket().unwrap();
    model.accept_profile(&ticket, Ok(p));
    model.choose_skin(skin(64), "skin.png".into());
    let (ticket, changes) = model.begin_apply().unwrap();
    let (_tx, c) = watch::channel(false);
    let out = service.apply_appearance(ID, changes, c).await.unwrap();
    model.finish_apply(&ticket, Ok(out));
    assert_eq!(model.state(), AppearanceState::Unavailable);
    assert!(model.begin_apply().is_none());
    assert!(
        model.begin_reconcile().is_some(),
        "acknowledged write must offer a read-only retry"
    );
}

#[tokio::test]
async fn appearance_model_confirmed_cape_survives_failed_readback_and_discard() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (_temp, server, service, snapshot, p) = fixture().await;
    server.reset().await;
    let body = serde_json::json!({"id":ID,"skins":[],"capes":[{"id":"cape-one","state":"INACTIVE","url":"https://textures.minecraft.net/texture/abcdef","alias":"One"}]});
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    Mock::given(path("/6"))
        .respond_with(move |_: &wiremock::Request| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200).set_body_json(body.clone())
            } else {
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    Mock::given(path("/6/capes/active"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let cape = p.capes()[0].id().clone();
    let mut model = AppearanceModel::default();
    model.set_accounts(snapshot);
    let ticket = model.ticket().unwrap();
    model.accept_profile(&ticket, Ok(p));
    model.set_cape(Some(cape.clone()));
    let (ticket, changes) = model.begin_apply().unwrap();
    let (_tx, c) = watch::channel(false);
    let out = service.apply_appearance(ID, changes, c).await.unwrap();
    model.finish_apply(&ticket, Ok(out));
    assert_eq!(model.selected_cape(), Some(&cape));
    assert!(!model.has_unapplied_changes());
    model.discard();
    assert_eq!(model.selected_cape(), Some(&cape));
}

#[tokio::test]
async fn appearance_model_removal_clears_prior_operation_evidence_and_leave_guard() {
    let (_temp, server, service, snapshot, p) = fixture().await;
    Mock::given(path("/6/skins"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let mut model = AppearanceModel::default();
    model.set_accounts(snapshot);
    let ticket = model.ticket().unwrap();
    model.accept_profile(&ticket, Ok(p));
    model.choose_skin(skin(64), "skin.png".into());
    let (ticket, changes) = model.begin_apply().unwrap();
    let (_tx, c) = watch::channel(false);
    let out = service.apply_appearance(ID, changes, c).await.unwrap();
    model.finish_apply(&ticket, Ok(out));
    assert!(model.outcome().is_some());
    service.remove(ID).await.unwrap();
    model.set_accounts(service.snapshot().await.unwrap());
    assert!(model.outcome().is_none());
    assert_eq!(model.leave_policy(), LeavePolicy::Allow);
    assert!(model.begin_reconcile().is_none());
}
