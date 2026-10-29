//! Recognising a loader version that is already completely installed in the
//! shared data dirs, so another instance can use it without reinstalling.

use futures::StreamExt;

use super::checked_profile_id;
use super::installer::{InstallerProfile, cached_installer, read_installer};
use super::modern::GENERATED_FILE;
use crate::download::Downloader;
use crate::fsutil::safe_join;
use crate::loader::list::LoaderVersion;
use crate::loader::{InstalledProfile, LoaderKind};
use crate::meta::{PartialVersionJson, RuleEnv};
use crate::paths::DataDirs;
use crate::resolve::resolve_libraries;

/// Most library files checked at once.
const CHECK_CONCURRENCY: usize = 8;

/// Whether loader profile `id` is completely installed:
/// - `versions/<id>/<id>.json` parses and has that id;
/// - its [`GENERATED_FILE`] (required when `modern`, checked whenever
///   present) parses and every file it lists exists;
/// - every library of the profile is in place: downloaded ones pass the
///   Downloader's existing-file check (SHA-1 when known), and `url: ""` ones
///   exist.
///
/// Reads only local files; nothing is modified.
pub(crate) async fn is_installed(data: &DataDirs, id: &str, modern: bool) -> bool {
    let Some(dir) = safe_join(&data.versions(), id) else {
        return false;
    };
    let Ok(bytes) = tokio::fs::read(dir.join(format!("{id}.json"))).await else {
        return false;
    };
    let Ok(profile) = serde_json::from_slice::<PartialVersionJson>(&bytes) else {
        return false;
    };
    if profile.id != id {
        return false;
    }
    let libraries = data.libraries();
    match tokio::fs::read(dir.join(GENERATED_FILE)).await {
        Ok(b) => {
            let Ok(rels) = serde_json::from_slice::<Vec<String>>(&b) else {
                return false;
            };
            let all = rels
                .iter()
                .all(|rel| safe_join(&libraries, rel).is_some_and(|p| p.is_file()));
            if !all {
                return false;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !modern => {}
        Err(_) => return false,
    }
    let resolved = resolve_libraries(&profile.libraries, &RuleEnv::current(), &libraries);
    if resolved.generated.iter().any(|p| !p.is_file()) {
        return false;
    }
    futures::stream::iter(resolved.jobs)
        .map(|job| async move { Downloader::existing_ok(&job).await })
        .buffer_unordered(CHECK_CONCURRENCY)
        .all(|ok| async move { ok })
        .await
}

/// The Forge / NeoForge version `v` (for `mc`) when it is already completely
/// installed (see [`is_installed`]). Its stored profile id comes from the
/// installer jar, so this needs the cached installer with a matching
/// recorded SHA-1 (see [`cached_installer`]); without one it is `None` and
/// the caller installs as usual. No network access.
pub(crate) async fn installed_forge(
    data: &DataDirs,
    kind: LoaderKind,
    mc: &str,
    v: &LoaderVersion,
) -> Option<InstalledProfile> {
    let (data_owned, mc_owned, v_owned) = (data.clone(), mc.to_string(), v.clone());
    let profile = tokio::task::spawn_blocking(move || {
        let jar = cached_installer(&data_owned, kind, &mc_owned, &v_owned)?;
        read_installer(&jar).ok()
    })
    .await
    .ok()??;
    let (version, modern) = match profile {
        InstallerProfile::Modern { version, .. } => (version, true),
        // Legacy installers are Forge-only.
        InstallerProfile::Legacy { version, .. } if kind == LoaderKind::Forge => (version, false),
        InstallerProfile::Legacy { .. } => return None,
    };
    let id = checked_profile_id(kind, &version.id).ok()?;
    is_installed(data, &id, modern)
        .await
        .then_some(InstalledProfile { profile_id: id })
}
