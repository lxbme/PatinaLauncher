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

/// Reads only JDK release metadata; callers run this alongside probing off-thread.
pub(crate) fn vendor_name(java: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(java.parent()?.parent()?.join("release")).ok()?;
    text.lines()
        .find_map(|line| {
            line.strip_prefix("IMPLEMENTOR=")
                .map(|v| v.trim_matches('"').to_string())
        })
        .filter(|v| !v.is_empty())
}

/// Runtime provenance controls global management; instance views only select.
#[derive(Clone, Debug)]
pub(crate) enum JavaSource {
    Automatic,
    Vendor(String),
    Mojang(String),
    System,
    Manual,
}
impl JavaSource {
    fn label(&self) -> String {
        match self {
            Self::Automatic => t!("java_list.source_automatic"),
            Self::Vendor(_) => t!("java_list.source_vendor"),
            Self::Mojang(_) => t!("java_list.source_mojang"),
            Self::System => t!("java_list.source_system"),
            Self::Manual => t!("java_list.source_manual"),
        }
        .to_string()
    }
}
#[derive(Clone, Debug)]
pub(crate) struct JavaOption {
    pub(crate) path: Option<std::path::PathBuf>,
    pub(crate) vendor: Option<String>,
    pub(crate) version: String,
    pub(crate) source: JavaSource,
    pub(crate) selectable: bool,
}
impl JavaOption {
    fn row_key(&self) -> String {
        match &self.source {
            JavaSource::Automatic => "java-row-auto".into(),
            JavaSource::Vendor(id) => format!("java-owned-{id}"),
            JavaSource::Mojang(component) => format!("prefs-runtime-{component}"),
            JavaSource::System => format!("java-system-{}", self.path.as_ref().unwrap().display()),
            JavaSource::Manual => format!("java-manual-{}", self.path.as_ref().unwrap().display()),
        }
    }
    fn row_id(&self) -> gpui_kit::ElementId {
        gpui_kit::ElementId::Name(gpui_kit::SharedString::from(self.row_key()))
    }
    pub(crate) fn title(&self) -> String {
        if self.path.is_none() {
            return t!("instance_settings.java_auto").to_string();
        }
        match &self.vendor {
            Some(vendor) => format!("{vendor} {}", self.version),
            None if self.version.is_empty() => t!("java_list.local_java").to_string(),
            None => format!("Java {}", self.version),
        }
    }
}

/// Shared standard Radio composition: one readable title, secondary path and
/// neutral source. A global-only command slot cannot activate the radio.
pub(crate) fn runtime_radio(
    option: &JavaOption,
    action: Option<gpui_kit::AnyElement>,
    cx: &App,
) -> gpui_kit::component::radio::Radio {
    use gpui_kit::{IntoElement as _, MouseButton};
    let title = option.title();
    let path = option.path.as_ref().map(|p| p.display().to_string());
    let content = v_flex()
        .id(option.row_id())
        .test_support()
        .aria_label(title.clone())
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(
            h_flex()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_ellipsis()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(title.clone()),
                )
                .children(option.path.as_ref().map(|_| {
                    div()
                        .id(gpui_kit::SharedString::from(format!(
                            "{}-source",
                            option.row_key()
                        )))
                        .test_support()
                        .aria_label(option.source.label())
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(option.source.label())
                })),
        )
        .children(path.as_ref().map(|path| {
            let full_path = path.clone();
            div()
                .id(gpui_kit::SharedString::from(format!(
                    "{}-path",
                    option.row_key()
                )))
                .test_support()
                .aria_label(path.clone())
                .tooltip(move |window, cx| {
                    let wrapped = full_path.replace('/', "/\u{200b}");
                    gpui_kit::component::tooltip::Tooltip::element(move |_, _| {
                        div().w(gpui_kit::rems(32.)).child(wrapped.clone())
                    })
                    .build(window, cx)
                })
                .text_sm()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(cx.theme().muted_foreground)
                .child(path.clone())
        }));
    let row = h_flex()
        .items_start()
        .w_full()
        .min_w_0()
        .gap_2()
        .child(content)
        .child(
            div()
                .id("java-command-slot")
                .w_6()
                .flex_none()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(|_, _, cx| cx.stop_propagation())
                .children(action),
        );
    gpui_kit::component::radio::Radio::new("java")
        .w_full()
        .min_w_0()
        .py_1()
        .disabled(!option.selectable)
        .accessibility_label(path.as_ref().map_or_else(
            || title.clone(),
            |path| format!("{title} · {} · {path}", option.source.label()),
        ))
        .child(row.into_any_element())
}
