//! Edits to files inside an instance's game directory.

use std::io;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, value};

use crate::fsutil::write_atomic;

const KEY: &str = "earlyWindowControl";

fn fml_toml(game_dir: &Path) -> PathBuf {
    game_dir.join("config").join("fml.toml")
}

fn invalid(path: &Path, e: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} is not valid TOML: {e}", path.display()),
    )
}

/// Reads `config/fml.toml`; `None` when the file does not exist.
fn read_doc(path: &Path) -> io::Result<Option<DocumentMut>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    text.parse::<DocumentMut>()
        .map(Some)
        .map_err(|e| invalid(path, e))
}

/// Whether NeoForge's early loading window is turned off in the instance:
/// true iff `config/fml.toml` has `earlyWindowControl = false`. A missing
/// file or key means false; an unparseable file is an `InvalidData` error.
pub fn early_window_disabled(game_dir: &Path) -> io::Result<bool> {
    let path = fml_toml(game_dir);
    Ok(read_doc(&path)?
        .and_then(|d| d.get(KEY).and_then(|v| v.as_bool()))
        .is_some_and(|control| !control))
}

/// Turns the early loading window off (`earlyWindowControl = false`) or on
/// (`true`; the key is kept). Other keys and comments are left as they are
/// and a missing file is created. An existing file that does not parse is
/// left untouched and reported as an `InvalidData` error.
pub fn set_early_window_disabled(game_dir: &Path, disabled: bool) -> io::Result<()> {
    let path = fml_toml(game_dir);
    let mut doc = read_doc(&path)?.unwrap_or_default();
    let mut new = value(!disabled);
    if let (Some(old), Some(new)) = (doc.get(KEY).and_then(|i| i.as_value()), new.as_value_mut()) {
        // Keep the old value's surrounding whitespace and inline comment.
        *new.decor_mut() = old.decor().clone();
    }
    doc[KEY] = new;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(&path, doc.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn fml_toml_edit_preserves_other_keys() {
        let d = dir();
        let path = fml_toml(d.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let src =
            "# keep me\nmaxThreads = -1\nearlyWindowControl = true # inline\n\n[section]\nx = 1\n";
        std::fs::write(&path, src).unwrap();
        assert!(!early_window_disabled(d.path()).unwrap());
        set_early_window_disabled(d.path(), true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep me"));
        assert!(text.contains("maxThreads = -1"));
        assert!(text.contains("[section]\nx = 1"));
        assert!(text.contains("earlyWindowControl = false # inline"));
        assert!(early_window_disabled(d.path()).unwrap());
        set_early_window_disabled(d.path(), false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("earlyWindowControl = true # inline"));
        assert!(text.contains("# keep me"));
        assert!(!early_window_disabled(d.path()).unwrap());
    }

    #[test]
    fn missing_fml_toml_is_created() {
        let d = dir();
        assert!(!early_window_disabled(d.path()).unwrap());
        set_early_window_disabled(d.path(), true).unwrap();
        let text = std::fs::read_to_string(fml_toml(d.path())).unwrap();
        assert!(text.contains("earlyWindowControl = false"));
        assert!(early_window_disabled(d.path()).unwrap());
    }

    #[test]
    fn unparseable_fml_toml_is_not_overwritten() {
        let d = dir();
        let path = fml_toml(d.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "this is = = broken [").unwrap();
        let e = early_window_disabled(d.path()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        let e = set_early_window_disabled(d.path(), true).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "this is = = broken ["
        );
    }
}
