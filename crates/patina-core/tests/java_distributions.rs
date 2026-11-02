use patina_core::{
    config::JavaSettings,
    download::{Downloader, Mirror},
    instance::InstanceStore,
    java::{
        catalog::parse_temurin,
        distributions::{DistributionError, DistributionStore},
    },
};
use sha2::{Digest, Sha256};
use std::sync::{Arc, atomic::AtomicBool};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn archive(link: Option<&str>, version: &str) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(encoder);
    for (name, content, mode) in [
        ("jdk/bin/java", "#!/bin/sh\nexit 0\n".into(), 0o755),
        (
            "jdk/release",
            format!("JAVA_VERSION=\"{version}\"\nOS_ARCH=\"x86_64\"\n"),
            0o644,
        ),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_size(content.len() as u64);
        h.set_mode(mode);
        h.set_cksum();
        tar.append_data(&mut h, name, content.as_bytes()).unwrap();
    }
    if let Some(target) = link {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        h.set_link_name(target).unwrap();
        h.set_cksum();
        tar.append_data(&mut h, "jdk/lib/alias", std::io::empty())
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}
async fn setup(
    body: Vec<u8>,
) -> (
    MockServer,
    Downloader,
    patina_core::java::catalog::JavaRelease,
) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jdk.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;
    let mut v: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/java/temurin-live.json")).unwrap();
    let package = &mut v[0]["binaries"][0]["package"];
    package["link"] = format!("{}/jdk.tar.gz", server.uri()).into();
    package["size"] = (body.len() as u64).into();
    package["checksum"] = hex::encode(Sha256::digest(&body)).into();
    let release = parse_temurin(&v.to_string(), 21).unwrap().remove(0);
    let dl = Downloader::new(reqwest::Client::new(), Mirror::official())
        .with_retry(1, std::time::Duration::ZERO);
    (server, dl, release)
}
#[tokio::test]
async fn checked_install_keeps_permissions_internal_links_and_is_idempotent() {
    use std::os::unix::fs::PermissionsExt;
    let (_s, dl, r) = setup(archive(Some("../release"), "21.0.13")).await;
    let tmp = tempfile::tempdir().unwrap();
    let store = DistributionStore::new(tmp.path().join("java-distributions"));
    let cancel = Arc::new(AtomicBool::new(false));
    let installed = store.install(&dl, &r, &None, cancel.clone()).await.unwrap();
    assert_eq!(installed.java().major, 21);
    assert!(
        std::fs::metadata(&installed.java().path)
            .unwrap()
            .permissions()
            .mode()
            & 0o111
            != 0
    );
    assert!(
        installed
            .java()
            .path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("lib/alias")
            .exists()
    );
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(
        store
            .install(&dl, &r, &None, cancel)
            .await
            .unwrap()
            .java()
            .path,
        installed.java().path
    );
}
#[tokio::test]
async fn failed_checksum_cancel_escaping_link_and_version_leave_no_install_and_retry_works() {
    let tmp = tempfile::tempdir().unwrap();
    let store = DistributionStore::new(tmp.path().join("managed"));
    for (body, canceled, corrupt) in [
        (archive(None, "21.0.13"), false, true),
        (archive(None, "21.0.13"), true, false),
        (archive(Some("../../../outside"), "21.0.13"), false, false),
        (archive(None, "17.0.13"), false, false),
    ] {
        let (_s, dl, r) = setup(body).await;
        let r = if corrupt {
            let mut v = serde_json::to_value(r).unwrap();
            v["sha256"] = "0".repeat(64).into();
            serde_json::from_value(v).unwrap()
        } else {
            r
        };
        assert!(
            store
                .install(&dl, &r, &None, Arc::new(AtomicBool::new(canceled)))
                .await
                .is_err()
        );
        assert!(store.list().unwrap().is_empty());
    }
    let (_s, dl, r) = setup(archive(None, "21.0.13")).await;
    store
        .install(&dl, &r, &None, Arc::new(AtomicBool::new(false)))
        .await
        .unwrap();
    assert_eq!(store.list().unwrap().len(), 1);
}
#[tokio::test]
async fn deletion_protects_global_instance_aliases_and_never_accepts_arbitrary_paths() {
    let (_s, dl, r) = setup(archive(None, "21.0.13")).await;
    let tmp = tempfile::tempdir().unwrap();
    let store = DistributionStore::new(tmp.path().join("managed"));
    let instances = InstanceStore::new(tmp.path().join("instances"));
    let installed = store
        .install(&dl, &r, &None, Arc::new(AtomicBool::new(false)))
        .await
        .unwrap();
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(&installed.java().path, &alias).unwrap();
    let mut global = JavaSettings {
        java_path: Some(alias.clone()),
        ..Default::default()
    };
    assert!(
        matches!(store.remove(&r.id(),&global,&instances).await,Err(DistributionError::Referenced(ref objects)) if !objects.is_empty())
    );
    global.java_path = None;
    let mut instance = instances.create("uses managed java", "1.21.1").unwrap();
    instance.config.java_override = Some(JavaSettings {
        java_path: Some(alias),
        ..Default::default()
    });
    instances.save(&instance).unwrap();
    assert!(
        matches!(store.remove(&r.id(),&global,&instances).await,Err(DistributionError::Referenced(ref objects)) if objects.iter().any(|o|o.contains("uses managed java")))
    );
    assert!(
        store
            .remove("../system", &global, &instances)
            .await
            .is_err()
    );
    instance.config.java_override = None;
    instances.save(&instance).unwrap();
    store.remove(&r.id(), &global, &instances).await.unwrap();
    assert!(store.list().unwrap().is_empty());
}

/// Explicitly authorized live acceptance only. Uses the supplied isolated data
/// directory, official network services and one archive per selected vendor.
#[tokio::test]
#[ignore]
async fn live_official_vendor_install_and_java_version() {
    use patina_core::java::catalog::{JavaCatalog, JavaVendor};
    let root = std::env::var_os("PATINA_JAVA_ACCEPTANCE_DIR")
        .expect("explicit isolated directory required");
    let data = patina_core::paths::DataDirs::new(root);
    let store = DistributionStore::new(data.java_distributions());
    let client = patina_core::prepare::http_client().unwrap();
    let catalog = JavaCatalog::new(client.clone());
    let dl = Downloader::new(client, Mirror::official());
    let filter =
        std::env::var("PATINA_JAVA_VENDORS").unwrap_or_else(|_| "temurin,zulu,microsoft".into());
    for vendor in JavaVendor::ALL
        .into_iter()
        .filter(|v| filter.contains(v.id()))
    {
        let releases = catalog.releases(vendor, 21).await.unwrap();
        let release = releases.first().expect("official Java 21 build");
        println!(
            "LIVE {} {} {}",
            vendor.name(),
            release.version(),
            release.url()
        );
        let installed = store
            .install(&dl, release, &None, Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        let output = std::process::Command::new(&installed.java().path)
            .arg("-version")
            .output()
            .unwrap();
        println!(
            "SHA256 {}\nPATH {}\nEXIT {:?}\n{}",
            installed.sha256(),
            installed.java().path.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success());
        assert_eq!(installed.java().major, 21);
    }
}

#[tokio::test]
async fn sha256_is_authoritative_when_official_metadata_size_is_rounded() {
    let (_s, dl, r) = setup(archive(None, "21.0.13")).await;
    let mut value = serde_json::to_value(r).unwrap();
    value["size"] = 1.into();
    let release = serde_json::from_value(value).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let store = DistributionStore::new(tmp.path().join("owned"));
    assert!(
        store
            .install(&dl, &release, &None, Arc::new(AtomicBool::new(false)))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn install_rejects_arm64_release_and_unsafe_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let store = DistributionStore::new(tmp.path().join("owned"));
    let body = archive(None, "21.0.13");
    let plain = flate2::read::GzDecoder::new(&body[..]);
    let mut tar = tar::Archive::new(plain);
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for entry in tar.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().into_owned();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        if path.ends_with("release") {
            bytes = String::from_utf8(bytes)
                .unwrap()
                .replace("x86_64", "aarch64")
                .into_bytes();
        }
        let mut h = entry.header().clone();
        h.set_size(bytes.len() as u64);
        h.set_cksum();
        builder.append_data(&mut h, path, &bytes[..]).unwrap();
    }
    let body = builder.into_inner().unwrap().finish().unwrap();
    let (_s, dl, r) = setup(body).await;
    assert!(
        store
            .install(&dl, &r, &None, Arc::new(AtomicBool::new(false)))
            .await
            .is_err()
    );
    let mut v = serde_json::to_value(r).unwrap();
    v["version"] = "../../escape".into();
    let r = serde_json::from_value(v).unwrap();
    assert!(
        store
            .install(&dl, &r, &None, Arc::new(AtomicBool::new(false)))
            .await
            .is_err()
    );
    assert!(!tmp.path().join("escape").exists());
}

#[tokio::test]
async fn actual_running_executable_blocks_deletion_after_selection_changed() {
    use patina_core::launch::running::{RunningRecord, write_record};
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (path, bytes) in [
        ("jdk/bin/java", std::fs::read("/bin/sh").unwrap()),
        (
            "jdk/release",
            b"JAVA_VERSION=\"21.0.13\"\nOS_ARCH=\"x86_64\"\n".to_vec(),
        ),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o755);
        h.set_cksum();
        builder.append_data(&mut h, path, &bytes[..]).unwrap();
    }
    let body = builder.into_inner().unwrap().finish().unwrap();
    let (_s, dl, r) = setup(body).await;
    let tmp = tempfile::tempdir().unwrap();
    let store = DistributionStore::new(tmp.path().join("owned"));
    let installed = store
        .install(&dl, &r, &None, Arc::new(AtomicBool::new(false)))
        .await
        .unwrap();
    let instances = InstanceStore::new(tmp.path().join("instances"));
    let inst = instances.create("Running old Java", "1.21.1").unwrap();
    let marker = format!("-Dpatina.instance={}", inst.id);
    let mut child = std::process::Command::new(&installed.java().path)
        .args(["-c", "sleep 20 & wait", "fixture", &marker])
        .spawn()
        .unwrap();
    let result=async {
  tokio::time::sleep(std::time::Duration::from_millis(80)).await;
  let info=patina_core::platform::proc_info(child.id()).unwrap();
  write_record(&inst,&RunningRecord{pid:child.id(),start_time:info.start_time,instance_id:inst.id.clone(),started_at:0,log:"fixture".into()}).unwrap();
  assert!(patina_core::launch::running_game(&inst).is_some(),"fixture marker must be accepted by production running check");
  let result=store.remove(&r.id(),&JavaSettings::default(),&instances).await;
  assert!(matches!(result,Err(DistributionError::Referenced(ref names)) if names.iter().any(|s|s.contains("Running old Java"))));
 }.await;
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[tokio::test]
async fn extraction_rejects_symlink_chain_that_lexically_looks_internal() {
    let body = archive(None, "21.0.13");
    let mut original = tar::Archive::new(flate2::read::GzDecoder::new(&body[..]));
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for entry in original.entries().unwrap() {
        let mut e = entry.unwrap();
        let path = e.path().unwrap().into_owned();
        let header = e.header().clone();
        builder
            .append_data(&mut header.clone(), path, &mut e)
            .unwrap();
    }
    for (path, target) in [("jdk/lib/up", ".."), ("jdk/lib/escape", "up/../../outside")] {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        h.set_link_name(target).unwrap();
        h.set_cksum();
        builder.append_data(&mut h, path, std::io::empty()).unwrap();
    }
    let body = builder.into_inner().unwrap().finish().unwrap();
    let (_s, dl, r) = setup(body).await;
    let tmp = tempfile::tempdir().unwrap();
    let store = DistributionStore::new(tmp.path().join("owned"));
    assert!(
        store
            .install(&dl, &r, &None, Arc::new(AtomicBool::new(false)))
            .await
            .is_err()
    );
    assert!(store.list().unwrap().is_empty());
}

#[tokio::test]
async fn update_installs_distinct_build_without_removing_or_switching_old_java() {
    let tmp = tempfile::tempdir().unwrap();
    let store = DistributionStore::new(tmp.path().join("owned"));
    let instances = InstanceStore::new(tmp.path().join("instances"));
    let (_old_server, dl, old) = setup(archive(None, "21.0.12")).await;
    let mut value = serde_json::to_value(old).unwrap();
    value["version"] = "21.0.12+7".into();
    value["build"] = serde_json::json!([21, 0, 12, 7]);
    let old = serde_json::from_value(value).unwrap();
    let first = store
        .install(&dl, &old, &None, Arc::new(AtomicBool::new(false)))
        .await
        .unwrap();
    let global = JavaSettings {
        java_path: Some(first.java().path.clone()),
        ..Default::default()
    };
    let (_new_server, dl, new) = setup(archive(None, "21.0.13")).await;
    assert!(new.is_newer_than(&old));
    let second = store
        .install(&dl, &new, &None, Arc::new(AtomicBool::new(false)))
        .await
        .unwrap();
    assert_ne!(first.java().path, second.java().path);
    assert!(first.java().path.is_file());
    assert_eq!(store.list().unwrap().len(), 2);
    assert_eq!(global.java_path, Some(first.java().path.clone()));
    assert!(matches!(
        store.remove(&old.id(), &global, &instances).await,
        Err(DistributionError::Referenced(_))
    ));
    store.remove(&new.id(), &global, &instances).await.unwrap();
    assert!(first.java().path.is_file());
}

#[tokio::test]
async fn archive_parent_path_is_rejected_and_unmarked_system_java_cannot_be_removed() {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut h = tar::Header::new_gnu();
    h.set_size(1);
    h.set_mode(0o644);
    h.as_mut_bytes()[..17].copy_from_slice(b"../../outside.txt");
    h.set_cksum();
    builder.append(&h, &b"x"[..]).unwrap();
    let body = builder.into_inner().unwrap().finish().unwrap();
    let (_s, dl, r) = setup(body).await;
    let tmp = tempfile::tempdir().unwrap();
    let store = DistributionStore::new(tmp.path().join("owned"));
    assert!(
        store
            .install(&dl, &r, &None, Arc::new(AtomicBool::new(false)))
            .await
            .is_err()
    );
    assert!(!tmp.path().join("outside.txt").exists());
    let system = tmp.path().join("owned/system/bin");
    std::fs::create_dir_all(&system).unwrap();
    std::fs::write(system.join("java"), b"user owned").unwrap();
    assert!(
        store
            .remove(
                "system",
                &JavaSettings::default(),
                &InstanceStore::new(tmp.path().join("instances"))
            )
            .await
            .is_err()
    );
    assert!(system.join("java").exists());
}

#[tokio::test]
async fn mojang_management_preserves_explicit_references_in_unified_selector() {
    use patina_core::java::ManagedRuntimes;
    let tmp = tempfile::tempdir().unwrap();
    let rt = ManagedRuntimes::new(tmp.path().join("runtimes"));
    let component = "java-runtime-gamma";
    let home = rt.home(component);
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::write(home.join("bin/java"), b"fixture").unwrap();
    std::fs::write(home.join(".patina-installed"), "21.0.13").unwrap();
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(rt.java_bin(component), &alias).unwrap();
    let instances = InstanceStore::new(tmp.path().join("instances"));
    let global = JavaSettings {
        java_path: Some(alias.clone()),
        ..Default::default()
    };
    assert!(matches!(
        rt.remove_unreferenced(component, &global, &instances).await,
        Err(DistributionError::Referenced(_))
    ));
    assert!(home.exists());
    let mut instance = instances.create("Mojang reference", "1.21.1").unwrap();
    instance.config.java_override = Some(JavaSettings {
        java_path: Some(alias),
        ..Default::default()
    });
    instances.save(&instance).unwrap();
    assert!(matches!(
        rt.remove_unreferenced(component, &JavaSettings::default(), &instances)
            .await,
        Err(DistributionError::Referenced(_))
    ));
    instance.config.java_override = None;
    instances.save(&instance).unwrap();
    rt.remove_unreferenced(component, &JavaSettings::default(), &instances)
        .await
        .unwrap();
    assert!(!home.exists());
}
