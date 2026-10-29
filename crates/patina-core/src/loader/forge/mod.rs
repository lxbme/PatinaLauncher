//! Forge and NeoForge installers.

pub mod installed;
pub mod installer;
pub mod legacy;
pub mod modern;
pub mod packxz;
pub mod processors;

pub use installer::{
    InstallerProfile, fetch_installer, installer_coordinate, installer_url, read_installer,
};
pub use legacy::install_legacy;
pub use modern::{GENERATED_FILE, install_modern};
pub use processors::{Processor, ProcessorEnv, expand_arg, resolve_data, run_processors};

use futures::StreamExt;

use super::LoaderKind;
use super::list::LoaderError;
use crate::download::{DownloadJob, Downloader};
use crate::fsutil::{is_safe_name, safe_join, write_atomic};
use crate::meta::PartialVersionJson;
use crate::paths::DataDirs;
use crate::progress::{Phase, ProgressSink, Unit, run_downloads};

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
/// downloads without a checksum. The miss is logged at debug level.
pub(crate) async fn fetch_sidecar(dl: &Downloader, url: &str) -> Option<String> {
    let body = match dl.fetch_bytes(url, None).await {
        Ok(body) => body,
        Err(e) => {
            tracing::debug!(url, error = ?e, "sha1 sidecar unavailable");
            return None;
        }
    };
    let parsed = parse_sha1_sidecar(&body);
    if parsed.is_none() {
        tracing::debug!(url, "sha1 sidecar malformed");
    }
    parsed
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

/// Runs `jobs`, forwarding progress as [`Phase::DownloadLibraries`].
/// `extra_items` is later work counted in the total from the start.
pub(crate) async fn download_libraries(
    dl: &Downloader,
    jobs: Vec<DownloadJob>,
    progress: &ProgressSink,
    extra_items: u64,
) -> Result<(), LoaderError> {
    Ok(run_downloads(
        dl,
        jobs,
        progress,
        Phase::DownloadLibraries,
        Unit::Items,
        extra_items,
    )
    .await?)
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

    pub const OFFICIAL_HOSTS: [&str; 6] = [
        "https://maven.minecraftforge.net",
        "https://files.minecraftforge.net",
        "https://maven.neoforged.net/releases",
        "https://libraries.minecraft.net",
        "https://piston-meta.mojang.com",
        "https://piston-data.mojang.com",
    ];

    /// A real `.pack.xz` (made with jre-legacy's `pack200`, Forge's signed
    /// layout, `xz -C crc64`) of a jar holding [`PACKED_ENTRIES`].
    pub const PACK_XZ: &[u8] =
        include_bytes!("../../../tests/fixtures/forge_legacy_lib.jar.pack.xz");
    pub const PACKED_ENTRIES: [(&str, &[u8]); 2] = [
        ("patina/hello.txt", b"hello from a packed library\n"),
        ("patina/data.txt", b"pack200 fixture\n"),
    ];
    /// SHA-1 of the fixture's signature block (its `checksums.sha1`), the
    /// value a library's `checksums` lists for the unpacked jar.
    pub const PACK_SIG_SHA1: &str = "a56262c317a1770746bfba9ce4001a0ebd844e90";
    /// SHA-1 of the fixture's pack200 data with the signature trailer cut.
    pub const PACK_SHA1: &str = "3b4099f9edaac04db6f1fe1270a1bbe1268f11a0";

    /// Fabric's official hosts (metadata and maven).
    pub const FABRIC_HOSTS: [&str; 2] = ["https://meta.fabricmc.net", "https://maven.fabricmc.net"];

    /// A client that can never reach the real `hosts` (`https://host[/path]`):
    /// they resolve to a closed local port and no proxy is used, so an
    /// "original URL" candidate fails fast instead of reaching the network.
    pub fn dead_hosts_client(hosts: &[&str]) -> reqwest::Client {
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut b = reqwest::Client::builder().no_proxy();
        for h in hosts {
            let host = h.trim_start_matches("https://").split('/').next().unwrap();
            b = b.resolve(host, dead);
        }
        b.build().unwrap()
    }

    /// [`dead_hosts_client`] for every official loader, Mojang and Fabric host.
    pub fn offline_client() -> reqwest::Client {
        let hosts: Vec<&str> = OFFICIAL_HOSTS
            .iter()
            .chain(&FABRIC_HOSTS)
            .copied()
            .collect();
        dead_hosts_client(&hosts)
    }

    /// Serves a managed `jre-legacy` (runtime index, manifest and a
    /// `bin/unpack200` with body `unpack200`) through [`downloader`]'s
    /// mirror; the index is expected `index_hits` times.
    pub async fn serve_jre_legacy(server: &MockServer, unpack200: &[u8], index_hits: u64) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let bin_sha1 = sha1_hex(unpack200);
        let manifest = serde_json::json!({"files": {
            "bin": {"type": "directory"},
            "bin/unpack200": {"type": "file", "executable": true, "downloads": {"raw": {
                "url": format!("https://piston-data.mojang.com/v1/objects/{bin_sha1}/unpack200"),
                "sha1": bin_sha1,
                "size": unpack200.len(),
            }}},
        }})
        .to_string();
        let mut index = serde_json::Map::new();
        index.insert(
            crate::java::runtime::platform_key().unwrap().to_string(),
            serde_json::json!({"jre-legacy": [{
                "manifest": {
                    "url": "https://piston-meta.mojang.com/v1/packages/fake/manifest.json",
                    "sha1": sha1_hex(manifest.as_bytes()),
                    "size": manifest.len(),
                },
                "version": {"name": "8u202"},
            }]}),
        );
        let index = serde_json::Value::Object(index).to_string();
        let index_path = crate::java::RUNTIME_INDEX_URL
            .strip_prefix("https://piston-meta.mojang.com")
            .unwrap();
        let bodies: [(String, Vec<u8>); 3] = [
            (index_path.to_string(), index.into_bytes()),
            (
                "/v1/packages/fake/manifest.json".into(),
                manifest.into_bytes(),
            ),
            (
                format!("/v1/objects/{bin_sha1}/unpack200"),
                unpack200.to_vec(),
            ),
        ];
        for (i, (p, body)) in bodies.into_iter().enumerate() {
            let mock = Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body));
            let mock = if i == 0 {
                mock.expect(index_hits)
            } else {
                mock
            };
            mock.mount(server).await;
        }
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
