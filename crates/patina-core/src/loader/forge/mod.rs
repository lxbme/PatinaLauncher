//! Forge and NeoForge installers.

pub mod installer;
pub mod legacy;

pub use installer::{
    InstallerProfile, fetch_installer, installer_coordinate, installer_url, read_installer,
};
pub use legacy::install_legacy;

use futures::StreamExt;

use super::list::LoaderError;
use super::{InstallPhase, LoaderKind, ProgressSink, report};
use crate::download::{DownloadJob, Downloader, Progress};
use crate::fsutil::{is_safe_name, safe_join, write_atomic};
use crate::meta::PartialVersionJson;
use crate::paths::DataDirs;

const NEOFORGE_ID_PREFIX: &str = "neoforge-";

/// Id under which a loader profile is stored in `versions/`. NeoForge's
/// 1.20.1 installers (`net.neoforged:forge`) reuse Forge's profile ids (both
/// `1.20.1-forge-47.1.28`, with different content), so NeoForge ids get a
/// `neoforge-` prefix unless they already start with it (`neoforge-21.1.77`).
pub(crate) fn stored_profile_id(kind: LoaderKind, id: &str) -> String {
    match kind {
        LoaderKind::Neoforge if !id.starts_with(NEOFORGE_ID_PREFIX) => {
            format!("{NEOFORGE_ID_PREFIX}{id}")
        }
        _ => id.to_string(),
    }
}

/// [`stored_profile_id`], rejected with `UnsafeMetadata` unless it is a safe
/// single path component.
pub(crate) fn checked_profile_id(kind: LoaderKind, id: &str) -> Result<String, LoaderError> {
    let stored = stored_profile_id(kind, id);
    if is_safe_name(&stored) {
        Ok(stored)
    } else {
        Err(LoaderError::UnsafeMetadata(id.to_string()))
    }
}

/// Writes `version` as `versions/<id>/<id>.json` (atomically; meant as the
/// last install step) with `id` set to its stored id, which is returned.
pub(crate) fn write_profile(
    data: &DataDirs,
    kind: LoaderKind,
    mut version: PartialVersionJson,
) -> Result<String, LoaderError> {
    let id = checked_profile_id(kind, &version.id)?;
    let dir =
        safe_join(&data.versions(), &id).ok_or_else(|| LoaderError::UnsafeMetadata(id.clone()))?;
    version.id = id.clone();
    let bytes = serde_json::to_vec_pretty(&version)?;
    write_atomic(&dir.join(format!("{id}.json")), &bytes)?;
    Ok(id)
}

/// Most `.sha1` sidecars fetched at once.
const SIDECAR_CONCURRENCY: usize = 8;

/// The SHA-1 a `.sha1` sidecar body announces: its first 40 characters after
/// leading whitespace (bodies may be `"<sha1>\n"` or `"<sha1>  <file>"`),
/// lowercased. `None` unless those are 40 hex digits.
pub(crate) fn parse_sha1_sidecar(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?.trim_start();
    let hex = text.get(..40)?;
    let ends = text[40..].chars().next().is_none_or(char::is_whitespace);
    (ends && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hex.to_ascii_lowercase())
}

/// Fetches `url` (a `.sha1` sidecar, already canonical) from the current
/// sources. `None` when it is unavailable or malformed: the caller then
/// downloads without a checksum. There is no developer log sink in
/// patina-core yet, so the miss is not logged.
pub(crate) async fn fetch_sidecar(dl: &Downloader, url: &str) -> Option<String> {
    let body = dl.fetch_bytes(url, None).await.ok()?;
    parse_sha1_sidecar(&body)
}

/// Fills `sha1` from `<url>.sha1` for each job `want` selects (at most
/// [`SIDECAR_CONCURRENCY`] requests in flight).
pub(crate) async fn fill_sha1_from_sidecars(
    dl: &Downloader,
    jobs: &mut [DownloadJob],
    want: impl Fn(&DownloadJob) -> bool,
) {
    let wanted: Vec<(usize, String)> = jobs
        .iter()
        .enumerate()
        .filter(|(_, j)| want(j))
        .map(|(i, j)| (i, format!("{}.sha1", j.url)))
        .collect();
    let found: Vec<(usize, Option<String>)> = futures::stream::iter(wanted)
        .map(|(i, url)| async move { (i, fetch_sidecar(dl, &url).await) })
        .buffer_unordered(SIDECAR_CONCURRENCY)
        .collect()
        .await;
    for (i, sha1) in found {
        if sha1.is_some() {
            jobs[i].sha1 = sha1;
        }
    }
}

/// Runs `jobs`, forwarding progress as [`InstallPhase::DownloadLibraries`].
pub(crate) async fn download_libraries(
    dl: &Downloader,
    jobs: Vec<DownloadJob>,
    progress: &ProgressSink,
) -> Result<(), LoaderError> {
    report(progress, InstallPhase::DownloadLibraries, 0, jobs.len());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
    let run = dl.run(jobs, Some(tx));
    let forward = async {
        while let Some(p) = rx.recv().await {
            report(progress, InstallPhase::DownloadLibraries, p.done, p.total);
        }
    };
    let (res, ()) = tokio::join!(run, forward);
    Ok(res?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn partial(id: &str) -> PartialVersionJson {
        serde_json::from_value(serde_json::json!({"id": id, "inheritsFrom": "1.20.1"})).unwrap()
    }

    #[test]
    fn neoforge_profile_ids_are_prefixed() {
        // Real ids: forge-1.20.1-47.1.28-installer.jar from both mavens has
        // version.json id "1.20.1-forge-47.1.28".
        let shared = "1.20.1-forge-47.1.28";
        assert_eq!(stored_profile_id(LoaderKind::Forge, shared), shared);
        assert_eq!(
            stored_profile_id(LoaderKind::Neoforge, shared),
            "neoforge-1.20.1-forge-47.1.28"
        );
        assert_eq!(
            stored_profile_id(LoaderKind::Neoforge, "neoforge-21.1.77"),
            "neoforge-21.1.77"
        );
        assert!(matches!(
            checked_profile_id(LoaderKind::Neoforge, "../x"),
            Err(LoaderError::UnsafeMetadata(_))
        ));
        assert!(matches!(
            checked_profile_id(LoaderKind::Forge, "a/b"),
            Err(LoaderError::UnsafeMetadata(_))
        ));
    }

    #[test]
    fn forge_and_neoforge_profiles_with_one_id_are_stored_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let shared = "1.20.1-forge-47.1.28";
        let f = write_profile(&data, LoaderKind::Forge, partial(shared)).unwrap();
        let n = write_profile(&data, LoaderKind::Neoforge, partial(shared)).unwrap();
        assert_eq!(f, shared);
        assert_eq!(n, "neoforge-1.20.1-forge-47.1.28");
        for id in [&f, &n] {
            let bytes = std::fs::read(data.versions().join(id).join(format!("{id}.json"))).unwrap();
            let v: PartialVersionJson = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(&v.id, id);
        }
    }

    #[test]
    fn sidecar_bodies() {
        let h = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        assert_eq!(parse_sha1_sidecar(h.as_bytes()).as_deref(), Some(h));
        let padded = format!("  {}\r\n", h.to_uppercase());
        assert_eq!(parse_sha1_sidecar(padded.as_bytes()).as_deref(), Some(h));
        let named = format!("{h}  forge-1.20.1-47.4.28-installer.jar\n");
        assert_eq!(parse_sha1_sidecar(named.as_bytes()).as_deref(), Some(h));
        for bad in [
            "",
            "da39a3ee",
            "zz39a3ee5e6b4b0d3255bfef95601890afd80709",
            "<html>not found</html> padded to be longer than forty characters",
            "da39a3ee5e6b4b0d3255bfef95601890afd807090",
        ] {
            assert_eq!(parse_sha1_sidecar(bad.as_bytes()), None, "{bad:?}");
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::Write;
    use std::net::SocketAddr;
    use std::path::Path;
    use std::time::Duration;

    use wiremock::MockServer;

    use crate::download::{Downloader, Mirror};

    pub const OFFICIAL_HOSTS: [&str; 4] = [
        "https://maven.minecraftforge.net",
        "https://files.minecraftforge.net",
        "https://maven.neoforged.net/releases",
        "https://libraries.minecraft.net",
    ];

    /// A client that can never reach the real official hosts: they resolve
    /// to a closed local port, so the "original URL" candidate fails fast.
    fn offline_client() -> reqwest::Client {
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut b = reqwest::Client::builder().no_proxy();
        for host in [
            "maven.minecraftforge.net",
            "files.minecraftforge.net",
            "maven.neoforged.net",
            "libraries.minecraft.net",
        ] {
            b = b.resolve(host, dead);
        }
        b.build().unwrap()
    }

    fn mirror(id: &str, base: String) -> Mirror {
        Mirror {
            id: id.into(),
            rules: OFFICIAL_HOSTS
                .iter()
                .map(|h| (h.to_string(), base.clone()))
                .collect(),
        }
    }

    /// Official hosts map to `server`; with `fallback` the fallback mirror
    /// maps them to `server/fb`.
    pub fn downloader(server: &MockServer, fallback: bool) -> Downloader {
        Downloader::new(offline_client(), mirror("fake", server.uri()))
            .with_fallback(fallback.then(|| mirror("fake-fb", format!("{}/fb", server.uri()))))
            .with_retry(1, Duration::ZERO)
    }

    pub fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap();
    }

    pub fn sha1_hex(body: &[u8]) -> String {
        use sha1::{Digest, Sha1};
        hex::encode(Sha1::digest(body))
    }
}
