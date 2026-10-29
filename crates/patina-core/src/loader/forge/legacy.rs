//! Installing legacy-format Forge (1.6.4 to 1.12.2: `install` + `versionInfo`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use futures::{StreamExt, TryStreamExt};

use super::installer::InstallerProfile;
use super::packxz::{Unpack200, ensure_legacy_library};
use super::{checked_profile_id, download_libraries, fill_sha1_from_sidecars, write_profile};
use crate::download::{DownloadJob, Downloader};
use crate::fsutil::{replace_atomic_part, safe_join};
use crate::loader::list::{InstallerProblem, LoaderError};
use crate::loader::{InstalledProfile, LoaderKind};
use crate::meta::{RuleEnv, maven_path};
use crate::paths::DataDirs;
use crate::progress::{Phase, ProgressSink, Unit, report};
use crate::resolve::resolve_libraries;

/// Most `checksums` libraries fetched (and unpacked) at once.
const LEGACY_CONCURRENCY: usize = 4;

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
        return Err(LoaderError::InstallerInvalid(InstallerProblem::NotLegacy));
    };
    // Legacy installers are Forge-only (NeoForge never shipped one).
    let profile_id = checked_profile_id(LoaderKind::Forge, &version.id)?;
    // Held until the profile json is written.
    let _version_lock = data.lock_version_exclusive(&profile_id)?;
    let libraries = data.libraries();
    let universal = maven_path(&install_path)
        .and_then(|rel| safe_join(&libraries, &rel))
        .ok_or_else(|| LoaderError::UnsafeMetadata(install_path.clone()))?;

    // The universal jar is in place before any download, so its own library
    // entry in `versionInfo` resolves to an existing file and is skipped.
    let (jar_owned, dest) = (jar.to_path_buf(), universal.clone());
    tokio::task::spawn_blocking(move || extract_entry(&jar_owned, &file_path, &dest))
        .await
        .map_err(LoaderError::from)??;

    let resolved = resolve_libraries(&version.libraries, &RuleEnv::current(), &libraries);
    // `checksums` libraries (name, checksums) by destination: they are
    // verified against those and may come from a `.pack.xz` instead.
    let checksummed: HashMap<PathBuf, (String, Vec<String>)> = version
        .libraries
        .iter()
        .filter(|l| !l.checksums.is_empty())
        .filter_map(|l| {
            let dest = maven_path(&l.name).and_then(|rel| safe_join(&libraries, &rel))?;
            Some((dest, (l.name.clone(), l.checksums.clone())))
        })
        .collect();
    let (legacy_jobs, mut jobs): (Vec<DownloadJob>, Vec<DownloadJob>) = resolved
        .jobs
        .into_iter()
        .partition(|j| checksummed.contains_key(&j.dest));
    fill_sha1_from_sidecars(dl, &mut jobs, |j| j.sha1.is_none() && j.dest != universal).await;
    let plain_total = jobs.len();
    let total = plain_total + legacy_jobs.len();
    download_libraries(dl, jobs, progress, legacy_jobs.len() as u64).await?;
    let tools = Unpack200::new(dl, data.runtimes());
    let mut done = plain_total;
    let mut legacy = futures::stream::iter(&legacy_jobs)
        .map(|job| {
            let (name, checksums) = &checksummed[&job.dest];
            ensure_legacy_library(&tools, job, name, checksums)
        })
        .buffer_unordered(LEGACY_CONCURRENCY);
    while legacy.try_next().await?.is_some() {
        done += 1;
        report(
            progress,
            Phase::DownloadLibraries,
            done as u64,
            total as u64,
            Unit::Items,
        );
    }

    report(progress, Phase::Finalize, 0, 1, Unit::Items);
    let profile_id = write_profile(data, LoaderKind::Forge, version)?;
    report(progress, Phase::Finalize, 1, 1, Unit::Items);
    Ok(InstalledProfile { profile_id })
}

/// Copies jar entry `name` to `dest` through a unique `.part` file (see
/// [`replace_atomic_part`]).
fn extract_entry(jar: &Path, name: &str, dest: &Path) -> Result<(), LoaderError> {
    let invalid = |e: &dyn std::fmt::Display| {
        LoaderError::InstallerInvalid(InstallerProblem::UnreadableEntry {
            name: name.to_string(),
            detail: e.to_string(),
        })
    };
    let mut zip = zip::ZipArchive::new(std::fs::File::open(jar)?).map_err(|e| invalid(&e))?;
    let mut entry = zip.by_name(name).map_err(|e| invalid(&e))?;
    replace_atomic_part(dest, |part| {
        let mut f = std::fs::File::create(part)?;
        std::io::copy(&mut entry, &mut f)?;
        f.sync_all()
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::forge::packxz::{SIGNATURE_ENTRY, library_valid};
    use crate::loader::forge::read_installer;
    use crate::loader::forge::test_support::{
        PACK_SHA1, PACK_SIG_SHA1, PACK_XZ, PACKED_ENTRIES, downloader, serve_jre_legacy, sha1_hex,
        write_zip,
    };
    use crate::loader::{Phase, TaskProgress};
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
    const AKKA_NAME: &str = "com.typesafe.akka:akka-actor_2.11:2.3.3";
    const OTHER_SHA1: &str = "ed62e9fc709ca0f2ff1a3220daa8b70a2870078e";

    /// The fixture plus the universal jar's own library entry, as real
    /// 1.7.10 profiles have it (legacy files.minecraftforge.net url). The
    /// served akka body `b"akka"` matches its second `checksums` entry.
    fn profile_json() -> Value {
        profile_with_akka_checksums(&[OTHER_SHA1, &sha1_hex(b"akka")])
    }

    fn profile_with_akka_checksums(checksums: &[&str]) -> Value {
        let mut v: Value = serde_json::from_str(PROFILE).unwrap();
        let libs = v["versionInfo"]["libraries"].as_array_mut().unwrap();
        let akka = libs.iter_mut().find(|l| l["name"] == AKKA_NAME).unwrap();
        akka["checksums"] = json!(checksums);
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

        let mut last: Option<TaskProgress> = None;
        while let Ok(p) = rx.try_recv() {
            last = Some(p);
        }
        assert_eq!(last.unwrap().phase, Phase::Finalize);
    }

    #[tokio::test]
    async fn legacy_library_total_never_decreases() {
        let server = MockServer::start().await;
        serve(&server, LAUNCHWRAPPER, b"launchwrapper").await;
        serve(
            &server,
            &format!("{LAUNCHWRAPPER}.sha1"),
            format!("{}\n", sha1_hex(b"launchwrapper")).as_bytes(),
        )
        .await;
        serve(&server, AKKA, b"akka").await;
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path().join("data"));
        let jar = installer(tmp.path(), &profile_json());
        let profile = read_installer(&jar).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        install_legacy(&downloader(&server, false), &data, &jar, profile, &Some(tx))
            .await
            .unwrap();
        let mut libs = Vec::new();
        while let Ok(p) = rx.try_recv() {
            if p.phase == Phase::DownloadLibraries {
                libs.push(p);
            }
        }
        // One fixed total (plain + pack.xz libraries), rising done.
        assert!(!libs.is_empty());
        assert!(libs.iter().all(|p| p.total == libs[0].total), "{libs:?}");
        assert!(libs.windows(2).all(|w| w[0].done <= w[1].done), "{libs:?}");
        assert_eq!(libs.last().map(|p| (p.done, p.total)), Some((3, 3)));
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

    /// The fake `unpack200`: exits 3 unless its input is the fixture's
    /// pack200 data with the trailer cut; otherwise copies `jar` to its
    /// output.
    fn fake_unpack200(jar: &Path) -> Vec<u8> {
        format!(
            "#!/bin/sh\n[ \"$(sha1sum < \"$1\" | cut -c1-40)\" = {PACK_SHA1} ] || exit 3\ncp '{}' \"$2\"\n",
            jar.display()
        )
        .into_bytes()
    }

    async fn not_found(server: &MockServer, p: &str, hits: u64) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(404))
            .expect(hits)
            .mount(server)
            .await;
    }

    async fn serve_launchwrapper(server: &MockServer) {
        serve(server, LAUNCHWRAPPER, b"launchwrapper").await;
        serve(
            server,
            &format!("{LAUNCHWRAPPER}.sha1"),
            sha1_hex(b"launchwrapper").as_bytes(),
        )
        .await;
    }

    #[tokio::test]
    async fn plain_jar_preferred_when_checksum_matches() {
        let server = MockServer::start().await;
        serve_launchwrapper(&server).await;
        Mock::given(method("GET"))
            .and(path(AKKA))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"akka".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        never(&server, r"\.pack\.xz$").await;
        never(&server, "^/v1/").await;

        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path().join("data"));
        let jar = installer(tmp.path(), &profile_json());
        let dl = downloader(&server, false);
        for _ in 0..2 {
            // the second run keeps the verified jar: still one request
            let profile = read_installer(&jar).unwrap();
            let out = install_legacy(&dl, &data, &jar, profile, &None)
                .await
                .unwrap();
            assert_eq!(out.profile_id, ID);
        }
        assert_eq!(
            std::fs::read(data.libraries().join(&AKKA[1..])).unwrap(),
            b"akka"
        );
    }

    #[tokio::test]
    async fn falls_back_to_pack_xz() {
        let server = MockServer::start().await;
        serve_launchwrapper(&server).await;
        // the plain jar downloads from the mirror but fails its checksums
        // (a body that downloaded is not retried on the other sources)
        serve(&server, AKKA, b"stale akka").await;
        not_found(&server, &format!("/fb{AKKA}"), 0).await;
        serve(&server, &format!("{AKKA}.pack.xz"), PACK_XZ).await;

        let tmp = tempfile::tempdir().unwrap();
        let unpacked = tmp.path().join("unpacked.jar");
        write_zip(&unpacked, &PACKED_ENTRIES);
        serve_jre_legacy(&server, &fake_unpack200(&unpacked), 1).await;

        let data = DataDirs::new(tmp.path().join("data"));
        let profile_json = profile_with_akka_checksums(&[OTHER_SHA1, PACK_SIG_SHA1]);
        let jar = installer(tmp.path(), &profile_json);
        let profile = read_installer(&jar).unwrap();
        let out = install_legacy(&downloader(&server, true), &data, &jar, profile, &None)
            .await
            .unwrap();
        assert_eq!(out.profile_id, ID);

        let akka = data.libraries().join(&AKKA[1..]);
        assert!(library_valid(&akka, &[PACK_SIG_SHA1.to_string()]).unwrap());
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&akka).unwrap()).unwrap();
        let mut names: Vec<String> = zip.file_names().map(|n| n.unwrap().into_owned()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [SIGNATURE_ENTRY, "patina/data.txt", "patina/hello.txt"]
        );
        let mut sig = String::new();
        std::io::Read::read_to_string(&mut zip.by_name(SIGNATURE_ENTRY).unwrap(), &mut sig)
            .unwrap();
        assert!(sig.contains(" patina/hello.txt\n"), "{sig}");
        let left: Vec<_> = std::fs::read_dir(akka.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [akka.file_name().unwrap()], "temporary files left");
        assert!(
            crate::java::ManagedRuntimes::new(data.runtimes())
                .is_installed(super::super::packxz::UNPACK200_RUNTIME)
        );
    }

    #[tokio::test]
    async fn legacy_library_missing_everywhere_is_unavailable() {
        let server = MockServer::start().await;
        serve_launchwrapper(&server).await;
        for p in [AKKA.to_string(), format!("{AKKA}.pack.xz")] {
            not_found(&server, &p, 1).await;
            not_found(&server, &format!("/fb{p}"), 1).await;
        }
        // no pack.xz to unpack: jre-legacy is not installed
        serve_jre_legacy(&server, b"unused", 0).await;

        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path().join("data"));
        let jar = installer(tmp.path(), &profile_json());
        let profile = read_installer(&jar).unwrap();
        let r = install_legacy(&downloader(&server, true), &data, &jar, profile, &None).await;
        match r {
            Err(LoaderError::LegacyLibraryUnavailable { name }) => assert_eq!(name, AKKA_NAME),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!data.versions().join(ID).exists());
        assert!(!data.libraries().join(&AKKA[1..]).exists());

        use crate::i18n::{Language, locale_lock, set_language};
        let _locale = locale_lock();
        let e = LoaderError::LegacyLibraryUnavailable {
            name: AKKA_NAME.into(),
        };
        for lang in [Language::En, Language::ZhCn] {
            set_language(lang);
            let msg = e.to_string();
            assert!(msg.contains(AKKA_NAME) && !msg.contains("errors."), "{msg}");
        }
    }

    #[tokio::test]
    async fn corrupt_unpack_result_is_unavailable() {
        let server = MockServer::start().await;
        serve_launchwrapper(&server).await;
        not_found(&server, AKKA, 1).await;
        serve(&server, &format!("{AKKA}.pack.xz"), PACK_XZ).await;
        let tmp = tempfile::tempdir().unwrap();
        // the "unpacked" jar's entries do not match the signature
        let unpacked = tmp.path().join("unpacked.jar");
        write_zip(&unpacked, &[("patina/hello.txt", b"tampered")]);
        serve_jre_legacy(&server, &fake_unpack200(&unpacked), 1).await;

        let data = DataDirs::new(tmp.path().join("data"));
        let profile_json = profile_with_akka_checksums(&[PACK_SIG_SHA1]);
        let jar = installer(tmp.path(), &profile_json);
        let profile = read_installer(&jar).unwrap();
        let r = install_legacy(&downloader(&server, false), &data, &jar, profile, &None).await;
        assert!(
            matches!(r, Err(LoaderError::LegacyLibraryUnavailable { .. })),
            "{r:?}"
        );
        let akka = data.libraries().join(&AKKA[1..]);
        let left = std::fs::read_dir(akka.parent().unwrap()).unwrap().count();
        assert_eq!(left, 0, "temporary files left");
    }
}
