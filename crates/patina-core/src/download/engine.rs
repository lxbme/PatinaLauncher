use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use futures::{StreamExt, TryStreamExt};
use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;

use super::Mirror;
use crate::fsutil::unique_part_path;

#[derive(Clone, Debug)]
pub struct DownloadJob {
    pub url: String,
    pub dest: PathBuf,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    /// Bytes received so far. A finished job counts as its known `size`;
    /// a job without one counts the bytes actually downloaded.
    pub bytes_done: u64,
    /// The sum of every known job `size`.
    pub bytes_total: u64,
}

/// An attempt fails as [`FailReason::TooSlow`] when, after the first
/// [`THROUGHPUT_WINDOW`], it received fewer than this many bytes per second
/// over the last window.
pub const MIN_THROUGHPUT_BYTES_PER_SEC: u64 = 4096;
/// See [`MIN_THROUGHPUT_BYTES_PER_SEC`].
pub const THROUGHPUT_WINDOW: Duration = Duration::from_secs(30);

/// How many [`FallbackEvent`]s a [`Downloader`] keeps (the latest ones).
const MAX_FALLBACK_EVENTS: usize = 256;
/// The throughput window is re-checked this many times per window length.
const WINDOW_SLICES: u32 = 4;
/// Minimum gap between mid-transfer progress messages of one run.
const BYTE_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// A fetch that failed on the primary candidate and succeeded on `used`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FallbackEvent {
    pub original: String,
    pub used: String,
}

/// Why a download failed: the most informative reason over all attempts
/// (see [`more_informative`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    /// Non-success HTTP status.
    Http(u16),
    /// The body's SHA-1 differs from the metadata.
    HashMismatch { expected: String, actual: String },
    /// The body's length differs from the metadata (no SHA-1 known).
    SizeMismatch { expected: u64, actual: u64 },
    /// The body arrived slower than [`MIN_THROUGHPUT_BYTES_PER_SEC`].
    TooSlow,
    /// Request or transfer error; the third-party error text, verbatim.
    Network(String),
    /// Writing the file failed; the OS error text, verbatim.
    Io(String),
    /// No candidate URL was tried.
    NoAttempt,
}

impl std::fmt::Display for FailReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use rust_i18n::t;
        let s = match self {
            FailReason::Http(status) => t!("errors.download.reason.http", status = status),
            FailReason::HashMismatch { expected, actual } => t!(
                "errors.download.reason.hash_mismatch",
                expected = expected,
                actual = actual
            ),
            FailReason::SizeMismatch { expected, actual } => t!(
                "errors.download.reason.size_mismatch",
                expected = expected,
                actual = actual
            ),
            FailReason::TooSlow => t!("errors.download.reason.too_slow"),
            FailReason::Network(e) => t!("errors.download.reason.network", error = e),
            FailReason::Io(e) => t!("errors.download.reason.io", error = e),
            FailReason::NoAttempt => t!("errors.download.reason.no_attempt"),
        };
        f.write_str(&s)
    }
}

/// How much a reason tells about a failure: `HashMismatch > SizeMismatch >
/// Http(4xx) > Http(5xx) > TooSlow > Network > Io > NoAttempt`. Other HTTP
/// statuses rank with 5xx.
fn rank(r: &FailReason) -> u8 {
    match r {
        FailReason::HashMismatch { .. } => 7,
        FailReason::SizeMismatch { .. } => 6,
        FailReason::Http(s) if (400..500).contains(s) => 5,
        FailReason::Http(_) => 4,
        FailReason::TooSlow => 3,
        FailReason::Network(_) => 2,
        FailReason::Io(_) => 1,
        FailReason::NoAttempt => 0,
    }
}

/// Whether `new` should replace `old` as the reported reason; on a tie the
/// first one seen is kept.
fn more_informative(new: &FailReason, old: &FailReason) -> bool {
    rank(new) > rank(old)
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("{}", rust_i18n::t!("errors.download.failed", url = .url, reason = .reason))]
    Failed { url: String, reason: FailReason },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub struct Downloader {
    client: reqwest::Client,
    mirror: Mirror,
    fallback: Option<Mirror>,
    concurrency: usize,
    attempts: u32,
    base_delay: Duration,
    fallbacks: AtomicUsize,
    fallback_events: Mutex<VecDeque<FallbackEvent>>,
    throughput_window: Duration,
}

/// SHA-1 of a file as lowercase hex.
pub fn sha1_file(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha1::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One attempt's part file next to its destination (a fresh
/// [`unique_part_path`] per attempt, so a late write from a failed attempt
/// never lands in a later attempt's file). It is removed when dropped
/// (failure or cancellation) unless it was renamed into place.
struct PartFile {
    path: PathBuf,
    renamed: bool,
}

impl PartFile {
    fn new(dest: &Path) -> Self {
        Self {
            path: unique_part_path(dest),
            renamed: false,
        }
    }
}

/// A successful fetch: the body in memory (empty when written to `part`),
/// its length, and the part file holding it when fetched for a destination.
struct Fetched {
    bytes: Vec<u8>,
    len: u64,
    part: Option<PartFile>,
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.renamed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The destination-lock key: `dest` made absolute and lexically normalized
/// (`.` dropped, `..` pops), so spellings of one path share one lock.
fn dest_key(dest: &Path) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in std::path::absolute(dest)?.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Process-wide locks, one per destination being written, shared by every
/// [`Downloader`] so two runs never write one file at once. Entries are weak
/// and dropped once no job holds or waits for them.
static DEST_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

async fn lock_dest(key: PathBuf) -> tokio::sync::OwnedMutexGuard<()> {
    let m = {
        let mut map = lock(&DEST_LOCKS);
        match map.get(&key).and_then(Weak::upgrade) {
            Some(m) => m,
            None => {
                map.retain(|_, w| w.strong_count() > 0);
                let m = Arc::new(tokio::sync::Mutex::new(()));
                map.insert(key, Arc::downgrade(&m));
                m
            }
        }
    };
    m.lock_owned().await
}

fn signed(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The progress of one [`Downloader::run`].
struct RunProgress {
    tx: tokio::sync::mpsc::UnboundedSender<Progress>,
    state: Mutex<(Progress, Option<Instant>)>,
}

impl RunProgress {
    fn new(
        tx: tokio::sync::mpsc::UnboundedSender<Progress>,
        total: usize,
        bytes_total: u64,
    ) -> Self {
        let p = Progress {
            done: 0,
            total,
            bytes_done: 0,
            bytes_total,
        };
        Self {
            tx,
            state: Mutex::new((p, None)),
        }
    }

    /// Adds `delta` bytes received (negative: rolled back after a failed
    /// attempt); sends at most every [`BYTE_PROGRESS_INTERVAL`].
    fn bytes(&self, delta: i64) {
        let mut s = lock(&self.state);
        s.0.bytes_done = s.0.bytes_done.saturating_add_signed(delta);
        if s.1.is_none_or(|t| t.elapsed() >= BYTE_PROGRESS_INTERVAL) {
            s.1 = Some(Instant::now());
            let _ = self.tx.send(s.0);
        }
    }

    /// Credits `files` finished files and `delta` bytes; always sends.
    fn finish(&self, files: usize, delta: i64) {
        let mut s = lock(&self.state);
        s.0.done += files;
        s.0.bytes_done = s.0.bytes_done.saturating_add_signed(delta);
        s.1 = Some(Instant::now());
        let _ = self.tx.send(s.0);
    }
}

/// The rolling body-rate check of one attempt: every `window /
/// WINDOW_SLICES` the bytes received over the last full window are compared
/// with [`MIN_THROUGHPUT_BYTES_PER_SEC`]. Its clock starts when the response
/// headers arrived, so connecting is bounded only by the client's timeouts.
struct Throughput {
    /// `None` when the window is zero (check disabled).
    ticker: Option<tokio::time::Interval>,
    /// Bytes required per window.
    min_bytes: u64,
    /// Bytes received at each slice boundary, oldest first.
    samples: VecDeque<u64>,
}

impl Throughput {
    fn new(window: Duration) -> Self {
        let slice = window / WINDOW_SLICES;
        let ticker = (!slice.is_zero()).then(|| {
            let mut t = tokio::time::interval_at(tokio::time::Instant::now() + slice, slice);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t
        });
        let min_bytes = u128::from(MIN_THROUGHPUT_BYTES_PER_SEC) * window.as_millis() / 1000;
        Self {
            ticker,
            min_bytes: u64::try_from(min_bytes).unwrap_or(u64::MAX),
            samples: VecDeque::from([0]),
        }
    }

    /// Waits for the next slice boundary; never resolves when disabled.
    async fn tick(&mut self) {
        match &mut self.ticker {
            Some(t) => {
                t.tick().await;
            }
            None => std::future::pending().await,
        }
    }

    /// Records `received` at a slice boundary; true once a full window has
    /// passed and the last window brought fewer than the required bytes.
    fn too_slow(&mut self, received: u64) -> bool {
        self.samples.push_back(received);
        let full = WINDOW_SLICES as usize + 1;
        if self.samples.len() > full {
            self.samples.pop_front();
        }
        self.samples.len() == full && received - self.samples[0] < self.min_bytes
    }
}

impl Downloader {
    pub fn new(client: reqwest::Client, mirror: Mirror) -> Self {
        Self {
            client,
            mirror,
            fallback: None,
            concurrency: 16,
            attempts: 3,
            base_delay: Duration::from_millis(300),
            fallbacks: AtomicUsize::new(0),
            fallback_events: Mutex::new(VecDeque::new()),
            throughput_window: THROUGHPUT_WINDOW,
        }
    }

    pub fn with_concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    pub fn with_retry(mut self, attempts: u32, base_delay: Duration) -> Self {
        self.attempts = attempts.max(1);
        self.base_delay = base_delay;
        self
    }

    /// A mirror tried after the primary mirror and the original URL.
    pub fn with_fallback(mut self, fallback: Option<Mirror>) -> Self {
        self.fallback = fallback;
        self
    }

    /// The window of the minimum-throughput check (default
    /// [`THROUGHPUT_WINDOW`]); `Duration::ZERO` disables the check.
    pub fn with_throughput_window(mut self, window: Duration) -> Self {
        self.throughput_window = window;
        self
    }

    pub fn mirror(&self) -> &Mirror {
        &self.mirror
    }

    pub fn fallback_mirror(&self) -> Option<&Mirror> {
        self.fallback.as_ref()
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Fetches that failed on the primary mirror and succeeded on a later
    /// candidate (the official URL or the fallback mirror).
    pub fn fallback_count(&self) -> usize {
        self.fallbacks.load(Ordering::Relaxed)
    }

    /// The latest fallbacks (at most 256), oldest first; see
    /// [`Self::fallback_count`].
    pub fn fallback_events(&self) -> Vec<FallbackEvent> {
        lock(&self.fallback_events).iter().cloned().collect()
    }

    fn record_fallback(&self, original: &str, used: &str) {
        tracing::info!(original = %original, used = %used, "download fell back");
        self.fallbacks.fetch_add(1, Ordering::Relaxed);
        let mut events = lock(&self.fallback_events);
        if events.len() == MAX_FALLBACK_EVENTS {
            events.pop_front();
        }
        events.push_back(FallbackEvent {
            original: original.to_string(),
            used: used.to_string(),
        });
    }

    pub async fn run(
        &self,
        jobs: Vec<DownloadJob>,
        progress: Option<tokio::sync::mpsc::UnboundedSender<Progress>>,
    ) -> Result<(), DownloadError> {
        let total = jobs.len();
        if total == 0 {
            return Ok(());
        }
        // Jobs sharing a destination would wait on each other's lock, so keep
        // the first job per dest (later duplicates, even with a differing
        // sha1, are dropped) and credit each duplicate when its
        // representative finishes.
        let mut index: HashMap<PathBuf, usize> = HashMap::new();
        let mut unique: Vec<(DownloadJob, PathBuf, usize)> = Vec::new();
        for job in jobs {
            let key = dest_key(&job.dest)?;
            match index.get(&key) {
                Some(&i) => unique[i].2 += 1,
                None => {
                    index.insert(key.clone(), unique.len());
                    unique.push((job, key, 1));
                }
            }
        }
        let bytes_total = unique.iter().filter_map(|(j, ..)| j.size).sum();
        let tracker = progress.map(|tx| RunProgress::new(tx, total, bytes_total));
        let tracker = tracker.as_ref();
        let mut results = futures::stream::iter(unique)
            .map(|(job, key, weight)| async move {
                let credit = self.run_job(&job, key, tracker).await?;
                Ok::<_, DownloadError>((weight, credit))
            })
            .buffer_unordered(self.concurrency);
        // Returning early on the first error drops the stream and cancels the rest.
        while let Some((weight, credit)) = results.try_next().await? {
            if let Some(t) = tracker {
                t.finish(weight, credit);
            }
        }
        Ok(())
    }

    pub async fn fetch_bytes(
        &self,
        url: &str,
        sha1: Option<&str>,
    ) -> Result<Vec<u8>, DownloadError> {
        Ok(self.fetch(url, sha1, None, None, None).await?.bytes)
    }

    /// Makes `job.dest` hold the file, under the destination lock `key` for
    /// the whole job so a waiting job re-checks the finished file. Returns
    /// the bytes left to credit to progress: the job's known size counts in
    /// full, received bytes were credited as they arrived.
    async fn run_job(
        &self,
        job: &DownloadJob,
        key: PathBuf,
        progress: Option<&RunProgress>,
    ) -> Result<i64, DownloadError> {
        let _lock = lock_dest(key).await;
        if Self::existing_ok(job).await {
            return Ok(job.size.map_or(0, signed));
        }
        if let Some(parent) = job.dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let got = self
            .fetch(
                &job.url,
                job.sha1.as_deref(),
                job.size,
                Some(&job.dest),
                progress,
            )
            .await?;
        if let Some(mut part) = got.part {
            tokio::fs::rename(&part.path, &job.dest).await?;
            part.renamed = true;
        }
        Ok(job.size.map_or(0, |s| signed(s) - signed(got.len)))
    }

    /// Whether `job.dest` already holds the file: its SHA-1 matches when
    /// known, else its size when known, else it merely exists.
    pub(crate) async fn existing_ok(job: &DownloadJob) -> bool {
        let dest = job.dest.clone();
        let (sha1, size) = (job.sha1.clone(), job.size);
        tokio::task::spawn_blocking(move || {
            let Ok(meta) = std::fs::metadata(&dest) else {
                return false;
            };
            if !meta.is_file() {
                return false;
            }
            match (sha1, size) {
                (Some(h), _) => sha1_file(&dest).is_ok_and(|a| a.eq_ignore_ascii_case(&h)),
                (None, Some(s)) => meta.len() == s,
                (None, None) => true,
            }
        })
        .await
        .unwrap_or(false)
    }

    /// Tries each candidate URL with retries. With `dest` each attempt
    /// writes its own part file next to it (returned on success), else the
    /// body is returned in memory. On failure the most informative reason
    /// over all attempts is reported.
    ///
    /// The minimum-throughput cutoff applies to every attempt except the last
    /// attempt of the last candidate: a slow but progressing last resort may
    /// finish (stalls are still bounded by the client's read timeout). So a
    /// `TooSlow` candidate is abandoned, except the last one, which is
    /// retried up to that uncut final attempt.
    async fn fetch(
        &self,
        url: &str,
        sha1: Option<&str>,
        size: Option<u64>,
        dest: Option<&Path>,
        progress: Option<&RunProgress>,
    ) -> Result<Fetched, DownloadError> {
        let mut reason = FailReason::NoAttempt;
        let candidates = self.candidates(url);
        let last = candidates.len().saturating_sub(1);
        for (i, cand) in candidates.iter().enumerate() {
            for n in 1..=self.attempts {
                let cutoff = i < last || n < self.attempts;
                match self.attempt(cand, sha1, size, dest, progress, cutoff).await {
                    Ok(got) => {
                        if i > 0 {
                            self.record_fallback(url, cand);
                        }
                        return Ok(got);
                    }
                    Err(e) => {
                        // Wrong content will not change on retry; a crawling
                        // source is left for the next candidate, if any.
                        let final_for_candidate = match e {
                            FailReason::HashMismatch { .. } => true,
                            FailReason::TooSlow => i < last,
                            _ => false,
                        };
                        if more_informative(&e, &reason) {
                            reason = e;
                        }
                        if final_for_candidate {
                            break;
                        }
                        if n < self.attempts {
                            tokio::time::sleep(self.base_delay * n).await;
                        }
                    }
                }
            }
        }
        Err(DownloadError::Failed {
            url: url.to_string(),
            reason,
        })
    }

    /// `[mirror.rewrite(url), url, fallback.rewrite(url)]`, deduplicated.
    fn candidates(&self, url: &str) -> Vec<String> {
        let mut out = Vec::with_capacity(3);
        let fallback = self.fallback.as_ref().map(|m| m.rewrite(url));
        for c in [
            Some(self.mirror.rewrite(url)),
            Some(url.to_string()),
            fallback,
        ]
        .into_iter()
        .flatten()
        {
            if !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    /// One request to `url`, written to a fresh part file next to `dest`
    /// (removed when the attempt fails) or kept in memory. Bytes it credited
    /// to `progress` are taken back when it fails. `cutoff` enables the
    /// minimum-throughput check.
    async fn attempt(
        &self,
        url: &str,
        sha1: Option<&str>,
        size: Option<u64>,
        dest: Option<&Path>,
        progress: Option<&RunProgress>,
        cutoff: bool,
    ) -> Result<Fetched, FailReason> {
        let part = dest.map(PartFile::new);
        let window = if cutoff {
            self.throughput_window
        } else {
            Duration::ZERO
        };
        let mut received = 0u64;
        let r = self
            .transfer(
                url,
                sha1,
                size,
                part.as_ref().map(|p| p.path.as_path()),
                progress,
                window,
                &mut received,
            )
            .await;
        match r {
            Ok((bytes, len)) => Ok(Fetched { bytes, len, part }),
            Err(e) => {
                if received > 0
                    && let Some(p) = progress
                {
                    p.bytes(-signed(received));
                }
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn transfer(
        &self,
        url: &str,
        sha1: Option<&str>,
        size: Option<u64>,
        part: Option<&Path>,
        progress: Option<&RunProgress>,
        window: Duration,
        received: &mut u64,
    ) -> Result<(Vec<u8>, u64), FailReason> {
        let net = |e: reqwest::Error| FailReason::Network(e.to_string());
        let io = |e: std::io::Error| FailReason::Io(e.to_string());
        let resp = self.client.get(url).send().await.map_err(net)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(FailReason::Http(status.as_u16()));
        }
        let mut file = match part {
            Some(p) => Some(tokio::fs::File::create(p).await.map_err(io)?),
            None => None,
        };
        let mut mem = Vec::new();
        let mut hasher = Sha1::new();
        let mut meter = Throughput::new(window);
        let mut stream = resp.bytes_stream();
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                () = meter.tick() => {
                    if meter.too_slow(*received) {
                        return Err(FailReason::TooSlow);
                    }
                    continue;
                }
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(net)?;
            *received += chunk.len() as u64;
            if let Some(p) = progress {
                p.bytes(signed(chunk.len() as u64));
            }
            hasher.update(&chunk);
            match file.as_mut() {
                Some(f) => f.write_all(&chunk).await.map_err(io)?,
                None => mem.extend_from_slice(&chunk),
            }
        }
        if let Some(mut f) = file {
            f.flush().await.map_err(io)?;
        }
        let len = *received;
        if let Some(expected) = sha1 {
            let actual = hex::encode(hasher.finalize());
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(FailReason::HashMismatch {
                    expected: expected.to_string(),
                    actual,
                });
            }
        } else if let Some(expected) = size
            && len != expected
        {
            return Err(FailReason::SizeMismatch {
                expected,
                actual: len,
            });
        }
        Ok((mem, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn hello_sha1() -> String {
        hex::encode(Sha1::digest(b"hello"))
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

    fn job(orig: &str, dir: &Path) -> DownloadJob {
        DownloadJob {
            url: format!("{orig}/a"),
            dest: dir.join("sub/a.bin"),
            sha1: Some(hello_sha1()),
            size: None,
        }
    }

    async fn serve(server: &MockServer, p: &str, status: u16, body: &[u8]) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(status).set_body_bytes(body.to_vec()))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn downloads_and_verifies() {
        let server = MockServer::start().await;
        serve(&server, "/mirror/a", 200, b"hello").await;
        let (d, orig) = setup(&server);
        let dir = tempfile::tempdir().unwrap();
        let j = job(&orig, dir.path());
        d.run(vec![j.clone()], None).await.unwrap();
        assert_eq!(std::fs::read(&j.dest).unwrap(), b"hello");
        let names: Vec<_> = std::fs::read_dir(j.dest.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[tokio::test]
    async fn skips_existing_valid_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .expect(0)
            .mount(&server)
            .await;
        let (d, orig) = setup(&server);
        let dir = tempfile::tempdir().unwrap();
        let j = job(&orig, dir.path());
        std::fs::create_dir_all(j.dest.parent().unwrap()).unwrap();
        std::fs::write(&j.dest, b"hello").unwrap();
        d.run(vec![j], None).await.unwrap();
    }

    #[tokio::test]
    async fn replaces_corrupted_existing_file() {
        let server = MockServer::start().await;
        serve(&server, "/mirror/a", 200, b"hello").await;
        let (d, orig) = setup(&server);
        let dir = tempfile::tempdir().unwrap();
        let j = job(&orig, dir.path());
        std::fs::create_dir_all(j.dest.parent().unwrap()).unwrap();
        std::fs::write(&j.dest, b"bad").unwrap();
        d.run(vec![j.clone()], None).await.unwrap();
        assert_eq!(std::fs::read(&j.dest).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn mirror_hash_mismatch_falls_back_to_original() {
        let server = MockServer::start().await;
        // A hash mismatch is final for that candidate: no retry on the mirror.
        Mock::given(method("GET"))
            .and(path("/mirror/a"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"stale".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        serve(&server, "/orig/a", 200, b"hello").await;
        let (d, orig) = setup(&server);
        let dir = tempfile::tempdir().unwrap();
        let j = job(&orig, dir.path());
        assert_eq!(d.fallback_count(), 0);
        d.run(vec![j.clone()], None).await.unwrap();
        assert_eq!(std::fs::read(&j.dest).unwrap(), b"hello");
        assert_eq!(d.fallback_count(), 1);
    }

    #[test]
    fn candidates_are_mirror_original_fallback_deduplicated() {
        let url = "https://piston-data.mojang.com/v1/objects/abc/client.jar";
        let bm = "https://bmclapi2.bangbang93.com/v1/objects/abc/client.jar";
        let c = reqwest::Client::new;
        // No fallback mirror configured: only the mirror and the original URL.
        let d = Downloader::new(c(), Mirror::official());
        assert_eq!(d.candidates(url), vec![url.to_string()]);
        let d = Downloader::new(c(), Mirror::official()).with_fallback(Some(Mirror::bmclapi()));
        assert_eq!(d.candidates(url), vec![url.to_string(), bm.to_string()]);
        let d = Downloader::new(c(), Mirror::bmclapi()).with_fallback(Some(Mirror::official()));
        assert_eq!(d.candidates(url), vec![bm.to_string(), url.to_string()]);
        let d = Downloader::new(c(), Mirror::bmclapi());
        assert_eq!(d.candidates(url), vec![bm.to_string(), url.to_string()]);
    }

    #[tokio::test]
    async fn official_failure_falls_back_to_fallback_mirror() {
        let server = MockServer::start().await;
        serve(&server, "/orig/a", 500, b"").await;
        serve(&server, "/bm/a", 200, b"hello").await;
        let orig = format!("{}/orig", server.uri());
        let fallback = Mirror {
            id: "bmclapi".into(),
            rules: vec![(orig.clone(), format!("{}/bm", server.uri()))],
        };
        let d = Downloader::new(reqwest::Client::new(), Mirror::official())
            .with_fallback(Some(fallback))
            .with_retry(2, Duration::ZERO);
        let dir = tempfile::tempdir().unwrap();
        let j = job(&orig, dir.path());
        d.run(vec![j.clone()], None).await.unwrap();
        assert_eq!(std::fs::read(&j.dest).unwrap(), b"hello");
        assert_eq!(d.fallback_count(), 1);
    }

    #[tokio::test]
    async fn downloader_without_fallback_is_strict() {
        let server = MockServer::start().await;
        serve(&server, "/orig/a", 500, b"").await;
        Mock::given(method("GET"))
            .and(path("/bm/a"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .expect(0)
            .mount(&server)
            .await;
        let orig = format!("{}/orig", server.uri());
        let d = Downloader::new(reqwest::Client::new(), Mirror::official())
            .with_retry(2, Duration::ZERO);
        let dir = tempfile::tempdir().unwrap();
        let r = d.run(vec![job(&orig, dir.path())], None).await;
        assert!(
            matches!(
                r,
                Err(DownloadError::Failed {
                    reason: FailReason::Http(500),
                    ..
                })
            ),
            "{r:?}"
        );
    }

    #[test]
    fn hash_mismatch_message_is_fully_localized() {
        use crate::i18n::{Language, locale_lock, set_language};
        let _locale = locale_lock();
        let url = "https://piston-data.mojang.com/v1/objects/abc/client.jar";
        let (expected, actual) = (hello_sha1(), hex::encode(Sha1::digest(b"stale")));
        let e = DownloadError::Failed {
            url: url.into(),
            reason: FailReason::HashMismatch {
                expected: expected.clone(),
                actual: actual.clone(),
            },
        };
        set_language(Language::ZhCn);
        let msg = e.to_string();
        assert!(msg.contains(url) && msg.contains(&expected) && msg.contains(&actual));
        let rest = msg
            .replace(url, "")
            .replace(&expected, "")
            .replace(&actual, "");
        assert!(
            !rest.chars().any(|c| c.is_ascii_alphabetic()),
            "English left in zh-CN message: {msg}"
        );
        set_language(Language::En);
        let msg = e.to_string();
        assert!(msg.is_ascii() && msg.contains(&expected), "{msg}");
        set_language(Language::ZhCn);
    }

    #[test]
    fn every_reason_renders_from_the_locale_files() {
        use crate::i18n::{Language, locale_lock, set_language};
        let _locale = locale_lock();
        let reasons = [
            FailReason::Http(404),
            FailReason::HashMismatch {
                expected: "aa".into(),
                actual: "bb".into(),
            },
            FailReason::SizeMismatch {
                expected: 5,
                actual: 3,
            },
            FailReason::Network("connection reset".into()),
            FailReason::Io("disk full".into()),
            FailReason::NoAttempt,
            FailReason::TooSlow,
        ];
        for lang in [Language::ZhCn, Language::En] {
            set_language(lang);
            for r in &reasons {
                let msg = r.to_string();
                assert!(!msg.is_empty() && !msg.contains("errors."), "{msg}");
            }
        }
        set_language(Language::En);
        assert!(reasons[0].to_string().contains("404"));
        assert!(reasons[3].to_string().contains("connection reset"));
        assert!(reasons[4].to_string().contains("disk full"));
        set_language(Language::ZhCn);
    }

    #[tokio::test]
    async fn mirror_error_falls_back_to_original() {
        let server = MockServer::start().await;
        serve(&server, "/mirror/a", 500, b"").await;
        serve(&server, "/orig/a", 200, b"hello").await;
        let (d, orig) = setup(&server);
        let dir = tempfile::tempdir().unwrap();
        let j = job(&orig, dir.path());
        d.run(vec![j.clone()], None).await.unwrap();
        assert_eq!(std::fs::read(&j.dest).unwrap(), b"hello");
    }

    /// The single "download fell back" line mentioning `server`.
    fn fallback_line(server: &MockServer) -> String {
        let lines = crate::test_log::test_log_buffer().lines_containing(&server.uri());
        let hits: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("download fell back"))
            .collect();
        assert_eq!(hits.len(), 1, "{lines:?}");
        hits[0].clone()
    }

    #[tokio::test]
    async fn fallback_is_logged_with_distinct_original_and_used() {
        let server = MockServer::start().await;
        serve(&server, "/mirror/a", 404, b"").await;
        serve(&server, "/orig/a", 404, b"").await;
        serve(&server, "/fb/a", 200, b"hello").await;
        let (d, orig) = setup(&server);
        let d = d.with_fallback(Some(Mirror {
            id: "fb".into(),
            rules: vec![(orig.clone(), format!("{}/fb", server.uri()))],
        }));
        let dir = tempfile::tempdir().unwrap();
        crate::test_log::test_log_buffer();
        d.run(vec![job(&orig, dir.path())], None).await.unwrap();
        let line = fallback_line(&server);
        assert!(line.contains(&format!("original={orig}/a")), "{line}");
        assert!(
            line.contains(&format!("used={}/fb/a", server.uri())),
            "{line}"
        );
        assert!(!line.contains("/mirror/a"), "{line}");
    }

    #[tokio::test]
    async fn mirror_fallback_logs_the_original_url() {
        let server = MockServer::start().await;
        serve(&server, "/mirror/a", 404, b"").await;
        serve(&server, "/orig/a", 200, b"hello").await;
        let (d, orig) = setup(&server);
        let dir = tempfile::tempdir().unwrap();
        crate::test_log::test_log_buffer();
        d.run(vec![job(&orig, dir.path())], None).await.unwrap();
        let line = fallback_line(&server);
        assert!(line.contains(&format!("original={orig}/a")), "{line}");
        assert!(line.contains(&format!("used={orig}/a")), "{line}");
    }

    #[tokio::test]
    async fn all_sources_fail_reports_url() {
        let server = MockServer::start().await;
        serve(&server, "/mirror/a", 404, b"").await;
        serve(&server, "/orig/a", 404, b"").await;
        let (d, orig) = setup(&server);
        let dir = tempfile::tempdir().unwrap();
        let j = job(&orig, dir.path());
        match d.run(vec![j.clone()], None).await {
            Err(DownloadError::Failed { url, .. }) => assert!(url.ends_with("/orig/a"), "{url}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!j.dest.exists());
        let leftover = std::fs::read_dir(j.dest.parent().unwrap()).unwrap().count();
        assert_eq!(leftover, 0);
    }

    #[tokio::test]
    async fn fetch_bytes_verifies_and_falls_back() {
        let server = MockServer::start().await;
        serve(&server, "/mirror/a", 200, b"stale").await;
        serve(&server, "/orig/a", 200, b"hello").await;
        let (d, orig) = setup(&server);
        let url = format!("{orig}/a");
        let got = d.fetch_bytes(&url, Some(&hello_sha1())).await.unwrap();
        assert_eq!(got, b"hello");
        assert!(d.fetch_bytes(&url, Some("deadbeef")).await.is_err());
    }

    #[tokio::test]
    async fn size_mismatch_without_sha1_fails() {
        let server = MockServer::start().await;
        serve(&server, "/mirror/a", 200, b"hello").await;
        serve(&server, "/orig/a", 200, b"hello").await;
        let (d, orig) = setup(&server);
        let dir = tempfile::tempdir().unwrap();
        let mut j = job(&orig, dir.path());
        j.sha1 = None;
        j.size = Some(99);
        assert!(d.run(vec![j.clone()], None).await.is_err());
        assert!(!j.dest.exists());
    }

    #[tokio::test]
    async fn reports_progress_for_many_jobs() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let (d, orig) = setup(&server);
        let d = d.with_concurrency(4);
        let dir = tempfile::tempdir().unwrap();
        let jobs: Vec<_> = (0..50)
            .map(|i| DownloadJob {
                url: format!("{orig}/f{i}"),
                dest: dir.path().join(format!("f{i}")),
                sha1: Some(hello_sha1()),
                size: None,
            })
            .collect();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.run(jobs, Some(tx)).await.unwrap();
        let mut last = None;
        while let Some(p) = rx.recv().await {
            last = Some(p);
        }
        assert_eq!(
            last,
            Some(Progress {
                done: 50,
                total: 50,
                bytes_done: 250,
                bytes_total: 0,
            })
        );
    }

    #[tokio::test]
    async fn duplicate_dest_jobs_download_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/mirror/a"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        let (d, orig) = setup(&server);
        let d = d.with_concurrency(4);
        let dir = tempfile::tempdir().unwrap();
        let j = job(&orig, dir.path());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.run(vec![j.clone(), j.clone()], Some(tx)).await.unwrap();
        assert_eq!(std::fs::read(&j.dest).unwrap(), b"hello");
        let mut last = None;
        while let Some(p) = rx.recv().await {
            last = Some(p);
        }
        assert_eq!(
            last,
            Some(Progress {
                done: 2,
                total: 2,
                bytes_done: 5,
                bytes_total: 0,
            })
        );
    }

    /// Serves every request with a 200 whose body `body` trickles out one
    /// byte per `every`; returns the server's base URL.
    async fn drip_server(body: &'static [u8], every: Duration) -> String {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
                    if sock.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    for b in body {
                        tokio::time::sleep(every).await;
                        if sock.write_all(&[*b]).await.is_err() || sock.flush().await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    fn dir_names(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn concurrent_runs_to_one_dest_do_not_corrupt() {
        let server = MockServer::start().await;
        let body: Vec<u8> = (0..512 * 1024u32).map(|i| (i % 251) as u8).collect();
        // The second run waits for the first, then finds the file in place.
        Mock::given(method("GET"))
            .and(path("/mirror/a"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body.clone())
                    .set_delay(Duration::from_millis(200)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (d1, orig) = setup(&server);
        let (d2, _) = setup(&server);
        let dir = tempfile::tempdir().unwrap();
        let mut j = job(&orig, dir.path());
        j.sha1 = Some(hex::encode(Sha1::digest(&body)));
        let (a, b) = tokio::join!(d1.run(vec![j.clone()], None), d2.run(vec![j.clone()], None));
        a.unwrap();
        b.unwrap();
        assert_eq!(std::fs::read(&j.dest).unwrap(), body);
        assert_eq!(dir_names(j.dest.parent().unwrap()), vec!["a.bin"]);
    }

    #[tokio::test]
    async fn slow_mirror_falls_back_as_too_slow() {
        let server = MockServer::start().await;
        serve(&server, "/orig/a", 200, b"hello").await;
        let drip = drip_server(b"hello", Duration::from_millis(200)).await;
        let orig = format!("{}/orig", server.uri());
        let mirror = Mirror {
            id: "m".into(),
            rules: vec![(orig.clone(), format!("{drip}/mirror"))],
        };
        let d = Downloader::new(reqwest::Client::new(), mirror)
            .with_retry(2, Duration::ZERO)
            .with_throughput_window(Duration::from_millis(300));
        let dir = tempfile::tempdir().unwrap();
        let j = job(&orig, dir.path());
        d.run(vec![j.clone()], None).await.unwrap();
        assert_eq!(std::fs::read(&j.dest).unwrap(), b"hello");
        assert_eq!(
            d.fallback_events(),
            vec![FallbackEvent {
                original: format!("{orig}/a"),
                used: format!("{orig}/a"),
            }]
        );
        assert_eq!(d.fallback_count(), 1);
    }

    #[tokio::test]
    async fn stalled_mirror_is_cut_as_too_slow() {
        // The mirror sends headers, then nothing; the official URL is
        // unreachable, so TooSlow (outranking Network) is reported.
        let drip = drip_server(b"hello", Duration::from_secs(30)).await;
        let official = "https://piston-data.mojang.com";
        let mirror = Mirror {
            id: "m".into(),
            rules: vec![(official.into(), format!("{drip}/m"))],
        };
        let client = crate::loader::forge::test_support::offline_client();
        let d = Downloader::new(client, mirror)
            .with_retry(1, Duration::ZERO)
            .with_throughput_window(Duration::from_millis(200));
        let dir = tempfile::tempdir().unwrap();
        let j = job(official, dir.path());
        let r = d.run(vec![j.clone()], None).await;
        assert!(
            matches!(
                r,
                Err(DownloadError::Failed {
                    reason: FailReason::TooSlow,
                    ..
                })
            ),
            "{r:?}"
        );
        assert!(dir_names(j.dest.parent().unwrap()).is_empty());
    }

    #[tokio::test]
    async fn last_attempt_of_last_candidate_may_finish_slowly() {
        // Only one candidate: its first attempt is cut, the final one is not.
        let drip = drip_server(b"hello", Duration::from_millis(100)).await;
        let d = Downloader::new(reqwest::Client::new(), Mirror::official())
            .with_retry(2, Duration::ZERO)
            .with_throughput_window(Duration::from_millis(200));
        let dir = tempfile::tempdir().unwrap();
        let j = job(&drip, dir.path());
        d.run(vec![j.clone()], None).await.unwrap();
        assert_eq!(std::fs::read(&j.dest).unwrap(), b"hello");
        assert_eq!(dir_names(j.dest.parent().unwrap()), vec!["a.bin"]);
        assert!(d.fallback_events().is_empty());
    }

    #[tokio::test]
    async fn most_informative_failure_wins() {
        let server = MockServer::start().await;
        serve(&server, "/mirror/a", 404, b"").await;
        let official = "https://piston-data.mojang.com";
        let mirror = Mirror {
            id: "m".into(),
            rules: vec![(official.into(), format!("{}/mirror", server.uri()))],
        };
        let client = crate::loader::forge::test_support::offline_client();
        let d = Downloader::new(client, mirror).with_retry(2, Duration::ZERO);
        let dir = tempfile::tempdir().unwrap();
        let r = d.run(vec![job(official, dir.path())], None).await;
        assert!(
            matches!(
                r,
                Err(DownloadError::Failed {
                    reason: FailReason::Http(404),
                    ..
                })
            ),
            "{r:?}"
        );
    }

    #[test]
    fn failure_ranking_keeps_the_most_informative_first() {
        let order = [
            FailReason::HashMismatch {
                expected: "a".into(),
                actual: "b".into(),
            },
            FailReason::SizeMismatch {
                expected: 1,
                actual: 2,
            },
            FailReason::Http(404),
            FailReason::Http(503),
            FailReason::TooSlow,
            FailReason::Network("x".into()),
            FailReason::Io("y".into()),
            FailReason::NoAttempt,
        ];
        for (i, hi) in order.iter().enumerate() {
            for lo in &order[i + 1..] {
                assert!(more_informative(hi, lo), "{hi:?} vs {lo:?}");
                assert!(!more_informative(lo, hi), "{lo:?} vs {hi:?}");
            }
        }
        // Ties keep the first reason seen.
        assert!(!more_informative(
            &FailReason::Http(410),
            &FailReason::Http(404)
        ));
        assert!(!more_informative(
            &FailReason::Network("later".into()),
            &FailReason::Network("first".into())
        ));
    }

    #[tokio::test]
    async fn progress_reports_bytes() {
        let server = MockServer::start().await;
        serve(&server, "/mirror/x", 200, b"abc").await;
        serve(&server, "/mirror/y", 200, b"hello").await;
        let (d, orig) = setup(&server);
        let dir = tempfile::tempdir().unwrap();
        let jobs = [("x", 3), ("y", 5)]
            .map(|(n, size)| DownloadJob {
                url: format!("{orig}/{n}"),
                dest: dir.path().join(n),
                sha1: None,
                size: Some(size),
            })
            .to_vec();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.run(jobs, Some(tx)).await.unwrap();
        let mut last = None;
        while let Some(p) = rx.recv().await {
            last = Some(p);
        }
        assert_eq!(
            last,
            Some(Progress {
                done: 2,
                total: 2,
                bytes_done: 8,
                bytes_total: 8,
            })
        );
    }

    #[test]
    fn fallback_events_keep_the_latest_256() {
        let d = Downloader::new(reqwest::Client::new(), Mirror::official());
        for i in 0..300 {
            d.record_fallback("o", &format!("u{i}"));
        }
        let events = d.fallback_events();
        assert_eq!(events.len(), 256);
        assert_eq!(events[0].used, "u44");
        assert_eq!(events[255].used, "u299");
        assert_eq!(d.fallback_count(), 300);
    }

    #[test]
    fn dest_lock_keys_are_absolute_and_normalized() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(dest_key(Path::new("a/./b/../c")).unwrap(), cwd.join("a/c"));
        assert_eq!(
            dest_key(Path::new("/x/y/../z")).unwrap(),
            PathBuf::from("/x/z")
        );
    }
}
