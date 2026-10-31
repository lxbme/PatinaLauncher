//! Layout metrics shared by every region of the main window (layout A of
//! the prototype), at UI scale 1.0 and in rem so they follow the zoom.

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::{
    Div, IntoElement, ParentElement as _, Rems, ScrollHandle, StatefulInteractiveElement, Styled,
    div,
};

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

/// `content` as a vertical scroll region filling its parent, with the
/// theme's scrollbar over the region's right edge. `content` stays the
/// scroll owner (it keeps its id, so tests still find it); `scroll` is the
/// region's handle, kept by the owning view. The bar sits on a wrapper so it
/// does not scroll away with the content.
pub fn scroll_y<E>(content: E, scroll: &ScrollHandle) -> Div
where
    E: StatefulInteractiveElement + Styled + IntoElement + 'static,
{
    div()
        .relative()
        .size_full()
        .child(content.size_full().overflow_y_scroll().track_scroll(scroll))
        .vertical_scrollbar(scroll)
}
