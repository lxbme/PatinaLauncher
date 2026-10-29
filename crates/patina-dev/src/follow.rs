//! `logs --follow`: stream a reattached game's log until it exits.

use std::future::Future;
use std::time::Duration;

use patina_core::launch::{GameSession, LogLine, SessionExit};

/// How long to keep printing the drained tail after the game exited.
const DRAIN_GRACE: Duration = Duration::from_secs(3);

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

        let session = GameSession::reattach(&inst).expect("reattach");
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

        let session = GameSession::reattach(&inst).expect("reattach");
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
