use patina_core::modpack::{ArchiveLimits, inspect_archive, inspect_bytes};
use serde_json::{Value, json};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;

fn manifest() -> Value {
    json!({"formatVersion":1,"game":"minecraft","versionId":"1","name":"Small pack","dependencies":{"minecraft":"1.21.1","fabric-loader":"0.16.10"},"files":[
    {"path":"mods/required.jar","hashes":{"sha1":"0000000000000000000000000000000000000000","sha512":"0".repeat(128)},"downloads":["https://cdn.modrinth.com/data/a/versions/b/a.jar"],"fileSize":1},
    {"path":"mods/optional.jar","hashes":{"sha1":"0".repeat(40),"sha512":"0".repeat(128)},"downloads":["https://cdn.modrinth.com/o.jar"],"fileSize":1,"env":{"client":"optional","server":"required"}},
    {"path":"mods/server.jar","hashes":{"sha1":"0".repeat(40),"sha512":"0".repeat(128)},"downloads":["https://cdn.modrinth.com/s.jar"],"fileSize":1,"env":{"client":"unsupported","server":"required"}}
    ]})
}
fn archive(m: &Value, entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("modrinth.index.json", SimpleFileOptions::default())
        .unwrap();
    z.write_all(&serde_json::to_vec(m).unwrap()).unwrap();
    for (p, b) in entries {
        z.start_file(*p, SimpleFileOptions::default()).unwrap();
        z.write_all(b).unwrap();
    }
    z.finish().unwrap().into_inner()
}
#[test]
fn reads_client_manifest_and_override_precedence() {
    let p = inspect_bytes(
        archive(
            &manifest(),
            &[
                ("overrides/config/a.txt", b"base"),
                ("client-overrides/config/a.txt", b"client"),
            ],
        ),
        ArchiveLimits::default(),
    )
    .unwrap();
    assert_eq!(p.name(), "Small pack");
    assert_eq!(p.mc_version(), "1.21.1");
    assert_eq!(p.files().len(), 2);
    assert!(
        p.files()
            .iter()
            .find(|f| f.path() == "mods/optional.jar")
            .unwrap()
            .is_optional()
    );
    assert_eq!(
        p.overrides().get("config/a.txt").unwrap().as_slice(),
        b"client"
    );
}
#[test]
fn rejects_unsupported_loader_without_vanilla_fallback() {
    for d in [
        json!({"minecraft":"1.21.1","quilt-loader":"0.27"}),
        json!({"minecraft":"1.21.1","fabric-loader":"1","forge":"2"}),
    ] {
        let mut m = manifest();
        m["dependencies"] = d;
        assert!(inspect_bytes(archive(&m, &[]), ArchiveLimits::default()).is_err());
    }
}
#[test]
fn rejects_unsafe_or_ambiguous_paths() {
    for path in [
        "../x",
        "/abs",
        "a/../b",
        "a//b",
        "a\\b",
        "C:/x",
        "mods/A.jar",
        "overrides/../../.patina-config.lock",
        "config/.",
        "config/..",
        "CON.txt",
        "a/./b",
    ] {
        let mut m = manifest();
        m["files"][0]["path"] = json!(path);
        if path == "mods/A.jar" {
            m["files"][1]["path"] = json!("mods/a.jar");
        }
        assert!(
            inspect_bytes(archive(&m, &[]), ArchiveLimits::default()).is_err(),
            "{path}"
        );
    }
    assert!(
        inspect_bytes(
            archive(
                &manifest(),
                &[("overrides/config", b"x"), ("overrides/config/a", b"y")]
            ),
            ArchiveLimits::default()
        )
        .is_err()
    );
}
#[test]
fn rejects_archive_limits_and_symlink_entries() {
    let bytes = archive(&manifest(), &[("overrides/a", b"123456789")]);
    assert!(inspect_bytes(bytes, ArchiveLimits::new(2, 8, 1024 * 1024)).is_err());
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("modrinth.index.json", SimpleFileOptions::default())
        .unwrap();
    z.write_all(&serde_json::to_vec(&manifest()).unwrap())
        .unwrap();
    z.add_symlink("overrides/link", "/tmp", SimpleFileOptions::default())
        .unwrap();
    assert!(inspect_bytes(z.finish().unwrap().into_inner(), ArchiveLimits::default()).is_err());
}
#[test]
fn unsupported_zip_and_preview_is_immutable() {
    assert!(
        inspect_bytes(
            archive(&json!({"manifestType":"minecraftModpack"}), &[]),
            ArchiveLimits::default()
        )
        .is_err()
    );
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("a.mrpack");
    std::fs::write(&path, archive(&manifest(), &[("overrides/a", b"old")])).unwrap();
    let p = inspect_archive(&path).unwrap();
    std::fs::write(&path, b"changed").unwrap();
    assert_eq!(p.overrides()["a"], b"old");
}

#[test]
fn rejects_overlarge_manifest_downloads_before_preview() {
    let mut m = manifest();
    m["files"][0]["fileSize"] = json!(u64::MAX);
    assert!(inspect_bytes(archive(&m, &[]), ArchiveLimits::default()).is_err());
}

#[test]
fn reads_bettermc_euphoria_dotfile_and_legitimate_hidden_directories() {
    let preview = inspect_bytes(
        archive(
            &manifest(),
            &[
                (
                    "overrides/config/euphoria_patcher/.data.json",
                    br#"{"enabled":true}"#,
                ),
                (
                    "overrides/.config/.cache/state.json",
                    b"hidden directory content",
                ),
            ],
        ),
        ArchiveLimits::default(),
    )
    .unwrap();
    assert_eq!(
        preview.overrides()["config/euphoria_patcher/.data.json"],
        br#"{"enabled":true}"#
    );
    assert_eq!(
        preview.overrides()[".config/.cache/state.json"],
        b"hidden directory content"
    );
}
