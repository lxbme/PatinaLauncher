pub mod mirror;
pub mod speedtest;

pub use mirror::Mirror;
pub use speedtest::{measure, pick_fastest, resolve_choice, PROBE_URL};
