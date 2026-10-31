//! Disk use of directory trees: a bounded walk that never follows symbolic
//! links ([`Walk`]), and sizes as file managers show them ([`format_size`]).
//! Shared by the status bar and the instance overview. Blocking: call it
//! through `core_bridge::run_blocking`.

use std::path::Path;

/// A disk-use walk with an entry budget.
#[derive(Debug)]
pub struct Walk {
    bytes: u64,
    left: usize,
}

impl Walk {
    /// A walk that visits at most `budget` directory entries in all.
    pub fn new(budget: usize) -> Self {
        Self {
            bytes: 0,
            left: budget,
        }
    }

    /// Bytes of the regular files counted so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Adds the regular files under `root`, without following symbolic
    /// links (a linked `root` counts nothing). False once the budget ran out.
    pub fn add_tree(&mut self, root: &Path) -> bool {
        match std::fs::symlink_metadata(root) {
            Ok(m) if m.is_dir() => {}
            _ => return true,
        }
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if self.left == 0 {
                    return false;
                }
                self.left -= 1;
                // Neither `file_type` nor `metadata` of a directory entry
                // follows a symbolic link.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    stack.push(entry.path());
                } else if kind.is_file()
                    && let Ok(m) = entry.metadata()
                {
                    self.bytes += m.len();
                }
            }
        }
        true
    }
}

/// `bytes` in decimal units with one decimal place (`25.1 GB`), as file
/// managers show sizes.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.;
    let mut unit = 0;
    // Round first, so 999.96 KB reads 1.0 MB rather than 1000.0 KB.
    while (value * 10.).round() / 10. >= 1000. && unit + 1 < UNITS.len() {
        value /= 1000.;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_a_file_manager() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1000), "1.0 KB");
        assert_eq!(format_size(1_536), "1.5 KB");
        assert_eq!(format_size(999_960), "1.0 MB");
        assert_eq!(format_size(25_100_000_000), "25.1 GB");
        assert_eq!(format_size(u64::MAX), "18446.7 PB");
    }

    #[test]
    fn walk_counts_regular_files_within_its_budget() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        std::fs::write(tmp.path().join("a/one"), [0u8; 10]).unwrap();
        std::fs::write(tmp.path().join("a/b/two"), [0u8; 20]).unwrap();
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("big"), [0u8; 5000]).unwrap();
            std::os::unix::fs::symlink(outside.path(), tmp.path().join("linked")).unwrap();
            std::os::unix::fs::symlink(outside.path().join("big"), tmp.path().join("l")).unwrap();
            let mut walk = Walk::new(usize::MAX);
            assert!(walk.add_tree(tmp.path()));
            assert_eq!(walk.bytes(), 30, "links are not followed");
        }
        let mut short = Walk::new(2);
        assert!(!short.add_tree(tmp.path()));
        assert!(short.bytes() < 30);
    }
}
