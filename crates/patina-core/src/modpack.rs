//! Modrinth packs use author manifests, never the single-mod dependency planner.
mod apply;
mod archive;
mod model;
mod service;
mod state;
mod updates;
pub use archive::{ArchiveLimits, inspect_archive, inspect_bytes};
pub use model::{PackFile, PackPreview};
pub use service::{PackInstallChoices, PackOutcome, PackService};
pub use state::{BaselineFile, PackState, ensure_ready, load_state};
pub use updates::{
    PackChange, PackChangeKind, PackConflictReason, PackDecision, PackDecisions, PackUpdatePlan,
};

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("{}", rust_i18n::t!("modpack.invalid"))]
    Invalid,
    #[error("{}", rust_i18n::t!("modpack.invalid_state"))]
    InvalidState,
    #[error("{}", rust_i18n::t!("modpack.stale_plan"))]
    StalePlan,
    #[error("{}", rust_i18n::t!("modpack.no_baseline"))]
    NoBaseline,
    #[error("{}", rust_i18n::t!("modpack.source_changed"))]
    SourceChanged,
    #[error("{}", rust_i18n::t!("modpack.recovery", path = .path.display(), error = .error))]
    Recovery {
        path: std::path::PathBuf,
        error: String,
    },
    #[error("{}", rust_i18n::t!("modpack.not_ready"))]
    NotReady,
    #[error("{}", rust_i18n::t!("modpack.cancelled"))]
    Cancelled,
    #[error("{}", rust_i18n::t!("modpack.verification"))]
    Verification,
    #[error("{}", rust_i18n::t!("modpack.file_download", path = .path, error = .error))]
    FileDownload { path: String, error: String },
    #[error("{}", rust_i18n::t!("modpack.name_conflict"))]
    NameConflict,
    #[error("{}", rust_i18n::t!("modpack.collision", path = .0))]
    Collision(String),
    #[error(transparent)]
    Instance(#[from] crate::instance::InstanceError),
    #[error(transparent)]
    Busy(#[from] crate::locks::Busy),
    #[error(transparent)]
    Prepare(#[from] crate::prepare::PrepareError),
    #[error(transparent)]
    Loader(#[from] crate::loader::LoaderError),
    #[error(transparent)]
    Content(#[from] crate::content::online::ContentError),
    #[error("{}", rust_i18n::t!("modpack.unsafe_path", path = .0))]
    UnsafePath(String),
    #[error("{}", rust_i18n::t!("modpack.unsupported"))]
    Unsupported,
    #[error("{}", rust_i18n::t!("modpack.limit"))]
    Limit,
    #[error("{}", rust_i18n::t!("modpack.io", error = .0))]
    Io(String),
}
impl From<std::io::Error> for PackError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
impl From<zip::result::ZipError> for PackError {
    fn from(_: zip::result::ZipError) -> Self {
        Self::Invalid
    }
}
impl From<serde_json::Error> for PackError {
    fn from(_: serde_json::Error) -> Self {
        Self::Invalid
    }
}
