//! Resolve libraries, assets and natives into classpath entries and download jobs.

pub mod assets;
pub mod libraries;
pub mod natives;

pub use assets::{
    AssetIndex, AssetLayout, AssetObject, RESOURCES_URL, asset_jobs, asset_layout,
    materialize_legacy,
};
pub use libraries::{LIBRARIES_URL, NativeJar, ResolvedLibraries, resolve_libraries};
pub use natives::extract_natives;
