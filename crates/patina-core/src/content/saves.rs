//! Worlds found in an instance's `saves/` folder.
//!
//! `level.dat` is untrusted: it is decompressed through a hard cap and
//! parsed leniently. A world whose metadata cannot be read is still listed.
//!
//! The scan is blocking; the GUI must call it through
//! `tokio::task::spawn_blocking`.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;

use super::nbt::{LevelMeta, read_level_meta};
use super::{list_sorted, read_file_capped, read_png_file};

/// Largest accepted `level.dat` (compressed and decompressed), in bytes.
const LEVEL_DAT_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldEntry {
    pub dir_name: String,
    pub path: PathBuf,
    pub level_name: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub last_played: Option<i64>,
    /// PNG bytes of `icon.png`.
    pub icon: Option<Vec<u8>>,
}

/// Lists every folder under `saves`, newest `last_played` first; worlds
/// without metadata come last, ordered by folder name. A missing directory
/// yields an empty list.
pub fn scan_saves(dir: &Path) -> io::Result<Vec<WorldEntry>> {
    let mut out: Vec<WorldEntry> = list_sorted(dir)?
        .into_iter()
        .filter(|(_, _, is_dir)| *is_dir)
        .map(|(dir_name, path, _)| {
            let meta = read_level(&path.join("level.dat"))
                .or_else(|| read_level(&path.join("level.dat_old")))
                .unwrap_or_default();
            WorldEntry {
                level_name: meta.level_name,
                last_played: meta.last_played,
                icon: read_png_file(&path.join("icon.png")),
                dir_name,
                path,
            }
        })
        .collect();
    // Stable sort keeps the case-insensitive name order from `list_sorted`
    // for ties; the explicit key adds the exact-name tie-break.
    out.sort_by(|a, b| {
        b.last_played
            .is_some()
            .cmp(&a.last_played.is_some())
            .then_with(|| b.last_played.cmp(&a.last_played))
            .then_with(|| {
                a.dir_name
                    .to_lowercase()
                    .cmp(&b.dir_name.to_lowercase())
                    .then_with(|| a.dir_name.cmp(&b.dir_name))
            })
    });
    Ok(out)
}

/// Reads `level.dat` (gzip, or raw NBT when the gzip magic is absent). `None`
/// when the file is unreadable or unparseable; a parseable file that lacks a
/// field simply leaves that field `None`.
fn read_level(path: &Path) -> Option<LevelMeta> {
    let raw = read_file_capped(path, LEVEL_DAT_MAX_BYTES)?;
    let nbt = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        GzDecoder::new(raw.as_slice())
            .take(LEVEL_DAT_MAX_BYTES as u64 + 1)
            .read_to_end(&mut out)
            .ok()?;
        if out.len() > LEVEL_DAT_MAX_BYTES {
            return None;
        }
        out
    } else {
        raw
    };
    read_level_meta(&nbt)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde::Serialize;

    use super::*;

    #[derive(Serialize)]
    struct OutDat {
        #[serde(rename = "Data")]
        data: OutData,
    }

    #[derive(Serialize)]
    struct OutData {
        #[serde(rename = "LevelName")]
        level_name: String,
        #[serde(rename = "LastPlayed")]
        last_played: i64,
        #[serde(rename = "Unknown")]
        unknown: Vec<i32>,
    }

    fn level_dat(name: &str, last: i64) -> Vec<u8> {
        let nbt = fastnbt::to_bytes(&OutDat {
            data: OutData {
                level_name: name.into(),
                last_played: last,
                unknown: vec![1, 2, 3],
            },
        })
        .unwrap();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&nbt).unwrap();
        gz.finish().unwrap()
    }

    fn world(root: &Path, dir: &str, dat: Option<Vec<u8>>) -> PathBuf {
        let p = root.join(dir);
        std::fs::create_dir(&p).unwrap();
        if let Some(d) = dat {
            std::fs::write(p.join("level.dat"), d).unwrap();
        }
        p
    }

    #[test]
    fn saves_read_level_dat() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let old = world(d, "old", Some(level_dat("Old World", 1000)));
        let new = world(d, "new", Some(level_dat("New 世界", 5000)));
        let mut icon = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        icon.extend([1, 2, 3]);
        std::fs::write(new.join("icon.png"), &icon).unwrap();
        std::fs::write(old.join("icon.png"), b"not a png").unwrap();
        std::fs::write(d.join("stray.txt"), "x").unwrap();

        let worlds = scan_saves(d).unwrap();
        let got: Vec<_> = worlds
            .iter()
            .map(|w| (w.dir_name.as_str(), w.level_name.as_deref(), w.last_played))
            .collect();
        assert_eq!(
            got,
            [
                ("new", Some("New 世界"), Some(5000)),
                ("old", Some("Old World"), Some(1000)),
            ]
        );
        assert_eq!(worlds[0].icon.as_deref(), Some(icon.as_slice()));
        assert_eq!(worlds[1].icon, None);
        assert!(scan_saves(&d.join("missing")).unwrap().is_empty());
    }

    #[test]
    fn corrupt_level_dat_still_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        world(d, "zeta", Some(b"garbage".to_vec()));
        world(d, "Alpha", None);
        world(d, "ok", Some(level_dat("Fine", 7)));
        // Corrupt level.dat falls back to level.dat_old.
        let healed = world(d, "healed", Some(b"garbage".to_vec()));
        std::fs::write(healed.join("level.dat_old"), level_dat("Recovered", 9)).unwrap();
        // Valid gzip but not NBT; and a decompression bomb.
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(b"plain text").unwrap();
        world(d, "text", Some(gz.finish().unwrap()));
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&vec![0u8; LEVEL_DAT_MAX_BYTES + 1024])
            .unwrap();
        world(d, "bomb", Some(gz.finish().unwrap()));

        let worlds = scan_saves(d).unwrap();
        let got: Vec<_> = worlds
            .iter()
            .map(|w| (w.dir_name.as_str(), w.level_name.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("healed", Some("Recovered")),
                ("ok", Some("Fine")),
                ("Alpha", None),
                ("bomb", None),
                ("text", None),
                ("zeta", None),
            ]
        );
    }

    fn raw_nbt(name: Option<&str>, last: Option<i64>) -> Vec<u8> {
        #[derive(Serialize)]
        struct D {
            #[serde(rename = "LevelName", skip_serializing_if = "Option::is_none")]
            n: Option<String>,
            #[serde(rename = "LastPlayed", skip_serializing_if = "Option::is_none")]
            l: Option<i64>,
        }
        #[derive(Serialize)]
        struct R {
            #[serde(rename = "Data")]
            d: D,
        }
        fastnbt::to_bytes(&R {
            d: D {
                n: name.map(str::to_owned),
                l: last,
            },
        })
        .unwrap()
    }

    fn gz(b: &[u8]) -> Vec<u8> {
        let mut g = GzEncoder::new(Vec::new(), Compression::default());
        g.write_all(b).unwrap();
        g.finish().unwrap()
    }

    #[test]
    fn partial_metadata_is_kept_without_old_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let only_name = world(d, "a", Some(gz(&raw_nbt(Some("Named"), None))));
        std::fs::write(only_name.join("level.dat_old"), level_dat("Old", 99)).unwrap();
        world(d, "b", Some(gz(&raw_nbt(None, Some(77)))));
        let w = scan_saves(d).unwrap();
        let got: Vec<_> = w
            .iter()
            .map(|w| (w.dir_name.as_str(), w.level_name.as_deref(), w.last_played))
            .collect();
        assert_eq!(got, [("b", None, Some(77)), ("a", Some("Named"), None)]);
    }

    #[test]
    fn uncompressed_level_dat_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        world(tmp.path(), "raw", Some(raw_nbt(Some("Raw"), Some(3))));
        let w = scan_saves(tmp.path()).unwrap();
        assert_eq!(w[0].level_name.as_deref(), Some("Raw"));
        assert_eq!(w[0].last_played, Some(3));
    }

    #[test]
    fn deeply_nested_level_dat_does_not_abort() {
        let tmp = tempfile::tempdir().unwrap();
        // Root{ Data{ LevelName, x: 100k nested compounds } }
        let mut v = vec![10, 0, 0, 10, 0, 4];
        v.extend(b"Data");
        v.extend([8, 0, 9]);
        v.extend(b"LevelName");
        v.extend([0, 4]);
        v.extend(b"Deep");
        for _ in 0..100_000 {
            v.extend([10, 0, 1, b'x']);
        }
        world(tmp.path(), "deep", Some(gz(&v)));
        let w = scan_saves(tmp.path()).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].level_name.as_deref(), Some("Deep"));
    }

    #[test]
    fn ordering_ties() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        world(d, "b", Some(level_dat("x", 5)));
        world(d, "B", Some(level_dat("x", 5)));
        world(d, "a", Some(level_dat("x", 5)));
        world(d, "n2", None);
        world(d, "N1", None);
        let names: Vec<_> = scan_saves(d)
            .unwrap()
            .into_iter()
            .map(|w| w.dir_name)
            .collect();
        assert_eq!(names, ["a", "B", "b", "N1", "n2"]);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_level_dat_does_not_hang() {
        let tmp = tempfile::tempdir().unwrap();
        let w = world(tmp.path(), "w", None);
        let Ok(st) = std::process::Command::new("mkfifo")
            .arg(w.join("level.dat"))
            .status()
        else {
            return;
        };
        assert!(st.success());
        let worlds = scan_saves(tmp.path()).unwrap();
        assert_eq!(worlds.len(), 1);
        assert_eq!(worlds[0].level_name, None);
    }
}
