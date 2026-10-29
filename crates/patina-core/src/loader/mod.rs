//! Mod loader support (Forge, NeoForge, Fabric).

pub mod fabric;
pub mod forge;
pub mod list;
pub mod ops;

use serde::{Deserialize, Serialize};

pub use crate::progress::{Phase, ProgressSink, TaskProgress, Unit};

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
