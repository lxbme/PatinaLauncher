//! Shared presentation of Java groups and read-only core GC presets.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::collapsible::Collapsible;
use gpui_kit::component::{ActiveTheme, Icon, Sizable, h_flex, v_flex};
use gpui_kit::{
    App, FontWeight, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, TestSupportExt, div,
};
use patina_core::jvm::GcPreset;
use rust_i18n::t;

use crate::fonts::Strong;
use crate::layout::TEXT_ROW;

pub(crate) fn group_heading(icon: IconName, text: String, cx: &App) -> impl IntoElement + use<> {
    field_label(text, Some(icon), cx)
}

fn field_label(text: String, icon: Option<IconName>, cx: &App) -> impl IntoElement + use<> {
    h_flex()
        .gap_2()
        .text_size(TEXT_ROW)
        .text_color(cx.theme().foreground)
        .children(icon.map(|icon| Icon::new(icon).small()))
        .child(div().strong(FontWeight::MEDIUM, cx).child(text))
}

/// Java field names are ordinary readable content, not secondary help text.
pub(crate) fn field(label: String, icon: Option<IconName>, cx: &App) -> gpui_kit::Div {
    v_flex().gap_2().child(field_label(label, icon, cx))
}

/// No runtime is implied by the global automatic choice. Conditional variants
/// come from the same core method as launch, not a second table of JVM flags.
fn gc_arguments(preset: GcPreset, major: Option<u32>) -> String {
    if matches!(preset, GcPreset::None | GcPreset::Custom) {
        return t!("java_settings.gc_no_flags").to_string();
    }
    match major {
        Some(major) if !preset.is_compatible(major) => {
            t!("java_settings.gc_incompatible", major = major).to_string()
        }
        Some(major) => preset.args(major).join("\n"),
        None if preset == GcPreset::GenerationalZgc => format!(
            "{}\n\n{}\n{}\n\n{}\n{}",
            t!("java_settings.gc_conditional"),
            t!("java_settings.gc_java", version = "21–22"),
            preset.args(21).join("\n"),
            t!("java_settings.gc_java", version = "23+"),
            preset.args(23).join("\n")
        ),
        None => format!(
            "{}\n\n{}\n{}",
            t!("java_settings.gc_conditional"),
            t!("java_settings.gc_java", version = "8+"),
            preset.args(8).join("\n")
        ),
    }
}

pub(crate) fn gc_disclosure(
    id: &'static str,
    preset: GcPreset,
    major: Option<u32>,
    open: bool,
    trigger: Button,
    cx: &App,
) -> Collapsible {
    let text = gc_arguments(preset, major);
    Collapsible::new().open(open).child(trigger).content(
        div()
            .id(format!("{id}-args"))
            .test_support()
            .aria_label(text.clone())
            .w_full()
            .min_w_0()
            .p_3()
            .rounded_md()
            .bg(cx.theme().muted)
            .text_color(cx.theme().foreground)
            .text_size(TEXT_ROW)
            .child(text),
    )
}
