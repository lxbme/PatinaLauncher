pub mod engine;
pub mod mirror;
pub mod speedtest;

pub use engine::{DownloadError, DownloadJob, Downloader, FailReason, Progress, sha1_file};
pub use mirror::Mirror;
pub use speedtest::{
    PROBE_URL, SPEEDTEST_TIMEOUT, fastest, measure, measure_all, pick_fastest, resolve_choice,
};
