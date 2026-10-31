//! `logs --follow`: stream a game's log until it exits, through a reattached
//! session or, when another Patina process holds the session, from the log
//! file directly.

use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use patina_core::launch::{GameSession, Log4jXmlParser, LogLine, SessionExit, Stream};

/// How long to keep printing the drained tail after the game exited.
const DRAIN_GRACE: Duration = Duration::from_secs(3);
/// How often [`follow_file`] looks for new output.
const FILE_POLL: Duration = Duration::from_millis(200);
/// Same raw-line bound as the session tailer.
const RAW_LINE_MAX: usize = 1024 * 1024;

/// Prints the log file at `path` of a game whose session another process
/// holds: everything already in it, then new lines as they are written,
/// until `running()` reports the game gone (returns `true` after printing
/// the rest) or `stop` resolves (returns `false`). Read-only: the file is
/// never changed, and an open file keeps working when the holder renames it
/// to a crash log.
pub async fn follow_file(
    path: &Path,
    mut out: impl FnMut(LogLine),
    mut running: impl FnMut() -> bool,
    stop: impl Future<Output = ()>,
) -> std::io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let mut parser = Log4jXmlParser::new();
    let mut partial: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut buf = vec![0u8; 64 * 1024];
    tokio::pin!(stop);
    loop {
        // Sampled before reading: output written before the game ended is
        // in the file by the time this read reaches its end.
        let alive = running();
        loop {
            // Yield and sample cancellation between bounded reads, even
            // when the producer never lets us reach EOF.
            tokio::select! {
                biased;
                _ = &mut stop => return Ok(false),
                _ = tokio::task::yield_now() => {}
            }
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            for chunk in buf[..n].split_inclusive(|&b| b == b'\n') {
                let (body, complete) = match chunk.strip_suffix(b"\n") {
                    Some(body) => (body, true),
                    None => (chunk, false),
                };
                let room = RAW_LINE_MAX.saturating_sub(partial.len());
                truncated |= body.len() > room;
                partial.extend_from_slice(&body[..body.len().min(room)]);
                if complete {
                    emit(&mut parser, &partial, truncated, &mut out);
                    partial.clear();
                    truncated = false;
                }
            }
        }
        if !alive {
            if !partial.is_empty() {
                emit(&mut parser, &partial, truncated, &mut out);
            }
            parser.finish(Stream::Stdout).into_iter().for_each(&mut out);
            return Ok(true);
        }
        tokio::select! {
            _ = &mut stop => return Ok(false),
            _ = tokio::time::sleep(FILE_POLL) => {}
        }
    }
}

fn emit(parser: &mut Log4jXmlParser, raw: &[u8], truncated: bool, out: &mut impl FnMut(LogLine)) {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let mut text = String::from_utf8_lossy(raw).into_owned();
    if truncated {
        text.push('…');
    }
    parser
        .push_line(&text, Stream::Stdout)
        .into_iter()
        .for_each(out);
}

/// Sends each log line of `session` to `out` until the game exits (returns
/// its exit) or `stop` resolves (returns `None`; the game keeps running).
///
/// The log channel only closes after `wait` has seen the exit, so lines are
/// forwarded on a separate task while this one waits.
pub async fn follow_logs(
    mut session: GameSession,
    mut out: impl FnMut(LogLine) + Send + 'static,
    stop: impl Future<Output = ()>,
) -> Option<SessionExit> {
    let mut rx = session.logs();
    let printer = tokio::spawn(async move {
        while let Some(l) = rx.recv().await {
            out(l);
        }
    });
    let wait = session.wait();
    tokio::pin!(wait, stop);
    tokio::select! {
        exit = &mut wait => {
            let _ = tokio::time::timeout(DRAIN_GRACE, printer).await;
            Some(exit)
        }
        // Dropping the pending wait drops the session: the tailer stops and
        // the running record stays for the next reattach.
        _ = &mut stop => {
            printer.abort();
            None
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use patina_core::instance::Instance;
    use patina_core::launch::LaunchCommand;
    use patina_core::launch::running::{CURRENT_LOG, RunningRecord, read_record, write_record};
    use patina_core::paths::DataDirs;
    use patina_core::platform::{kill_pid, proc_info, spawn_detached};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Kills a test-spawned process on drop, but only while the pid still
    /// carries that process's identity (it may be reaped and reused by then).
    struct Killer {
        pid: u32,
        token: Option<u64>,
    }
    fn killer(pid: u32) -> Killer {
        Killer {
            pid,
            token: proc_info(pid).map(|i| i.start_time),
        }
    }
    impl Drop for Killer {
        fn drop(&mut self) {
            if self.token.is_some() && proc_info(self.pid).map(|i| i.start_time) == self.token {
                let _ = kill_pid(self.pid);
            }
        }
    }

    /// Regression: `logs --follow` on a reattached game used to drain the
    /// channel before waiting, but the channel only closes once `wait` saw the
    /// exit, so it never returned after the game ended.
    #[tokio::test]
    async fn follow_returns_when_a_reattached_game_exits() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path().to_path_buf());
        let dir = data.instances().join("i1");
        std::fs::create_dir_all(dir.join("minecraft")).unwrap();
        let inst = Instance {
            id: "i1".into(),
            dir,
            config: Default::default(),
        };
        let log = inst.dir.join(CURRENT_LOG);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let go = inst.dir.join("go");
        let script = format!(
            "echo hello; while [ ! -f '{}' ]; do sleep 0.05; done; echo bye",
            go.display()
        );
        let cmd = LaunchCommand {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                script,
                "x".into(),
                "-Dpatina.instance=i1".into(),
            ],
            cwd: std::env::temp_dir(),
        };
        // Started by "another launcher": only the record and the log file link us.
        let mut child = spawn_detached(&cmd, &log).unwrap();
        let pid = child.id();
        let _k = killer(pid);
        let rec = RunningRecord {
            pid,
            start_time: proc_info(pid).unwrap().start_time,
            instance_id: "i1".into(),
            started_at: 0,
            log: CURRENT_LOG.into(),
        };
        write_record(&inst, &rec).unwrap();

        let session = GameSession::reattach(&inst).attached().expect("reattach");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        let task = tokio::spawn(follow_logs(
            session,
            move |l| sink.lock().unwrap().push(l.text),
            std::future::pending(),
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(&go, "").unwrap();

        let exit = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("follow must return once the game exits")
            .unwrap()
            .expect("an exit, not a stop");
        assert!(!exit.crashed);
        assert!(read_record(&inst).is_none());
        assert_eq!(*lines.lock().unwrap(), ["hello", "bye"]);
        let _ = child.wait();
    }

    #[tokio::test]
    async fn follow_file_prints_until_the_game_is_gone() {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("current.log");
        std::fs::write(&path, "one\n").unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let (r, sink) = (running.clone(), lines.clone());
        let p = path.clone();
        let task = tokio::spawn(async move {
            follow_file(
                &p,
                move |l| sink.lock().unwrap().push(l.text),
                move || r.load(Ordering::SeqCst),
                std::future::pending(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"two\r\nthree").unwrap();
        running.store(false, Ordering::SeqCst);
        let ended = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(ended);
        assert_eq!(*lines.lock().unwrap(), ["one", "two", "three"]);
        // The file is only read.
        assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\r\nthree");
    }

    #[tokio::test]
    async fn huge_partial_line_is_bounded_and_ready_stop_preempts_reading() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("huge.log");
        let bytes = vec![b'x'; 8 * 1024 * 1024];
        std::fs::write(&path, &bytes).unwrap();
        let mut lines = Vec::new();
        assert!(
            follow_file(
                &path,
                |l| lines.push(l.text),
                || false,
                std::future::pending()
            )
            .await
            .unwrap()
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].len() <= 1024 * 1024 + 3);
        assert!(lines[0].ends_with('…'));
        std::fs::write(&path, b"one\ntwo\n").unwrap();
        let mut count = 0;
        assert!(
            !follow_file(&path, |_| count += 1, || true, std::future::ready(()))
                .await
                .unwrap()
        );
        assert_eq!(
            count, 0,
            "a ready cancellation preempts even the initial drain"
        );
    }

    #[tokio::test]
    async fn cancellation_is_observed_while_the_log_keeps_growing() {
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("growing.log");
        std::fs::write(&path, "line\n").unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let mut stop = Some(stop);
        let mut count = 0;
        let ended = tokio::time::timeout(
            Duration::from_secs(1),
            follow_file(
                &path,
                |_| {
                    count += 1;
                    writer.write_all(b"another line\n").unwrap();
                    if count == 4 {
                        stop.take().unwrap().send(()).unwrap();
                    }
                },
                || true,
                async {
                    stopped.await.ok();
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!ended);
        assert_eq!(count, 4);
    }

    #[tokio::test]
    async fn follow_file_stop_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("current.log");
        std::fs::write(&path, "x\n").unwrap();
        let ended = follow_file(
            &path,
            |_| {},
            || true,
            tokio::time::sleep(Duration::from_millis(300)),
        )
        .await
        .unwrap();
        assert!(!ended);
    }

    #[tokio::test]
    async fn stop_leaves_the_game_running() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path().to_path_buf());
        let dir = data.instances().join("i1");
        std::fs::create_dir_all(dir.join("minecraft")).unwrap();
        let inst = Instance {
            id: "i1".into(),
            dir,
            config: Default::default(),
        };
        let log = inst.dir.join(CURRENT_LOG);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let cmd = LaunchCommand {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                // Not `…; sleep 60`: bash may exec a trailing simple command,
                // which drops the `-Dpatina.instance=` marker `verify` needs.
                "echo hello; sleep 60 & wait".into(),
                "x".into(),
                "-Dpatina.instance=i1".into(),
            ],
            cwd: std::env::temp_dir(),
        };
        let mut child = spawn_detached(&cmd, &log).unwrap();
        let pid = child.id();
        let k = killer(pid);
        let rec = RunningRecord {
            pid,
            start_time: proc_info(pid).unwrap().start_time,
            instance_id: "i1".into(),
            started_at: 0,
            log: CURRENT_LOG.into(),
        };
        write_record(&inst, &rec).unwrap();

        let session = GameSession::reattach(&inst).attached().expect("reattach");
        let out = follow_logs(
            session,
            |_| {},
            tokio::time::sleep(Duration::from_millis(300)),
        );
        let r = tokio::time::timeout(Duration::from_secs(10), out)
            .await
            .unwrap();
        assert!(r.is_none());
        assert!(proc_info(pid).is_some(), "the game keeps running");
        assert!(read_record(&inst).is_some(), "the record stays");
        drop(k);
        let _ = child.wait();
    }
}
