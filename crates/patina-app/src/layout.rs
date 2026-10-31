//! Layout metrics shared by every region of the main window (layout A of
//! the prototype), at UI scale 1.0 and in rem so they follow the zoom.

use gpui_kit::Rems;

/// Primary text (13 px).
pub const TEXT_PRIMARY: Rems = Rems(0.8125);
/// Secondary labels (12 px).
pub const TEXT_SECONDARY: Rems = Rems(0.75);
/// Row titles (14 px).
pub const TEXT_ROW: Rems = Rems(0.875);
/// Panel gutter (12 px).
pub const GUTTER: Rems = Rems(0.75);
/// Height of every panel's top strip (title bar, column headers, tab bar),
/// so their bottom dividers line up across columns.
pub const STRIP_H: Rems = Rems(2.5);
