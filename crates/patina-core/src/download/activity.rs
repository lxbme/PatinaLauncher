//! Bounded current-file snapshots for one downloader run; completed files are not history.
use super::engine::{DownloadJob, lock};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileActivityState {
    Checking,
    Connecting,
    Downloading,
    Retrying,
}
impl FileActivityState {
    pub fn display_name(self) -> String {
        rust_i18n::t!(match self {
            Self::Checking => "progress.file.checking",
            Self::Connecting => "progress.file.connecting",
            Self::Downloading => "progress.file.downloading",
            Self::Retrying => "progress.file.retrying",
        })
        .to_string()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileActivity {
    pub id: String,
    pub name: String,
    pub state: FileActivityState,
    pub done_bytes: u64,
    pub total_bytes: Option<u64>,
}
impl FileActivity {
    pub fn new(id: impl Into<String>, name: impl Into<String>, state: FileActivityState) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            state,
            done_bytes: 0,
            total_bytes: None,
        }
    }
    pub fn with_bytes(mut self, done: u64, total: Option<u64>) -> Self {
        self.done_bytes = done;
        self.total_bytes = total;
        self
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransferSnapshot {
    pub run: u64,
    pub files: Vec<FileActivity>,
    pub network_bytes: u64,
    pub bytes_per_second: u64,
}
impl TransferSnapshot {
    pub fn new(run: u64, files: Vec<FileActivity>) -> Self {
        Self {
            run,
            files,
            ..Self::default()
        }
    }
    pub fn with_network(mut self, bytes: u64, rate: u64) -> Self {
        self.network_bytes = bytes;
        self.bytes_per_second = rate;
        self
    }
}

const MAX_ACTIVE: usize = 64;
const GAP: Duration = Duration::from_millis(100);
static NEXT_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
struct State {
    files: BTreeMap<PathBuf, FileActivity>,
    network: u64,
    samples: std::collections::VecDeque<(Instant, u64)>,
    last: Instant,
}
pub(super) struct ActivityTracker {
    run: u64,
    tx: tokio::sync::watch::Sender<Arc<TransferSnapshot>>,
    state: Mutex<State>,
}
impl ActivityTracker {
    pub(super) fn new(tx: tokio::sync::watch::Sender<Arc<TransferSnapshot>>) -> Self {
        let now = Instant::now();
        Self {
            run: NEXT_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            tx,
            state: Mutex::new(State {
                files: BTreeMap::new(),
                network: 0,
                samples: std::collections::VecDeque::from([(now, 0)]),
                last: now,
            }),
        }
    }
    pub(super) fn file<'a>(&'a self, job: &DownloadJob, key: PathBuf) -> FileGuard<'a> {
        let name = reqwest::Url::parse(&job.url)
            .ok()
            .and_then(|u| {
                u.path_segments()
                    .and_then(|mut s| s.next_back())
                    .map(|segment| {
                        percent_encoding::percent_decode_str(segment)
                            .decode_utf8_lossy()
                            .into_owned()
                    })
            })
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                job.dest
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        // Display text only: keep a single line without changing URL, destination or cache identity.
        let name: String = name
            .chars()
            .map(|c| {
                if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let mut s = lock(&self.state);
        if s.files.len() < MAX_ACTIVE {
            s.files.insert(
                key.clone(),
                FileActivity::new(key.to_string_lossy(), name, FileActivityState::Checking)
                    .with_bytes(0, job.size),
            );
        }
        self.emit(&mut s, true);
        FileGuard { owner: self, key }
    }
    fn emit(&self, s: &mut State, force: bool) {
        let now = Instant::now();
        if !force && now.duration_since(s.last) < GAP {
            return;
        }
        s.last = now;
        while s.samples.len() > 1 && now.duration_since(s.samples[1].0) >= Duration::from_secs(1) {
            s.samples.pop_front();
        }
        let (at, bytes) = s.samples.front().copied().unwrap();
        let rate = if s
            .files
            .values()
            .any(|f| f.state == FileActivityState::Downloading)
        {
            ((s.network - bytes) as f64 / now.duration_since(at).as_secs_f64().max(0.001)) as u64
        } else {
            0
        };
        if s.samples
            .back()
            .is_none_or(|(at, _)| now.duration_since(*at) >= GAP)
        {
            s.samples.push_back((now, s.network));
        }
        self.tx.send_replace(Arc::new(
            TransferSnapshot::new(self.run, s.files.values().cloned().collect())
                .with_network(s.network, rate),
        ));
    }
    pub(super) fn heartbeat(&self) {
        self.emit(&mut lock(&self.state), false);
    }
}
impl Drop for ActivityTracker {
    fn drop(&mut self) {
        let s = lock(&self.state);
        self.tx.send_replace(Arc::new(
            TransferSnapshot::new(self.run, vec![]).with_network(s.network, 0),
        ));
    }
}
pub(super) struct FileGuard<'a> {
    owner: &'a ActivityTracker,
    key: PathBuf,
}
impl FileGuard<'_> {
    pub(super) fn state(&self, state: FileActivityState, total: Option<u64>) {
        let mut s = lock(&self.owner.state);
        if let Some(f) = s.files.get_mut(&self.key) {
            f.state = state;
            f.done_bytes = 0;
            if total.is_some() {
                f.total_bytes = total;
            }
        }
        self.owner.emit(&mut s, true);
    }
    pub(super) fn bytes(&self, done: u64, delta: u64) {
        let mut s = lock(&self.owner.state);
        s.network = s.network.saturating_add(delta);
        if let Some(f) = s.files.get_mut(&self.key) {
            f.done_bytes = done;
        }
        self.owner.emit(&mut s, false);
    }
}
impl Drop for FileGuard<'_> {
    fn drop(&mut self) {
        let mut s = lock(&self.owner.state);
        s.files.remove(&self.key);
        self.owner.emit(&mut s, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn activity_filename_decodes_url_path_once_without_changing_transfer_identity() {
        let cases = [
            (
                "Cobblemon%20Official%20Modpack%20%5BFabric%5D%201.8.4.mrpack",
                "Cobblemon Official Modpack [Fabric] 1.8.4.mrpack",
            ),
            (
                "%E6%95%B4%E5%90%88%E5%8C%85%20%E2%9C%A8.mrpack",
                "整合包 ✨.mrpack",
            ),
            ("plain-file.mrpack", "plain-file.mrpack"),
            (
                "already plain [Fabric].mrpack",
                "already plain [Fabric].mrpack",
            ),
            (
                "literal+plus%2Band%20space.jar",
                "literal+plus+and space.jar",
            ),
            ("100%25%20literal%2520.jar", "100% literal%20.jar"),
            ("bad%ZZ%2-tail.jar", "bad%ZZ%2-tail.jar"),
            ("invalid%FF%E4%B8.jar", "invalid��.jar"),
            (
                "line%0A%0D%09%00%7F%E2%80%A8%E2%80%A9.jar",
                "line_______.jar",
            ),
            ("literal%250A.jar", "literal%0A.jar"),
        ];
        for (segment, expected) in cases {
            let url = format!(
                "https://cdn.modrinth.com/data/project/versions/release/{segment}?unchanged=%2520+%2B"
            );
            assert_activity_name(&url, "hash-addressed-cache", expected);
        }
    }

    #[test]
    fn activity_filename_fallback_preserves_literal_disk_percent_and_plus() {
        for url in [
            "not a URL",
            "https://cdn.modrinth.com/",
            "data:text/plain,opaque",
        ] {
            assert_activity_name(url, "literal%20+disk.jar", "literal%20+disk.jar");
        }
        assert_activity_name("not a URL", "line\n\tdisk.jar", "line__disk.jar");
    }

    fn assert_activity_name(url: &str, basename: &str, expected: &str) {
        let (tx, rx) = tokio::sync::watch::channel(Arc::new(TransferSnapshot::default()));
        let tracker = ActivityTracker::new(tx);
        let dest = PathBuf::from("/isolated/cache").join(basename);
        let job = DownloadJob {
            url: url.into(),
            dest: dest.clone(),
            sha1: Some("untouched author hash".into()),
            size: Some(123),
        };
        let file = tracker.file(&job, dest.clone());
        let snapshot = rx.borrow();
        assert_eq!(snapshot.files[0].name, expected, "display for {url}");
        assert_eq!(snapshot.files[0].id, dest.to_string_lossy());
        assert_eq!(snapshot.files[0].total_bytes, Some(123));
        assert_eq!(snapshot.network_bytes, 0);
        assert_eq!(snapshot.bytes_per_second, 0);
        assert_eq!(job.url, url, "display must not rewrite requests");
        assert_eq!(
            job.dest, dest,
            "display must not rename cache or disk paths"
        );
        assert_eq!(job.sha1.as_deref(), Some("untouched author hash"));
        drop(snapshot);
        drop(file);
    }

    #[test]
    fn retry_keeps_identity_and_only_real_body_bytes_feed_rate() {
        let (tx, rx) = tokio::sync::watch::channel(Arc::new(TransferSnapshot::default()));
        let tracker = ActivityTracker::new(tx);
        let key = PathBuf::from("/isolated/a.jar");
        let file = tracker.file(
            &DownloadJob {
                url: "https://cdn.modrinth.com/a.jar".into(),
                dest: key.clone(),
                sha1: None,
                size: Some(10),
            },
            key,
        );
        let id = rx.borrow().files[0].id.clone();
        file.state(FileActivityState::Downloading, Some(10));
        file.bytes(4, 4);
        file.state(FileActivityState::Retrying, Some(10));
        assert_eq!(rx.borrow().network_bytes, 4);
        assert_eq!(rx.borrow().files[0].done_bytes, 0);
        assert_eq!(rx.borrow().files[0].id, id);
        assert_eq!(rx.borrow().bytes_per_second, 0);
        file.state(FileActivityState::Downloading, Some(10));
        file.bytes(10, 10);
        drop(file);
        drop(tracker);
        assert_eq!(rx.borrow().network_bytes, 14);
        assert!(rx.borrow().files.is_empty());
        assert_eq!(rx.borrow().bytes_per_second, 0);
    }
}
