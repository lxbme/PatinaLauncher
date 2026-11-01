//! Optional decoration, isolated from every authenticated client and URL.
use super::{http::cancelled, login::CancelToken};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, Limits, Pixel, RgbaImage, codecs::png::PngDecoder,
};
use reqwest::{Client, Url};
use std::{io::Cursor, time::Duration};

pub(super) const MAX_DOWNLOAD: usize = 256 * 1024;
const MAX_DECODE: u64 = 1024 * 1024;
// png 0.18.1 / fdeflate 0.3.7 with metadata excluded by pixel_png:
// 256KiB fixed filter copy; 512KiB inflater allowance (two <=2^15-entry
// u16 secondary tables, 2x Vec growth and realloc overlap, 18KiB primary
// tables/reset overlap and <=2KiB boxed state); 128KiB PNG allowance
// (<=66KiB unfilter Vec + old allocation, rows, <=1KiB individual chunk/palette buffers);
// 32KiB RGBA16 output. IDAT streams into the bounded frame, never an ICC Vec.
// These conservative capacity allowances total 928KiB, not a length limit.
pub(super) const PIXEL_DECODE_ALLOCATION_BOUND: usize =
    MAX_DOWNLOAD + 512 * 1024 + 128 * 1024 + 32 * 1024;
const _: () = assert!(PIXEL_DECODE_ALLOCATION_BOUND <= MAX_DECODE as usize);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SkinError {
    TooLarge,
    InvalidPng,
    InvalidDimensions,
    Decode,
}

pub(super) fn valid_texture_hash(hash: &str) -> bool {
    !hash.is_empty()
        && hash.len() <= 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn texture_url(hash: &str) -> Option<Url> {
    // Profile parsing already discards the URL and normalizes official HTTP
    // metadata. Never accept a URL here, even from persisted metadata.
    if !valid_texture_hash(hash) {
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

/// Exclude unused metadata *before* png parses it. Keep only the PNG pixel
/// stream and bounded palette/transparency chunks; library code still owns
/// CRC validation, inflate, filtering, color conversion and interlacing.
pub(super) fn pixel_png(bytes: &[u8]) -> Result<Vec<u8>, SkinError> {
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
    // One allocation with no growth, including when every chunk is retained.
    let mut pixels = Vec::with_capacity(bytes.len());
    pixels.extend_from_slice(&bytes[..33]);
    let mut offset = 33;
    let mut palette = false;
    let mut transparency = false;
    let mut data = false;
    let mut data_ended = false;
    while let Some(header) = bytes.get(offset..offset + 8) {
        let len = u32::from_be_bytes(header[..4].try_into().expect("chunk length")) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|n| n.checked_add(len))
            .ok_or(SkinError::InvalidPng)?;
        let chunk = bytes.get(offset..end).ok_or(SkinError::InvalidPng)?;
        let kind = &header[4..8];
        if !kind.iter().all(u8::is_ascii_alphabetic) || !kind[2].is_ascii_uppercase() {
            return Err(SkinError::InvalidPng);
        }
        let keep = match kind {
            b"PLTE" if !palette && !data && (3..=768).contains(&len) && len.is_multiple_of(3) => {
                palette = true;
                true
            }
            b"tRNS" if !transparency && !data && (1..=256).contains(&len) => {
                transparency = true;
                true
            }
            b"IDAT" if !data_ended => {
                data = true;
                true
            }
            b"IEND" if len == 0 && data && end == bytes.len() => {
                pixels.extend_from_slice(chunk);
                return Ok(pixels);
            }
            b"tRNS" => return Err(SkinError::InvalidPng),
            _ if kind[0].is_ascii_uppercase() => return Err(SkinError::InvalidPng),
            _ => false,
        };
        if data && kind != b"IDAT" {
            data_ended = true;
        }
        if keep {
            pixels.extend_from_slice(chunk);
        }
        offset = end;
    }
    Err(SkinError::InvalidPng)
}

/// One decoder input path for both downloaded skins and replaced cached heads.
pub(super) fn decode_pixels(bytes: &[u8]) -> Result<RgbaImage, SkinError> {
    let pixels = pixel_png(bytes)?;
    // The allowlist/capacity accounting excludes ancillary allocation; limits
    // remain defense in depth, not a claim about Vec capacity in dependencies.
    let mut limits = Limits::default();
    limits.max_image_width = Some(64);
    limits.max_image_height = Some(64);
    limits.max_alloc = Some(MAX_DECODE);
    let decoder =
        PngDecoder::with_limits(Cursor::new(pixels), limits).map_err(|_| SkinError::Decode)?;
    if decoder.dimensions().0 != 64 || !matches!(decoder.dimensions().1, 32 | 64) {
        return Err(SkinError::InvalidDimensions);
    }
    Ok(DynamicImage::from_decoder(decoder)
        .map_err(|_| SkinError::Decode)?
        .into_rgba8())
}

pub(super) fn decode_head(bytes: &[u8]) -> Result<Vec<u8>, SkinError> {
    let skin = decode_pixels(bytes)?;
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
