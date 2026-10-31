use super::credentials::*;
use crate::paths::DataDirs;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

fn setup() -> (tempfile::TempDir, DataDirs, PrivateCredentialDir) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let dir = PrivateCredentialDir::open(&data).unwrap();
    (tmp, data, dir)
}

#[test]
fn secure_files_reject_links_and_bad_modes() {
    let (tmp, data, dir) = setup();
    dir.write("record.json", b"synthetic").unwrap();
    let root = tmp.path().join("credentials");
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(root.join("record.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let victim = tmp.path().join("victim");
    fs::write(&victim, b"unchanged").unwrap();
    symlink(&victim, root.join("link.json")).unwrap();
    assert!(dir.read("link.json").is_err());
    assert!(dir.write("link.json", b"changed").is_err());
    assert!(dir.delete("link.json").is_err());
    assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
    fs::hard_link(root.join("record.json"), root.join("hard.json")).unwrap();
    assert!(dir.read("record.json").is_err());
    assert!(dir.write("hard.json", b"changed").is_err());
    fs::remove_file(root.join("hard.json")).unwrap();
    fs::set_permissions(root.join("record.json"), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(dir.read("record.json").is_err());
    fs::create_dir(root.join("directory.json")).unwrap();
    assert!(dir.read("directory.json").is_err());
    for name in ["../victim", "/absolute", "", "a/b", ".", ".."] {
        assert!(dir.write(name, b"no").is_err());
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(PrivateCredentialDir::open(&data).is_err());
}

#[test]
fn roots_ancestors_and_replaced_paths_are_pinned() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    fs::create_dir(&real).unwrap();
    symlink(&real, tmp.path().join("link")).unwrap();
    assert!(PrivateCredentialDir::open(&DataDirs::new(tmp.path().join("link"))).is_err());
    assert!(PrivateCredentialDir::open(&DataDirs::new(tmp.path().join("link/child"))).is_err());
    fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(PrivateCredentialDir::open(&DataDirs::new(&real)).is_err());
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    let dir = PrivateCredentialDir::open(&DataDirs::new(&real)).unwrap();
    fs::rename(real.join("credentials"), real.join("pinned")).unwrap();
    symlink(tmp.path(), real.join("credentials")).unwrap();
    dir.write("record.json", b"pinned").unwrap();
    assert_eq!(
        fs::read(real.join("pinned/record.json")).unwrap(),
        b"pinned"
    );
    assert!(!tmp.path().join("record.json").exists());
}

// A post-rename fsync failure cannot honestly be reported as rollback.
#[test]
fn atomic_secret_failure_keeps_old_record_until_rename() {
    for stage in [
        WriteStage::Create,
        WriteStage::Write,
        WriteStage::FileSync,
        WriteStage::Rename,
        WriteStage::DirectorySync,
    ] {
        let (tmp, _, dir) = setup();
        dir.write("record.json", b"old").unwrap();
        assert!(dir.write_failing("record.json", b"new", stage).is_err());
        let expected: &[u8] = if stage == WriteStage::DirectorySync {
            b"new"
        } else {
            b"old"
        };
        assert_eq!(dir.read("record.json").unwrap().unwrap(), expected);
        assert_eq!(
            fs::read_dir(tmp.path().join("credentials"))
                .unwrap()
                .count(),
            1
        );
    }
}

#[test]
fn temp_collision_does_not_follow_link() {
    let (tmp, _, dir) = setup();
    let victim = tmp.path().join("victim");
    fs::write(&victim, b"unchanged").unwrap();
    symlink(&victim, tmp.path().join("credentials/.temp-test")).unwrap();
    assert!(
        dir.write_with_temp("record.json", b"new", ".temp-test", None)
            .is_err()
    );
    assert_eq!(fs::read(victim).unwrap(), b"unchanged");
}

#[test]
fn deletion_missing_is_idempotent_and_lock_is_exclusive() {
    let (_tmp, data, dir) = setup();
    dir.delete("missing.json").unwrap();
    let guard = dir.try_lock().unwrap().unwrap();
    assert!(
        PrivateCredentialDir::open(&data)
            .unwrap()
            .try_lock()
            .unwrap()
            .is_none()
    );
    drop(guard);
    assert!(dir.try_lock().unwrap().is_some());
}

#[test]
fn operation_lock_excludes_another_process() {
    let (_tmp, data, dir) = setup();
    let guard = dir.try_lock().unwrap().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "platform::credentials_tests::lock_child",
        ])
        .env("PATINA_TEST_CREDENTIAL_LOCK", data.root())
        .status()
        .unwrap();
    assert!(child.success());
    drop(guard);
}
#[test]
#[ignore = "isolated subprocess helper, invoked by operation_lock_excludes_another_process"]
fn lock_child() {
    let path =
        std::env::var_os("PATINA_TEST_CREDENTIAL_LOCK").expect("parent supplies isolated root");
    assert!(
        PrivateCredentialDir::open(&DataDirs::new(path))
            .unwrap()
            .try_lock()
            .unwrap()
            .is_none()
    );
}

#[test]
fn operation_lock_cleans_only_valid_private_crash_temps() {
    let (tmp, _, dir) = setup();
    let stale = ".temp-c7137070-3696-4aa9-b592-7f21e0c10823";
    dir.write(stale, b"SYNTHETIC_CRASH_SECRET").unwrap();
    dir.write("unrelated", b"preserve").unwrap();
    let lock = dir.try_lock().unwrap().unwrap();
    assert!(!tmp.path().join("credentials").join(stale).exists());
    assert_eq!(dir.read("unrelated").unwrap().unwrap(), b"preserve");
    drop(lock);
    let victim = tmp.path().join("victim");
    fs::write(&victim, b"unchanged").unwrap();
    symlink(&victim, tmp.path().join("credentials").join(stale)).unwrap();
    assert!(dir.try_lock().is_err());
    assert_eq!(fs::read(victim).unwrap(), b"unchanged");
}

#[test]
fn special_files_and_bad_lock_journal_permissions_are_rejected() {
    let (tmp, _, dir) = setup();
    let root = tmp.path().join("credentials");
    let fifo = root.join("fifo.json");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    assert!(dir.read("fifo.json").is_err());
    let _socket = std::os::unix::net::UnixListener::bind(root.join("socket.json")).unwrap();
    assert!(dir.read("socket.json").is_err());
    dir.write("cleanup.json", b"[]").unwrap();
    fs::set_permissions(root.join("cleanup.json"), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(dir.read("cleanup.json").is_err());
    fs::write(root.join("operation.lock"), b"").unwrap();
    fs::set_permissions(
        root.join("operation.lock"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(dir.try_lock().is_err());
}

#[test]
fn account_metadata_accepts_legacy_modes_but_rejects_links_and_unsafe_entries() {
    let (tmp, data, _dir) = setup();
    let metadata = tmp.path().join("accounts.toml");
    fs::write(&metadata, b"legacy").unwrap();
    fs::set_permissions(&metadata, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(read_account_metadata(&data).unwrap().unwrap(), b"legacy");
    write_account_metadata(&data, b"durable", None).unwrap();
    assert_eq!(
        fs::metadata(&metadata).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(read_account_metadata(&data).unwrap().unwrap(), b"durable");
    let linked = tmp.path().join("linked");
    fs::hard_link(&metadata, &linked).unwrap();
    assert_eq!(
        read_account_metadata(&data).unwrap_err(),
        crate::accounts::credentials::CredentialError::UnsafePath
    );
    assert!(write_account_metadata(&data, b"must-not-write", None).is_err());
    fs::remove_file(&metadata).unwrap();
    symlink(&linked, &metadata).unwrap();
    assert!(read_account_metadata(&data).is_err());
    assert!(write_account_metadata(&data, b"must-not-write", None).is_err());
    assert_eq!(fs::read(&linked).unwrap(), b"durable");
    fs::remove_file(&metadata).unwrap();
    fs::write(&metadata, b"unsafe").unwrap();
    fs::set_permissions(&metadata, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(read_account_metadata(&data).is_err());
    assert!(write_account_metadata(&data, b"must-not-write", None).is_err());
    assert_eq!(fs::read(&metadata).unwrap(), b"unsafe");
}
