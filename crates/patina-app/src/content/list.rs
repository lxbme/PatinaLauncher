//! [`ContentTab`]: the model behind one local-content tab, plus the toolbar
//! and the empty state the tabs share.

use std::io;
use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, TestSupportExt as _, Window, div, rems,
};
use patina_core::instance::Instance;
use rust_i18n::t;

use crate::core_bridge::{Revision, run_blocking};
use crate::layout::{GUTTER, STRIP_H, TEXT_PRIMARY};

/// Where a tab's list stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    /// The first load has not finished.
    Loading,
    Loaded,
    /// The last load failed; the items of the load before it (if any) stay.
    Failed(String),
}

/// Emitted by a [`ContentTab`] each time a listing has landed (not for
/// results dropped as stale).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reloaded;

impl<T: Send + 'static> EventEmitter<Reloaded> for ContentTab<T> {}

/// An item the filter can match: by the name the row shows.
pub trait Named {
    fn filter_name(&self) -> &str;
}

/// Whether `name` matches the filter `query`: a case-insensitive substring
/// match; an empty (or blank) query matches everything.
pub fn matches_filter(name: &str, query: &str) -> bool {
    let query = query.trim();
    query.is_empty() || name.to_lowercase().contains(&query.to_lowercase())
}

/// Lists one content directory of an instance (blocking I/O; it runs on the
/// blocking pool).
pub type ContentLoader<T> = Arc<dyn Fn(&Instance) -> io::Result<Vec<T>> + Send + Sync>;

/// One model per content tab (Mods, resource packs, shaders, saves).
///
/// A reload ([`ContentTab::reload`]) lists the directory in the background
/// and keeps showing the old items until the new ones arrive. Only the
/// newest reload counts; results of older ones are dropped. The model
/// notifies when the items, the state, the reloading flag or the filter
/// change.
pub struct ContentTab<T: Send + 'static> {
    instance: Instance,
    loader: ContentLoader<T>,
    items: Vec<T>,
    state: LoadState,
    reloading: bool,
    /// A reload was asked for while one ran: one more follows it.
    rerun: bool,
    /// Bumped by [`ContentTab::update_items`]: a listing started before an
    /// edit is superseded by it.
    edits: u64,
    filter: String,
    revision: Revision,
    task: Task<()>,
    _activation: Option<Subscription>,
}

impl<T: Send + 'static> ContentTab<T> {
    /// A tab for `instance` listed by `loader`. It starts in
    /// [`LoadState::Loading`] and does not load until [`ContentTab::shown`]
    /// or [`ContentTab::reload`] is called.
    pub fn new(
        instance: Instance,
        loader: impl Fn(&Instance) -> io::Result<Vec<T>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            instance,
            loader: Arc::new(loader),
            items: Vec::new(),
            state: LoadState::Loading,
            reloading: false,
            rerun: false,
            edits: 0,
            filter: String::new(),
            revision: Revision::default(),
            task: Task::ready(()),
            _activation: None,
        }
    }

    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// All loaded items (the previous load's while a reload runs).
    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn state(&self) -> &LoadState {
        &self.state
    }

    /// Whether a reload is in flight (also true during the first load).
    pub fn is_reloading(&self) -> bool {
        self.reloading
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Sets the filter text (see [`matches_filter`]).
    pub fn set_filter(&mut self, filter: impl Into<String>, cx: &mut Context<Self>) {
        let filter = filter.into();
        if self.filter != filter {
            self.filter = filter;
            cx.notify();
        }
    }

    /// Points the tab at `instance` (the detail shows another instance, or
    /// its config changed). Reloads when the directory is another one.
    pub fn set_instance(&mut self, instance: Instance, cx: &mut Context<Self>) {
        let moved = self.instance.dir != instance.dir;
        self.instance = instance;
        if moved {
            self.items.clear();
            self.state = LoadState::Loading;
            // Whatever listed the old directory is stale from now on.
            self.revision.start();
            self.task = Task::ready(());
            self.reloading = false;
            self.rerun = false;
            self.reload(cx);
        }
    }

    /// Lists the directory in the background. The items on show stay until
    /// the result lands; a failure keeps them too and sets
    /// [`LoadState::Failed`]. One listing runs at a time: asking while one
    /// runs queues a single follow-up, however often it is asked, so bursts
    /// (window activation, several mutations) do not stack listings.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.reloading {
            self.rerun = true;
            return;
        }
        self.start_load(cx);
        cx.notify();
    }

    fn start_load(&mut self, cx: &mut Context<Self>) {
        let id = self.revision.start();
        let instance = self.instance.clone();
        let loader = self.loader.clone();
        let load = run_blocking(cx, move || loader(&instance));
        let edits = self.edits;
        self.reloading = true;
        self.task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if !this.revision.is_current(id) {
                    return;
                }
                if this.edits != edits {
                    // The directory was read before an edit of the items
                    // (which queued a rerun): the rerun brings the truth;
                    // this result would undo the edit.
                    this.reloading = false;
                    this.rerun = false;
                    this.start_load(cx);
                    cx.notify();
                    return;
                }
                match result {
                    Ok(Ok(items)) => {
                        this.items = items;
                        this.state = LoadState::Loaded;
                    }
                    Ok(Err(error)) => this.state = LoadState::Failed(error.to_string()),
                    Err(error) => this.state = LoadState::Failed(error.to_string()),
                }
                this.reloading = false;
                cx.emit(Reloaded);
                if std::mem::take(&mut this.rerun) {
                    this.start_load(cx);
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Changes the loaded items in place, for a mutation whose result is
    /// known (a toggled mod), without listing the directory again. A
    /// listing in flight may have read the directory before the change: its
    /// result is dropped (no [`Reloaded`]) and one more listing follows.
    pub fn update_items(&mut self, update: impl FnOnce(&mut Vec<T>), cx: &mut Context<Self>) {
        update(&mut self.items);
        self.edits += 1;
        if self.reloading {
            self.rerun = true;
        }
        cx.notify();
    }

    /// The tab was shown: reload.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    /// This app changed the directory (an add, a toggle, a delete): reload.
    pub fn mutated(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    /// Reloads whenever `window` becomes active again (another program may
    /// have changed the directory). There is no file watcher.
    pub fn reload_on_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._activation = Some(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.reload(cx);
            }
        }));
    }
}

impl<T: Named + Send + 'static> ContentTab<T> {
    /// The items that match the filter, in list order.
    pub fn filtered(&self) -> Vec<&T> {
        self.items
            .iter()
            .filter(|item| matches_filter(item.filter_name(), &self.filter))
            .collect()
    }
}

/// The filter input of a tab, wired to `tab`'s filter. Keep the returned
/// subscription with the input.
pub fn filter_input<T: Send + 'static>(
    tab: &Entity<ContentTab<T>>,
    window: &mut Window,
    cx: &mut App,
) -> (Entity<InputState>, Subscription) {
    let input = cx.new(|cx| InputState::new(window, cx).clean_on_escape());
    crate::i18n::localized_placeholder(&input, "content.filter.placeholder", window, cx);
    let tab = tab.downgrade();
    let subscription = cx.subscribe(&input, move |input, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
            let text = input.read(cx).value().to_string();
            tab.update(cx, |tab, cx| tab.set_filter(text, cx)).ok();
        }
    });
    (input, subscription)
}

type ToolbarAction = Box<dyn Fn(&mut Window, &mut App)>;

/// The strip above a tab's list: the filter box on the left, "Open folder"
/// and refresh on the right.
#[derive(IntoElement)]
pub struct ContentToolbar {
    filter: Entity<InputState>,
    refreshing: bool,
    /// A tab's own command, before "Open folder" (the Mods tab's "Get
    /// online").
    action: Option<AnyElement>,
    on_open: ToolbarAction,
    on_refresh: ToolbarAction,
}

impl ContentToolbar {
    pub fn new(
        filter: &Entity<InputState>,
        on_open: impl Fn(&mut Window, &mut App) + 'static,
        on_refresh: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            filter: filter.clone(),
            refreshing: false,
            action: None,
            on_open: Box::new(on_open),
            on_refresh: Box::new(on_refresh),
        }
    }

    /// Adds a tab's own command (a button) before "Open folder".
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.action = Some(action.into_any_element());
        self
    }

    /// Disables the refresh button while a reload runs.
    pub fn refreshing(mut self, refreshing: bool) -> Self {
        self.refreshing = refreshing;
        self
    }
}

impl RenderOnce for ContentToolbar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (on_open, on_refresh) = (self.on_open, self.on_refresh);
        let refresh = t!("content.refresh").to_string();
        h_flex()
            .flex_none()
            .h(STRIP_H)
            .px(GUTTER)
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div().flex_1().min_w(rems(6.)).max_w(rems(20.)).child(
                    Input::new(&self.filter)
                        .id("content-filter")
                        .small()
                        .cleanable(true)
                        .prefix(
                            Icon::new(IconName::Search)
                                .xsmall()
                                .text_color(theme.muted_foreground),
                        ),
                ),
            )
            .child(div().flex_1())
            .children(self.action)
            .child(
                Button::new("content-open-folder")
                    .outline()
                    .small()
                    .icon(Icon::new(IconName::FolderOpen))
                    .label(t!("common.open_folder").to_string())
                    .on_click(move |_, window, cx| on_open(window, cx)),
            )
            .child(
                Button::new("content-refresh")
                    .ghost()
                    .small()
                    .icon(Icon::new(Lucide::RefreshCw))
                    .tooltip(refresh.clone())
                    .accessibility_label(refresh)
                    .disabled(self.refreshing)
                    .on_click(move |_, window, cx| on_refresh(window, cx)),
            )
    }
}

/// Which tab an [`EmptyState`] is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyKind {
    Mods,
    ResourcePacks,
    Shaders,
    Saves,
}

impl EmptyKind {
    /// The one-line "what to do next" of an empty tab.
    pub fn text(self) -> String {
        match self {
            EmptyKind::Mods => t!("content.empty.mods"),
            EmptyKind::ResourcePacks => t!("content.empty.resourcepacks"),
            EmptyKind::Shaders => t!("content.empty.shaders"),
            EmptyKind::Saves => t!("content.empty.saves"),
        }
        .to_string()
    }
}

/// What a tab shows with nothing to list: a sentence about the next step,
/// or, when a filter hides everything, that nothing matches it.
#[derive(IntoElement)]
pub struct EmptyState {
    text: SharedString,
}

impl EmptyState {
    /// The tab has no items at all.
    pub fn new(kind: EmptyKind) -> Self {
        Self {
            text: kind.text().into(),
        }
    }

    /// Items exist, but none matches `query`.
    pub fn no_match(query: &str) -> Self {
        Self {
            text: t!("content.no_match", query = query).to_string().into(),
        }
    }

    /// The tab could not be read.
    pub fn failed(error: &str) -> Self {
        Self {
            text: t!("content.failed", error = error).to_string().into(),
        }
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            // Sits a little above the optical centre, as the other empty
            // states do.
            .pb(rems(3.))
            .child(
                div()
                    .id("content-empty")
                    .aria_label(self.text.clone())
                    .test_support()
                    .text_size(TEXT_PRIMARY)
                    .text_color(cx.theme().muted_foreground)
                    .child(self.text),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::mpsc;

    use gpui_kit::TestAppContext;
    use patina_core::instance::InstanceConfig;

    use super::*;
    use crate::content::open_folder_with;
    use crate::core_bridge::testing::{self, FakeCatalog};
    use patina_core::paths::DataDirs;

    struct Row(String);
    impl Named for Row {
        fn filter_name(&self) -> &str {
            &self.0
        }
    }

    fn instance(dir: PathBuf) -> Instance {
        Instance {
            id: "pack-1".into(),
            dir,
            config: InstanceConfig::default(),
        }
    }

    fn boot(cx: &mut TestAppContext) -> (tokio::runtime::Runtime, tempfile::TempDir) {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = testing::core(
            rt.handle().clone(),
            DataDirs::new(dir.path()),
            Arc::new(FakeCatalog::new().with_manifest_error("offline")),
        );
        cx.update(|cx| cx.set_global(core));
        cx.executor().allow_parking();
        (rt, dir)
    }

    /// Counts the landed listings of `tab` and lets a test await the next.
    struct Landed(
        futures::channel::mpsc::UnboundedReceiver<()>,
        #[allow(dead_code)] Subscription,
    );

    impl Landed {
        fn watch<T: Send + 'static>(tab: &Entity<ContentTab<T>>, cx: &mut TestAppContext) -> Self {
            let (tx, rx) = futures::channel::mpsc::unbounded();
            let sub = cx.update(|cx| {
                cx.subscribe(tab, move |_, _: &Reloaded, _| {
                    tx.unbounded_send(()).ok();
                })
            });
            Self(rx, sub)
        }

        async fn next(&mut self) {
            use futures::StreamExt as _;
            self.0.next().await.expect("the tab is alive");
        }
    }

    fn items<T: Clone + Send + 'static>(
        tab: &Entity<ContentTab<T>>,
        cx: &TestAppContext,
    ) -> Vec<T> {
        tab.read_with(cx, |t, _| t.items().to_vec())
    }

    #[test]
    fn filter_is_a_case_insensitive_substring() {
        assert!(matches_filter("Sodium-0.5.jar", "sod"));
        assert!(matches_filter("Sodium-0.5.jar", "  SODIUM "));
        assert!(matches_filter("anything", ""));
        assert!(!matches_filter("Sodium", "lithium"));
    }

    #[gpui_kit::test]
    async fn reload_keeps_old_items_until_done(cx: &mut TestAppContext) {
        let (_rt, dir) = boot(cx);
        // Each load waits for a message, then returns its number.
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let calls = Mutex::new(0usize);
        let tab = cx.new(|_| {
            ContentTab::new(instance(dir.path().into()), move |_| {
                let n = {
                    let mut c = calls.lock().unwrap();
                    *c += 1;
                    *c
                };
                rx.lock().unwrap().recv().ok();
                Ok(vec![n])
            })
        });
        let mut landed = Landed::watch(&tab, cx);
        tab.update(cx, |t, cx| t.reload(cx));
        assert_eq!(
            tab.read_with(cx, |t, _| t.state().clone()),
            LoadState::Loading
        );
        tx.send(()).unwrap();
        landed.next().await;
        assert_eq!(items(&tab, cx), vec![1]);

        // The second load is held back: the old items stay on show.
        tab.update(cx, |t, cx| t.reload(cx));
        assert!(tab.read_with(cx, |t, _| t.is_reloading()));
        assert_eq!(items(&tab, cx), vec![1]);
        assert_eq!(
            tab.read_with(cx, |t, _| t.state().clone()),
            LoadState::Loaded
        );
        tx.send(()).unwrap();
        landed.next().await;
        assert_eq!(items(&tab, cx), vec![2]);
    }

    #[gpui_kit::test]
    async fn reloads_are_single_flight(cx: &mut TestAppContext) {
        let (_rt, dir) = boot(cx);
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let calls = Arc::new(Mutex::new(0usize));
        let seen = calls.clone();
        let tab = cx.new(|_| {
            ContentTab::new(instance(dir.path().into()), move |_| {
                let n = {
                    let mut c = seen.lock().unwrap();
                    *c += 1;
                    *c
                };
                rx.lock().unwrap().recv().ok();
                Ok(vec![n])
            })
        });
        let mut landed = Landed::watch(&tab, cx);
        // A burst of five asks while the first listing runs.
        for _ in 0..5 {
            tab.update(cx, |t, cx| t.reload(cx));
        }
        tx.send(()).unwrap();
        landed.next().await;
        // One follow-up runs for all of them.
        tx.send(()).unwrap();
        landed.next().await;
        assert_eq!(items(&tab, cx), vec![2]);
        assert!(!tab.read_with(cx, |t, _| t.is_reloading()));
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[gpui_kit::test]
    async fn stale_reload_dropped(cx: &mut TestAppContext) {
        let (_rt, dir) = boot(cx);
        let old_dir = dir.path().join("old");
        let new_dir = dir.path().join("new");
        // The old directory's listing waits for `release`; the new one is quick.
        let (release, gate) = mpsc::channel::<()>();
        let (entered_tx, entered) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let entered_tx = Mutex::new(entered_tx);
        let old = old_dir.clone();
        let tab = cx.new(|_| {
            ContentTab::new(instance(old_dir.clone()), move |inst| {
                if inst.dir == old {
                    entered_tx.lock().unwrap().send(()).ok();
                    gate.lock().unwrap().recv().ok();
                    Ok(vec!["old"])
                } else {
                    Ok(vec!["new"])
                }
            })
        });
        let mut landed = Landed::watch(&tab, cx);
        tab.update(cx, |t, cx| t.reload(cx));
        entered.recv().unwrap();
        // The detail moves to another instance while the old listing runs.
        tab.update(cx, |t, cx| t.set_instance(instance(new_dir.clone()), cx));
        landed.next().await;
        assert_eq!(items(&tab, cx), vec!["new"]);
        // The old listing finishes now: nothing lands, nothing changes.
        release.send(()).unwrap();
        cx.run_until_parked();
        // The old listing runs on another thread: give it time to (wrongly) land.
        std::thread::sleep(std::time::Duration::from_millis(50));
        cx.run_until_parked();
        assert!(landed.0.try_recv().is_err(), "no second listing landed");
        assert_eq!(items(&tab, cx), vec!["new"]);
        assert_eq!(
            tab.read_with(cx, |t, _| t.state().clone()),
            LoadState::Loaded
        );
    }

    #[gpui_kit::test]
    async fn an_edit_supersedes_a_listing_in_flight(cx: &mut TestAppContext) {
        let (_rt, dir) = boot(cx);
        // Each listing waits for a message, then returns its number.
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let calls = Arc::new(Mutex::new(0usize));
        let seen = calls.clone();
        let tab = cx.new(|_| {
            ContentTab::new(instance(dir.path().into()), move |_| {
                let n = {
                    let mut c = seen.lock().unwrap();
                    *c += 1;
                    *c
                };
                rx.lock().unwrap().recv().ok();
                Ok(vec![n])
            })
        });
        let mut landed = Landed::watch(&tab, cx);
        tab.update(cx, |t, cx| t.reload(cx));
        tx.send(()).unwrap();
        landed.next().await;
        assert_eq!(items(&tab, cx), vec![1]);

        // A listing starts, then the app edits the items (a toggle).
        tab.update(cx, |t, cx| t.reload(cx));
        tab.update(cx, |t, cx| t.update_items(|items| items[0] = 99, cx));
        // The listing that read the folder before the edit finishes: it
        // lands nothing and the edit stays.
        tx.send(()).unwrap();
        settle_calls(&calls, 3, cx);
        assert_eq!(items(&tab, cx), vec![99]);
        assert!(
            landed.0.try_recv().is_err(),
            "the stale listing did not land"
        );
        assert!(tab.read_with(cx, |t, _| t.is_reloading()));
        // The listing after the edit lands.
        tx.send(()).unwrap();
        landed.next().await;
        assert_eq!(items(&tab, cx), vec![3]);
        assert!(!tab.read_with(cx, |t, _| t.is_reloading()));
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    /// Runs gpui until the loader has been called `n` times (at most 5 s).
    fn settle_calls(calls: &Mutex<usize>, n: usize, cx: &mut TestAppContext) {
        for _ in 0..500 {
            cx.run_until_parked();
            if *calls.lock().unwrap() >= n {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the loader was not called {n} times");
    }

    #[gpui_kit::test]
    async fn failed_reload_keeps_items(cx: &mut TestAppContext) {
        let (_rt, dir) = boot(cx);
        let calls = Mutex::new(0usize);
        let tab = cx.new(|_| {
            ContentTab::new(instance(dir.path().into()), move |_| {
                let mut c = calls.lock().unwrap();
                *c += 1;
                if *c == 1 {
                    Ok(vec![7])
                } else {
                    Err(io::Error::other("boom"))
                }
            })
        });
        let mut landed = Landed::watch(&tab, cx);
        tab.update(cx, |t, cx| t.reload(cx));
        landed.next().await;
        tab.update(cx, |t, cx| t.reload(cx));
        landed.next().await;
        assert_eq!(items(&tab, cx), vec![7]);
        assert!(matches!(
            tab.read_with(cx, |t, _| t.state().clone()),
            LoadState::Failed(_)
        ));
    }

    #[gpui_kit::test]
    async fn filtered_follows_the_filter(cx: &mut TestAppContext) {
        let tab = cx.new(|_| ContentTab::new(instance(PathBuf::new()), |_| Ok(Vec::<Row>::new())));
        tab.update(cx, |t, cx| {
            t.items = vec![Row("Sodium".into()), Row("Iris".into())];
            t.set_filter("sod", cx);
        });
        let names = tab.read_with(cx, |t, _| {
            t.filtered().iter().map(|r| r.0.clone()).collect::<Vec<_>>()
        });
        assert_eq!(names, ["Sodium"]);
    }

    #[gpui_kit::test]
    async fn open_folder_creates_missing_dir(cx: &mut TestAppContext) {
        let (_rt, dir) = boot(cx);
        let target = dir.path().join("minecraft").join("mods");
        assert!(!target.exists());
        let opened = Arc::new(Mutex::new(None::<PathBuf>));
        let seen = opened.clone();
        let task = cx.update(|cx| {
            open_folder_with(target.clone(), cx, move |path, _| {
                *seen.lock().unwrap() = Some(path.to_path_buf());
            })
        });
        task.await.unwrap();
        assert!(target.is_dir());
        assert_eq!(opened.lock().unwrap().as_deref(), Some(target.as_path()));
    }
}
