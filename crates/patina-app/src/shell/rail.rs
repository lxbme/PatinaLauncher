//! The left rail (48 px at scale 1.0): instances, downloads and accounts at
//! the top, settings at the bottom. The page on show has a raised fill and
//! an accent bar on the rail's left edge, as in the prototype.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Icon, IconName, Selectable, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    Context, Entity, InteractiveElement, IntoElement, ParentElement, Rems, Render, Styled,
    Subscription, TestSupportExt, Window, div, rems,
};

use super::{Nav, ShellState};

/// Rail width at UI scale 1.0 (48 px).
pub const RAIL_W: Rems = Rems(3.);
/// Height of one rail slot, and of the button inside it.
const SLOT_H: Rems = Rems(2.5);
const BUTTON: Rems = Rems(2.25);

/// The rail; reads and changes the page through [`ShellState`].
pub struct Rail {
    shell: Entity<ShellState>,
    _observe: Subscription,
}

impl Rail {
    pub fn new(shell: Entity<ShellState>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&shell, |_, _, cx| cx.notify());
        Self { shell, _observe }
    }

    fn render_item(
        &self,
        nav: Nav,
        icon: impl Into<Icon>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let selected = self.shell.read(cx).nav() == nav;
        let label = nav.label();
        // The prototype's look on a real Button (focus, keyboard, a11y):
        // muted at rest, a faint raised fill on hover, raised when selected.
        let variant = ButtonCustomVariant::new(cx)
            .foreground(if selected {
                theme.foreground
            } else {
                theme.muted_foreground
            })
            .hover(theme.muted.opacity(0.55))
            .active(theme.muted);
        let shell = self.shell.clone();
        div()
            .relative()
            .w_full()
            .h(SLOT_H)
            .flex()
            .items_center()
            .justify_center()
            .when(selected, |d| {
                d.child(
                    div()
                        .id("rail-selection")
                        .absolute()
                        .left_0()
                        .top(rems(0.75))
                        .h(rems(1.))
                        .w(rems(0.1875))
                        .rounded_r(rems(0.125))
                        .bg(theme.primary)
                        .test_support(),
                )
            })
            .child(
                Button::new(nav.rail_id())
                    .custom(variant)
                    .selected(selected)
                    .size(BUTTON)
                    .p_0()
                    .tooltip(label.clone())
                    .accessibility_label(label)
                    .child(icon.into().size(rems(1.125)))
                    .on_click(move |_, _, cx| shell.update(cx, |s, cx| s.navigate(nav, cx))),
            )
    }
}

impl Render for Rail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, border) = (theme.sidebar, theme.sidebar_border);
        v_flex()
            .flex_none()
            .w(RAIL_W)
            .h_full()
            .py_1()
            .bg(bg)
            .border_r_1()
            .border_color(border)
            .child(self.render_item(Nav::Instances, Lucide::Boxes, cx))
            .child(self.render_item(Nav::Downloads, Lucide::Download, cx))
            .child(self.render_item(Nav::Accounts, IconName::CircleUser, cx))
            .child(div().flex_1())
            .child(self.render_item(Nav::Settings, IconName::Settings, cx))
    }
}
