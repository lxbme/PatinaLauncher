//! The tabs for an instance's resource packs, shader packs and saves: one
//! view, [`LocalTab`], told apart by its [`LocalKind`]. Each is a list of
//! two-line rows (icon, name and a second line, a muted note, and for packs
//! and shaders a delete button), the shared toolbar and the empty states.
//!
//! - What a kind lists, and how it builds its items, is in
//!   `packs_tab`, `shaders_tab` and `saves_tab`.
//! - Icons are not kept in the items: a row on show reads its icon
//!   (`pack.png` of a folder or a zip, `icon.png` of a world) into the
//!   [`ImageCache`](super::ImageCache).
//! - Deleting a pack asks first (`删除"<名称>"？`, focus on 取消), then
//!   removes the file or folder under the instance lock
//!   ([`with_instance_lock`]). Saves cannot be deleted here, by design.
//! - Changes are refused, with the reason on show, while the instance is
//!   busy; when it is free again the folder is listed again.
//!
//! Keyboard: the rows are one Tab stop; Up / Down / Home / End move the
//! selection, Delete asks to delete the selected pack.

use std::cell::Cell;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::SystemTime;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::InputState;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, VirtualListScrollHandle,
    WindowExt as _, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, FocusHandle, FontWeight,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Pixels, Render,
    ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    TestSupportExt as _, Window, actions, div, px, rems, size,
};
use patina_core::content::packs::{delete_pack, read_pack_icon};
use patina_core::content::saves::read_world_icon;
use patina_core::instance::Instance;
use rust_i18n::t;

use super::image_cache::{ImageKey, icon_tile};
use super::list::{
    ContentTab, ContentToolbar, EmptyKind, EmptyState, LoadState, Named, filter_input,
    matches_filter,
};
use super::mods_tab::{FILE_W, ICON, ROW_H, WindowAction, notify};
use super::refresh::{open_folder, with_instance_lock};
use super::{packs_tab, saves_tab, shaders_tab};
use crate::fonts::Strong as _;
use crate::instances::{Entry, InstancesModel};
use crate::layout::{GUTTER, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY};

/// Key context of the rows of these tabs.
pub const LIST_CONTEXT: &str = "LocalList";

actions!(
    patina,
    [
        /// Selects the row above the selected one.
        SelectPrevRow,
        /// Selects the row below the selected one.
        SelectNextRow,
        /// Selects the first row.
        SelectFirstRow,
        /// Selects the last row.
        SelectLastRow,
        /// Asks to delete the selected pack.
        DeleteRow
    ]
);

/// Binds the keys of the rows.
pub(crate) fn init(cx: &mut App) {
    let context = Some(LIST_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevRow, context),
        KeyBinding::new("down", SelectNextRow, context),
        KeyBinding::new("home", SelectFirstRow, context),
        KeyBinding::new("end", SelectLastRow, context),
        KeyBinding::new("delete", DeleteRow, context),
    ]);
}

/// Which folder of the instance a [`LocalTab`] shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalKind {
    ResourcePacks,
    Shaders,
    Saves,
}

impl LocalKind {
    /// The prefix of the element ids of this kind's tab.
    pub fn prefix(self) -> &'static str {
        match self {
            LocalKind::ResourcePacks => "pack",
            LocalKind::Shaders => "shader",
            LocalKind::Saves => "save",
        }
    }

    /// The folder this kind lists.
    pub fn dir(self, instance: &Instance) -> PathBuf {
        match self {
            LocalKind::ResourcePacks => packs_tab::dir(instance),
            LocalKind::Shaders => shaders_tab::dir(instance),
            LocalKind::Saves => saves_tab::dir(instance),
        }
    }

    /// Lists the folder (blocking).
    fn load(self, instance: &Instance) -> io::Result<Vec<LocalItem>> {
        match self {
            LocalKind::ResourcePacks => packs_tab::load(instance),
            LocalKind::Shaders => shaders_tab::load(instance),
            LocalKind::Saves => saves_tab::load(instance),
        }
    }

    /// Whether rows can be deleted: packs and shaders yes, worlds never (a
    /// misclick must not cost a world).
    pub fn deletable(self) -> bool {
        self != LocalKind::Saves
    }

    fn empty(self) -> EmptyKind {
        match self {
            LocalKind::ResourcePacks => EmptyKind::ResourcePacks,
            LocalKind::Shaders => EmptyKind::Shaders,
            LocalKind::Saves => EmptyKind::Saves,
        }
    }

    fn placeholder(self) -> Lucide {
        match self {
            LocalKind::ResourcePacks => Lucide::Image,
            LocalKind::Shaders => Lucide::SunMedium,
            LocalKind::Saves => Lucide::Earth,
        }
    }

    /// Reads the icon of the item at `path` (blocking).
    fn read_icon(self, path: &std::path::Path) -> Option<Vec<u8>> {
        match self {
            LocalKind::ResourcePacks => read_pack_icon(path),
            LocalKind::Shaders => None,
            LocalKind::Saves => read_world_icon(path),
        }
    }
}

/// Element id of the row of item `key` in a tab of `kind`.
pub fn local_row_id(kind: LocalKind, key: &str) -> ElementId {
    ElementId::Name(format!("{}-row-{key}", kind.prefix()).into())
}

/// Element id of the delete button of item `key` in a tab of `kind`.
pub fn local_delete_id(kind: LocalKind, key: &str) -> ElementId {
    ElementId::Name(format!("{}-delete-{key}", kind.prefix()).into())
}

/// One row of a [`LocalTab`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalItem {
    /// The file or folder name in the tab's folder (unique).
    pub key: String,
    pub path: PathBuf,
    /// The name the row shows: a pack's file name, a world's name (its
    /// folder's when it has none).
    pub title: String,
    /// A pack's description, on one line.
    pub description: Option<String>,
    /// A world's last play, unix milliseconds.
    pub last_played: Option<i64>,
    /// A folder (a world always is) rather than a file.
    pub is_dir: bool,
    /// Whether there is an icon. Its bytes are not kept: a row on show
    /// reads them into the image cache.
    pub has_icon: bool,
    /// Size and modification time of the file the icon comes from, so a
    /// changed icon is a new image.
    pub icon_size: u64,
    pub icon_modified: Option<SystemTime>,
}

impl LocalItem {
    fn image_key(&self, kind: LocalKind, instance: &str) -> ImageKey {
        ImageKey::new(
            instance,
            format!("{}:{}", kind.prefix(), self.key),
            self.icon_size,
            self.icon_modified,
        )
    }
}

impl Named for LocalItem {
    fn filter_name(&self) -> &str {
        &self.title
    }
}

/// Size and modification time of `path` (zero and `None` when unknown).
pub(super) fn icon_stamp(path: &std::path::Path) -> (u64, Option<SystemTime>) {
    match std::fs::metadata(path) {
        Ok(md) => (md.len(), md.modified().ok()),
        Err(_) => (0, None),
    }
}

/// The tab of one instance's resource packs, shaders or saves (an entity).
/// The listing belongs to its [`ContentTab`]; the tab owns the selection
/// and the deletions.
pub struct LocalTab {
    kind: LocalKind,
    model: Entity<InstancesModel>,
    id: String,
    list: Entity<ContentTab<LocalItem>>,
    filter: Entity<InputState>,
    /// Indices into the list's items of the rows on show (the filter's).
    rows: Rc<Vec<usize>>,
    /// The selected item, by key.
    selected: Option<String>,
    /// Items with a deletion in flight.
    changing: HashSet<String>,
    /// Why the instance takes no change now.
    busy: Option<String>,
    /// The model's count of operations on the instance, last seen.
    op_changes: u64,
    /// Whether the tab is on show; a hidden tab does not list on window
    /// activation or when the instance is free again.
    visible: bool,
    /// The tab's region: where its commands are dispatched from.
    tab_focus: FocusHandle,
    /// The rows region: a Tab stop that takes the list's keys.
    focus: FocusHandle,
    scroll: VirtualListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl LocalTab {
    /// The tab of `instance`; it starts listing at once (it is made when
    /// it is shown).
    pub fn new(
        kind: LocalKind,
        model: Entity<InstancesModel>,
        instance: Instance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let id = instance.id.clone();
        let list = cx.new(|_| ContentTab::new(instance, move |instance| kind.load(instance)));
        let (filter, filter_changes) = filter_input(&list, window, cx);
        let activity = model.read(cx).activity().clone();
        let _subscriptions = vec![
            filter_changes,
            cx.observe(&list, |this, _, cx| {
                this.rebuild_rows(cx);
                cx.notify();
            }),
            cx.observe(&model, |this, _, cx| this.sync_instance(cx)),
            cx.observe(&activity, |this, _, cx| this.sync_activity(cx)),
            // Another program may have changed the folder meanwhile.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() && this.visible {
                    this.list.update(cx, |list, cx| list.reload(cx));
                }
            }),
        ];
        let mut this = Self {
            kind,
            model,
            id,
            list,
            filter,
            rows: Rc::default(),
            selected: None,
            changing: HashSet::new(),
            busy: None,
            op_changes: 0,
            visible: true,
            tab_focus: cx.focus_handle(),
            focus: cx.focus_handle().tab_stop(true),
            scroll: VirtualListScrollHandle::new(),
            _subscriptions,
        };
        let model = this.model.read(cx);
        this.busy = model.busy_reason(&this.id, cx);
        this.op_changes = model.op_changes(&this.id, cx);
        this.list.update(cx, |list, cx| list.shown(cx));
        this
    }

    pub fn kind(&self) -> LocalKind {
        self.kind
    }

    pub fn instance_id(&self) -> &str {
        &self.id
    }

    /// The listing (items, state, filter).
    pub fn list(&self) -> &Entity<ContentTab<LocalItem>> {
        &self.list
    }

    /// The rows region's focus handle.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The selected item's key.
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// How many deletions are in flight.
    pub fn changes_in_flight(&self) -> usize {
        self.changing.len()
    }

    /// The tab is shown again: list the folder.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        self.visible = true;
        self.list.update(cx, |list, cx| list.shown(cx));
    }

    /// The tab is hidden: no more listings until it is shown.
    pub fn hidden(&mut self) {
        self.visible = false;
    }

    fn item(&self, key: &str, cx: &App) -> Option<LocalItem> {
        (self.list.read(cx).items().iter())
            .find(|i| i.key == key)
            .cloned()
    }

    // --- deleting ---

    /// Asks before deleting item `key`: `删除"<名称>"？`, what is lost,
    /// 删除 / 取消, with the keyboard on 取消. Nothing for saves.
    pub fn confirm_delete(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.kind.deletable() {
            return;
        }
        let Some(item) = self.item(key, cx) else {
            return;
        };
        if !self.may_change(key, window, cx) {
            return;
        }
        let prefix = self.kind.prefix();
        let this = cx.weak_entity();
        let title = SharedString::from(t!("content.delete_title", name = item.title));
        let body = if item.is_dir {
            t!("content.delete_body_dir")
        } else {
            t!("content.delete_body_file")
        }
        .to_string();
        // The footer's region, to tell whether one of its buttons has the
        // keyboard; also where the keyboard is put on 取消 from.
        let footer_focus = cx.focus_handle();
        let first_draw = Cell::new(true);
        window.open_alert_dialog(cx, {
            let footer_focus = footer_focus.clone();
            move |alert, window, cx| {
                focus_first_button(&footer_focus, &first_draw, window, cx);
                let delete: Rc<WindowAction> = {
                    let (this, key) = (this.clone(), item.key.clone());
                    Rc::new(move |window, cx| {
                        this.update(cx, |this, cx| this.delete(&key, window, cx))
                            .ok();
                    })
                };
                let footer = DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label(t!("content.cancel").to_string())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child({
                        let delete = delete.clone();
                        Button::new("ok")
                            .danger()
                            .label(t!("content.delete_confirm").to_string())
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                delete(window, cx);
                            })
                    });
                let footer_focus = footer_focus.clone();
                alert
                    .title(
                        div()
                            .id(ElementId::Name(format!("{prefix}-delete-title").into()))
                            .aria_label(title.clone())
                            .test_support()
                            .child(title.clone()),
                    )
                    .description({
                        let body = body.clone();
                        div()
                            .id(ElementId::Name(format!("{prefix}-delete-body").into()))
                            .aria_label(body.clone())
                            .test_support()
                            .child(body)
                    })
                    .footer(div().w_full().track_focus(&footer_focus).child(footer))
                    // Enter deletes from the dialog itself; on a button it is
                    // that button's own key, so on 取消 it cancels.
                    .on_ok(move |_, window, cx| {
                        if footer_focus.contains_focused(window, cx) {
                            cx.propagate();
                            return false;
                        }
                        delete(window, cx);
                        true
                    })
            }
        });
    }

    /// Deletes item `key` under the instance lock and drops its row.
    fn delete(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.kind.deletable() {
            return;
        }
        let Some(item) = self.item(key, cx) else {
            return;
        };
        if !self.may_change(key, window, cx) {
            return;
        }
        let dir = self.kind.dir(self.list.read(cx).instance());
        let path = item.path.clone();
        let task = with_instance_lock(&self.model, &self.id, cx, move || delete_pack(&dir, &path));
        self.changing.insert(item.key.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.changing.remove(&item.key);
                match result {
                    Ok(Ok(())) => this.deleted(&item.key, cx),
                    Ok(Err(error)) => this.delete_failed(&item, &error, window, cx),
                    Err(reason) => {
                        notify(Notification::warning(reason.clone()), reason, window, cx)
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Whether item `key` may be changed now; when the instance is busy,
    /// says why.
    fn may_change(&self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.changing.contains(key) {
            return false;
        }
        match self.model.read(cx).busy_reason(&self.id, cx) {
            Some(reason) => {
                let text = t!("content.busy", reason = reason).to_string();
                notify(Notification::warning(text.clone()), text, window, cx);
                false
            }
            None => true,
        }
    }

    /// Item `key` is gone: its row too; the selection moves to the row that
    /// takes its place.
    fn deleted(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(key) {
            let items = self.list.read(cx).items();
            let name = |ix: &usize| items.get(*ix).map(|i| i.key.as_str());
            let at = (self.rows.iter()).position(|ix| name(ix) == Some(key));
            let next = at.and_then(|at| {
                let rows = &self.rows;
                rows.get(at + 1)
                    .or_else(|| at.checked_sub(1).and_then(|p| rows.get(p)))
                    .and_then(|ix| name(ix).map(str::to_owned))
            });
            self.selected = next;
        }
        self.list.update(cx, |list, cx| {
            list.update_items(|items| items.retain(|i| i.key != key), cx)
        });
    }

    fn delete_failed(
        &mut self,
        item: &LocalItem,
        error: &io::Error,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if error.kind() == io::ErrorKind::NotFound {
            let text = t!("content.changed_outside").to_string();
            notify(Notification::warning(text.clone()), text, window, cx);
            self.list.update(cx, |list, cx| list.mutated(cx));
        } else {
            let text = t!("content.delete_failed", name = item.title, error = error).to_string();
            notify(Notification::error(text.clone()), text, window, cx);
        }
    }

    // --- following the model ---

    /// Follows the instance's config (and a rename of its folder).
    fn sync_instance(&mut self, cx: &mut Context<Self>) {
        let Some(Entry::Healthy(instance)) = self.model.read(cx).entry(&self.id) else {
            return;
        };
        if self.list.read(cx).instance() == instance {
            return;
        }
        let instance = instance.clone();
        self.list
            .update(cx, |list, cx| list.set_instance(instance, cx));
    }

    /// Follows whether the instance is busy; lists the folder again when an
    /// operation finished or the instance is free again (the game or the
    /// operation may have changed it).
    fn sync_activity(&mut self, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let busy = model.busy_reason(&self.id, cx);
        let op_changes = model.op_changes(&self.id, cx);
        let freed = self.busy.is_some() && busy.is_none();
        let operated = op_changes != self.op_changes;
        let notify = busy != self.busy;
        self.busy = busy;
        self.op_changes = op_changes;
        if (freed || operated) && self.visible {
            self.list.update(cx, |list, cx| list.reload(cx));
        }
        if notify {
            cx.notify();
        }
    }

    fn rebuild_rows(&mut self, cx: &App) {
        let list = self.list.read(cx);
        let query = list.filter();
        let rows: Vec<usize> = (list.items().iter().enumerate())
            .filter(|(_, i)| matches_filter(&i.title, query) || matches_filter(&i.key, query))
            .map(|(ix, _)| ix)
            .collect();
        if *self.rows != rows {
            self.rows = Rc::new(rows);
        }
    }

    // --- keyboard ---

    fn select_prev(&mut self, _: &SelectPrevRow, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn select_next(&mut self, _: &SelectNextRow, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn select_first(&mut self, _: &SelectFirstRow, _: &mut Window, cx: &mut Context<Self>) {
        self.select_row(0, cx);
    }

    fn select_last(&mut self, _: &SelectLastRow, _: &mut Window, cx: &mut Context<Self>) {
        self.select_row(self.rows.len().saturating_sub(1), cx);
    }

    fn delete_selected(&mut self, _: &DeleteRow, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = self.selected.clone() {
            self.confirm_delete(&key, window, cx);
        }
    }

    /// Moves the selection `step` rows (clamped); with none selected (or
    /// the selected one filtered out), selects the first row.
    fn step(&mut self, step: isize, cx: &mut Context<Self>) {
        let items = self.list.read(cx).items();
        let at = (self.selected.as_ref()).and_then(|key| {
            (self.rows.iter()).position(|ix| items.get(*ix).is_some_and(|i| i.key == *key))
        });
        let row = match at {
            Some(at) => at.saturating_add_signed(step),
            None => 0,
        };
        self.select_row(row, cx);
    }

    /// Selects row `row` (clamped to the last) and scrolls it into view.
    fn select_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let row = row.min(self.rows.len().saturating_sub(1));
        let items = self.list.read(cx).items();
        let Some(item) = self.rows.get(row).and_then(|ix| items.get(*ix)) else {
            return;
        };
        self.selected = Some(item.key.clone());
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    // --- rendering ---

    fn render_icon(&self, item: &LocalItem, cx: &mut Context<Self>) -> AnyElement {
        if item.has_icon {
            // Read again only for a row on show, and only until the image
            // is cached (capped by the core and the cache).
            let (kind, path) = (self.kind, item.path.clone());
            return icon_tile(
                item.image_key(self.kind, &self.id),
                move || kind.read_icon(&path),
                ICON,
                cx,
            );
        }
        let theme = cx.theme();
        h_flex()
            .flex_none()
            .size(ICON)
            .rounded(rems(ICON.0 * 0.22))
            .justify_center()
            .bg(theme.muted)
            .text_color(theme.muted_foreground)
            .child(Icon::new(self.kind.placeholder()).size(rems(1.)))
            .into_any_element()
    }

    /// The muted note at the row's right: a pack's "文件夹" or "压缩包", a
    /// world's folder name.
    fn aside(&self, item: &LocalItem) -> String {
        match self.kind {
            LocalKind::Saves => item.key.clone(),
            _ if item.is_dir => t!("content.kind.folder").to_string(),
            _ => t!("content.kind.archive").to_string(),
        }
    }

    /// The row's second line: a pack's description, a world's last play.
    fn detail(&self, item: &LocalItem) -> Option<String> {
        match self.kind {
            LocalKind::Saves => item.last_played.map(|ms| {
                t!(
                    "content.last_played",
                    when = saves_tab::format_last_played(ms)
                )
                .to_string()
            }),
            _ => item.description.clone(),
        }
    }

    /// Row `ix` of the items; `ring` draws the keyboard focus ring on it
    /// when selected.
    fn render_row(&self, ix: usize, ring: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.list.read(cx).items().get(ix).cloned() else {
            return div().into_any_element();
        };
        let kind = self.kind;
        let key = item.key.clone();
        let selected = self.selected.as_deref() == Some(key.as_str());
        let locked = self.busy.is_some() || self.changing.contains(&key);
        let icon = self.render_icon(&item, cx);
        let (aside_text, detail) = (self.aside(&item), self.detail(&item));
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let name = SharedString::from(item.title.clone());

        let first = div()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .text_size(TEXT_ROW)
            .text_color(fg)
            .strong(FontWeight::MEDIUM, cx)
            .child(name.clone());
        let second = div()
            .id(ElementId::Name(
                format!("{}-detail-{key}", kind.prefix()).into(),
            ))
            .aria_label(detail.clone().unwrap_or_default())
            .test_support()
            .w_full()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .text_size(TEXT_SECONDARY)
            .text_color(muted)
            .children(detail);
        let aside = div()
            .id(ElementId::Name(
                format!("{}-aside-{key}", kind.prefix()).into(),
            ))
            .aria_label(aside_text.clone())
            .test_support()
            .min_w_0()
            .max_w(FILE_W)
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .text_size(TEXT_SECONDARY)
            .text_color(muted)
            .child(aside_text);

        let this = cx.weak_entity();
        let focus = self.focus.clone();
        let delete = kind.deletable().then(|| {
            let (this, focus, key) = (this.clone(), focus.clone(), key.clone());
            Button::new(local_delete_id(kind, &key))
                .ghost()
                .xsmall()
                .icon(Icon::new(Lucide::Trash))
                .tab_stop(false)
                .accessibility_label(t!("content.delete", name = name).to_string())
                .disabled(locked)
                .when_some(self.busy.clone(), |b, reason| b.tooltip(reason))
                .on_click(move |_, window, cx| {
                    focus.focus(window, cx);
                    this.update(cx, |this, cx| {
                        this.selected = Some(key.clone());
                        this.confirm_delete(&key, window, cx);
                    })
                    .ok();
                })
        });
        let select = {
            let key = key.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                // A clicked row hands the keyboard to the list.
                focus.focus(window, cx);
                this.update(cx, |this, cx| {
                    this.selected = Some(key.clone());
                    cx.notify();
                })
                .ok();
            }
        };
        let (hover, active) = (theme.list_hover, theme.list_active);
        let ring_color = if selected && ring {
            theme.ring
        } else {
            theme.transparent
        };
        h_flex()
            .id(local_row_id(kind, &key))
            .w_full()
            .h(ROW_H)
            .px(GUTTER)
            .gap_2p5()
            // Every row has the 1px border so the ring does not move the
            // content.
            .border_1()
            .border_color(ring_color)
            .map(|r| {
                if selected {
                    r.bg(active)
                } else {
                    r.hover(move |s| s.bg(hover))
                }
            })
            .aria_label(name)
            .aria_selected(selected)
            .on_click(select)
            .child(icon)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    // 1px between the two lines, as in the prototype.
                    .gap(px(1.))
                    .child(first)
                    .child(second),
            )
            .child(aside)
            .children(delete)
            .test_support()
            .into_any_element()
    }

    fn render_rows(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row_h: Pixels = ROW_H.to_pixels(window.rem_size());
        // Only the heights are used; the list measures the width itself.
        let sizes = Rc::new(vec![size(px(0.), row_h); self.rows.len()]);
        let prefix = self.kind.prefix();
        v_flex()
            .id(ElementId::Name(format!("{prefix}-rows-region").into()))
            // Before `track_focus`, so tests see the focus.
            .test_support()
            .key_context(LIST_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::delete_selected))
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                v_virtual_list(
                    cx.entity(),
                    ElementId::Name(format!("{prefix}-rows").into()),
                    sizes,
                    |this, range, window, cx| {
                        // The selected row shows the ring only while the
                        // keyboard drives the list (focus-visible).
                        let ring =
                            this.focus.is_focused(window) && window.last_input_was_keyboard();
                        let rows = this.rows.clone();
                        rows[range]
                            .iter()
                            .map(|ix| this.render_row(*ix, ring, cx))
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0(),
            )
            .vertical_scrollbar(&self.scroll)
    }

    fn render_toolbar(&self, cx: &App) -> ContentToolbar {
        let list = self.list.read(cx);
        let dir = self.kind.dir(list.instance());
        let refresh = self.list.clone();
        ContentToolbar::new(
            &self.filter,
            move |_, cx| open_folder(dir.clone(), cx),
            move |_, cx| refresh.update(cx, |list, cx| list.reload(cx)),
        )
        .refreshing(list.is_reloading())
    }

    /// The line under the toolbar while the instance takes no change (only
    /// where something can be changed).
    fn render_busy(&self, cx: &App) -> Option<impl IntoElement> {
        let reason = self.busy.as_ref().filter(|_| self.kind.deletable())?;
        let text = t!("content.busy", reason = reason).to_string();
        let theme = cx.theme();
        Some(
            div()
                .id(ElementId::Name(
                    format!("{}-busy", self.kind.prefix()).into(),
                ))
                .aria_label(text.clone())
                .test_support()
                .flex_none()
                .px(GUTTER)
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .text_size(TEXT_SECONDARY)
                .text_color(theme.muted_foreground)
                .child(text),
        )
    }
}

/// Puts the keyboard on the first button of a dialog's footer once the
/// dialog is drawn (call it from the dialog's builder; it acts on its first
/// call only): the footer's region (`footer`) takes the focus, and the next
/// tab stop after it is the first button. The destructive button is never
/// the default.
fn focus_first_button(
    footer: &FocusHandle,
    pending: &Cell<bool>,
    window: &mut Window,
    cx: &mut App,
) {
    if !pending.replace(false) {
        return;
    }
    let footer = footer.clone();
    // After the frame in progress: only then the footer is in the focus
    // tree.
    window.defer(cx, move |window, cx| {
        footer.focus(window, cx);
        window.focus_next(cx);
    });
}

impl Render for LocalTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(cx);
        let busy = self.render_busy(cx).map(IntoElement::into_any_element);
        let list = self.list.read(cx);
        let failed = match list.state() {
            LoadState::Failed(error) => Some(error.clone()),
            _ => None,
        };
        let (loading, has_items) = (
            *list.state() == LoadState::Loading,
            !list.items().is_empty(),
        );
        let filter = list.filter().to_string();
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        // A failed listing over older items says so above them.
        let failed_note = failed.as_ref().filter(|_| has_items).map(|error| {
            div()
                .flex_none()
                .px(GUTTER)
                .py_2()
                .border_b_1()
                .border_color(border)
                .text_size(TEXT_SECONDARY)
                .text_color(muted)
                .child(t!("content.failed", error = error).to_string())
        });
        let body = if !self.rows.is_empty() {
            self.render_rows(window, cx).into_any_element()
        } else if loading {
            div().flex_1().into_any_element()
        } else if let Some(error) = failed.filter(|_| !has_items) {
            EmptyState::failed(&error).into_any_element()
        } else if has_items {
            EmptyState::no_match(&filter).into_any_element()
        } else {
            EmptyState::new(self.kind.empty()).into_any_element()
        };
        v_flex()
            .id(ElementId::Name(
                format!("{}s-tab", self.kind.prefix()).into(),
            ))
            .test_support()
            .track_focus(&self.tab_focus)
            .size_full()
            .text_size(TEXT_PRIMARY)
            .child(toolbar)
            .children(busy)
            .children(failed_note)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
    }
}
