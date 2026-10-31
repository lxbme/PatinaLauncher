//! The instance column: "实例 N" with ＋, the filter chips, then two-line
//! rows (icon, name, last played; loader · version). Broken entries come
//! after the healthy ones under "全部".

use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, Sizable, VirtualListScrollHandle, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, Context, Entity, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, Pixels, Rems, Render, ScrollStrategy, StatefulInteractiveElement,
    Styled, Subscription, TestSupportExt, Window, actions, div, px, rems, size,
};
use patina_core::instance::{BrokenInstance, Instance};
use patina_core::loader::LoaderKind;
use rust_i18n::t;

use super::model::{Entry, InstancesModel};
use super::{
    AddInstance, icon_for, instance_icon, loader_color, loader_name, now_secs, row_id, tile,
    time_ago,
};
use crate::fonts::Strong;
use crate::layout::{GUTTER, STRIP_H, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY};

/// Column width at UI scale 1.0 (392 px). Measured in the headless test
/// (`english_filter_chips_fit`): the six English chips with their padding
/// and gaps take 390 px including the gutters; Chinese takes less.
pub const LIST_W: Rems = Rems(24.5);
/// Row height (two lines), 48 px.
const ROW_H: Rems = Rems(3.);
/// Row icon, 32 px: half the 64 px built-in PNGs at scale 1.0.
const ICON: Rems = Rems(2.);
const CHIPS_H: Rems = Rems(2.25);

actions!(
    patina,
    [
        /// Selects the row above the selected one in the instance list.
        SelectPrev,
        /// Selects the row below the selected one in the instance list.
        SelectNext
    ]
);

/// Key context of the list's rows (inside the instances page's context).
pub const LIST_CONTEXT: &str = "InstanceList";

/// Binds Up / Down in the list's rows.
pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", SelectPrev, Some(LIST_CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(LIST_CONTEXT)),
    ]);
}

/// The row `step` rows away from `current` in `rows`, clamped to the ends;
/// with no current row (or one filtered out) the first row.
pub fn step_selection(rows: &[String], current: Option<&str>, step: isize) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let Some(at) = current.and_then(|id| rows.iter().position(|r| r == id)) else {
        return Some(0);
    };
    Some(at.saturating_add_signed(step).min(rows.len() - 1))
}

/// A filter chip of the list; view state, not saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Running,
    Vanilla,
    Loader(LoaderKind),
}

impl Filter {
    /// The chips, in order.
    pub const CHIPS: [Filter; 6] = [
        Filter::All,
        Filter::Running,
        Filter::Vanilla,
        Filter::Loader(LoaderKind::Fabric),
        Filter::Loader(LoaderKind::Forge),
        Filter::Loader(LoaderKind::Neoforge),
    ];

    /// Whether `instance` (running or not) passes the filter.
    pub fn accepts(self, instance: &Instance, running: bool) -> bool {
        let loader = instance.config.loader().map(|(kind, _)| kind);
        match self {
            Filter::All => true,
            Filter::Running => running,
            Filter::Vanilla => loader.is_none(),
            Filter::Loader(kind) => loader == Some(kind),
        }
    }

    /// Broken entries have no loader to filter by: only "全部" lists them.
    pub fn shows_broken(self) -> bool {
        self == Filter::All
    }

    pub fn label(self) -> String {
        match self {
            Filter::All => t!("instances.filter.all").to_string(),
            Filter::Running => t!("instances.filter.running").to_string(),
            Filter::Vanilla => t!("instances.vanilla").to_string(),
            Filter::Loader(kind) => kind.display_name(),
        }
    }

    /// Element id of the chip.
    pub fn chip_id(self) -> &'static str {
        match self {
            Filter::All => "filter-all",
            Filter::Running => "filter-running",
            Filter::Vanilla => "filter-vanilla",
            Filter::Loader(LoaderKind::Fabric) => "filter-fabric",
            Filter::Loader(LoaderKind::Forge) => "filter-forge",
            Filter::Loader(LoaderKind::Neoforge) => "filter-neoforge",
        }
    }
}

/// The ids of the entries `filter` lets through, in list order: healthy
/// instances, then (under "全部") the broken ones.
pub fn visible_ids(model: &InstancesModel, filter: Filter) -> Vec<String> {
    let healthy = model
        .instances()
        .iter()
        .filter(|i| filter.accepts(i, model.is_running(&i.id)))
        .map(|i| i.id.clone());
    let broken = model
        .broken()
        .iter()
        .filter(|_| filter.shows_broken())
        .map(|b| b.id.clone());
    healthy.chain(broken).collect()
}

/// The list column (an entity). Owns the filter; the list and the
/// selection belong to the model.
pub struct InstanceList {
    model: Entity<InstancesModel>,
    filter: Filter,
    /// The visible rows' ids, rebuilt when the model or the filter changes.
    rows: Rc<Vec<String>>,
    /// Where [`AddInstance`] is dispatched (the instances page).
    page_focus: FocusHandle,
    /// The rows region: a Tab stop that takes Up / Down.
    focus: FocusHandle,
    scroll: VirtualListScrollHandle,
    _observe: Subscription,
}

impl InstanceList {
    pub fn new(
        model: Entity<InstancesModel>,
        page_focus: FocusHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        let _observe = cx.observe(&model, |this, _, cx| this.rebuild_rows(cx));
        let mut this = Self {
            model,
            filter: Filter::All,
            rows: Rc::default(),
            page_focus,
            focus: cx.focus_handle().tab_stop(true),
            scroll: VirtualListScrollHandle::new(),
            _observe,
        };
        this.rebuild_rows(cx);
        this
    }

    pub fn filter(&self) -> Filter {
        self.filter
    }

    /// The ids of the rows on show, in order.
    pub fn rows(&self) -> &[String] {
        &self.rows
    }

    pub fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter;
            self.rebuild_rows(cx);
        }
    }

    /// The rows region's focus handle.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    /// Moves the selection `step` rows within the rows on show and scrolls
    /// the new row into view.
    fn step(&mut self, step: isize, cx: &mut Context<Self>) {
        let current = self.model.read(cx).selected();
        let Some(ix) = step_selection(&self.rows, current, step) else {
            return;
        };
        let id = self.rows[ix].clone();
        self.model.update(cx, |m, cx| m.select(&id, cx));
        self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let rows = visible_ids(self.model.read(cx), self.filter);
        if *self.rows != rows {
            self.rows = Rc::new(rows);
        }
        cx.notify();
    }

    fn render_header(&self, count: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let add = t!("instances.add").to_string();
        let focus = self.page_focus.clone();
        h_flex()
            .flex_none()
            .h(STRIP_H)
            .pl(GUTTER)
            .pr(rems(0.5))
            .justify_between()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_size(TEXT_PRIMARY)
                            .strong(FontWeight::SEMIBOLD, cx)
                            .text_color(theme.foreground)
                            .child(t!("instances.title").to_string()),
                    )
                    .child(
                        div()
                            .id("instances-count")
                            .text_size(TEXT_SECONDARY)
                            .text_color(theme.muted_foreground)
                            .aria_label(count.to_string())
                            .child(count.to_string())
                            .test_support(),
                    ),
            )
            .child(
                Button::new("instances-add")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::Plus))
                    .tooltip(add.clone())
                    .accessibility_label(add)
                    .on_click(move |_, window, cx| focus.dispatch_action(&AddInstance, window, cx)),
            )
    }

    fn render_chips(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let this = cx.weak_entity();
        h_flex()
            .id("instance-filters")
            .test_support()
            .flex_none()
            .h(CHIPS_H)
            .px(GUTTER)
            .gap_1()
            // At a large zoom or in a longer language the chips scroll
            // sideways rather than wrap.
            .overflow_x_scroll()
            .children(Filter::CHIPS.into_iter().map(|filter| {
                let selected = filter == self.filter;
                let variant = ButtonCustomVariant::new(cx)
                    .color(theme.transparent)
                    .foreground(theme.muted_foreground)
                    .hover(theme.muted)
                    .active(theme.list_active);
                let label = filter.label();
                let this = this.clone();
                Button::new(filter.chip_id())
                    .custom(variant)
                    .xsmall()
                    .label(label)
                    .toggled(selected)
                    .flex_none()
                    .h(rems(1.5))
                    .px(rems(0.375))
                    .rounded_full()
                    .border_1()
                    .text_size(TEXT_SECONDARY)
                    .map(|b| {
                        if selected {
                            b.border_color(theme.primary)
                                .text_color(theme.primary)
                                .strong(FontWeight::MEDIUM, cx)
                        } else {
                            b.border_color(theme.border)
                        }
                    })
                    .on_click(move |_, _, cx| {
                        this.update(cx, |list, cx| list.set_filter(filter, cx)).ok();
                    })
            }))
    }

    /// Row `id`; `ring` draws the keyboard focus ring on it when selected.
    fn render_row(&self, id: &str, ring: bool, cx: &App) -> AnyElement {
        let model = self.model.read(cx);
        let selected = model.selected() == Some(id);
        let select = {
            let model = self.model.clone();
            let focus = self.focus.clone();
            let id = id.to_string();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                // A clicked row hands the keyboard to the list.
                focus.focus(window, cx);
                model.update(cx, |m, cx| m.select(&id, cx));
            }
        };
        let content = match model.entry(id) {
            Some(Entry::Healthy(instance)) => {
                healthy_row(instance, model.is_running(id), cx).into_any_element()
            }
            Some(Entry::Broken(broken)) => broken_row(broken, cx).into_any_element(),
            None => div().into_any_element(),
        };
        let theme = cx.theme();
        let (hover, active) = (theme.list_hover, theme.list_active);
        let ring_color = if selected && ring {
            theme.ring
        } else {
            theme.transparent
        };
        h_flex()
            .id(row_id(id))
            .w_full()
            .h(ROW_H)
            .px(GUTTER)
            .gap_2p5()
            // Every row has the 1px border so the ring does not move the
            // content.
            .border_1()
            .border_color(ring_color)
            .when(selected, |r| r.bg(active))
            .when(!selected, |r| r.hover(move |s| s.bg(hover)))
            .aria_label(id.to_string())
            .aria_selected(selected)
            .on_click(select)
            .child(content)
            .test_support()
            .into_any_element()
    }
}

/// Two lines of a healthy instance: name and last played, then its loader
/// (coloured) and Minecraft version. A running instance gets a dot on its
/// icon and "运行中" in place of the last-played time.
fn healthy_row(instance: &Instance, running: bool, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let config = &instance.config;
    let icon = div()
        .relative()
        .flex_none()
        .child(instance_icon(&icon_for(instance), ICON, cx))
        .when(running, |d| {
            d.child(
                div()
                    .absolute()
                    .right(rems(-0.125))
                    .bottom(rems(-0.125))
                    .size(rems(0.625))
                    .rounded_full()
                    .border_2()
                    .border_color(theme.background)
                    .bg(theme.success),
            )
        });
    let corner = if running {
        h_flex()
            .flex_none()
            .gap_1()
            .text_size(TEXT_SECONDARY)
            .text_color(theme.success)
            .child(Icon::new(Lucide::Play).size(rems(0.625)))
            .child(t!("instances.running").to_string())
            .into_any_element()
    } else {
        div()
            .flex_none()
            .whitespace_nowrap()
            .text_size(TEXT_SECONDARY)
            .text_color(theme.muted_foreground)
            .children(config.last_played.map(|t| time_ago(t, now_secs())))
            .into_any_element()
    };
    h_flex().flex_1().min_w_0().gap_2p5().child(icon).child(
        v_flex()
            .flex_1()
            .min_w_0()
            // 1px between the two lines, as in the prototype.
            .gap(px(1.))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(name(config.name.clone(), cx))
                    .child(corner),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .whitespace_nowrap()
                    .text_size(TEXT_SECONDARY)
                    .child(
                        div()
                            .flex_none()
                            .text_color(loader_color(config, cx))
                            .child(loader_name(config)),
                    )
                    .child(div().text_color(theme.border).child("·"))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(theme.muted_foreground)
                            .child(config.mc_version().to_string()),
                    ),
            ),
    )
}

/// Two lines of a broken entry: its directory name, then "配置损坏".
fn broken_row(broken: &BrokenInstance, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .flex_1()
        .min_w_0()
        .gap_2p5()
        .child(
            tile(theme.warning, ICON, cx)
                .child(Icon::new(IconName::TriangleAlert).size(rems(0.875))),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                // 1px between the two lines, as in the prototype.
                .gap(px(1.))
                .child(name(broken.id.clone(), cx))
                .child(
                    div()
                        .text_size(TEXT_SECONDARY)
                        .text_color(theme.warning)
                        .child(t!("instances.broken.label").to_string()),
                ),
        )
}

fn name(text: String, cx: &App) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .text_size(TEXT_ROW)
        .text_color(cx.theme().foreground)
        .child(text)
}

impl Render for InstanceList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, border, muted) = (theme.background, theme.border, theme.muted_foreground);
        let model = self.model.read(cx);
        let count = model.instances().len();
        let no_match = model.is_loaded() && !model.is_empty() && self.rows.is_empty();
        let row_h: Pixels = ROW_H.to_pixels(window.rem_size());
        // Only the heights are used; the list measures the width itself.
        let sizes = Rc::new(vec![size(px(0.), row_h); self.rows.len()]);
        v_flex()
            .flex_none()
            .w(LIST_W)
            .h_full()
            .overflow_hidden()
            .bg(bg)
            .border_r_1()
            .border_color(border)
            .child(self.render_header(count, cx))
            .child(self.render_chips(cx))
            .when(no_match, |d| {
                d.child(
                    div()
                        .px(GUTTER)
                        .py_3()
                        .text_size(TEXT_SECONDARY)
                        .text_color(muted)
                        .child(t!("instances.no_match").to_string()),
                )
            })
            .child(
                v_flex()
                    .id("instance-rows-region")
                    // Before `track_focus`, so tests see the focus.
                    .test_support()
                    .key_context(LIST_CONTEXT)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::select_prev))
                    .on_action(cx.listener(Self::select_next))
                    .flex_1()
                    .min_h_0()
                    .pt_1()
                    .child(
                        v_virtual_list(
                            cx.entity(),
                            "instance-rows",
                            sizes,
                            |this, range, window, cx| {
                                // The selected row shows the ring only while the
                                // keyboard drives the list (focus-visible).
                                let ring = this.focus.is_focused(window)
                                    && window.last_input_was_keyboard();
                                let rows = this.rows.clone();
                                rows[range]
                                    .iter()
                                    .map(|id| this.render_row(id, ring, cx))
                                    .collect::<Vec<_>>()
                            },
                        )
                        .track_scroll(&self.scroll)
                        .flex_1()
                        .min_h_0(),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use patina_core::instance::{Component, ComponentUid, InstanceConfig};

    use super::*;

    fn instance(id: &str, loader: Option<ComponentUid>) -> Instance {
        let mut config = InstanceConfig {
            name: id.to_string(),
            components: vec![Component {
                uid: ComponentUid::Minecraft,
                version: "1.21.1".into(),
                profile: None,
            }],
            ..Default::default()
        };
        if let Some(uid) = loader {
            config.components.push(Component {
                uid,
                version: "1.0".into(),
                profile: None,
            });
        }
        Instance {
            id: id.to_string(),
            dir: PathBuf::from(id),
            config,
        }
    }

    #[test]
    fn steps_stay_within_the_rows() {
        let rows: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        assert_eq!(step_selection(&rows, Some("a"), -1), Some(0), "top stays");
        assert_eq!(step_selection(&rows, Some("a"), 1), Some(1));
        assert_eq!(step_selection(&rows, Some("c"), 1), Some(2), "bottom stays");
        assert_eq!(step_selection(&rows, Some("hidden"), 1), Some(0));
        assert_eq!(step_selection(&rows, None, -1), Some(0));
        assert_eq!(step_selection(&[], Some("a"), 1), None);
    }

    #[test]
    fn chips_filter_by_loader_and_running() {
        let vanilla = instance("v", None);
        let fabric = instance("f", Some(ComponentUid::Fabric));
        let forge = instance("g", Some(ComponentUid::Forge));
        let neo = instance("n", Some(ComponentUid::Neoforge));
        let all = [&vanilla, &fabric, &forge, &neo];
        let pass = |filter: Filter, running: &str| -> Vec<&str> {
            all.iter()
                .filter(|i| filter.accepts(i, i.id == running))
                .map(|i| i.id.as_str())
                .collect()
        };
        assert_eq!(pass(Filter::All, ""), ["v", "f", "g", "n"]);
        assert_eq!(pass(Filter::Vanilla, ""), ["v"]);
        assert_eq!(pass(Filter::Loader(LoaderKind::Fabric), ""), ["f"]);
        assert_eq!(pass(Filter::Loader(LoaderKind::Forge), ""), ["g"]);
        assert_eq!(pass(Filter::Loader(LoaderKind::Neoforge), ""), ["n"]);
        assert_eq!(pass(Filter::Running, ""), Vec::<&str>::new());
        assert_eq!(pass(Filter::Running, "g"), ["g"]);
        assert!(Filter::All.shows_broken());
        assert!(Filter::CHIPS[1..].iter().all(|f| !f.shows_broken()));
    }
}
