//! Retained, bounded heads driven by the single account service subscription.
use crate::core_bridge::run;
use gpui_kit::{App, Context, Entity, Global, RenderImage, Task};
use image::{DynamicImage, ImageDecoder, Limits, codecs::png::PngDecoder};
use patina_core::accounts::{Account, AccountKind, AccountService};
use std::{collections::HashMap, io::Cursor, sync::Arc};

const MAX_HEADS: usize = 64;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const RASTER_BYTES: usize = 64 * 64 * 4;

/// The service emits only noninterlaced RGBA8 IHDR/IDAT/IEND. Refuse any
/// ancillary data before constructing image's PNG parser (including ICC).
/// No filtering copy is allocated here; core owns normalization of raw skins.
fn normalized_head_input(bytes: &[u8]) -> bool {
    if bytes.len() > 256 * 1024
        || bytes.len() < 33
        || &bytes[..16] != b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"
        || bytes[16..24] != [0, 0, 0, 64, 0, 0, 0, 64]
        || bytes[24..29] != [8, 6, 0, 0, 0]
    {
        return false;
    }
    let mut offset: usize = 33;
    let mut data = false;
    while let Some(header) = bytes.get(offset..offset + 8) {
        let len = u32::from_be_bytes(header[..4].try_into().expect("chunk length")) as usize;
        let Some(end) = offset.checked_add(12).and_then(|n| n.checked_add(len)) else {
            return false;
        };
        if end > bytes.len() {
            return false;
        }
        match &header[4..8] {
            b"IDAT" => data = true,
            b"IEND" => return len == 0 && data && end == bytes.len(),
            _ => return false,
        }
        offset = end;
    }
    false
}

struct PreparedHead(image::RgbaImage);
impl PreparedHead {
    fn decode(bytes: &[u8]) -> Option<Self> {
        if !normalized_head_input(bytes) {
            return None;
        }
        let mut limits = Limits::default();
        limits.max_image_width = Some(64);
        limits.max_image_height = Some(64);
        // No metadata parser path is reachable. The core pixel-stream bound
        // also covers this decoder, with no filter copy and RGBA8-only output.
        limits.max_alloc = Some(1024 * 1024);
        let decoder = PngDecoder::with_limits(Cursor::new(bytes), limits).ok()?;
        if decoder.dimensions() != (64, 64) {
            return None;
        }
        let mut raster = DynamicImage::from_decoder(decoder).ok()?.into_rgba8();
        // GPUI RenderImage consumes BGRA. Decode once, away from GPUI; using
        // RenderImage avoids a second unbounded path/encoded-image asset cache.
        for pixel in raster.pixels_mut() {
            pixel.0.swap(0, 2);
        }
        Some(Self(raster))
    }
}
struct HeadEntry {
    account: Account,
    image: Arc<RenderImage>,
    bytes: usize,
}
pub(crate) struct AvatarImages(pub(crate) Entity<AvatarCache>);
impl Global for AvatarImages {}

pub(crate) struct AvatarCache {
    revision: Option<u64>,
    allowed: HashMap<String, Account>,
    entries: HashMap<String, HeadEntry>,
    pending: HashMap<String, Task<()>>,
    bytes: usize,
    // App queue/final-decode admission. Non-preemptible core decodes retain
    // their own service-owned admission even when this pipeline is aborted.
    loads: Arc<tokio::sync::Semaphore>,
}
impl AvatarCache {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        cx.on_release(|this, cx| this.clear(cx)).detach();
        Self {
            revision: None,
            allowed: HashMap::new(),
            entries: HashMap::new(),
            pending: HashMap::new(),
            bytes: 0,
            loads: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
    /// Coalesced watch values are revisions, not a count of wakeups. A repeated
    /// snapshot does not reread files, decode again, or allocate new image IDs.
    pub(crate) fn reconcile(
        &mut self,
        revision: u64,
        accounts: &[Account],
        cx: &mut Context<Self>,
    ) {
        if self.revision.is_some_and(|old| revision <= old) {
            return;
        }
        self.revision = Some(revision);
        self.pending.clear();
        self.allowed = accounts
            .iter()
            .filter(|a| a.kind == AccountKind::Msa && a.skin_hash().is_some())
            .take(MAX_HEADS)
            .map(|a| (a.id.clone(), a.clone()))
            .collect();
        let removed: Vec<_> = self
            .entries
            .iter()
            .filter(|(id, entry)| self.allowed.get(*id) != Some(&entry.account))
            .map(|(id, _)| id.clone())
            .collect();
        for id in removed {
            self.evict(&id, cx);
        }
        cx.notify();
    }
    pub(crate) fn load(
        &mut self,
        account: &Account,
        service: AccountService,
        cx: &mut Context<Self>,
    ) {
        if self.allowed.get(&account.id) != Some(account)
            || self.entries.contains_key(&account.id)
            || self.pending.contains_key(&account.id)
        {
            return;
        }
        let Some(revision) = self.revision else {
            return;
        };
        let account = account.clone();
        let id = account.id.clone();
        let changes = service.changes();
        let loads = self.loads.clone();
        let work = run(cx, async move {
            let permit = loads.acquire_owned().await.ok()?;
            let (_cancel, token) = tokio::sync::watch::channel(false);
            service
                .cache_head(&account.id, token.clone())
                .await
                .ok()
                .flatten()?;
            let bytes = service
                .read_cached_head(&account.id, token)
                .await
                .ok()
                .flatten()?;
            let head = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                PreparedHead::decode(&bytes)
            })
            .await
            .ok()
            .flatten()?;
            Some((account, head))
        });
        let pending_id = id.clone();
        let task = cx.spawn(async move |weak, cx| {
            let result = work.await.ok().flatten();
            let _ = weak.update(cx, |this, cx| {
                if this.revision != Some(revision) {
                    return;
                }
                this.pending.remove(&pending_id);
                // Check the producer's latest value too, not only the last GPUI
                // wake: a committed removal may precede our watch callback.
                if *changes.borrow() != revision {
                    return;
                }
                if let Some((account, head)) = result {
                    this.insert(revision, &account, head, cx);
                }
            });
        });
        self.pending.insert(id, task);
    }
    fn insert(
        &mut self,
        revision: u64,
        account: &Account,
        head: PreparedHead,
        cx: &mut Context<Self>,
    ) {
        if self.revision != Some(revision)
            || self.allowed.get(&account.id) != Some(account)
            || self.entries.contains_key(&account.id)
        {
            return;
        }
        let bytes = head.0.as_raw().capacity();
        if head.0.as_raw().len() != RASTER_BYTES
            || self.entries.len() >= MAX_HEADS
            || self.bytes + bytes > MAX_BYTES
        {
            return;
        }
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(head.0)]));
        self.entries.insert(
            account.id.clone(),
            HeadEntry {
                account: account.clone(),
                image,
                bytes,
            },
        );
        self.bytes += bytes;
        cx.notify();
    }
    pub(crate) fn image(&self, account: &Account) -> Option<Arc<RenderImage>> {
        self.entries
            .get(&account.id)
            .filter(|entry| entry.account == *account)
            .map(|entry| entry.image.clone())
    }
    fn evict(&mut self, id: &str, cx: &mut App) {
        if let Some(entry) = self.entries.remove(id) {
            self.bytes -= entry.bytes;
            // No encoded/path asset exists. Defer atlas removal until GPUI has
            // restored any currently updating window to App.windows.
            cx.defer(move |cx| cx.drop_image(entry.image, None));
        }
    }
    fn clear(&mut self, cx: &mut App) {
        self.pending.clear();
        self.allowed.clear();
        for (_, entry) in self.entries.drain() {
            cx.defer(move |cx| cx.drop_image(entry.image, None));
        }
        self.bytes = 0;
    }
}

pub(crate) fn install(cx: &mut App) -> Entity<AvatarCache> {
    use gpui_kit::AppContext;
    if let Some(images) = cx.try_global::<AvatarImages>() {
        return images.0.clone();
    }
    let cache = cx.new(AvatarCache::new);
    cx.set_global(AvatarImages(cache.clone()));
    cache
}

pub(crate) fn cached_image(account: &Account, cx: &App) -> Option<Arc<RenderImage>> {
    cx.try_global::<AvatarImages>()?.0.read(cx).image(account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AppContext, TestAppContext};
    use image::{DynamicImage, ImageFormat, RgbaImage};
    use std::io::Cursor;

    fn head() -> PreparedHead {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(RgbaImage::new(64, 64))
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        PreparedHead::decode(&png.into_inner()).unwrap()
    }
    fn account(ix: u128) -> patina_core::accounts::Account {
        let uuid = format!("{ix:032x}");
        // Public serialized metadata is the fixture seam, never a native service.
        toml::from_str(&format!("id = '{uuid}'\nkind = 'msa'\nusername = 'Player'\nuuid = '{uuid}'\nskin_hash = 'abcdef'\n")).unwrap()
    }
    #[gpui_kit::test]
    fn retained_heads_are_bounded_and_removed_on_snapshot_change(cx: &mut TestAppContext) {
        let cache = cx.new(AvatarCache::new);
        let accounts: Vec<_> = (1..=70).map(account).collect();
        let old = cache.update(cx, |cache, cx| {
            cache.reconcile(1, &accounts, cx);
            for a in &accounts {
                cache.insert(1, a, head(), cx);
            }
            assert!(cache.entries.len() <= 64);
            assert!(cache.bytes <= 2 * 1024 * 1024);
            assert!(cache.image(&accounts[0]).is_some());
            let old = std::sync::Arc::downgrade(&cache.image(&accounts[0]).unwrap());
            cache.reconcile(2, &[], cx);
            assert_eq!(cache.bytes, 0);
            cache.insert(1, &accounts[0], head(), cx);
            assert!(cache.image(&accounts[0]).is_none());
            old
        });
        assert!(old.upgrade().is_none());
    }
    #[gpui_kit::test]
    fn repeated_snapshots_reuse_ids_and_relogin_never_uses_old_head(cx: &mut TestAppContext) {
        let cache = cx.new(AvatarCache::new);
        let original = account(1);
        let changed: Account = toml::from_str(&format!(
            "{}\ntoken_ref = '11111111-1111-1111-1111-111111111111'",
            toml::to_string(&original).unwrap()
        ))
        .unwrap();
        cache.update(cx, |cache, cx| {
            cache.reconcile(10, std::slice::from_ref(&original), cx);
            cache.insert(10, &original, head(), cx);
            let id = cache.image(&original).unwrap().id;
            cache.reconcile(10, std::slice::from_ref(&original), cx);
            cache.insert(10, &original, head(), cx);
            assert_eq!(cache.image(&original).unwrap().id, id);
            assert!(cache.image(&changed).is_none());
            cache.reconcile(20, std::slice::from_ref(&changed), cx);
            cache.insert(10, &original, head(), cx);
            assert!(cache.image(&original).is_none());
            cache.insert(20, &changed, head(), cx);
            assert_ne!(cache.image(&changed).unwrap().id, id);
            cache.reconcile(19, std::slice::from_ref(&original), cx);
            assert!(cache.image(&changed).is_some());
        });
    }
    #[gpui_kit::test]
    fn release_drops_all_retained_images(cx: &mut TestAppContext) {
        let cache = cx.new(AvatarCache::new);
        let weak = cache.update(cx, |cache, cx| {
            let account = account(1);
            cache.reconcile(0, std::slice::from_ref(&account), cx);
            cache.insert(0, &account, head(), cx);
            Arc::downgrade(&cache.image(&account).unwrap())
        });
        cx.update(|_| drop(cache));
        cx.run_until_parked();
        assert!(weak.upgrade().is_none());
    }
    #[gpui_kit::test]
    fn retention_accounts_for_raster_allocation_not_just_length(cx: &mut TestAppContext) {
        let cache = cx.new(AvatarCache::new);
        cache.update(cx, |cache, cx| {
            let accounts: Vec<_> = (1..=3).map(account).collect();
            cache.reconcile(0, &accounts, cx);
            for account in &accounts {
                let mut bytes = Vec::with_capacity(1024 * 1024);
                bytes.resize(RASTER_BYTES, 0);
                let head = PreparedHead(RgbaImage::from_raw(64, 64, bytes).unwrap());
                cache.insert(0, account, head, cx);
            }
            assert_eq!(cache.bytes, 2 * 1024 * 1024);
            assert_eq!(cache.entries.len(), 2);
            assert!(cache.image(&accounts[2]).is_none());
        });
    }
    #[gpui_kit::test]
    async fn lifecycle_load_uses_runtime_and_latest_service_revision(cx: &mut TestAppContext) {
        use crate::core_bridge::testing::{self, FakeCatalog};
        use patina_core::{
            accounts::{
                error::AuthStage,
                testing::{
                    AuthClock, AuthHttp, MemoryCredentials, MsaClient, credential_store,
                    seed_verified,
                },
            },
            paths::DataDirs,
        };
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
        struct Clock;
        impl AuthClock for Clock {
            fn unix_seconds(&self) -> u64 {
                1_800_000_000
            }
        }
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let data = DataDirs::new(dir.path());
        let core = testing::core(
            rt.handle().clone(),
            data.clone(),
            Arc::new(FakeCatalog::new()),
        );
        cx.update(|cx| cx.set_global(core));
        cx.executor().allow_parking();
        let setup = cx.update(|cx| {
            run(cx, async move {
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
                let clock: Arc<dyn AuthClock> = Arc::new(Clock);
                let mut service = AccountService::for_test(
                    data.clone(),
                    MsaClient::for_test(http, clock.clone()),
                    credential_store(data.clone(), Arc::new(MemoryCredentials::default())),
                    clock,
                );
                service.set_skin_endpoint_for_test(&server.uri()).unwrap();
                let a = seed_verified(
                    &service,
                    "Player",
                    account(1).uuid,
                    "REFRESH_SENTINEL",
                    1_800_000_000,
                    1_800_003_600,
                )
                .await
                .unwrap();
                // Seed the optional nonsecret profile field through persisted metadata.
                let file = data.root().join("accounts.toml");
                let mut metadata: toml::Value =
                    toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
                metadata["accounts"][0]
                    .as_table_mut()
                    .unwrap()
                    .insert("skin_hash".into(), toml::Value::String("abcdef".into()));
                std::fs::write(file, toml::to_string(&metadata).unwrap()).unwrap();
                service.reload().await.unwrap();
                let mut png = Cursor::new(Vec::new());
                DynamicImage::ImageRgba8(RgbaImage::from_pixel(
                    64,
                    64,
                    image::Rgba([255, 0, 0, 255]),
                ))
                .write_to(&mut png, ImageFormat::Png)
                .unwrap();
                Mock::given(path("/texture/abcdef"))
                    .respond_with(ResponseTemplate::new(200).set_body_bytes(png.into_inner()))
                    .mount(&server)
                    .await;
                let snapshot = service.snapshot().await.unwrap();
                let account = snapshot
                    .accounts()
                    .iter()
                    .find(|current| current.id == a.id)
                    .unwrap()
                    .clone();
                (service, server, snapshot.revision(), account)
            })
        });
        let (service, server, revision, account) = setup.await.unwrap();
        let cache = cx.new(AvatarCache::new);
        cx.update(|cx| cx.set_global(AvatarImages(cache.clone())));
        cache.update(cx, |cache, cx| {
            cache.reconcile(revision, std::slice::from_ref(&account), cx);
            cache.load(&account, service.clone(), cx);
        });
        cx.condition(&cache, |cache, _| cache.image(&account).is_some())
            .await;
        cx.update(|cx| {
            let image = cached_image(&account, cx).unwrap();
            assert_eq!(&image.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
            assert_eq!(image.frame_count(), 1);
        });
        cache.update(cx, |cache, cx| cache.load(&account, service.clone(), cx));
        let requests = cx
            .update(|cx| run(cx, async move { server.received_requests().await.unwrap() }))
            .await
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].headers.contains_key("authorization"));
        let id = account.id.clone();
        let latest = cx
            .update(|cx| {
                run(cx, async move {
                    service.remove(&id).await.unwrap();
                    service.snapshot().await.unwrap()
                })
            })
            .await
            .unwrap();
        cache.update(cx, |cache, cx| {
            cache.reconcile(latest.revision(), latest.accounts(), cx)
        });
        cx.update(|cx| assert!(cached_image(&account, cx).is_none()));
    }
    #[gpui_kit::test]
    fn eviction_from_window_update_removes_current_window_atlas(cx: &mut TestAppContext) {
        use gpui_kit::{IntoElement, Render, Window, rems};
        struct HeadView(Account);
        impl Render for HeadView {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                super::super::avatar(&self.0, rems(2.), cx)
            }
        }
        cx.update(gpui_kit::init);
        let account = account(1);
        let cache = cx.new(AvatarCache::new);
        let image = cache.update(cx, |cache, cx| {
            cache.reconcile(1, std::slice::from_ref(&account), cx);
            cache.insert(1, &account, head(), cx);
            cache.image(&account).unwrap()
        });
        cx.update(|cx| cx.set_global(AvatarImages(cache.clone())));
        let (_, cx) = cx.add_window_view(|window, cx| {
            crate::theme::apply(window, cx);
            HeadView(account)
        });
        cx.update(|window, _| assert!(window.has_image_atlas_entry(&image)));
        // GPUI temporarily removes the current window from App.windows here.
        cx.update(|_, cx| cache.update(cx, |cache, cx| cache.reconcile(2, &[], cx)));
        cx.update(|window, _| assert!(!window.has_image_atlas_entry(&image)));
    }
    #[test]
    fn normalized_head_guard_rejects_metadata_before_png_parser() {
        let mut original = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(RgbaImage::new(64, 64))
            .write_to(&mut original, ImageFormat::Png)
            .unwrap();
        for kind in [b"iCCP", b"zTXt", b"eXIf"] {
            let data = b"profile\0\0\x78\x9c\x03\x00\x00\x00\x00\x01";
            let mut body = kind.to_vec();
            body.extend(data);
            let mut crc = !0u32;
            for &byte in &body {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb88320 } else { 0 };
                }
            }
            let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
            chunk.extend(body);
            chunk.extend((!crc).to_be_bytes());
            let mut annotated = original.get_ref().clone();
            annotated.splice(33..33, chunk);
            assert!(
                PreparedHead::decode(&annotated).is_none(),
                "unused ancillary reached app decoder: {kind:?}"
            );
        }
        assert!(PreparedHead::decode(original.get_ref()).is_some());
    }
    #[test]
    fn malformed_heads_use_letter_fallback() {
        assert!(PreparedHead::decode(b"bad PNG").is_none());
        assert!(PreparedHead::decode(&vec![0; 256 * 1024 + 1]).is_none());
    }
}
