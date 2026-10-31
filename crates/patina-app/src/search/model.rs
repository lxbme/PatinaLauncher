//! [`SearchModel`]: the global search's domain state: the persistent mod
//! index and when it is refreshed, the query and its results.
//!
//! The index ([`ModIndex`]) is opened on the blocking pool at startup and
//! then refreshed on the app's runtime, copy-on-write: the model keeps the
//! current index as an `Arc<ModIndex>` that searches read, a refresh works
//! on a copy, and the copy replaces the current index when it lands (then
//! the query is searched again). A search therefore never waits for a
//! refresh; it reads the index it finds, on the runtime.
//!
//! Refreshes happen at startup, after the Mods tab changed a mods folder
//! ([`ModsChanged`]) and when the window becomes active again, at most once
//! per [`REFRESH_GAP`]: a trigger within the gap is held until its end, and
//! triggers while a refresh is in flight queue one rerun. When building the
//! index failed ([`IndexState::Failed`], shown in the results), the next
//! trigger tries again at once.
//!
//! The query is searched [`DEBOUNCE`] after the last keystroke; every
//! keystroke starts a new request ([`Revision`]) and results of older ones
//! are dropped.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{Context, Entity, SharedString, Subscription, Task, Window};
use patina_core::content::index::{ModHit, ModIndex};
use patina_core::instance::{InstanceStore, ListOutcome};
use tokio::sync::RwLock;

use crate::content::{ModsChanged, ModsEvents};
use crate::core_bridge::{BridgeError, Core, Revision, RevisionId, run, run_blocking};
use crate::instances::{IconSource, InstancesModel, icon_for, loader_name};

/// How long after the last keystroke the query is searched.
pub const DEBOUNCE: Duration = Duration::from_millis(120);
/// The shortest time between the starts of two index refreshes.
pub const REFRESH_GAP: Duration = Duration::from_secs(30);
/// At most this many instances are listed.
pub const INSTANCE_LIMIT: usize = 5;
/// At most this many installed mods are listed.
pub const MOD_LIMIT: usize = 20;

/// The mods group of a search being made: settled at once (the index is
/// not built), or a search of the index in flight on the runtime.
enum PendingMods {
    Settled(ModGroup),
    Searching(Task<Result<Vec<ModHit>, BridgeError>>),
}

/// Holds the refreshes or the mod searches before they start, for as long
/// as it lives (tests use it to keep work in flight).
pub type Hold = tokio::sync::OwnedRwLockWriteGuard<()>;

/// One result of the search, by domain identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchItem {
    /// Instance `id`.
    Instance(String),
    /// Mod file `file_name` of instance `instance_id`.
    Mod {
        instance_id: String,
        file_name: String,
    },
    /// The online search, with the query.
    Online,
}

/// An instance that matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceResult {
    pub id: String,
    pub name: String,
    /// Loader and Minecraft version, as the list's second line says them.
    pub detail: String,
    pub icon: IconSource,
}

/// An installed mod that matches, with its instance's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModResult {
    pub hit: ModHit,
    pub instance_name: String,
}

/// Where the mod index stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexState {
    /// Being built for the first time.
    Building,
    /// Built; searches read it (a refresh may be in flight).
    Ready,
    /// Building it failed, with why (localized); the next trigger retries.
    Failed(SharedString),
}

/// The installed-mods group of the results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModGroup {
    /// The index is being built.
    Indexing,
    /// Building the index failed, with why.
    Failed(SharedString),
    /// At most [`MOD_LIMIT`] matches.
    Hits(Vec<ModResult>),
}

/// The results of one query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResults {
    pub query: String,
    /// At most [`INSTANCE_LIMIT`].
    pub instances: Vec<InstanceResult>,
    pub mods: ModGroup,
}

impl SearchResults {
    /// Every result in the popup's order: instances, mods, then online.
    pub fn items(&self) -> Vec<SearchItem> {
        let instances = self
            .instances
            .iter()
            .map(|i| SearchItem::Instance(i.id.clone()));
        let mods = self.mod_hits().iter().map(|m| SearchItem::Mod {
            instance_id: m.hit.instance_id.clone(),
            file_name: m.hit.file_name.clone(),
        });
        instances.chain(mods).chain([SearchItem::Online]).collect()
    }

    /// The installed mods that match (none while indexing or failed).
    pub fn mod_hits(&self) -> &[ModResult] {
        match &self.mods {
            ModGroup::Hits(hits) => hits,
            _ => &[],
        }
    }

    /// The name of the instance result `item` belongs to.
    pub fn instance_name(&self, item: &SearchItem) -> Option<&str> {
        match item {
            SearchItem::Instance(id) => self
                .instances
                .iter()
                .find(|i| i.id == *id)
                .map(|i| i.name.as_str()),
            SearchItem::Mod {
                instance_id,
                file_name,
            } => self
                .mod_hits()
                .iter()
                .find(|m| m.hit.instance_id == *instance_id && m.hit.file_name == *file_name)
                .map(|m| m.instance_name.as_str()),
            SearchItem::Online => None,
        }
    }
}

/// Where a refresh starts from.
enum Base {
    /// The index just opened.
    Opened(ModIndex),
    /// A copy of the current index.
    Current(Arc<ModIndex>),
}

/// The global search's state (an entity). See the module docs.
pub struct SearchModel {
    instances: Entity<InstancesModel>,
    /// The index searches read; `None` until the first build lands.
    index: Option<Arc<ModIndex>>,
    state: IndexState,
    /// A refresh (or the startup open) is in flight.
    refreshing: bool,
    /// When the last refresh started, on gpui's clock.
    last_refresh: Option<Instant>,
    /// A trigger came while a refresh was in flight.
    rerun_queued: bool,
    /// A refresh waits for the end of the gap.
    scheduled: bool,
    /// Refreshes finished, for tests and logs.
    refreshes: u64,
    refresh_task: Task<()>,
    schedule_task: Task<()>,
    /// Passed by every refresh and every mod search before it starts;
    /// uncontended unless a test holds it ([`Self::hold_refreshes`],
    /// [`Self::hold_searches`]).
    refresh_gate: Arc<RwLock<()>>,
    search_gate: Arc<RwLock<()>>,
    query: String,
    revision: Revision,
    search_task: Task<()>,
    results: Option<SearchResults>,
    _subscriptions: [Subscription; 2],
}

impl SearchModel {
    /// Opens the index and builds it; refreshes it whenever the Mods tab
    /// changes a folder or `window` becomes active (throttled).
    pub fn new(
        instances: Entity<InstancesModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let _subscriptions = [
            cx.subscribe(&ModsEvents::entity(cx), |this, _, _: &ModsChanged, cx| {
                this.request_refresh(cx)
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.request_refresh(cx);
                }
            }),
        ];
        let mut this = Self {
            instances,
            index: None,
            state: IndexState::Building,
            refreshing: false,
            last_refresh: None,
            rerun_queued: false,
            scheduled: false,
            refreshes: 0,
            refresh_task: Task::ready(()),
            schedule_task: Task::ready(()),
            refresh_gate: Arc::default(),
            search_gate: Arc::default(),
            query: String::new(),
            revision: Revision::default(),
            search_task: Task::ready(()),
            results: None,
            _subscriptions,
        };
        this.start_refresh(cx);
        this
    }

    /// The index searches read, once built.
    pub fn index(&self) -> Option<&Arc<ModIndex>> {
        self.index.as_ref()
    }

    pub fn index_state(&self) -> &IndexState {
        &self.state
    }

    /// Whether the index has been built.
    pub fn is_indexed(&self) -> bool {
        self.state == IndexState::Ready
    }

    /// Whether a refresh of the index is in flight.
    pub fn is_refreshing(&self) -> bool {
        self.refreshing
    }

    /// How many refreshes of the index have finished (or failed).
    pub fn refreshes(&self) -> u64 {
        self.refreshes
    }

    /// The query, trimmed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The results of the last query searched; `None` for an empty query
    /// and until the first results of a query land. They may belong to an
    /// earlier query while the current one is being searched
    /// ([`SearchResults::query`]).
    pub fn results(&self) -> Option<&SearchResults> {
        self.results.as_ref()
    }

    /// Whether the results on show are those of the current query.
    pub fn is_current(&self) -> bool {
        self.results.as_ref().is_some_and(|r| r.query == self.query)
    }

    /// Holds every refresh before it starts until the returned guard is
    /// dropped. Panics if one is passing the gate this very moment.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_refreshes(&self) -> Hold {
        self.refresh_gate
            .clone()
            .try_write_owned()
            .expect("no refresh is passing the gate")
    }

    /// Holds every mod search before it starts until the returned guard is
    /// dropped.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_searches(&self) -> Hold {
        self.search_gate
            .clone()
            .try_write_owned()
            .expect("no search is passing the gate")
    }

    // --- the index ---------------------------------------------------------

    /// Refreshes the index now or, within [`REFRESH_GAP`] of the last
    /// refresh, at the end of the gap; while one is in flight, once more
    /// after it. After a failure, at once.
    pub fn request_refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            self.rerun_queued = true;
            return;
        }
        let now = cx.background_executor().now();
        let failed = matches!(self.state, IndexState::Failed(_));
        let wait = match self.last_refresh {
            Some(last) if !failed => {
                REFRESH_GAP.saturating_sub(now.saturating_duration_since(last))
            }
            _ => Duration::ZERO,
        };
        if wait.is_zero() {
            self.scheduled = false;
            self.schedule_task = Task::ready(());
            self.start_refresh(cx);
            return;
        }
        if self.scheduled {
            return;
        }
        self.scheduled = true;
        let timer = cx.background_executor().timer(wait);
        self.schedule_task = cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| {
                this.scheduled = false;
                this.request_refresh(cx);
            })
            .ok();
        });
    }

    /// Refreshes a copy of the current index, or opens the index (on the
    /// blocking pool) and builds it when there is none yet.
    fn start_refresh(&mut self, cx: &mut Context<Self>) {
        self.refreshing = true;
        self.last_refresh = Some(cx.background_executor().now());
        if let Some(current) = self.index.clone() {
            self.run_refresh(Base::Current(current), cx);
            return;
        }
        let data = Core::global(cx).data.clone();
        let open = run_blocking(cx, move || ModIndex::open(&data));
        self.refresh_task = cx.spawn(async move |this, cx| {
            let opened = open.await;
            this.update(cx, |this, cx| match opened {
                Ok(index) => this.run_refresh(Base::Opened(index), cx),
                Err(error) => {
                    tracing::warn!(%error, "opening the mod index failed");
                    this.refreshed(None, Some(error.to_string()), cx);
                }
            })
            .ok();
        });
    }

    /// Refreshes `base` on the runtime, away from the index searches read.
    /// Takes no instance locks (the core does not) and skips broken
    /// instances.
    fn run_refresh(&mut self, base: Base, cx: &mut Context<Self>) {
        let store: InstanceStore = Core::global(cx).store.clone();
        let gate = self.refresh_gate.clone();
        let work = run(cx, async move {
            drop(gate.read().await);
            let mut index = match base {
                Base::Opened(index) => index,
                Base::Current(current) => ModIndex::clone(&current),
            };
            let result = index.refresh(&store).await;
            (index, result)
        });
        self.refresh_task = cx.spawn(async move |this, cx| {
            let (index, error) = match work.await {
                Ok((index, Ok(stats))) => {
                    tracing::debug!(?stats, "mod index refreshed");
                    (Some(index), None)
                }
                // What the refresh read before failing is still newer.
                Ok((index, Err(error))) => (Some(index), Some(error.to_string())),
                Err(error) => (None, Some(error.to_string())),
            };
            this.update(cx, |this, cx| this.refreshed(index, error, cx))
                .ok();
        });
    }

    /// A refresh landed: its copy becomes the index searches read.
    fn refreshed(
        &mut self,
        index: Option<ModIndex>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = index {
            self.index = Some(Arc::new(index));
        }
        match error {
            None => self.state = IndexState::Ready,
            Some(error) => {
                tracing::warn!(%error, "refreshing the mod index failed");
                // Once built, a failed refresh keeps the index on show.
                if self.state != IndexState::Ready {
                    self.state = IndexState::Failed(error.into());
                }
            }
        }
        self.refreshing = false;
        self.refreshes += 1;
        if std::mem::take(&mut self.rerun_queued) {
            self.request_refresh(cx);
        }
        // What is on show may have changed (or was waiting for the index).
        if !self.query.is_empty() {
            self.search(None, cx);
        }
        cx.notify();
    }

    // --- the query ---------------------------------------------------------

    /// Searches `text` (trimmed) after [`DEBOUNCE`]; an empty text clears
    /// the results at once.
    pub fn set_query(&mut self, text: &str, cx: &mut Context<Self>) {
        let query = text.trim();
        if query == self.query {
            return;
        }
        self.query = query.to_string();
        if self.query.is_empty() {
            self.revision.start();
            self.search_task = Task::ready(());
            if self.results.take().is_some() {
                cx.notify();
            }
            return;
        }
        self.search(Some(DEBOUNCE), cx);
    }

    /// Starts a request for the query, after `delay`. Replacing the task
    /// drops the previous request; the revision drops its result should it
    /// land anyway.
    fn search(&mut self, delay: Option<Duration>, cx: &mut Context<Self>) {
        let id = self.revision.start();
        let timer = delay.map(|d| cx.background_executor().timer(d));
        self.search_task = cx.spawn(async move |this, cx| {
            if let Some(timer) = timer {
                timer.await;
            }
            let Ok((query, instances, mods)) = this.update(cx, |this, cx| this.start_search(cx))
            else {
                return;
            };
            let mods = match mods {
                PendingMods::Settled(group) => Ok(group),
                PendingMods::Searching(work) => Err(work.await.unwrap_or_else(|error| {
                    tracing::warn!(%error, "searching the mod index failed");
                    Vec::new()
                })),
            };
            this.update(cx, |this, cx| this.apply(id, query, instances, mods, cx))
                .ok();
        });
    }

    /// The instances matching the query, and the mods group: settled when
    /// the index is not built, or the search started on the runtime over
    /// the current index.
    fn start_search(
        &mut self,
        cx: &mut Context<Self>,
    ) -> (String, Vec<InstanceResult>, PendingMods) {
        let query = self.query.clone();
        let outcome: Rc<ListOutcome> = self.instances.read(cx).outcome().clone();
        let instances = InstanceStore::search_instances(&outcome, &query)
            .into_iter()
            .take(INSTANCE_LIMIT)
            .map(|i| InstanceResult {
                id: i.id.clone(),
                name: i.config.name.clone(),
                detail: format!("{} · {}", loader_name(&i.config), i.config.mc_version()),
                icon: icon_for(i),
            })
            .collect();
        let mods = match (&self.state, &self.index) {
            (IndexState::Building, _) => PendingMods::Settled(ModGroup::Indexing),
            (IndexState::Failed(error), _) => PendingMods::Settled(ModGroup::Failed(error.clone())),
            (IndexState::Ready, None) => PendingMods::Settled(ModGroup::Hits(Vec::new())),
            (IndexState::Ready, Some(index)) => {
                let index = index.clone();
                let gate = self.search_gate.clone();
                let q = query.clone();
                // Some hits may belong to instances gone since the last
                // refresh; ask for spares.
                PendingMods::Searching(run(cx, async move {
                    drop(gate.read().await);
                    index.search(&q, MOD_LIMIT * 2)
                }))
            }
        };
        (query, instances, mods)
    }

    fn apply(
        &mut self,
        id: RevisionId,
        query: String,
        instances: Vec<InstanceResult>,
        // The settled group, or the hits of the index search.
        mods: Result<ModGroup, Vec<ModHit>>,
        cx: &mut Context<Self>,
    ) {
        if !self.revision.is_current(id) {
            return;
        }
        let list = self.instances.read(cx);
        let mods = mods.unwrap_or_else(|hits| {
            ModGroup::Hits(
                hits.into_iter()
                    .filter_map(|hit| {
                        let instance_name = list.display_name(&hit.instance_id)?;
                        Some(ModResult { hit, instance_name })
                    })
                    .take(MOD_LIMIT)
                    .collect(),
            )
        });
        let results = SearchResults {
            query,
            instances,
            mods,
        };
        if self.results.as_ref() != Some(&results) {
            self.results = Some(results);
            cx.notify();
        }
    }
}
