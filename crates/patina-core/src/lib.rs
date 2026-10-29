rust_i18n::i18n!("locales", fallback = "en");

/// Whether this build has development-only offline accounts (the `dev-offline`
/// feature). Release applications should `const`-assert that it is `false`.
pub const DEV_OFFLINE: bool = cfg!(feature = "dev-offline");

pub mod accounts;
pub mod config;
pub mod content;
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
