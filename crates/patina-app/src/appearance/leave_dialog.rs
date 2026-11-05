use super::LeavePolicy;
use crate::{accounts::WindowAction, content::focus_first_button};
use gpui_kit::component::{
    WindowExt,
    button::{Button, ButtonVariants},
    dialog::DialogFooter,
};
use gpui_kit::{
    App, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    TestSupportExt, Window, div,
};
use rust_i18n::t;
use std::{cell::Cell, rc::Rc};

/// Returns false if another decision already owns the window; never stacks guards.
pub(crate) fn confirm_leave(
    policy: LeavePolicy,
    account: &str,
    confirm: Rc<WindowAction>,
    cancel: Rc<WindowAction>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if window.has_active_dialog(cx) {
        return false;
    }
    let focus = cx.focus_handle();
    let first = Cell::new(true);
    let title = if policy == LeavePolicy::WaitForApply {
        t!("appearance.applying").to_string()
    } else {
        t!("appearance.discard_title").to_string()
    };
    let body = match policy {
        LeavePolicy::WaitForApply => t!("appearance.wait_body").to_string(),
        LeavePolicy::ConfirmUncertain => {
            t!("appearance.leave_uncertain", name = account).to_string()
        }
        _ => t!("appearance.discard_body", name = account).to_string(),
    };
    window.open_alert_dialog(cx, move |alert, window, cx| {
        focus_first_button(&focus, &first, window, cx);
        let cancel_action = cancel.clone();
        let leave = confirm.clone();
        let footer = DialogFooter::new().child(
            Button::new("appearance-leave-stay")
                .label(t!("appearance.stay").to_string())
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    cancel_action(window, cx);
                }),
        );
        let footer = if policy == LeavePolicy::WaitForApply {
            footer
        } else {
            footer.child(
                Button::new("appearance-leave-discard")
                    .danger()
                    .label(if policy == LeavePolicy::ConfirmUncertain {
                        t!("appearance.leave_anyway").to_string()
                    } else {
                        t!("appearance.discard_leave").to_string()
                    })
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        leave(window, cx);
                    }),
            )
        };
        let cancel = cancel.clone();
        alert
            .title(title.clone())
            .description(
                div()
                    .id("appearance-leave-body")
                    .test_support()
                    .aria_label(SharedString::from(body.clone()))
                    .child(body.clone()),
            )
            .footer(div().w_full().track_focus(&focus).child(footer))
            .on_cancel(move |_, window, cx| {
                cancel(window, cx);
                true
            })
            // Enter is owned by the focused safe button, never implicit discard.
            .on_ok(|_, _, cx| {
                cx.propagate();
                false
            })
    });
    true
}
