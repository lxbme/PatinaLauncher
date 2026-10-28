use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::download::DownloadJob;
use crate::fsutil::safe_join;

pub const RESOURCES_URL: &str = "https://resources.download.minecraft.net";

#[derive(Clone, Debug, Deserialize)]
pub struct AssetObject {
    pub hash: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AssetIndex {
    pub objects: BTreeMap<String, AssetObject>,
    #[serde(default)]
    pub map_to_resources: bool,
    #[serde(default, rename = "virtual")]
    pub is_virtual: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetLayout {
    Modern,
    Virtual(PathBuf),
    Resources(PathBuf),
}

/// Object hashes are SHA-1: exactly 40 ASCII hex characters.
fn is_valid_hash(hash: &str) -> bool {
    hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

fn object_path(assets_dir: &Path, hash: &str) -> PathBuf {
    assets_dir.join("objects").join(&hash[..2]).join(hash)
}

pub fn asset_jobs(index: &AssetIndex, assets_dir: &Path) -> Vec<DownloadJob> {
    index
        .objects
        .values()
        .filter(|o| is_valid_hash(&o.hash))
        .map(|o| DownloadJob {
            url: format!("{RESOURCES_URL}/{}/{}", &o.hash[..2], o.hash),
            dest: object_path(assets_dir, &o.hash),
            sha1: Some(o.hash.clone()),
            size: Some(o.size),
        })
        .collect()
}

pub fn asset_layout(
    index: &AssetIndex,
    index_id: &str,
    assets_dir: &Path,
    game_dir: &Path,
) -> AssetLayout {
    if index.is_virtual {
        AssetLayout::Virtual(assets_dir.join("virtual").join(index_id))
    } else if index.map_to_resources {
        AssetLayout::Resources(game_dir.join("resources"))
    } else {
        AssetLayout::Modern
    }
}

pub fn materialize_legacy(
    index: &AssetIndex,
    assets_dir: &Path,
    layout: &AssetLayout,
) -> io::Result<()> {
    let root = match layout {
        AssetLayout::Modern => return Ok(()),
        AssetLayout::Virtual(p) | AssetLayout::Resources(p) => p,
    };
    for (name, obj) in &index.objects {
        if !is_valid_hash(&obj.hash) {
            continue;
        }
        // Reject names that would escape the target root.
        let Some(target) = safe_join(root, name) else {
            continue;
        };
        if std::fs::metadata(&target).is_ok_and(|m| m.len() == obj.size) {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(object_path(assets_dir, &obj.hash), &target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "bdf48ef6b5d0d23bbb02e17d04865216179f510a";

    fn index(virt: bool, res: bool) -> AssetIndex {
        let json = format!(
            r#"{{"objects":{{"minecraft/sounds/a.ogg":{{"hash":"{HASH}","size":3}}}},"virtual":{virt},"map_to_resources":{res}}}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn asset_paths_and_layouts() {
        let assets = Path::new("/assets");
        let game = Path::new("/game");
        let jobs = asset_jobs(&index(false, false), assets);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].dest, assets.join("objects/bd").join(HASH));
        assert_eq!(jobs[0].url, format!("{RESOURCES_URL}/bd/{HASH}"));
        assert_eq!(jobs[0].sha1.as_deref(), Some(HASH));
        assert_eq!(jobs[0].size, Some(3));

        assert_eq!(
            asset_layout(&index(true, false), "legacy", assets, game),
            AssetLayout::Virtual(assets.join("virtual/legacy"))
        );
        assert_eq!(
            asset_layout(&index(false, true), "pre-1.6", assets, game),
            AssetLayout::Resources(game.join("resources"))
        );
        assert_eq!(
            asset_layout(&index(false, false), "12", assets, game),
            AssetLayout::Modern
        );
    }

    #[test]
    fn index_defaults_flags_to_false() {
        let i: AssetIndex = serde_json::from_str(r#"{"objects":{}}"#).unwrap();
        assert!(!i.is_virtual && !i.map_to_resources);
    }

    #[test]
    fn materialize_virtual_copies_named_files() {
        let tmp = tempfile::tempdir().unwrap();
        let assets = tmp.path().join("assets");
        let obj = assets.join("objects/bd").join(HASH);
        std::fs::create_dir_all(obj.parent().unwrap()).unwrap();
        std::fs::write(&obj, b"abc").unwrap();
        let layout = asset_layout(&index(true, false), "legacy", &assets, tmp.path());
        materialize_legacy(&index(true, false), &assets, &layout).unwrap();
        let target = assets.join("virtual/legacy/minecraft/sounds/a.ogg");
        assert_eq!(std::fs::read(&target).unwrap(), b"abc");
        // second run skips (same size) without error
        materialize_legacy(&index(true, false), &assets, &layout).unwrap();
    }

    #[test]
    fn materialize_modern_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        materialize_legacy(&index(false, false), tmp.path(), &AssetLayout::Modern).unwrap();
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn only_40_hex_char_hashes_are_accepted() {
        let json = format!(
            r#"{{"objects":{{
            "good":{{"hash":"{HASH}","size":3}},
            "short":{{"hash":"bd","size":3}},
            "traversal":{{"hash":"../../../../etc/passwd/aaaaaaaaaaaaaaaaaaaa","size":3}},
            "nonhex":{{"hash":"zzf48ef6b5d0d23bbb02e17d04865216179f510a","size":3}},
            "long":{{"hash":"{HASH}00","size":3}},
            "sep":{{"hash":"bd/48ef6b5d0d23bbb02e17d04865216179f510a","size":3}}
            }},"virtual":true}}"#
        );
        let index: AssetIndex = serde_json::from_str(&json).unwrap();
        let jobs = asset_jobs(&index, Path::new("/assets"));
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0].sha1.as_deref(), Some(HASH));

        // Legacy materialization ignores the bad entries instead of copying them.
        let tmp = tempfile::tempdir().unwrap();
        let assets = tmp.path().join("assets");
        let obj = assets.join("objects/bd").join(HASH);
        std::fs::create_dir_all(obj.parent().unwrap()).unwrap();
        std::fs::write(&obj, b"abc").unwrap();
        let layout = asset_layout(&index, "legacy", &assets, tmp.path());
        materialize_legacy(&index, &assets, &layout).unwrap();
        let virt = assets.join("virtual/legacy");
        let names: Vec<_> = std::fs::read_dir(&virt)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("good")]);
    }
}
