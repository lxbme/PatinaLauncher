//! Running-record file: lets a reopened launcher find and verify a game it
//! started earlier.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::instance::Instance;

pub const RUNNING_FILE: &str = "patina-running.json";
/// Game output log, relative to the instance directory.
pub const CURRENT_LOG: &str = "patina-logs/current.log";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningRecord {
    pub pid: u32,
    /// Opaque process identity token (see `platform::ProcInfo::start_time`).
    pub start_time: u64,
    pub instance_id: String,
    /// Wall-clock Unix seconds when the launcher started the game (display only).
    pub started_at: u64,
    /// Log file path relative to the instance directory.
    pub log: String,
}

fn record_path(inst: &Instance) -> PathBuf {
    inst.dir.join(RUNNING_FILE)
}

pub fn write_record(inst: &Instance, rec: &RunningRecord) -> io::Result<()> {
    let data = serde_json::to_vec_pretty(rec).map_err(io::Error::other)?;
    crate::fsutil::write_atomic(&record_path(inst), &data)
}

/// The stored record; `None` when missing or unparseable.
pub fn read_record(inst: &Instance) -> Option<RunningRecord> {
    let path = record_path(inst);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!("cannot read running record {}: {e}", path.display());
            }
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(
                "ignoring unparseable running record {}: {e}",
                path.display()
            );
            None
        }
    }
}

pub fn clear_record(inst: &Instance) {
    if let Err(e) = std::fs::remove_file(record_path(inst))
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(
            "cannot remove running record {}: {e}",
            record_path(inst).display()
        );
    }
}

/// Whether `rec` still describes the live game: the process exists, started at
/// the recorded time, and carries this instance's marker (guards against pid reuse).
pub fn verify(rec: &RunningRecord) -> bool {
    let Some(info) = crate::platform::proc_info(rec.pid) else {
        return false;
    };
    let marker = format!("{}={}", super::args::INSTANCE_MARKER_PROP, rec.instance_id);
    info.start_time == rec.start_time && info.cmdline.contains(&marker)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::launch::LaunchCommand;
    use crate::platform::{kill_pid, proc_info, spawn_detached};
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

    fn sh(script: &str) -> LaunchCommand {
        LaunchCommand {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
        }
    }

    fn inst(dir: &std::path::Path) -> Instance {
        Instance {
            id: "i1".into(),
            dir: dir.to_path_buf(),
            config: Default::default(),
        }
    }

    #[test]
    #[allow(clippy::zombie_processes)] // the point: the launcher drops the Child
    fn detached_child_survives_launcher_drop() {
        let dir = tempfile::tempdir().unwrap();
        let child =
            spawn_detached(&sh("exec sleep 30"), &dir.path().join("l/current.log")).unwrap();
        let pid = child.id();
        let _g = killer(pid);
        drop(child);
        assert!(proc_info(pid).is_some());
        assert_eq!(crate::platform::process_group_of(pid), Some(pid));
        kill_pid(pid).unwrap();
        assert!(proc_info(pid).is_none());
    }

    #[test]
    fn kill_pid_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = spawn_detached(&sh("sleep 30 & wait"), &dir.path().join("l.log")).unwrap();
        let pid = child.id();
        let _g = killer(pid);
        // Find the sleep child by process group.
        let find = || {
            std::fs::read_dir("/proc")
                .unwrap()
                .flatten()
                .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
                .find(|&p| p != pid && crate::platform::process_group_of(p) == Some(pid))
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let sleeper = loop {
            if let Some(p) = find() {
                break p;
            }
            assert!(std::time::Instant::now() < deadline, "no group member");
            std::thread::sleep(Duration::from_millis(20));
        };
        kill_pid(pid).unwrap();
        child.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        // The orphaned sleeper is reparented and reaped by init; it must vanish.
        while proc_info(sleeper).is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "group member survived"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn log_is_appended_across_spawns() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("current.log");
        for w in ["first", "second"] {
            spawn_detached(&sh(&format!("echo {w}")), &log)
                .unwrap()
                .wait()
                .unwrap();
        }
        let s = std::fs::read_to_string(&log).unwrap();
        assert!(s.contains("first") && s.contains("second"));
    }

    #[test]
    fn output_goes_to_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("current.log");
        let mut c = spawn_detached(&sh("echo out; echo err 1>&2"), &log).unwrap();
        c.wait().unwrap();
        let s = std::fs::read_to_string(&log).unwrap();
        assert!(s.contains("out") && s.contains("err"));
    }

    #[test]
    fn zombie_counts_as_not_running() {
        let mut c = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = c.id();
        // Exited but unreaped: it must disappear from proc_info.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while proc_info(pid).is_some() {
            assert!(std::time::Instant::now() < deadline, "still reported alive");
            std::thread::sleep(Duration::from_millis(20));
        }
        let rec = RunningRecord {
            pid,
            start_time: 0,
            instance_id: "i1".into(),
            started_at: 0,
            log: CURRENT_LOG.into(),
        };
        assert!(!verify(&rec));
        kill_pid(pid).unwrap();
        c.wait().unwrap();
    }

    #[test]
    fn verify_accepts_live_game_and_rejects_stale() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = spawn_detached(
            &LaunchCommand {
                program: "sh".into(),
                args: ["-c", "sleep 30 & wait", "x", "-Dpatina.instance=i1"]
                    .map(String::from)
                    .to_vec(),
                cwd: std::env::temp_dir(),
            },
            &dir.path().join("l.log"),
        )
        .unwrap();
        let pid = child.id();
        let _g = killer(pid);
        std::thread::sleep(Duration::from_millis(200));
        let info = proc_info(pid).unwrap();
        let mut rec = RunningRecord {
            pid,
            start_time: info.start_time,
            instance_id: "i1".into(),
            started_at: 0,
            log: CURRENT_LOG.into(),
        };
        assert!(verify(&rec));
        rec.start_time += 100;
        assert!(!verify(&rec));
        rec.start_time = info.start_time;
        rec.instance_id = "other".into();
        assert!(!verify(&rec));
        kill_pid(pid).unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn stale_record_with_reused_pid_is_rejected() {
        let me = std::process::id();
        let info = proc_info(me).unwrap();
        let mut rec = RunningRecord {
            pid: me,
            start_time: info.start_time + 1000,
            instance_id: "i1".into(),
            started_at: 0,
            log: CURRENT_LOG.into(),
        };
        assert!(!verify(&rec));
        // Right start time, but no marker in the command line.
        rec.start_time = info.start_time;
        assert!(!verify(&rec));
    }

    #[test]
    fn record_round_trip_and_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let i = inst(dir.path());
        assert!(read_record(&i).is_none());
        let rec = RunningRecord {
            pid: 7,
            start_time: 8,
            instance_id: "i1".into(),
            started_at: 9,
            log: CURRENT_LOG.into(),
        };
        write_record(&i, &rec).unwrap();
        assert_eq!(read_record(&i), Some(rec));
        clear_record(&i);
        assert!(read_record(&i).is_none());
        std::fs::write(dir.path().join(RUNNING_FILE), "{nope").unwrap();
        assert!(read_record(&i).is_none());
        clear_record(&i);
        clear_record(&i);
    }
}
