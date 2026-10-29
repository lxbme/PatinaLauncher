//! Installing legacy-format Forge (1.6.4 to 1.12.2: `install` + `versionInfo`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::installer::InstallerProfile;
use super::{checked_profile_id, download_libraries, fill_sha1_from_sidecars, write_profile};
use crate::download::Downloader;
use crate::fsutil::safe_join;
use crate::loader::list::LoaderError;
use crate::loader::{InstallPhase, InstalledProfile, LoaderKind, ProgressSink, report};
use crate::meta::{RuleEnv, maven_path};
use crate::paths::DataDirs;
use crate::resolve::resolve_libraries;

/// Installs a legacy Forge profile: extracts the universal jar from the
/// installer into `libraries/`, downloads the other client libraries and, as
/// the very last step, writes `versions/<id>/<id>.json`.
pub async fn install_legacy(
    dl: &Downloader,
    data: &DataDirs,
    jar: &Path,
    profile: InstallerProfile,
    progress: &ProgressSink,
) -> Result<InstalledProfile, LoaderError> {
    // TODO(forge-1.6/1.7.2): FML downloads fmllibs at runtime from a dead URL; launching is best effort (see plan Global Constraints).
    let InstallerProfile::Legacy {
        install_path,
        file_path,
        version,
    } = profile
    else {
        return Err(LoaderError::InstallerInvalid(
            "expected a legacy (versionInfo) install profile".into(),
        ));
    };
    // Legacy installers are Forge-only (NeoForge never shipped one).
    checked_profile_id(LoaderKind::Forge, &version.id)?;
    let libraries = data.libraries();
    let universal = maven_path(&install_path)
        .and_then(|rel| safe_join(&libraries, &rel))
        .ok_or_else(|| LoaderError::UnsafeMetadata(install_path.clone()))?;

    // The universal jar is in place before any download, so its own library
    // entry in `versionInfo` resolves to an existing file and is skipped.
    let (jar_owned, dest) = (jar.to_path_buf(), universal.clone());
    tokio::task::spawn_blocking(move || extract_entry(&jar_owned, &file_path, &dest))
        .await
        .map_err(|e| LoaderError::Io(std::io::Error::other(e)))??;

    let resolved = resolve_libraries(&version.libraries, &RuleEnv::current(), &libraries);
    let mut jobs = resolved.jobs;
    // `checksums` libraries are verified separately (pack.xz handling).
    let checksummed: HashSet<PathBuf> = version
        .libraries
        .iter()
        .filter(|l| !l.checksums.is_empty())
        .filter_map(|l| maven_path(&l.name).and_then(|rel| safe_join(&libraries, &rel)))
        .collect();
    fill_sha1_from_sidecars(dl, &mut jobs, |j| {
        j.sha1.is_none() && j.dest != universal && !checksummed.contains(&j.dest)
    })
    .await;
    download_libraries(dl, jobs, progress).await?;

    report(progress, InstallPhase::Finalize, 0, 1);
    let profile_id = write_profile(data, LoaderKind::Forge, version)?;
    report(progress, InstallPhase::Finalize, 1, 1);
    Ok(InstalledProfile { profile_id })
}

/// Copies jar entry `name` to `dest` through a `.part` file.
fn extract_entry(jar: &Path, name: &str, dest: &Path) -> Result<(), LoaderError> {
    let invalid = |e: &dyn std::fmt::Display| LoaderError::InstallerInvalid(format!("{name}: {e}"));
    let mut zip = zip::ZipArchive::new(std::fs::File::open(jar)?).map_err(|e| invalid(&e))?;
    let mut entry = zip.by_name(name).map_err(|e| invalid(&e))?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let copied = std::fs::File::create(&part).and_then(|mut f| {
        std::io::copy(&mut entry, &mut f)?;
        f.sync_all()
    });
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&part);
        return Err(e.into());
    }
    std::fs::rename(&part, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::forge::read_installer;
    use crate::loader::forge::test_support::{downloader, sha1_hex, write_zip};
    use crate::loader::{InstallPhase, InstallProgress};
    use crate::meta::PartialVersionJson;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PROFILE: &str = include_str!("../../../tests/fixtures/forge_1.7.10_install_profile.json");
    const ID: &str = "1.7.10-Forge10.13.4.1614-1.7.10";
    const UNIVERSAL_FILE: &str = "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar";
    const UNIVERSAL_REL: &str =
        "net/minecraftforge/forge/1.7.10-10.13.4.1614-1.7.10/forge-1.7.10-10.13.4.1614-1.7.10.jar";
    const LAUNCHWRAPPER: &str = "/net/minecraft/launchwrapper/1.12/launchwrapper-1.12.jar";
    const AKKA: &str = "/com/typesafe/akka/akka-actor_2.11/2.3.3/akka-actor_2.11-2.3.3.jar";

    /// The fixture plus the universal jar's own library entry, as real
    /// 1.7.10 profiles have it (legacy files.minecraftforge.net url).
    fn profile_json() -> Value {
        let mut v: Value = serde_json::from_str(PROFILE).unwrap();
        v["versionInfo"]["libraries"]
            .as_array_mut()
            .unwrap()
            .insert(
                0,
                json!({
                    "name": "net.minecraftforge:forge:1.7.10-10.13.4.1614-1.7.10",
                    "url": "http://files.minecraftforge.net/maven/"
                }),
            );
        v
    }

    fn installer(dir: &Path, profile: &Value) -> std::path::PathBuf {
        let jar = dir.join("installer.jar");
        write_zip(
            &jar,
            &[
                ("install_profile.json", profile.to_string().as_bytes()),
                (UNIVERSAL_FILE, b"universal jar bytes"),
            ],
        );
        jar
    }

    async fn serve(server: &MockServer, p: &str, body: &[u8]) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .mount(server)
            .await;
    }

    async fn never(server: &MockServer, re: &str) {
        Mock::given(method("GET"))
            .and(path_regex(re))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"nope".to_vec()))
            .expect(0)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn legacy_install_extracts_universal_and_writes_profile() {
        let server = MockServer::start().await;
        serve(&server, LAUNCHWRAPPER, b"launchwrapper").await;
        serve(
            &server,
            &format!("{LAUNCHWRAPPER}.sha1"),
            format!("{}\n", sha1_hex(b"launchwrapper")).as_bytes(),
        )
        .await;
        serve(&server, AKKA, b"akka").await;
        // akka has `checksums`: no sidecar lookup
        never(&server, r"akka-actor_2\.11-2\.3\.3\.jar\.sha1$").await;
        // clientreq: false
        never(&server, "fake-server-only").await;
        // the universal jar comes from the installer, never from the network
        never(&server, "^/net/minecraftforge/forge/").await;

        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path().join("data"));
        let jar = installer(tmp.path(), &profile_json());
        let profile = read_installer(&jar).unwrap();
        let expected_version = match &profile {
            InstallerProfile::Legacy { version, .. } => version.clone(),
            other => panic!("expected Legacy, got {other:?}"),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = install_legacy(&downloader(&server, false), &data, &jar, profile, &Some(tx))
            .await
            .unwrap();
        assert_eq!(out.profile_id, ID);

        let universal = data.libraries().join(UNIVERSAL_REL);
        assert_eq!(std::fs::read(universal).unwrap(), b"universal jar bytes");
        assert_eq!(
            std::fs::read(data.libraries().join(&LAUNCHWRAPPER[1..])).unwrap(),
            b"launchwrapper"
        );
        assert!(data.libraries().join(&AKKA[1..]).is_file());
        assert!(
            !data
                .libraries()
                .join("net/minecraft/fake-server-only")
                .exists()
        );

        let written = std::fs::read(data.versions().join(ID).join(format!("{ID}.json"))).unwrap();
        let written: PartialVersionJson = serde_json::from_slice(&written).unwrap();
        assert_eq!(written, expected_version);

        let mut last: Option<InstallProgress> = None;
        while let Ok(p) = rx.try_recv() {
            last = Some(p);
        }
        assert_eq!(last.unwrap().phase, InstallPhase::Finalize);
    }

    #[tokio::test]
    async fn legacy_library_sidecar_mismatch_fails_without_profile() {
        let server = MockServer::start().await;
        for prefix in ["", "/fb"] {
            serve(&server, &format!("{prefix}{LAUNCHWRAPPER}"), b"tampered").await;
            serve(
                &server,
                &format!("{prefix}{LAUNCHWRAPPER}.sha1"),
                sha1_hex(b"launchwrapper").as_bytes(),
            )
            .await;
            serve(&server, &format!("{prefix}{AKKA}"), b"akka").await;
        }
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path().join("data"));
        let jar = installer(tmp.path(), &profile_json());
        let profile = read_installer(&jar).unwrap();
        let r = install_legacy(&downloader(&server, true), &data, &jar, profile, &None).await;
        assert!(
            matches!(
                r,
                Err(LoaderError::Download(
                    crate::download::DownloadError::Failed {
                        reason: crate::download::FailReason::HashMismatch { .. },
                        ..
                    }
                ))
            ),
            "{r:?}"
        );
        assert!(!data.versions().join(ID).exists());
    }

    #[tokio::test]
    async fn legacy_rejects_bad_profiles() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path().join("data"));
        let dl = downloader(&server, false);
        let jar = installer(tmp.path(), &profile_json());
        let InstallerProfile::Legacy {
            install_path,
            file_path,
            version,
        } = read_installer(&jar).unwrap()
        else {
            panic!("expected Legacy");
        };
        let legacy = |install_path: &str, file_path: &str, id: &str| {
            let mut version = version.clone();
            version.id = id.to_string();
            InstallerProfile::Legacy {
                install_path: install_path.to_string(),
                file_path: file_path.to_string(),
                version,
            }
        };

        let modern = InstallerProfile::Modern {
            profile: json!({}),
            version: version.clone(),
        };
        let r = install_legacy(&dl, &data, &jar, modern, &None).await;
        assert!(matches!(r, Err(LoaderError::InstallerInvalid(_))), "{r:?}");

        let r = install_legacy(
            &dl,
            &data,
            &jar,
            legacy(&install_path, &file_path, "../evil"),
            &None,
        )
        .await;
        assert!(matches!(r, Err(LoaderError::UnsafeMetadata(_))), "{r:?}");

        let r = install_legacy(&dl, &data, &jar, legacy("a:..:1", &file_path, ID), &None).await;
        assert!(matches!(r, Err(LoaderError::UnsafeMetadata(_))), "{r:?}");

        let r = install_legacy(
            &dl,
            &data,
            &jar,
            legacy(&install_path, "missing.jar", ID),
            &None,
        )
        .await;
        assert!(matches!(r, Err(LoaderError::InstallerInvalid(_))), "{r:?}");

        assert!(!data.versions().exists());
    }
}
