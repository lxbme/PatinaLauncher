//! The "Mods" tab of an instance's detail: the jars of `minecraft/mods` as
//! two-line rows (icon, name with its format tag, "版本 · 作者"; the file
//! name; the enable switch and delete), the shared toolbar with "在线获取",
//! and the empty states.
//!
//! - Enabling and disabling renames the jar (`.disabled` suffix) under the
//!   instance lock ([`with_instance_lock`]); the row is updated in place,
//!   the folder is not listed again.
//! - Deleting asks first (`删除"<名称>"？`).
//! - A file another program renamed or removed meanwhile is reported as
//!   "文件已在外部被改动" and the folder is listed again.
//! - Every change of this app, and every listing that found the folder
//!   changed, emits [`ModsChanged`] (the global search refreshes its index).
//! - The global search jumps to a mod with [`ModsTab::focus_file`].
//! - Changes are refused, with the reason on show, while the instance is
//!   busy (its game runs, an operation is in flight); when it is free again
//!   the folder is listed again (the game or the operation may have changed
//!   it).
//!
//! Keyboard: the rows are one Tab stop; Up / Down / Home / End move the
//! selection, Space toggles the selected mod, Delete asks to delete it.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash as _, Hasher as _};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::InputState;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, VirtualListScrollHandle,
    WindowExt as _, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, EventEmitter, FocusHandle,
    FontWeight, Global, Hsla, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _,
    Pixels, Rems, Render, ScrollStrategy, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, TestSupportExt as _, Window, actions, div, px, rems, size,
};
use patina_core::content::mods::{
    ModFormat, ModMeta, delete_mod, list_mod_files, read_mod_meta, set_mod_enabled, split_mod_name,
};
use patina_core::instance::Instance;
use patina_core::loader::LoaderKind;
use rust_i18n::t;

use super::image_cache::{ImageKey, icon_tile};
use super::list::{
    ContentTab, ContentToolbar, EmptyKind, EmptyState, LoadState, Named, Reloaded, filter_input,
    matches_filter,
};
use super::refresh::{open_folder, with_instance_lock};
use crate::fonts::Strong as _;
use crate::instances::{Entry, InstancesModel, labelled};
use crate::layout::{GUTTER, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY};
use crate::shell::OpenDownloads;
use crate::theme::ProductColors;

/// How long a row the global search jumped to stays highlighted.
pub const HIGHLIGHT_FOR: Duration = Duration::from_secs(2);
/// Row height (two lines), 48 px.
pub(super) const ROW_H: Rems = Rems(3.);
/// Row icon, 32 px.
pub(super) const ICON: Rems = Rems(2.);
/// The widest the file-name column gets.
pub(super) const FILE_W: Rems = Rems(14.);

/// Key context of the Mods tab's rows.
pub const LIST_CONTEXT: &str = "ModList";

actions!(
    patina,
    [
        /// Selects the mod above the selected one.
        SelectPrevMod,
        /// Selects the mod below the selected one.
        SelectNextMod,
        /// Selects the first mod.
        SelectFirstMod,
        /// Selects the last mod.
        SelectLastMod,
        /// Enables the selected mod when it is disabled, and the other way
        /// round.
        ToggleMod,
        /// Asks to delete the selected mod.
        DeleteMod
    ]
);

/// Binds the keys of the Mods tab's rows.
pub(crate) fn init(cx: &mut App) {
    let context = Some(LIST_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevMod, context),
        KeyBinding::new("down", SelectNextMod, context),
        KeyBinding::new("home", SelectFirstMod, context),
        KeyBinding::new("end", SelectLastMod, context),
        KeyBinding::new("space", ToggleMod, context),
        KeyBinding::new("delete", DeleteMod, context),
    ]);
}

// --- the mods-changed event ------------------------------------------------

/// The mods of instance `instance_id` changed: this app enabled, disabled
/// or deleted one, or a listing found the folder changed. Emitted by
/// [`ModsEvents`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModsChanged {
    pub instance_id: String,
}

/// Where [`ModsChanged`] comes from (an entity; subscribe to
/// [`ModsEvents::entity`]). Set up by `content::init`.
pub struct ModsEvents;

impl EventEmitter<ModsChanged> for ModsEvents {}

struct ModsEventsHub(Entity<ModsEvents>);

impl Global for ModsEventsHub {}

impl ModsEvents {
    /// Creates the event source, once per app.
    pub fn init(cx: &mut App) {
        if !cx.has_global::<ModsEventsHub>() {
            let hub = cx.new(|_| ModsEvents);
            cx.set_global(ModsEventsHub(hub));
        }
    }

    /// The entity to subscribe to.
    pub fn entity(cx: &App) -> Entity<ModsEvents> {
        cx.global::<ModsEventsHub>().0.clone()
    }

    /// Emits [`ModsChanged`] for `instance_id`.
    pub fn emit(instance_id: &str, cx: &mut App) {
        let event = ModsChanged {
            instance_id: instance_id.to_string(),
        };
        Self::entity(cx).update(cx, |_, cx| cx.emit(event));
    }
}

// --- the items -------------------------------------------------------------

/// What a jar's metadata says, as a row shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModInfo {
    pub id: Option<String>,
    pub name: String,
    pub version: Option<String>,
    pub authors: Vec<String>,
    pub format: ModFormat,
    /// Whether the jar has an icon. Its bytes are not kept: a row on show
    /// reads them again into the [`ImageCache`](super::ImageCache), so what
    /// a folder of many mods keeps in memory does not grow with their
    /// icons.
    pub has_icon: bool,
}

impl From<ModMeta> for ModInfo {
    fn from(meta: ModMeta) -> Self {
        Self {
            id: meta.id,
            name: meta.name,
            version: meta.version,
            authors: meta.authors,
            format: meta.format,
            has_icon: meta.icon.is_some(),
        }
    }
}

/// One mod file of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModItem {
    pub file_name: String,
    pub path: PathBuf,
    pub enabled: bool,
    pub size: u64,
    /// Modification time in unix nanoseconds (0 when unknown).
    pub modified_nanos: u64,
    /// `None`: the jar's metadata could not be read (a damaged jar).
    pub info: Option<Arc<ModInfo>>,
}

impl ModItem {
    /// The name the row shows: the mod's, else the file's without `.jar`.
    pub fn name(&self) -> &str {
        match &self.info {
            Some(info) => &info.name,
            None => self.stem(),
        }
    }

    fn stem(&self) -> &str {
        split_mod_name(&self.file_name).map_or(self.file_name.as_str(), |(stem, _)| stem)
    }

    /// "版本 · 作者" of the second line, as far as known.
    pub fn detail(&self) -> Option<String> {
        let info = self.info.as_ref()?;
        let authors = (!info.authors.is_empty()).then(|| info.authors.join(", "));
        let parts: Vec<String> = [info.version.clone(), authors]
            .into_iter()
            .flatten()
            .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// The icon's cache key.
    fn image_key(&self, instance: &str) -> ImageKey {
        let modified = (self.modified_nanos > 0)
            .then(|| UNIX_EPOCH + Duration::from_nanos(self.modified_nanos));
        ImageKey::new(instance, self.file_name.clone(), self.size, modified)
    }

    /// The metadata cache's key.
    fn meta_key(&self) -> MetaKey {
        (self.file_name.clone(), self.size, self.modified_nanos)
    }
}

impl Named for ModItem {
    fn filter_name(&self) -> &str {
        self.name()
    }
}

/// The mod format an instance's loader reads first (see `read_mod_meta`).
pub fn preferred_format(instance: &Instance) -> Option<ModFormat> {
    match instance.config.loader().map(|(kind, _)| kind) {
        Some(LoaderKind::Fabric) => Some(ModFormat::Fabric),
        Some(LoaderKind::Forge) => Some(ModFormat::Forge),
        Some(LoaderKind::Neoforge) => Some(ModFormat::NeoForge),
        None => None,
    }
}

/// The instance's mods folder.
pub fn mods_dir(instance: &Instance) -> PathBuf {
    instance.game_dir().join("mods")
}

/// (file name, size, modification time in ns) of a mod file.
type MetaKey = (String, u64, u64);

/// The metadata of the last listing, by [`MetaKey`]: a listing opens only
/// the jars that are new or changed since. A toggle moves its entry to the
/// new name ([`MetaCache::rename`]), so the renamed jar is not opened
/// again; `x.jar` and `x.jar.disabled` side by side are two entries.
#[derive(Default)]
struct MetaCache {
    prefer: Option<ModFormat>,
    entries: HashMap<MetaKey, Option<Arc<ModInfo>>>,
}

impl MetaCache {
    /// The entry of `from` is now `to`'s (a rename keeps size and time).
    fn rename(&mut self, from: &MetaKey, to: MetaKey) {
        if let Some(info) = self.entries.remove(from) {
            self.entries.insert(to, info);
        }
    }
}

/// Lists the mods in `dir` with their metadata (blocking: it opens the
/// jars not in `cache`).
fn list_mods(
    dir: &Path,
    prefer: Option<ModFormat>,
    cache: &Mutex<MetaCache>,
) -> io::Result<Vec<ModItem>> {
    let files = list_mod_files(dir)?;
    let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
    if cache.prefer != prefer {
        cache.entries.clear();
        cache.prefer = prefer;
    }
    let mut seen = HashMap::with_capacity(files.len());
    let items = files
        .into_iter()
        .map(|file| {
            let key = (file.file_name.clone(), file.size, file.modified_nanos);
            let info = match cache.entries.remove(&key) {
                Some(info) => info,
                None => read_mod_meta(&file.path, prefer).map(|meta| Arc::new(meta.into())),
            };
            seen.insert(key, info.clone());
            ModItem {
                file_name: file.file_name,
                path: file.path,
                enabled: file.enabled,
                size: file.size,
                modified_nanos: file.modified_nanos,
                info,
            }
        })
        .collect();
    cache.entries = seen;
    Ok(items)
}

/// What tells two listings apart: the names, sizes and times of the files.
fn listing_hash(items: &[ModItem]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for item in items {
        (&item.file_name, item.size, item.modified_nanos).hash(&mut hasher);
    }
    hasher.finish()
}

/// What a failed change says when another program changed the folder:
/// the file was renamed or removed, or a file already has the new name.
/// `None` for other failures.
fn outside_change_message(error: &io::Error) -> Option<String> {
    match error.kind() {
        io::ErrorKind::NotFound => Some(t!("mods.changed_outside").to_string()),
        io::ErrorKind::AlreadyExists => Some(t!("mods.name_taken").to_string()),
        _ => None,
    }
}

/// The format tag's text.
fn format_label(format: ModFormat) -> String {
    match format {
        ModFormat::Fabric => t!("mods.format.fabric"),
        ModFormat::Quilt => t!("mods.format.quilt"),
        ModFormat::Forge => t!("mods.format.forge"),
        ModFormat::NeoForge => t!("mods.format.neoforge"),
        ModFormat::LegacyForge => t!("mods.format.legacy_forge"),
        ModFormat::Unknown => t!("mods.format.unknown"),
    }
    .to_string()
}

/// The format tag's colour: the colour of the loader that reads it.
fn format_color(format: Option<ModFormat>, cx: &App) -> Hsla {
    let colors = ProductColors::get(cx);
    match format {
        Some(ModFormat::Fabric | ModFormat::Quilt) => colors.loader(Some(LoaderKind::Fabric)),
        Some(ModFormat::Forge | ModFormat::LegacyForge) => colors.loader(Some(LoaderKind::Forge)),
        Some(ModFormat::NeoForge) => colors.loader(Some(LoaderKind::Neoforge)),
        Some(ModFormat::Unknown) | None => cx.theme().muted_foreground,
    }
}

/// Element id of the row of mod file `file_name`.
pub fn mod_row_id(file_name: &str) -> ElementId {
    ElementId::Name(format!("mod-row-{file_name}").into())
}

/// Element id of the enable switch of mod file `file_name`.
pub fn mod_switch_id(file_name: &str) -> ElementId {
    ElementId::Name(format!("mod-switch-{file_name}").into())
}

/// Element id of the "已禁用" mark of mod file `file_name`.
pub fn mod_disabled_id(file_name: &str) -> ElementId {
    ElementId::Name(format!("mod-disabled-{file_name}").into())
}

/// Element id of the delete button of mod file `file_name`.
pub fn mod_delete_id(file_name: &str) -> ElementId {
    ElementId::Name(format!("mod-delete-{file_name}").into())
}

// --- the tab ---------------------------------------------------------------

/// The Mods tab of one instance (an entity). The listing belongs to its
/// [`ContentTab`]; the tab owns the selection, the highlight and the
/// changes.
pub struct ModsTab {
    model: Entity<InstancesModel>,
    id: String,
    list: Entity<ContentTab<ModItem>>,
    /// The listing's metadata cache (shared with its loader).
    cache: Arc<Mutex<MetaCache>>,
    filter: Entity<InputState>,
    /// Indices into the list's items of the rows on show (the filter's),
    /// rebuilt when the items or the filter change.
    rows: Rc<Vec<usize>>,
    /// The selected mod, by file name.
    selected: Option<String>,
    /// The mod the global search jumped to, for [`HIGHLIGHT_FOR`].
    highlight: Option<String>,
    highlight_task: Task<()>,
    /// A jump that waits for the listing.
    pending_focus: Option<String>,
    /// Mods with a change in flight.
    changing: HashSet<String>,
    /// Why the instance takes no change now.
    busy: Option<String>,
    /// The model's count of operations on the instance, last seen.
    op_changes: u64,
    /// The last listing's [`listing_hash`] (also after this app's changes).
    listed: Option<u64>,
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

impl ModsTab {
    /// The tab of `instance`; it starts listing at once (it is made when
    /// it is shown).
    pub fn new(
        model: Entity<InstancesModel>,
        instance: Instance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let id = instance.id.clone();
        let cache = Arc::new(Mutex::new(MetaCache::default()));
        let list = cx.new(|_| {
            let cache = cache.clone();
            ContentTab::new(instance, move |instance| {
                list_mods(&mods_dir(instance), preferred_format(instance), &cache)
            })
        });
        let (filter, filter_changes) = filter_input(&list, window, cx);
        let activity = model.read(cx).activity().clone();
        let _subscriptions = vec![
            filter_changes,
            cx.observe(&list, |this, _, cx| {
                this.rebuild_rows(cx);
                cx.notify();
            }),
            cx.subscribe_in(&list, window, |this, _, _: &Reloaded, window, cx| {
                this.landed(window, cx)
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
            model,
            id,
            list,
            cache,
            filter,
            rows: Rc::default(),
            selected: None,
            highlight: None,
            highlight_task: Task::ready(()),
            pending_focus: None,
            changing: HashSet::new(),
            busy: None,
            op_changes: 0,
            listed: None,
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

    pub fn instance_id(&self) -> &str {
        &self.id
    }

    /// The listing (items, state, filter).
    pub fn list(&self) -> &Entity<ContentTab<ModItem>> {
        &self.list
    }

    /// The rows region's focus handle.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The selected mod's file name.
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The file name of the mod highlighted after a jump, for
    /// [`HIGHLIGHT_FOR`].
    pub fn highlighted(&self) -> Option<&str> {
        self.highlight.as_deref()
    }

    /// How many enables, disables and deletions are in flight.
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

    /// Scrolls to mod file `file_name`, selects it and highlights it for
    /// [`HIGHLIGHT_FOR`], giving the rows the keyboard. A filter hiding it
    /// is cleared. Before the listing has landed, waits for it; a file that
    /// is not there is ignored.
    pub fn focus_file(&mut self, file_name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_focus = Some(file_name.to_string());
        self.apply_focus(window, cx);
    }

    fn apply_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.pending_focus.clone() else {
            return;
        };
        let list = self.list.read(cx);
        let Some(ix) = list.items().iter().position(|m| m.file_name == file) else {
            // A landed listing without it: the file is gone.
            if *list.state() != LoadState::Loading && !list.is_reloading() {
                self.pending_focus = None;
            }
            return;
        };
        self.pending_focus = None;
        if !self.rows.contains(&ix) {
            self.filter
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.list.update(cx, |list, cx| list.set_filter("", cx));
            self.rebuild_rows(cx);
        }
        if let Some(row) = self.rows.iter().position(|r| *r == ix) {
            self.scroll.scroll_to_item(row, ScrollStrategy::Center);
        }
        self.focus.focus(window, cx);
        self.selected = Some(file.clone());
        self.highlight = Some(file.clone());
        let timer = cx.background_executor().timer(HIGHLIGHT_FOR);
        self.highlight_task = cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| {
                if this.highlight.as_deref() == Some(file.as_str()) {
                    this.highlight = None;
                    cx.notify();
                }
            })
            .ok();
        });
        cx.notify();
    }

    /// Enables or disables mod file `file_name` (renames it), under the
    /// instance lock. On success only its row changes; a file changed by
    /// another program is reported and the folder listed again.
    pub fn set_enabled(
        &mut self,
        file_name: &str,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.item(file_name, cx) else {
            return;
        };
        if item.enabled == enabled || !self.may_change(file_name, window, cx) {
            return;
        }
        let path = item.path.clone();
        let task = with_instance_lock(&self.model, &self.id, cx, move || {
            set_mod_enabled(&path, enabled)
        });
        self.changing.insert(item.file_name.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.changing.remove(&item.file_name);
                match result {
                    Ok(Ok(path)) => this.toggled(&item, path, enabled, cx),
                    Ok(Err(error)) => this.change_failed(&item, &error, window, cx),
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

    /// Asks before deleting mod file `file_name`: `删除"<名称>"？`, "文件会被
    /// 永久删除", 删除 / 取消.
    pub fn confirm_delete(&mut self, file_name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.item(file_name, cx) else {
            return;
        };
        if !self.may_change(file_name, window, cx) {
            return;
        }
        let this = cx.weak_entity();
        let title = SharedString::from(t!("mods.delete_title", name = item.name()));
        // The footer's region, to tell whether one of its buttons has the
        // keyboard.
        let footer_focus = cx.focus_handle();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let delete: Rc<WindowAction> = {
                let (this, file) = (this.clone(), item.file_name.clone());
                Rc::new(move |window, cx| {
                    this.update(cx, |this, cx| this.delete(&file, window, cx))
                        .ok();
                })
            };
            let footer = DialogFooter::new()
                .child(
                    Button::new("cancel")
                        .label(t!("mods.cancel").to_string())
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child({
                    let delete = delete.clone();
                    Button::new("ok")
                        .danger()
                        .label(t!("mods.delete_confirm").to_string())
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            delete(window, cx);
                        })
                });
            let footer_focus = footer_focus.clone();
            alert
                .title(
                    div()
                        .id("mods-delete-title")
                        .aria_label(title.clone())
                        .test_support()
                        .child(title.clone()),
                )
                .description({
                    let body = t!("mods.delete_body").to_string();
                    div()
                        .id("mods-delete-body")
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
        });
    }

    /// Deletes mod file `file_name` under the instance lock and drops its
    /// row.
    fn delete(&mut self, file_name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.item(file_name, cx) else {
            return;
        };
        if !self.may_change(file_name, window, cx) {
            return;
        }
        let path = item.path.clone();
        let task = with_instance_lock(&self.model, &self.id, cx, move || delete_mod(&path));
        self.changing.insert(item.file_name.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.changing.remove(&item.file_name);
                match result {
                    Ok(Ok(())) => this.deleted(&item.file_name, cx),
                    Ok(Err(error)) => this.change_failed(&item, &error, window, cx),
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

    fn item(&self, file_name: &str, cx: &App) -> Option<ModItem> {
        (self.list.read(cx).items().iter())
            .find(|m| m.file_name == file_name)
            .cloned()
    }

    /// Whether mod `file_name` may be changed now; when the instance is
    /// busy, says why.
    fn may_change(&self, file_name: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.changing.contains(file_name) {
            return false;
        }
        match self.model.read(cx).busy_reason(&self.id, cx) {
            Some(reason) => {
                let text = t!("mods.busy", reason = reason).to_string();
                notify(Notification::warning(text.clone()), text, window, cx);
                false
            }
            None => true,
        }
    }

    /// `item` was renamed to `path`: its row follows.
    fn toggled(&mut self, item: &ModItem, path: PathBuf, enabled: bool, cx: &mut Context<Self>) {
        let new_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| item.file_name.clone());
        let old_name = item.file_name.clone();
        // The renamed jar keeps its metadata. A listing holding the cache
        // now reads that jar again instead (its result is superseded by
        // this edit anyway).
        if let Ok(mut cache) = self.cache.try_lock() {
            let renamed = (new_name.clone(), item.size, item.modified_nanos);
            cache.rename(&item.meta_key(), renamed);
        }
        self.list.update(cx, |list, cx| {
            list.update_items(
                |items| {
                    if let Some(row) = items.iter_mut().find(|m| m.file_name == old_name) {
                        row.file_name = new_name.clone();
                        row.path = path;
                        row.enabled = enabled;
                    }
                },
                cx,
            )
        });
        for name in [&mut self.selected, &mut self.highlight]
            .into_iter()
            .flatten()
        {
            if *name == item.file_name {
                *name = new_name.clone();
            }
        }
        self.changed(cx);
    }

    /// Mod file `file_name` is gone: its row too; the selection moves to
    /// the row that takes its place.
    fn deleted(&mut self, file_name: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(file_name) {
            let items = self.list.read(cx).items();
            let name = |ix: &usize| items.get(*ix).map(|m| m.file_name.as_str());
            let at = (self.rows.iter()).position(|ix| name(ix) == Some(file_name));
            let next = at.and_then(|at| {
                let rows = &self.rows;
                rows.get(at + 1)
                    .or_else(|| at.checked_sub(1).and_then(|p| rows.get(p)))
                    .and_then(|ix| name(ix).map(str::to_owned))
            });
            self.selected = next;
        }
        self.list.update(cx, |list, cx| {
            list.update_items(|items| items.retain(|m| m.file_name != file_name), cx)
        });
        self.changed(cx);
    }

    /// This app changed the mods: remember the listing as it is now and
    /// tell the search.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.listed = Some(listing_hash(self.list.read(cx).items()));
        ModsEvents::emit(&self.id, cx);
    }

    fn change_failed(
        &mut self,
        item: &ModItem,
        error: &io::Error,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = outside_change_message(error) {
            notify(Notification::warning(text.clone()), text, window, cx);
            self.list.update(cx, |list, cx| list.mutated(cx));
        } else {
            let text = t!("mods.failed", name = item.name(), error = error).to_string();
            notify(Notification::error(text.clone()), text, window, cx);
        }
    }

    /// A listing landed: tell the search when the folder changed (not for
    /// the first listing: the search reads the folders itself at start),
    /// then do a jump that waited for it.
    fn landed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list = self.list.read(cx);
        if *list.state() == LoadState::Loaded {
            let now = listing_hash(list.items());
            if self.listed.replace(now).is_some_and(|before| before != now) {
                ModsEvents::emit(&self.id, cx);
            }
        }
        self.apply_focus(window, cx);
    }

    /// Follows the instance's config (its loader decides which metadata is
    /// read first).
    fn sync_instance(&mut self, cx: &mut Context<Self>) {
        let Some(Entry::Healthy(instance)) = self.model.read(cx).entry(&self.id) else {
            return;
        };
        let current = self.list.read(cx).instance();
        if current == instance {
            return;
        }
        let relist = preferred_format(current) != preferred_format(instance);
        let instance = instance.clone();
        self.list.update(cx, |list, cx| {
            list.set_instance(instance, cx);
            if relist {
                list.reload(cx);
            }
        });
    }

    /// Follows whether the instance is busy; lists the folder again when an
    /// operation finished or the instance is free again.
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
            .filter(|(_, m)| matches_filter(m.name(), query) || matches_filter(&m.file_name, query))
            .map(|(ix, _)| ix)
            .collect();
        if *self.rows != rows {
            self.rows = Rc::new(rows);
        }
    }

    // --- keyboard ---

    fn select_prev(&mut self, _: &SelectPrevMod, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn select_next(&mut self, _: &SelectNextMod, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn select_first(&mut self, _: &SelectFirstMod, _: &mut Window, cx: &mut Context<Self>) {
        self.select_row(0, cx);
    }

    fn select_last(&mut self, _: &SelectLastMod, _: &mut Window, cx: &mut Context<Self>) {
        self.select_row(self.rows.len().saturating_sub(1), cx);
    }

    fn toggle_selected(&mut self, _: &ToggleMod, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.selected.clone().and_then(|f| self.item(&f, cx)) {
            self.set_enabled(&item.file_name, !item.enabled, window, cx);
        }
    }

    fn delete_selected(&mut self, _: &DeleteMod, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(file) = self.selected.clone() {
            self.confirm_delete(&file, window, cx);
        }
    }

    /// Moves the selection `step` rows (clamped); with none selected (or
    /// the selected one filtered out), selects the first row.
    fn step(&mut self, step: isize, cx: &mut Context<Self>) {
        let items = self.list.read(cx).items();
        let at = (self.selected.as_ref()).and_then(|file| {
            (self.rows.iter()).position(|ix| items.get(*ix).is_some_and(|m| m.file_name == *file))
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
        self.selected = Some(item.file_name.clone());
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// The metadata format the instance's loader reads first.
    fn prefer(&self, cx: &App) -> Option<ModFormat> {
        preferred_format(self.list.read(cx).instance())
    }

    // --- rendering ---

    fn render_icon(&self, item: &ModItem, cx: &mut Context<Self>) -> AnyElement {
        let has_icon = item.info.as_ref().is_some_and(|info| info.has_icon);
        let tile = if has_icon {
            // Read from the jar again only for a row on show, and only until
            // the image is cached (capped by the core and the cache).
            let (path, prefer) = (item.path.clone(), self.prefer(cx));
            icon_tile(
                item.image_key(&self.id),
                move || read_mod_meta(&path, prefer).and_then(|meta| meta.icon),
                ICON,
                cx,
            )
        } else {
            let theme = cx.theme();
            h_flex()
                .flex_none()
                .size(ICON)
                .rounded(rems(ICON.0 * 0.22))
                .justify_center()
                .bg(theme.muted)
                .text_color(theme.muted_foreground)
                .child(Icon::new(Lucide::Puzzle).size(rems(1.)))
                .into_any_element()
        };
        div()
            .flex_none()
            .when(!item.enabled, |d| d.opacity(0.5))
            .child(tile)
            .into_any_element()
    }

    /// Row `ix` of the items; `ring` draws the keyboard focus ring on it
    /// when selected.
    fn render_row(&self, ix: usize, ring: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.list.read(cx).items().get(ix).cloned() else {
            return div().into_any_element();
        };
        let file = item.file_name.clone();
        let selected = self.selected.as_deref() == Some(file.as_str());
        let highlighted = self.highlight.as_deref() == Some(file.as_str());
        let locked = self.busy.is_some() || self.changing.contains(&file);
        let icon = self.render_icon(&item, cx);
        let format = item.info.as_ref().map(|i| i.format);
        let tag_color = format_color(format, cx);
        let signature = ProductColors::get(cx).signature();
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let name = SharedString::from(item.name().to_string());
        let detail = match item.detail() {
            Some(detail) => Some(detail),
            None if item.info.is_none() => Some(t!("mods.unreadable").to_string()),
            None => None,
        };

        let tag = div()
            .flex_none()
            .px_1p5()
            .rounded_sm()
            .bg(tag_color.opacity(0.14))
            .text_color(tag_color)
            .text_size(TEXT_SECONDARY)
            .line_height(rems(1.125))
            .child(format_label(format.unwrap_or(ModFormat::Unknown)));
        let disabled_mark = (!item.enabled).then(|| {
            let text = t!("mods.disabled").to_string();
            div()
                .id(mod_disabled_id(&file))
                .aria_label(text.clone())
                .test_support()
                .flex_none()
                .text_size(TEXT_SECONDARY)
                .text_color(muted)
                .child(text)
        });
        let first = h_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(TEXT_ROW)
                    .text_color(if item.enabled { fg } else { muted })
                    .when(item.enabled, |d| d.strong(FontWeight::MEDIUM, cx))
                    .child(name.clone()),
            )
            .child(tag)
            .children(disabled_mark);
        let second = div()
            .w_full()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .text_size(TEXT_SECONDARY)
            .text_color(muted)
            .children(detail);
        let file_label = div()
            .min_w_0()
            .max_w(FILE_W)
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .text_size(TEXT_SECONDARY)
            .text_color(muted)
            .child(file.clone());

        let this = cx.weak_entity();
        let focus = self.focus.clone();
        let switch = {
            let (this, focus, file) = (this.clone(), focus.clone(), file.clone());
            Switch::new(mod_switch_id(&file))
                .small()
                .checked(item.enabled)
                .disabled(locked)
                .tab_stop(false)
                .accessibility_label(t!("mods.enable", name = name).to_string())
                .when_some(self.busy.clone(), |s, reason| s.tooltip(reason))
                .on_change(move |enabled, window, cx| {
                    focus.focus(window, cx);
                    this.update(cx, |this, cx| {
                        this.selected = Some(file.clone());
                        this.set_enabled(&file, *enabled, window, cx);
                    })
                    .ok();
                })
        };
        let delete = {
            let (this, focus, file) = (this.clone(), focus.clone(), file.clone());
            Button::new(mod_delete_id(&file))
                .ghost()
                .xsmall()
                .icon(Icon::new(Lucide::Trash))
                .tab_stop(false)
                .accessibility_label(t!("mods.delete", name = name).to_string())
                .disabled(locked)
                .on_click(move |_, window, cx| {
                    focus.focus(window, cx);
                    this.update(cx, |this, cx| {
                        this.selected = Some(file.clone());
                        this.confirm_delete(&file, window, cx);
                    })
                    .ok();
                })
        };
        let select = {
            let file = file.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                // A clicked row hands the keyboard to the list.
                focus.focus(window, cx);
                this.update(cx, |this, cx| {
                    this.selected = Some(file.clone());
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
            .id(mod_row_id(&file))
            .w_full()
            .h(ROW_H)
            .px(GUTTER)
            .gap_2p5()
            // Every row has the 1px border so the ring does not move the
            // content.
            .border_1()
            .border_color(ring_color)
            .map(|r| {
                if highlighted {
                    r.bg(signature.opacity(0.16))
                } else if selected {
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
            .child(file_label)
            .child(switch)
            .child(delete)
            .test_support()
            .into_any_element()
    }

    fn render_rows(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row_h: Pixels = ROW_H.to_pixels(window.rem_size());
        // Only the heights are used; the list measures the width itself.
        let sizes = Rc::new(vec![size(px(0.), row_h); self.rows.len()]);
        v_flex()
            .id("mod-rows-region")
            // Before `track_focus`, so tests see the focus.
            .test_support()
            .key_context(LIST_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::toggle_selected))
            .on_action(cx.listener(Self::delete_selected))
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                v_virtual_list(cx.entity(), "mod-rows", sizes, |this, range, window, cx| {
                    // The selected row shows the ring only while the
                    // keyboard drives the list (focus-visible).
                    let ring = this.focus.is_focused(window) && window.last_input_was_keyboard();
                    let rows = this.rows.clone();
                    rows[range]
                        .iter()
                        .map(|ix| this.render_row(*ix, ring, cx))
                        .collect::<Vec<_>>()
                })
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0(),
            )
            .vertical_scrollbar(&self.scroll)
    }

    fn render_toolbar(&self, cx: &App) -> ContentToolbar {
        let list = self.list.read(cx);
        let dir = mods_dir(list.instance());
        let refresh = self.list.clone();
        let (focus, id) = (self.tab_focus.clone(), self.id.clone());
        let online = Button::new("mods-online")
            .outline()
            .small()
            .icon(Icon::new(Lucide::Download))
            .label(t!("mods.online").to_string())
            .on_click(move |_, window, cx| {
                let open = OpenDownloads {
                    target: Some(id.clone()),
                    query: None,
                };
                focus.dispatch_action(&open, window, cx);
            });
        ContentToolbar::new(
            &self.filter,
            move |_, cx| open_folder(dir.clone(), cx),
            move |_, cx| refresh.update(cx, |list, cx| list.reload(cx)),
        )
        .refreshing(list.is_reloading())
        .action(online)
    }

    /// The line under the toolbar while the instance takes no change.
    fn render_busy(&self, cx: &App) -> Option<impl IntoElement> {
        let reason = self.busy.as_ref()?;
        let text = t!("mods.busy", reason = reason).to_string();
        let theme = cx.theme();
        Some(
            div()
                .id("mods-busy")
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

/// A command run with the window (the delete confirmation's).
pub(super) type WindowAction = dyn Fn(&mut Window, &mut App);

/// Shows `notification` (its text `text` labelled for tests).
pub(super) fn notify(notification: Notification, text: String, window: &mut Window, cx: &mut App) {
    window.push_notification(labelled(notification, text), cx);
}

impl Render for ModsTab {
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
            EmptyState::new(EmptyKind::Mods).into_any_element()
        };
        v_flex()
            .id("mods-tab")
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

#[cfg(test)]
mod tests {
    use patina_core::instance::{Component, ComponentUid, InstanceConfig};

    use super::*;

    fn instance(loader: Option<ComponentUid>) -> Instance {
        let mut config = InstanceConfig::default();
        config.set_mc_version("1.21.1");
        if let Some(uid) = loader {
            config
                .set_loader(Some(Component {
                    uid,
                    version: "1.0".into(),
                    profile: None,
                }))
                .unwrap();
        }
        Instance {
            id: "pack".into(),
            dir: PathBuf::from("pack"),
            config,
        }
    }

    fn item(file: &str, info: Option<ModInfo>) -> ModItem {
        ModItem {
            file_name: file.into(),
            path: PathBuf::from(file),
            enabled: !file.ends_with(".disabled"),
            size: 1,
            modified_nanos: 0,
            info: info.map(Arc::new),
        }
    }

    fn info(version: Option<&str>, authors: &[&str]) -> ModInfo {
        ModInfo {
            id: None,
            name: "Sodium".into(),
            version: version.map(Into::into),
            authors: authors.iter().map(|a| a.to_string()).collect(),
            format: ModFormat::Fabric,
            has_icon: false,
        }
    }

    #[test]
    fn the_loader_picks_the_metadata_read_first() {
        assert_eq!(
            preferred_format(&instance(Some(ComponentUid::Fabric))),
            Some(ModFormat::Fabric)
        );
        assert_eq!(
            preferred_format(&instance(Some(ComponentUid::Forge))),
            Some(ModFormat::Forge)
        );
        assert_eq!(
            preferred_format(&instance(Some(ComponentUid::Neoforge))),
            Some(ModFormat::NeoForge)
        );
        assert_eq!(preferred_format(&instance(None)), None);
    }

    #[test]
    fn rows_name_version_and_authors() {
        let full = item(
            "sodium.jar",
            Some(info(Some("0.5"), &["jellysquid", "Ims"])),
        );
        assert_eq!(full.name(), "Sodium");
        assert_eq!(full.detail().as_deref(), Some("0.5 · jellysquid, Ims"));
        assert_eq!(
            item("s.jar", Some(info(None, &["A"]))).detail().as_deref(),
            Some("A")
        );
        assert_eq!(item("s.jar", Some(info(None, &[]))).detail(), None);
        // An unreadable jar goes by its file name.
        let broken = item("Broken-1.2.jar.disabled", None);
        assert_eq!(broken.name(), "Broken-1.2");
        assert_eq!(broken.detail(), None);
    }

    #[test]
    fn twins_have_their_own_icon_keys() {
        let on = item("a.jar", None);
        let mut off = on.clone();
        off.file_name = "a.jar.disabled".into();
        assert_ne!(on.image_key("pack"), off.image_key("pack"));
        assert_ne!(on.meta_key(), off.meta_key());
    }

    #[test]
    fn external_changes_have_their_own_messages() {
        let _locale = crate::i18n::locale_lock();
        crate::i18n::set_language(crate::i18n::Language::En);
        let message = |kind| outside_change_message(&io::Error::from(kind));
        assert_eq!(
            message(io::ErrorKind::NotFound).as_deref(),
            Some("The file was changed outside Patina")
        );
        assert_eq!(
            message(io::ErrorKind::AlreadyExists).as_deref(),
            Some("A file with the same name already exists")
        );
        assert_eq!(message(io::ErrorKind::PermissionDenied), None);
    }

    /// Writes a Fabric jar `name` into `dir`, with a PNG icon when `icon`.
    fn jar(dir: &Path, name: &str, icon: bool) -> PathBuf {
        use std::io::Write as _;
        let path = dir.join(name);
        let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("fabric.mod.json", opts).unwrap();
        let icon_field = if icon { r#","icon":"icon.png""# } else { "" };
        write!(w, r#"{{"id":"m","name":"M"{icon_field}}}"#).unwrap();
        if icon {
            w.start_file("icon.png", opts).unwrap();
            w.write_all(b"\x89PNG\r\n\x1a\nnot really an image")
                .unwrap();
        }
        w.finish().unwrap();
        path
    }

    #[test]
    fn a_listing_reuses_the_metadata_of_unchanged_jars() {
        let dir = tempfile::tempdir().unwrap();
        jar(dir.path(), "a.jar", true);
        jar(dir.path(), "b.jar", false);
        let cache = Mutex::new(MetaCache::default());
        let first = list_mods(dir.path(), None, &cache).unwrap();
        let info = |items: &[ModItem], ix: usize| items[ix].info.clone().unwrap();
        // Only whether there is an icon is kept, not its bytes.
        assert!(info(&first, 0).has_icon);
        assert!(!info(&first, 1).has_icon);

        // Listed again: nothing is opened again.
        let second = list_mods(dir.path(), None, &cache).unwrap();
        assert!(Arc::ptr_eq(&info(&first, 0), &info(&second, 0)));

        // A toggle moves its entry to the new name.
        let off = dir.path().join("a.jar.disabled");
        std::fs::rename(dir.path().join("a.jar"), &off).unwrap();
        let renamed = (
            "a.jar.disabled".to_string(),
            first[0].size,
            first[0].modified_nanos,
        );
        cache.lock().unwrap().rename(&first[0].meta_key(), renamed);
        std::fs::remove_file(dir.path().join("b.jar")).unwrap();
        let third = list_mods(dir.path(), None, &cache).unwrap();
        assert_eq!(third.len(), 1);
        assert!(!third[0].enabled);
        assert!(Arc::ptr_eq(&info(&first, 0), &info(&third, 0)));
        assert_eq!(cache.lock().unwrap().entries.len(), 1, "gone files leave");

        // Twins (`a.jar` beside `a.jar.disabled`) are two entries.
        jar(dir.path(), "a.jar", true);
        let fourth = list_mods(dir.path(), None, &cache).unwrap();
        assert_eq!(fourth.len(), 2);
        assert_eq!(cache.lock().unwrap().entries.len(), 2);
        assert!(!Arc::ptr_eq(&info(&fourth, 0), &info(&fourth, 1)));

        // Another loader reads differently: the cache starts over.
        let fifth = list_mods(dir.path(), Some(ModFormat::Forge), &cache).unwrap();
        assert!(!Arc::ptr_eq(&info(&fourth, 0), &info(&fifth, 0)));
        assert_eq!(cache.lock().unwrap().prefer, Some(ModFormat::Forge));
    }

    #[test]
    fn listings_differ_by_name_size_and_time() {
        let a = vec![item("a.jar", None)];
        let mut b = a.clone();
        assert_eq!(listing_hash(&a), listing_hash(&b));
        b[0].size = 2;
        assert_ne!(listing_hash(&a), listing_hash(&b));
        let mut c = a.clone();
        c[0].file_name = "a.jar.disabled".into();
        assert_ne!(listing_hash(&a), listing_hash(&c));
    }
}
