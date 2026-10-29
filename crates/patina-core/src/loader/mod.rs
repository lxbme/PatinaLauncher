//! Mod loader support (Forge, NeoForge, Fabric).

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
