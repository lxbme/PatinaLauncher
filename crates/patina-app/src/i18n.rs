//! Interface language. Translations live in `crates/patina-app/locales/*.yml`
//! (loaded by `rust_i18n::i18n!` in `lib.rs`); look them up with
//! `rust_i18n::t!("key")`.
//!
//! The locale is process-global. The core library (errors, progress steps),
//! this crate and gpui-component (its own widget strings) each translate from
//! it, and [`set_language`] switches all three together.

pub use patina_core::i18n::Language;

/// Switches the interface language of the core library, this crate and
/// gpui-component.
pub fn set_language(l: Language) {
    // Sets the shared locale, after forcing the core's lazy rust-i18n
    // backend (see there for the race this avoids).
    patina_core::i18n::set_language(l);
    // This crate's backend has the same lazy initializer, which reads the
    // current locale and writes it back; build it now so it cannot undo a
    // switch made while it runs on another thread.
    let _ = rust_i18n::t!("app.title");
    rust_i18n::set_locale(l.code());
    // gpui-component's backend has the same initializer; build it as well,
    // so all three exist before the write below. (`_rust_i18n_backend` is
    // the accessor `rust_i18n::i18n!` generates; it is public but hidden.)
    let _ = gpui_kit::component::_rust_i18n_backend();
    // gpui-component's own setter, so its widget strings follow even if it
    // ever keeps a locale of its own.
    gpui_kit::component::set_locale(l.code());
}

/// The language a first run starts in: Chinese when the system locale
/// (`platform::system_locale`) is a Chinese one, English otherwise.
pub fn system_language() -> Language {
    language_for_locale(crate::platform::system_locale().as_deref())
}

/// `ZhCn` for a POSIX locale starting with `zh` (`zh_CN.UTF-8`, `zh_TW`),
/// else `En`.
pub fn language_for_locale(locale: Option<&str>) -> Language {
    match locale {
        Some(l) if l.starts_with("zh") => Language::ZhCn,
        _ => Language::En,
    }
}

/// Serializes tests that switch the process-global locale.
#[cfg(test)]
pub(crate) fn locale_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// Leaf key → its text.
    fn flatten(prefix: &str, v: &serde_json::Value, out: &mut BTreeMap<String, String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, v) in m {
                    if k.starts_with('_') {
                        continue;
                    }
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    flatten(&key, v, out);
                }
            }
            serde_json::Value::String(t) => {
                out.insert(prefix.to_owned(), t.clone());
            }
            other => {
                out.insert(prefix.to_owned(), other.to_string());
            }
        }
    }

    fn entries(file: &str) -> BTreeMap<String, String> {
        let path = format!("{}/locales/{file}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_saphyr::from_str(&text).unwrap();
        let mut out = BTreeMap::new();
        flatten("", &v, &mut out);
        out
    }

    /// The `%{name}` placeholders of a translation.
    fn placeholders(text: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut rest = text;
        while let Some(start) = rest.find("%{") {
            rest = &rest[start + 2..];
            let Some(end) = rest.find('}') else { break };
            out.insert(rest[..end].to_string());
            rest = &rest[end + 1..];
        }
        out
    }

    #[test]
    fn locale_keys_match() {
        let en = entries("en.yml");
        let zh = entries("zh-CN.yml");
        assert!(!en.is_empty(), "en.yml has no keys");
        let only_en: Vec<_> = en.keys().filter(|k| !zh.contains_key(*k)).collect();
        let only_zh: Vec<_> = zh.keys().filter(|k| !en.contains_key(*k)).collect();
        assert!(
            only_en.is_empty() && only_zh.is_empty(),
            "only en: {only_en:?}, only zh-CN: {only_zh:?}"
        );
        let mismatched: Vec<_> = en
            .iter()
            .filter(|(k, text)| placeholders(text) != placeholders(&zh[*k]))
            .map(|(k, _)| k)
            .collect();
        assert!(
            mismatched.is_empty(),
            "placeholder mismatch: {mismatched:?}"
        );
        assert_eq!(
            placeholders("a %{url}: %{reason} %{url}"),
            BTreeSet::from(["url".to_string(), "reason".to_string()])
        );
    }

    #[test]
    fn language_switch_reaches_core() {
        use patina_core::progress::Phase;
        let _locale = locale_lock();

        set_language(Language::En);
        assert_eq!(Phase::FetchMetadata.display_name(), "Fetching version info");
        assert_eq!(rust_i18n::t!("notice.close"), "Close");
        assert_eq!(&*gpui_kit::component::locale(), "en");

        set_language(Language::ZhCn);
        assert_eq!(Phase::FetchMetadata.display_name(), "正在获取版本信息");
        assert_eq!(rust_i18n::t!("notice.close"), "关闭");
        assert_eq!(&*gpui_kit::component::locale(), "zh-CN");
    }

    #[test]
    fn first_run_language_follows_lang() {
        assert_eq!(language_for_locale(Some("zh_CN.UTF-8")), Language::ZhCn);
        assert_eq!(language_for_locale(Some("zh_TW")), Language::ZhCn);
        assert_eq!(language_for_locale(Some("en_US.UTF-8")), Language::En);
        assert_eq!(language_for_locale(Some("C")), Language::En);
        assert_eq!(language_for_locale(Some("")), Language::En);
        assert_eq!(language_for_locale(None), Language::En);
    }
}
