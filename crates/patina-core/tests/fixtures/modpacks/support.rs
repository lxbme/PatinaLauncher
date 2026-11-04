use patina_core::{
    config::GlobalConfig,
    download::{Downloader, Mirror},
    instance::InstanceStore,
    modpack::{ArchiveLimits, PackPreview, PackService, inspect_bytes},
    paths::DataDirs,
    prepare::Launcher,
};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use sha2::Sha512;
use std::{
    io::{Cursor, Write},
    sync::Arc,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};
use zip::write::SimpleFileOptions;
pub fn pack(version: &str, entries: &[(&str, &[u8])]) -> PackPreview {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("modrinth.index.json", SimpleFileOptions::default())
        .unwrap();
    z.write_all(&serde_json::to_vec(&json!({"formatVersion":1,"game":"minecraft","versionId":version,"name":"Fixture pack","dependencies":{"minecraft":"fixture"},"files":[]})).unwrap()).unwrap();
    for (p, b) in entries {
        z.start_file(format!("overrides/{p}"), SimpleFileOptions::default())
            .unwrap();
        z.write_all(b).unwrap();
    }
    inspect_bytes(z.finish().unwrap().into_inner(), ArchiveLimits::default()).unwrap()
}
pub fn manifest_pack(files: Vec<Value>) -> PackPreview {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("modrinth.index.json", SimpleFileOptions::default())
        .unwrap();
    z.write_all(&serde_json::to_vec(&json!({"formatVersion":1,"game":"minecraft","versionId":"1","name":"Exact","dependencies":{"minecraft":"fixture"},"files":files})).unwrap()).unwrap();
    inspect_bytes(z.finish().unwrap().into_inner(), ArchiveLimits::default()).unwrap()
}
pub fn file(p: &str, body: &[u8], url: &str) -> Value {
    json!({"path":p,"hashes":{"sha1":hex::encode(Sha1::digest(body)),"sha512":hex::encode(Sha512::digest(body))},"fileSize":body.len(),"downloads":[url]})
}
pub struct Setup {
    pub tmp: tempfile::TempDir,
    pub server: MockServer,
    pub service: PackService,
    pub store: InstanceStore,
    pub launcher: Arc<Launcher>,
}
impl Setup {
    pub async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let store = InstanceStore::new(data.instances());
        let server = MockServer::start().await;
        let index = serde_json::to_vec(&json!({"objects":{}})).unwrap();
        let jar = b"client";
        let v = json!({"id":"fixture","type":"release","mainClass":"net.minecraft.client.main.Main","minecraftArguments":"--username ${auth_player_name}","assets":"fixture","assetIndex":{"id":"fixture","url":"https://resources.download.minecraft.net/index","sha1":hex::encode(Sha1::digest(&index)),"size":index.len(),"totalSize":0},"downloads":{"client":{"url":"https://piston-data.mojang.com/client.jar","sha1":hex::encode(Sha1::digest(jar)),"size":jar.len()}},"libraries":[],"javaVersion":{"component":"jre-legacy","majorVersion":8}});
        let v = serde_json::to_vec(&v).unwrap();
        let manifest = json!({"latest":{"release":"fixture","snapshot":"fixture"},"versions":[{"id":"fixture","type":"release","url":"https://piston-meta.mojang.com/fixture.json","sha1":hex::encode(Sha1::digest(&v)),"releaseTime":"2026-01-01T00:00:00Z"}]});
        for (p, b) in [
            (
                "/mc/game/version_manifest_v2.json",
                serde_json::to_vec(&manifest).unwrap(),
            ),
            ("/fixture.json", v),
            ("/index", index),
            ("/client.jar", jar.to_vec()),
        ] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b))
                .mount(&server)
                .await;
        }
        let dl = Downloader::new(
            reqwest::Client::new(),
            Mirror {
                id: "fixture".into(),
                rules: [
                    "https://piston-meta.mojang.com",
                    "https://piston-data.mojang.com",
                    "https://resources.download.minecraft.net",
                    "https://cdn.modrinth.com",
                ]
                .into_iter()
                .map(|h| (h.into(), server.uri()))
                .collect(),
            },
        )
        .with_retry(1, std::time::Duration::ZERO);
        let launcher = Arc::new(Launcher {
            data,
            downloader: dl,
        });
        let mut global = GlobalConfig::default();
        let java = tmp.path().join("java");
        std::fs::write(&java,"#!/bin/sh\necho '    java.version = 1.8.0_202' >&2\necho '    sun.arch.data.model = 64' >&2\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        global.java.java_path = Some(java);
        let service = PackService::new(launcher.clone(), global);
        Self {
            tmp,
            server,
            service,
            store,
            launcher,
        }
    }
    pub async fn serve(&self, p: &str, b: &[u8]) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b.to_vec()))
            .mount(&self.server)
            .await;
    }
}

pub async fn online_pack(s: &Setup, project: &str, version: &str) -> PackPreview {
    use patina_core::content::online::{
        ContentFile, ContentVersion, FileHashes, Platform, ProjectRef, ReleaseChannel, VersionRef,
    };
    let bytes = online_archive(version);
    let path = format!("/data/{project}/versions/{version}/pack.mrpack");
    s.serve(&path, &bytes).await;
    let v = ContentVersion::new(
        VersionRef::new(ProjectRef::new(Platform::Modrinth, project), version),
        version,
        ReleaseChannel::Release,
    )
    .with_files(vec![ContentFile::new(
        "pack.mrpack",
        bytes.len() as u64,
        FileHashes::new(
            Some(hex::encode(Sha1::digest(&bytes))),
            Some(hex::encode(Sha512::digest(&bytes))),
            None,
        ),
        Some(format!("https://cdn.modrinth.com{path}")),
        "https://modrinth.com/modpack/fixture",
    )]);
    let (_tx, c) = tokio::sync::watch::channel(false);
    s.service.preview_online(v, c, None).await.unwrap()
}

pub fn online_archive(version: &str) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("modrinth.index.json", SimpleFileOptions::default())
        .unwrap();
    z.write_all(&serde_json::to_vec(&json!({"formatVersion":1,"game":"minecraft","versionId":version,"name":"Online fixture","dependencies":{"minecraft":"fixture"},"files":[]})).unwrap()).unwrap();
    z.finish().unwrap().into_inner()
}
