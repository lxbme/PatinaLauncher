use super::{AppearanceError, AppearanceProfile, CapeId, SkinUpload};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum CapeChange {
    #[default]
    Keep,
    Disable,
    Select(CapeId),
}
#[derive(Debug, Clone)]
pub struct AppearanceChanges {
    pub(crate) baseline: AppearanceProfile,
    pub(crate) skin: Option<SkinUpload>,
    pub(crate) cape: CapeChange,
}
impl AppearanceChanges {
    pub fn new(baseline: &AppearanceProfile) -> Self {
        Self {
            baseline: baseline.clone(),
            skin: None,
            cape: CapeChange::Keep,
        }
    }
    pub fn with_skin(mut self, skin: SkinUpload) -> Self {
        self.skin = Some(skin);
        self
    }
    pub fn with_cape(mut self, cape: CapeChange) -> Self {
        self.cape = cape;
        self
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldOutcome {
    NotRequested,
    Confirmed,
    Failed(AppearanceError),
    Uncertain(AppearanceError),
}
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub(crate) changes: AppearanceChanges,
    pub(crate) skin: FieldOutcome,
    pub(crate) cape: FieldOutcome,
    pub(crate) profile: Option<AppearanceProfile>,
    pub(crate) refresh_error: Option<AppearanceError>,
}
impl ApplyOutcome {
    pub fn skin(&self) -> &FieldOutcome {
        &self.skin
    }
    pub fn cape(&self) -> &FieldOutcome {
        &self.cape
    }
    pub fn profile(&self) -> Option<&AppearanceProfile> {
        self.profile.as_ref()
    }
    pub fn refresh_error(&self) -> Option<&AppearanceError> {
        self.refresh_error.as_ref()
    }
    pub fn is_uncertain(&self) -> bool {
        matches!(self.skin, FieldOutcome::Uncertain(_))
            || matches!(self.cape, FieldOutcome::Uncertain(_))
    }
}
