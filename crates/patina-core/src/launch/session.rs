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
    CURRENT_LOG, RUNNING_FILE, RunningInfo, RunningRecord, clear_record, clear_stop_request,
    read_record, read_stop_request, stop_request_path, verified_record, verify, write_record,
    write_stop_request,
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

/// How a [`KillHandle`] sends signals; tests swap in recorders so they never
/// signal a process they did not spawn.
#[derive(Clone, Copy)]
pub(crate) struct Signals {
    pub(crate) kill_pid: fn(u32) -> Result<(), platform::KillFailure>,
    pub(crate) kill_group: fn(u32) -> io::Result<()>,
}

impl Signals {
    pub(crate) const REAL: Signals = Signals {
        kill_pid: platform::kill_pid_checked,
        kill_group: platform::kill_process_group,
    };
}

/// Terminates the game; cloneable so it can be used while `wait` is pending.
/// Once the session's `wait` has seen the exit, it never signals anything.
#[derive(Clone)]
pub struct KillHandle {
    pid: u32,
    /// Process identity token; `None` when the game was already gone at start.
    token: Option<u64>,
    killed: Arc<AtomicBool>,
    /// The session's state (`RUNNING` / `EXITED` / `ABANDONED`).
    state: Arc<AtomicU8>,
    /// Where the stop request marker goes (see
    /// [`STOP_REQUEST`](super::running::STOP_REQUEST)).
    stop_marker: PathBuf,
    /// Built from the running record by a process that does not own the
    /// game ([`KillHandle::for_running`]).
    external: bool,
    signals: Signals,
}

impl KillHandle {
    /// A handle for the game of `inst` that runs right now, wherever its
    /// session is held (this process, another Patina process, or nowhere):
    /// built from the verified running record, takes no lock. `None` when no
    /// game of `inst` runs.
    ///
    /// Unlike a session's own handle, it never signals the group of a game
    /// whose leader already exited: only the owner, which knows the leader
    /// is not yet reaped, may do that.
    pub fn for_running(inst: &Instance) -> Option<KillHandle> {
        let rec = verified_record(inst)?;
        Some(KillHandle {
            pid: rec.pid,
            token: Some(rec.start_time),
            killed: Arc::new(AtomicBool::new(false)),
            state: Arc::new(AtomicU8::new(RUNNING)),
            stop_marker: stop_request_path(inst),
            external: true,
            signals: Signals::REAL,
        })
    }

    /// SIGTERM to the game's process group, SIGKILL after a grace period.
    /// A pid that no longer carries the game's identity is left alone.
    pub async fn kill(&self) -> io::Result<()> {
        let h = self.clone();
        tokio::task::spawn_blocking(move || h.kill_blocking())
            .await
            .map_err(io::Error::other)?
    }

    /// Blocking form of [`KillHandle::kill`]. The session is marked as
    /// user-killed before signalling, so `wait` seeing the exit can never
    /// miss it, and unmarked again if the kill failed before SIGKILL was
    /// delivered. The same goes for the stop request marker (see
    /// [`STOP_REQUEST`](super::running::STOP_REQUEST)), which tells a
    /// launcher in another process that holds the session that this exit
    /// was a stop, not a crash.
    ///
    /// When the game's leader already exited but is not yet reaped (a zombie
    /// with the game's start token), members it left in its process group
    /// are still stopped: an unreaped leader keeps its pid, and so the group
    /// id, reserved. A reaped leader's group is never signalled, since its
    /// id may name an unrelated group by now.
    pub(crate) fn kill_blocking(&self) -> io::Result<()> {
        if self.state.load(Ordering::SeqCst) == EXITED {
            tracing::debug!("game {} already exited; nothing to kill", self.pid);
            return Ok(());
        }
        let Some(token) = self.token else {
            return Ok(());
        };
        match platform::proc_info(self.pid) {
            Some(info) if info.start_time == token => {
                self.killed.store(true, Ordering::SeqCst);
                if let Err(e) = write_stop_request(&self.stop_marker, self.pid) {
                    // Still stop the game; only another process's view of
                    // the exit suffers.
                    tracing::warn!(
                        "cannot write stop request {}: {e}",
                        self.stop_marker.display()
                    );
                }
                if let Err(f) = (self.signals.kill_pid)(self.pid) {
                    if !f.sigkill_delivered {
                        self.killed.store(false, Ordering::SeqCst);
                        clear_stop_request(&self.stop_marker);
                    }
                    return Err(f.error);
                }
                Ok(())
            }
            // The pid now names another process.
            Some(_) => Ok(()),
            // The leader already exited on its own, so its exit status is
            // final: no stop request, which could hide a crash. Only the
            // owner knows the leader is still unreaped (its group id still
            // reserved); a zombie seen from outside may be reaped any moment.
            None if !self.external && platform::is_zombie_with_start(self.pid, token) => {
                (self.signals.kill_group)(self.pid)
            }
            None => {
                tracing::debug!("game {} is gone; not signalling its group", self.pid);
                Ok(())
            }
        }
    }
}

/// What [`GameSession::reattach`] found.
// A short-lived return value that is matched at once; boxing the session
// would only add an indirection to every caller.
#[allow(clippy::large_enum_variant)]
pub enum Reattach {
    Attached(GameSession),
    /// The game runs, but its session is held by someone else.
    HeldElsewhere(RunningInfo),
    NotRunning,
}

impl std::fmt::Debug for Reattach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reattach::Attached(s) => f.debug_tuple("Attached").field(&s.pid).finish(),
            Reattach::HeldElsewhere(info) => f.debug_tuple("HeldElsewhere").field(info).finish(),
            Reattach::NotRunning => f.write_str("NotRunning"),
        }
    }
}

impl Reattach {
    /// The attached session, if any (a test and CLI convenience).
    pub fn attached(self) -> Option<GameSession> {
        match self {
            Reattach::Attached(s) => Some(s),
            _ => None,
        }
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

/// A game that is prepared but not yet started. Holds the instance lock and
/// a shared lock on the version; dropping it cancels the launch, releases
/// both and spawns nothing.
pub struct PendingLaunch {
    inst: Instance,
    cmd: LaunchCommand,
    warnings: Vec<PrepareWarning>,
    instance_lock: InstanceGuard,
    version_lock: VersionShared,
}

impl std::fmt::Debug for PendingLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingLaunch")
            .field("instance", &self.inst.id)
            .field("cmd", &self.cmd)
            .field("warnings", &self.warnings)
            .finish_non_exhaustive()
    }
}

impl PendingLaunch {
    /// Findings from preparing the game, available before it starts.
    pub fn warnings(&self) -> &[PrepareWarning] {
        &self.warnings
    }

    /// Starts the game. Must be called within a Tokio runtime (it spawns the
    /// tailer).
    pub async fn launch(self, progress: ProgressSink) -> Result<GameSession, SessionError> {
        let session = self.spawn_now()?;
        report(&progress, Phase::Launching, 1, 1, Unit::Items);
        Ok(session)
    }

    fn spawn_now(self) -> Result<GameSession, SessionError> {
        // Someone else may have started the game while the caller waited.
        refuse_if_running(&self.inst)?;
        // Left over from an earlier game; it must not excuse this one's exit.
        clear_stop_request(&stop_request_path(&self.inst));
        let PendingLaunch {
            inst,
            cmd,
            warnings,
            instance_lock,
            version_lock,
        } = self;
        let mut session = GameSession::spawn(&inst, cmd, instance_lock, version_lock)?;
        session
            .warnings
            .splice(0..0, warnings.into_iter().map(SessionWarning::Prepare));
        Ok(session)
    }
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
        Self::prepare_launch(launcher, inst, global, account, progress.clone())
            .await?
            .launch(progress)
            .await
    }

    /// Takes the locks, refuses a running instance, then prepares the game
    /// without starting it. Drop the result to cancel.
    pub async fn prepare_launch(
        launcher: &Launcher,
        inst: &Instance,
        global: &GlobalConfig,
        account: &LaunchAccount,
        progress: ProgressSink,
    ) -> Result<PendingLaunch, SessionError> {
        let instance_lock = lock_instance(inst)?;
        let version_lock = launcher.data.lock_version_shared(version_id(inst))?;
        refuse_if_running(inst)?;
        let prepared = launcher.prepare(inst, global, account, progress).await?;
        Ok(PendingLaunch {
            inst: inst.clone(),
            cmd: prepared.command,
            warnings: prepared.warnings,
            instance_lock,
            version_lock,
        })
    }

    /// Like `prepare_launch` but with a ready-made command (no prepare). The
    /// data dir is derived from the `<data>/instances/<id>` layout.
    #[cfg(any(test, feature = "test-support"))]
    pub fn prepare_launch_with_command(
        inst: &Instance,
        cmd: LaunchCommand,
        warnings: Vec<PrepareWarning>,
    ) -> Result<PendingLaunch, SessionError> {
        let root = inst
            .dir
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| SessionError::Io {
                path: inst.dir.clone(),
                error: "instance directory is not inside <data>/instances".to_string(),
            })?;
        Self::pending_with_command(inst, &DataDirs::new(root.to_path_buf()), cmd, warnings)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn pending_with_command(
        inst: &Instance,
        data: &DataDirs,
        cmd: LaunchCommand,
        warnings: Vec<PrepareWarning>,
    ) -> Result<PendingLaunch, SessionError> {
        let instance_lock = lock_instance(inst)?;
        let version_lock = data.lock_version_shared(version_id(inst))?;
        refuse_if_running(inst)?;
        Ok(PendingLaunch {
            inst: inst.clone(),
            cmd,
            warnings,
            instance_lock,
            version_lock,
        })
    }

    /// Test entry: like `start` but with a ready-made command (no prepare).
    /// Must be called within a Tokio runtime (it spawns the tailer).
    #[cfg(test)]
    pub(crate) fn start_with_command(
        inst: &Instance,
        data: &DataDirs,
        cmd: &LaunchCommand,
    ) -> Result<GameSession, SessionError> {
        Self::pending_with_command(inst, data, cmd.clone(), Vec::new())?.spawn_now()
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
    /// Tails the log from its beginning.
    ///
    /// - [`Reattach::Attached`]: this call now holds the game's session.
    /// - [`Reattach::HeldElsewhere`]: the game runs, but another session
    ///   holds the instance (in this or another Patina process), or its
    ///   version is locked; the record is kept. Use
    ///   [`KillHandle::for_running`] to stop it and read [`CURRENT_LOG`] for
    ///   its output.
    /// - [`Reattach::NotRunning`]: no verified game; a stale record is
    ///   cleared when this call could take the instance lock.
    ///
    /// Must be called within a Tokio runtime. The data dir (for the version
    /// lock) is derived from the `<data>/instances/<id>` layout of `inst.dir`.
    pub fn reattach(inst: &Instance) -> Reattach {
        let Some(rec) = read_record(inst) else {
            return Reattach::NotRunning;
        };
        if rec.instance_id != inst.id {
            // A copied instance directory brings a foreign record along.
            tracing::warn!("clearing a running record that names another instance");
            clear_record(inst);
            return Reattach::NotRunning;
        }
        let instance_lock = match lock_instance(inst) {
            Ok(l) => l,
            // Someone else holds the instance: report, never clear, the record.
            Err(_) if verify(&rec) => return Reattach::HeldElsewhere(RunningInfo::from(&rec)),
            Err(_) => return Reattach::NotRunning,
        };
        if !verify(&rec) {
            clear_record(inst);
            return Reattach::NotRunning;
        }
        let Some(data_dir) = inst.dir.parent().and_then(Path::parent) else {
            return Reattach::NotRunning;
        };
        let data = DataDirs::new(data_dir.to_path_buf());
        let version_lock = match data.lock_version_shared(version_id(inst)) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("cannot reattach to {}: {e}", inst.id);
                return Reattach::HeldElsewhere(RunningInfo::from(&rec));
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
        Reattach::Attached(s)
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
            state: self.state.clone(),
            stop_marker: stop_request_path(&self.inst),
            external: false,
            signals: Signals::REAL,
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
        // A game the user stopped exits non-zero by design, also when the
        // stop came from another launcher process (its stop request names
        // this game's pid), even if that escalated to SIGKILL.
        // Death by signal (SIGABRT, SIGSEGV, OOM kill) has no exit code but is a crash.
        let bad_code = failed && !killed;
        // Directory scans, a 64 KiB read and a rename: off the async workers.
        // The locks (held by `self`) are released only after this finished.
        let (inst, started_at, pid) = (self.inst.clone(), self.started_at, self.pid);
        let finish = tokio::task::spawn_blocking(move || {
            let marker = stop_request_path(&inst);
            let stop_requested = read_stop_request(&marker) == Some(pid);
            let crashed = (bad_code && !stop_requested) || crash_evidence(&inst, started_at);
            let saved_log = crashed.then(|| save_log(&inst)).flatten();
            clear_record(&inst);
            clear_stop_request(&marker);
            (crashed, saved_log)
        })
        .await;
        let (crashed, saved_log) = finish.unwrap_or_else(|e| {
            tracing::warn!("cannot finish the game session: {e}");
            (bad_code, None)
        });
        SessionExit {
            code,
            crashed,
            saved_log,
        }
    }
}

/// A crash report written since `started_at`, or a crash marker near the end
/// of the log.
fn crash_evidence(inst: &Instance, started_at: u64) -> bool {
    let reports = inst.game_dir().join("crash-reports");
    if let Ok(rd) = std::fs::read_dir(&reports) {
        for e in rd.flatten() {
            let secs = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            if secs.is_some_and(|s| s >= started_at) {
                return true;
            }
        }
    }
    let tail = read_tail(&inst.dir.join(CURRENT_LOG), CRASH_SCAN_BYTES);
    CRASH_MARKERS.iter().any(|m| tail.contains(m))
}

/// Renames the current log to `patina-logs/<UTC time>-crash.log`.
fn save_log(inst: &Instance) -> Option<PathBuf> {
    let src = inst.dir.join(CURRENT_LOG);
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
    use crate::launch::running::{STOP_REQUEST, read_record};
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

    fn marker(e: &Env) -> PathBuf {
        e.inst.dir.join(STOP_REQUEST)
    }

    thread_local! {
        /// `(marker path, its content when the signal went out)`.
        static MARKER_AT_SIGNAL: std::cell::RefCell<Option<(PathBuf, Option<String>)>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Another launcher process's force stop, already escalated: SIGKILL
    /// straight away to the game's process group. Only ever reached through
    /// `kill_blocking`, which checks the pid still carries the identity of
    /// the test's own game (a detached group leader, so the group is the
    /// test's own too).
    fn external_sigkill(pid: u32) -> Result<(), platform::KillFailure> {
        MARKER_AT_SIGNAL.with(|m| {
            if let Some((path, seen)) = m.borrow_mut().as_mut() {
                *seen = std::fs::read_to_string(path).ok();
            }
        });
        let st = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status()
            .map_err(|error| platform::KillFailure {
                error,
                sigkill_delivered: false,
            })?;
        assert!(st.success());
        Ok(())
    }

    fn no_group(_: u32) -> io::Result<()> {
        panic!("the game's group must not be signalled here");
    }

    #[tokio::test]
    async fn stop_request_marks_external_kill_as_stop() {
        let e = env("i1");
        // `sleep 60 & wait`, not `exec sleep 60`: the shell must stay the
        // group leader carrying the `-Dpatina.instance=` marker `verify` needs.
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "sleep 60 & wait"))
            .unwrap();
        let _k = Killer(s.killer());
        let pid = s.pid();
        // Another process's handle: built from the record, it shares no
        // flags with the session.
        let mut other = KillHandle::for_running(&e.inst).expect("the game runs");
        assert!(other.external);
        assert_eq!((other.pid, other.token), (pid, s.token));
        assert_eq!(other.stop_marker, marker(&e));
        other.signals = Signals {
            kill_pid: external_sigkill,
            kill_group: no_group,
        };
        MARKER_AT_SIGNAL.with(|m| *m.borrow_mut() = Some((marker(&e), None)));
        other.kill_blocking().unwrap();
        let seen = MARKER_AT_SIGNAL.with(|m| m.borrow_mut().take()).unwrap().1;
        assert_eq!(
            seen,
            Some(pid.to_string()),
            "marker written before the signal"
        );
        let exit = s.wait().await;
        assert_eq!(exit.code, None, "killed by SIGKILL");
        assert!(!exit.crashed, "a requested stop is not a crash");
        assert_eq!(exit.saved_log, None);
        assert!(!marker(&e).exists(), "wait clears the marker");
    }

    #[tokio::test]
    async fn stop_request_for_another_pid_is_ignored() {
        let e = env("i1");
        let go = e.inst.dir.join("go");
        let script = format!(
            "while [ ! -f {} ]; do sleep 0.05; done; kill -KILL $$",
            go.display()
        );
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", &script)).unwrap();
        let _k = Killer(s.killer());
        let stale = s.pid().wrapping_add(1).to_string();
        crate::fsutil::write_atomic(&marker(&e), stale.as_bytes()).unwrap();
        std::fs::write(&go, "").unwrap();
        let exit = s.wait().await;
        assert_eq!(exit.code, None);
        assert!(exit.crashed, "SIGKILL without a matching stop request");
        assert!(!marker(&e).exists());
    }

    #[tokio::test]
    async fn stale_stop_request_is_cleared_on_launch() {
        let e = env("i1");
        crate::fsutil::write_atomic(&marker(&e), b"12345").unwrap();
        let pending =
            GameSession::prepare_launch_with_command(&e.inst, sh("i1", "exit 0"), Vec::new())
                .unwrap();
        assert!(marker(&e).exists(), "preparing changes nothing yet");
        let s = pending.launch(None).await.unwrap();
        assert!(!marker(&e).exists());
        s.wait().await;
    }

    #[tokio::test]
    async fn refused_launch_keeps_the_running_games_stop_request() {
        let e = env("i1");
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "sleep 60 & wait"))
            .unwrap();
        let _k = Killer(s.killer());
        drop(s);
        crate::fsutil::write_atomic(&marker(&e), b"1").unwrap();
        let r = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "exit 0"));
        assert!(matches!(r, Err(SessionError::AlreadyRunning(_))));
        assert!(marker(&e).exists());
    }

    /// A verified game whose instance lock is held through another open file
    /// of the lock file, as another Patina process would hold it.
    #[tokio::test]
    async fn reattach_reports_a_game_held_elsewhere() {
        use std::fs::File;
        let e = env("i1");
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "sleep 60 & wait"))
            .unwrap();
        let _k = Killer(s.killer());
        let pid = s.pid();
        drop(s);
        let other = File::options()
            .read(true)
            .write(true)
            .open(e.inst.dir.join(crate::locks::LOCK_FILE))
            .unwrap();
        // The session just released it; a concurrent spawn may still share it.
        crate::locks::test_lock(&other, false).unwrap();
        match GameSession::reattach(&e.inst) {
            Reattach::HeldElsewhere(info) => assert_eq!(info.pid, pid),
            r => panic!("expected HeldElsewhere, got {r:?}"),
        }
        assert!(read_record(&e.inst).is_some(), "the record is kept");
        assert_eq!(
            crate::launch::running::running_game(&e.inst).map(|i| i.pid),
            Some(pid)
        );
        drop(other);
        let s = GameSession::reattach(&e.inst)
            .attached()
            .expect("attached once the other holder is gone");
        assert_eq!(s.pid(), pid);
        s.kill().await.unwrap();
        assert!(!s.wait().await.crashed);
    }

    #[tokio::test]
    async fn for_running_needs_a_verified_game() {
        let e = env("i1");
        assert!(KillHandle::for_running(&e.inst).is_none());
        let rec = RunningRecord {
            pid: 1,
            start_time: 1,
            instance_id: "i1".into(),
            started_at: 0,
            log: CURRENT_LOG.into(),
        };
        write_record(&e.inst, &rec).unwrap();
        assert!(KillHandle::for_running(&e.inst).is_none());
        assert_eq!(read_record(&e.inst), Some(rec), "not cleared");
    }

    /// An external handle never signals the group of a leader that already
    /// exited, even while it is an unreaped zombie. Signals are recorded,
    /// not sent.
    #[tokio::test]
    async fn external_handle_never_signals_the_group() {
        let e = env("i1");
        let go = e.inst.dir.join("go");
        let script = format!(
            "while [ ! -f {} ]; do sleep 0.05; done; sleep 60 & exit 0",
            go.display()
        );
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", &script)).unwrap();
        let (pid, token) = (s.pid(), s.token.unwrap());
        let owner = s.killer();
        let mut other = KillHandle::for_running(&e.inst).expect("the game runs");
        other.signals = recording(None);
        std::fs::write(&go, "").unwrap();
        poll_zombie(pid, token).await;
        SENT.with(|s| s.borrow_mut().clear());
        other.kill_blocking().unwrap();
        assert_eq!(sent(), [], "only the owner may signal the group");
        assert!(!marker(&e).exists());
        // The owner cleans up the test's own leftover group member.
        owner.kill().await.unwrap();
        assert!(group_gone(pid).await);
        s.wait().await;
    }

    #[test]
    fn pending_launch_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PendingLaunch>();
    }

    #[tokio::test]
    async fn pending_launch_holds_locks_until_dropped() {
        let e = env("i1");
        let store = InstanceStore::new(e.data.instances());
        let pending = GameSession::prepare_launch_with_command(
            &e.inst,
            sh("i1", "exec sleep 60"),
            Vec::new(),
        )
        .unwrap();
        let busy = store.lock("i1").unwrap_err();
        assert!(matches!(busy.what, BusyKind::Instance(_)));
        drop(pending);
        drop(store.lock("i1").expect("released after drop"));
        // Nothing was spawned: no running record and no log file.
        assert!(read_record(&e.inst).is_none());
        assert!(!e.inst.dir.join(CURRENT_LOG).exists());
    }

    #[tokio::test]
    async fn pending_launch_exposes_warnings_before_spawn() {
        let e = env("i1");
        let low = PrepareWarning::LowMemory {
            requested_mib: 8192,
            available_mib: 1024,
        };
        let pending = GameSession::prepare_launch_with_command(
            &e.inst,
            sh("i1", "exit 0"),
            vec![low.clone()],
        )
        .unwrap();
        assert_eq!(pending.warnings(), std::slice::from_ref(&low));
        assert!(read_record(&e.inst).is_none());
        let s = pending.launch(None).await.unwrap();
        assert!(matches!(s.warnings().first(), Some(SessionWarning::Prepare(w)) if *w == low));
        let _ = s.wait().await;
    }

    #[tokio::test]
    async fn pending_launch_refuses_a_running_instance() {
        let e = env("i1");
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "sleep 60 & wait"))
            .unwrap();
        let _k = Killer(s.killer());
        drop(s);
        assert!(matches!(
            GameSession::prepare_launch_with_command(&e.inst, sh("i1", "exit 0"), Vec::new()),
            Err(SessionError::AlreadyRunning(_) | SessionError::Busy(_))
        ));
    }

    #[tokio::test]
    async fn launch_rechecks_for_a_game_started_meanwhile() {
        let e = env("i1");
        let pending =
            GameSession::prepare_launch_with_command(&e.inst, sh("i1", "exit 0"), Vec::new())
                .unwrap();
        // A game of this instance appears after prepare (a process this test spawns).
        // The shell blocks on stdin and keeps the marker in its argv.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "read x", "x", "-Dpatina.instance=i1"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let start_time = platform::proc_info(pid).unwrap().start_time;
        write_record(
            &e.inst,
            &RunningRecord {
                pid,
                start_time,
                instance_id: "i1".into(),
                started_at: 0,
                log: CURRENT_LOG.into(),
            },
        )
        .unwrap();
        let r = pending.launch(None).await;
        let _ = child.kill();
        let _ = child.wait();
        assert!(matches!(r, Err(SessionError::AlreadyRunning(_))));
        assert!(!e.inst.dir.join(CURRENT_LOG).exists());
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

        let mut s = GameSession::reattach(&e.inst).attached().expect("reattach");
        assert_eq!(s.pid(), pid);
        // Already attached in this process now.
        assert!(matches!(
            GameSession::reattach(&e.inst),
            Reattach::HeldElsewhere(RunningInfo { pid: p, .. }) if p == pid
        ));
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
        assert!(matches!(
            GameSession::reattach(&e.inst),
            Reattach::NotRunning
        ));
        assert!(read_record(&e.inst).is_none());

        let other = RunningRecord {
            instance_id: "zzz".into(),
            ..rec
        };
        crate::launch::running::write_record(&e.inst, &other).unwrap();
        assert!(matches!(
            GameSession::reattach(&e.inst),
            Reattach::NotRunning
        ));
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

    thread_local! {
        /// Signals the recording `Signals` "sent" on this thread.
        static SENT: std::cell::RefCell<Vec<(&'static str, u32)>> = Default::default();
    }

    fn sent() -> Vec<(&'static str, u32)> {
        SENT.with(|s| s.borrow().clone())
    }

    /// Records every signal instead of sending it; `kill_pid` answers with
    /// `outcome`.
    fn recording(outcome: Option<bool>) -> Signals {
        fn group(pid: u32) -> io::Result<()> {
            SENT.with(|s| s.borrow_mut().push(("group", pid)));
            Ok(())
        }
        fn ok(pid: u32) -> Result<(), platform::KillFailure> {
            SENT.with(|s| s.borrow_mut().push(("pid", pid)));
            Ok(())
        }
        fn fail(pid: u32, sigkill_delivered: bool) -> Result<(), platform::KillFailure> {
            SENT.with(|s| s.borrow_mut().push(("pid", pid)));
            Err(platform::KillFailure {
                error: io::Error::other("injected"),
                sigkill_delivered,
            })
        }
        fn fail_early(pid: u32) -> Result<(), platform::KillFailure> {
            fail(pid, false)
        }
        fn fail_late(pid: u32) -> Result<(), platform::KillFailure> {
            fail(pid, true)
        }
        Signals {
            kill_pid: match outcome {
                None => ok,
                Some(false) => fail_early,
                Some(true) => fail_late,
            },
            kill_group: group,
        }
    }

    fn handle(
        pid: u32,
        token: Option<u64>,
        state: u8,
        stop_marker: &Path,
        signals: Signals,
    ) -> KillHandle {
        KillHandle {
            pid,
            token,
            killed: Arc::new(AtomicBool::new(false)),
            state: Arc::new(AtomicU8::new(state)),
            stop_marker: stop_marker.to_path_buf(),
            external: false,
            signals,
        }
    }

    /// A test-owned detached `exec sleep 60` (its own group leader).
    fn own_sleeper(dir: &Path) -> (Child, u32, u64) {
        let cmd = LaunchCommand {
            program: "sh".into(),
            args: vec!["-c".into(), "exec sleep 60".into()],
            cwd: std::env::temp_dir(),
        };
        let child = platform::spawn_detached(&cmd, &dir.join("sleeper.log")).unwrap();
        let pid = child.id();
        let token = platform::proc_info(pid).unwrap().start_time;
        (child, pid, token)
    }

    /// Waits until no live process is left in group `pgid`.
    async fn group_gone(pgid: u32) -> bool {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if crate::platform::process_group_members(pgid).is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    async fn poll_zombie(pid: u32, token: u64) {
        let end = Instant::now() + Duration::from_secs(10);
        while !platform::is_zombie_with_start(pid, token) {
            assert!(Instant::now() < end, "leader never became a zombie");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The leader exited but is unreaped (the session has not waited yet),
    /// and left a member of its group behind: the kill handle stops it.
    #[tokio::test]
    async fn kill_reaches_the_group_of_an_unreaped_leader() {
        let e = env("i1");
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "sleep 60 & exit 0"))
            .unwrap();
        let (pid, token) = (s.pid(), s.token.unwrap());
        let killer = s.killer();
        poll_zombie(pid, token).await;
        assert!(
            !platform::process_group_members(pid).is_empty(),
            "the sleep must outlive the leader"
        );
        killer.kill().await.unwrap();
        assert!(group_gone(pid).await, "group members survived the kill");
        // The leader had already exited on its own: no stop request for it.
        assert!(!marker(&e).exists());
        s.wait().await;
    }

    /// Regression (re-review): a reaped leader's pid may be recycled, so its
    /// group is never signalled, even while members are left.
    #[tokio::test]
    async fn reaped_leader_group_is_not_signalled() {
        let e = env("i1");
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "sleep 60 & exit 0"))
            .unwrap();
        let (pid, token) = (s.pid(), s.token.unwrap());
        s.wait().await; // reaps the leader
        assert!(!platform::process_group_members(pid).is_empty());
        SENT.with(|s| s.borrow_mut().clear());
        handle(pid, Some(token), RUNNING, &marker(&e), recording(None))
            .kill_blocking()
            .unwrap();
        assert_eq!(sent(), []);
        assert!(!marker(&e).exists());
        // Clean up the test's own leftover member (its group is still ours).
        platform::kill_process_group(pid).unwrap();
        assert!(group_gone(pid).await);
    }

    /// Regression (re-review): after `wait` the handle never signals, even a
    /// live process with the right identity.
    #[tokio::test]
    async fn handle_after_wait_signals_nothing() {
        let e = env("i1");
        let s = GameSession::start_with_command(&e.inst, &e.data, &sh("i1", "exit 0")).unwrap();
        let mut h = s.killer();
        h.signals = recording(None);
        s.wait().await;
        SENT.with(|s| s.borrow_mut().clear());
        h.kill_blocking().unwrap();
        assert_eq!(sent(), []);

        let (mut child, pid, token) = own_sleeper(&e.inst.dir);
        handle(pid, Some(token), EXITED, &marker(&e), recording(None))
            .kill_blocking()
            .unwrap();
        assert_eq!(sent(), [], "an exited session's handle stays silent");
        assert!(!marker(&e).exists());
        handle(pid, Some(token), RUNNING, &marker(&e), recording(None))
            .kill_blocking()
            .unwrap();
        assert_eq!(sent(), [("pid", pid)], "the seam is what would signal");
        assert_eq!(
            std::fs::read_to_string(marker(&e)).unwrap(),
            pid.to_string(),
            "the stop request is written before signalling"
        );
        platform::kill_pid(pid).unwrap();
        child.wait().unwrap();
    }

    /// A kill that fails before SIGKILL leaves the session not marked as
    /// user-killed; once SIGKILL was delivered the mark stays. Signals are
    /// injected: nothing is sent to any process.
    #[tokio::test]
    async fn failed_kill_rolls_back_the_killed_flag_until_sigkill() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, pid, token) = own_sleeper(dir.path());
        let stop = dir.path().join(STOP_REQUEST);
        for (sigkill_delivered, flag) in [(false, false), (true, true)] {
            let h = handle(
                pid,
                Some(token),
                RUNNING,
                &stop,
                recording(Some(sigkill_delivered)),
            );
            assert!(h.kill_blocking().is_err());
            assert_eq!(h.killed.load(Ordering::SeqCst), flag);
            assert_eq!(stop.exists(), flag, "the stop request is rolled back too");
        }
        platform::kill_pid(pid).unwrap();
        child.wait().unwrap();
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
        assert!(matches!(
            GameSession::reattach(&e.inst),
            Reattach::NotRunning
        ));
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
