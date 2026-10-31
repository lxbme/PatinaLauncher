//! Typography helpers: the CJK family for weighted text ([`Strong`]) and
//! tabular figures ([`tnum`]).
//!
//! gpui's Linux text system takes CJK glyphs from a fallback face at Regular
//! weight whatever weight was asked for, so bold Chinese next to bold Latin
//! looks thin. Naming an installed CJK family as the primary font makes gpui
//! select its real Medium / Bold faces (their Latin glyphs are a close
//! humanist sans). Plain-weight text keeps the theme's UI font.

use std::sync::{Arc, LazyLock};

use gpui_kit::{App, FontFeatures, FontWeight, Global, SharedString, Styled};

/// CJK families with real weights, in order of preference.
pub const CJK_FAMILIES: [&str; 5] = [
    "Noto Sans CJK SC",
    "Source Han Sans SC",
    "Source Han Sans CN",
    "WenQuanYi Micro Hei",
    "WenQuanYi Zen Hei",
];

/// The first of [`CJK_FAMILIES`] that is in `available`.
pub fn pick_cjk_family(available: &[String]) -> Option<String> {
    CJK_FAMILIES
        .into_iter()
        .find(|family| available.iter().any(|a| a == family))
        .map(str::to_owned)
}

/// The CJK family picked at startup (a gpui `Global`); `None` when none of
/// [`CJK_FAMILIES`] is installed.
pub struct CjkFont {
    family: Option<SharedString>,
}

impl Global for CjkFont {}

impl CjkFont {
    pub fn family(cx: &App) -> Option<&SharedString> {
        cx.try_global::<Self>()?.family.as_ref()
    }
}

/// Picks the CJK family from the installed fonts, once per process.
pub fn init(cx: &mut App) {
    let family = pick_cjk_family(&cx.text_system().all_font_names());
    match &family {
        Some(f) => tracing::info!(family = %f, "CJK font for weighted text"),
        None => tracing::warn!(
            candidates = ?CJK_FAMILIES,
            "no CJK font installed; weighted Chinese text uses the fallback face"
        ),
    }
    cx.set_global(CjkFont {
        family: family.map(SharedString::from),
    });
}

/// `font_weight` plus the picked CJK family (see the module docs); just the
/// weight when no CJK family is installed.
pub trait Strong: Styled + Sized {
    fn strong(self, weight: FontWeight, cx: &App) -> Self {
        let this = self.font_weight(weight);
        match CjkFont::family(cx) {
            Some(family) => this.font_family(family.clone()),
            None => this,
        }
    }
}

impl<T: Styled> Strong for T {}

/// OpenType tabular figures. Set on a root so every digit below it has the
/// same advance and changing numbers do not jitter.
pub fn tnum() -> FontFeatures {
    static TNUM: LazyLock<FontFeatures> =
        LazyLock::new(|| FontFeatures(Arc::new(vec![("tnum".into(), 1)])));
    // A clone shares the one allocation (`FontFeatures` is an `Arc`).
    TNUM.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pick_cjk_family_prefers_order() {
        let all = names(&[
            "DejaVu Sans",
            "WenQuanYi Zen Hei",
            "Source Han Sans CN",
            "Noto Sans CJK SC",
        ]);
        assert_eq!(pick_cjk_family(&all).as_deref(), Some("Noto Sans CJK SC"));
        let no_noto = names(&["WenQuanYi Zen Hei", "Source Han Sans CN"]);
        assert_eq!(
            pick_cjk_family(&no_noto).as_deref(),
            Some("Source Han Sans CN")
        );
        let last = names(&["Cantarell", "WenQuanYi Zen Hei"]);
        assert_eq!(pick_cjk_family(&last).as_deref(), Some("WenQuanYi Zen Hei"));
        // Exact names only: a different face of the family is not a match.
        let near = names(&["Noto Sans CJK JP", "noto sans cjk sc", "DejaVu Sans"]);
        assert_eq!(pick_cjk_family(&near), None);
        assert_eq!(pick_cjk_family(&[]), None);
    }
}
