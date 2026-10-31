//! Asks before force-stopping a game: `强制关闭"<名称>"？`.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::{
    App, Entity, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement,
    TestSupportExt, Window, div,
};
use rust_i18n::t;

use crate::instances::InstancesModel;

/// Asks whether to force-stop `id`'s game; "强制关闭" stops it through the
/// model, "取消" leaves it running.
pub fn open(model: Entity<InstancesModel>, id: String, window: &mut Window, cx: &mut App) {
    let name = model
        .read(cx)
        .display_name(&id)
        .unwrap_or_else(|| id.clone());
    window.open_alert_dialog(cx, move |alert, _, _| {
        let title = SharedString::from(t!("launch.stop.title", name = name));
        let (model, id) = (model.clone(), id.clone());
        alert
            .title(
                div()
                    .id("stop-title")
                    .aria_label(title.clone())
                    .test_support()
                    .child(title),
            )
            .description(t!("launch.stop.body").to_string())
            .confirm()
            .ok_text(t!("launch.stop.confirm").to_string())
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("launch.stop.cancel").to_string())
            .on_ok(move |_, _, cx| {
                model.update(cx, |m, cx| m.force_stop(&id, cx));
                true
            })
    });
}
