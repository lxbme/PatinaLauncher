//! Behaviour shared by the forms' text fields.
//!
//! A multi-line text area takes Tab as indentation, which would trap the
//! keyboard in it. The JVM-argument fields have no use for tab characters,
//! so inside one wrapped with [`tab_leaves`] Tab and Shift-Tab move the
//! keyboard to the next and previous control (the field then loses focus
//! and commits).

use gpui_kit::{App, InteractiveElement, KeyBinding, actions};

actions!(patina, [FocusNextField, FocusPrevField]);

/// Key context of a field whose Tab leaves it.
pub const TAB_LEAVES: &str = "TabLeaves";

/// Binds Tab and Shift-Tab inside [`tab_leaves`] fields. They outrank the
/// text area's own indentation keys: same depth, bound later.
pub fn init(cx: &mut App) {
    let inside = format!("{TAB_LEAVES} > Input");
    cx.bind_keys([
        KeyBinding::new("tab", FocusNextField, Some(&inside)),
        KeyBinding::new("shift-tab", FocusPrevField, Some(&inside)),
    ]);
}

/// `field` (around a text area) with Tab and Shift-Tab moving the keyboard
/// on instead of indenting.
pub fn tab_leaves<E: InteractiveElement>(field: E) -> E {
    field
        .key_context(TAB_LEAVES)
        .on_action(|_: &FocusNextField, window, cx| window.focus_next(cx))
        .on_action(|_: &FocusPrevField, window, cx| window.focus_prev(cx))
}
