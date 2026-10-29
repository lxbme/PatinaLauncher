rust_i18n::i18n!("locales", fallback = "en");

pub mod config;
pub mod download;
pub mod fsutil;
pub mod i18n;
pub mod instance;
pub mod java;
pub mod jvm;
pub mod launch;
pub mod loader;
pub mod meta;
pub mod paths;
pub mod platform;
pub mod prepare;
pub mod resolve;
