//! The main window's frame: title bar, left rail, status bar and the page
//! in between ([`RootView`]), and which page is on show ([`ShellState`]).
//!
//! Layout A of the prototype (`spikes/gpui-component`): every top strip is
//! [`STRIP_H`](crate::layout::STRIP_H) high so the 1px dividers under them
//! line up across columns, no card boxes, tabular figures throughout.

mod nav;
pub mod placeholder;
mod rail;
mod root;
mod status_bar;
mod title_bar;

use gpui_kit::{App, KeyBinding};

pub use crate::disk_usage::format_size;
pub use nav::{Nav, ShellState};
pub use rail::Rail;
pub use root::{FocusSearch, ROOT_CONTEXT, RootView};
pub use status_bar::{InstanceUsage, SCAN_INTERVAL, StatusBar};
pub use title_bar::{AccountLabel, TitleBarView};

/// Binds the shell's keys: `/` focuses the search unless a text input has
/// the keyboard.
pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "/",
        FocusSearch,
        Some(&format!("{ROOT_CONTEXT} && !Input")),
    )]);
}
