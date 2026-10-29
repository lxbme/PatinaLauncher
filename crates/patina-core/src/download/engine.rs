use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::{StreamExt, TryStreamExt};
use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;

use super::Mirror;

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
}

/// Why the last attempt of a download failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    /// Non-success HTTP status.
    Http(u16),
    /// The body's SHA-1 differs from the metadata.
    HashMismatch { expected: String, actual: String },
    /// The body's length differs from the metadata (no SHA-1 known).
    SizeMismatch { expected: u64, actual: u64 },
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
            FailReason::Network(e) => t!("errors.download.reason.network", error = e),
            FailReason::Io(e) => t!("errors.download.reason.io", error = e),
            FailReason::NoAttempt => t!("errors.download.reason.no_attempt"),
        };
        f.write_str(&s)
    }
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

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
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

    pub async fn run(
        &self,
        jobs: Vec<DownloadJob>,
        progress: Option<tokio::sync::mpsc::UnboundedSender<Progress>>,
    ) -> Result<(), DownloadError> {
        let total = jobs.len();
        if total == 0 {
            return Ok(());
        }
        // Jobs sharing a destination would race on one .part file, so keep the
        // first job per dest (later duplicates, even with a differing sha1, are
        // dropped) and credit each duplicate when its representative finishes.
        let mut index: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
        let mut unique: Vec<(DownloadJob, usize)> = Vec::new();
        for job in jobs {
            match index.get(&job.dest) {
                Some(&i) => unique[i].1 += 1,
                None => {
                    index.insert(job.dest.clone(), unique.len());
                    unique.push((job, 1));
                }
            }
        }
        let mut done = 0usize;
        let mut results = futures::stream::iter(unique)
            .map(|(job, weight)| async move { self.run_job(&job).await.map(|()| weight) })
            .buffer_unordered(self.concurrency);
        // Returning early on the first error drops the stream and cancels the rest.
        while let Some(weight) = results.try_next().await? {
            done += weight;
            if let Some(tx) = &progress {
                let _ = tx.send(Progress { done, total });
            }
        }
        Ok(())
    }

    pub async fn fetch_bytes(
        &self,
        url: &str,
        sha1: Option<&str>,
    ) -> Result<Vec<u8>, DownloadError> {
        self.fetch(url, sha1, None, None).await
    }

    async fn run_job(&self, job: &DownloadJob) -> Result<(), DownloadError> {
        if Self::existing_ok(job).await {
            return Ok(());
        }
        if let Some(parent) = job.dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let part = part_path(&job.dest);
        self.fetch(&job.url, job.sha1.as_deref(), job.size, Some(&part))
            .await?;
        tokio::fs::rename(&part, &job.dest).await?;
        Ok(())
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

    /// Tries each candidate URL with retries. With `part` the body is written
    /// there (removed on failure) and an empty Vec is returned; without it the
    /// body is returned in memory.
    async fn fetch(
        &self,
        url: &str,
        sha1: Option<&str>,
        size: Option<u64>,
        part: Option<&Path>,
    ) -> Result<Vec<u8>, DownloadError> {
        let mut last = FailReason::NoAttempt;
        for (i, cand) in self.candidates(url).iter().enumerate() {
            for n in 1..=self.attempts {
                match self.attempt(cand, sha1, size, part).await {
                    Ok(bytes) => {
                        if i > 0 {
                            self.fallbacks.fetch_add(1, Ordering::Relaxed);
                        }
                        return Ok(bytes);
                    }
                    Err(e) => {
                        if let Some(p) = part {
                            let _ = tokio::fs::remove_file(p).await;
                        }
                        // Wrong content from this URL will not change on retry.
                        let final_for_candidate = matches!(e, FailReason::HashMismatch { .. });
                        last = e;
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
            reason: last,
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

    async fn attempt(
        &self,
        url: &str,
        sha1: Option<&str>,
        size: Option<u64>,
        part: Option<&Path>,
    ) -> Result<Vec<u8>, FailReason> {
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
        let mut len = 0u64;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(net)?;
            len += chunk.len() as u64;
            hasher.update(&chunk);
            match file.as_mut() {
                Some(f) => f.write_all(&chunk).await.map_err(io)?,
                None => mem.extend_from_slice(&chunk),
            }
        }
        if let Some(mut f) = file {
            f.flush().await.map_err(io)?;
        }
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
        Ok(mem)
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
                total: 50
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
        assert_eq!(last, Some(Progress { done: 2, total: 2 }));
    }
}
