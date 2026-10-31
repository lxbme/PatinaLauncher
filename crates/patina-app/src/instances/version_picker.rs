//! Choosing what an instance runs: the Minecraft version ([`VersionPicker`])
//! and the loader with its version ([`LoaderPicker`]). The add-instance
//! dialog uses both; so does the versions tab (plan 4b task 12).
//!
//! Both pickers own their lists: they fetch them from the core's
//! [`Catalog`](crate::core_bridge::Catalog) in the background, show the
//! loading and failed states (with "重试") in place of the list, and drop a
//! result that a newer request has made stale. They report what is chosen
//! through an event; their owner reads the choice back.
//!
//! Keyboard: each list is one tab stop; Up / Down move the choice within the
//! rows on show. The loader kinds are a segmented control of buttons.

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonGroup};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, VirtualListScrollHandle, h_flex,
    v_flex, v_virtual_list,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, EventEmitter, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Pixels, Rems, Render,
    ScrollStrategy, SharedString, StatefulInteractiveElement, Styled, Subscription, Task,
    TestSupportExt, Window, actions, div, rems, size,
};
use patina_core::loader::{LoaderError, LoaderKind, LoaderVersion};
use patina_core::meta::{VersionKind, VersionManifest};
use rust_i18n::t;

use crate::core_bridge::{Core, Revision, run};
use crate::fonts::Strong;
use crate::layout::{TEXT_PRIMARY, TEXT_SECONDARY};

actions!(
    patina,
    [
        /// Chooses the row above the chosen one in a picker's list.
        ChoosePrev,
        /// Chooses the row below the chosen one in a picker's list.
        ChooseNext
    ]
);

/// Key context of the Minecraft version list.
pub const VERSION_LIST_CONTEXT: &str = "VersionList";
/// Key context of the loader version list.
pub const LOADER_LIST_CONTEXT: &str = "LoaderVersionList";

/// Binds Up / Down in the pickers' lists.
pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", ChoosePrev, Some(VERSION_LIST_CONTEXT)),
        KeyBinding::new("down", ChooseNext, Some(VERSION_LIST_CONTEXT)),
        KeyBinding::new("up", ChoosePrev, Some(LOADER_LIST_CONTEXT)),
        KeyBinding::new("down", ChooseNext, Some(LOADER_LIST_CONTEXT)),
    ]);
}

/// Row height of both lists (32 px).
const ROW_H: Rems = Rems(2.);
/// Height of the Minecraft version list: seven rows.
const VERSION_LIST_H: Rems = Rems(14.);
/// Height of the loader version list: four rows.
const LOADER_LIST_H: Rems = Rems(8.);

/// The loader kinds in the order of the segmented control, after "无".
pub const LOADER_KINDS: [LoaderKind; 3] =
    [LoaderKind::Fabric, LoaderKind::Forge, LoaderKind::Neoforge];

/// Element id of the row of Minecraft version `id`.
pub fn version_row_id(id: &str) -> ElementId {
    ElementId::Name(format!("version-row-{id}").into())
}

/// Element id of the row of loader version `version`.
pub fn loader_row_id(version: &str) -> ElementId {
    ElementId::Name(format!("loader-version-row-{version}").into())
}

/// Element id of the segment of `kind` (`None`: "无").
pub fn loader_segment_id(kind: Option<LoaderKind>) -> &'static str {
    match kind {
        None => "loader-none",
        Some(LoaderKind::Fabric) => "loader-fabric",
        Some(LoaderKind::Forge) => "loader-forge",
        Some(LoaderKind::Neoforge) => "loader-neoforge",
    }
}

/// Which kinds of Minecraft version the list shows: "正式版" are releases,
/// "快照" everything else (snapshots, and the old alphas and betas, which
/// would otherwise be unreachable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindFilter {
    pub release: bool,
    pub snapshot: bool,
}

impl Default for KindFilter {
    /// Releases only.
    fn default() -> Self {
        Self {
            release: true,
            snapshot: false,
        }
    }
}

impl KindFilter {
    pub fn accepts(self, kind: VersionKind) -> bool {
        match kind {
            VersionKind::Release => self.release,
            _ => self.snapshot,
        }
    }
}

/// Indices into `manifest.versions` of the versions `filter` lets through
/// whose id contains `query` (case-insensitive, surrounding blanks ignored),
/// in manifest order (newest first).
pub fn visible_versions(manifest: &VersionManifest, filter: KindFilter, query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    (manifest.versions.iter().enumerate())
        .filter(|(_, v)| filter.accepts(v.kind) && v.id.to_lowercase().contains(&query))
        .map(|(ix, _)| ix)
        .collect()
}

/// The loader version chosen by default: the recommended one, else the
/// latest, else the first listed.
pub fn default_loader_version(list: &[LoaderVersion]) -> Option<&LoaderVersion> {
    (list.iter().find(|v| v.recommended))
        .or_else(|| list.iter().find(|v| v.latest))
        .or_else(|| list.first())
}

/// The row `step` rows from `current` within `rows` (clamped to the ends);
/// with no current row (or one not on show) the first row.
fn step_row<T: PartialEq>(rows: &[T], current: Option<&T>, step: isize) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let Some(at) = current.and_then(|c| rows.iter().position(|r| r == c)) else {
        return Some(0);
    };
    Some(at.saturating_add_signed(step).min(rows.len() - 1))
}

/// The loading or failed state of a list, in the list's place: a spinner
/// and `loading`, or `error` with "重试" (id `retry_id`) running `retry`.
fn list_status(
    error: Option<SharedString>,
    loading: String,
    ids: (&'static str, &'static str),
    retry: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let (error_id, retry_id) = ids;
    let body = match error {
        None => h_flex()
            .gap_2()
            .text_size(TEXT_PRIMARY)
            .text_color(theme.muted_foreground)
            .child(Spinner::new().small())
            .child(loading)
            .into_any_element(),
        Some(error) => v_flex()
            .items_center()
            .gap_2()
            .px_4()
            .child(
                div()
                    .id(error_id)
                    .test_support()
                    .aria_label(error.clone())
                    .text_size(TEXT_PRIMARY)
                    .text_color(theme.danger)
                    .child(error),
            )
            .child(
                Button::new(retry_id)
                    .outline()
                    .small()
                    .label(t!("add_instance.retry").to_string())
                    .on_click(move |_, window, cx| retry(window, cx)),
            )
            .into_any_element(),
    };
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .child(body)
        .into_any_element()
}

/// One row of a picker's list: `label`, then `tags`; `chosen` marks it,
/// `ring` draws the keyboard focus ring on the chosen row. The caller adds
/// the click handler, then `test_support()`.
fn list_row(
    id: ElementId,
    label: String,
    tags: Vec<AnyElement>,
    chosen: bool,
    ring: bool,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let theme = cx.theme();
    let (hover, active) = (theme.list_hover, theme.list_active);
    let ring_color = if chosen && ring {
        theme.ring
    } else {
        theme.transparent
    };
    h_flex()
        .id(id)
        .w_full()
        .h(ROW_H)
        .px_3()
        .gap_2()
        // Every row has the 1px border so the ring does not move the content.
        .border_1()
        .border_color(ring_color)
        .rounded_md()
        .text_size(TEXT_PRIMARY)
        .text_color(theme.foreground)
        .cursor_pointer()
        .when(chosen, |r| r.bg(active))
        .when(!chosen, |r| r.hover(move |s| s.bg(hover)))
        .aria_label(label.clone())
        .aria_selected(chosen)
        .child(div().flex_1().min_w_0().overflow_hidden().child(label))
        .children(tags)
}

/// A neutral tag ("最新", "推荐").
fn tag(text: String, primary: bool) -> AnyElement {
    let tag = if primary {
        Tag::primary().outline()
    } else {
        Tag::secondary()
    };
    tag.xsmall().child(text).into_any_element()
}

/// The frame of a list: a bordered box `height` high.
fn list_frame(height: Rems, cx: &App) -> gpui_kit::Div {
    let theme = cx.theme();
    v_flex()
        .h(height)
        .w_full()
        .p_1()
        .border_1()
        .border_color(theme.border)
        .rounded_md()
        .overflow_hidden()
}

/// Emitted when the user (or loading the list, for the default) chooses a
/// Minecraft version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionChosen(pub String);

enum ManifestState {
    Loading,
    Loaded(Rc<VersionManifest>),
    Failed(SharedString),
}

/// The Minecraft version list: "正式版" / "快照" filter chips (releases only
/// by default), a search box, and the versions, newest first, with "最新"
/// on the latest release and snapshot. See the module docs.
pub struct VersionPicker {
    manifest: ManifestState,
    filter: KindFilter,
    search: Entity<InputState>,
    /// The rows on show: indices into the manifest's versions.
    rows: Rc<Vec<usize>>,
    chosen: Option<String>,
    focus: FocusHandle,
    scroll: VirtualListScrollHandle,
    revision: Revision,
    load: Task<()>,
    _search: Subscription,
}

impl EventEmitter<VersionChosen> for VersionPicker {}

impl VersionPicker {
    /// A picker with `chosen` marked; with `None` the latest release is
    /// chosen once the list has loaded. Starts loading the list.
    pub fn new(chosen: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).clean_on_escape());
        crate::i18n::localized_placeholder(&search, "add_instance.search", window, cx);
        let _search = cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.rebuild_rows(cx);
            }
        });
        let mut this = Self {
            manifest: ManifestState::Loading,
            filter: KindFilter::default(),
            search,
            rows: Rc::default(),
            chosen,
            focus: cx.focus_handle().tab_stop(true),
            scroll: VirtualListScrollHandle::new(),
            revision: Revision::default(),
            load: Task::ready(()),
            _search,
        };
        this.reload(cx);
        this
    }

    /// The focus handle of the version list (not the search box).
    pub fn list_focus(&self) -> &FocusHandle {
        &self.focus
    }

    /// The chosen version's id.
    pub fn chosen(&self) -> Option<&str> {
        self.chosen.as_deref()
    }

    pub fn filter(&self) -> KindFilter {
        self.filter
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.manifest, ManifestState::Loading)
    }

    /// Why the list failed to load, if it did.
    pub fn error(&self) -> Option<&SharedString> {
        match &self.manifest {
            ManifestState::Failed(error) => Some(error),
            _ => None,
        }
    }

    /// The ids of the rows on show, in order.
    pub fn row_ids(&self) -> Vec<String> {
        match &self.manifest {
            ManifestState::Loaded(m) => self
                .rows
                .iter()
                .map(|&i| m.versions[i].id.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// (Re)loads the version list.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let id = self.revision.start();
        self.manifest = ManifestState::Loading;
        let request = Core::global(cx).catalog.manifest();
        let job = run(cx, request);
        // Replacing the task drops the previous request.
        self.load = cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if !this.revision.is_current(id) {
                    return;
                }
                this.manifest = match result {
                    Ok(Ok(manifest)) => ManifestState::Loaded(Rc::new(manifest)),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "loading the version manifest failed");
                        ManifestState::Failed(
                            t!("add_instance.load_failed", error = error)
                                .to_string()
                                .into(),
                        )
                    }
                    Err(error) => ManifestState::Failed(error.to_string().into()),
                };
                this.rebuild_rows(cx);
                this.choose_default(cx);
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    pub fn set_filter(&mut self, filter: KindFilter, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter;
            self.rebuild_rows(cx);
        }
    }

    /// Chooses version `id` (emits [`VersionChosen`] if that is a change).
    pub fn choose(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.chosen.as_deref() == Some(id) {
            return;
        }
        self.chosen = Some(id.to_string());
        cx.emit(VersionChosen(id.to_string()));
        cx.notify();
    }

    /// With nothing chosen, chooses the latest release (if listed) and
    /// scrolls to the chosen row.
    fn choose_default(&mut self, cx: &mut Context<Self>) {
        let ManifestState::Loaded(manifest) = &self.manifest else {
            return;
        };
        if self.chosen.is_none() {
            let latest = manifest.latest.release.clone();
            if manifest.find(&latest).is_some() {
                self.choose(&latest, cx);
            }
        }
        self.scroll_to_chosen();
    }

    fn scroll_to_chosen(&self) {
        let ManifestState::Loaded(manifest) = &self.manifest else {
            return;
        };
        let at = (self.rows.iter())
            .position(|&i| Some(manifest.versions[i].id.as_str()) == self.chosen.as_deref());
        if let Some(at) = at {
            self.scroll.scroll_to_item(at, ScrollStrategy::Nearest);
        }
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let rows = match &self.manifest {
            ManifestState::Loaded(manifest) => {
                let query = self.search.read(cx).value();
                visible_versions(manifest, self.filter, &query)
            }
            _ => Vec::new(),
        };
        if *self.rows != rows {
            self.rows = Rc::new(rows);
        }
        cx.notify();
    }

    fn step(&mut self, step: isize, cx: &mut Context<Self>) {
        let ids = self.row_ids();
        let current = self.chosen.clone();
        let Some(at) = step_row(&ids, current.as_ref(), step) else {
            return;
        };
        self.choose(&ids[at], cx);
        self.scroll.scroll_to_item(at, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.filter;
        let release = super::list::filter_chip(
            "version-filter-release",
            t!("add_instance.release").to_string(),
            filter.release,
            cx,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.set_filter(
                KindFilter {
                    release: !filter.release,
                    ..filter
                },
                cx,
            )
        }));
        let snapshot = super::list::filter_chip(
            "version-filter-snapshot",
            t!("add_instance.snapshot").to_string(),
            filter.snapshot,
            cx,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.set_filter(
                KindFilter {
                    snapshot: !filter.snapshot,
                    ..filter
                },
                cx,
            )
        }));
        h_flex()
            .w_full()
            .gap_1()
            .child(release)
            .child(snapshot)
            .child(
                div().flex_1().min_w(rems(6.)).ml_2().child(
                    Input::new(&self.search)
                        .id("version-search")
                        .small()
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).xsmall()),
                ),
            )
    }

    fn render_list(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let manifest = match &self.manifest {
            ManifestState::Loaded(manifest) => manifest.clone(),
            ManifestState::Loading | ManifestState::Failed(_) => {
                let this = cx.weak_entity();
                return list_status(
                    self.error().cloned(),
                    t!("add_instance.loading").to_string(),
                    ("version-error", "version-retry"),
                    move |_, cx| {
                        this.update(cx, |this, cx| this.reload(cx)).ok();
                    },
                    cx,
                );
            }
        };
        if self.rows.is_empty() {
            return div()
                .p_2()
                .text_size(TEXT_SECONDARY)
                .text_color(cx.theme().muted_foreground)
                .child(t!("add_instance.no_match").to_string())
                .into_any_element();
        }
        let row_h: Pixels = ROW_H.to_pixels(window.rem_size());
        // Only the heights are used; the list measures the width itself.
        let sizes = Rc::new(vec![size(Pixels::ZERO, row_h); self.rows.len()]);
        v_virtual_list(
            cx.entity(),
            "version-rows",
            sizes,
            move |this, range, window, cx| {
                // The chosen row shows the ring only while the keyboard drives
                // the list (focus-visible).
                let ring = this.focus.is_focused(window) && window.last_input_was_keyboard();
                let rows = this.rows.clone();
                rows[range]
                    .iter()
                    .map(|&ix| this.render_row(&manifest, ix, ring, cx))
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.scroll)
        .size_full()
        .into_any_element()
    }

    fn render_row(
        &self,
        manifest: &VersionManifest,
        ix: usize,
        ring: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entry = &manifest.versions[ix];
        let id = entry.id.clone();
        let latest = id == manifest.latest.release || id == manifest.latest.snapshot;
        let mut tags = Vec::new();
        let kind = match entry.kind {
            VersionKind::Snapshot => Some(t!("add_instance.kind.snapshot")),
            VersionKind::OldBeta => Some(t!("add_instance.kind.old_beta")),
            VersionKind::OldAlpha => Some(t!("add_instance.kind.old_alpha")),
            VersionKind::Release | VersionKind::Unknown => None,
        };
        if let Some(kind) = kind {
            tags.push(
                div()
                    .text_size(TEXT_SECONDARY)
                    .text_color(cx.theme().muted_foreground)
                    .child(kind.to_string())
                    .into_any_element(),
            );
        }
        if latest {
            tags.push(tag(t!("add_instance.latest").to_string(), false));
        }
        let chosen = self.chosen.as_deref() == Some(id.as_str());
        list_row(version_row_id(&id), id.clone(), tags, chosen, ring, cx)
            .on_click(cx.listener(move |this, _, window, cx| {
                // A clicked row hands the keyboard to the list.
                this.focus.focus(window, cx);
                this.choose(&id, cx);
            }))
            .test_support()
            .into_any_element()
    }
}

impl Render for VersionPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w_full()
            .gap_2()
            .child(self.render_filters(cx))
            .child(
                list_frame(VERSION_LIST_H, cx).child(
                    v_flex()
                        .id("version-list")
                        // Before `track_focus`, so tests see the focus.
                        .test_support()
                        .key_context(VERSION_LIST_CONTEXT)
                        .track_focus(&self.focus)
                        .on_action(cx.listener(|this, _: &ChoosePrev, _, cx| this.step(-1, cx)))
                        .on_action(cx.listener(|this, _: &ChooseNext, _, cx| this.step(1, cx)))
                        .relative()
                        .size_full()
                        .child(self.render_list(window, cx))
                        .vertical_scrollbar(&self.scroll),
                ),
            )
    }
}

/// The versions of one loader kind for the Minecraft version on show.
#[derive(Debug, Clone)]
enum LoaderList {
    Loading,
    Ready(Rc<Vec<LoaderVersion>>),
    /// The loader has no versions for this Minecraft version.
    Unsupported,
    Failed(SharedString),
}

/// Emitted when the chosen loader kind or version changes (read them back
/// with [`LoaderPicker::kind`] and [`LoaderPicker::version`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoaderChanged;

/// "无 / Fabric / Forge / NeoForge" for a Minecraft version, and the chosen
/// loader's versions with "推荐" and "最新" marked; the recommended version
/// is chosen by default, the latest when none is recommended.
///
/// The lists of all three kinds load as soon as the Minecraft version is
/// set, so a kind without versions for it shows greyed out (with a hint)
/// before anyone picks it. See the module docs.
pub struct LoaderPicker {
    mc: Option<String>,
    kind: Option<LoaderKind>,
    /// The kind cannot be changed: the segmented control is not shown.
    kind_locked: bool,
    /// Segments left out of the control (`None`: "无").
    hidden: Vec<Option<LoaderKind>>,
    version: Option<String>,
    lists: HashMap<LoaderKind, LoaderList>,
    revisions: HashMap<LoaderKind, Revision>,
    loads: HashMap<LoaderKind, Task<()>>,
    focus: FocusHandle,
    scroll: VirtualListScrollHandle,
}

impl EventEmitter<LoaderChanged> for LoaderPicker {}

impl LoaderPicker {
    /// A picker with "无" chosen and no Minecraft version yet.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            mc: None,
            kind: None,
            kind_locked: false,
            hidden: Vec::new(),
            version: None,
            lists: HashMap::new(),
            revisions: HashMap::new(),
            loads: HashMap::new(),
            focus: cx.focus_handle().tab_stop(true),
            scroll: VirtualListScrollHandle::new(),
        }
    }

    /// The focus handle of the loader version list.
    pub fn list_focus(&self) -> &FocusHandle {
        &self.focus
    }

    /// The chosen loader kind (`None`: "无").
    pub fn kind(&self) -> Option<LoaderKind> {
        self.kind
    }

    /// The chosen loader version; `None` with "无" or while the chosen
    /// kind's list is not ready.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Whether the choice is complete: "无", or a loader with a version.
    pub fn is_ready(&self) -> bool {
        self.kind.is_none() || self.version.is_some()
    }

    /// Whether `kind` has versions for the Minecraft version: `None` while
    /// that is not known (loading, failed, no Minecraft version).
    pub fn is_supported(&self, kind: LoaderKind) -> Option<bool> {
        match self.lists.get(&kind)? {
            LoaderList::Ready(_) => Some(true),
            LoaderList::Unsupported => Some(false),
            LoaderList::Loading | LoaderList::Failed(_) => None,
        }
    }

    /// Shows the loaders for Minecraft `mc`: reloads every kind's list.
    /// The chosen kind stays (unless it turns out not to support `mc`); its
    /// version is chosen again from the new list.
    pub fn set_mc(&mut self, mc: Option<String>, cx: &mut Context<Self>) {
        if self.mc == mc {
            return;
        }
        self.mc = mc;
        self.lists.clear();
        self.loads.clear();
        let kind = self.kind;
        self.set_choice(kind, None, cx);
        for kind in LOADER_KINDS {
            self.load(kind, cx);
        }
        cx.notify();
    }

    /// Fixes the kind to `kind`: only its versions are shown, with no
    /// segmented control. For changing the version of an installed loader.
    pub fn lock_kind(&mut self, kind: LoaderKind, cx: &mut Context<Self>) {
        self.kind_locked = true;
        self.choose_kind(Some(kind), cx);
        cx.notify();
    }

    /// Leaves the segment of `kind` (`None`: "无") out of the control.
    pub fn hide_kind(&mut self, kind: Option<LoaderKind>) {
        self.hidden.push(kind);
    }

    /// Chooses loader `kind` (`None`: "无") and its default version.
    pub fn choose_kind(&mut self, kind: Option<LoaderKind>, cx: &mut Context<Self>) {
        if self.kind == kind {
            return;
        }
        self.set_choice(kind, None, cx);
        self.settle(cx);
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Chooses `version` of the chosen loader.
    pub fn choose_version(&mut self, version: &str, cx: &mut Context<Self>) {
        let kind = self.kind;
        self.set_choice(kind, Some(version.to_string()), cx);
    }

    fn set_choice(
        &mut self,
        kind: Option<LoaderKind>,
        version: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.kind != kind || self.version != version {
            self.kind = kind;
            self.version = version;
            cx.emit(LoaderChanged);
            cx.notify();
        }
    }

    /// Brings the choice in line with the lists: a kind without versions
    /// falls back to "无"; a loaded kind gets its default version unless
    /// the chosen one is listed.
    fn settle(&mut self, cx: &mut Context<Self>) {
        let Some(kind) = self.kind else {
            return;
        };
        match self.lists.get(&kind) {
            Some(LoaderList::Unsupported) => self.set_choice(None, None, cx),
            Some(LoaderList::Ready(list)) => {
                let listed = (self.version.as_deref())
                    .is_some_and(|chosen| list.iter().any(|v| v.version == chosen));
                if !listed {
                    let version = default_loader_version(list).map(|v| v.version.clone());
                    self.set_choice(Some(kind), version, cx);
                    self.scroll_to_chosen();
                }
            }
            Some(LoaderList::Loading | LoaderList::Failed(_)) | None => {}
        }
    }

    fn scroll_to_chosen(&self) {
        let Some(LoaderList::Ready(list)) = self.kind.and_then(|k| self.lists.get(&k)) else {
            return;
        };
        let at = list
            .iter()
            .position(|v| Some(v.version.as_str()) == self.version.as_deref());
        if let Some(at) = at {
            self.scroll.scroll_to_item(at, ScrollStrategy::Nearest);
        }
    }

    /// (Re)loads the versions of `kind` for the Minecraft version.
    pub fn load(&mut self, kind: LoaderKind, cx: &mut Context<Self>) {
        let Some(mc) = self.mc.clone() else {
            return;
        };
        let id = self.revisions.entry(kind).or_default().start();
        self.lists.insert(kind, LoaderList::Loading);
        let request = Core::global(cx).catalog.loader_versions(kind, mc);
        let job = run(cx, request);
        let task = cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if !this.revisions.get(&kind).is_some_and(|r| r.is_current(id)) {
                    return;
                }
                let list = match result {
                    Ok(Ok(list)) if !list.is_empty() => LoaderList::Ready(Rc::new(list)),
                    Ok(Ok(_)) | Ok(Err(LoaderError::NoVersions { .. })) => LoaderList::Unsupported,
                    Ok(Err(error)) => {
                        tracing::warn!(%error, ?kind, "loading the loader versions failed");
                        LoaderList::Failed(
                            t!(
                                "add_instance.loader_failed",
                                loader = kind.display_name(),
                                error = error
                            )
                            .to_string()
                            .into(),
                        )
                    }
                    Err(error) => LoaderList::Failed(error.to_string().into()),
                };
                this.lists.insert(kind, list);
                this.settle(cx);
                cx.notify();
            })
            .ok();
        });
        self.loads.insert(kind, task);
        cx.notify();
    }

    /// The chosen kind's versions, if loaded.
    fn chosen_list(&self) -> Option<Rc<Vec<LoaderVersion>>> {
        match self.lists.get(&self.kind?)? {
            LoaderList::Ready(list) => Some(list.clone()),
            _ => None,
        }
    }

    fn step(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(list) = self.chosen_list() else {
            return;
        };
        let ids: Vec<&str> = list.iter().map(|v| v.version.as_str()).collect();
        let current = self.version.as_deref();
        let Some(at) = step_row(&ids, current.as_ref(), step) else {
            return;
        };
        self.choose_version(ids[at], cx);
        self.scroll.scroll_to_item(at, ScrollStrategy::Nearest);
    }

    fn render_segments(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (active, primary) = (theme.list_active, theme.primary);
        let segments = std::iter::once(None)
            .chain(LOADER_KINDS.map(Some))
            .filter(|kind| !self.hidden.contains(kind))
            .map(|kind: Option<LoaderKind>| {
                let label = kind.map_or_else(
                    || t!("add_instance.loader_none").to_string(),
                    |k| k.display_name(),
                );
                // Unknown support (loading, failed) leaves the segment usable.
                let unsupported = kind.is_some_and(|k| self.is_supported(k) == Some(false));
                let selected = self.kind == kind;
                // Each segment handles its own click, so the keyboard's
                // Enter / Space on a focused segment chooses it too.
                Button::new(loader_segment_id(kind))
                    .label(label)
                    .selected(selected)
                    .disabled(self.mc.is_none() && kind.is_some() || unsupported)
                    .when(selected, |b| {
                        b.bg(active)
                            .text_color(primary)
                            .strong(FontWeight::MEDIUM, cx)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.choose_kind(kind, cx)))
            });
        ButtonGroup::new("loader-kinds")
            .outline()
            .small()
            .children(segments)
    }

    fn render_list(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(kind) = self.kind else {
            return div().into_any_element();
        };
        let list = match self.lists.get(&kind) {
            Some(LoaderList::Ready(list)) => list.clone(),
            Some(LoaderList::Failed(error)) => {
                let this = cx.weak_entity();
                return list_status(
                    Some(error.clone()),
                    String::new(),
                    ("loader-error", "loader-retry"),
                    move |_, cx| {
                        this.update(cx, |this, cx| this.load(kind, cx)).ok();
                    },
                    cx,
                );
            }
            Some(LoaderList::Loading | LoaderList::Unsupported) | None => {
                return list_status(
                    None,
                    t!("add_instance.loader_loading", loader = kind.display_name()).to_string(),
                    ("loader-error", "loader-retry"),
                    |_, _| {},
                    cx,
                );
            }
        };
        let row_h: Pixels = ROW_H.to_pixels(window.rem_size());
        let sizes = Rc::new(vec![size(Pixels::ZERO, row_h); list.len()]);
        v_virtual_list(
            cx.entity(),
            "loader-rows",
            sizes,
            move |this, range, window, cx| {
                let ring = this.focus.is_focused(window) && window.last_input_was_keyboard();
                list[range]
                    .iter()
                    .map(|v| this.render_row(v, ring, cx))
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.scroll)
        .size_full()
        .into_any_element()
    }

    fn render_row(&self, v: &LoaderVersion, ring: bool, cx: &mut Context<Self>) -> AnyElement {
        let mut tags = Vec::new();
        if v.recommended {
            tags.push(tag(t!("add_instance.recommended").to_string(), true));
        }
        if v.latest {
            tags.push(tag(t!("add_instance.latest").to_string(), false));
        }
        let chosen = self.version.as_deref() == Some(v.version.as_str());
        let version = v.version.clone();
        list_row(
            loader_row_id(&version),
            version.clone(),
            tags,
            chosen,
            ring,
            cx,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.focus.focus(window, cx);
            this.choose_version(&version, cx);
        }))
        .test_support()
        .into_any_element()
    }
}

impl Render for LoaderPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let unsupported = LOADER_KINDS
            .iter()
            .any(|&k| self.is_supported(k) == Some(false));
        let hint = self.mc.clone().filter(|_| unsupported).map(|mc| {
            let text = SharedString::from(t!("add_instance.loader_unsupported", mc = mc));
            div()
                .id("loader-unsupported")
                .test_support()
                .aria_label(text.clone())
                .text_size(TEXT_SECONDARY)
                .text_color(muted)
                .child(text)
        });
        v_flex()
            .w_full()
            .gap_2()
            .when(!self.kind_locked, |this| {
                this.child(self.render_segments(cx)).children(hint)
            })
            .when(self.kind.is_some(), |this| {
                this.child(
                    div()
                        .text_size(TEXT_SECONDARY)
                        .text_color(muted)
                        .child(t!("add_instance.loader_version").to_string()),
                )
                .child(
                    list_frame(LOADER_LIST_H, cx).child(
                        v_flex()
                            .id("loader-version-list")
                            .test_support()
                            .key_context(LOADER_LIST_CONTEXT)
                            .track_focus(&self.focus)
                            .on_action(cx.listener(|this, _: &ChoosePrev, _, cx| this.step(-1, cx)))
                            .on_action(cx.listener(|this, _: &ChooseNext, _, cx| this.step(1, cx)))
                            .relative()
                            .size_full()
                            .child(self.render_list(window, cx))
                            .vertical_scrollbar(&self.scroll),
                    ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use patina_core::meta::{Latest, ManifestEntry};

    use super::*;

    fn entry(id: &str, kind: VersionKind) -> ManifestEntry {
        ManifestEntry {
            id: id.into(),
            kind,
            url: String::new(),
            sha1: String::new(),
            release_time: String::new(),
        }
    }

    fn manifest() -> VersionManifest {
        VersionManifest {
            latest: Latest {
                release: "1.21.1".into(),
                snapshot: "24w45a".into(),
            },
            versions: vec![
                entry("24w45a", VersionKind::Snapshot),
                entry("1.21.1", VersionKind::Release),
                entry("1.21", VersionKind::Release),
                entry("1.20.1", VersionKind::Release),
                entry("b1.7.3", VersionKind::OldBeta),
            ],
        }
    }

    fn ids(m: &VersionManifest, rows: Vec<usize>) -> Vec<&str> {
        rows.into_iter()
            .map(|i| m.versions[i].id.as_str())
            .collect()
    }

    #[test]
    fn release_filter_and_search() {
        let m = manifest();
        let releases = KindFilter::default();
        assert_eq!(
            ids(&m, visible_versions(&m, releases, "")),
            ["1.21.1", "1.21", "1.20.1"],
            "releases only by default"
        );
        let both = KindFilter {
            release: true,
            snapshot: true,
        };
        assert_eq!(
            ids(&m, visible_versions(&m, both, "")),
            ["24w45a", "1.21.1", "1.21", "1.20.1", "b1.7.3"]
        );
        let snapshots = KindFilter {
            release: false,
            snapshot: true,
        };
        assert_eq!(
            ids(&m, visible_versions(&m, snapshots, "")),
            ["24w45a", "b1.7.3"],
            "old versions sit with the snapshots"
        );
        assert_eq!(
            ids(&m, visible_versions(&m, releases, "1.21")),
            ["1.21.1", "1.21"]
        );
        assert_eq!(
            ids(&m, visible_versions(&m, both, " 24W ")),
            ["24w45a"],
            "case and blanks"
        );
        assert_eq!(
            ids(&m, visible_versions(&m, releases, "24w")),
            Vec::<&str>::new()
        );
        let none = KindFilter {
            release: false,
            snapshot: false,
        };
        assert!(visible_versions(&m, none, "").is_empty());
    }

    fn loader(version: &str, recommended: bool, latest: bool) -> LoaderVersion {
        LoaderVersion {
            kind: LoaderKind::Fabric,
            mc: "1.21.1".into(),
            version: version.into(),
            maven_version: version.into(),
            stable: true,
            recommended,
            latest,
        }
    }

    #[test]
    fn recommended_is_preselected() {
        let list = [
            loader("0.16.9", false, true),
            loader("0.16.5", true, false),
            loader("0.16.0", false, false),
        ];
        assert_eq!(default_loader_version(&list).unwrap().version, "0.16.5");
        let no_recommended = [
            loader("0.15.0", false, false),
            loader("0.15.11", false, true),
        ];
        assert_eq!(
            default_loader_version(&no_recommended).unwrap().version,
            "0.15.11",
            "the latest without a recommended one"
        );
        let neither = [loader("1", false, false), loader("2", false, false)];
        assert_eq!(default_loader_version(&neither).unwrap().version, "1");
        assert!(default_loader_version(&[]).is_none());
    }

    #[test]
    fn steps_stay_within_the_rows() {
        let rows = ["a", "b", "c"];
        assert_eq!(step_row(&rows, Some(&"a"), -1), Some(0));
        assert_eq!(step_row(&rows, Some(&"b"), 1), Some(2));
        assert_eq!(step_row(&rows, Some(&"c"), 1), Some(2));
        assert_eq!(step_row(&rows, Some(&"x"), 1), Some(0));
        assert_eq!(step_row(&rows, None, 1), Some(0));
        assert_eq!(step_row::<&str>(&[], None, 1), None);
    }
}
