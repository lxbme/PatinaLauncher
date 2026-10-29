//! Locating, fetching and reading Forge / NeoForge installer jars.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;
use zip::ZipArchive;
use zip::result::ZipError;

use super::fetch_sidecar;
use crate::download::mirror::canonical_url;
use crate::download::{DownloadJob, Downloader};
use crate::fsutil::is_safe_name;
use crate::loader::list::{LoaderError, LoaderVersion};
use crate::loader::{InstallPhase, LoaderKind, ProgressSink, report};
use crate::meta::{PartialVersionJson, maven_path};
use crate::paths::DataDirs;

/// What `install_profile.json` describes.
#[derive(Debug, Clone, PartialEq)]
pub enum InstallerProfile {
    /// Forge 1.13+ / NeoForge (and late 1.12.2): processors + `version.json`.
    Modern {
        profile: serde_json::Value,
        version: PartialVersionJson,
    },
    /// Forge 1.6.4 to 1.12.2: `install` + `versionInfo`.
    Legacy {
        install_path: String,
        file_path: String,
        version: PartialVersionJson,
    },
}

const FORGE_MAVEN: &str = "https://maven.minecraftforge.net/";
const NEOFORGE_MAVEN: &str = "https://maven.neoforged.net/releases/";

/// Maven coordinate of the installer jar. `mc` and `v.maven_version` must be
/// safe (see [`fetch_installer`]).
pub fn installer_coordinate(kind: LoaderKind, mc: &str, v: &LoaderVersion) -> String {
    let maven = &v.maven_version;
    match kind {
        LoaderKind::Neoforge if mc == "1.20.1" => {
            format!("net.neoforged:forge:{maven}:installer")
        }
        LoaderKind::Neoforge => format!("net.neoforged:neoforge:{maven}:installer"),
        // Fabric has no installer jar; Forge is the only other kind.
        LoaderKind::Forge | LoaderKind::Fabric => {
            format!("net.minecraftforge:forge:{maven}:installer")
        }
    }
}

/// Download URL of `coord` on the loader's official maven. `coord` must be a
/// valid coordinate (as built by [`installer_coordinate`]); an invalid one
/// yields the bare repository URL.
pub fn installer_url(kind: LoaderKind, coord: &str) -> String {
    let base = match kind {
        LoaderKind::Neoforge => NEOFORGE_MAVEN,
        LoaderKind::Forge | LoaderKind::Fabric => FORGE_MAVEN,
    };
    format!("{base}{}", maven_path(coord).unwrap_or_default())
}

/// A version string that is safe inside a maven coordinate, a URL path and a
/// file name: one path component, no `?`, `#`, `%`, `:`, `@`, whitespace or
/// control characters.
fn is_safe_version(s: &str) -> bool {
    is_safe_name(s)
        && !s.contains(['?', '#', '%', ':', '@'])
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Downloads the installer jar to `cache/installers/<kind>/<file>`, verified by the
/// `.sha1` sidecar when one is available (an unavailable sidecar is not an
/// error). An already cached jar that matches is reused.
pub async fn fetch_installer(
    dl: &Downloader,
    data: &DataDirs,
    kind: LoaderKind,
    mc: &str,
    v: &LoaderVersion,
    progress: &ProgressSink,
) -> Result<PathBuf, LoaderError> {
    if kind == LoaderKind::Fabric {
        return Err(invalid("Fabric has no installer jar"));
    }
    for s in [mc, v.maven_version.as_str()] {
        if !is_safe_version(s) {
            return Err(LoaderError::UnsafeMetadata(s.to_string()));
        }
    }
    let coord = installer_coordinate(kind, mc, v);
    let file_name = maven_path(&coord)
        .and_then(|rel| rel.rsplit('/').next().map(str::to_string))
        .filter(|f| is_safe_name(f))
        .ok_or_else(|| LoaderError::UnsafeMetadata(coord.clone()))?;
    let url = canonical_url(&installer_url(kind, &coord));
    // Forge and NeoForge 1.20.1 installers share file names but not bytes.
    let dest = data
        .cache()
        .join("installers")
        .join(kind_dir(kind))
        .join(file_name);

    report(progress, InstallPhase::DownloadInstaller, 0, 1);
    let sha1 = fetch_sidecar(dl, &canonical_url(&format!("{url}.sha1"))).await;
    let job = DownloadJob {
        url,
        dest: dest.clone(),
        sha1,
        size: None,
    };
    dl.run(vec![job], None).await?;
    report(progress, InstallPhase::DownloadInstaller, 1, 1);
    Ok(dest)
}

fn kind_dir(kind: LoaderKind) -> &'static str {
    match kind {
        LoaderKind::Forge => "forge",
        LoaderKind::Neoforge => "neoforge",
        LoaderKind::Fabric => "fabric",
    }
}

fn invalid(msg: impl std::fmt::Display) -> LoaderError {
    LoaderError::InstallerInvalid(msg.to_string())
}

fn read_entry(zip: &mut ZipArchive<File>, name: &str) -> Result<Vec<u8>, LoaderError> {
    let mut entry = match zip.by_name(name) {
        Ok(e) => e,
        Err(ZipError::FileNotFound) => return Err(invalid(format!("missing {name}"))),
        Err(e) => return Err(invalid(format!("{name}: {e}"))),
    };
    let mut out = Vec::new();
    entry.read_to_end(&mut out)?;
    Ok(out)
}

/// Reads `install_profile.json` from an installer jar. A `versionInfo` field
/// means the legacy format; otherwise the version JSON is the jar entry named
/// by the profile's `json` field (default `/version.json`).
pub fn read_installer(jar: &Path) -> Result<InstallerProfile, LoaderError> {
    let mut zip = ZipArchive::new(File::open(jar)?).map_err(invalid)?;
    let profile: Value = serde_json::from_slice(&read_entry(&mut zip, "install_profile.json")?)?;
    if let Some(info) = profile.get("versionInfo") {
        let install_str = |key: &str| {
            profile
                .get("install")
                .and_then(|i| i.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| invalid(format!("install_profile.json lacks install.{key}")))
        };
        return Ok(InstallerProfile::Legacy {
            install_path: install_str("path")?,
            file_path: install_str("filePath")?,
            version: serde_json::from_value(info.clone())?,
        });
    }
    let name = profile
        .get("json")
        .and_then(Value::as_str)
        .unwrap_or("/version.json");
    let version = serde_json::from_slice(&read_entry(&mut zip, name.trim_start_matches('/'))?)?;
    Ok(InstallerProfile::Modern { profile, version })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::{DownloadError, FailReason};
    use crate::loader::forge::test_support::{downloader, sha1_hex, write_zip};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const LEGACY_PROFILE: &str =
        include_str!("../../../tests/fixtures/forge_1.7.10_install_profile.json");

    fn lv(kind: LoaderKind, mc: &str, maven: &str) -> LoaderVersion {
        LoaderVersion {
            kind,
            mc: mc.into(),
            version: maven.into(),
            maven_version: maven.into(),
            stable: true,
            recommended: false,
            latest: false,
        }
    }

    #[test]
    fn coordinates_and_urls() {
        let f = lv(LoaderKind::Forge, "1.20.1", "1.20.1-47.4.28");
        let c = installer_coordinate(LoaderKind::Forge, "1.20.1", &f);
        assert_eq!(c, "net.minecraftforge:forge:1.20.1-47.4.28:installer");
        assert_eq!(
            installer_url(LoaderKind::Forge, &c),
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.20.1-47.4.28/forge-1.20.1-47.4.28-installer.jar"
        );

        let n = lv(LoaderKind::Neoforge, "1.20.1", "1.20.1-47.1.106");
        let c = installer_coordinate(LoaderKind::Neoforge, "1.20.1", &n);
        assert_eq!(c, "net.neoforged:forge:1.20.1-47.1.106:installer");
        assert_eq!(
            installer_url(LoaderKind::Neoforge, &c),
            "https://maven.neoforged.net/releases/net/neoforged/forge/1.20.1-47.1.106/forge-1.20.1-47.1.106-installer.jar"
        );

        let n = lv(LoaderKind::Neoforge, "1.21.1", "21.1.77");
        let c = installer_coordinate(LoaderKind::Neoforge, "1.21.1", &n);
        assert_eq!(c, "net.neoforged:neoforge:21.1.77:installer");
        assert_eq!(
            installer_url(LoaderKind::Neoforge, &c),
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.77/neoforge-21.1.77-installer.jar"
        );
    }

    #[test]
    fn detects_legacy_and_modern_profiles() {
        let dir = tempfile::tempdir().unwrap();

        let legacy = dir.path().join("legacy.jar");
        write_zip(
            &legacy,
            &[("install_profile.json", LEGACY_PROFILE.as_bytes())],
        );
        match read_installer(&legacy).unwrap() {
            InstallerProfile::Legacy {
                install_path,
                file_path,
                version,
            } => {
                assert_eq!(
                    install_path,
                    "net.minecraftforge:forge:1.7.10-10.13.4.1614-1.7.10"
                );
                assert_eq!(file_path, "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar");
                assert_eq!(version.id, "1.7.10-Forge10.13.4.1614-1.7.10");
                assert_eq!(version.libraries.len(), 3);
            }
            other => panic!("expected Legacy, got {other:?}"),
        }

        let profile = json!({
            "spec": 1,
            "minecraft": "1.20.1",
            "json": "/version.json",
            "processors": [{"jar": "a:b:1", "classpath": [], "args": []}],
            "libraries": [],
            "data": {}
        });
        let version =
            json!({"id": "1.20.1-forge-47.4.28", "inheritsFrom": "1.20.1", "libraries": []});
        let modern = dir.path().join("modern.jar");
        write_zip(
            &modern,
            &[
                ("install_profile.json", profile.to_string().as_bytes()),
                ("version.json", version.to_string().as_bytes()),
            ],
        );
        match read_installer(&modern).unwrap() {
            InstallerProfile::Modern {
                profile: p,
                version,
            } => {
                assert_eq!(p, profile);
                assert_eq!(version.id, "1.20.1-forge-47.4.28");
                assert_eq!(version.inherits_from.as_deref(), Some("1.20.1"));
            }
            other => panic!("expected Modern, got {other:?}"),
        }

        // `json` absent → defaults to /version.json
        let mut no_json = profile.clone();
        no_json.as_object_mut().unwrap().remove("json");
        let p = dir.path().join("nojson.jar");
        write_zip(
            &p,
            &[
                ("install_profile.json", no_json.to_string().as_bytes()),
                ("version.json", version.to_string().as_bytes()),
            ],
        );
        assert!(matches!(
            read_installer(&p).unwrap(),
            InstallerProfile::Modern { .. }
        ));

        let empty = dir.path().join("empty.jar");
        write_zip(&empty, &[("other.txt", b"x")]);
        assert!(matches!(
            read_installer(&empty),
            Err(LoaderError::InstallerInvalid(_))
        ));
    }

    const FORGE_PATH: &str =
        "/net/minecraftforge/forge/1.20.1-47.4.28/forge-1.20.1-47.4.28-installer.jar";

    async fn serve(server: &MockServer, p: &str, body: &[u8]) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn fetch_installer_uses_sidecar_and_cache_dir() {
        let server = MockServer::start().await;
        let body = b"installer bytes".to_vec();
        serve(&server, FORGE_PATH, &body).await;
        let sidecar = format!(
            "{}  forge-1.20.1-47.4.28-installer.jar\n",
            sha1_hex(&body).to_uppercase()
        );
        serve(&server, &format!("{FORGE_PATH}.sha1"), sidecar.as_bytes()).await;
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let v = lv(LoaderKind::Forge, "1.20.1", "1.20.1-47.4.28");
        let jar = fetch_installer(
            &downloader(&server, false),
            &data,
            LoaderKind::Forge,
            "1.20.1",
            &v,
            &None,
        )
        .await
        .unwrap();
        assert_eq!(
            jar,
            data.cache()
                .join("installers/forge/forge-1.20.1-47.4.28-installer.jar")
        );
        assert_eq!(std::fs::read(&jar).unwrap(), body);
    }

    #[tokio::test]
    async fn fetch_installer_without_sidecar_still_downloads() {
        let server = MockServer::start().await;
        serve(&server, FORGE_PATH, b"installer bytes").await;
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let v = lv(LoaderKind::Forge, "1.20.1", "1.20.1-47.4.28");
        let jar = fetch_installer(
            &downloader(&server, false),
            &data,
            LoaderKind::Forge,
            "1.20.1",
            &v,
            &None,
        )
        .await
        .unwrap();
        assert!(jar.is_file());
    }

    #[tokio::test]
    async fn installer_sha1_sidecar_is_verified() {
        let server = MockServer::start().await;
        let sidecar = sha1_hex(b"the real installer");
        for prefix in ["", "/fb"] {
            serve(&server, &format!("{prefix}{FORGE_PATH}"), b"tampered").await;
            serve(
                &server,
                &format!("{prefix}{FORGE_PATH}.sha1"),
                sidecar.as_bytes(),
            )
            .await;
        }
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let v = lv(LoaderKind::Forge, "1.20.1", "1.20.1-47.4.28");
        let r = fetch_installer(
            &downloader(&server, true),
            &data,
            LoaderKind::Forge,
            "1.20.1",
            &v,
            &None,
        )
        .await;
        match r {
            Err(LoaderError::Download(DownloadError::Failed {
                reason: FailReason::HashMismatch { expected, .. },
                ..
            })) => assert_eq!(expected, sidecar),
            other => panic!("expected a hash mismatch, got {other:?}"),
        }
        assert!(
            !data
                .cache()
                .join("installers/forge/forge-1.20.1-47.4.28-installer.jar")
                .exists()
        );
    }

    #[tokio::test]
    async fn unsafe_versions_are_rejected_before_any_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let dl = downloader(&server, false);
        for (mc, maven) in [
            ("1.20.1", "1.20.1-47.4.28/../../x"),
            ("1.20.1", "47?x=1"),
            ("1.20.1", "47#frag"),
            ("1.20.1", ".."),
            ("1.20.1", "a:b"),
            ("1.20/1", "1.20.1-47.4.28"),
            ("1.20.1?", "1.20.1-47.4.28"),
            ("", "1.20.1-47.4.28"),
        ] {
            let v = lv(LoaderKind::Forge, mc, maven);
            let r = fetch_installer(&dl, &data, LoaderKind::Forge, mc, &v, &None).await;
            assert!(
                matches!(r, Err(LoaderError::UnsafeMetadata(_))),
                "{mc:?} {maven:?}: {r:?}"
            );
        }
    }

    #[tokio::test]
    async fn forge_and_neoforge_installers_do_not_share_a_cache_file() {
        let server = MockServer::start().await;
        // Same file name on both mavens, different bytes, no `.sha1` served.
        let file = "forge-1.20.1-47.1.28-installer.jar";
        serve(
            &server,
            &format!("/net/minecraftforge/forge/1.20.1-47.1.28/{file}"),
            b"forge installer",
        )
        .await;
        serve(
            &server,
            &format!("/net/neoforged/forge/1.20.1-47.1.28/{file}"),
            b"neoforge installer",
        )
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let dl = downloader(&server, false);
        let f = lv(LoaderKind::Forge, "1.20.1", "1.20.1-47.1.28");
        let n = lv(LoaderKind::Neoforge, "1.20.1", "1.20.1-47.1.28");
        let fj = fetch_installer(&dl, &data, LoaderKind::Forge, "1.20.1", &f, &None)
            .await
            .unwrap();
        let nj = fetch_installer(&dl, &data, LoaderKind::Neoforge, "1.20.1", &n, &None)
            .await
            .unwrap();
        assert_ne!(fj, nj);
        assert_eq!(std::fs::read(&fj).unwrap(), b"forge installer");
        assert_eq!(std::fs::read(&nj).unwrap(), b"neoforge installer");
    }
}
