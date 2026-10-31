//! 外观: the interface scale, the language and the theme.

use gpui_kit::component::button::{Button, ButtonGroup};
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{Selectable, Sizable};
use gpui_kit::{Context, ElementId, IntoElement, ParentElement};
use rust_i18n::t;

use super::{Section, SettingsPage};
use crate::i18n::{Language, apply_language};
use crate::scale::{UI_SCALES, applied_scale, max_scale, set_scale};
use crate::settings::SettingsStore;

/// The languages offered, with the key of their name (each named in
/// itself).
const LANGUAGES: [(Language, &str); 2] = [
    (Language::ZhCn, "prefs.language_zh_cn"),
    (Language::En, "prefs.language_en"),
];

/// Element id of the scale control's step `scale`.
pub fn scale_step_id(scale: f32) -> ElementId {
    ElementId::Name(format!("prefs-scale-{}", (scale * 100.).round() as u32).into())
}

impl SettingsPage {
    pub(super) fn render_appearance(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        Self::section(Section::Appearance, cx)
            .child(self.render_scale(cx))
            .child(self.render_language(cx))
            .child(Self::render_theme(cx))
    }

    /// The steps that fit the display, the one shown now marked. The zoom
    /// keys go through the same [`set_scale`], so both stay in step.
    fn render_scale(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (now, max) = (applied_scale(cx), max_scale(cx));
        let steps: Vec<f32> = UI_SCALES.into_iter().filter(|s| *s <= max).collect();
        let buttons = steps.iter().map(|&step| {
            let percent = (step * 100.).round() as u32;
            Button::new(scale_step_id(step))
                .label(t!("prefs.scale_step", percent = percent).to_string())
                .selected(step == now)
        });
        let limited = max < UI_SCALES[UI_SCALES.len() - 1];
        Self::field(t!("prefs.scale").to_string(), cx)
            .child(
                ButtonGroup::new("prefs-scale")
                    .outline()
                    .small()
                    .children(buttons)
                    .on_click(move |clicked: &Vec<usize>, _, cx| {
                        if let Some(step) = clicked.first().and_then(|ix| steps.get(*ix)) {
                            set_scale(*step, cx);
                        }
                    }),
            )
            .child(Self::hint(
                "prefs-scale-hint",
                if limited {
                    format!("{} {}", t!("prefs.scale_limited"), t!("prefs.scale_hint"))
                } else {
                    t!("prefs.scale_hint").to_string()
                },
                cx,
            ))
    }

    /// 中文 / English: switches at once and is saved to `gui.toml`.
    fn render_language(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let now = SettingsStore::get(cx).language;
        Self::field(t!("prefs.language").to_string(), cx).child(
            RadioGroup::horizontal("prefs-language")
                .selected_index(LANGUAGES.iter().position(|(l, _)| *l == now))
                .children(LANGUAGES.map(|(_, key)| Radio::new(key).label(t!(key).to_string())))
                .on_click(|ix: &usize, window, cx| {
                    if let Some((language, _)) = LANGUAGES.get(*ix) {
                        apply_language(*language, window, cx);
                    }
                }),
        )
    }

    /// Dark only: light comes later.
    fn render_theme(cx: &mut Context<Self>) -> impl IntoElement + use<> {
        Self::field(t!("prefs.theme").to_string(), cx)
            .child(
                RadioGroup::horizontal("prefs-theme")
                    .selected_index(Some(0))
                    .child(Radio::new("dark").label(t!("prefs.theme_dark").to_string()))
                    .child(
                        Radio::new("light")
                            .label(t!("prefs.theme_light").to_string())
                            .disabled(true),
                    ),
            )
            .child(Self::hint(
                "prefs-theme-hint",
                t!("prefs.theme_light_later").to_string(),
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_steps_have_stable_ids() {
        assert_eq!(
            scale_step_id(1.25),
            ElementId::Name("prefs-scale-125".into())
        );
        assert_eq!(scale_step_id(0.8), ElementId::Name("prefs-scale-80".into()));
    }
}
