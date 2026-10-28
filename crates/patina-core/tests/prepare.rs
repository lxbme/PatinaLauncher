//! Offline end-to-end test of `Launcher::prepare` against a fake Mojang.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use patina_core::config::GlobalConfig;
use patina_core::download::{Downloader, Mirror};
use patina_core::instance::InstanceStore;
use patina_core::launch::{LaunchAccount, UserType, offline_uuid};
use patina_core::paths::DataDirs;
use patina_core::prepare::{Launcher, PrepareError};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODERN: &str = include_str!("fixtures/version_modern.json");

fn sha1_hex(b: &[u8]) -> String {
    hex::encode(Sha1::digest(b))
}

fn url_path(url: &str) -> String {
    let rest = url.split_once("://").unwrap().1;
    format!("/{}", rest.split_once('/').unwrap().1)
}

async fn serve(server: &MockServer, p: &str, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path(p.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(server)
        .await;
}

/// Points `file_ref` at `body` (real sha1/size) and serves it at the ref's URL path.
async fn serve_ref(server: &MockServer, file_ref: &mut Value, body: &[u8]) {
    file_ref["sha1"] = json!(sha1_hex(body));
    file_ref["size"] = json!(body.len());
    serve(
        server,
        &url_path(file_ref["url"].as_str().unwrap()),
        body.to_vec(),
    )
    .await;
}

struct Fake {
    server: MockServer,
    asset_hash: String,
}

/// Serves manifest, version JSON (java major `java_major`), client jar, the two
/// Linux libraries, asset index with one object, the object and logging config.
async fn fake_mojang(java_major: u32) -> Fake {
    let server = MockServer::start().await;
    let mut v: Value = serde_json::from_str(MODERN).unwrap();
    v["javaVersion"]["majorVersion"] = json!(java_major);

    serve_ref(&server, &mut v["downloads"]["client"], b"client jar bytes").await;
    serve_ref(
        &server,
        &mut v["logging"]["client"]["file"],
        b"<Configuration/>",
    )
    .await;
    for (i, lib) in v["libraries"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        let body = format!("library {i}").into_bytes();
        serve_ref(&server, &mut lib["downloads"]["artifact"], &body).await;
    }

    let object = b"a sound".to_vec();
    let asset_hash = sha1_hex(&object);
    serve(
        &server,
        &format!("/{}/{}", &asset_hash[..2], asset_hash),
        object.clone(),
    )
    .await;
    let index = serde_json::to_vec(&json!({
        "objects": { "minecraft/sounds/a.ogg": { "hash": asset_hash, "size": object.len() } }
    }))
    .unwrap();
    serve_ref(&server, &mut v["assetIndex"], &index).await;

    let version_bytes = serde_json::to_vec(&v).unwrap();
    let version_url = "https://piston-meta.mojang.com/v1/packages/abc/26.3.json";
    serve(&server, &url_path(version_url), version_bytes.clone()).await;
    let manifest = json!({
        "latest": { "release": "26.3", "snapshot": "26.3" },
        "versions": [{
            "id": "26.3", "type": "release", "url": version_url,
            "sha1": sha1_hex(&version_bytes), "releaseTime": "2026-09-30T10:00:00+00:00"
        }]
    });
    serve(
        &server,
        "/mc/game/version_manifest_v2.json",
        serde_json::to_vec(&manifest).unwrap(),
    )
    .await;
    Fake { server, asset_hash }
}

fn launcher(server: &MockServer, data: &Path) -> Launcher {
    let rules = [
        "https://piston-meta.mojang.com",
        "https://piston-data.mojang.com",
        "https://libraries.minecraft.net",
        "https://resources.download.minecraft.net",
    ]
    .into_iter()
    .map(|h| (h.to_string(), server.uri()))
    .collect();
    let mirror = Mirror {
        id: "fake".into(),
        rules,
    };
    Launcher {
        data: DataDirs::new(data),
        downloader: Downloader::new(reqwest::Client::new(), mirror).with_retry(1, Duration::ZERO),
    }
}

fn account() -> LaunchAccount {
    LaunchAccount {
        name: "Dev".into(),
        uuid: offline_uuid("Dev"),
        access_token: "0".into(),
        user_type: UserType::Legacy,
        xuid: None,
    }
}

fn fake_java(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("jdk/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let java = bin.join("java");
    std::fs::write(
        &java,
        "#!/bin/sh\necho '    java.version = 1.8.0_202' >&2\necho '    sun.arch.data.model = 64' >&2\n",
    )
    .unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
    java
}

struct Setup {
    _tmp: tempfile::TempDir,
    data: PathBuf,
    java: PathBuf,
    global: GlobalConfig,
}

fn setup() -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("数据 目录");
    let java = fake_java(tmp.path());
    let mut global = GlobalConfig::default();
    global.java.java_path = Some(java.clone());
    Setup {
        _tmp: tmp,
        data,
        java,
        global,
    }
}

#[tokio::test]
async fn prepare_builds_command_against_fake_mojang() {
    let fake = fake_mojang(8).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    let inst = InstanceStore::new(l.data.instances())
        .create("测试 实例", "26.3")
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let prepared = l
        .prepare(&inst, &s.global, &account(), Some(tx))
        .await
        .unwrap();

    assert_eq!(prepared.command.program, s.java.canonicalize().unwrap());
    assert!(s.data.join("versions/26.3/26.3.jar").is_file());
    assert!(s.data.join("versions/26.3/26.3.json").is_file());
    assert!(s.data.join("assets/indexes/30.json").is_file());
    let h = &fake.asset_hash;
    assert!(
        s.data
            .join(format!("assets/objects/{}/{h}", &h[..2]))
            .is_file()
    );
    let log_arg = format!(
        "-Dlog4j.configurationFile={}",
        s.data.join("assets/log_configs/client-1.12.xml").display()
    );
    assert!(
        prepared.command.args.contains(&log_arg),
        "{:?}",
        prepared.command
    );
    // Classpath: both Linux libraries, client jar last.
    let cp_idx = prepared
        .command
        .args
        .iter()
        .position(|a| a == "-cp")
        .unwrap();
    let cp = &prepared.command.args[cp_idx + 1];
    assert!(cp.contains("brigadier-1.0.18.jar"));
    assert!(cp.contains("lwjgl-3.3.3-natives-linux.jar"));
    assert!(!cp.contains("macos"));
    assert!(cp.ends_with("versions/26.3/26.3.jar"));
    assert!(inst.natives_dir().is_dir());
    assert!(inst.game_dir().is_dir());
    assert_eq!(prepared.command.cwd, inst.game_dir());

    let mut last = None;
    while let Ok(p) = rx.try_recv() {
        last = Some(p);
    }
    let last = last.expect("progress reported");
    assert_eq!(last.done, last.total);
}

#[tokio::test]
async fn ensure_version_caches_and_reuses_json() {
    let fake = fake_mojang(8).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    assert_eq!(l.ensure_version("26.3").await.unwrap().id, "26.3");
    assert!(matches!(
        l.ensure_version("9.9.9").await,
        Err(PrepareError::UnknownVersion(id)) if id == "9.9.9"
    ));
    assert!(matches!(
        l.ensure_version("../26.3").await,
        Err(PrepareError::UnknownVersion(_))
    ));
    let version_requests = || async {
        fake.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/26.3.json"))
            .count()
    };
    assert_eq!(version_requests().await, 1);
    // A cached file matching the manifest sha1 is reused without a download.
    assert_eq!(l.ensure_version("26.3").await.unwrap().id, "26.3");
    assert_eq!(version_requests().await, 1);
    // A corrupted cache is replaced.
    std::fs::write(s.data.join("versions/26.3/26.3.json"), b"{}").unwrap();
    assert_eq!(l.ensure_version("26.3").await.unwrap().id, "26.3");
    assert_eq!(version_requests().await, 2);
}

#[tokio::test]
async fn prepare_rejects_too_old_java() {
    let fake = fake_mojang(21).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    let inst = InstanceStore::new(l.data.instances())
        .create("x", "26.3")
        .unwrap();
    let r = l.prepare(&inst, &s.global, &account(), None).await;
    assert!(
        matches!(
            r,
            Err(PrepareError::JavaTooOld {
                required: 21,
                found: 8
            })
        ),
        "{:?}",
        r.err()
    );
}

#[tokio::test]
async fn prepare_rejects_bad_jvm_args() {
    let fake = fake_mojang(8).await;
    let mut s = setup();
    s.global.java.extra_jvm_args = "-Xmx2G".into();
    let l = launcher(&fake.server, &s.data);
    let inst = InstanceStore::new(l.data.instances())
        .create("x", "26.3")
        .unwrap();
    let r = l.prepare(&inst, &s.global, &account(), None).await;
    assert!(
        matches!(r, Err(PrepareError::InvalidJvmArgs(_))),
        "{:?}",
        r.err()
    );
}

#[tokio::test]
async fn prepare_reports_missing_java() {
    let fake = fake_mojang(8).await;
    let mut s = setup();
    s.global.java.java_path = Some(s.data.join("nope/bin/java"));
    let l = launcher(&fake.server, &s.data);
    let inst = InstanceStore::new(l.data.instances())
        .create("x", "26.3")
        .unwrap();
    let r = l.prepare(&inst, &s.global, &account(), None).await;
    assert!(
        matches!(r, Err(PrepareError::JavaNotFound(_))),
        "{:?}",
        r.err()
    );
}
