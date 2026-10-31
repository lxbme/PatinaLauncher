//! The detail of a broken entry: the core's error, "打开文件夹", and
//! "删除…" behind a confirmation.

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt, h_flex, v_flex,
};
use gpui_kit::{
    App, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div, rems,
};
use patina_core::instance::BrokenInstance;
use rust_i18n::t;

use super::model::InstancesModel;
use super::{HEADER_ICON, HEADER_TITLE, tile};
use crate::fonts::Strong;
use crate::layout::{GUTTER, TEXT_PRIMARY, TEXT_SECONDARY};

/// See the module docs. `deleting` disables "删除…" while a deletion runs.
#[derive(IntoElement)]
pub struct BrokenDetail {
    broken: BrokenInstance,
    model: Entity<InstancesModel>,
    deleting: bool,
}

impl BrokenDetail {
    pub fn new(broken: BrokenInstance, model: Entity<InstancesModel>, deleting: bool) -> Self {
        Self {
            broken,
            model,
            deleting,
        }
    }
}

/// Asks before deleting `broken`'s folder; on 删除 deletes it through the
/// model and reports a failure as a notification.
fn confirm_delete(
    broken: BrokenInstance,
    model: Entity<InstancesModel>,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let title = SharedString::from(t!("instances.broken.delete_title", id = broken.id));
        let (broken, model) = (broken.clone(), model.clone());
        alert
            .title(
                div()
                    .id("delete-title")
                    .aria_label(title.clone())
                    .child(title)
                    .test_support(),
            )
            .description(t!("instances.broken.delete_body").to_string())
            .confirm()
            .ok_text(t!("instances.broken.delete_confirm").to_string())
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("instances.broken.cancel").to_string())
            .on_ok(move |_, window, cx| {
                let id = broken.id.clone();
                let job = model.update(cx, |m, cx| m.delete_broken(broken.clone(), cx));
                window
                    .spawn(cx, async move |cx| {
                        if let Err(error) = job.await {
                            let text = t!("instances.broken.delete_failed", id = id, error = error);
                            cx.update(|window, cx| {
                                window.push_notification(Notification::error(text.to_string()), cx);
                            })
                            .ok();
                        }
                    })
                    .detach();
                true
            })
    });
}

impl RenderOnce for BrokenDetail {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let error = SharedString::from(self.broken.error.to_string());
        let dir = self.broken.dir.clone();
        let (broken, model) = (self.broken.clone(), self.model);
        v_flex()
            .id("broken-detail")
            .test_support()
            .size_full()
            .child(
                h_flex()
                    .flex_none()
                    .px(GUTTER)
                    .pt(rems(1.125))
                    .pb(rems(1.))
                    .gap(rems(0.875))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        tile(theme.warning, HEADER_ICON, cx)
                            .child(Icon::new(IconName::TriangleAlert).size(rems(1.5))),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(rems(0.375))
                            .child(
                                div()
                                    .text_size(HEADER_TITLE)
                                    .line_height(rems(1.75))
                                    .strong(FontWeight::SEMIBOLD, cx)
                                    .text_color(theme.foreground)
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(self.broken.id.clone()),
                            )
                            .child(
                                div()
                                    .text_size(TEXT_SECONDARY)
                                    .text_color(theme.warning)
                                    .child(t!("instances.broken.label").to_string()),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .px(GUTTER)
                    .py_3()
                    .gap_3()
                    .child(
                        div()
                            .id("broken-error")
                            .text_size(TEXT_PRIMARY)
                            .text_color(theme.foreground)
                            .aria_label(error.clone())
                            .child(error)
                            .test_support(),
                    )
                    .child(
                        div()
                            .text_size(TEXT_SECONDARY)
                            .text_color(theme.muted_foreground)
                            .child(self.broken.dir.display().to_string()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("broken-open-folder")
                                    .outline()
                                    .small()
                                    .icon(Icon::new(IconName::FolderOpen))
                                    .label(t!("common.open_folder").to_string())
                                    .on_click(move |_, _, cx| cx.open_with_system(&dir)),
                            )
                            .child(
                                Button::new("broken-delete")
                                    .danger()
                                    .outline()
                                    .small()
                                    .label(t!("instances.broken.delete").to_string())
                                    .disabled(self.deleting)
                                    .loading(self.deleting)
                                    .on_click(move |_, window, cx| {
                                        confirm_delete(broken.clone(), model.clone(), window, cx)
                                    }),
                            ),
                    ),
            )
    }
}
