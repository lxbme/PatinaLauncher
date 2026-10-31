//! The app's own settings, `<data>/gui.toml` (the launcher's settings proper
//! live in the core's config file).
//!
//! Every function takes the directory explicitly so tests can use a temp
//! dir; the app passes the data dir. Writes are atomic (unique temp file +
//! rename) and happen on a background thread ([`Saver`]), debounced, and
//! flushed when the app quits. A file that cannot be read or parsed is
//! renamed to `gui.toml.bak` and defaults are used, so a broken file never
//! blocks startup and is never silently overwritten.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;
use std::{fs, io};

use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize, Serializer};

use crate::i18n::{Language, system_language};
use crate::scale::{DEFAULT_SCALE, snap_scale};

pub const SETTINGS_FILE: &str = "gui.toml";
/// Quiet period after the last change before a [`Saver`] writes.
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// Longest [`Saver::flush`] waits for the write.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// What the launcher window does once a game has started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterLaunch {
    #[default]
    Keep,
    Minimize,
    Close,
}

/// Fields are public to read and edit (through `SettingsStore::update`);
/// build one from `AppSettings::default()`, as fields may be added.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct AppSettings {
    /// One of `scale::UI_SCALES` (snapped on load).
    #[serde(serialize_with = "two_decimals")]
    pub ui_scale: f32,
    /// A missing value (first run) follows `$LANG`.
    pub language: Language,
    pub after_launch: AfterLaunch,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            ui_scale: DEFAULT_SCALE,
            language: system_language(),
            after_launch: AfterLaunch::default(),
        }
    }
}

/// Writes an `f32` step such as 1.1 as `1.1` rather than its widened
/// `1.100000023841858`.
fn two_decimals<S: Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((f64::from(*v) * 100.0).round() / 100.0)
}

/// Reads `dir/gui.toml`: `Ok(None)` when it does not exist, `Err` (a message
/// without file contents) when it cannot be read or parsed.
fn read(dir: &Path) -> Result<Option<AppSettings>, String> {
    let text = match fs::read_to_string(dir.join(SETTINGS_FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let mut s: AppSettings = toml::from_str(&text).map_err(|e| e.message().to_owned())?;
    s.ui_scale = snap_scale(s.ui_scale);
    Ok(Some(s))
}

/// The settings in `dir`. Missing → defaults; unreadable or unparsable →
/// backed up to the first free `gui.toml.bak[.N]`, then removed; defaults.
pub fn load(dir: &Path) -> AppSettings {
    let err = match read(dir) {
        Ok(s) => return s.unwrap_or_default(),
        Err(err) => err,
    };
    let file = dir.join(SETTINGS_FILE);
    match crate::core_bridge::back_up(&file) {
        Ok(Some(bak)) => {
            if let Err(e) = fs::remove_file(&file) {
                tracing::warn!(%e, "removing backed-up invalid settings failed");
            }
            tracing::warn!(file = %file.display(), %err, bak = %bak.display(), "invalid settings backed up; using defaults");
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(file = %file.display(), %err, backup_error = %e, "invalid settings; using defaults")
        }
    }
    AppSettings::default()
}

/// The settings in `dir` without touching the file (defaults when it is
/// missing or broken). For a process that must not change the data dir,
/// such as a second instance that only reports the first one.
pub fn peek(dir: &Path) -> AppSettings {
    read(dir).ok().flatten().unwrap_or_default()
}

/// Writes `dir/gui.toml` atomically (creating `dir`).
pub fn save(dir: &Path, s: &AppSettings) -> io::Result<()> {
    let text = toml::to_string(s).map_err(io::Error::other)?;
    patina_core::fsutil::write_atomic(&dir.join(SETTINGS_FILE), text.as_bytes())
}

enum SaveMsg<T> {
    Save(Box<T>),
    Flush(mpsc::Sender<()>),
}

/// Debounced background writer: [`Saver::schedule`] hands over the latest
/// value, which is written [`SAVE_DEBOUNCE`] after the last call on a
/// dedicated thread (never the UI thread). Dropping the saver writes
/// anything pending.
pub struct Saver<T: Send + 'static> {
    tx: Option<mpsc::Sender<SaveMsg<T>>>,
    thread: Option<JoinHandle<()>>,
}

/// Writes `gui.toml`.
pub type SettingsSaver = Saver<AppSettings>;

impl SettingsSaver {
    pub fn new(dir: PathBuf) -> Self {
        Saver::spawn("patina-settings", move |s| match save(&dir, s) {
            Ok(()) => tracing::debug!(dir = %dir.display(), "settings saved"),
            Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "failed to save settings"),
        })
    }
}

impl<T: Send + 'static> Saver<T> {
    fn spawn(name: &str, write: impl Fn(&T) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || saver_loop(&rx, write))
            .expect("failed to spawn the saver thread");
        Self {
            tx: Some(tx),
            thread: Some(thread),
        }
    }

    pub fn schedule(&self, v: T) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(SaveMsg::Save(Box::new(v)));
        }
    }

    /// Writes any pending value now and waits (bounded) for the write.
    pub fn flush(&self) {
        let Some(tx) = &self.tx else { return };
        let (ack_tx, ack_rx) = mpsc::channel();
        if tx.send(SaveMsg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(FLUSH_TIMEOUT);
        }
    }
}

impl<T: Send + 'static> Drop for Saver<T> {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn saver_loop<T>(rx: &mpsc::Receiver<SaveMsg<T>>, write: impl Fn(&T)) {
    let mut pending: Option<Box<T>> = None;
    loop {
        let msg = if pending.is_some() {
            match rx.recv_timeout(SAVE_DEBOUNCE) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(v) = pending.take() {
                        write(&v);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            }
        };
        match msg {
            SaveMsg::Save(v) => pending = Some(v),
            SaveMsg::Flush(ack) => {
                if let Some(v) = pending.take() {
                    write(&v);
                }
                let _ = ack.send(());
            }
        }
    }
    if let Some(v) = pending.take() {
        write(&v);
    }
}

/// The settings of this process (a gpui `Global`): the current value, and
/// the [`SettingsSaver`] that persists every change to it.
pub struct SettingsStore {
    current: AppSettings,
    saver: SettingsSaver,
}

impl Global for SettingsStore {}

impl SettingsStore {
    /// `current` as loaded from `dir`; changes are saved back there.
    pub fn new(dir: PathBuf, current: AppSettings) -> Self {
        Self {
            current,
            saver: SettingsSaver::new(dir),
        }
    }

    pub fn get(cx: &App) -> &AppSettings {
        &cx.global::<Self>().current
    }

    /// Edits the settings and schedules a save when `edit` changed them.
    pub fn update(cx: &mut App, edit: impl FnOnce(&mut AppSettings)) {
        let store = cx.global_mut::<Self>();
        let before = store.current.clone();
        edit(&mut store.current);
        if store.current != before {
            store.saver.schedule(store.current.clone());
        }
    }

    /// Writes a pending change now (on quit).
    pub fn flush(cx: &App) {
        cx.global::<Self>().saver.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom() -> AppSettings {
        AppSettings {
            ui_scale: 1.1,
            language: Language::En,
            after_launch: AfterLaunch::Minimize,
        }
    }

    #[test]
    fn settings_round_trip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(SETTINGS_FILE);

        // Missing → defaults; nothing is written by a load.
        assert_eq!(load(dir.path()), AppSettings::default());
        assert!(!file.exists());
        assert_eq!(AppSettings::default().ui_scale, 1.0);
        assert_eq!(AppSettings::default().after_launch, AfterLaunch::Keep);
        assert_eq!(AppSettings::default().language, system_language());

        // Round trip, with the scale written as the step itself.
        save(dir.path(), &custom()).unwrap();
        assert_eq!(load(dir.path()), custom());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("ui_scale = 1.1\n"), "{text}");
        assert!(text.contains("language = \"en\""), "{text}");
        assert!(text.contains("after_launch = \"minimize\""), "{text}");

        // Missing fields take their defaults.
        std::fs::write(&file, "language = \"zh-CN\"\n").unwrap();
        let partial = load(dir.path());
        assert_eq!(partial.language, Language::ZhCn);
        assert_eq!(partial.ui_scale, 1.0);
        assert_eq!(partial.after_launch, AfterLaunch::Keep);

        // An off-step scale snaps.
        std::fs::write(&file, "ui_scale = 9.0\n").unwrap();
        assert_eq!(load(dir.path()).ui_scale, 2.0);

        // A broken file is moved to `.bak` (not overwritten) and defaults
        // are used; `peek` leaves a broken file where it is.
        std::fs::write(&file, "{{ not toml ]]").unwrap();
        assert_eq!(peek(dir.path()), AppSettings::default());
        assert!(file.exists());
        assert_eq!(load(dir.path()), AppSettings::default());
        let bak = dir.path().join("gui.toml.bak");
        assert_eq!(std::fs::read_to_string(bak).unwrap(), "{{ not toml ]]");
        assert!(!file.exists());

        // A value of the wrong type is broken too.
        std::fs::write(&file, "after_launch = \"explode\"\n").unwrap();
        assert_eq!(load(dir.path()), AppSettings::default());
        assert!(!file.exists());
    }

    fn with_scale(ui_scale: f32) -> AppSettings {
        AppSettings {
            ui_scale,
            ..custom()
        }
    }

    #[test]
    fn saver_debounces_and_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let saver = SettingsSaver::new(dir.path().to_path_buf());
        saver.schedule(with_scale(1.1));
        saver.schedule(with_scale(1.5));
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !dir.path().join(SETTINGS_FILE).exists(),
            "saved before the debounce"
        );
        // Written once the debounce has passed (polled: a loaded machine
        // can run the saver thread late).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !dir.path().join(SETTINGS_FILE).exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(load(dir.path()), with_scale(1.5));

        saver.schedule(with_scale(2.0));
        saver.flush();
        assert_eq!(load(dir.path()), with_scale(2.0));

        saver.schedule(with_scale(0.8));
        drop(saver);
        assert_eq!(
            load(dir.path()),
            with_scale(0.8),
            "drop writes the pending save"
        );
    }
    #[test]
    fn repeated_corruption_preserves_both_backups() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(SETTINGS_FILE);
        for text in ["first [broken", "second [broken"] {
            std::fs::write(&file, text).unwrap();
            load(dir.path());
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("gui.toml.bak")).unwrap(),
            "first [broken"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("gui.toml.bak.1")).unwrap(),
            "second [broken"
        );
    }
}
