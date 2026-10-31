//! The global search in the title bar ([`GlobalSearch`]): instances by
//! name, the mods installed in any instance (through the persistent
//! [`ModIndex`](patina_core::content::index::ModIndex), see
//! [`SearchModel`]) and an "online" entry that hands the words to the
//! downloads page.
//!
//! The results show in a popup under the search box, in three labelled
//! groups. The keyboard stays in the box: Up / Down move through the
//! results across the groups, Enter opens the chosen one, Escape closes the
//! popup, clears the box and gives the keyboard back to where it was before
//! the search took it. Clicking outside the box and the popup closes it too.

mod model;
mod popup;

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::Popup;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, WindowExt as _};
use gpui_kit::{
    Anchor, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Pixels, Render, ScrollHandle,
    Styled, Subscription, Window, actions, div,
};
use rust_i18n::t;

pub use model::{
    DEBOUNCE, Hold, INSTANCE_LIMIT, IndexState, InstanceResult, MOD_LIMIT, ModGroup, ModResult,
    REFRESH_GAP, SearchItem, SearchModel, SearchResults,
};
pub use popup::search_row_id;

use crate::instances::{Entry, InstancesPage, labelled};
use crate::shell::{Nav, OpenDownloads, ShellState};

actions!(
    search,
    [
        /// Moves the search's choice to the previous result.
        SelectPrevResult,
        /// Moves the search's choice to the next result.
        SelectNextResult,
        /// Opens the chosen result.
        OpenResult,
        /// Closes the results and gives the keyboard back.
        CloseSearch
    ]
);

/// Key context of the search box (around the input).
pub const SEARCH_CONTEXT: &str = "GlobalSearch";

/// Binds the search's keys. The input passes Up, Down, Enter and Escape
/// on (it is a single line and does not clear itself on Escape), so they
/// reach these bindings.
pub fn init(cx: &mut App) {
    let context = Some(SEARCH_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevResult, context),
        KeyBinding::new("down", SelectNextResult, context),
        KeyBinding::new("enter", OpenResult, context),
        KeyBinding::new("escape", CloseSearch, context),
    ]);
}

/// The title bar's search box and its results popup (an entity). Owns the
/// box's state, the popup's open state and the keyboard choice; the
/// results come from its [`SearchModel`].
pub struct GlobalSearch {
    model: Entity<SearchModel>,
    input: Entity<InputState>,
    shell: Entity<ShellState>,
    page: Entity<InstancesPage>,
    /// Whether the popup is on show (it also needs results to show).
    open: bool,
    /// The chosen result, an index into [`SearchResults::items`].
    cursor: usize,
    /// The results the cursor belongs to: new results move it back to the
    /// top.
    cursor_for: Option<SearchResults>,
    /// Where the keyboard was before the search box took it.
    return_focus: Option<FocusHandle>,
    /// The shell's own focus: where the keyboard goes when there is no
    /// place to return to, or that place left with its page.
    home_focus: FocusHandle,
    /// The search box's bounds as last laid out under the popup, so a
    /// click on the box does not count as a click outside.
    box_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    scroll: ScrollHandle,
    _subscriptions: [Subscription; 2],
}

impl GlobalSearch {
    /// The search over `page`'s instances; `shell` is navigated to show a
    /// chosen result; `home_focus` is the shell's own focus.
    pub fn new(
        shell: Entity<ShellState>,
        page: Entity<InstancesPage>,
        home_focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("shell.search.placeholder").to_string())
        });
        let instances = page.read(cx).model().clone();
        let model = cx.new(|cx| SearchModel::new(instances, window, cx));
        let _subscriptions = [
            cx.subscribe_in(&input, window, Self::on_input),
            cx.observe(&model, |this, _, cx| this.results_changed(cx)),
        ];
        Self {
            model,
            input,
            shell,
            page,
            open: false,
            cursor: 0,
            cursor_for: None,
            return_focus: None,
            home_focus,
            box_bounds: Rc::default(),
            scroll: ScrollHandle::new(),
            _subscriptions,
        }
    }

    pub fn model(&self) -> &Entity<SearchModel> {
        &self.model
    }

    /// Whether the results popup is on show.
    pub fn is_open(&self, cx: &App) -> bool {
        self.open && self.model.read(cx).results().is_some()
    }

    /// The chosen result, if the popup is on show with the results of
    /// the words in the box. While those are still being searched (the
    /// popup shows the previous ones) nothing is chosen, so Enter does
    /// nothing until they land.
    pub fn chosen(&self, cx: &App) -> Option<SearchItem> {
        let model = self.model.read(cx);
        let results = model.results().filter(|_| model.is_current())?;
        self.is_open(cx)
            .then(|| results.items().get(self.cursor).cloned())
            .flatten()
    }

    /// Gives the search box the keyboard (`/`), remembering where it was.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_focus(window, cx);
        self.input.update(cx, |s, cx| s.focus(window, cx));
    }

    fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    /// Notes where the keyboard is, unless the search box has it already.
    fn remember_focus(&mut self, window: &Window, cx: &App) {
        if !self.input_focused(window, cx) {
            self.return_focus = window.focused(cx);
        }
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
                self.open = !text.trim().is_empty();
                self.model.update(cx, |m, cx| m.set_query(&text, cx));
                cx.notify();
            }
            InputEvent::Focus => {
                if !self.model.read(cx).query().is_empty() && !self.open {
                    self.open = true;
                    cx.notify();
                }
            }
            InputEvent::Blur => self.set_open(false, cx),
            InputEvent::PressEnter { .. } => {}
        }
    }

    fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.open != open {
            self.open = open;
            cx.notify();
        }
    }

    /// New results put the choice back on the first one.
    fn results_changed(&mut self, cx: &mut Context<Self>) {
        let results = self.model.read(cx).results();
        if self.cursor_for.as_ref() != results {
            self.cursor_for = results.cloned();
            self.cursor = 0;
            self.scroll.scroll_to_item(0);
        }
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(count) = self.model.read(cx).results().map(|r| r.items().len()) else {
            return;
        };
        if !self.open {
            // The box still has words: the keys bring their results back.
            self.set_open(!self.model.read(cx).query().is_empty(), cx);
            return;
        }
        if count == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).rem_euclid(count as isize) as usize;
        if let Some(results) = self.model.read(cx).results() {
            self.scroll
                .scroll_to_item(popup::child_index(results, self.cursor));
        }
        cx.notify();
    }

    fn select_prev(&mut self, _: &SelectPrevResult, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn select_next(&mut self, _: &SelectNextResult, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn open_chosen(&mut self, _: &OpenResult, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.chosen(cx) {
            self.activate(item, window, cx);
        }
    }

    fn close(&mut self, _: &CloseSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(window, cx);
        self.restore_focus(window, cx);
    }

    /// Closes the popup and empties the box.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        self.model.update(cx, |m, cx| m.set_query("", cx));
        cx.notify();
    }

    /// Gives the keyboard back to where it was before the search.
    fn restore_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.return_focus.take() {
            Some(focus) => focus.focus(window, cx),
            None => self.home_focus.focus(window, cx),
        }
    }

    /// Opens result `item`: the instance (selected, on the instances
    /// page), the mod (its instance's Mods tab, scrolled to the file and
    /// highlighted) or the downloads page searching the words.
    /// A result whose instance is gone says so instead.
    pub fn activate(&mut self, item: SearchItem, window: &mut Window, cx: &mut Context<Self>) {
        let search = self.model.read(cx);
        let query = search.query().to_string();
        let name = search
            .results()
            .and_then(|r| r.instance_name(&item))
            .map(str::to_owned);
        self.dismiss(window, cx);
        let model = self.page.read(cx).model().clone();
        let healthy =
            |id: &str, cx: &App| matches!(model.read(cx).entry(id), Some(Entry::Healthy(_)));
        let gone = |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            let text = t!(
                "shell.search.gone",
                name = name.as_deref().unwrap_or_default()
            );
            let text = text.to_string();
            window.push_notification(labelled(Notification::warning(text.clone()), text), cx);
            this.restore_focus(window, cx);
        };
        match item {
            SearchItem::Instance(id) => {
                if !healthy(&id, cx) {
                    gone(self, window, cx);
                    return;
                }
                self.return_focus = None;
                model.update(cx, |m, cx| m.select(&id, cx));
                self.shell
                    .update(cx, |s, cx| s.navigate(Nav::Instances, cx));
                let list = self.page.read(cx).list().read(cx).focus_handle().clone();
                list.focus(window, cx);
            }
            SearchItem::Mod {
                instance_id,
                file_name,
            } => {
                if !healthy(&instance_id, cx) {
                    gone(self, window, cx);
                    return;
                }
                self.return_focus = None;
                model.update(cx, |m, cx| m.select(&instance_id, cx));
                self.shell
                    .update(cx, |s, cx| s.navigate(Nav::Instances, cx));
                let detail = self.page.read(cx).detail().clone();
                detail.update(cx, |d, cx| d.show_mod(&file_name, window, cx));
            }
            SearchItem::Online => {
                window.dispatch_action(
                    Box::new(OpenDownloads {
                        target: None,
                        query: Some(query),
                    }),
                    cx,
                );
                // The place the keyboard came from may be on the page
                // just left.
                self.return_focus = None;
                self.home_focus.focus(window, cx);
            }
        }
    }

    /// A click outside the popup closes it, unless it is on the box.
    fn clicked_outside(&mut self, position: gpui_kit::Point<Pixels>, cx: &mut Context<Self>) {
        let on_box = self.box_bounds.get().is_some_and(|b| b.contains(&position));
        if !on_box {
            self.set_open(false, cx);
        }
    }
}

impl Render for GlobalSearch {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let input = Input::new(&self.input)
            .id("search")
            .small()
            .cleanable(true)
            .rounded_full()
            .bg(theme.background)
            .prefix(
                Icon::new(IconName::Search)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            );
        let trigger = div()
            .key_context(SEARCH_CONTEXT)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::open_chosen))
            .on_action(cx.listener(Self::close))
            // Before the input takes the keyboard on a click.
            .capture_any_mouse_down(
                cx.listener(|this, _, window, cx| this.remember_focus(window, cx)),
            )
            .w_full()
            .child(input);
        let box_bounds = self.box_bounds.clone();
        let host = Popup::new("search-host", trigger)
            .anchor(Anchor::TopLeft)
            .offset(window.rem_size() * 0.25)
            .on_position(move |_, trigger| box_bounds.set(Some(trigger)))
            .w_full();
        let results = self.model.read(cx).results().cloned();
        match results.filter(|_| self.open) {
            Some(results) => host.content(self.render_popup(&results, cx)),
            None => host,
        }
    }
}
