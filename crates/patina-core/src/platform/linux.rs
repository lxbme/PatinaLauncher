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
        raw.extend(path_java_candidates(&path));
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

/// `java` in each `$PATH` entry. Empty and relative entries are skipped: they
/// mean the current directory, which must not pick the Java to run.
fn path_java_candidates(path: &std::ffi::OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join("java"))
        .collect()
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

/// `unpack200` of a Java home (Java 8 runtimes ship it; it was removed in 14).
pub fn unpack200_in_home(home: &Path) -> PathBuf {
    home.join("bin").join("unpack200")
}

/// Whether spawning failed with `ETXTBSY`: an executable written moments ago
/// can still be open for writing in a child another thread just forked.
pub fn is_text_file_busy(e: &io::Error) -> bool {
    e.raw_os_error() == Some(26)
}

pub fn os_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Platform key used by Mojang's java-runtime index, if one exists for this machine.
pub fn mojang_runtime_key() -> Option<&'static str> {
    if cfg!(target_arch = "x86_64") {
        Some("linux")
    } else if cfg!(target_arch = "x86") {
        Some("linux-i386")
    } else {
        None
    }
}

/// Spawns the game detached from the launcher: own process group, null stdin,
/// stdout and stderr appended to one shared `log` file. The `Child` may be
/// dropped without killing the process.
pub fn spawn_detached(
    cmd: &crate::launch::LaunchCommand,
    log: &Path,
) -> io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let err = out.try_clone()?;
    Command::new(&cmd.program)
        .args(&cmd.args)
        .current_dir(&cmd.cwd)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
}

/// Identity of a running process, used to tell the game from a pid reused by
/// an unrelated process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    /// Opaque identity token: the process start time in clock ticks since
    /// boot (`/proc/<pid>/stat` field 22). Only meaningful for equality
    /// comparison; unlike wall-clock time it does not drift with NTP steps.
    pub start_time: u64,
    pub cmdline: Vec<String>,
}

/// `(pgid, start ticks)` from the content of `/proc/<pid>/stat`. Parses the
/// fields after the last `)` because the command name may contain anything.
fn parse_stat(s: &str) -> Option<(u32, u64)> {
    let rest = &s[s.rfind(')')? + 1..];
    let mut f = rest.split_whitespace();
    // f[0] = state (field 3), f[2] = pgrp (field 5), f[19] = starttime (field 22).
    let pgid = f.clone().nth(2)?.parse().ok()?;
    let start = f.nth(19)?.parse().ok()?;
    Some((pgid, start))
}

fn read_stat(pid: u32) -> Option<(u32, u64)> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn refresh_one(pid: u32) -> Option<sysinfo::System> {
    let mut sys = sysinfo::System::new().ok()?;
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
    );
    Some(sys)
}

/// Info about a live process. A zombie (exited, not yet reaped) counts as gone.
pub fn proc_info(pid: u32) -> Option<ProcInfo> {
    let sys = refresh_one(pid)?;
    let p = sys.process(sysinfo::Pid::from_u32(pid))?;
    if p.status() == sysinfo::ProcessStatus::Zombie {
        return None;
    }
    let (_, start_time) = read_stat(pid)?;
    Some(ProcInfo {
        start_time,
        cmdline: p
            .cmd()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
    })
}

/// Process group id of a live process.
pub fn process_group_of(pid: u32) -> Option<u32> {
    read_stat(pid).map(|(g, _)| g)
}

/// SIGTERM, wait up to 5 s, then SIGKILL. When `pid` leads its own process
/// group (as `spawn_detached` makes it) the whole group is signalled, so
/// wrappers and game children die too. Ok if the process is already gone.
pub fn kill_pid(pid: u32) -> io::Result<()> {
    use std::time::{Duration, Instant};
    fn wait_gone(pid: u32, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        loop {
            if proc_info(pid).is_none() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn send(pid: u32, leader: bool, term: bool) -> io::Result<()> {
        if leader {
            let sig = if term {
                rustix::process::Signal::TERM
            } else {
                rustix::process::Signal::KILL
            };
            let p = i32::try_from(pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
                .ok_or_else(|| io::Error::other("invalid pid"))?;
            return match rustix::process::kill_process_group(p, sig) {
                Err(rustix::io::Errno::SRCH) => Ok(()),
                r => r.map_err(io::Error::from),
            };
        }
        let sys = refresh_one(pid).ok_or_else(|| io::Error::other("cannot query processes"))?;
        let Some(p) = sys.process(sysinfo::Pid::from_u32(pid)) else {
            return Ok(());
        };
        let sig = if term {
            sysinfo::Signal::Term
        } else {
            sysinfo::Signal::Kill
        };
        match p.kill_with(sig) {
            Some(true) => Ok(()),
            _ => Err(io::Error::other(format!("could not signal process {pid}"))),
        }
    }

    let Some(info) = proc_info(pid) else {
        return Ok(());
    };
    let leader = process_group_of(pid) == Some(pid);
    send(pid, leader, true)?;
    if wait_gone(pid, Duration::from_secs(5)) {
        return Ok(());
    }
    // Pid-reuse guard: only escalate while it is still the same process.
    if proc_info(pid).is_none_or(|i| i.start_time != info.start_time) {
        return Ok(());
    }
    send(pid, leader, false)?;
    if !wait_gone(pid, Duration::from_secs(2)) {
        return Err(io::Error::other(format!("process {pid} did not exit")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parser_handles_odd_comm() {
        let line = "42 (a) b (c) S 1 777 777 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 123456 0 0";
        assert_eq!(parse_stat(line), Some((777, 123456)));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S 1"), None);
    }

    #[test]
    fn start_token_is_stable_across_reads() {
        let me = std::process::id();
        assert_eq!(read_stat(me), read_stat(me));
        assert!(read_stat(me).is_some());
    }

    #[test]
    fn path_candidates_skip_empty_and_relative_entries() {
        let path = std::ffi::OsString::from("/usr/bin::bin:./x:/opt/jdk/bin:");
        assert_eq!(
            path_java_candidates(&path),
            vec![
                PathBuf::from("/usr/bin/java"),
                PathBuf::from("/opt/jdk/bin/java")
            ]
        );
    }
}
