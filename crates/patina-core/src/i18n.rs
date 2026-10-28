//! Interface language. Translations live in `crates/patina-core/locales/*.yml`
//! (loaded by `rust_i18n::i18n!` in `lib.rs`); look them up with
//! `rust_i18n::t!("key")`. The locale is process-global and shared with the
//! UI crates built on top of this library.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Language {
    /// Product default.
    #[default]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

impl Language {
    /// rust-i18n locale code.
    pub fn code(self) -> &'static str {
        match self {
            Language::En => "en",
            Language::ZhCn => "zh-CN",
        }
    }
}

/// Sets the process-global locale. The library never sets one on its own;
/// the application calls this at startup.
pub fn set_language(l: Language) {
    rust_i18n::set_locale(l.code());
}

/// Serializes tests that switch the process-global locale.
#[cfg(test)]
pub fn locale_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::DownloadError;
    use crate::jvm::JvmArgsError;
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

    fn keys(file: &str) -> BTreeSet<String> {
        entries(file).into_keys().collect()
    }

    fn placeholders(text: &str) -> BTreeSet<String> {
        let re = regex::Regex::new(r"%\{(\w+)\}").unwrap();
        re.captures_iter(text).map(|c| c[1].to_string()).collect()
    }

    #[test]
    fn locales_have_same_placeholders() {
        let en = entries("en.yml");
        let zh = entries("zh-CN.yml");
        let mut bad = Vec::new();
        for (key, en_text) in &en {
            let Some(zh_text) = zh.get(key) else { continue };
            let (a, b) = (placeholders(en_text), placeholders(zh_text));
            if a != b {
                bad.push(format!("{key}: en {a:?} vs zh-CN {b:?}"));
            }
        }
        assert!(bad.is_empty(), "placeholder mismatch:\n{}", bad.join("\n"));
        assert_eq!(
            placeholders("a %{url}: %{reason} %{url}"),
            BTreeSet::from(["url".to_string(), "reason".to_string()])
        );
    }

    #[test]
    fn locales_have_same_keys() {
        let en = keys("en.yml");
        let zh = keys("zh-CN.yml");
        assert!(!en.is_empty(), "en.yml has no keys");
        let only_en: Vec<_> = en.difference(&zh).collect();
        let only_zh: Vec<_> = zh.difference(&en).collect();
        assert!(
            only_en.is_empty() && only_zh.is_empty(),
            "only en: {only_en:?}, only zh: {only_zh:?}"
        );
    }

    fn has_cjk(s: &str) -> bool {
        s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    }

    #[test]
    fn error_messages_follow_locale() {
        let _locale = locale_lock();
        let download = || DownloadError::Failed {
            url: "https://example.com/a".into(),
            reason: crate::download::FailReason::Network("timeout".into()),
        };
        let jvm = || JvmArgsError::MemoryFlag("-Xmx2G".into());

        set_language(Language::ZhCn);
        for msg in [download().to_string(), jvm().to_string()] {
            assert!(has_cjk(&msg), "zh-CN message has no CJK: {msg}");
            assert!(!msg.starts_with("errors."), "raw key leaked: {msg}");
        }
        set_language(Language::En);
        for msg in [download().to_string(), jvm().to_string()] {
            assert!(msg.is_ascii(), "en message is not ASCII: {msg}");
            assert!(!msg.is_empty() && !msg.starts_with("errors."), "bad: {msg}");
        }
        assert!(download().to_string().contains("https://example.com/a"));
        assert!(jvm().to_string().contains("-Xmx2G"));
        set_language(Language::ZhCn);
    }
}
