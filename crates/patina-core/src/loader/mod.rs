//! Mod loader support (Forge, NeoForge, Fabric).

pub mod fabric;
pub mod forge;
pub mod list;

use serde::{Deserialize, Serialize};

pub use list::{
    LoaderError, LoaderVersion, VersionPick, list_versions, parse_fabric_list, parse_forge,
    parse_neoforge, parse_neoforge_legacy,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoaderKind {
    Forge,
    Neoforge,
    Fabric,
}

impl LoaderKind {
    pub fn display_name(self) -> String {
        match self {
            LoaderKind::Forge => rust_i18n::t!("loader.forge").to_string(),
            LoaderKind::Neoforge => rust_i18n::t!("loader.neoforge").to_string(),
            LoaderKind::Fabric => rust_i18n::t!("loader.fabric").to_string(),
        }
    }
}

/// Result of a successful loader install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledProfile {
    /// Id of the loader version JSON (`Component.profile`).
    pub profile_id: String,
}

/// Receives install progress; `None` discards it.
pub type ProgressSink = Option<tokio::sync::mpsc::UnboundedSender<InstallProgress>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    FetchMetadata,
    DownloadInstaller,
    DownloadLibraries,
    RunProcessor,
    Finalize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    pub phase: InstallPhase,
    pub done: usize,
    pub total: usize,
}

pub(crate) fn report(sink: &ProgressSink, phase: InstallPhase, done: usize, total: usize) {
    if let Some(tx) = sink {
        let _ = tx.send(InstallProgress { phase, done, total });
    }
}
