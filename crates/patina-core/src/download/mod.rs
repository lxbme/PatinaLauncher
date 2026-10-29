pub mod engine;
pub mod mirror;
pub mod speedtest;

pub use engine::{
    DownloadError, DownloadJob, Downloader, FailReason, FallbackEvent,
    MIN_THROUGHPUT_BYTES_PER_SEC, Progress, THROUGHPUT_WINDOW, sha1_file,
};
pub use mirror::Mirror;
pub use speedtest::{
    PROBE_URL, SPEEDTEST_TIMEOUT, fastest, measure, measure_all, pick_fastest, resolve_choice,
};
