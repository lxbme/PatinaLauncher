//! The pre-launch warnings: a prepared launch with warnings waits here for
//! "继续启动" or "取消".

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::v_flex;
use gpui_kit::{
    App, Entity, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, TestSupportExt, Window, div,
};
use rust_i18n::t;

use crate::instances::InstancesModel;

/// Element id of the `index`th warning in the dialog.
pub fn warning_id(index: usize) -> gpui_kit::ElementId {
    ("prelaunch-warning", index).into()
}

/// Lists the warnings of `id`'s prepared launch. "继续启动" starts it; "取消",
/// Escape or any other way of closing the dialog cancels it, which releases
/// its locks. Does nothing when no launch of `id` waits.
pub fn open(model: Entity<InstancesModel>, id: String, window: &mut Window, cx: &mut App) {
    let (name, warnings) = {
        let m = model.read(cx);
        let name = m.display_name(&id).unwrap_or_else(|| id.clone());
        match m.pending_warnings(&id, cx) {
            Some(warnings) => (name, warnings),
            None => return,
        }
    };
    window.open_alert_dialog(cx, move |alert, _, _| {
        let title = SharedString::from(t!("launch.prelaunch.title", name = name));
        let list = v_flex()
            .gap_1()
            .children(warnings.iter().enumerate().map(|(i, text)| {
                let text = SharedString::from(text.clone());
                div()
                    .id(warning_id(i))
                    .aria_label(text.clone())
                    .test_support()
                    .child(text)
            }));
        let (confirm, cancel, close) = (model.clone(), model.clone(), model.clone());
        let (id_ok, id_cancel, id_close) = (id.clone(), id.clone(), id.clone());
        alert
            .title(
                div()
                    .id("prelaunch-title")
                    .aria_label(title.clone())
                    .test_support()
                    .child(title),
            )
            .description(list)
            .confirm()
            .ok_text(t!("launch.prelaunch.confirm").to_string())
            .cancel_text(t!("launch.prelaunch.cancel").to_string())
            .on_ok(move |_, _, cx| {
                confirm.update(cx, |m, cx| m.confirm_launch(&id_ok, cx));
                true
            })
            .on_cancel(move |_, _, cx| {
                cancel.update(cx, |m, cx| m.cancel_launch(&id_cancel, cx));
                true
            })
            // After either answer; a launch still waiting is cancelled.
            .on_close(move |_, _, cx| {
                close.update(cx, |m, cx| m.cancel_launch(&id_close, cx));
            })
    });
}
