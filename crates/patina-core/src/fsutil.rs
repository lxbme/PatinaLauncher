//! Path containment for paths that come from downloaded metadata, and
//! atomic file replacement.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// `rel` is a non-empty relative path made only of normal components: no
/// root, no `.` or `..`, no empty segment, no backslash.
pub fn is_safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && rel
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != ".." && !s.contains('\\'))
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// `name` is a single safe path component (a file or directory name).
pub fn is_safe_name(name: &str) -> bool {
    is_safe_rel(name) && !name.contains('/')
}

/// `root.join(rel)` when `rel` is safe (see [`is_safe_rel`]), so the result
/// always stays inside `root`.
pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    is_safe_rel(rel).then(|| root.join(rel))
}

/// Suffixes of every temporary file Patina writes next to its target. All
/// are named `<name>.<pid>.<counter><rest>` by [`unique_sibling`], so a file
/// whose `<pid>` is not a live Patina process is a leftover:
///
/// - `<name>.<pid>.<n>.tmp`: [`replace_atomic`] / [`write_atomic`].
/// - `<name>.<pid>.<n>.part`: downloads (one per attempt) and jar entry
///   extraction ([`replace_atomic_part`]).
/// - `<name>.<pid>.<n>.pack.xz.part`, `<name>.<pid>.<n>.pack.part`,
///   `<name>.<pid>.<n>.unpacked.part`: the legacy Forge `.pack.xz` fallback
///   (the downloaded `.pack.xz.part` gets its own download part file too).
///
/// A sweep therefore covers `*.<pid>.<n>.tmp` and `*.part`.
pub const TEMP_SUFFIXES: [&str; 2] = [TMP_SUFFIX, PART_SUFFIX];
const TMP_SUFFIX: &str = ".tmp";
const PART_SUFFIX: &str = ".part";

/// A file name next to `path` that no other writer in this or another
/// process uses: `<name>.<pid>.<counter><suffix>`.
pub fn unique_sibling(path: &Path, suffix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}{suffix}", std::process::id()));
    path.with_file_name(name)
}

/// A temp file name next to `path` that no other writer uses:
/// `<name>.<pid>.<counter>.tmp` (see [`unique_sibling`]).
pub fn unique_tmp_path(path: &Path) -> PathBuf {
    unique_sibling(path, TMP_SUFFIX)
}

/// A part file name next to `path` that no other writer uses:
/// `<name>.<pid>.<counter>.part` (see [`unique_sibling`]).
pub fn unique_part_path(path: &Path) -> PathBuf {
    unique_sibling(path, PART_SUFFIX)
}

/// Makes `path` by `fill`ing a unique temp file next to it (see
/// [`unique_tmp_path`]) and renaming that over `path`; concurrent writers of
/// one path never share a temp file. The temp file is removed on failure.
pub fn replace_atomic(
    path: &Path,
    fill: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    replace_via(path, unique_tmp_path(path), fill)
}

/// [`replace_atomic`] through a unique `.part` file (see
/// [`unique_part_path`]), for extracted content.
pub fn replace_atomic_part(
    path: &Path,
    fill: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    replace_via(path, unique_part_path(path), fill)
}

fn replace_via(
    path: &Path,
    tmp: PathBuf,
    fill: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let r = fill(&tmp).and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Writes `data` to a unique temp file next to `path`, then renames.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    replace_atomic(path, |tmp| std::fs::write(tmp, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_accepts_plain_relative_paths() {
        let root = Path::new("/root");
        assert_eq!(
            safe_join(root, "org/lwjgl/lwjgl-3.3.3.jar"),
            Some(PathBuf::from("/root/org/lwjgl/lwjgl-3.3.3.jar"))
        );
        assert_eq!(safe_join(root, "1.8.9"), Some(PathBuf::from("/root/1.8.9")));
        assert_eq!(safe_join(root, "a..b"), Some(PathBuf::from("/root/a..b")));
        assert_eq!(
            safe_join(root, ".hidden"),
            Some(PathBuf::from("/root/.hidden"))
        );
    }

    #[test]
    fn safe_join_rejects_escapes() {
        let root = Path::new("/root");
        for bad in [
            "",
            ".",
            "..",
            "./a",
            "a/.",
            "a/./b",
            "../x",
            "a/../../x",
            "a/..",
            "/abs",
            "/",
            "a//b",
            "a/",
            "..\\x",
            "a\\b",
        ] {
            assert_eq!(safe_join(root, bad), None, "{bad:?} was accepted");
            assert!(!is_safe_rel(bad), "{bad:?}");
        }
    }

    #[test]
    fn concurrent_atomic_writes_to_one_path_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("profile.json");
        let bodies: Vec<Vec<u8>> = (0..32u8).map(|i| vec![i; 64 * 1024]).collect();
        for _ in 0..4 {
            let barrier = std::sync::Barrier::new(bodies.len());
            std::thread::scope(|s| {
                let handles: Vec<_> = bodies
                    .iter()
                    .map(|b| {
                        let (barrier, path) = (&barrier, &path);
                        s.spawn(move || {
                            barrier.wait();
                            write_atomic(path, b)
                        })
                    })
                    .collect();
                for h in handles {
                    h.join().unwrap().unwrap();
                }
            });
            let got = std::fs::read(&path).unwrap();
            assert!(bodies.contains(&got));
        }
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("profile.json")]);
    }

    #[test]
    fn failed_atomic_write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // Renaming a file onto a non-empty directory fails.
        let target = dir.path().join("taken");
        std::fs::create_dir_all(target.join("inner")).unwrap();
        assert!(write_atomic(&target, b"x").is_err());
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, vec![std::ffi::OsString::from("taken")]);
    }

    #[test]
    fn temp_paths_are_unique_siblings() {
        let p = Path::new("/d/x.json");
        let (a, b) = (unique_tmp_path(p), unique_tmp_path(p));
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert_eq!(t.parent(), p.parent());
            let name = t.file_name().unwrap().to_str().unwrap();
            assert!(
                name.starts_with("x.json.") && name.ends_with(".tmp"),
                "{name}"
            );
        }
    }

    #[test]
    fn part_paths_are_unique_siblings() {
        let p = Path::new("/d/x.jar");
        let (a, b) = (unique_part_path(p), unique_part_path(p));
        assert_ne!(a, b);
        let pid = std::process::id();
        for t in [&a, &b] {
            assert_eq!(t.parent(), p.parent());
            let name = t.file_name().unwrap().to_str().unwrap();
            let mid = name
                .strip_prefix(&format!("x.jar.{pid}."))
                .and_then(|r| r.strip_suffix(".part"))
                .unwrap_or_else(|| panic!("{name}"));
            assert!(mid.parse::<u64>().is_ok(), "{name}");
        }
        let t = unique_sibling(p, ".pack.xz.part");
        assert!(
            TEMP_SUFFIXES
                .iter()
                .any(|s| t.to_str().unwrap().ends_with(s))
        );
    }

    #[test]
    fn failed_part_replace_leaves_no_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.jar");
        let r = replace_atomic_part(&target, |tmp| {
            std::fs::write(tmp, b"half")?;
            Err(std::io::Error::other("copy failed"))
        });
        assert!(r.is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        replace_atomic_part(&target, |tmp| std::fs::write(tmp, b"ok")).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"ok");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn safe_names_are_single_components() {
        assert!(is_safe_name("1.8.9"));
        assert!(is_safe_name("client-1.12.xml"));
        assert!(!is_safe_name("a/b"));
        assert!(!is_safe_name(".."));
        assert!(!is_safe_name(""));
        assert!(!is_safe_name("/abs"));
    }
}
