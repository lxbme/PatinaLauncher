//! [`DownloadsModel`]: what the downloads page is asking for (content kind,
//! words, filters, target instance) and what the content source answered.
//!
//! The target instance and the filters are linked: choosing a target fills
//! the game version (and, for mods, the loader) from the instance; changing
//! either filter by hand to something the target does not have drops the
//! target. Every change of the question starts a new search
//! ([`Revision`]: older answers are dropped); typing waits [`DEBOUNCE`] for
//! the next keystroke. Nothing is searched before the page is first shown
//! ([`DownloadsModel::activate`]).

use std::time::Duration;

use gpui_kit::{Context, Entity, SharedString, Subscription, Task};
use patina_core::content::online::{
    ContentKind, OnlineError, OnlinePage, OnlineQuery, Platform, SortOrder,
};
use patina_core::instance::Instance;
use patina_core::loader::LoaderKind;

use crate::core_bridge::{BridgeError, Core, Revision, RevisionId, run};
use crate::instances::{Entry, InstancesModel};

/// How long after the last keystroke the words are searched.
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// Hits asked for per search.
pub const PAGE_SIZE: u32 = 20;

/// What the content source answered to the latest question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Results {
    /// Nothing asked yet (the page was never shown).
    Idle,
    /// A search is in flight.
    Loading,
    /// The source cannot be searched (yet).
    Unavailable,
    /// The search failed, with why.
    Failed(SharedString),
    Loaded(OnlinePage),
}

/// Whether `kind` is filtered by the mod loader: only mods are; packs and
/// shaders do not depend on it and a modpack brings its own.
pub fn takes_loader(kind: ContentKind) -> bool {
    kind == ContentKind::Mod
}

/// The downloads page's domain state (an entity).
pub struct DownloadsModel {
    instances: Entity<InstancesModel>,
    kind: ContentKind,
    text: String,
    platform: Platform,
    mc_version: Option<String>,
    loader: Option<LoaderKind>,
    category: Option<String>,
    sort: SortOrder,
    target: Option<String>,
    /// The hit shown in the detail column.
    selected: Option<String>,
    results: Results,
    /// A request is in flight (its answer not in yet): the previous
    /// results stay on show meanwhile.
    searching: bool,
    active: bool,
    revision: Revision,
    task: Task<()>,
    _instances: Subscription,
}

impl DownloadsModel {
    pub fn new(instances: Entity<InstancesModel>, cx: &mut Context<Self>) -> Self {
        let _instances = cx.observe(&instances, |this, _, cx| this.instances_changed(cx));
        Self {
            instances,
            kind: ContentKind::Mod,
            text: String::new(),
            platform: Platform::Modrinth,
            mc_version: None,
            loader: None,
            category: None,
            sort: SortOrder::Relevance,
            target: None,
            selected: None,
            results: Results::Idle,
            searching: false,
            active: false,
            revision: Revision::default(),
            task: Task::ready(()),
            _instances,
        }
    }

    pub fn kind(&self) -> ContentKind {
        self.kind
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn platform(&self) -> Platform {
        self.platform
    }

    pub fn mc_version(&self) -> Option<&str> {
        self.mc_version.as_deref()
    }

    pub fn loader(&self) -> Option<LoaderKind> {
        self.loader
    }

    pub fn category(&self) -> Option<&str> {
        self.category.as_deref()
    }

    pub fn sort(&self) -> SortOrder {
        self.sort
    }

    /// The id of the instance content would be installed into.
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    /// The id of the hit shown in the detail column.
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn results(&self) -> &Results {
        &self.results
    }

    /// Whether a request is in flight; the results shown are then the
    /// previous answer.
    pub fn is_searching(&self) -> bool {
        self.searching
    }

    /// The question as the source gets it.
    pub fn query(&self) -> OnlineQuery {
        OnlineQuery {
            kind: self.kind,
            text: self.text.trim().to_string(),
            platform: self.platform,
            mc_version: self.mc_version.clone(),
            loader: self.loader.filter(|_| takes_loader(self.kind)),
            category: self.category.clone(),
            sort: self.sort,
            offset: 0,
            limit: PAGE_SIZE,
        }
    }

    // --- the question ------------------------------------------------------

    /// Shows the page for the first time: searches once. Later calls do
    /// nothing.
    pub fn activate(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            self.active = true;
            self.search(None, cx);
        }
    }

    /// Opens the page as an entry point asks, then searches once:
    /// - with a `target` (from the Mods tab) and no `query`: the words are
    ///   emptied and the target applied;
    /// - with a `query` and no `target` (from the global search): the words
    ///   are set and the target cleared (the filters stay);
    /// - with both: both apply;
    /// - a `target` that is not a listed, healthy instance is ignored (the
    ///   words are then left alone unless a `query` is given).
    pub fn open(&mut self, target: Option<&str>, query: Option<&str>, cx: &mut Context<Self>) {
        self.active = true;
        match (target, query) {
            (Some(id), query) => {
                let applied = self.apply_target(id, cx);
                match query {
                    Some(q) => self.text = q.trim().to_string(),
                    None if applied => self.text.clear(),
                    None => {}
                }
            }
            (None, Some(q)) => {
                self.text = q.trim().to_string();
                self.target = None;
            }
            (None, None) => {}
        }
        self.search(None, cx);
    }

    pub fn set_kind(&mut self, kind: ContentKind, cx: &mut Context<Self>) {
        if self.kind == kind {
            return;
        }
        self.kind = kind;
        // Categories belong to a kind; the loader only to mods.
        self.category = None;
        self.selected = None;
        self.loader = if takes_loader(kind) {
            self.target_instance(cx).and_then(|i| loader_of(&i))
        } else {
            None
        };
        self.search(None, cx);
    }

    /// Searches `text` after [`DEBOUNCE`].
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.text == text {
            return;
        }
        self.text = text.to_string();
        self.search(Some(DEBOUNCE), cx);
    }

    /// Searches the words now (Enter).
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        self.search(None, cx);
    }

    pub fn set_platform(&mut self, platform: Platform, cx: &mut Context<Self>) {
        if self.platform == platform {
            return;
        }
        self.platform = platform;
        self.category = None;
        self.search(None, cx);
    }

    /// Sets the game version filter. `None` ("all versions") is a change
    /// the target cannot agree with, so it clears the target.
    pub fn set_mc_version(&mut self, version: Option<String>, cx: &mut Context<Self>) {
        if self.mc_version == version {
            return;
        }
        self.mc_version = version;
        self.drop_disagreeing_target(cx);
        self.search(None, cx);
    }

    pub fn set_loader(&mut self, loader: Option<LoaderKind>, cx: &mut Context<Self>) {
        if self.loader == loader || !takes_loader(self.kind) {
            return;
        }
        self.loader = loader;
        self.drop_disagreeing_target(cx);
        self.search(None, cx);
    }

    pub fn set_category(&mut self, category: Option<String>, cx: &mut Context<Self>) {
        if self.category == category {
            return;
        }
        self.category = category;
        self.search(None, cx);
    }

    pub fn set_sort(&mut self, sort: SortOrder, cx: &mut Context<Self>) {
        if self.sort == sort {
            return;
        }
        self.sort = sort;
        self.search(None, cx);
    }

    /// Aims at instance `id` (`None`: no target), taking its game version
    /// and, for mods, its loader into the filters. An id that is not a
    /// listed, healthy instance is ignored; returns whether it was taken.
    pub fn set_target(&mut self, id: Option<&str>, cx: &mut Context<Self>) -> bool {
        let Some(id) = id else {
            if self.target.take().is_some() {
                cx.notify();
            }
            return true;
        };
        if !self.apply_target(id, cx) {
            return false;
        }
        self.search(None, cx);
        true
    }

    /// Selects hit `id` for the detail column.
    pub fn select(&mut self, id: Option<&str>, cx: &mut Context<Self>) {
        if self.selected.as_deref() != id {
            self.selected = id.map(str::to_string);
            cx.notify();
        }
    }

    /// Searches again with the same question.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        self.search(None, cx);
    }

    // --- internals ---------------------------------------------------------

    fn target_instance(&self, cx: &Context<Self>) -> Option<Instance> {
        let id = self.target.as_deref()?;
        healthy(self.instances.read(cx), id)
    }

    /// Takes instance `id`'s version and loader into the filters and makes
    /// it the target. No search.
    fn apply_target(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let Some(instance) = healthy(self.instances.read(cx), id) else {
            return false;
        };
        self.target = Some(instance.id.clone());
        let version = instance.config.mc_version();
        self.mc_version = (!version.is_empty()).then(|| version.to_string());
        if takes_loader(self.kind) {
            self.loader = loader_of(&instance);
        }
        true
    }

    /// Forgets the target when the filters no longer match it.
    fn drop_disagreeing_target(&mut self, cx: &mut Context<Self>) {
        let Some(instance) = self.target_instance(cx) else {
            return;
        };
        let version = instance.config.mc_version();
        let same_version = self.mc_version.as_deref() == Some(version);
        let same_loader = !takes_loader(self.kind) || self.loader == loader_of(&instance);
        if !(same_version && same_loader) {
            self.target = None;
        }
    }

    /// The instances moved: a target that is gone is forgotten; one whose
    /// version or loader changed hands them to the filters again.
    fn instances_changed(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.target.clone() else {
            return;
        };
        if self.target_instance(cx).is_none() {
            self.target = None;
            cx.notify();
            return;
        }
        let before = (self.mc_version.clone(), self.loader);
        self.apply_target(&id, cx);
        if (self.mc_version.clone(), self.loader) != before {
            self.search(None, cx);
        }
    }

    /// Starts a request for the current question, after `delay`. Replacing
    /// the task drops the previous request; the revision drops its answer
    /// should it land anyway.
    fn search(&mut self, delay: Option<Duration>, cx: &mut Context<Self>) {
        cx.notify();
        if !self.active {
            return;
        }
        let id = self.revision.start();
        if self.results == Results::Idle {
            self.results = Results::Loading;
        }
        let query = self.query();
        let source = Core::global(cx).content_source.clone();
        let timer = delay.map(|d| cx.background_executor().timer(d));
        self.task = cx.spawn(async move |this, cx| {
            if let Some(timer) = timer {
                timer.await;
            }
            // The request starts now, after any debounce.
            let Ok(work) = this.update(cx, |this, cx| {
                this.searching = true;
                cx.notify();
                run(cx, source.search(query))
            }) else {
                return;
            };
            let answer = work.await;
            this.update(cx, |this, cx| this.apply(id, answer, cx)).ok();
        });
    }

    fn apply(
        &mut self,
        id: RevisionId,
        answer: Result<Result<OnlinePage, OnlineError>, BridgeError>,
        cx: &mut Context<Self>,
    ) {
        if !self.revision.is_current(id) {
            return;
        }
        self.searching = false;
        self.results = match answer {
            Ok(Ok(page)) => Results::Loaded(page),
            Ok(Err(OnlineError::Unavailable)) => Results::Unavailable,
            Ok(Err(e)) => Results::Failed(e.to_string().into()),
            Err(e) => Results::Failed(e.to_string().into()),
        };
        if let Results::Loaded(page) = &self.results
            && !(self.selected.as_deref()).is_some_and(|s| page.hits.iter().any(|h| h.id == s))
        {
            self.selected = None;
        }
        cx.notify();
    }
}

/// The healthy instance `id`, as a copy.
fn healthy(instances: &InstancesModel, id: &str) -> Option<Instance> {
    match instances.entry(id)? {
        Entry::Healthy(i) => Some(i.clone()),
        Entry::Broken(_) => None,
    }
}

fn loader_of(instance: &Instance) -> Option<LoaderKind> {
    instance.config.loader().map(|(kind, _)| kind)
}
