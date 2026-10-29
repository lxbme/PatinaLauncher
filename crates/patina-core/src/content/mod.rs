//! Content managed inside an instance (mods, packs, worlds).
//!
//! Shared hostile-input helpers live here: everything read from disk or from
//! an archive goes through a hard size cap, and no declared size is trusted.

pub mod mods;
mod nbt;
pub mod packs;
pub mod saves;

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Largest accepted icon, in bytes.
pub const ICON_MAX_BYTES: usize = 1024 * 1024;
/// Largest metadata entry (after decompression), in bytes.
pub const ENTRY_MAX_BYTES: usize = 4 * 1024 * 1024;
/// An archive with more entries than this is rejected outright.
pub const MAX_ZIP_ENTRIES: usize = 65536;

pub(crate) const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

pub(crate) type Jar = zip::ZipArchive<File>;

/// Reads one zip entry fully, refusing more than `limit` decompressed bytes
/// whatever the zip header declares.
pub(crate) fn read_capped(ar: &mut Jar, name: &str, limit: usize) -> Option<Vec<u8>> {
    let f = ar.by_name(name).ok()?;
    if f.is_dir() {
        return None;
    }
    let mut buf = Vec::new();
    f.take(limit as u64 + 1).read_to_end(&mut buf).ok()?;
    (buf.len() <= limit).then_some(buf)
}

/// Reads a regular file through a hard cap (never trusts the file size).
/// Anything that is not a regular file (a FIFO or device would block or
/// never end) yields `None`; symlinks to regular files are followed.
pub(crate) fn read_file_capped(path: &Path, limit: usize) -> Option<Vec<u8>> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    File::open(path)
        .ok()?
        .take(limit as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    (buf.len() <= limit).then_some(buf)
}

/// Reads a PNG icon from a file, size-capped and signature-checked.
pub(crate) fn read_png_file(path: &Path) -> Option<Vec<u8>> {
    let bytes = read_file_capped(path, ICON_MAX_BYTES)?;
    bytes.starts_with(&PNG_SIGNATURE).then_some(bytes)
}

/// Lists `(name, path, is_dir)` of regular files and directories (symlinks
/// followed), sorted by name case-insensitively, ties by exact name. A
/// missing directory yields an empty list.
pub(crate) fn list_sorted(dir: &Path) -> io::Result<Vec<(String, PathBuf, bool)>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let path = entry.path();
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        if md.is_dir() || md.is_file() {
            out.push((name, path, md.is_dir()));
        }
    }
    out.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn fifo_is_not_read() {
        let d = tempfile::tempdir().unwrap();
        let fifo = d.path().join("pack.mcmeta");
        let Ok(st) = std::process::Command::new("mkfifo").arg(&fifo).status() else {
            return;
        };
        assert!(st.success());
        // Would block forever if opened for reading.
        assert_eq!(read_file_capped(&fifo, 1024), None);
        assert_eq!(read_png_file(&fifo), None);
    }
}
