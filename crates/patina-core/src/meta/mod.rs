//! Mojang version metadata: manifest, version JSON, rules, maven coordinates.

pub mod manifest;
pub mod maven;
pub mod merge;
pub mod rules;
pub mod version;

pub use manifest::{Latest, MANIFEST_URL, ManifestEntry, VersionKind, VersionManifest};
pub use maven::maven_path;
pub use merge::{library_key, merge_versions};
pub use rules::{OsRule, Rule, RuleAction, RuleEnv, rules_allow};
pub use version::{
    ArgValue, Arguments, AssetIndexRef, Extract, FileRef, JavaVersionReq, Library,
    LibraryDownloads, Logging, LoggingClient, LoggingFile, OneOrMany, PartialVersionJson,
    VersionDownloads, VersionJson,
};
