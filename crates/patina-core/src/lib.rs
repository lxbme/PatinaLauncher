rust_i18n::i18n!("locales", fallback = "en");

pub mod config;
pub mod download;
pub mod fsutil;
pub mod i18n;
pub mod instance;
pub mod instance_files;
pub mod java;
pub mod jvm;
pub mod launch;
pub mod loader;
pub mod locks;
pub mod meta;
pub mod paths;
pub mod platform;
pub mod prepare;
pub mod progress;
pub mod resolve;
#[cfg(test)]
mod test_log;
