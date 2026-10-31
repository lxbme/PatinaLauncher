//! Platform-specific queries of the app (the core library keeps its own in
//! `patina_core::platform`). Everything else in this crate is portable.

/// The user's interface locale as a POSIX-style name such as `zh_CN.UTF-8`,
/// or `None` when it is unknown.
///
/// Linux: `$LANG`, which the desktop session sets from the user's language
/// choice.
#[cfg(target_os = "linux")]
pub fn system_locale() -> Option<String> {
    std::env::var("LANG").ok().filter(|l| !l.is_empty())
}

/// The user's interface locale; see the Linux version.
///
/// Other platforms have no native lookup yet and return `None`, so a first
/// run starts in English until the user picks a language.
#[cfg(not(target_os = "linux"))]
pub fn system_locale() -> Option<String> {
    None
}
