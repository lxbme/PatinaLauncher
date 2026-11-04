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
    ExtractNatives,
    PrepareGameDirectory,
    DownloadContent,
    DownloadPackArchive,
    InspectPackArchive,
    VerifyPackFiles,
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
            Phase::DownloadGameFiles => t!("progress.phase.prepare_game_files"),
            Phase::ExtractNatives => t!("progress.phase.extract_natives"),
            Phase::PrepareGameDirectory => t!("progress.phase.prepare_game_directory"),
            Phase::DownloadContent => t!("progress.phase.download_content"),
            Phase::DownloadPackArchive => t!("progress.phase.download_pack_archive"),
            Phase::InspectPackArchive => t!("progress.phase.inspect_pack_archive"),
            Phase::VerifyPackFiles => t!("progress.phase.verify_pack_files"),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskProgress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
    pub unit: Unit,
    pub activity: Option<std::sync::Arc<crate::download::TransferSnapshot>>,
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
            activity: None,
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
            activity: None,
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
    run_downloads_inner(
        dl,
        jobs.into_iter().map(|job| (job, Vec::new())).collect(),
        sink,
        phase,
        unit,
        extra_items,
        None,
    )
    .await
}

/// Same shared scheduler/progress path, with author candidates and hard actual-byte bounds.
pub(crate) async fn run_bounded_downloads(
    dl: &Downloader,
    jobs: Vec<(DownloadJob, Vec<String>)>,
    sink: &ProgressSink,
    phase: Phase,
    limits: (u64, u64),
) -> Result<(), DownloadError> {
    run_downloads_inner(dl, jobs, sink, phase, Unit::Bytes, 0, Some(limits)).await
}

async fn run_downloads_inner(
    dl: &Downloader,
    jobs: Vec<(DownloadJob, Vec<String>)>,
    sink: &ProgressSink,
    phase: Phase,
    unit: Unit,
    extra_items: u64,
    limits: Option<(u64, u64)>,
) -> Result<(), DownloadError> {
    let Some(out) = sink else {
        return match limits {
            Some(limits) => dl.run_bounded_candidates(jobs, None, limits).await,
            None => {
                dl.run(jobs.into_iter().map(|(job, _)| job).collect(), None)
                    .await
            }
        };
    };
    let initial_total = match unit {
        Unit::Items => jobs.len() as u64 + extra_items,
        Unit::Bytes => {
            let mut seen = std::collections::HashSet::new();
            jobs.iter()
                .map(|(job, _)| job)
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
    let (activity_tx, mut activity_rx) = tokio::sync::watch::channel(std::sync::Arc::new(
        crate::download::TransferSnapshot::default(),
    ));
    let run = dl.run_observed(jobs, Some(tx), limits, activity_tx);
    let mut last_sent = None;
    let mut send = |t: TaskProgress| {
        if last_sent.as_ref() != Some(&t) {
            let _ = out.send(t.clone());
            last_sent = Some(t);
        }
    };
    let forward = async {
        let mut summary_open = true;
        let mut activity_open = true;
        while summary_open || activity_open {
            tokio::select! {
                p=rx.recv(), if summary_open => {
                    if let Some(p)=p {
                        if let Some(mut t)=fwd.tick(p) { t.activity=Some(activity_rx.borrow().clone()); send(t); }
                    } else { summary_open=false; }
                }
                change=activity_rx.changed(), if activity_open => {
                    if change.is_err(){activity_open=false;} else {
                        send(TaskProgress{phase,done:fwd.last.0,total:fwd.last.1,unit,activity:Some(activity_rx.borrow_and_update().clone())});
                    }
                }
            }
        }
    };
    let (res, ()) = tokio::join!(run, forward);
    // A fresh empty snapshot also clears any last coalesced row on success/failure.
    send(TaskProgress {
        phase,
        done: fwd.last.0,
        total: fwd.last.1,
        unit,
        activity: Some(activity_rx.borrow().clone()),
    });
    if res.is_ok()
        && let Some(mut t) = fwd.finish()
    {
        t.activity = Some(activity_rx.borrow().clone());
        send(t);
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
        assert_eq!(Phase::DownloadGameFiles.display_name(), "正在准备游戏文件");
        assert_eq!(Phase::DownloadContent.display_name(), "正在下载内容");
        set_language(Language::En);
        assert_eq!(
            Phase::DownloadGameFiles.display_name(),
            "Preparing game files"
        );
        assert_eq!(Phase::FetchMetadata.display_name(), "Fetching version info");
        assert_eq!(Phase::Launching.display_name(), "Launching game");
        assert_eq!(Phase::DownloadContent.display_name(), "Downloading content");
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

        #[tokio::test]
        async fn file_activity_keeps_parallel_identity_and_excludes_cached_bytes_from_network() {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let mut jobs = Vec::new();
            for name in ["alpha", "beta"] {
                Mock::given(path(format!("/{name}")))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_bytes(b"hello")
                            .set_delay(Duration::from_millis(200)),
                    )
                    .mount(&server)
                    .await;
                jobs.push(DownloadJob {
                    url: format!("{}/{name}", server.uri()),
                    dest: dir.path().join(name),
                    sha1: Some(hex::encode(Sha1::digest(b"hello"))),
                    size: None,
                });
            }
            let dl =
                Downloader::new(reqwest::Client::new(), Mirror::official()).with_concurrency(2);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            run_downloads(
                &dl,
                jobs.clone(),
                &Some(tx),
                Phase::DownloadContent,
                Unit::Items,
                0,
            )
            .await
            .unwrap();
            let ticks = collect(rx);
            let snapshots: Vec<_> = ticks.iter().filter_map(|p| p.activity.as_deref()).collect();
            assert!(
                snapshots.iter().any(|s| s.files.len() == 2),
                "both parallel files need independent activity"
            );
            let parallel = snapshots.iter().find(|s| s.files.len() == 2).unwrap();
            assert_ne!(parallel.files[0].id, parallel.files[1].id);
            assert!(
                snapshots
                    .iter()
                    .any(|s| s.files.iter().any(|f| f.total_bytes.is_none()))
            );
            let last = snapshots.last().unwrap();
            assert!(last.files.is_empty(), "completed rows must be removed");
            assert_eq!(last.network_bytes, 10);
            assert_eq!(last.bytes_per_second, 0);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            run_downloads(&dl, jobs, &Some(tx), Phase::DownloadContent, Unit::Items, 0)
                .await
                .unwrap();
            let cached = collect(rx);
            assert!(
                cached
                    .iter()
                    .filter_map(|p| p.activity.as_ref())
                    .all(|s| s.network_bytes == 0 && s.bytes_per_second == 0)
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }

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
            let last = collect(rx).last().unwrap().clone();
            assert!(last.done > 0 && last.done == last.total, "{last:?}");
        }
    }
}

#[cfg(test)]
mod activity_lifecycle_tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
    #[tokio::test]
    async fn aborting_observed_run_removes_all_active_rows() {
        let server = MockServer::start().await;
        Mock::given(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"slow")
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let job = DownloadJob {
            url: format!("{}/slow", server.uri()),
            dest: dir.path().join("slow"),
            sha1: None,
            size: None,
        };
        let (tx, mut rx) =
            tokio::sync::watch::channel(Arc::new(crate::download::TransferSnapshot::default()));
        let run = tokio::spawn(async move {
            Downloader::new(reqwest::Client::new(), crate::download::Mirror::official())
                .run_observed(vec![(job, vec![])], None, None, tx)
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while rx.borrow().files.is_empty() {
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        run.abort();
        assert!(run.await.unwrap_err().is_cancelled());
        assert!(
            rx.borrow().files.is_empty(),
            "aborted run cannot leave a ghost row"
        );
        assert_eq!(rx.borrow().bytes_per_second, 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[cfg(test)]
mod observed_retry_tests {
    use super::*;
    use crate::download::{FileActivityState, Mirror};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
    #[tokio::test]
    async fn retry_snapshot_keeps_one_file_and_never_counts_cache_as_network() {
        let server = MockServer::start().await;
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        Mock::given(path("/retry"))
            .respond_with(move |_: &wiremock::Request| {
                if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(503)
                } else {
                    ResponseTemplate::new(200).set_body_bytes(b"hello")
                }
            })
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let dl = Downloader::new(reqwest::Client::new(), Mirror::official())
            .with_retry(2, Duration::from_millis(150));
        run_downloads(
            &dl,
            vec![DownloadJob {
                url: format!("{}/retry", server.uri()),
                dest: dir.path().join("retry"),
                sha1: None,
                size: None,
            }],
            &Some(tx),
            Phase::DownloadContent,
            Unit::Bytes,
            0,
        )
        .await
        .unwrap();
        let mut retry = false;
        let mut ids = std::collections::HashSet::new();
        let mut last = None;
        while let Ok(p) = rx.try_recv() {
            if let Some(snapshot) = p.activity {
                for file in &snapshot.files {
                    ids.insert(file.id.clone());
                    if file.state == FileActivityState::Retrying {
                        retry = true;
                        assert_eq!(file.done_bytes, 0);
                    }
                }
                last = Some(snapshot);
            }
        }
        assert!(retry);
        assert_eq!(ids.len(), 1);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(last.as_ref().unwrap().network_bytes, 5);
        assert!(last.unwrap().files.is_empty());
    }
}
