//! [`ImageCache`]: icon images of local content (mod icons, pack icons,
//! world icons) keyed by where they came from.
//!
//! PNGs are decoded with icon-sized limits and normalized on the blocking
//! pool before GPUI sees them. Cache accounting includes the retained PNG
//! and an upper bound for GPUI's decoded raster.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

use gpui_kit::component::{ActiveTheme as _, Icon, h_flex};
use gpui_kit::{
    AnyElement, Context, Global, Image, ImageFormat, ImageSource, IntoElement, ObjectFit, Rems,
    Styled as _, StyledImage as _, Task, img, rems,
};
use gpui_kit::{ParentElement as _, assets::IconName};

use crate::core_bridge::run_blocking;

/// Most images kept; beyond it the least recently used goes first.
pub const CAPACITY: usize = 512;
/// Most retained encoded and decoded bytes; the least recently used go first beyond it.
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
/// Icons larger than this are not loaded (the placeholder stays).
pub const MAX_ICON_BYTES: usize = 256 * 1024;
const ICON_SIDE: u32 = 64;
const DECODED_ICON_BYTES: usize = (ICON_SIDE * ICON_SIDE * 4) as usize;
const MAX_SOURCE_SIDE: u32 = 1024;

/// Where an image came from: instance id, file name inside the content
/// directory, and the file's size and modification time, so a changed file
/// is a new entry. Callers pass the raw values of the file's metadata
/// (`len()` and `modified()`), not rounded ones.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageKey {
    pub instance: String,
    pub file: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl ImageKey {
    pub fn new(
        instance: impl Into<String>,
        file: impl Into<String>,
        size: u64,
        modified: Option<SystemTime>,
    ) -> Self {
        Self {
            instance: instance.into(),
            file: file.into(),
            size,
            modified,
        }
    }
}

struct Entry {
    image: Arc<Image>,
    used: u64,
    bytes: usize,
}

/// The cache (a gpui global, see `content::init`).
///
/// Evicting an entry returns its image: gpui keeps its own decoded copy
/// keyed by the `Arc<Image>`, which stays until `Image::remove_asset` is
/// called, so the code that evicts ([`icon_tile`]) calls it for every
/// returned image.
pub struct ImageCache {
    entries: HashMap<ImageKey, Entry>,
    /// Bumped on every use; the entry with the lowest value is the oldest.
    tick: u64,
    capacity: usize,
    max_bytes: usize,
    bytes: usize,
    /// Keys being read, so a redraw does not start a second read.
    pending: HashSet<ImageKey>,
    /// Keys that could not be read; not retried until the file changes (its
    /// size and modification time are part of the key).
    failed: HashSet<ImageKey>,
}

impl Global for ImageCache {}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageCache {
    pub fn new() -> Self {
        Self::with_limits(CAPACITY, MAX_BYTES)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_limits(capacity, MAX_BYTES)
    }

    pub fn with_limits(capacity: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            tick: 0,
            capacity: capacity.max(1),
            max_bytes,
            bytes: 0,
            pending: HashSet::new(),
            failed: HashSet::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes of image data held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The cached image for `key`, counted as used.
    pub fn get(&mut self, key: &ImageKey) -> Option<Arc<Image>> {
        self.tick += 1;
        let tick = self.tick;
        let entry = self.entries.get_mut(key)?;
        entry.used = tick;
        Some(entry.image.clone())
    }

    /// Whether `key` is cached, without counting a use.
    pub fn contains(&self, key: &ImageKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Whether `key` is being read.
    pub fn is_pending(&self, key: &ImageKey) -> bool {
        self.pending.contains(key)
    }

    /// Caches `image` as the newest entry, dropping least recently used
    /// ones while over the count or byte limit. Returns what was dropped
    /// (the replaced entry of `key` included).
    #[must_use = "call Image::remove_asset on what was evicted"]
    pub fn insert(&mut self, key: ImageKey, image: Arc<Image>) -> Vec<Arc<Image>> {
        self.tick += 1;
        self.pending.remove(&key);
        let bytes = image.bytes.len() + DECODED_ICON_BYTES;
        let mut evicted = Vec::new();
        let entry = Entry {
            image,
            used: self.tick,
            bytes,
        };
        if let Some(old) = self.entries.insert(key.clone(), entry) {
            self.bytes -= old.bytes;
            evicted.push(old.image);
        }
        self.bytes += bytes;
        // The entry just inserted is the newest, so it goes last.
        while self.entries.len() > self.capacity || self.bytes > self.max_bytes {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(old) = self.entries.remove(&oldest) {
                self.bytes -= old.bytes;
                evicted.push(old.image);
            }
        }
        evicted
    }

    /// Validates and normalizes a PNG on the blocking pool, never the UI thread.
    fn image_from_png(bytes: Vec<u8>) -> Option<Arc<Image>> {
        use std::io::Cursor;
        let mut reader =
            image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_SOURCE_SIDE);
        limits.max_image_height = Some(MAX_SOURCE_SIDE);
        limits.max_alloc = Some(4 * 1024 * 1024);
        reader.limits(limits);
        let image = reader
            .decode()
            .ok()?
            .thumbnail(ICON_SIDE, ICON_SIDE)
            .into_rgba8();
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).ok()?;
        Some(Arc::new(Image::from_bytes(
            ImageFormat::Png,
            png.into_inner(),
        )))
    }
}

/// Reads `path` for an icon: `None` when it cannot be read or is larger
/// than [`MAX_ICON_BYTES`] (never reads more than that). For use in the
/// `read` of [`icon_tile`].
pub fn read_icon_file(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_ICON_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_ICON_BYTES).then_some(bytes)
}

/// Starts loading the image for `key` through `read` (PNG bytes; it runs on
/// the blocking pool; more than [`MAX_ICON_BYTES`] counts as unreadable)
/// unless it is cached, loading or known unreadable. The result goes into
/// the global cache even when `view` is gone by then; `view` is notified
/// if it is still alive. The task finishes when the load has landed.
pub fn request_image<V: 'static>(
    key: ImageKey,
    read: impl FnOnce() -> Option<Vec<u8>> + Send + 'static,
    cx: &mut Context<V>,
) -> Task<()> {
    let cache = cx.global_mut::<ImageCache>();
    if cache.contains(&key) || cache.failed.contains(&key) || !cache.pending.insert(key.clone()) {
        return Task::ready(());
    }
    let load = run_blocking(cx, move || {
        read()
            .filter(|bytes| bytes.len() <= MAX_ICON_BYTES)
            .and_then(ImageCache::image_from_png)
    });
    cx.spawn(async move |this, cx| {
        let loaded = load.await.ok().flatten();
        cx.update_global::<ImageCache, _>(|cache, cx| match loaded {
            Some(image) => {
                for old in cache.insert(key, image) {
                    old.remove_asset(cx);
                }
            }
            None => {
                cache.pending.remove(&key);
                cache.failed.insert(key);
            }
        });
        this.update(cx, |_, cx| cx.notify()).ok();
    })
}

/// A square icon for `key` in a view's render: the cached image, or a
/// placeholder block while `read` loads it (see [`request_image`]).
pub fn icon_tile<V: 'static>(
    key: ImageKey,
    read: impl FnOnce() -> Option<Vec<u8>> + Send + 'static,
    size: Rems,
    fallback: IconName,
    cx: &mut Context<V>,
) -> AnyElement {
    let radius = rems(size.0 * 0.22);
    let cached = cx.global_mut::<ImageCache>().get(&key);
    if let Some(image) = cached {
        return img(ImageSource::Image(image))
            .flex_none()
            .size(size)
            .rounded(radius)
            .object_fit(ObjectFit::Cover)
            .into_any_element();
    }
    request_image(key, read, cx).detach();
    let theme = cx.theme();
    h_flex()
        .flex_none()
        .size(size)
        .rounded(radius)
        .justify_center()
        .bg(theme.muted)
        .text_color(theme.muted_foreground)
        .child(Icon::new(fallback).size(rems(size.0 * 0.5)))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc;

    use gpui_kit::{AppContext as _, TestAppContext};
    use patina_core::paths::DataDirs;

    use super::*;
    use crate::core_bridge::testing::{self, FakeCatalog};

    fn key(n: usize) -> ImageKey {
        ImageKey::new("inst", format!("{n}.jar"), 3, None)
    }

    fn image() -> Arc<Image> {
        Arc::new(Image::from_bytes(ImageFormat::Png, vec![1, 2, 3]))
    }

    #[test]
    fn image_cache_evicts_lru() {
        let mut cache = ImageCache::new();
        for n in 0..CAPACITY {
            assert!(cache.insert(key(n), image()).is_empty());
        }
        assert_eq!(cache.len(), CAPACITY);
        // Using entry 0 makes entry 1 the oldest.
        assert!(cache.get(&key(0)).is_some());
        assert_eq!(cache.insert(key(CAPACITY), image()).len(), 1);
        assert_eq!(cache.len(), CAPACITY);
        assert!(cache.contains(&key(0)));
        assert!(!cache.contains(&key(1)));
        assert!(cache.contains(&key(CAPACITY)));
    }

    #[test]
    fn bytes_are_capped_too() {
        let mut cache = ImageCache::with_limits(100, 2 * (DECODED_ICON_BYTES + 4));
        let big = |n: u8| Arc::new(Image::from_bytes(ImageFormat::Png, vec![n; 4]));
        let _ = cache.insert(key(0), big(0));
        let _ = cache.insert(key(1), big(1));
        assert_eq!(cache.bytes(), 2 * (DECODED_ICON_BYTES + 4));
        // A third raster exceeds the budget: the oldest and its bytes go.
        assert_eq!(cache.insert(key(2), big(2)).len(), 1);
        assert_eq!(cache.bytes(), 2 * (DECODED_ICON_BYTES + 4));
        assert!(!cache.contains(&key(0)));
        assert!(cache.contains(&key(2)));
        // Replacing a key does not double count it.
        assert_eq!(cache.insert(key(2), big(3)).len(), 1);
        assert_eq!(cache.bytes(), 2 * (DECODED_ICON_BYTES + 4));
    }

    #[test]
    fn a_changed_file_is_a_new_entry() {
        let t = SystemTime::UNIX_EPOCH;
        let a = ImageKey::new("i", "a.jar", 5, Some(t));
        let b = ImageKey::new("i", "a.jar", 5, Some(t + std::time::Duration::from_secs(1)));
        let c = ImageKey::new("i", "a.jar", 6, Some(t));
        let mut cache = ImageCache::new();
        let _ = cache.insert(a.clone(), image());
        assert!(cache.get(&b).is_none());
        assert!(cache.get(&c).is_none());
        assert!(cache.get(&a).is_some());
    }

    #[test]
    fn oversized_icon_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.png");
        let big = dir.path().join("big.png");
        std::fs::write(&small, vec![0u8; MAX_ICON_BYTES]).unwrap();
        std::fs::write(&big, vec![0u8; MAX_ICON_BYTES + 1]).unwrap();
        assert!(read_icon_file(&small).is_some());
        assert!(read_icon_file(&big).is_none());
        assert!(read_icon_file(&dir.path().join("missing.png")).is_none());
    }

    #[gpui_kit::test]
    async fn oversized_compressible_and_corrupt_pngs_are_rejected(cx: &mut TestAppContext) {
        use image::ImageEncoder as _;
        let (_rt, _dir) = boot(cx);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                &vec![0; 2048 * 2048],
                2048,
                2048,
                image::ExtendedColorType::L8,
            )
            .unwrap();
        assert!(png.len() < MAX_ICON_BYTES);
        let view = cx.new(|_| View);
        for (n, bytes) in [(10, png), (11, b"\x89PNG\r\n\x1a\ncorrupt".to_vec())] {
            view.update(cx, |_, cx| request_image(key(n), move || Some(bytes), cx))
                .await;
            assert!(!cx.update(|cx| cx.global::<ImageCache>().contains(&key(n))));
            assert!(!cx.update(|cx| cx.global::<ImageCache>().is_pending(&key(n))));
        }
    }

    #[gpui_kit::test]
    async fn accepted_icons_are_normalized_and_accounted_as_rasters(cx: &mut TestAppContext) {
        use image::ImageEncoder as _;
        let (_rt, _dir) = boot(cx);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&vec![0; 512 * 512], 512, 512, image::ExtendedColorType::L8)
            .unwrap();
        let view = cx.new(|_| View);
        view.update(cx, |_, cx| request_image(key(12), move || Some(png), cx))
            .await;
        let (image, retained) = cx.update(|cx| {
            let cache = cx.global_mut::<ImageCache>();
            (cache.get(&key(12)).unwrap(), cache.bytes())
        });
        let decoded = image::load_from_memory(&image.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 64));
        assert_eq!(retained, image.bytes.len() + 16_384);
    }

    struct View;

    fn boot(cx: &mut TestAppContext) -> (tokio::runtime::Runtime, tempfile::TempDir) {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = testing::core(
            rt.handle().clone(),
            DataDirs::new(dir.path()),
            Arc::new(FakeCatalog::new().with_manifest_error("offline")),
        );
        cx.update(|cx| {
            cx.set_global(core);
            cx.set_global(ImageCache::new());
        });
        cx.executor().allow_parking();
        (rt, dir)
    }

    #[gpui_kit::test]
    async fn a_load_survives_its_view_being_dropped(cx: &mut TestAppContext) {
        let (_rt, _dir) = boot(cx);
        let k = key(1);
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let view = cx.new(|_| View);
        let task = view.update(cx, |_, cx| {
            request_image(
                k.clone(),
                move || {
                    gate.lock().unwrap().recv().ok();
                    Some(include_bytes!("../../assets/instance-icons/axe.png").to_vec())
                },
                cx,
            )
        });
        assert!(cx.update(|cx| cx.global::<ImageCache>().is_pending(&k)));
        // The view goes away while the load is in flight.
        drop(view);
        cx.run_until_parked();
        release.send(()).unwrap();
        task.await;
        assert!(cx.update(|cx| cx.global::<ImageCache>().contains(&k)));
        assert!(!cx.update(|cx| cx.global::<ImageCache>().is_pending(&k)));

        // A new view finds it cached and starts no read.
        let view = cx.new(|_| View);
        let read_again = view.update(cx, |_, cx| {
            request_image(k.clone(), || panic!("must not read again"), cx)
        });
        read_again.await;
    }

    #[gpui_kit::test]
    async fn an_oversized_read_is_not_cached_or_retried(cx: &mut TestAppContext) {
        let (_rt, _dir) = boot(cx);
        let k = key(2);
        let view = cx.new(|_| View);
        let task = view.update(cx, |_, cx| {
            request_image(k.clone(), || Some(vec![0; MAX_ICON_BYTES + 1]), cx)
        });
        task.await;
        assert!(!cx.update(|cx| cx.global::<ImageCache>().contains(&k)));
        view.update(cx, |_, cx| {
            request_image(k.clone(), || panic!("failed keys are not retried"), cx)
        })
        .await;
    }
}
