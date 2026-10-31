//! The instances page (layout A): the instance list on the left, the
//! selected instance's detail on the right ([`InstancesPage`]).
//!
//! [`InstancesModel`] owns the domain state (the list, the selection) and
//! the [`InstanceActivity`] (run states, operations, logs); the list
//! column ([`InstanceList`]) and the detail ([`DetailView`]) are views that
//! read them. With nothing to list the right side is the empty state; a
//! broken entry shows its error and lets the user open or delete its
//! folder.

mod activity;
mod add_dialog;
mod broken;
mod detail;
mod empty;
mod icon;
mod icon_picker;
mod list;
mod model;
mod overview;
mod settings_tab;
mod version_picker;
mod versions_tab;

use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::component::notification::Notification;
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext, Context, ElementId, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Rems, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, TestSupportExt as _, Window, actions, div,
    rems,
};
use patina_core::instance::InstanceConfig;
use rust_i18n::t;

use crate::fonts::Strong;
use crate::launch::{LaunchEvent, prelaunch_dialog};
use crate::layout::TEXT_PRIMARY;
use crate::theme::ProductColors;

pub(crate) use activity::observe_activity;
pub use activity::{ActivityEvent, InstanceActivity};
pub use add_dialog::{
    AddInstanceForm, FormChanged, NewInstance, create_instance, default_name,
    open as open_add_instance_dialog,
};
pub use detail::{DetailTab, DetailView, NextTab, PrevTab};
pub use icon::{
    BUILTIN_ICONS, IconChoice, IconSource, asset_bytes, forget_cached_image, icon_for,
    instance_icon, save_icon, tint_for,
};
pub use icon_picker::{IconPicked, IconPicker, is_supported_upload, pick_id};
pub use list::{Filter, InstanceList};
pub use model::{Entry, InstancesChanged, InstancesModel, RefreshFailed};
pub use settings_tab::{SettingsForm, seed_java_list};
pub use versions_tab::{Incompatible, Row as VersionsRow, VersionsEvent, VersionsTab};
// The run state lives with the launch flow; listed here for the views.
pub use crate::launch::RunState;
pub use overview::OverviewStats;
pub use version_picker::{
    ChooseNext, ChoosePrev, KindFilter, LOADER_KINDS, LoaderChanged, LoaderPicker, VersionChosen,
    VersionPicker, default_loader_version, loader_row_id, loader_segment_id, version_row_id,
    visible_versions,
};

actions!(
    patina,
    [
        /// Opens the add-instance dialog. Dispatched by the list's ＋ and the
        /// empty state's button.
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
/// instance list has the keyboard; Ctrl+Tab / Ctrl+Shift+Tab and Ctrl+Page
/// Down / Up switch the detail's tabs anywhere on the page.
pub fn init(cx: &mut gpui_kit::App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-tab", NextTab, Some(INSTANCES_CONTEXT)),
        KeyBinding::new("ctrl-pagedown", NextTab, Some(INSTANCES_CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PrevTab, Some(INSTANCES_CONTEXT)),
        KeyBinding::new("ctrl-pageup", PrevTab, Some(INSTANCES_CONTEXT)),
    ]);
    list::init(cx);
    icon_picker::init(cx);
    version_picker::init(cx);
}

/// Element id of the text of a notification made with [`labelled`].
pub const NOTICE_TEXT: &str = "notice-text";

/// `notification` (made with `text`) carrying `text` again as an empty
/// labelled element ([`NOTICE_TEXT`]), so UI tests can tell which notice is
/// on show: gpui-component's notification text has no id.
pub(crate) fn labelled(notification: Notification, text: String) -> Notification {
    let text = SharedString::from(text);
    notification.content(move |_, _, _| {
        div()
            .id(NOTICE_TEXT)
            .aria_label(text.clone())
            .test_support()
            .into_any_element()
    })
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
    _launch_events: Subscription,
    _versions_events: Subscription,
    _refresh_failed: Subscription,
}

impl InstancesPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = cx.new(|cx| InstancesModel::new(window, cx));
        let focus = cx.focus_handle();
        let _observe = cx.observe(&model, |_, _, cx| cx.notify());
        // A prepared launch with warnings asks before it starts.
        let _launch_events = cx.subscribe_in(
            &model,
            window,
            |_, model, event: &LaunchEvent, window, cx| {
                if let LaunchEvent::ConfirmWarnings { id } = event {
                    prelaunch_dialog::open(model.clone(), id.clone(), window, cx);
                }
            },
        );
        // A version change's notices and questions, from the model.
        let _versions_events = cx.subscribe_in(
            &model,
            window,
            |_, model, event: &VersionsEvent, window, cx| {
                versions_tab::show_event(model, event, window, cx);
            },
        );
        // A refresh that failed keeps the last list; say so, in place of a
        // notice still on show from the same run of failures.
        let _refresh_failed = cx.subscribe_in(
            &model,
            window,
            |_, _, RefreshFailed(error): &RefreshFailed, window, cx| {
                let text = t!("instances.refresh_failed", error = error).to_string();
                let notice = labelled(Notification::warning(text.clone()), text);
                window.push_notification(notice.id::<RefreshFailed>(), cx);
            },
        );
        Self {
            list: cx.new(|cx| InstanceList::new(model.clone(), focus.clone(), cx)),
            detail: cx.new(|cx| DetailView::new(model.clone(), focus.clone(), cx)),
            model,
            focus,
            _observe,
            _launch_events,
            _versions_events,
            _refresh_failed,
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

    /// Switches the detail's tab, when a healthy instance shows one.
    fn step_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.model.read(cx).selected_entry(),
            Some(Entry::Healthy(_))
        ) {
            self.detail
                .update(cx, |d, cx| d.step_tab(delta, window, cx));
        }
    }

    /// Opens the add-instance dialog.
    fn add_instance(&mut self, _: &AddInstance, window: &mut Window, cx: &mut Context<Self>) {
        add_dialog::open(self.model.clone(), window, cx);
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
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.step_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| this.step_tab(-1, window, cx)))
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
