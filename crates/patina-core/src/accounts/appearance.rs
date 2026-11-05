//! Bounded, immutable Java appearance inputs; authentication stays in AccountService.
use super::{error::AuthError, skin};
use image::RgbaImage;
use std::sync::Arc;

mod changes;
pub use changes::{AppearanceChanges, ApplyOutcome, CapeChange, FieldOutcome};
mod preview;
mod profile;
pub use preview::{PreviewInput, PreviewSide, compose_preview};
pub(crate) use profile::{AppearanceIdentity, parse_profile};
pub use profile::{AppearanceProfile, AppearanceSkin, AppearanceTexture, Cape, CapeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkinModel {
    Classic,
    Slim,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AppearanceError {
    #[error("{}", rust_i18n::t!("errors.appearance.too_large"))]
    TooLarge,
    #[error("{}", rust_i18n::t!("errors.appearance.invalid_png"))]
    InvalidPng,
    #[error("{}", rust_i18n::t!("errors.appearance.dimensions"))]
    InvalidDimensions,
    #[error("{}", rust_i18n::t!("errors.appearance.legacy_slim"))]
    LegacySlim,
    #[error("{0}")]
    Auth(AuthError),
}
impl From<skin::SkinError> for AppearanceError {
    fn from(error: skin::SkinError) -> Self {
        match error {
            skin::SkinError::TooLarge => Self::TooLarge,
            skin::SkinError::InvalidDimensions => Self::InvalidDimensions,
            skin::SkinError::InvalidPng | skin::SkinError::Decode => Self::InvalidPng,
        }
    }
}
impl From<AuthError> for AppearanceError {
    fn from(error: AuthError) -> Self {
        Self::Auth(error)
    }
}

/// Original validated bytes are retained separately from the metadata-stripped decoder input.
#[derive(Clone)]
pub struct SkinUpload {
    bytes: Arc<[u8]>,
    pixels: Arc<RgbaImage>,
    model: SkinModel,
}
impl std::fmt::Debug for SkinUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkinUpload")
            .field("model", &self.model)
            .field("legacy", &self.is_legacy())
            .finish_non_exhaustive()
    }
}
impl SkinUpload {
    pub fn from_png(bytes: Vec<u8>, model: SkinModel) -> Result<Self, AppearanceError> {
        let pixels = skin::decode_pixels(&bytes)?;
        if pixels.height() == 32 && model == SkinModel::Slim {
            return Err(AppearanceError::LegacySlim);
        }
        Ok(Self {
            bytes: bytes.into(),
            pixels: Arc::new(pixels),
            model,
        })
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn model(&self) -> SkinModel {
        self.model
    }
    pub fn is_legacy(&self) -> bool {
        self.pixels.height() == 32
    }
    pub fn with_model(&self, model: SkinModel) -> Result<Self, AppearanceError> {
        if self.is_legacy() && model == SkinModel::Slim {
            return Err(AppearanceError::LegacySlim);
        }
        Ok(Self {
            model,
            ..self.clone()
        })
    }
}

/// Validated official 64×32 cape atlas. No arbitrary image dimensions enter composition.
#[derive(Debug, Clone)]
pub struct CapeTexture(Arc<RgbaImage>);
impl CapeTexture {
    pub fn thumbnail(&self) -> RgbaImage {
        image::imageops::crop_imm(&*self.0, 1, 1, 10, 16).to_image()
    }
    pub fn from_png(bytes: &[u8]) -> Result<Self, AppearanceError> {
        let pixels = skin::decode_pixels(bytes)?;
        if pixels.dimensions() != (64, 32) {
            return Err(AppearanceError::InvalidDimensions);
        }
        Ok(Self(Arc::new(pixels)))
    }
}

#[cfg(test)]
#[path = "appearance/preview_tests.rs"]
mod preview_tests;

#[cfg(test)]
#[path = "appearance/profile_tests.rs"]
mod profile_tests;

#[cfg(test)]
#[path = "appearance/transport_tests.rs"]
mod transport_tests;

#[cfg(test)]
#[path = "appearance/write_tests.rs"]
mod write_tests;
