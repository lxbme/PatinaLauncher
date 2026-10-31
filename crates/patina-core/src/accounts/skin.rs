//! Optional decoration, isolated from every authenticated client and URL.
use super::{http::cancelled, login::CancelToken};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, Limits, Pixel, RgbaImage, codecs::png::PngDecoder,
};
use reqwest::{Client, Url};
use std::{io::Cursor, time::Duration};

pub(super) const MAX_DOWNLOAD: usize = 256 * 1024;
const MAX_DECODE: u64 = 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SkinError {
    TooLarge,
    InvalidPng,
    InvalidDimensions,
    Decode,
}

pub(super) fn texture_url(hash: &str) -> Option<Url> {
    // Profile parsing already discards the URL and normalizes official HTTP
    // metadata. Never accept a URL here, even from persisted metadata.
    if hash.is_empty()
        || hash.len() > 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Url::parse(&format!("https://textures.minecraft.net/texture/{hash}")).ok()
}

pub(super) struct SkinClient {
    #[cfg(any(test, feature = "test-support"))]
    endpoint: Option<Url>,
}
impl SkinClient {
    pub(super) fn production() -> Self {
        Self {
            #[cfg(any(test, feature = "test-support"))]
            endpoint: None,
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn for_test(endpoint: &str) -> Option<Self> {
        let url = Url::parse(endpoint).ok()?;
        if url.scheme() != "http"
            || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        Some(Self {
            endpoint: Some(url),
        })
    }
    pub(super) async fn fetch(&self, hash: &str, mut cancel: CancelToken) -> Option<Vec<u8>> {
        let url = texture_url(hash)?;
        #[cfg(any(test, feature = "test-support"))]
        let url = self.endpoint.as_ref().map_or(url, |base| {
            base.join(&format!("texture/{hash}"))
                .expect("validated test base")
        });
        if *cancel.borrow() || cancel.has_changed().is_err() {
            return None;
        }
        // No shared/default headers, cookies, redirects, proxy or mirror rewriting.
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .ok()?;
        let download = async {
            let mut response = client.get(url).send().await.ok()?;
            if !response.status().is_success()
                || response
                    .content_length()
                    .is_some_and(|n| n > MAX_DOWNLOAD as u64)
            {
                return None;
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.ok()? {
                if chunk.len() > MAX_DOWNLOAD - bytes.len() {
                    return None;
                }
                bytes.extend_from_slice(&chunk);
            }
            Some(bytes)
        };
        tokio::select! { biased; _ = cancelled(&mut cancel) => None, result = download => result }
    }
}

pub(super) fn decode_head(bytes: &[u8]) -> Result<Vec<u8>, SkinError> {
    if bytes.len() > MAX_DOWNLOAD {
        return Err(SkinError::TooLarge);
    }
    if bytes.len() < 33 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[8..16] != b"\0\0\0\rIHDR" {
        return Err(SkinError::InvalidPng);
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("header"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("header"));
    if width != 64 || !matches!(height, 32 | 64) {
        return Err(SkinError::InvalidDimensions);
    }
    // with_limits applies png's allocation budget during header/chunk parsing,
    // unlike constructing an unlimited decoder and setting limits afterwards.
    let mut limits = Limits::default();
    limits.max_image_width = Some(64);
    limits.max_image_height = Some(64);
    // DynamicImage allocates its output outside png's internal budget.
    // A 64×64 RGBA16 source is the largest supported output (32 KiB).
    limits.max_alloc = Some(MAX_DECODE - 64 * 64 * 8);
    let decoder =
        PngDecoder::with_limits(Cursor::new(bytes), limits).map_err(|_| SkinError::Decode)?;
    if decoder.dimensions() != (width, height) {
        return Err(SkinError::InvalidDimensions);
    }
    let skin = DynamicImage::from_decoder(decoder)
        .map_err(|_| SkinError::Decode)?
        .into_rgba8();
    let mut face = RgbaImage::new(8, 8);
    for y in 0..8 {
        for x in 0..8 {
            let mut pixel = *skin.get_pixel(8 + x, 8 + y);
            pixel.blend(skin.get_pixel(40 + x, 8 + y));
            face.put_pixel(x, y, pixel);
        }
    }
    let head = image::imageops::resize(&face, 64, 64, image::imageops::FilterType::Nearest);
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(head)
        .write_to(&mut out, ImageFormat::Png)
        .map_err(|_| SkinError::Decode)?;
    Ok(out.into_inner())
}
