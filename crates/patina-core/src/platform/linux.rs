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

/// Live (not zombie) processes in process group `pgid`.
pub fn process_group_members(pgid: u32) -> Vec<u32> {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| {
                    let rest = &s[s.rfind(')')? + 1..];
                    let mut f = rest.split_whitespace();
                    let state = f.next()?;
                    let group: u32 = f.nth(1)?.parse().ok()?;
                    Some(group == pgid && state != "Z" && state != "X")
                })
                .unwrap_or(false)
        })
        .collect()
}

/// The group id as a signal target, or an error for ids that must never be
/// signalled: 0 and 1 (`kill(0)` is the caller's own group, `kill(-1)` every
/// process the user may signal), this launcher's own process group, and a
/// group named after this process's pid.
fn signalable_group(pgid: u32) -> io::Result<rustix::process::Pid> {
    let own_group = rustix::process::getpgrp().as_raw_nonzero().get();
    i32::try_from(pgid)
        .ok()
        .filter(|&p| p > 1 && p != own_group)
        .filter(|_| pgid != std::process::id())
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| io::Error::other(format!("refusing to signal process group {pgid}")))
}

/// Sends `sig` to process group `pgid`; a group that no longer exists is Ok.
/// Groups refused by [`signalable_group`] are never signalled.
fn signal_group(pgid: u32, sig: rustix::process::Signal) -> io::Result<()> {
    let p = signalable_group(pgid)?;
    match rustix::process::kill_process_group(p, sig) {
        Err(rustix::io::Errno::SRCH) => Ok(()),
        r => r.map_err(io::Error::from),
    }
}

/// SIGTERM to process group `pgid`, up to 5 s for every member to exit, then
/// SIGKILL. Works when the group's leader already exited: the kernel keeps a
/// group id reserved (never handing it out as a new pid) while any member
/// lives, so a group with members is still the one the leader created. The
/// caller must know that group is the game's, i.e. its leader passed the
/// identity check when the game was started or reattached. Ok when the group
/// is empty.
pub fn kill_process_group(pgid: u32) -> io::Result<()> {
    use std::time::{Duration, Instant};
    let wait_empty = |limit: Duration| {
        let end = Instant::now() + limit;
        loop {
            if process_group_members(pgid).is_empty() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    if process_group_members(pgid).is_empty() {
        return Ok(());
    }
    signal_group(pgid, rustix::process::Signal::TERM)?;
    if wait_empty(Duration::from_secs(5)) {
        return Ok(());
    }
    signal_group(pgid, rustix::process::Signal::KILL)?;
    if !wait_empty(Duration::from_secs(2)) {
        return Err(io::Error::other(format!(
            "process group {pgid} did not exit"
        )));
    }
    Ok(())
}

/// Whether `pid` is an exited but unreaped process (a zombie) whose start
/// token is `start`. While it is unreaped its pid, and so its process group
/// id, cannot be handed out again.
pub fn is_zombie_with_start(pid: u32, start: u64) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            let state = s[s.rfind(')')? + 1..]
                .split_whitespace()
                .next()?
                .to_string();
            let (_, st) = parse_stat(&s)?;
            Some(state == "Z" && st == start)
        })
        .unwrap_or(false)
}

/// Whether two metadata describe the same file (same device and inode).
pub fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Process group id of a live process.
pub fn process_group_of(pid: u32) -> Option<u32> {
    read_stat(pid).map(|(g, _)| g)
}

/// Why [`kill_pid_checked`] failed, and whether the process was stopped
/// anyway.
#[derive(Debug)]
pub struct KillFailure {
    pub error: io::Error,
    /// SIGKILL had already been delivered when a later step failed, so the
    /// process is dying because of this kill.
    pub sigkill_delivered: bool,
}

/// [`kill_pid`] without the detail of [`KillFailure`].
pub fn kill_pid(pid: u32) -> io::Result<()> {
    kill_pid_checked(pid).map_err(|f| f.error)
}

/// SIGTERM, wait up to 5 s, then SIGKILL. When `pid` leads its own process
/// group (as `spawn_detached` makes it) the whole group is signalled, so
/// wrappers and game children die too, including members that outlive the
/// leader. Ok if the process is already gone; a caller that knows the game's
/// leader exited but its group may remain uses [`kill_process_group`].
pub fn kill_pid_checked(pid: u32) -> Result<(), KillFailure> {
    use std::time::{Duration, Instant};
    let before_kill = |error| KillFailure {
        error,
        sigkill_delivered: false,
    };
    let after_kill = |error| KillFailure {
        error,
        sigkill_delivered: true,
    };
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
            return signal_group(pid, sig);
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
    send(pid, leader, true).map_err(before_kill)?;
    if wait_gone(pid, Duration::from_secs(5)) {
        if leader {
            // Members that ignore SIGTERM outlive the leader.
            kill_process_group(pid).map_err(before_kill)?;
        }
        return Ok(());
    }
    // Pid-reuse guard: only escalate while it is still the same process.
    if proc_info(pid).is_none_or(|i| i.start_time != info.start_time) {
        return Ok(());
    }
    send(pid, leader, false).map_err(before_kill)?;
    if !wait_gone(pid, Duration::from_secs(2)) {
        return Err(after_kill(io::Error::other(format!(
            "process {pid} did not exit"
        ))));
    }
    if leader {
        // Members that outlived the leader (the group id stays theirs).
        kill_process_group(pid).map_err(after_kill)?;
    }
    Ok(())
}

/// Signals that ask a process to stop: SIGHUP, SIGINT, SIGTERM.
const STOP_SIGNALS: [i32; 3] = [1, 2, 15];

/// Whether a game that ended with `status` was asked to stop rather than
/// failing: it died of SIGHUP/SIGINT/SIGTERM, or exited with `128 + signal`
/// for one of them (the JVM does that after running its shutdown hooks).
/// True whoever sent the signal, this launcher or another process.
pub fn is_stop_request(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    match (status.signal(), status.code()) {
        (Some(sig), _) => STOP_SIGNALS.contains(&sig),
        (None, Some(code)) => code > 128 && STOP_SIGNALS.contains(&(code - 128)),
        (None, None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_requests_are_recognised() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        let exited = |code: i32| ExitStatus::from_raw(code << 8);
        let signalled = ExitStatus::from_raw;
        for st in [
            exited(143),
            exited(130),
            exited(129),
            signalled(15),
            signalled(2),
        ] {
            assert!(is_stop_request(&st), "{st:?}");
        }
        for st in [exited(0), exited(1), exited(255), exited(137), signalled(6)] {
            assert!(!is_stop_request(&st), "{st:?}");
        }
        assert!(
            !is_stop_request(&signalled(9)),
            "SIGKILL may be the OOM killer"
        );
    }

    #[test]
    fn stat_parser_handles_odd_comm() {
        let line = "42 (a) b (c) S 1 777 777 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 123456 0 0";
        assert_eq!(parse_stat(line), Some((777, 123456)));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S 1"), None);
    }

    /// Checks the guard alone: no signal is ever sent by this test.
    #[test]
    fn dangerous_group_ids_are_refused() {
        let own_group = rustix::process::getpgrp().as_raw_nonzero().get() as u32;
        for g in [0, 1, own_group, std::process::id()] {
            assert!(signalable_group(g).is_err(), "{g}");
        }
        assert!(signalable_group(u32::MAX).is_err());
        // An ordinary id passes the guard (nothing is signalled here).
        let other = (2..)
            .find(|g| *g != own_group && *g != std::process::id())
            .unwrap();
        assert!(signalable_group(other).is_ok());
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
