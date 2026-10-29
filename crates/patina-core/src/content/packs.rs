//! Resource packs and shader packs found in an instance.
//!
//! Packs are untrusted: every file read goes through a hard cap, declared
//! sizes are never believed, and no input may panic. A pack whose metadata
//! cannot be read is still listed, just without metadata.
//!
//! All scans are blocking; the GUI must call them through
//! `tokio::task::spawn_blocking`.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;
use zip::ZipArchive;

use super::{
    ENTRY_MAX_BYTES, ICON_MAX_BYTES, MAX_ZIP_ENTRIES, PNG_SIGNATURE, list_sorted, read_capped,
    read_file_capped, read_png_file,
};

/// A resource pack or shader pack (zip file or folder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackEntry {
    /// File or folder name as shown in the folder (zip packs keep `.zip`).
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    /// PNG bytes of `pack.png`.
    pub icon: Option<Vec<u8>>,
    pub is_dir: bool,
}

fn is_zip_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// Resource packs: `*.zip` files and folders, with `pack.mcmeta`
/// description and `pack.png` icon.
pub fn scan_resource_packs(dir: &Path) -> io::Result<Vec<PackEntry>> {
    Ok(list_sorted(dir)?
        .into_iter()
        .filter(|(name, _, is_dir)| *is_dir || is_zip_name(name))
        .map(|(name, path, is_dir)| {
            let (mcmeta, icon) = if is_dir {
                (
                    read_file_capped(&path.join("pack.mcmeta"), ENTRY_MAX_BYTES),
                    read_png_file(&path.join("pack.png")),
                )
            } else {
                read_zip_pack(&path)
            };
            PackEntry {
                description: mcmeta.as_deref().and_then(parse_description),
                name,
                path,
                icon,
                is_dir,
            }
        })
        .collect())
}

/// Shader packs: `*.zip` files and folders; option files such as
/// `<name>.zip.txt` are skipped. Names only.
pub fn scan_shader_packs(dir: &Path) -> io::Result<Vec<PackEntry>> {
    Ok(list_sorted(dir)?
        .into_iter()
        .filter(|(name, _, is_dir)| *is_dir || is_zip_name(name))
        .map(|(name, path, is_dir)| PackEntry {
            name,
            path,
            description: None,
            icon: None,
            is_dir,
        })
        .collect())
}

fn read_zip_pack(path: &Path) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let Some(mut ar) = File::open(path)
        .ok()
        .and_then(|f| ZipArchive::new(f).ok())
        .filter(|ar| ar.len() <= MAX_ZIP_ENTRIES)
    else {
        return (None, None);
    };
    let mcmeta = read_capped(&mut ar, "pack.mcmeta", ENTRY_MAX_BYTES);
    let icon =
        read_capped(&mut ar, "pack.png", ICON_MAX_BYTES).filter(|b| b.starts_with(&PNG_SIGNATURE));
    (mcmeta, icon)
}

/// Extracts the plain-text `pack.description` of a `pack.mcmeta`.
fn parse_description(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let mut raw = String::new();
    flatten_component(v.get("pack")?.get("description")?, &mut raw);
    let text = strip_format_codes(&raw);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Appends the text of a string, text component or array of those.
fn flatten_component(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => out.push_str(s),
        Value::Array(items) => items.iter().for_each(|i| flatten_component(i, out)),
        Value::Object(map) => {
            match map.get("text").and_then(Value::as_str) {
                Some(t) => out.push_str(t),
                None => {
                    if let Some(t) = map.get("translate").and_then(Value::as_str) {
                        out.push_str(t);
                    }
                }
            }
            if let Some(extra) = map.get("extra") {
                flatten_component(extra, out);
            }
        }
        _ => {}
    }
}

/// Removes `§x` formatting codes (the `§` and the following character).
fn strip_format_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\u{a7}' {
            it.next();
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::forge::test_support::write_zip;

    fn png(extra: usize) -> Vec<u8> {
        let mut v = PNG_SIGNATURE.to_vec();
        v.extend(std::iter::repeat_n(7u8, extra));
        v
    }

    fn mcmeta(desc: &str) -> String {
        format!(r#"{{"pack":{{"pack_format":15,"description":{desc}}}}}"#)
    }

    fn desc(json: &str) -> Option<String> {
        parse_description(mcmeta(json).as_bytes())
    }

    #[test]
    fn resource_pack_description_forms() {
        assert_eq!(desc(r#""Plain""#).as_deref(), Some("Plain"));
        assert_eq!(
            desc(r#"{"text":"A","extra":[{"text":"B","extra":["C"]},{"text":"D"}]}"#).as_deref(),
            Some("ABCD")
        );
        assert_eq!(
            desc(r#"[{"text":"X"}, "Y", ["Z"]]"#).as_deref(),
            Some("XYZ")
        );
        assert_eq!(
            desc(r#"{"translate":"pack.desc.key"}"#).as_deref(),
            Some("pack.desc.key")
        );
        assert_eq!(
            desc(r#""§6Gold §lbold§r end""#).as_deref(),
            Some("Gold bold end")
        );
        // BOM, then a trailing lone section sign.
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend(mcmeta(r#""Hi§""#).as_bytes());
        assert_eq!(parse_description(&bom).as_deref(), Some("Hi"));
        assert_eq!(desc("42"), None);
        assert_eq!(desc(r#""   ""#), None);
        assert_eq!(parse_description(b"not json"), None);
    }

    #[test]
    fn section_sign_edge_cases() {
        // Section sign followed by a multi-byte char eats the whole char.
        assert_eq!(desc(r#""a\u00a7\u4e16b""#).as_deref(), Some("ab"));
        assert_eq!(strip_format_codes("\u{a7}\u{a7}x"), "x");
    }

    #[test]
    fn deeply_nested_extra_does_not_panic() {
        for depth in [100, 127, 200, 5000] {
            let mut j = String::from(r#""end""#);
            for _ in 0..depth {
                j = format!(r#"{{"text":"a","extra":[{j}]}}"#);
            }
            let _ = desc(&j); // too deep for serde_json: None, never a panic
        }
        assert!(desc(r#"{"text":"a","extra":[{"text":"b"}]}"#).is_some());
    }

    #[test]
    fn exact_name_tie_break() {
        let tmp = tempfile::tempdir().unwrap();
        for n in ["b", "B", "a"] {
            std::fs::create_dir(tmp.path().join(n)).unwrap();
        }
        let names: Vec<_> = scan_resource_packs(tmp.path())
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["a", "B", "b"]);
    }

    #[test]
    fn zip_and_folder_packs() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let icon = png(10);
        write_zip(
            &d.join("beta.ZIP"),
            &[
                ("pack.mcmeta", mcmeta(r#""Zipped""#).as_bytes()),
                ("pack.png", &icon),
            ],
        );
        let folder = d.join("Alpha");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("pack.mcmeta"), mcmeta(r#""Folder""#)).unwrap();
        std::fs::write(folder.join("pack.png"), &icon).unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        std::fs::write(d.join("broken.zip"), "garbage").unwrap();
        // Bad icon (no PNG signature) and oversized icon are dropped.
        let bad = d.join("bad");
        std::fs::create_dir(&bad).unwrap();
        std::fs::write(bad.join("pack.png"), b"nope").unwrap();
        let big = d.join("big");
        std::fs::create_dir(&big).unwrap();
        std::fs::write(big.join("pack.png"), png(ICON_MAX_BYTES)).unwrap();

        let packs = scan_resource_packs(d).unwrap();
        let names: Vec<_> = packs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "bad", "beta.ZIP", "big", "broken.zip"]);
        assert!(packs[0].is_dir);
        assert_eq!(packs[0].description.as_deref(), Some("Folder"));
        assert_eq!(packs[0].icon.as_deref(), Some(icon.as_slice()));
        assert_eq!(packs[1].icon, None);
        assert!(!packs[2].is_dir);
        assert_eq!(packs[2].description.as_deref(), Some("Zipped"));
        assert_eq!(packs[2].icon.as_deref(), Some(icon.as_slice()));
        assert_eq!(packs[3].icon, None);
        assert_eq!(packs[4].description, None);
        assert_eq!(packs[4].icon, None);

        assert!(scan_resource_packs(&d.join("missing")).unwrap().is_empty());
    }

    #[test]
    fn shader_packs_skip_option_files() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write_zip(&d.join("Complementary.zip"), &[("shaders/a.fsh", b"x")]);
        std::fs::write(d.join("Complementary.zip.txt"), "opt=1").unwrap();
        std::fs::create_dir(d.join("bsl")).unwrap();
        std::fs::write(d.join("readme.md"), "x").unwrap();
        let shaders = scan_shader_packs(d).unwrap();
        let got: Vec<_> = shaders
            .iter()
            .map(|p| (p.name.as_str(), p.is_dir))
            .collect();
        assert_eq!(got, [("bsl", true), ("Complementary.zip", false)]);
        assert!(scan_shader_packs(&d.join("missing")).unwrap().is_empty());
    }

    #[test]
    fn hostile_zip_pack_is_listed_without_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let big = vec![b' '; ENTRY_MAX_BYTES + 10];
        write_zip(&tmp.path().join("p.zip"), &[("pack.mcmeta", &big)]);
        let packs = scan_resource_packs(tmp.path()).unwrap();
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].description, None);
    }
}
