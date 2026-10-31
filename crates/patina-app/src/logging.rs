//! Logging: `tracing` to `<data>/logs/patina.log`, one file per run; the
//! previous run's log is kept as `patina.prev.log`.
//!
//! The default filter is [`DEFAULT_FILTER`]; `RUST_LOG` replaces it.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use patina_core::paths::DataDirs;
use tracing_subscriber::EnvFilter;

pub const DEFAULT_FILTER: &str = "warn,patina_core=info,patina_app=info";
pub const LOG_FILE: &str = "patina.log";
pub const PREV_LOG_FILE: &str = "patina.prev.log";

/// `<data>/logs`.
pub fn log_dir(data: &DataDirs) -> PathBuf {
    data.root().join("logs")
}

/// Installs the global subscriber, writing to `<data>/logs/patina.log`
/// (stderr when that cannot be opened), and logs panics. Call once, after
/// the single-instance lock: a second instance must not rotate the first
/// one's log.
pub fn init(data: &DataDirs) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_thread_names(true);
    let dir = log_dir(data);
    let installed = match open_log(&dir) {
        Ok(file) => builder
            .with_ansi(false)
            .with_writer(Mutex::new(file))
            .try_init(),
        Err(e) => {
            eprintln!("patina: cannot open the log in {}: {e}", dir.display());
            builder.with_writer(io::stderr).try_init()
        }
    };
    if let Err(e) = installed {
        eprintln!("patina: logging already initialized: {e}");
        return;
    }
    // A GUI process has no terminal to print a panic to; put it in the log
    // as well (the default hook still prints it).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(%info, "panic");
        default_hook(info);
    }));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), data = %data.root().display(), "patina starting");
}

/// Moves last run's `patina.log` to `patina.prev.log` (replacing the older
/// one) and creates a fresh `patina.log` in `dir`.
fn open_log(dir: &Path) -> io::Result<File> {
    fs::create_dir_all(dir)?;
    let log = dir.join(LOG_FILE);
    match fs::rename(&log, dir.join(PREV_LOG_FILE)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            eprintln!("patina: cannot keep the previous log: {e}");
        }
        _ => {}
    }
    File::create(log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn each_run_keeps_one_previous_log() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap();

        writeln!(open_log(&dir).unwrap(), "run 1").unwrap();
        assert_eq!(read(LOG_FILE), "run 1\n");
        assert!(!dir.join(PREV_LOG_FILE).exists());

        writeln!(open_log(&dir).unwrap(), "run 2").unwrap();
        assert_eq!(read(LOG_FILE), "run 2\n");
        assert_eq!(read(PREV_LOG_FILE), "run 1\n");

        writeln!(open_log(&dir).unwrap(), "run 3").unwrap();
        assert_eq!(read(LOG_FILE), "run 3\n");
        assert_eq!(read(PREV_LOG_FILE), "run 2\n");
    }
}
