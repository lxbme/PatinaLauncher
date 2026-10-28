pub mod engine;
pub mod mirror;
pub mod speedtest;

pub use engine::{DownloadError, DownloadJob, Downloader, Progress, sha1_file};
pub use mirror::Mirror;
pub use speedtest::{PROBE_URL, measure, pick_fastest, resolve_choice};
