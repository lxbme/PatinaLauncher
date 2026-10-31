//! [`IconPicker`]: the built-in icons in a grid, "上传图片…" and "使用首字母".
//!
//! Keyboard: the grid is one tab stop. Left / Right / Up / Down move the
//! cursor (a focus ring on one cell, shown while the keyboard drives it),
//! Enter and Space pick the cell under it. Each cell has the icon's file
//! stem as its tooltip and accessible label.
//!
//! The current choice is marked: a built-in icon by a primary border on its
//! cell, an uploaded image by "上传图片…" being selected, the letter tile by
//! "使用首字母" being selected.
//!
//! The picker saves nothing: it emits [`IconPicked`] and its owner (the
//! add-instance dialog, the instance settings) saves, then shows a failure
//! with [`IconPicker::set_error`].

use std::path::Path;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Selectable, Sizable, h_flex, v_flex};
use gpui_kit::{
    App, Context, EventEmitter, FocusHandle, InteractiveElement, IntoElement, KeyBinding,
    ParentElement, PathPromptOptions, Rems, Render, SharedString, StatefulInteractiveElement,
    Styled, TestSupportExt, Window, actions, div, rems,
};
use rust_i18n::t;

use super::icon::{BUILTIN_ICONS, IconChoice, IconSource, instance_icon};
use crate::layout::TEXT_SECONDARY;

/// Emitted when the user picks an icon (including a file to upload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconPicked(pub IconChoice);

actions!(
    patina,
    [
        /// Moves the icon grid's cursor one cell left.
        IconLeft,
        /// Moves the icon grid's cursor one cell right.
        IconRight,
        /// Moves the icon grid's cursor one row up.
        IconUp,
        /// Moves the icon grid's cursor one row down.
        IconDown,
        /// Picks the icon under the grid's cursor.
        IconConfirm
    ]
);

/// Key context of the icon grid.
pub const GRID_CONTEXT: &str = "IconGrid";

/// Binds the grid's keys.
pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("left", IconLeft, Some(GRID_CONTEXT)),
        KeyBinding::new("right", IconRight, Some(GRID_CONTEXT)),
        KeyBinding::new("up", IconUp, Some(GRID_CONTEXT)),
        KeyBinding::new("down", IconDown, Some(GRID_CONTEXT)),
        KeyBinding::new("enter", IconConfirm, Some(GRID_CONTEXT)),
        KeyBinding::new("space", IconConfirm, Some(GRID_CONTEXT)),
    ]);
}

/// Icons per grid row.
const COLUMNS: usize = 8;
/// Icon side: 32 px at scale 1.0, half the 64 px source.
const ICON: Rems = Rems(2.);
/// A grid cell: the icon, 0.25 rem around it, and the 1px selection border
/// (borders are inside the size).
const CELL: Rems = Rems(2.5);

/// Image extensions the upload accepts (the file dialog may not filter).
const UPLOAD_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp"];

/// Whether `path` names an image the upload accepts.
pub fn is_supported_upload(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        UPLOAD_EXTENSIONS
            .iter()
            .any(|ok| e.eq_ignore_ascii_case(ok))
    })
}

/// Element id of the grid cell of built-in icon `name`.
pub fn pick_id(name: &str) -> gpui_kit::ElementId {
    gpui_kit::ElementId::Name(format!("icon-pick-{name}").into())
}

/// The cell `step` rows and `across` columns from `at`, or `at` when that
/// would leave the grid.
fn step_cursor(at: usize, across: isize, down: isize) -> usize {
    let (row, col) = ((at / COLUMNS) as isize, (at % COLUMNS) as isize);
    let (row, col) = (row + down, col + across);
    let next = row * COLUMNS as isize + col;
    if row < 0 || !(0..COLUMNS as isize).contains(&col) || next >= BUILTIN_ICONS.len() as isize {
        at
    } else {
        next as usize
    }
}

/// See the module docs.
pub struct IconPicker {
    /// The choice marked as current.
    current: IconChoice,
    /// What `current` was before the last pick, restored when saving that
    /// pick failed ([`Self::set_error`]).
    before_pick: IconChoice,
    /// The grid's cursor, an index into [`BUILTIN_ICONS`].
    cursor: usize,
    focus: FocusHandle,
    error: Option<SharedString>,
}

impl EventEmitter<IconPicked> for IconPicker {}

impl IconPicker {
    /// A picker marking `current`: [`IconChoice::None`] is the letter tile.
    pub fn new(current: IconChoice, cx: &mut Context<Self>) -> Self {
        let cursor = match &current {
            IconChoice::Builtin(name) => BUILTIN_ICONS
                .iter()
                .position(|known| known == name)
                .unwrap_or(0),
            _ => 0,
        };
        Self {
            before_pick: current.clone(),
            current,
            cursor,
            focus: cx.focus_handle().tab_stop(true),
            error: None,
        }
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    /// The choice marked as current.
    pub fn current(&self) -> &IconChoice {
        &self.current
    }

    /// Name of the icon under the grid's cursor.
    pub fn cursor_name(&self) -> &'static str {
        BUILTIN_ICONS[self.cursor]
    }

    /// Shows why the last pick could not be saved (and marks the choice
    /// from before that pick again), or with `None` clears the message.
    pub fn set_error(&mut self, error: Option<SharedString>, cx: &mut Context<Self>) {
        if error.is_some() {
            self.current = self.before_pick.clone();
        }
        self.error = error;
        cx.notify();
    }

    fn pick(&mut self, choice: IconChoice, cx: &mut Context<Self>) {
        self.error = None;
        self.before_pick = std::mem::replace(&mut self.current, choice.clone());
        cx.emit(IconPicked(choice));
        cx.notify();
    }

    fn move_cursor(&mut self, across: isize, down: isize, cx: &mut Context<Self>) {
        self.cursor = step_cursor(self.cursor, across, down);
        cx.notify();
    }

    fn upload(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let result = chosen.await;
            this.update(cx, |this, cx| match result {
                Ok(Ok(Some(paths))) => {
                    let Some(path) = paths.into_iter().next() else {
                        return;
                    };
                    if is_supported_upload(&path) {
                        this.pick(IconChoice::Custom(path), cx);
                    } else {
                        this.set_error(
                            Some(t!("instances.icon.unsupported").to_string().into()),
                            cx,
                        );
                    }
                }
                Ok(Ok(None)) | Err(_) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "the file picker failed");
                    this.set_error(
                        Some(t!("instances.icon.picker_failed").to_string().into()),
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Render for IconPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (primary, hover, danger, ring, clear) = (
            theme.primary,
            theme.accent,
            theme.danger,
            theme.ring,
            theme.transparent,
        );
        // The ring follows the keyboard only (focus-visible).
        let keyboard = self.focus.is_focused(window) && window.last_input_was_keyboard();
        let cells = BUILTIN_ICONS.iter().enumerate().map(|(at, name)| {
            let selected = self.current == IconChoice::Builtin((*name).to_owned());
            let choice = IconChoice::Builtin((*name).to_owned());
            let label = SharedString::from(*name);
            let border = if keyboard && at == self.cursor {
                ring
            } else if selected {
                primary
            } else {
                clear
            };
            div()
                .id(pick_id(name))
                .test_support()
                .flex_none()
                .size(CELL)
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .border_1()
                .border_color(border)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .aria_label(label.clone())
                .aria_selected(selected)
                .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.cursor = at;
                    this.focus.focus(window, cx);
                    this.pick(choice.clone(), cx);
                }))
                .child(instance_icon(&IconSource::Builtin(name), ICON, cx))
        });
        v_flex()
            .gap(rems(0.75))
            .child(
                h_flex()
                    .id("icon-grid")
                    .test_support()
                    .key_context(GRID_CONTEXT)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(|this, _: &IconLeft, _, cx| this.move_cursor(-1, 0, cx)))
                    .on_action(cx.listener(|this, _: &IconRight, _, cx| this.move_cursor(1, 0, cx)))
                    .on_action(cx.listener(|this, _: &IconUp, _, cx| this.move_cursor(0, -1, cx)))
                    .on_action(cx.listener(|this, _: &IconDown, _, cx| this.move_cursor(0, 1, cx)))
                    .on_action(cx.listener(|this, _: &IconConfirm, _, cx| {
                        let name = this.cursor_name();
                        this.pick(IconChoice::Builtin(name.to_owned()), cx);
                    }))
                    .flex_wrap()
                    .w(rems(CELL.0 * COLUMNS as f32))
                    .children(cells),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("icon-upload")
                            .outline()
                            .small()
                            .selected(matches!(self.current, IconChoice::Custom(_)))
                            .label(t!("instances.icon.upload").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.upload(cx))),
                    )
                    .child(
                        Button::new("icon-letter")
                            .ghost()
                            .small()
                            .selected(self.current == IconChoice::None)
                            .label(t!("instances.icon.letter").to_string())
                            .on_click(
                                cx.listener(|this, _, _, cx| this.pick(IconChoice::None, cx)),
                            ),
                    ),
            )
            .children(self.error.clone().map(|error| {
                div()
                    .id("icon-error")
                    .test_support()
                    .text_size(TEXT_SECONDARY)
                    .text_color(danger)
                    .child(error)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_steps_stay_in_the_grid() {
        // 24 icons in rows of 8.
        assert_eq!(step_cursor(0, -1, 0), 0);
        assert_eq!(step_cursor(0, 0, -1), 0);
        assert_eq!(step_cursor(0, 1, 0), 1);
        assert_eq!(step_cursor(7, 1, 0), 7, "no wrap");
        assert_eq!(step_cursor(3, 0, 1), 11);
        assert_eq!(step_cursor(19, 0, 1), 19, "last row");
        assert_eq!(step_cursor(23, 1, 0), 23);
    }

    #[test]
    fn upload_accepts_only_images() {
        for ok in ["a.png", "a.PNG", "b.jpg", "c.jpeg", "d.webp", "/x/y.WebP"] {
            assert!(is_supported_upload(Path::new(ok)), "{ok}");
        }
        for bad in ["a.gif", "a.svg", "png", "a", "a.png.exe", ""] {
            assert!(!is_supported_upload(Path::new(bad)), "{bad}");
        }
    }
}
