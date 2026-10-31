//! [`InstancesModel`]: the instance list as the instances page knows it, the
//! selection, and which instances are running.

use std::collections::{HashMap, HashSet};

use gpui_kit::{Context, EventEmitter, SharedString, Subscription, Task, Window};
use patina_core::instance::{BrokenInstance, Instance, ListOutcome};

use crate::core_bridge::{Core, Revision, run_blocking};

/// What a running instance is doing. Plan 4b task 10 (launch) adds the
/// states between "prepared" and "exited"; until then nothing sets it
/// outside tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunState {
    /// The game process is up.
    Running,
}

/// One entry of the list: an instance that loads, or a directory whose
/// config does not.
#[derive(Debug, Clone, Copy)]
pub enum Entry<'a> {
    Healthy(&'a Instance),
    Broken(&'a BrokenInstance),
}

impl Entry<'_> {
    pub fn id(&self) -> &str {
        match self {
            Entry::Healthy(i) => &i.id,
            Entry::Broken(b) => &b.id,
        }
    }
}

/// Emitted after this app changed the instances on disk (created, deleted,
/// repaired), so views that summarise them (the status bar) re-read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstancesChanged;

/// The instances page's domain state (an entity).
///
/// The list is read from the core's `InstanceStore` in the background
/// ([`InstancesModel::refresh`]): when the model is created, whenever the
/// window becomes active (another program may have changed the directory)
/// and after each of this app's own operations. A newer refresh makes the
/// results of older ones stale.
///
/// The selection is an instance id (the directory name, which healthy and
/// broken entries share), so it survives refreshes and reordering; when the
/// selected entry is gone the first entry is selected.
pub struct InstancesModel {
    outcome: ListOutcome,
    loaded: bool,
    refreshing: bool,
    /// Why the last refresh failed; the previous list stays on show.
    error: Option<SharedString>,
    /// Bumped by every refresh that succeeds, so dependants can tell a new
    /// list from other changes.
    generation: u64,
    selected: Option<String>,
    running: HashMap<String, RunState>,
    /// Broken entries being deleted, so a second request is ignored.
    deleting: HashSet<String>,
    revision: Revision,
    task: Task<()>,
    _activation: Subscription,
}

impl EventEmitter<InstancesChanged> for InstancesModel {}

impl InstancesModel {
    /// Starts loading the list, and reloads it whenever `window` becomes
    /// active.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.refresh(cx);
            }
        });
        let mut this = Self {
            outcome: ListOutcome::default(),
            loaded: false,
            refreshing: false,
            error: None,
            generation: 0,
            selected: None,
            running: HashMap::new(),
            deleting: HashSet::new(),
            revision: Revision::default(),
            task: Task::ready(()),
            _activation,
        };
        this.refresh(cx);
        this
    }

    /// The listed instances, most recently played first.
    pub fn instances(&self) -> &[Instance] {
        &self.outcome.instances
    }

    /// Directories whose config failed to load, by id.
    pub fn broken(&self) -> &[BrokenInstance] {
        &self.outcome.broken
    }

    /// Whether the first refresh has finished (successfully or not).
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Whether a refresh is in flight.
    pub fn is_refreshing(&self) -> bool {
        self.refreshing
    }

    /// Why the last refresh failed, if it did.
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    /// Counts the refreshes that brought a new list.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether there is nothing at all to list, healthy or broken.
    pub fn is_empty(&self) -> bool {
        self.outcome.instances.is_empty() && self.outcome.broken.is_empty()
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The selected entry, if the selection is set.
    pub fn selected_entry(&self) -> Option<Entry<'_>> {
        self.entry(self.selected.as_deref()?)
    }

    /// The entry `id`, healthy or broken.
    pub fn entry(&self, id: &str) -> Option<Entry<'_>> {
        find_entry(&self.outcome, id)
    }

    /// Selects entry `id` if it is listed; notifies only on a change.
    pub fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(id) || self.entry(id).is_none() {
            return;
        }
        self.selected = Some(id.to_string());
        cx.notify();
    }

    pub fn run_state(&self, id: &str) -> Option<RunState> {
        self.running.get(id).copied()
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains_key(id)
    }

    /// Records what instance `id` is doing (`None`: not running).
    pub fn set_run_state(&mut self, id: &str, state: Option<RunState>, cx: &mut Context<Self>) {
        let changed = match state {
            Some(state) => self.running.insert(id.to_string(), state) != Some(state),
            None => self.running.remove(id).is_some(),
        };
        if changed {
            cx.notify();
        }
    }

    /// Whether broken entry `id` is being deleted.
    pub fn is_deleting(&self, id: &str) -> bool {
        self.deleting.contains(id)
    }

    /// Re-reads the list in the background; keeps the selection when its
    /// entry is still listed and otherwise selects the first entry.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let id = self.revision.start();
        let store = Core::global(cx).store.clone();
        let list = run_blocking(cx, move || store.list());
        self.refreshing = true;
        // Replacing the task drops the previous refresh.
        self.task = cx.spawn(async move |this, cx| {
            let result = list.await;
            this.update(cx, |this, cx| {
                if !this.revision.is_current(id) {
                    return;
                }
                match result {
                    Ok(Ok(outcome)) => this.apply(outcome),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "listing the instances failed");
                        this.error = Some(error.to_string().into());
                    }
                    Err(error) => this.error = Some(error.to_string().into()),
                }
                this.refreshing = false;
                this.loaded = true;
                cx.notify();
            })
            .ok();
        });
    }

    fn apply(&mut self, outcome: ListOutcome) {
        self.selected = reselect(self.selected.as_deref(), &outcome);
        self.outcome = outcome;
        self.error = None;
        self.generation += 1;
    }

    /// Deletes the directory of broken entry `broken` in the background,
    /// then refreshes. Resolves to the error message if that failed. A
    /// second request for an entry being deleted resolves at once.
    pub fn delete_broken(
        &mut self,
        broken: BrokenInstance,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), SharedString>> {
        let id = broken.id.clone();
        if !self.deleting.insert(id.clone()) {
            return Task::ready(Ok(()));
        }
        cx.notify();
        let store = Core::global(cx).store.clone();
        let job = run_blocking(cx, move || store.delete_broken(&broken));
        cx.spawn(async move |this, cx| {
            let result = match job.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(SharedString::from(error.to_string())),
                Err(error) => Err(SharedString::from(error.to_string())),
            };
            if let Err(error) = &result {
                tracing::warn!(%error, id, "deleting a broken instance failed");
            }
            this.update(cx, |this, cx| {
                this.deleting.remove(&id);
                if result.is_ok() {
                    cx.emit(InstancesChanged);
                }
                // Either way the directory may have changed (repaired,
                // removed by someone else).
                this.refresh(cx);
                cx.notify();
            })
            .ok();
            result
        })
    }
}

fn find_entry<'a>(outcome: &'a ListOutcome, id: &str) -> Option<Entry<'a>> {
    let healthy = outcome.instances.iter().find(|i| i.id == id);
    healthy.map(Entry::Healthy).or_else(|| {
        let broken = outcome.broken.iter().find(|b| b.id == id);
        broken.map(Entry::Broken)
    })
}

/// The selection after the list became `outcome`: `previous` if it is still
/// listed, else the first healthy entry, else the first broken one.
pub(crate) fn reselect(previous: Option<&str>, outcome: &ListOutcome) -> Option<String> {
    if let Some(id) = previous
        && find_entry(outcome, id).is_some()
    {
        return Some(id.to_string());
    }
    let first = outcome.instances.first().map(|i| &i.id);
    first
        .or_else(|| outcome.broken.first().map(|b| &b.id))
        .cloned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use patina_core::instance::{InstanceConfig, InstanceError};

    use super::*;

    fn healthy(id: &str) -> Instance {
        Instance {
            id: id.to_string(),
            dir: PathBuf::from(id),
            config: InstanceConfig {
                name: id.to_string(),
                ..Default::default()
            },
        }
    }

    fn broken(id: &str) -> BrokenInstance {
        BrokenInstance {
            id: id.to_string(),
            dir: PathBuf::from(id),
            error: InstanceError::Parse("bad".into()),
        }
    }

    fn outcome(ok: &[&str], bad: &[&str]) -> ListOutcome {
        ListOutcome {
            instances: ok.iter().map(|id| healthy(id)).collect(),
            broken: bad.iter().map(|id| broken(id)).collect(),
        }
    }

    #[test]
    fn reselect_keeps_a_listed_entry_and_falls_back_to_the_first() {
        let list = outcome(&["b", "a"], &["x"]);
        assert_eq!(reselect(Some("a"), &list).as_deref(), Some("a"));
        // A broken entry can stay selected.
        assert_eq!(reselect(Some("x"), &list).as_deref(), Some("x"));
        assert_eq!(reselect(Some("gone"), &list).as_deref(), Some("b"));
        assert_eq!(reselect(None, &list).as_deref(), Some("b"));
        // Only broken entries left: the first of them.
        let only_broken = outcome(&[], &["x", "y"]);
        assert_eq!(reselect(Some("a"), &only_broken).as_deref(), Some("x"));
        assert_eq!(reselect(Some("a"), &outcome(&[], &[])), None);
    }
}
