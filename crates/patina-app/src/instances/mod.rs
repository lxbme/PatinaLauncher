//! The instances page (layout A): the instance list on the left, the
//! selected instance's detail on the right ([`InstancesPage`]).
//!
//! [`InstancesModel`] owns the domain state (the list, the selection, run
//! states); the list column ([`InstanceList`]) and the detail
//! ([`DetailView`]) are views that read it. With nothing to list the right
//! side is the empty state; a broken entry shows its error and lets the
//! user open or delete its folder.

mod broken;
mod detail;
mod empty;
mod icon;
mod icon_picker;
mod list;
mod model;
mod overview;

use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext, Context, ElementId, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement, IntoElement, ParentElement, Rems, Render, Styled, Subscription, Window,
    actions, div, rems,
};
use patina_core::instance::InstanceConfig;
use rust_i18n::t;

use crate::fonts::Strong;
use crate::layout::TEXT_PRIMARY;
use crate::theme::ProductColors;

pub use detail::{DetailTab, DetailView};
pub use icon::{
    BUILTIN_ICONS, IconChoice, IconSource, asset_bytes, forget_cached_image, icon_for,
    instance_icon, save_icon, tint_for,
};
pub use icon_picker::{IconPicked, IconPicker, is_supported_upload, pick_id};
pub use list::{Filter, InstanceList};
pub use model::{Entry, InstancesChanged, InstancesModel, RunState};
pub use overview::OverviewStats;

actions!(
    patina,
    [
        /// Opens the add-instance dialog (plan 4b task 9). Dispatched by the
        /// list's ＋ and the empty state's button.
        AddInstance
    ]
);

/// Key context of the instances page.
pub const INSTANCES_CONTEXT: &str = "Instances";

/// Icon tile of the detail headers (healthy and broken), 64 px: the size of
/// the built-in PNGs at scale 1.0.
pub(crate) const HEADER_ICON: Rems = Rems(4.);
/// Title size of the detail headers (22 px), also the letter on their tile.
pub(crate) const HEADER_TITLE: Rems = Rems(1.375);

/// Binds the instances page's keys: Up / Down move the selection while the
/// instance list has the keyboard.
pub fn init(cx: &mut gpui_kit::App) {
    list::init(cx);
    icon_picker::init(cx);
}

/// Element id of the list row of entry `id` (healthy or broken).
pub fn row_id(id: &str) -> ElementId {
    ElementId::Name(format!("instance-row-{id}").into())
}

/// The instances page (an entity): owns the model and the two views.
pub struct InstancesPage {
    model: Entity<InstancesModel>,
    list: Entity<InstanceList>,
    detail: Entity<DetailView>,
    /// The page's place in the dispatch tree: its buttons dispatch
    /// [`AddInstance`] here, whatever has the keyboard.
    focus: FocusHandle,
    _observe: Subscription,
}

impl InstancesPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = cx.new(|cx| InstancesModel::new(window, cx));
        let focus = cx.focus_handle();
        let _observe = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            list: cx.new(|cx| InstanceList::new(model.clone(), focus.clone(), cx)),
            detail: cx.new(|cx| DetailView::new(model.clone(), cx)),
            model,
            focus,
            _observe,
        }
    }

    pub fn model(&self) -> &Entity<InstancesModel> {
        &self.model
    }

    pub fn list(&self) -> &Entity<InstanceList> {
        &self.list
    }

    pub fn detail(&self) -> &Entity<DetailView> {
        &self.detail
    }

    /// Stub until the add-instance dialog exists (plan 4b task 9): nothing
    /// opens yet, so the action goes on to app-level listeners.
    fn add_instance(&mut self, _: &AddInstance, _: &mut Window, cx: &mut Context<Self>) {
        cx.propagate();
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let model = self.model.read(cx);
        if !model.is_loaded() {
            return div().flex_1().into_any_element();
        }
        match model.selected_entry() {
            Some(Entry::Healthy(_)) => self.detail.clone().into_any_element(),
            Some(Entry::Broken(b)) => {
                broken::BrokenDetail::new(b.clone(), self.model.clone(), model.is_deleting(&b.id))
                    .into_any_element()
            }
            None => match model.error() {
                Some(error) => empty::ListError::new(error.clone()).into_any_element(),
                None => empty::EmptyState::new(self.focus.clone()).into_any_element(),
            },
        }
    }
}

impl Render for InstancesPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context(INSTANCES_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::add_instance))
            .size_full()
            // Full-height columns.
            .items_stretch()
            .child(self.list.clone())
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_main(cx)),
            )
    }
}

/// The loader's name as the list and the detail show it: the loader kind,
/// or "原版" for plain Minecraft.
pub(crate) fn loader_name(config: &InstanceConfig) -> String {
    match config.loader() {
        Some((kind, _)) => kind.display_name(),
        None => t!("instances.vanilla").to_string(),
    }
}

/// The loader with its version ("Fabric 0.16.5"), or "原版".
pub(crate) fn loader_full(config: &InstanceConfig) -> String {
    match config.loader() {
        Some((kind, component)) => format!("{} {}", kind.display_name(), component.version),
        None => t!("instances.vanilla").to_string(),
    }
}

/// The loader's tag colour.
pub(crate) fn loader_color(config: &InstanceConfig, cx: &gpui_kit::App) -> Hsla {
    ProductColors::get(cx).loader(config.loader().map(|(kind, _)| kind))
}

/// Seconds since the Unix epoch, now.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How long ago `then` was, relative to `now` (both Unix seconds): "刚刚",
/// "5 分钟前", "3 小时前", "2 天前", or the date after 30 days.
pub(crate) fn time_ago(then: u64, now: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let ago = now.saturating_sub(then);
    if ago < MINUTE {
        t!("instances.time.just_now").to_string()
    } else if ago < HOUR {
        t!("instances.time.minutes_ago", count = ago / MINUTE).to_string()
    } else if ago < DAY {
        t!("instances.time.hours_ago", count = ago / HOUR).to_string()
    } else if ago < 30 * DAY {
        t!("instances.time.days_ago", count = ago / DAY).to_string()
    } else {
        date(then)
    }
}

/// The local date of `secs` (Unix seconds), `2026-10-30`.
pub(crate) fn date(secs: u64) -> String {
    use chrono::{Local, TimeZone};
    i64::try_from(secs)
        .ok()
        .and_then(|s| Local.timestamp_opt(s, 0).single())
        .map_or_else(String::new, |d| d.format("%Y-%m-%d").to_string())
}

/// A rounded tile on a faint fill of `color` (icons, broken entries).
pub(crate) fn tile(color: Hsla, size: Rems, cx: &gpui_kit::App) -> gpui_kit::Div {
    // The letter scales with the tile: the header's size on a header tile,
    // 13 px on the list's 28 px tile.
    let text = if size.0 >= HEADER_ICON.0 {
        HEADER_TITLE
    } else {
        TEXT_PRIMARY
    };
    div()
        .flex_none()
        .size(size)
        .flex()
        .items_center()
        .justify_center()
        .rounded(rems(size.0 * 0.22))
        .bg(color.opacity(0.16))
        .border_1()
        .border_color(color.opacity(0.22))
        .text_size(text)
        .strong(FontWeight::SEMIBOLD, cx)
        .text_color(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_ago_reads_naturally() {
        let _locale = crate::i18n::locale_lock();
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        let now = 1_800_000_000;
        assert_eq!(time_ago(now - 10, now), "刚刚");
        assert_eq!(time_ago(now + 100, now), "刚刚", "a clock in the future");
        assert_eq!(time_ago(now - 5 * 60, now), "5 分钟前");
        assert_eq!(time_ago(now - 3 * 3600, now), "3 小时前");
        assert_eq!(time_ago(now - 2 * 86400, now), "2 天前");
        assert_eq!(time_ago(now - 40 * 86400, now), date(now - 40 * 86400));
        assert_eq!(date(now).len(), "2027-01-15".len());
    }
}
