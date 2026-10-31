//! Unified, phased progress for `prepare`, the managed Java runtime install
//! and every loader install. A UI shows [`Phase::display_name`] as the step
//! description and `done / total` as the bar.

use tokio::sync::mpsc::UnboundedSender;

use crate::download::engine::dest_key;
use crate::download::{DownloadError, DownloadJob, Downloader, Progress};

/// A step of a long-running operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    FetchMetadata,
    DownloadRuntime,
    DownloadAssetIndex,
    /// Verifying the game's files (client jar, assets) and downloading only
    /// the missing or damaged ones; on an installed game, mostly checking.
    DownloadGameFiles,
    DownloadInstaller,
    DownloadLibraries,
    RunProcessor,
    Finalize,
    Launching,
}

impl Phase {
    /// The localized step description.
    pub fn display_name(self) -> String {
        use rust_i18n::t;
        match self {
            Phase::FetchMetadata => t!("progress.phase.fetch_metadata"),
            Phase::DownloadRuntime => t!("progress.phase.download_runtime"),
            Phase::DownloadAssetIndex => t!("progress.phase.download_asset_index"),
            Phase::DownloadGameFiles => t!("progress.phase.check_game_files"),
            Phase::DownloadInstaller => t!("progress.phase.download_installer"),
            Phase::DownloadLibraries => t!("progress.phase.download_libraries"),
            Phase::RunProcessor => t!("progress.phase.run_processor"),
            Phase::Finalize => t!("progress.phase.finalize"),
            Phase::Launching => t!("progress.phase.launching"),
        }
        .to_string()
    }
}

/// What `done` and `total` count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Items,
    Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskProgress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
    pub unit: Unit,
}

/// Receives progress; `None` discards it.
pub type ProgressSink = Option<UnboundedSender<TaskProgress>>;

pub(crate) fn report(sink: &ProgressSink, phase: Phase, done: u64, total: u64, unit: Unit) {
    if let Some(tx) = sink {
        let _ = tx.send(TaskProgress {
            phase,
            done,
            total,
            unit,
        });
    }
}

/// Turns Downloader ticks into [`TaskProgress`] values: `done` is clamped to
/// a non-zero `total`, never decreases, and a tick is passed on only when
/// the pair changed.
struct Forwarder {
    phase: Phase,
    unit: Unit,
    extra_items: u64,
    last: (u64, u64),
}

impl Forwarder {
    fn new(phase: Phase, unit: Unit, initial_total: u64, extra_items: u64) -> Self {
        Self {
            phase,
            unit,
            extra_items,
            last: (0, initial_total),
        }
    }

    fn emit(&mut self, done: u64, total: u64) -> Option<TaskProgress> {
        if (done, total) == self.last {
            return None;
        }
        self.last = (done, total);
        Some(TaskProgress {
            phase: self.phase,
            done,
            total,
            unit: self.unit,
        })
    }

    fn tick(&mut self, p: Progress) -> Option<TaskProgress> {
        let (done, total) = match self.unit {
            Unit::Items => (p.done as u64, p.total as u64 + self.extra_items),
            Unit::Bytes => (p.bytes_done, p.bytes_total),
        };
        let done = if total > 0 { done.min(total) } else { done };
        self.emit(done.max(self.last.0), total)
    }

    /// After a successful run with no known byte total, completes the phase.
    fn finish(&mut self) -> Option<TaskProgress> {
        if self.unit == Unit::Bytes && self.last.1 == 0 {
            return self.emit(self.last.0, self.last.0);
        }
        None
    }
}

/// Runs `jobs` and forwards the downloader's progress as `phase`.
///
/// `extra_items` is added to the item total (work the caller does after the
/// download, so the total never jumps). Within the phase `done` never
/// decreases and never exceeds a non-zero `total`; a tick is forwarded only
/// when the reported value changed. The byte total counts each destination
/// once, as the Downloader does.
pub(crate) async fn run_downloads(
    dl: &Downloader,
    jobs: Vec<DownloadJob>,
    sink: &ProgressSink,
    phase: Phase,
    unit: Unit,
    extra_items: u64,
) -> Result<(), DownloadError> {
    let Some(out) = sink else {
        return dl.run(jobs, None).await;
    };
    let initial_total = match unit {
        Unit::Items => jobs.len() as u64 + extra_items,
        Unit::Bytes => {
            let mut seen = std::collections::HashSet::new();
            jobs.iter()
                .filter(|j| match dest_key(&j.dest) {
                    Ok(k) => seen.insert(k),
                    Err(_) => true,
                })
                .filter_map(|j| j.size)
                .sum()
        }
    };
    let mut fwd = Forwarder::new(phase, unit, initial_total, extra_items);
    report(sink, phase, 0, initial_total, unit);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
    let run = dl.run(jobs, Some(tx));
    let forward = async {
        while let Some(p) = rx.recv().await {
            if let Some(t) = fwd.tick(p) {
                let _ = out.send(t);
            }
        }
    };
    let (res, ()) = tokio::join!(run, forward);
    if res.is_ok()
        && let Some(t) = fwd.finish()
    {
        let _ = out.send(t);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Language, locale_lock, set_language};

    #[test]
    fn phase_names_localized() {
        let _g = locale_lock();
        set_language(Language::ZhCn);
        assert_eq!(Phase::FetchMetadata.display_name(), "正在获取版本信息");
        assert_eq!(Phase::DownloadGameFiles.display_name(), "正在检查游戏文件");
        set_language(Language::En);
        assert_eq!(
            Phase::DownloadGameFiles.display_name(),
            "Checking game files"
        );
        assert_eq!(Phase::FetchMetadata.display_name(), "Fetching version info");
        assert_eq!(Phase::Launching.display_name(), "Launching game");
        set_language(Language::ZhCn);
    }

    fn prog(done: usize, total: usize, bytes_done: u64, bytes_total: u64) -> Progress {
        Progress {
            done,
            total,
            bytes_done,
            bytes_total,
        }
    }

    fn fwd(unit: Unit, total: u64, extra: u64) -> Forwarder {
        Forwarder::new(Phase::DownloadRuntime, unit, total, extra)
    }

    #[test]
    fn bytes_done_is_clamped_to_total() {
        let mut f = fwd(Unit::Bytes, 10, 0);
        let t = f.tick(prog(0, 1, 25, 10)).unwrap();
        assert_eq!((t.done, t.total), (10, 10));
    }

    #[test]
    fn done_never_decreases_and_unchanged_ticks_are_dropped() {
        let mut f = fwd(Unit::Bytes, 10, 0);
        assert_eq!(f.tick(prog(0, 1, 6, 10)).unwrap().done, 6);
        // rolled back: nothing to forward, done stays 6
        assert!(f.tick(prog(0, 1, 0, 10)).is_none());
        assert!(f.tick(prog(0, 1, 6, 10)).is_none());
        assert_eq!(f.tick(prog(1, 1, 10, 10)).unwrap().done, 10);
        assert!(f.tick(prog(1, 1, 10, 10)).is_none());
    }

    #[test]
    fn extra_items_join_the_item_total() {
        let mut f = fwd(Unit::Items, 5, 2);
        let t = f.tick(prog(1, 3, 0, 0)).unwrap();
        assert_eq!((t.done, t.total, t.unit), (1, 5, Unit::Items));
    }

    #[test]
    fn unknown_byte_total_completes_on_finish() {
        let mut f = fwd(Unit::Bytes, 0, 0);
        assert_eq!(f.tick(prog(0, 1, 7, 0)).unwrap().done, 7);
        let t = f.finish().unwrap();
        assert_eq!((t.done, t.total), (7, 7));
        assert!(f.finish().is_none());
        assert!(fwd(Unit::Items, 3, 0).finish().is_none());
    }

    mod run {
        use super::*;
        use crate::download::Mirror;
        use sha1::{Digest, Sha1};
        use std::time::Duration;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn serve(server: &MockServer, p: &str, body: &[u8]) {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
                .mount(server)
                .await;
        }

        fn setup(server: &MockServer) -> (Downloader, String) {
            let orig = format!("{}/orig", server.uri());
            let mirror = Mirror {
                id: "m".into(),
                rules: vec![(orig.clone(), format!("{}/mirror", server.uri()))],
            };
            let d = Downloader::new(reqwest::Client::new(), mirror).with_retry(2, Duration::ZERO);
            (d, orig)
        }

        fn collect(
            mut rx: tokio::sync::mpsc::UnboundedReceiver<TaskProgress>,
        ) -> Vec<TaskProgress> {
            let mut v = Vec::new();
            while let Ok(p) = rx.try_recv() {
                v.push(p);
            }
            v
        }

        #[tokio::test]
        async fn rolled_back_attempt_never_lowers_done() {
            let server = MockServer::start().await;
            // The mirror serves corrupt bytes (rolled back after the sha1
            // check), the original serves the right ones.
            serve(&server, "/mirror/a", b"xxxxx").await;
            serve(&server, "/orig/a", b"hello").await;
            let (d, orig) = setup(&server);
            let dir = tempfile::tempdir().unwrap();
            let job = DownloadJob {
                url: format!("{orig}/a"),
                dest: dir.path().join("a"),
                sha1: Some(hex::encode(Sha1::digest(b"hello"))),
                size: Some(5),
            };
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            run_downloads(
                &d,
                vec![job],
                &Some(tx),
                Phase::DownloadRuntime,
                Unit::Bytes,
                0,
            )
            .await
            .unwrap();
            let ev = collect(rx);
            assert!(ev.windows(2).all(|w| w[0].done <= w[1].done), "{ev:?}");
            assert!(ev.windows(2).all(|w| w[0] != w[1]), "{ev:?}");
            assert!(ev.iter().all(|p| p.total == 5 && p.done <= 5), "{ev:?}");
            let last = ev.last().unwrap();
            assert_eq!((last.done, last.total), (5, 5));
        }

        #[tokio::test]
        async fn duplicate_destinations_count_once_in_byte_total() {
            let server = MockServer::start().await;
            serve(&server, "/mirror/a", b"hello").await;
            let (d, orig) = setup(&server);
            let dir = tempfile::tempdir().unwrap();
            let job = DownloadJob {
                url: format!("{orig}/a"),
                dest: dir.path().join("a"),
                sha1: None,
                size: Some(5),
            };
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            run_downloads(
                &d,
                vec![job.clone(), job],
                &Some(tx),
                Phase::DownloadRuntime,
                Unit::Bytes,
                0,
            )
            .await
            .unwrap();
            let ev = collect(rx);
            assert!(ev.iter().all(|p| p.total == 5), "{ev:?}");
            assert_eq!(ev.last().map(|p| p.done), Some(5));
        }

        #[tokio::test]
        async fn unknown_sizes_still_complete() {
            let server = MockServer::start().await;
            serve(&server, "/mirror/a", b"hello").await;
            let (d, orig) = setup(&server);
            let dir = tempfile::tempdir().unwrap();
            let job = DownloadJob {
                url: format!("{orig}/a"),
                dest: dir.path().join("a"),
                sha1: None,
                size: None,
            };
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            run_downloads(
                &d,
                vec![job],
                &Some(tx),
                Phase::DownloadRuntime,
                Unit::Bytes,
                0,
            )
            .await
            .unwrap();
            let last = *collect(rx).last().unwrap();
            assert!(last.done > 0 && last.done == last.total, "{last:?}");
        }
    }
}
