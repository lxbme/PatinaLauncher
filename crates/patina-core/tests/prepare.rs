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
    fake_mojang_with(java_major, |_| {}).await
}

/// Like [`fake_mojang`], with `edit` applied to the version JSON last.
async fn fake_mojang_with(java_major: u32, edit: impl FnOnce(&mut Value)) -> Fake {
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

    edit(&mut v);
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

#[tokio::test]
async fn prepare_rejects_unsafe_asset_index_id() {
    let fake = fake_mojang_with(8, |v| v["assetIndex"]["id"] = json!("../../evil")).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    let inst = InstanceStore::new(l.data.instances())
        .create("x", "26.3")
        .unwrap();
    let r = l.prepare(&inst, &s.global, &account(), None).await;
    assert!(
        matches!(&r, Err(PrepareError::UnsafeMetadata(n)) if n == "../../evil"),
        "{:?}",
        r.err()
    );
    assert!(!s.data.join("evil.json").exists());
}

#[tokio::test]
async fn prepare_rejects_unsafe_version_id() {
    let fake = fake_mojang_with(8, |v| v["id"] = json!("../../evil")).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    let inst = InstanceStore::new(l.data.instances())
        .create("x", "26.3")
        .unwrap();
    let r = l.prepare(&inst, &s.global, &account(), None).await;
    assert!(
        matches!(&r, Err(PrepareError::UnsafeMetadata(n)) if n == "../../evil"),
        "{:?}",
        r.err()
    );
}

#[tokio::test]
async fn client_jar_is_keyed_on_requested_id() {
    let fake = fake_mojang_with(8, |v| v["id"] = json!("26.3-renamed")).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    let inst = InstanceStore::new(l.data.instances())
        .create("x", "26.3")
        .unwrap();
    let prepared = l.prepare(&inst, &s.global, &account(), None).await.unwrap();
    assert!(s.data.join("versions/26.3/26.3.jar").is_file());
    assert!(s.data.join("versions/26.3/26.3.json").is_file());
    assert!(!s.data.join("versions/26.3-renamed").exists());
    let args = &prepared.command.args;
    let cp = &args[args.iter().position(|a| a == "-cp").unwrap() + 1];
    assert!(cp.ends_with("versions/26.3/26.3.jar"), "{cp}");
}

// ---- loader instances ----

use patina_core::instance::{Component, ComponentUid, Instance};
use patina_core::loader::forge::GENERATED_FILE;

const PROFILE_ID: &str = "fabric-loader-0.16.10-26.3";

fn loader_instance(l: &Launcher, uid: ComponentUid, profile: Option<&str>) -> Instance {
    let mut inst = InstanceStore::new(l.data.instances())
        .create("加载器", "26.3")
        .unwrap();
    inst.config
        .set_loader(Some(Component {
            uid,
            version: "0.16.10".into(),
            profile: profile.map(String::from),
        }))
        .unwrap();
    inst
}

fn write_profile(data: &Path, id: &str, profile: &Value) {
    let dir = data.join("versions").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_vec(profile).unwrap(),
    )
    .unwrap();
}

fn art(name: &str, path: &str, url: &str, body: &[u8]) -> Value {
    json!({ "name": name, "downloads": { "artifact": {
        "path": path, "url": url, "sha1": sha1_hex(body), "size": body.len() } } })
}

fn classpath(args: &[String]) -> Vec<String> {
    let cp = &args[args.iter().position(|a| a == "-cp").unwrap() + 1];
    cp.split(':').map(String::from).collect()
}

#[tokio::test]
async fn prepare_merges_fabric_profile() {
    let fake = fake_mojang(8).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    let body = b"fabric loader";
    serve(&fake.server, "/fabric/loader.jar", body.to_vec()).await;
    let url = format!("{}/fabric/loader.jar", fake.server.uri());
    // Same group:artifact as a vanilla library, different version.
    let dup_body = b"brigadier override";
    serve(&fake.server, "/fabric/brig.jar", dup_body.to_vec()).await;
    let dup_url = format!("{}/fabric/brig.jar", fake.server.uri());
    write_profile(
        &s.data,
        PROFILE_ID,
        &json!({
            "id": PROFILE_ID, "inheritsFrom": "26.3", "type": "release",
            "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
            "arguments": { "game": [], "jvm": [] },
            "libraries": [
                art("net.fabricmc:fabric-loader:0.16.10", "net/fabricmc/fabric-loader/0.16.10/fabric-loader-0.16.10.jar", &url, body),
                art("com.mojang:brigadier:9.9.9", "com/mojang/brigadier/9.9.9/brigadier-9.9.9.jar", &dup_url, dup_body),
            ]
        }),
    );
    let inst = loader_instance(&l, ComponentUid::Fabric, Some(PROFILE_ID));
    let prepared = l.prepare(&inst, &s.global, &account(), None).await.unwrap();
    let args = &prepared.command.args;
    assert!(args.contains(&"net.fabricmc.loader.impl.launch.knot.KnotClient".to_string()));
    assert!(!args.contains(&"net.minecraft.client.main.Main".to_string()));
    let cp = classpath(args);
    assert!(cp.iter().any(|p| p.ends_with("fabric-loader-0.16.10.jar")));
    let brig: Vec<_> = cp.iter().filter(|p| p.contains("brigadier")).collect();
    assert_eq!(brig.len(), 1, "{cp:?}");
    assert!(brig[0].ends_with("brigadier-9.9.9.jar"));
    // Like the official launcher, a loader launches on a copy of the
    // vanilla jar named after its profile.
    let jar = format!("versions/{PROFILE_ID}/{PROFILE_ID}.jar");
    assert!(cp.last().unwrap().ends_with(&jar), "{cp:?}");
    let v = args.iter().position(|a| a == "--version").unwrap();
    assert_eq!(args[v + 1], PROFILE_ID);
    assert!(s.data.join("versions/26.3/26.3.jar").is_file());
    assert_eq!(
        std::fs::read(s.data.join(&jar)).unwrap(),
        b"client jar bytes"
    );
}

/// NeoForge and Forge 1.17+ keep the client jar out of the module layer with
/// `-DignoreList=...,${version_name}.jar`, matched against the classpath
/// file names: with the jar at `versions/<mc>/<mc>.jar` the game fails with
/// "Modules _1._21._1 and minecraft export package net.minecraft.server".
#[tokio::test]
async fn loader_client_jar_matches_ignore_list() {
    let fake = fake_mojang(8).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    let id = "neoforge-21.1.265";
    write_profile(
        &s.data,
        id,
        &json!({
            "id": id, "inheritsFrom": "26.3", "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "arguments": { "game": [], "jvm": [
                "-DignoreList=client-extra,${version_name}.jar",
                "-DlibraryDirectory=${library_directory}"
            ] },
            "libraries": []
        }),
    );
    std::fs::write(s.data.join("versions").join(id).join(GENERATED_FILE), "[]").unwrap();
    let inst = loader_instance(&l, ComponentUid::Neoforge, Some(id));
    let prepared = l.prepare(&inst, &s.global, &account(), None).await.unwrap();
    let args = &prepared.command.args;
    let ignore = args
        .iter()
        .find_map(|a| a.strip_prefix("-DignoreList="))
        .unwrap();
    let cp = classpath(args);
    let client = Path::new(cp.last().unwrap());
    let name = client.file_name().unwrap().to_str().unwrap();
    assert!(
        ignore.split(',').any(|i| name.starts_with(i)) && name == format!("{id}.jar"),
        "{ignore} vs {cp:?}"
    );
    assert_eq!(std::fs::read(client).unwrap(), b"client jar bytes");

    // A damaged copy is replaced from the verified vanilla jar.
    std::fs::write(client, b"damaged").unwrap();
    l.prepare(&inst, &s.global, &account(), None).await.unwrap();
    assert_eq!(std::fs::read(client).unwrap(), b"client jar bytes");
}

#[tokio::test]
async fn missing_generated_library_requires_reinstall() {
    let fake = fake_mojang(8).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    write_profile(
        &s.data,
        "forge-x",
        &json!({
            "id": "forge-x", "inheritsFrom": "26.3", "mainClass": "m",
            "libraries": [art("net.minecraft:client:1-x:srg", "net/minecraft/client/1-x/client-1-x-srg.jar", "", b"")]
        }),
    );
    let inst = loader_instance(&l, ComponentUid::Forge, Some("forge-x"));
    let err = l
        .prepare(&inst, &s.global, &account(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PrepareError::LoaderNeedsReinstall { .. }),
        "{err}"
    );
    // Present on disk: launches.
    let f = s
        .data
        .join("libraries/net/minecraft/client/1-x/client-1-x-srg.jar");
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(&f, b"x").unwrap();
    l.prepare(&inst, &s.global, &account(), None).await.unwrap();
}

#[tokio::test]
async fn generated_file_list_is_checked_and_modern_needs_it() {
    let fake = fake_mojang(8).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    write_profile(
        &s.data,
        "neoforge-x",
        &json!({
            "id": "neoforge-x", "inheritsFrom": "26.3", "mainClass": "m",
            "arguments": { "game": [], "jvm": ["-DlibraryDirectory=${library_directory}"] },
            "libraries": []
        }),
    );
    let inst = loader_instance(&l, ComponentUid::Neoforge, Some("neoforge-x"));
    // Modern profile without the generated list: incomplete install.
    let e = l.prepare(&inst, &s.global, &account(), None).await;
    assert!(matches!(e, Err(PrepareError::LoaderNeedsReinstall { .. })));
    let dir = s.data.join("versions/neoforge-x");
    std::fs::write(dir.join(GENERATED_FILE), r#"["a/b/1/b-1.jar"]"#).unwrap();
    let e = l.prepare(&inst, &s.global, &account(), None).await;
    assert!(matches!(e, Err(PrepareError::LoaderNeedsReinstall { .. })));
    let f = s.data.join("libraries/a/b/1/b-1.jar");
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(&f, b"x").unwrap();
    l.prepare(&inst, &s.global, &account(), None).await.unwrap();
}

#[tokio::test]
async fn missing_profile_requires_reinstall() {
    let fake = fake_mojang(8).await;
    let s = setup();
    let l = launcher(&fake.server, &s.data);
    for profile in [Some(PROFILE_ID), None] {
        let inst = loader_instance(&l, ComponentUid::Fabric, profile);
        let err = l
            .prepare(&inst, &s.global, &account(), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PrepareError::LoaderNeedsReinstall { .. }),
            "{err}"
        );
    }
    // Unparseable profile too.
    let dir = s.data.join("versions").join(PROFILE_ID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{PROFILE_ID}.json")), "{ nope").unwrap();
    let inst = loader_instance(&l, ComponentUid::Fabric, Some(PROFILE_ID));
    let err = l
        .prepare(&inst, &s.global, &account(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, PrepareError::LoaderNeedsReinstall { .. }));
}
