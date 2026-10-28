use std::io;
use std::path::{Path, PathBuf};

pub const CLASSPATH_SEPARATOR: &str = ":";

pub fn default_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("patina"))
}

/// Parses `/proc/meminfo` content into `(MemTotal, MemAvailable)` in MiB.
pub fn parse_meminfo(s: &str) -> (Option<u64>, Option<u64>) {
    fn kib_to_mib(rest: &str) -> Option<u64> {
        let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
        Some(kib / 1024)
    }
    let mut total = None;
    let mut available = None;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = kib_to_mib(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = kib_to_mib(rest);
        }
    }
    (total, available)
}

fn read_meminfo() -> (Option<u64>, Option<u64>) {
    std::fs::read_to_string("/proc/meminfo")
        .map(|s| parse_meminfo(&s))
        .unwrap_or((None, None))
}

pub fn total_memory_mib() -> Option<u64> {
    read_meminfo().0
}

pub fn available_memory_mib() -> Option<u64> {
    read_meminfo().1
}

pub fn rule_os_name() -> &'static str {
    "linux"
}

pub fn rule_arch() -> &'static str {
    if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "x86_64"
    }
}

/// Existing, canonicalized `java` executables from the usual JVM locations,
/// `$JAVA_HOME` and `$PATH`. May contain duplicates only if canonicalization
/// differs; callers dedup anyway.
pub fn java_candidates() -> Vec<PathBuf> {
    let mut raw: Vec<PathBuf> = Vec::new();
    for root in ["/usr/lib/jvm", "/usr/lib64/jvm", "/opt"] {
        if let Ok(rd) = std::fs::read_dir(root) {
            raw.extend(rd.flatten().map(|e| e.path().join("bin").join("java")));
        }
    }
    if let Some(home) = std::env::var_os("JAVA_HOME").filter(|h| !h.is_empty()) {
        raw.push(PathBuf::from(home).join("bin").join("java"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        raw.extend(std::env::split_paths(&path).map(|d| d.join("java")));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for p in raw {
        if let Ok(c) = p.canonicalize()
            && c.is_file()
            && !out.contains(&c)
        {
            out.push(c);
        }
    }
    out
}

pub fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)
}

pub fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

pub fn java_bin_in_home(home: &Path) -> PathBuf {
    home.join("bin").join("java")
}

pub fn os_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}
