//! Path containment for paths that come from downloaded metadata, and
//! atomic file replacement.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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

/// Deletes stale temp files under `root` (recursively, never following
/// symlinks) and returns how many were removed. A file qualifies when its
/// name has exactly the shape [`unique_sibling`] produces
/// (`<name>.<pid>.<n>.tmp`, or `<name>.<pid>.<n>[.pack.xz|.pack|.unpacked].part`),
/// its `<pid>` is neither this process nor a live process, and it was last
/// modified more than `older_than` ago.
///
/// `root` must be a directory Patina owns: any file of that shape below it is
/// deleted, so never pass a game directory or anything holding user files
/// (see [`sweep_owned_temps`]). A missing `root` yields `Ok(0)`; an
/// unreadable `root` is an error; problems with deeper entries are skipped.
pub fn sweep_stale_temps(root: &Path, older_than: Duration) -> std::io::Result<usize> {
    let mut sw = Sweeper::new(older_than);
    sw.dir(root, true, true)?;
    Ok(sw.removed)
}

/// Sweeps only the places in a Patina data directory where Patina writes temp
/// files: `versions/`, `libraries/`, `assets/` and `runtimes/` recursively,
/// and the top level of each `instances/<id>/` (never `minecraft/` or any
/// other subdirectory, which hold user game files). Missing directories are
/// skipped.
pub fn sweep_owned_temps(data_dir: &Path, min_age: Duration) -> std::io::Result<usize> {
    let mut sw = Sweeper::new(min_age);
    for name in ["versions", "libraries", "assets", "runtimes"] {
        sw.dir(&data_dir.join(name), true, true)?;
    }
    let instances = data_dir.join("instances");
    let entries = match std::fs::read_dir(&instances) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(sw.removed),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            sw.dir(&entry.path(), false, false)?;
        }
    }
    Ok(sw.removed)
}

struct Sweeper {
    older_than: Duration,
    sys: Option<sysinfo::System>,
    removed: usize,
}

impl Sweeper {
    fn new(older_than: Duration) -> Self {
        Self {
            older_than,
            sys: None,
            removed: 0,
        }
    }

    /// Sweeps `dir`. With `strict`, an unreadable `dir` (other than missing)
    /// is an error; otherwise it is skipped.
    fn dir(&mut self, dir: &Path, recurse: bool, strict: bool) -> std::io::Result<()> {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if strict => return Err(e),
            Err(e) => {
                tracing::debug!("temp sweep skipped {}: {e}", dir.display());
                return Ok(());
            }
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = entry.path();
            if ft.is_dir() {
                if recurse {
                    self.dir(&path, true, false)?;
                }
                continue;
            }
            let Some(pid) = entry.file_name().to_str().and_then(temp_pid) else {
                continue;
            };
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > self.older_than);
            if !old || self.pid_alive(pid) {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => self.removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("could not remove stale temp {}: {e}", path.display()),
            }
        }
        Ok(())
    }

    fn pid_alive(&mut self, pid: u32) -> bool {
        if pid == std::process::id() {
            return true;
        }
        if self.sys.is_none() {
            match sysinfo::System::new() {
                Ok(mut s) => {
                    s.refresh_processes_specifics(
                        sysinfo::ProcessesToUpdate::All,
                        true,
                        sysinfo::ProcessRefreshKind::nothing(),
                    );
                    self.sys = Some(s);
                }
                Err(e) => {
                    // Cannot tell: assume alive so nothing is deleted.
                    tracing::warn!("cannot list processes, keeping temp files: {e}");
                    return true;
                }
            }
        }
        match &self.sys {
            Some(sys) => sys.process(sysinfo::Pid::from_u32(pid)).is_some(),
            None => true,
        }
    }
}

/// The `<pid>` of a file name shaped like a Patina temp file, if it is one.
fn temp_pid(name: &str) -> Option<u32> {
    let (stem, rests): (&str, &[&str]) = if let Some(s) = name.strip_suffix(TMP_SUFFIX) {
        (s, &[""])
    } else {
        let s = name.strip_suffix(PART_SUFFIX)?;
        (s, &["", ".pack.xz", ".pack", ".unpacked"])
    };
    rests.iter().find_map(|rest| {
        let s = stem.strip_suffix(rest)?;
        let (s, counter) = s.rsplit_once('.')?;
        let (base, pid) = s.rsplit_once('.')?;
        let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        (!base.is_empty() && digits(counter) && digits(pid))
            .then(|| pid.parse().ok())
            .flatten()
    })
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
    fn dead_pid() -> u32 {
        // Above any Linux pid_max (4194304), so never a live process.
        4_000_000_000
    }

    fn age(path: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - Duration::from_secs(secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn sweep_removes_only_old_temps() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        std::fs::create_dir_all(d.join("sub/deep")).unwrap();
        let dead = dead_pid();
        let me = std::process::id();
        let stale = [
            format!("a.json.{dead}.0.tmp"),
            format!("sub/lib.jar.{dead}.7.part"),
            format!("sub/deep/f.{dead}.1.pack.xz.part"),
            format!("sub/deep/f.{dead}.2.pack.part"),
            format!("f.{dead}.3.unpacked.part"),
        ];
        let kept = [
            "plain.txt".to_string(),
            "real.part".to_string(),
            "real.tmp".to_string(),
            format!("x.{dead}.tmp"),
            format!("x.{dead}.a.tmp"),
            format!(".{dead}.1.tmp"),
            format!("mine.{me}.1.tmp"),
            format!("fresh.{dead}.9.tmp"),
            format!("fresh.{dead}.9.part"),
        ];
        for n in stale.iter().chain(kept.iter()) {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        for n in stale
            .iter()
            .chain(kept.iter().filter(|n| !n.starts_with("fresh")))
        {
            age(&d.join(n), 3600);
        }
        let n = sweep_stale_temps(d, Duration::from_secs(60)).unwrap();
        assert_eq!(n, stale.len());
        for s in &stale {
            assert!(!d.join(s).exists(), "{s}");
        }
        for k in &kept {
            assert!(d.join(k).exists(), "{k}");
        }
    }

    #[test]
    fn sweep_skips_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join(format!("v.{}.0.tmp", dead_pid()));
        std::fs::write(&victim, b"x").unwrap();
        age(&victim, 3600);
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("dirlink")).unwrap();
        std::os::unix::fs::symlink(&victim, root.join(format!("l.{}.1.tmp", dead_pid()))).unwrap();
        assert_eq!(
            sweep_stale_temps(&root, Duration::from_secs(60)).unwrap(),
            0
        );
        assert!(victim.exists());
    }

    #[test]
    fn sweep_keeps_files_of_live_other_processes() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = std::os::unix::process::parent_id();
        let f = tmp.path().join(format!("live.{parent}.1.tmp"));
        std::fs::write(&f, b"x").unwrap();
        age(&f, 3600);
        assert_eq!(
            sweep_stale_temps(tmp.path(), Duration::from_secs(60)).unwrap(),
            0
        );
        assert!(f.exists());
    }

    #[test]
    fn sweep_missing_root_is_ok_and_file_root_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let none = sweep_stale_temps(&tmp.path().join("nope"), Duration::ZERO);
        assert_eq!(none.unwrap(), 0);
        let file = tmp.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(sweep_stale_temps(&file, Duration::ZERO).is_err());
    }

    #[test]
    fn owned_sweep_never_enters_game_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let name = format!("t.{}.1.tmp", dead_pid());
        let removed = ["versions/1.20", "instances/x", "libraries/a/b"];
        let kept = ["instances/x/minecraft/mods", "instances/x/natives", "other"];
        for dir in removed.iter().chain(kept.iter()) {
            std::fs::create_dir_all(d.join(dir)).unwrap();
            let f = d.join(dir).join(&name);
            std::fs::write(&f, b"x").unwrap();
            age(&f, 3600);
        }
        // assets/ and runtimes/ are absent: skipped.
        let n = sweep_owned_temps(d, Duration::from_secs(60)).unwrap();
        assert_eq!(n, removed.len());
        for dir in removed {
            assert!(!d.join(dir).join(&name).exists(), "{dir}");
        }
        for dir in kept {
            assert!(d.join(dir).join(&name).exists(), "{dir}");
        }
        assert_eq!(
            sweep_owned_temps(&d.join("nope"), Duration::ZERO).unwrap(),
            0
        );
    }
}
