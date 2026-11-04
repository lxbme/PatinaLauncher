//! Concise localized presentation; rules and attempt lifetime stay outside render.

use crate::fonts::Strong;
use gpui_kit::component::{ActiveTheme, button::Button, collapsible::Collapsible, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, FontWeight, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, TestSupportExt, div, rems,
};
use patina_core::launch::diagnosis::{Diagnosis, DiagnosisKind};
use rust_i18n::t;

pub(crate) fn diagnosis_text(diagnosis: &Diagnosis) -> (String, String) {
    let (title, text) = match diagnosis.kind() {
        DiagnosisKind::JavaVersion => {
            let text = match (diagnosis.required_java(), diagnosis.supported_java()) {
                (Some(required), Some(supported)) => t!(
                    "diagnosis.java.both",
                    required = required,
                    supported = supported
                ),
                (Some(required), None) => t!("diagnosis.java.required", required = required),
                _ => t!("diagnosis.java.unknown"),
            };
            (t!("diagnosis.java.title"), text)
        }
        DiagnosisKind::JvmArgument => {
            let mut argument = diagnosis.argument().unwrap_or_default().to_string();
            patina_core::launch::log::truncate_with_ellipsis(&mut argument, 120);
            (
                t!("diagnosis.argument.title"),
                t!("diagnosis.argument.body", argument = argument),
            )
        }
        DiagnosisKind::HeapExhausted => (t!("diagnosis.heap.title"), t!("diagnosis.heap.body")),
        DiagnosisKind::HeapReservation => (
            t!("diagnosis.reservation.title"),
            t!("diagnosis.reservation.body"),
        ),
        DiagnosisKind::NativeMemory => (t!("diagnosis.native.title"), t!("diagnosis.native.body")),
        DiagnosisKind::PermissionDenied => (
            t!("diagnosis.permission.title"),
            t!("diagnosis.permission.body"),
        ),
        DiagnosisKind::DiskFull => (t!("diagnosis.disk.title"), t!("diagnosis.disk.body")),
        _ => unreachable!("only supported local diagnosis kinds are produced"),
    };
    (title.to_string(), text.to_string())
}

pub(crate) fn render_diagnosis(
    diagnosis: &Diagnosis,
    expanded: bool,
    toggle: Button,
    actions: AnyElement,
    cx: &App,
) -> impl IntoElement + use<> {
    let (title, text) = diagnosis_text(diagnosis);
    v_flex()
        .id("instance-diagnosis")
        .test_support()
        .flex_none()
        .min_w_0()
        .px(crate::layout::GUTTER)
        .pb_4()
        .gap_2()
        .text_sm()
        .text_color(cx.theme().foreground)
        .child(
            div()
                .id("diagnosis-title")
                .aria_label(title.clone())
                .test_support()
                .strong(FontWeight::SEMIBOLD, cx)
                .child(title),
        )
        .child(
            div()
                .id("diagnosis-explanation")
                .aria_label(text.clone())
                .test_support()
                .line_height(rems(1.375))
                .child(text),
        )
        .child(
            Collapsible::new()
                .open(expanded)
                .child(h_flex().child(toggle))
                .content(
                    div()
                        .id("diagnosis-evidence")
                        .test_support()
                        .min_w_0()
                        .max_w_full()
                        .max_h(rems(8.))
                        .overflow_x_scroll()
                        .overflow_y_scroll()
                        .p_2()
                        .bg(cx.theme().muted)
                        .font_family(cx.theme().mono_font_family.clone())
                        .whitespace_nowrap()
                        .child(diagnosis.evidence().to_string()),
                ),
        )
        .child(actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use patina_core::launch::diagnosis::{EvidenceContext, analyze};

    #[test]
    fn diagnosis_argument_copy_is_bounded_and_evidence_is_complete() {
        let _locale = crate::i18n::locale_lock();
        crate::i18n::set_language(crate::i18n::Language::En);
        let argument = "a".repeat(3000);
        let source = format!("Unrecognized VM option '{argument}'");
        let diagnosis = analyze(&source, EvidenceContext::TerminalError).unwrap();
        let (title, text) = diagnosis_text(&diagnosis);
        assert_eq!(title, "JVM argument rejected");
        assert!(text.len() < 300);
        assert!(text.contains('…'));
        assert_eq!(diagnosis.evidence(), source);
    }
}
