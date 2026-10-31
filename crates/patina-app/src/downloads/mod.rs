//! The downloads page (layout A, three columns): filters | results | detail.
//!
//! [`DownloadsModel`] owns the question (content kind, words, filters,
//! target instance) and the content source's answer; [`DownloadsPage`] is
//! the view over it and keeps the controls (the dropdowns, the search box)
//! in step with it. The source behind the page is `Core.content_source`; for
//! now it is always unavailable and the results say so, so a real source
//! only has to replace that one object.
//!
//! The kind tabs are not focusable by themselves (as the instance tabs):
//! `ctrl-tab` / `ctrl-pagedown` and `ctrl-shift-tab` / `ctrl-pageup` step
//! through them while the page has the keyboard.

mod detail;
mod filters;
mod model;
mod results;

use gpui_kit::base::{Tab as TabElement, Tabs};
use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    App, AppContext, Context, Entity, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, Render, ScrollHandle, SharedString, Styled, Subscription, Task,
    Window, actions, div, px, rems,
};
use patina_core::content::online::{ContentKind, SortOrder};
use patina_core::loader::LoaderKind;
use patina_core::meta::VersionKind;
use rust_i18n::t;

pub use model::{DEBOUNCE, DownloadsModel, PAGE_SIZE, Results, takes_loader};
pub use results::hit_row_id;

use crate::core_bridge::{Core, run};
use crate::fonts::Strong;
use crate::instances::InstancesModel;
use crate::layout::{GUTTER, STRIP_H, TEXT_PRIMARY};
use crate::theme::ProductColors;

actions!(
    downloads,
    [
        /// Shows the next content kind.
        NextKind,
        /// Shows the previous content kind.
        PrevKind,
        /// Selects the previous hit.
        PrevHit,
        /// Selects the next hit.
        NextHit,
        /// Selects the first hit if none is selected.
        ConfirmHit
    ]
);

/// Key context of the downloads page.
pub const DOWNLOADS_CONTEXT: &str = "Downloads";
/// Key context of the hit list.
pub const HITS_CONTEXT: &str = "DownloadHits";

/// Binds the downloads page's keys.
pub fn init(cx: &mut App) {
    let context = Some(DOWNLOADS_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("ctrl-tab", NextKind, context),
        KeyBinding::new("ctrl-pagedown", NextKind, context),
        KeyBinding::new("ctrl-shift-tab", PrevKind, context),
        KeyBinding::new("ctrl-pageup", PrevKind, context),
        KeyBinding::new("up", PrevHit, Some(HITS_CONTEXT)),
        KeyBinding::new("down", NextHit, Some(HITS_CONTEXT)),
        KeyBinding::new("enter", ConfirmHit, Some(HITS_CONTEXT)),
        KeyBinding::new("space", ConfirmHit, Some(HITS_CONTEXT)),
    ]);
}

/// The content kinds, in tab order.
pub const KINDS: [ContentKind; 4] = [
    ContentKind::Mod,
    ContentKind::ResourcePack,
    ContentKind::Shader,
    ContentKind::Modpack,
];

/// Element id of the tab of content kind `kind`.
pub fn kind_tab_id(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Mod => "downloads-tab-mod",
        ContentKind::ResourcePack => "downloads-tab-resource-pack",
        ContentKind::Shader => "downloads-tab-shader",
        ContentKind::Modpack => "downloads-tab-modpack",
    }
}

fn kind_label(kind: ContentKind) -> String {
    match kind {
        ContentKind::Mod => t!("downloads.kind.mod"),
        ContentKind::ResourcePack => t!("downloads.kind.resource_pack"),
        ContentKind::Shader => t!("downloads.kind.shader"),
        ContentKind::Modpack => t!("downloads.kind.modpack"),
    }
    .to_string()
}

/// Width of the filters column.
const FILTERS_W: gpui_kit::Rems = gpui_kit::Rems(14.);
/// Width of the detail column.
const DETAIL_W: gpui_kit::Rems = gpui_kit::Rems(16.);

/// How a [`Choice`] is named. Resolved when drawn, so a language switch
/// reaches the open page.
#[derive(Clone)]
pub(crate) enum ChoiceLabel {
    Text(SharedString),
    /// A locale key.
    Key(&'static str),
    Loader(LoaderKind),
}

/// One entry of a dropdown: a value and its name.
#[derive(Clone)]
pub(crate) struct Choice<V> {
    value: V,
    label: ChoiceLabel,
}

impl<V> Choice<V> {
    fn key(value: V, key: &'static str) -> Self {
        Self {
            value,
            label: ChoiceLabel::Key(key),
        }
    }

    fn text(value: V, text: impl Into<SharedString>) -> Self {
        Self {
            value,
            label: ChoiceLabel::Text(text.into()),
        }
    }
}

impl<V: Clone + PartialEq> SearchableListItem for Choice<V> {
    type Value = V;

    fn title(&self) -> SharedString {
        match &self.label {
            ChoiceLabel::Text(text) => text.clone(),
            ChoiceLabel::Key(key) => t!(*key).to_string().into(),
            ChoiceLabel::Loader(kind) => kind.display_name().into(),
        }
    }

    fn value(&self) -> &V {
        &self.value
    }
}

type Choices<V> = Vec<Choice<V>>;
pub(crate) type Dropdown<V> = Entity<SelectState<Choices<V>>>;

/// Whether the game versions for the version dropdown are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Versions {
    /// Not asked yet (the page was never shown).
    NotLoaded,
    Loading,
    Loaded,
    /// The manifest did not load; asked again on the next visit or retry.
    Failed,
}

/// The downloads page (an entity): the view over [`DownloadsModel`].
pub struct DownloadsPage {
    model: Entity<DownloadsModel>,
    instances: Entity<InstancesModel>,
    focus: FocusHandle,
    input: Entity<InputState>,
    version: Dropdown<Option<String>>,
    loader: Dropdown<Option<LoaderKind>>,
    category: Dropdown<Option<String>>,
    target: Dropdown<Option<String>>,
    sort: Dropdown<SortOrder>,
    /// The release ids of the game, newest first, once the manifest is in.
    releases: Vec<String>,
    versions: Versions,
    hits_focus: FocusHandle,
    /// What the dropdowns list now, to replace their items only on change.
    version_listing: (usize, Option<String>),
    target_listing: Vec<(String, String)>,
    filters_scroll: ScrollHandle,
    results_scroll: ScrollHandle,
    detail_scroll: ScrollHandle,
    _manifest: Task<()>,
    _subscriptions: Vec<Subscription>,
}

fn dropdown<V: Clone + PartialEq + 'static>(
    items: Choices<V>,
    searchable: bool,
    window: &mut Window,
    cx: &mut Context<DownloadsPage>,
) -> Dropdown<V> {
    cx.new(|cx| {
        SelectState::new(items, Some(IndexPath::default()), window, cx).searchable(searchable)
    })
}

fn loader_choices() -> Choices<Option<LoaderKind>> {
    let mut items = vec![Choice::key(None, "downloads.filters.all_loaders")];
    for kind in [LoaderKind::Fabric, LoaderKind::Forge, LoaderKind::Neoforge] {
        items.push(Choice {
            value: Some(kind),
            label: ChoiceLabel::Loader(kind),
        });
    }
    items
}

fn sort_choices() -> Choices<SortOrder> {
    vec![
        Choice::key(SortOrder::Relevance, "downloads.sort.relevance"),
        Choice::key(SortOrder::Downloads, "downloads.sort.downloads"),
        Choice::key(SortOrder::Updated, "downloads.sort.updated"),
    ]
}

/// "All versions", then `current` if the manifest does not list it (a
/// snapshot, an instance's own version), then the releases.
fn version_choices(releases: &[String], current: Option<&str>) -> Choices<Option<String>> {
    let mut items = vec![Choice::key(None, "downloads.filters.all_versions")];
    if let Some(current) = current.filter(|c| !releases.iter().any(|r| r == c)) {
        items.push(Choice::text(Some(current.to_string()), current.to_string()));
    }
    items.extend(
        releases
            .iter()
            .map(|r| Choice::text(Some(r.clone()), r.clone())),
    );
    items
}

impl DownloadsPage {
    pub fn new(
        instances: Entity<InstancesModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let model = cx.new(|cx| DownloadsModel::new(instances.clone(), cx));
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("downloads.search_placeholder").to_string())
        });
        let version = dropdown(version_choices(&[], None), true, window, cx);
        let loader = dropdown(loader_choices(), false, window, cx);
        let category = dropdown(
            vec![Choice::key(None, "downloads.filters.all_categories")],
            false,
            window,
            cx,
        );
        let target = dropdown(
            vec![Choice::key(None, "downloads.filters.no_target")],
            false,
            window,
            cx,
        );
        let sort = dropdown(sort_choices(), false, window, cx);
        let mut this = Self {
            model: model.clone(),
            instances: instances.clone(),
            focus: cx.focus_handle(),
            input,
            version,
            loader,
            category,
            target,
            sort,
            releases: Vec::new(),
            versions: Versions::NotLoaded,
            hits_focus: cx.focus_handle().tab_stop(true),
            version_listing: (0, None),
            target_listing: Vec::new(),
            filters_scroll: ScrollHandle::new(),
            results_scroll: ScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
            _manifest: Task::ready(()),
            _subscriptions: Vec::new(),
        };
        this._subscriptions = vec![
            cx.observe_in(&model, window, |this, _, window, cx| this.sync(window, cx)),
            cx.observe_in(&instances, window, |this, _, window, cx| {
                this.sync_targets(window, cx);
                this.sync(window, cx);
            }),
            cx.subscribe_in(&this.input, window, Self::on_input),
            cx.subscribe_in(
                &this.version,
                window,
                |this, _, e: &SelectEvent<_>, _, cx| {
                    let SelectEvent::Confirm(v) = e;
                    let v = v.clone().flatten();
                    this.model.update(cx, |m, cx| m.set_mc_version(v, cx));
                },
            ),
            cx.subscribe_in(
                &this.loader,
                window,
                |this, _, e: &SelectEvent<_>, _, cx| {
                    let SelectEvent::Confirm(v) = e;
                    let v = v.flatten();
                    this.model.update(cx, |m, cx| m.set_loader(v, cx));
                },
            ),
            cx.subscribe_in(
                &this.target,
                window,
                |this, _, e: &SelectEvent<_>, _, cx| {
                    let SelectEvent::Confirm(v) = e;
                    let v = v.clone().flatten();
                    this.model
                        .update(cx, |m, cx| m.set_target(v.as_deref(), cx));
                },
            ),
            cx.subscribe_in(&this.sort, window, |this, _, e: &SelectEvent<_>, _, cx| {
                if let SelectEvent::Confirm(Some(v)) = e {
                    let v = *v;
                    this.model.update(cx, |m, cx| m.set_sort(v, cx));
                }
            }),
        ];
        this.sync_targets(window, cx);
        this
    }

    pub fn model(&self) -> &Entity<DownloadsModel> {
        &self.model
    }

    /// The search box's state.
    pub fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    /// The page was navigated to: searches for the first time, and reads
    /// the game versions if they are not in.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.activate(cx));
        self.ensure_versions(cx);
    }

    /// An entry point's request (see `shell::OpenDownloads`).
    pub fn open(
        &mut self,
        target: Option<&str>,
        query: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |m, cx| m.open(target, query, cx));
        self.ensure_versions(cx);
        self.sync(window, cx);
    }

    /// Whether the game versions are in, loading or failed.
    pub fn versions(&self) -> Versions {
        self.versions
    }

    /// Reads the release list for the version dropdown, unless it is in or
    /// being read.
    pub(crate) fn ensure_versions(&mut self, cx: &mut Context<Self>) {
        if matches!(self.versions, Versions::Loading | Versions::Loaded) {
            return;
        }
        self.versions = Versions::Loading;
        cx.notify();
        let catalog = Core::global(cx).catalog.clone();
        self._manifest = cx.spawn(async move |this, cx| {
            let Ok(work) = this.update(cx, |_, cx| run(cx, catalog.manifest())) else {
                return;
            };
            let manifest = work.await.ok().and_then(Result::ok);
            this.update_in(cx, |this, window, cx| {
                match manifest {
                    Some(manifest) => {
                        this.releases = (manifest.versions.iter())
                            .filter(|v| v.kind == VersionKind::Release)
                            .map(|v| v.id.clone())
                            .collect();
                        this.versions = Versions::Loaded;
                    }
                    None => {
                        tracing::warn!("the game versions for the downloads filters did not load");
                        this.versions = Versions::Failed;
                    }
                }
                this.sync(window, cx);
                cx.notify();
            })
            .ok();
        });
    }

    /// Moves the selected hit by `step` (the first one if none is selected).
    fn step_hit(&mut self, step: isize, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let Results::Loaded(page) = model.results() else {
            return;
        };
        if page.hits.is_empty() {
            return;
        }
        let at = (model.selected()).and_then(|s| page.hits.iter().position(|h| h.id == s));
        let next = match at {
            None => 0,
            Some(at) => at.saturating_add_signed(step).min(page.hits.len() - 1),
        };
        let id = page.hits[next].id.clone();
        self.model.update(cx, |m, cx| m.select(Some(&id), cx));
    }

    fn on_input(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let text = input.read(cx).value().to_string();
                self.model.update(cx, |m, cx| m.set_text(&text, cx));
            }
            InputEvent::PressEnter { .. } => self.model.update(cx, |m, cx| m.submit(cx)),
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    /// The target dropdown lists the healthy instances.
    fn sync_targets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let listing: Vec<(String, String)> = (self.instances.read(cx).instances().iter())
            .map(|i| (i.id.clone(), i.config.name.clone()))
            .collect();
        if listing == self.target_listing {
            return;
        }
        let mut items = vec![Choice::key(None, "downloads.filters.no_target")];
        items.extend(
            (listing.iter()).map(|(id, name)| Choice::text(Some(id.clone()), name.clone())),
        );
        self.target_listing = listing;
        self.target
            .update(cx, |s, cx| s.set_items(items, window, cx));
    }

    /// Puts the model's question into the controls.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let text = model.text().to_string();
        let mc = model.mc_version().map(str::to_string);
        let (loader, sort) = (model.loader(), model.sort());
        let target = model.target().map(str::to_string);

        if self.input.read(cx).value() != text {
            self.input.update(cx, |i, cx| i.set_value(text, window, cx));
        }
        // A version the manifest does not list gets an entry of its own.
        let extra = mc.clone().filter(|m| !self.releases.contains(m));
        let listing = (self.releases.len(), extra);
        if listing != self.version_listing {
            let items = version_choices(&self.releases, mc.as_deref());
            self.version_listing = listing;
            self.version
                .update(cx, |s, cx| s.set_items(items, window, cx));
        }
        select_value(&self.version, mc, window, cx);
        select_value(&self.loader, loader, window, cx);
        select_value(&self.sort, sort, window, cx);
        select_value(&self.target, target, window, cx);
    }

    fn step_kind(&mut self, step: isize, cx: &mut Context<Self>) {
        let at = KINDS
            .iter()
            .position(|k| *k == self.model.read(cx).kind())
            .unwrap_or(0);
        let next = (at as isize + step).rem_euclid(KINDS.len() as isize) as usize;
        self.model.update(cx, |m, cx| m.set_kind(KINDS[next], cx));
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let teal = ProductColors::get(cx).signature();
        let current = self.model.read(cx).kind();
        let model = self.model.downgrade();
        Tabs::new("downloads-kinds")
            .flex()
            .flex_none()
            .items_center()
            .h(STRIP_H)
            .px(GUTTER)
            .gap_5()
            .border_b_1()
            .border_color(border)
            .children(KINDS.into_iter().enumerate().map(|(i, kind)| {
                let selected = kind == current;
                let model = model.clone();
                TabElement::new(kind_tab_id(kind))
                    .selected(selected)
                    .set_position(i + 1, KINDS.len())
                    .accessibility_label(kind_label(kind))
                    .flex_none()
                    .h_full()
                    .px_0p5()
                    .relative()
                    .whitespace_nowrap()
                    .text_size(TEXT_PRIMARY)
                    .text_color(if selected { fg } else { muted })
                    .when(selected, |t| t.strong(FontWeight::MEDIUM, cx))
                    .when(!selected, |t| t.hover(move |s| s.text_color(fg)))
                    .child(kind_label(kind))
                    .when(selected, |t| {
                        t.child(
                            div()
                                .absolute()
                                // Over the strip's 1px bottom divider.
                                .bottom(px(-1.))
                                .left_0()
                                .right_0()
                                .h(rems(0.125))
                                .bg(teal),
                        )
                    })
                    .on_click(move |_, _, cx| {
                        model.update(cx, |m, cx| m.set_kind(kind, cx)).ok();
                    })
            }))
    }
}

/// Selects `value` in `dropdown` unless it already is.
fn select_value<V: Clone + PartialEq + 'static>(
    dropdown: &Dropdown<V>,
    value: V,
    window: &mut Window,
    cx: &mut App,
) {
    if dropdown.read(cx).selected_value() != Some(&value) {
        dropdown.update(cx, |s, cx| s.set_selected_value(&value, window, cx));
    }
}

impl Render for DownloadsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let search = h_flex()
            .flex_none()
            .gap_2()
            .p(GUTTER)
            .border_b_1()
            .border_color(border)
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.input)
                        .id("downloads-search")
                        .small()
                        .w_full()
                        .cleanable(true),
                ),
            )
            .child(self.render_sort());
        h_flex()
            .id("downloads")
            .key_context(DOWNLOADS_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NextKind, _, cx| this.step_kind(1, cx)))
            .on_action(cx.listener(|this, _: &PrevKind, _, cx| this.step_kind(-1, cx)))
            .on_action(cx.listener(|this, _: &PrevHit, _, cx| this.step_hit(-1, cx)))
            .on_action(cx.listener(|this, _: &NextHit, _, cx| this.step_hit(1, cx)))
            .on_action(cx.listener(|this, _: &ConfirmHit, _, cx| this.step_hit(0, cx)))
            .size_full()
            .items_stretch()
            .child(
                v_flex()
                    .flex_none()
                    .w(FILTERS_W)
                    .h_full()
                    .border_r_1()
                    .border_color(border)
                    .child(self.render_filters(window, cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_tabs(cx))
                    .child(search)
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_results(window, cx)),
                    ),
            )
            .child(
                v_flex()
                    .flex_none()
                    .w(DETAIL_W)
                    .h_full()
                    .border_l_1()
                    .border_color(border)
                    .child(self.render_detail(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_choices_keep_an_unlisted_current_version() {
        let releases = vec!["1.21.1".to_string(), "1.20.1".to_string()];
        let values = |current| {
            version_choices(&releases, current)
                .into_iter()
                .map(|c| c.value)
                .collect::<Vec<_>>()
        };
        assert_eq!(values(Some("1.20.1")).len(), 3);
        let with_snapshot = values(Some("24w14a"));
        assert_eq!(with_snapshot[1].as_deref(), Some("24w14a"));
        assert_eq!(with_snapshot.len(), 4);
        assert_eq!(values(None)[0], None);
    }
}
