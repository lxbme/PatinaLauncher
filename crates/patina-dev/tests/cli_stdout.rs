//! Running the `patina-dev` binary with a closed stdout.

/// Regression: `patina-dev status | head` panicked on EPIPE. Here stdout is a
/// pipe whose reader is closed before the tool starts, so its first write
/// fails; it must exit quietly instead.
#[test]
fn closed_stdout_exits_quietly() {
    let tmp = tempfile::tempdir().unwrap();
    for id in ["a", "b"] {
        let dir = tmp.path().join("instances").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("instance.toml"),
            "name = \"x\"\n\n[[components]]\nuid = \"minecraft\"\nversion = \"1.20.1\"\n",
        )
        .unwrap();
    }
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_patina-dev"))
        .args(["--data-dir"])
        .arg(tmp.path())
        .arg("status")
        .env_remove("RUST_LOG")
        .stdout(writer)
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(out.status.success(), "{:?}: {stderr}", out.status);
}
