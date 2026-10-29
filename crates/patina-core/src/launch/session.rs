//! A running game: start it detached, tail its log file into a channel,
//! detect exit and crashes, and reattach after the launcher restarts.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use crate::config::GlobalConfig;
use crate::instance::Instance;
use crate::launch::log::{Log4jXmlParser, LogLine, Stream, truncate_with_ellipsis};
use crate::launch::running::{
    CURRENT_LOG, RUNNING_FILE, RunningRecord, clear_record, read_record, verify, write_record,
};
use crate::launch::{LaunchAccount, LaunchCommand};
use crate::locks::{Busy, InstanceGuard, Locks, VersionShared};
use crate::paths::DataDirs;
use crate::platform;
use crate::prepare::{Launcher, PrepareError, PrepareWarning};
use crate::progress::{Phase, ProgressSink, Unit, report};

/// Longest log line delivered, in bytes; longer lines are cut and end in `…`.
pub const LOG_LINE_MAX: usize = 16 * 1024;
/// Log channel capacity. When it is full the tailer waits: the log file is the
/// buffer, so nothing is dropped.
pub const LOG_CHANNEL_CAP: usize = 4096;

/// Raw bytes kept per line before decoding (log4j events can be long).
const RAW_LINE_MAX: usize = 1024 * 1024;
const TAIL_POLL: Duration = Duration::from_millis(150);
const OWN_EXIT_POLL: Duration = Duration::from_millis(100);
const READ_WARN_EVERY: Duration = Duration::from_secs(60);
const REATTACH_POLL: Duration = Duration::from_secs(1);
const CRASH_SCAN_BYTES: u64 = 64 * 1024;
const CRASH_MARKERS: [&str; 2] = ["---- Minecraft Crash Report ----", "#@!@# Game crashed!"];

const RUNNING: u8 = 0;
const EXITED: u8 = 1;
const ABANDONED: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExit {
    /// `None` for a reattached game (its exit code is not observable) or a
    /// process killed by a signal.
    pub code: Option<i32>,
    pub crashed: bool,
    /// Where the log was saved when the game crashed.
    pub saved_log: Option<PathBuf>,
}

/// Non-fatal findings while starting a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionWarning {
    Prepare(PrepareWarning),
    /// The running record could not be written, so a restarted launcher will
    /// not find this game again.
    RecordNotSaved {
        path: PathBuf,
        error: String,
    },
}

impl std::fmt::Display for SessionWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionWarning::Prepare(w) => write!(f, "{w}"),
            SessionWarning::RecordNotSaved { path, error } => write!(
                f,
                "{}",
                rust_i18n::t!(
                    "errors.session.record_not_saved",
                    path = path.display(),
                    error = error
                )
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Busy(#[from] Busy),
    #[error(transparent)]
    Prepare(#[from] PrepareError),
    #[error("{}", rust_i18n::t!("errors.session.already_running", id = .0))]
    AlreadyRunning(String),
    #[error("{}", rust_i18n::t!("errors.session.io", path = .path.display(), error = .error))]
    Io { path: PathBuf, error: String },
}

/// Terminates the game; cloneable so it can be used while `wait` is pending.
#[derive(Clone)]
pub struct KillHandle {
    pid: u32,
    /// Process identity token; `None` when the game was already gone at start.
    token: Option<u64>,
    killed: Arc<AtomicBool>,
}

impl KillHandle {
    /// SIGTERM to the game's process group, SIGKILL after a grace period.
    /// A pid that no longer carries the game's identity is left alone.
    pub async fn kill(&self) -> io::Result<()> {
        let h = self.clone();
        tokio::task::spawn_blocking(move || h.kill_blocking())
            .await
            .map_err(io::Error::other)?
    }

    /// Blocking form of [`KillHandle::kill`]; marks the session as user-killed
    /// only when the kill succeeded.
    pub(crate) fn kill_blocking(&self) -> io::Result<()> {
        let alive = matches!(
            (self.token, platform::proc_info(self.pid)),
            (Some(t), Some(info)) if info.start_time == t
        );
        if alive {
            platform::kill_pid(self.pid)?;
            self.killed.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

pub struct GameSession {
    inst: Instance,
    pid: u32,
    token: Option<u64>,
    started_at: u64,
    /// The process handle when this launcher started the game (reaped by `wait`).
    child: Option<Child>,
    rx: Option<mpsc::Receiver<LogLine>>,
    state: Arc<AtomicU8>,
    killed: Arc<AtomicBool>,
    warnings: Vec<SessionWarning>,
    _instance_lock: InstanceGuard,
    _version_lock: VersionShared,
}

impl Drop for GameSession {
    fn drop(&mut self) {
        // Stop the tailer unless `wait` already told it the game exited. The
        // running record stays so a later launcher can reattach.
        let _ = self
            .state
            .compare_exchange(RUNNING, ABANDONED, Ordering::SeqCst, Ordering::SeqCst);
        // Reap the game when it ends so it does not linger as a zombie.
        if let Some(mut child) = self.child.take() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn io_at(path: &Path) -> impl Fn(io::Error) -> SessionError + '_ {
    move |e| SessionError::Io {
        path: path.to_path_buf(),
        error: e.to_string(),
    }
}

/// The lock key matches `InstanceStore::lock`: instances root plus id.
fn lock_instance(inst: &Instance) -> Result<InstanceGuard, Busy> {
    let root = inst.dir.parent().unwrap_or(&inst.dir);
    Locks::global().instance(root, &inst.id)
}

/// The version a game of `inst` uses: its loader profile, else the MC version.
fn version_id(inst: &Instance) -> &str {
    inst.config
        .loader()
        .and_then(|(_, c)| c.profile.as_deref())
        .unwrap_or_else(|| inst.config.mc_version())
}

impl GameSession {
    /// Prepares and starts the game for `inst`. Holds the instance lock and a
    /// shared lock on its version for as long as the session lives; both are
    /// released again if this fails. Must be called within a Tokio runtime.
    pub async fn start(
        launcher: &Launcher,
        inst: &Instance,
        global: &GlobalConfig,
        account: &LaunchAccount,
        progress: ProgressSink,
    ) -> Result<GameSession, SessionError> {
        let instance_lock = lock_instance(inst)?;
        let version_lock = launcher.data.lock_version_shared(version_id(inst))?;
        refuse_if_running(inst)?;
        let prepared = launcher
            .prepare(inst, global, account, progress.clone())
            .await?;
        let mut session = Self::spawn(inst, prepared.command, instance_lock, version_lock)?;
        session.warnings.splice(
            0..0,
            prepared.warnings.into_iter().map(SessionWarning::Prepare),
        );
        report(&progress, Phase::Launching, 1, 1, Unit::Items);
        Ok(session)
    }

    /// Test entry: like `start` but with a ready-made command (no prepare).
    /// Must be called within a Tokio runtime (it spawns the tailer).
    #[cfg(test)]
    pub(crate) fn start_with_command(
        inst: &Instance,
        data: &DataDirs,
        cmd: &LaunchCommand,
    ) -> Result<GameSession, SessionError> {
        let instance_lock = lock_instance(inst)?;
        let version_lock = data.lock_version_shared(version_id(inst))?;
        refuse_if_running(inst)?;
        Self::spawn(inst, cmd.clone(), instance_lock, version_lock)
    }

    fn spawn(
        inst: &Instance,
        cmd: LaunchCommand,
        instance_lock: InstanceGuard,
        version_lock: VersionShared,
    ) -> Result<GameSession, SessionError> {
        let log = inst.dir.join(CURRENT_LOG);
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir).map_err(io_at(dir))?;
        }
        std::fs::write(&log, b"").map_err(io_at(&log))?;
        let started_at = now_secs();
        let child = platform::spawn_detached(&cmd, &log).map_err(io_at(&log))?;
        let pid = child.id();
        let token = platform::proc_info(pid).map(|i| i.start_time);
        let mut record_warning = None;
        match token {
            Some(start_time) => {
                let rec = RunningRecord {
                    pid,
                    start_time,
                    instance_id: inst.id.clone(),
                    started_at,
                    log: CURRENT_LOG.to_string(),
                };
                if let Err(e) = write_record(inst, &rec) {
                    tracing::warn!("cannot write the running record: {e}");
                    record_warning = Some(SessionWarning::RecordNotSaved {
                        path: inst.dir.join(RUNNING_FILE),
                        error: e.to_string(),
                    });
                }
            }
            // Exited before we could look: nothing to reattach to.
            None => tracing::debug!("game {pid} already exited at start"),
        }
        let mut s = Self::assemble(
            inst,
            pid,
            token,
            started_at,
            Some(child),
            instance_lock,
            version_lock,
        );
        s.warnings.extend(record_warning);
        s.start_tailer();
        Ok(s)
    }

    fn assemble(
        inst: &Instance,
        pid: u32,
        token: Option<u64>,
        started_at: u64,
        child: Option<Child>,
        instance_lock: InstanceGuard,
        version_lock: VersionShared,
    ) -> GameSession {
        GameSession {
            inst: inst.clone(),
            pid,
            token,
            started_at,
            child,
            rx: None,
            state: Arc::new(AtomicU8::new(RUNNING)),
            killed: Arc::new(AtomicBool::new(false)),
            warnings: Vec::new(),
            _instance_lock: instance_lock,
            _version_lock: version_lock,
        }
    }

    fn start_tailer(&mut self) {
        let (tx, rx) = mpsc::channel(LOG_CHANNEL_CAP);
        self.rx = Some(rx);
        let path = self.inst.dir.join(CURRENT_LOG);
        let file = match std::fs::File::open(&path) {
            Ok(f) => Some(tokio::fs::File::from_std(f)),
            Err(e) => {
                tracing::warn!("cannot open the game log {}: {e}", path.display());
                None
            }
        };
        tokio::spawn(tail(file, tx, self.state.clone()));
    }

    /// Picks up a game this launcher started earlier and that still runs.
    /// Tails the log from its beginning. `None` when there is no verified
    /// running game, or this process already holds the instance.
    ///
    /// Must be called within a Tokio runtime. The data dir (for the version
    /// lock) is derived from the `<data>/instances/<id>` layout of `inst.dir`.
    pub fn reattach(inst: &Instance) -> Option<GameSession> {
        let rec = read_record(inst)?;
        if rec.instance_id != inst.id {
            // A copied instance directory brings a foreign record along.
            tracing::warn!("clearing a running record that names another instance");
            clear_record(inst);
            return None;
        }
        let instance_lock = match lock_instance(inst) {
            Ok(l) => l,
            Err(_) => return None, // already attached or busy here: keep the record
        };
        if !verify(&rec) {
            clear_record(inst);
            return None;
        }
        let data = DataDirs::new(inst.dir.parent()?.parent()?.to_path_buf());
        let version_lock = match data.lock_version_shared(version_id(inst)) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("cannot reattach to {}: {e}", inst.id);
                return None;
            }
        };
        let mut s = Self::assemble(
            inst,
            rec.pid,
            Some(rec.start_time),
            rec.started_at,
            None,
            instance_lock,
            version_lock,
        );
        s.start_tailer();
        Some(s)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Non-fatal findings from preparing the game (empty when reattached).
    pub fn warnings(&self) -> &[SessionWarning] {
        &self.warnings
    }

    /// The game's log lines (stdout and stderr share one file, so all lines
    /// are `Stream::Stdout`). Can be taken once; later calls return a closed
    /// channel. The channel closes after the game exited and the log is drained.
    pub fn logs(&mut self) -> mpsc::Receiver<LogLine> {
        self.rx.take().unwrap_or_else(|| mpsc::channel(1).1)
    }

    pub fn killer(&self) -> KillHandle {
        KillHandle {
            pid: self.pid,
            token: self.token,
            killed: self.killed.clone(),
        }
    }

    pub async fn kill(&self) -> io::Result<()> {
        self.killer().kill().await
    }

    /// Waits for the game to end, decides whether it crashed (saving the log
    /// if so), clears the running record and releases the locks.
    pub async fn wait(mut self) -> SessionExit {
        let mut code = None;
        // Whether the process ended abnormally; unknown (false) for a
        // reattached game and when waiting failed.
        let mut failed = false;
        match self.child.take() {
            Some(mut child) => loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        code = status.code();
                        // A stop asked for by another process (or the user's
                        // system monitor) is not a crash either.
                        failed = !status.success() && !platform::is_stop_request(&status);
                        break;
                    }
                    Ok(None) => tokio::time::sleep(OWN_EXIT_POLL).await,
                    Err(e) => {
                        tracing::warn!("cannot wait for the game: {e}");
                        break;
                    }
                }
            },
            None => loop {
                let alive = match (self.token, platform::proc_info(self.pid)) {
                    (Some(t), Some(info)) => info.start_time == t,
                    _ => false,
                };
                if !alive {
                    break;
                }
                tokio::time::sleep(REATTACH_POLL).await;
            },
        }
        self.state.store(EXITED, Ordering::SeqCst);
        let killed = self.killed.load(Ordering::SeqCst);
        // A game the user stopped exits non-zero by design.
        // Death by signal (SIGABRT, SIGSEGV, OOM kill) has no exit code but is a crash.
        let bad_code = failed && !killed;
        let crashed = bad_code || self.crash_evidence();
        let saved_log = crashed.then(|| self.save_log()).flatten();
        clear_record(&self.inst);
        SessionExit {
            code,
            crashed,
            saved_log,
        }
    }

    fn crash_evidence(&self) -> bool {
        let reports = self.inst.game_dir().join("crash-reports");
        if let Ok(rd) = std::fs::read_dir(&reports) {
            for e in rd.flatten() {
                let secs = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                if secs.is_some_and(|s| s >= self.started_at) {
                    return true;
                }
            }
        }
        let tail = read_tail(&self.inst.dir.join(CURRENT_LOG), CRASH_SCAN_BYTES);
        CRASH_MARKERS.iter().any(|m| tail.contains(m))
    }

    /// Renames the current log to `patina-logs/<UTC time>-crash.log`.
    fn save_log(&self) -> Option<PathBuf> {
        let src = self.inst.dir.join(CURRENT_LOG);
        let dir = src.parent()?;
        let stamp = format_utc_compact(now_secs());
        let mut dest = dir.join(format!("{stamp}-crash.log"));
        let mut n = 0;
        while dest.exists() {
            n += 1;
            dest = dir.join(format!("{stamp}-crash-{n}.log"));
        }
        match std::fs::rename(&src, &dest) {
            Ok(()) => Some(dest),
            Err(e) => {
                tracing::warn!("cannot save the crash log: {e}");
                None
            }
        }
    }
}

fn refuse_if_running(inst: &Instance) -> Result<(), SessionError> {
    match read_record(inst) {
        Some(rec) if rec.instance_id == inst.id && verify(&rec) => {
            Err(SessionError::AlreadyRunning(inst.id.clone()))
        }
        Some(_) => {
            clear_record(inst);
            Ok(())
        }
        None => Ok(()),
    }
}

fn read_tail(path: &Path, max: u64) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(max))).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(max).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Reads the log file incrementally and sends parsed lines. Stops when the
/// receiver is gone, the session was abandoned, or the game exited and the
/// file is drained.
async fn tail(file: Option<tokio::fs::File>, tx: mpsc::Sender<LogLine>, state: Arc<AtomicU8>) {
    let Some(mut file) = file else { return };
    let mut parser = Log4jXmlParser::new();
    let mut line: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut buf = vec![0u8; 64 * 1024];
    let mut last_warn: Option<std::time::Instant> = None;
    loop {
        // Sampled before the read: if the game had exited by then, EOF is final.
        let st = state.load(Ordering::SeqCst);
        if st == ABANDONED || tx.is_closed() {
            return;
        }
        let n = match file.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                if last_warn.is_none_or(|t| t.elapsed() >= READ_WARN_EVERY) {
                    tracing::warn!("cannot read the game log: {e}");
                    last_warn = Some(std::time::Instant::now());
                }
                0
            }
        };
        if n == 0 {
            if st == EXITED {
                break;
            }
            tokio::time::sleep(TAIL_POLL).await;
            continue;
        }
        for chunk in buf[..n].split_inclusive(|&b| b == b'\n') {
            let (body, complete) = match chunk.split_last() {
                Some((b'\n', rest)) => (rest, true),
                _ => (chunk, false),
            };
            let room = RAW_LINE_MAX.saturating_sub(line.len());
            if body.len() > room {
                truncated = true;
            }
            line.extend_from_slice(&body[..body.len().min(room)]);
            if complete {
                if !emit_line(&mut parser, &line, truncated, &tx).await {
                    return;
                }
                line.clear();
                truncated = false;
            }
        }
    }
    // The game ended: flush an unterminated last line and a partial event.
    if !line.is_empty() && !emit_line(&mut parser, &line, truncated, &tx).await {
        return;
    }
    for l in parser.finish(Stream::Stdout) {
        if tx.send(cap(l, false)).await.is_err() {
            return;
        }
    }
}

fn cap(mut l: LogLine, truncated: bool) -> LogLine {
    if truncated && l.text.len() <= LOG_LINE_MAX {
        l.text.push('…');
    }
    truncate_with_ellipsis(&mut l.text, LOG_LINE_MAX);
    l
}

/// Returns false when the receiver is gone.
async fn emit_line(
    parser: &mut Log4jXmlParser,
    raw: &[u8],
    truncated: bool,
    tx: &mpsc::Sender<LogLine>,
) -> bool {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let text = String::from_utf8_lossy(raw);
    for l in parser.push_line(&text, Stream::Stdout) {
        if tx.send(cap(l, truncated)).await.is_err() {
            return false;
        }
    }
    true
}

/// `YYYYMMDD-HHMMSS` in UTC for a Unix timestamp (civil-from-days algorithm).
fn format_utc_compact(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::instance::InstanceStore;
    use crate::launch::running::read_record;
    use crate::locks::BusyKind;
    use std::time::{Duration, Instant};

    /// Kills the game on drop, only while it still has its original identity.
    struct Killer(KillHandle);
    impl Drop for Killer {
        fn drop(&mut self) {
            let _ = self.0.kill_blocking();
        }
    }

    struct Env {
        _tmp: tempfile::TempDir,
        data: DataDirs,
        inst: Instance,
    }

    fn env(id: &str) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path().to_path_buf());
        let dir = data.instances().join(id);
        std::fs::create_dir_all(dir.join("minecraft")).unwrap();
        let inst = Instance {
            id: id.into(),
            dir,
            config: Default::default(),
        };
        Env {
            _tmp: tmp,
            data,
            inst,
        }
    }

    fn sh(inst_id: &str, script: &str) -> LaunchCommand {
        LaunchCommand {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                script.into(),
                "x".into(),
                format!("-Dpatina.instance={inst_id}"),
            ],
            cwd: std::env::temp_dir(),
        }
    }

    async fn drain(mut rx: mpsc::Receiver<LogLine>) -> Vec<LogLine> {
        let mut v = vec![];
        let end = Instant::now() + Duration::from_secs(20);
        while let Ok(Some(l)) = tokio::time::timeout_at(end.into(), rx.recv()).await {
            v.push(l);
        }
        v
    }

    async fn poll_until(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while !f() {
            assert!(Instant::now() < end, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn utc_name_formatting() {
        assert_eq!(format_utc_compact(0), "19700101-000000");
        assert_eq!(format_utc_compact(1793264200), "20261029-085640");
        assert_eq!(format_utc_compact(951782400), "20000229-000000");
        assert_eq!(format_utc_compact(4107542399), "21000228-235959");
    }

    #[tokio::test]
    async fn session_streams_log_and_exits_cleanly() {
        let e = env("i1");
        let mut s = GameSession::start_with_command(
            &e.inst,
            &e.data,
            &sh("i1", "echo one; echo two; echo three; exit 0"),
        )
        .unwrap();
        let _k = Killer(s.killer());
        let rx = s.logs();
        let exit = s.wait().await;
        assert_eq!(
            exit,
            SessionExit {
                code: Some(0),
                crashed: false,
                saved_log: None
            }
        );
        let lines: Vec<_> = drain(rx).await.into_iter().map(|l| l.text).collect();
        assert_eq!(lines, ["one", "two", "three"]);
        assert!(read_record(&e.inst).is_none());
    }

    #[tokio::test]
    async fn crash_saves_log() {
        let e = env("i1");
        let mut s =
            GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "echo boom; exit 1"))
                .unwrap();
        let rx = s.logs();
        let exit = s.wait().await;
        assert!(exit.crashed);
        assert_eq!(exit.code, Some(1));
        let saved = exit.saved_log.unwrap();
        assert!(std::fs::read_to_string(&saved).unwrap().contains("boom"));
        assert!(
            saved
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-crash.log")
        );
        assert!(!e.inst.dir.join(CURRENT_LOG).exists());
        // The tailer still delivers the output after the rename.
        let lines = drain(rx).await;
        assert!(lines.iter().any(|l| l.text == "boom"));
        assert!(read_record(&e.inst).is_none());
    }

    #[tokio::test]
    async fn existing_crash_log_is_not_overwritten() {
        let e = env("i1");
        let mut names = std::collections::HashSet::new();
        for _ in 0..3 {
            let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "exit 1")).unwrap();
            // Same second most of the time; names must be unique regardless.
            let exit = s.wait().await;
            assert!(names.insert(exit.saved_log.unwrap()));
        }
    }

    #[tokio::test]
    async fn crash_report_file_marks_crash_even_with_exit_zero() {
        let e = env("i1");
        let reports = e.inst.game_dir().join("crash-reports");
        let script = format!(
            "mkdir -p {0}; echo x > {0}/crash-1.txt; exit 0",
            reports.display()
        );
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", &script)).unwrap();
        let exit = s.wait().await;
        assert_eq!(exit.code, Some(0));
        assert!(exit.crashed);
        assert!(exit.saved_log.unwrap().exists());
    }

    #[tokio::test]
    async fn crash_marker_in_log_tail_marks_crash() {
        let e = env("i1");
        let s = GameSession::start_with_command(
            &e.inst,
            &e.data,
            &sh(
                "i1",
                "echo '#@!@# Game crashed! Crash report saved'; exit 0",
            ),
        )
        .unwrap();
        assert!(s.wait().await.crashed);
    }

    #[tokio::test]
    async fn old_crash_reports_do_not_count() {
        let e = env("i1");
        let reports = e.inst.game_dir().join("crash-reports");
        std::fs::create_dir_all(&reports).unwrap();
        let f = std::fs::File::create(reports.join("old.txt")).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "exit 0")).unwrap();
        assert!(!s.wait().await.crashed);
    }

    #[tokio::test]
    async fn long_and_binary_lines_are_capped() {
        let e = env("i1");
        let script = "head -c 102400 /dev/zero | tr '\\0' a; echo; \
                      printf 'a\\377\\376b\\n'; head -c 3000 /dev/urandom; echo; echo end";
        let mut s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", script)).unwrap();
        let rx = s.logs();
        let exit = s.wait().await;
        assert_eq!(exit.code, Some(0));
        let lines = drain(rx).await;
        assert!(lines.len() >= 3);
        assert!(lines.iter().all(|l| l.text.len() <= LOG_LINE_MAX + 3));
        assert!(lines[0].text.ends_with('…'));
        assert_eq!(lines[1].text, "a\u{FFFD}\u{FFFD}b");
        assert_eq!(lines.last().unwrap().text, "end");
    }

    #[tokio::test]
    async fn partial_last_line_is_flushed_at_exit() {
        let e = env("i1");
        let mut s =
            GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "printf 'a\\nb'")).unwrap();
        let rx = s.logs();
        s.wait().await;
        let lines: Vec<_> = drain(rx).await.into_iter().map(|l| l.text).collect();
        assert_eq!(lines, ["a", "b"]);
    }

    #[tokio::test]
    async fn instance_lock_is_held_while_running() {
        let e = env("i1");
        let store = InstanceStore::new(e.data.instances());
        let go = e.inst.dir.join("go");
        let script = format!("while [ ! -f {} ]; do sleep 0.05; done", go.display());
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", &script)).unwrap();
        let _k = Killer(s.killer());
        assert!(matches!(
            store.lock("i1"),
            Err(Busy {
                what: BusyKind::Instance(_)
            })
        ));
        assert!(
            matches!(
                GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "exit 0")),
                Err(SessionError::Busy(_))
            ),
            "second start must be busy"
        );
        std::fs::write(&go, "").unwrap();
        s.wait().await;
        assert!(store.lock("i1").is_ok());
    }

    #[tokio::test]
    async fn reattached_session_detects_exit_and_crash() {
        let e = env("i1");
        let go = e.inst.dir.join("go");
        let script = format!(
            "echo hello; while [ ! -f {} ]; do sleep 0.1; done; echo '#@!@# Game crashed!'; exit 2",
            go.display()
        );
        let mut s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", &script)).unwrap();
        let pid = s.pid();
        let _k = Killer(s.killer());
        drop(s.logs());
        drop(s);
        assert!(read_record(&e.inst).is_some());

        let mut s = GameSession::reattach(&e.inst).expect("reattach");
        assert_eq!(s.pid(), pid);
        // Already attached in this process now.
        assert!(GameSession::reattach(&e.inst).is_none());
        assert!(read_record(&e.inst).is_some());
        let mut rx = s.logs();
        let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.text, "hello");
        std::fs::write(&go, "").unwrap();
        let exit = s.wait().await;
        assert_eq!(exit.code, None);
        assert!(exit.crashed);
        assert!(exit.saved_log.unwrap().exists());
        assert!(read_record(&e.inst).is_none());
        let rest = drain(rx).await;
        assert!(rest.iter().any(|l| l.text.contains("Game crashed")));
    }

    #[tokio::test]
    async fn reattach_clears_stale_record_and_rejects_other_instance() {
        let e = env("i1");
        let rec = RunningRecord {
            pid: 1,
            start_time: 1,
            instance_id: "i1".into(),
            started_at: 0,
            log: CURRENT_LOG.into(),
        };
        crate::launch::running::write_record(&e.inst, &rec).unwrap();
        assert!(GameSession::reattach(&e.inst).is_none());
        assert!(read_record(&e.inst).is_none());

        let other = RunningRecord {
            instance_id: "zzz".into(),
            ..rec
        };
        crate::launch::running::write_record(&e.inst, &other).unwrap();
        assert!(GameSession::reattach(&e.inst).is_none());
    }

    #[tokio::test]
    async fn kill_ends_the_game() {
        let e = env("i1");
        let mut s =
            GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "exec sleep 60")).unwrap();
        let pid = s.pid();
        let _k = Killer(s.killer());
        let rx = s.logs();
        let killer = s.killer();
        let t = Instant::now();
        s.kill().await.unwrap();
        let exit = s.wait().await;
        assert!(t.elapsed() < Duration::from_secs(8));
        assert!(!exit.crashed, "a user kill is not a crash");
        assert!(crate::platform::proc_info(pid).is_none());
        killer.kill().await.unwrap(); // after exit: harmless
        assert!(drain(rx).await.is_empty());
        poll_until("record cleared", || read_record(&e.inst).is_none()).await;
    }

    #[tokio::test]
    async fn dropping_session_releases_locks_but_keeps_record() {
        let e = env("i1");
        let store = InstanceStore::new(e.data.instances());
        let s =
            GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "exec sleep 60")).unwrap();
        let _k = Killer(s.killer());
        assert!(store.lock("i1").is_err());
        drop(s);
        assert!(store.lock("i1").is_ok());
        assert!(read_record(&e.inst).is_some());
    }

    #[tokio::test]
    async fn death_by_signal_is_a_crash() {
        let e = env("i1");
        let s =
            GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "echo x; kill -ABRT $$"))
                .unwrap();
        let exit = s.wait().await;
        assert_eq!(exit.code, None);
        assert!(exit.crashed);
        assert!(exit.saved_log.unwrap().exists());
    }

    /// Sends SIGTERM to `pid` from outside the session, as another launcher
    /// process or a system monitor would.
    fn external_term(pid: u32) {
        let st = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap();
        assert!(st.success());
    }

    /// Regression: a stop requested from another process (here: SIGTERM, which
    /// the JVM turns into exit code 143) was reported as a crash by the
    /// launcher that owns the game, and its log was saved as a crash log.
    #[tokio::test]
    async fn external_stop_with_exit_143_is_not_a_crash() {
        let e = env("i1");
        let script = "trap 'exit 143' TERM; echo up; while :; do sleep 0.05; done";
        let mut s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", script)).unwrap();
        let _k = Killer(s.killer());
        let mut rx = s.logs();
        let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.text, "up");
        external_term(s.pid());
        let exit = s.wait().await;
        assert_eq!(exit.code, Some(143));
        assert!(!exit.crashed, "a stop request is not a crash");
        assert_eq!(exit.saved_log, None);
        assert!(e.inst.dir.join(CURRENT_LOG).exists());
    }

    #[tokio::test]
    async fn external_stop_by_sigterm_is_not_a_crash() {
        let e = env("i1");
        let script = "echo up; while :; do sleep 0.05; done";
        let mut s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", script)).unwrap();
        let _k = Killer(s.killer());
        let mut rx = s.logs();
        let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.text, "up");
        external_term(s.pid());
        let exit = s.wait().await;
        assert_eq!(exit.code, None);
        assert!(!exit.crashed, "a stop request is not a crash");
        assert_eq!(exit.saved_log, None);
    }

    #[tokio::test]
    async fn running_game_blocks_a_second_start_and_keeps_its_log() {
        let e = env("i1");
        let s = GameSession::start_with_command(
            &e.inst,
            &e.data,
            &sh("i1", "echo keepme; sleep 60 & wait"),
        )
        .unwrap();
        let _k = Killer(s.killer());
        let log = e.inst.dir.join(CURRENT_LOG);
        poll_until("log written", || {
            std::fs::read_to_string(&log).is_ok_and(|t| t.contains("keepme"))
        })
        .await;
        drop(s);
        let r = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "exit 0"));
        assert!(matches!(r, Err(SessionError::AlreadyRunning(_))));
        assert!(std::fs::read_to_string(&log).unwrap().contains("keepme"));
    }

    #[tokio::test]
    async fn foreign_record_is_stale() {
        let e = env("i1");
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "sleep 60 & wait"))
            .unwrap();
        let _k = Killer(s.killer());
        let mut rec = read_record(&e.inst).unwrap();
        drop(s);
        rec.instance_id = "copied-from".into();
        crate::launch::running::write_record(&e.inst, &rec).unwrap();
        assert!(GameSession::reattach(&e.inst).is_none());
        assert!(read_record(&e.inst).is_none());
        crate::launch::running::write_record(&e.inst, &rec).unwrap();
        assert!(refuse_if_running(&e.inst).is_ok());
        assert!(read_record(&e.inst).is_none());
    }

    #[tokio::test]
    async fn failed_start_releases_locks() {
        let e = env("i1");
        let store = InstanceStore::new(e.data.instances());
        let cmd = LaunchCommand {
            program: "/nonexistent/patina-test-program".into(),
            args: vec![],
            cwd: std::env::temp_dir(),
        };
        let r = GameSession::start_with_command(&e.inst, &e.data, &cmd);
        assert!(matches!(r, Err(SessionError::Io { .. })));
        assert!(store.lock("i1").is_ok());
        assert!(e.data.lock_version_exclusive(version_id(&e.inst)).is_ok());
        assert!(read_record(&e.inst).is_none());
    }

    #[tokio::test]
    async fn unwritable_record_becomes_a_warning() {
        let e = env("i1");
        // A directory where the record file should go makes the write fail.
        std::fs::create_dir(e.inst.dir.join(RUNNING_FILE)).unwrap();
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "exit 0")).unwrap();
        assert!(matches!(
            s.warnings(),
            [SessionWarning::RecordNotSaved { .. }]
        ));
        assert!(!s.warnings()[0].to_string().is_empty());
        s.wait().await;
    }

    #[tokio::test]
    async fn dropped_session_does_not_leave_a_zombie() {
        let e = env("i1");
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "sleep 0.3")).unwrap();
        let pid = s.pid();
        drop(s);
        let state = || {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|t| t.rsplit(')').next().map(|r| r.trim().chars().next()))
                .flatten()
        };
        poll_until("process reaped", || state().is_none()).await;
    }
}
