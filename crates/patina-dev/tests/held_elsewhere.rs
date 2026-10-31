//! `status`, `logs` and `kill` on a game whose session another Patina
//! process holds (here: the test, through its own handle on the instance's
//! lock file).
#![cfg(target_os = "linux")]

use std::fs::File;
use std::process::Command;

use patina_core::instance::InstanceStore;
use patina_core::launch::LaunchCommand;
use patina_core::launch::running::{CURRENT_LOG, RunningRecord, write_record};
use patina_core::locks::LOCK_FILE;
use patina_core::platform::{kill_pid, proc_info, spawn_detached};

/// Kills the test's own game on drop, only while the pid still carries its
/// identity.
struct Killer {
    pid: u32,
    token: Option<u64>,
}
impl Drop for Killer {
    fn drop(&mut self) {
        if self.token.is_some() && proc_info(self.pid).map(|i| i.start_time) == self.token {
            let _ = kill_pid(self.pid);
        }
    }
}

fn dev(data: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_patina-dev"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn status_logs_and_kill_reach_a_game_held_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("instances").join("a");
    std::fs::create_dir_all(dir.join("minecraft")).unwrap();
    std::fs::write(
        dir.join("instance.toml"),
        "name = \"x\"\n\n[[components]]\nuid = \"minecraft\"\nversion = \"1.20.1\"\n",
    )
    .unwrap();
    let inst = InstanceStore::new(tmp.path().join("instances"))
        .load("a")
        .unwrap();
    let cmd = LaunchCommand {
        program: "sh".into(),
        // The shell stays the group leader that carries the marker.
        args: [
            "-c",
            "echo hello; sleep 60 & wait",
            "x",
            "-Dpatina.instance=a",
        ]
        .map(String::from)
        .to_vec(),
        cwd: std::env::temp_dir(),
    };
    let mut child = spawn_detached(&cmd, &dir.join(CURRENT_LOG)).unwrap();
    let pid = child.id();
    let token = proc_info(pid).map(|i| i.start_time);
    let _k = Killer { pid, token };
    write_record(
        &inst,
        &RunningRecord {
            pid,
            start_time: token.unwrap(),
            instance_id: "a".into(),
            started_at: 0,
            log: CURRENT_LOG.into(),
        },
    )
    .unwrap();
    // "Another Patina process" holds the instance.
    let holder = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE))
        .unwrap();
    holder.try_lock().unwrap();

    let (ok, out, err) = dev(tmp.path(), &["status"]);
    assert!(ok, "{err}");
    assert!(
        out.contains(&format!(
            "a  running pid={pid} (held by another Patina process)"
        )),
        "{out}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (ok, out, err) = dev(tmp.path(), &["logs", "a"]);
        assert!(ok, "{err}");
        if out.contains("hello") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no log output: {out}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Stops the test's own game: it was spawned above and verified by pid,
    // start time and marker before any signal.
    let (ok, out, err) = dev(tmp.path(), &["kill", "a"]);
    assert!(ok, "{err}");
    assert!(out.contains(&format!("Stopped pid {pid}")), "{out}");
    child.wait().unwrap();
    // The holder (here: nobody) would clear the record; the stop request
    // is there for it.
    assert_eq!(
        std::fs::read_to_string(dir.join("patina-logs/stop-request")).unwrap(),
        pid.to_string()
    );
    let (ok, out, _) = dev(tmp.path(), &["status"]);
    assert!(ok);
    assert!(out.contains("a  stopped"), "{out}");
    drop(holder);
}
