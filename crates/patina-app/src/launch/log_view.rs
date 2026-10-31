//! The log tab: [`LogBuffer`] (a bounded ring of the game's output), the
//! forwarder that fills it from a session's log channel ([`forward_logs`])
//! and the virtualized view ([`LogView`]).
//!
//! Each instance has one `Entity<LogBuffer>` in the [`InstancesModel`]. The
//! buffer notifies only its observers, i.e. the log view, once per batch:
//! the instance list and the header are not re-rendered by log output.
//!
//! The list scrolls with the wheel, its scrollbar, and Page Up / Page Down /
//! Home / End while it has the keyboard; however it got there, "Auto-scroll"
//! follows whether the end of the log is in view.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::scroll::{ScrollableElement as _, ScrollbarHandle};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, Disableable, Selectable, Sizable, VirtualListScrollHandle, h_flex, v_flex,
    v_virtual_list,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Bounds, ClipboardItem, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    KeyBinding, MouseButton, ParentElement, Pixels, Point, Rems, Render, ScrollStrategy,
    SharedString, Size, StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt,
    WeakEntity, Window, actions, div, point, rems, size,
};
use patina_core::launch::running::CURRENT_LOG;
use patina_core::launch::{LogLevel, LogLine};
use rust_i18n::t;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::error::TryRecvError;

use crate::instances::{Entry, InstancesModel};
use crate::layout::{GUTTER, TEXT_SECONDARY};

/// Lines kept per instance; older ones are dropped and counted.
pub const LOG_VIEW_MAX_LINES: usize = 20_000;
/// Total text bytes kept per instance; the oldest lines go first.
pub const LOG_VIEW_MAX_BYTES: usize = 32 * 1024 * 1024;
/// Lines arriving within this window are appended together.
pub const LOG_BATCH_WINDOW: Duration = Duration::from_millis(50);
/// Row height of the log; the font is mono, so every row is one line.
const ROW_H: Rems = Rems(1.375);
/// How close to the end counts as "at the bottom".
const BOTTOM_SLACK: Rems = Rems(0.5);

/// Row sizes keyed by (row count, row height).
type RowSizes = ((usize, Pixels), Rc<Vec<Size<Pixels>>>);

/// Key context of the log list while it has the keyboard.
const LOG_CONTEXT: &str = "LogView";

actions!(
    patina,
    [
        /// Scrolls the log up by a page.
        LogPageUp,
        /// Scrolls the log down by a page.
        LogPageDown,
        /// Scrolls to the log's first line.
        LogTop,
        /// Scrolls to the log's last line.
        LogBottom
    ]
);

/// Binds the log list's scrolling keys.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("pageup", LogPageUp, Some(LOG_CONTEXT)),
        KeyBinding::new("pagedown", LogPageDown, Some(LOG_CONTEXT)),
        KeyBinding::new("home", LogTop, Some(LOG_CONTEXT)),
        KeyBinding::new("end", LogBottom, Some(LOG_CONTEXT)),
    ]);
}

/// The log list's scroll handle as its scrollbar sees it: it records that
/// the scrollbar moved the list (a drag or a click on the track), so the
/// view can re-read whether the end is in view. The scrollbar re-renders
/// the view after every move.
#[derive(Clone)]
struct ScrollbarMoves {
    list: VirtualListScrollHandle,
    moved: Rc<Cell<bool>>,
}

impl ScrollbarHandle for ScrollbarMoves {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.list.viewport_bounds()
    }

    fn offset(&self) -> Point<Pixels> {
        ScrollbarHandle::offset(&self.list)
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        ScrollbarHandle::set_offset(&self.list, offset);
        self.moved.set(true);
    }

    fn content_size(&self) -> Size<Pixels> {
        self.list.content_size()
    }

    fn start_drag(&self) {
        self.list.start_drag();
    }

    fn end_drag(&self) {
        self.list.end_drag();
        self.moved.set(true);
    }
}

/// The newest [`LOG_VIEW_MAX_LINES`] lines of one instance's output.
#[derive(Debug, Default)]
pub struct LogBuffer {
    lines: VecDeque<(LogLevel, SharedString)>,
    bytes: usize,
    dropped: u64,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a line, dropping the oldest while the buffer holds too many
    /// lines or bytes (the newest line is always kept).
    pub fn push(&mut self, level: LogLevel, text: impl Into<SharedString>) {
        let text = text.into();
        self.bytes += text.len();
        self.lines.push_back((level, text));
        while self.lines.len() > LOG_VIEW_MAX_LINES
            || (self.bytes > LOG_VIEW_MAX_BYTES && self.lines.len() > 1)
        {
            if let Some((_, old)) = self.lines.pop_front() {
                self.bytes -= old.len();
                self.dropped += 1;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// How many older lines were dropped to stay within the bound.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn get(&self, ix: usize) -> Option<(LogLevel, &SharedString)> {
        self.lines.get(ix).map(|(level, text)| (*level, text))
    }

    pub fn iter(&self) -> impl Iterator<Item = (LogLevel, &SharedString)> {
        self.lines.iter().map(|(level, text)| (*level, text))
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
        self.dropped = 0;
    }

    /// Every line, joined with newlines.
    pub fn text(&self) -> String {
        let mut out = String::with_capacity(self.bytes + self.lines.len());
        for (i, (_, text)) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(text);
        }
        out
    }
}

/// Which lines the log tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LevelFilter {
    #[default]
    All,
    /// Warn, Error and Fatal. Lines of unknown level (stack traces, plain
    /// output) are hidden.
    WarnUp,
}

impl LevelFilter {
    pub fn admits(self, level: LogLevel) -> bool {
        match self {
            LevelFilter::All => true,
            LevelFilter::WarnUp => {
                matches!(level, LogLevel::Warn | LogLevel::Error | LogLevel::Fatal)
            }
        }
    }
}

/// Positions of the lines `filter` shows. A line of unknown level (a stack
/// trace, a continuation) takes the level of the classified line before it,
/// so an error keeps its trace; one with nothing classified before it keeps
/// `Unknown`. The stored levels are not changed.
pub fn visible_rows(buffer: &LogBuffer, filter: LevelFilter) -> Vec<usize> {
    let mut inherited = LogLevel::Unknown;
    let mut rows = Vec::new();
    for (i, (level, _)) in buffer.iter().enumerate() {
        if level != LogLevel::Unknown {
            inherited = level;
        }
        if filter.admits(inherited) {
            rows.push(i);
        }
    }
    rows
}

/// Appends the lines received from `rx` to `buffer`, one batch per
/// [`LOG_BATCH_WINDOW`], notifying the buffer once per batch. Ends when the
/// channel closes (after appending what is left) or the buffer is gone.
///
/// A tokio receiver is runtime-agnostic, so it is polled from gpui directly.
/// Store the returned task; dropping it stops the forwarding.
pub fn forward_logs(
    cx: &mut App,
    buffer: WeakEntity<LogBuffer>,
    mut rx: Receiver<LogLine>,
) -> Task<()> {
    cx.spawn(async move |cx| {
        while let Some(first) = rx.recv().await {
            cx.background_executor().timer(LOG_BATCH_WINDOW).await;
            let mut batch = vec![first];
            let closed = loop {
                match rx.try_recv() {
                    Ok(line) => batch.push(line),
                    Err(TryRecvError::Empty) => break false,
                    Err(TryRecvError::Disconnected) => break true,
                }
            };
            let applied = buffer.update(cx, |buffer, cx| {
                for line in batch {
                    buffer.push(line.level, line.text);
                }
                cx.notify();
            });
            if applied.is_err() || closed {
                return;
            }
        }
    })
}

/// The log tab of one instance (an entity).
pub struct LogView {
    model: Entity<InstancesModel>,
    id: String,
    buffer: Entity<LogBuffer>,
    filter: LevelFilter,
    /// Buffer positions shown under a filter other than `All`.
    visible: Option<Rc<Vec<usize>>>,
    autoscroll: bool,
    scroll: VirtualListScrollHandle,
    /// [`Self::scroll`] for the scrollbar.
    scrollbar: ScrollbarMoves,
    /// The list's keyboard focus (a Tab stop; a click on the list takes it).
    focus: FocusHandle,
    /// Row sizes, reused while the count and the rem size stay the same.
    sizes: Option<RowSizes>,
    _observe: [Subscription; 2],
}

impl LogView {
    pub fn new(model: Entity<InstancesModel>, id: String, cx: &mut Context<Self>) -> Self {
        let buffer = model.update(cx, |m, cx| m.log_buffer(&id, cx));
        let observe_buffer = cx.observe(&buffer, |this, _, cx| {
            this.rebuild(cx);
        });
        // The crash-log banner comes from the model.
        let observe_model = cx.observe(&model, |_, _, cx| cx.notify());
        let scroll = VirtualListScrollHandle::new();
        Self {
            model,
            id,
            buffer,
            filter: LevelFilter::All,
            visible: None,
            autoscroll: true,
            scrollbar: ScrollbarMoves {
                list: scroll.clone(),
                moved: Rc::default(),
            },
            scroll,
            focus: cx.focus_handle().tab_stop(true),
            sizes: None,
            _observe: [observe_buffer, observe_model],
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.id
    }

    pub fn filter(&self) -> LevelFilter {
        self.filter
    }

    pub fn autoscroll(&self) -> bool {
        self.autoscroll
    }

    /// How many rows are on show under the current filter.
    pub fn visible_len(&self, cx: &App) -> usize {
        match &self.visible {
            Some(rows) => rows.len(),
            None => self.buffer.read(cx).len(),
        }
    }

    pub fn set_filter(&mut self, filter: LevelFilter, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter;
            self.rebuild(cx);
        }
    }

    pub fn set_autoscroll(&mut self, on: bool, cx: &mut Context<Self>) {
        self.autoscroll = on;
        if on {
            self.scroll_to_end(cx);
        }
        cx.notify();
    }

    /// Scrolls to the last row on show. (The handle's own `scroll_to_bottom`
    /// takes the row count of the last frame, so after new lines it would
    /// stop one batch short.)
    fn scroll_to_end(&self, cx: &App) {
        if let Some(last) = self.visible_len(cx).checked_sub(1) {
            self.scroll.scroll_to_item(last, ScrollStrategy::Bottom);
        }
    }

    /// Copies the whole buffer, whatever the filter.
    pub fn copy_all(&self, cx: &mut Context<Self>) {
        let text = self.buffer.read(cx).text();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.visible = match self.filter {
            LevelFilter::All => None,
            filter => Some(Rc::new(visible_rows(self.buffer.read(cx), filter))),
        };
        if self.autoscroll {
            self.scroll_to_end(cx);
        }
        cx.notify();
    }

    fn log_dir(&self, cx: &App) -> Option<PathBuf> {
        let Some(Entry::Healthy(instance)) = self.model.read(cx).entry(&self.id) else {
            return None;
        };
        // No log folder before the first run: open the instance's own.
        let logs = Path::new(CURRENT_LOG)
            .parent()
            .map(|dir| instance.dir.join(dir))
            .filter(|dir| dir.is_dir());
        Some(logs.unwrap_or_else(|| instance.dir.clone()))
    }

    /// After the user scrolled (wheel, scrollbar, keys): autoscroll follows
    /// whether the list ends in view.
    fn follow_scroll(&mut self, window: &Window, cx: &mut Context<Self>) {
        let at_bottom = self.at_bottom(window);
        if at_bottom != self.autoscroll {
            self.autoscroll = at_bottom;
            cx.notify();
        }
    }

    /// Whether the end of the list is in view.
    fn at_bottom(&self, window: &Window) -> bool {
        let (offset, max) = (self.scroll.offset(), self.scroll.max_offset());
        let slack = BOTTOM_SLACK.to_pixels(window.rem_size());
        -offset.y >= max.y - slack
    }

    /// Scrolls the list so `top` (pixels from the start, clamped) is at the
    /// top of the view.
    fn scroll_to(&mut self, top: Pixels, window: &Window, cx: &mut Context<Self>) {
        let max = self.scroll.max_offset().y;
        let top = top.clamp(Pixels::ZERO, max.max(Pixels::ZERO));
        let x = self.scroll.offset().x;
        self.scroll.set_offset(point(x, -top));
        self.follow_scroll(window, cx);
        cx.notify();
    }

    /// One page: the view's height less a row, so a line stays in sight.
    fn page(&self, window: &Window) -> Pixels {
        let row = ROW_H.to_pixels(window.rem_size());
        (self.scroll.bounds().size.height - row).max(row)
    }

    fn page_up(&mut self, _: &LogPageUp, window: &mut Window, cx: &mut Context<Self>) {
        let top = -self.scroll.offset().y - self.page(window);
        self.scroll_to(top, window, cx);
    }

    fn page_down(&mut self, _: &LogPageDown, window: &mut Window, cx: &mut Context<Self>) {
        let top = -self.scroll.offset().y + self.page(window);
        self.scroll_to(top, window, cx);
    }

    fn scroll_top(&mut self, _: &LogTop, window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_to(Pixels::ZERO, window, cx);
    }

    fn scroll_bottom(&mut self, _: &LogBottom, _: &mut Window, cx: &mut Context<Self>) {
        // Follows new lines from here, as at the bottom by any other way.
        self.set_autoscroll(true, cx);
    }

    /// The list's focus handle (tests and Tab order).
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let this = cx.weak_entity();
        let log_dir = self.log_dir(cx);
        let filter_button = |id: &'static str, filter: LevelFilter, label: String| {
            let this = this.clone();
            Button::new(id)
                .ghost()
                .small()
                .selected(self.filter == filter)
                .label(label)
                .on_click(move |_, _, cx| {
                    this.update(cx, |v, cx| v.set_filter(filter, cx)).ok();
                })
        };
        h_flex()
            .id("log-toolbar")
            .flex_none()
            .h(rems(2.5))
            .px(GUTTER)
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(filter_button(
                "log-filter-all",
                LevelFilter::All,
                t!("log.filter_all").to_string(),
            ))
            .child(filter_button(
                "log-filter-warn",
                LevelFilter::WarnUp,
                t!("log.filter_warn").to_string(),
            ))
            .child(div().flex_1())
            .child({
                let this = this.clone();
                Switch::new("log-autoscroll")
                    .checked(self.autoscroll)
                    .label(t!("log.autoscroll").to_string())
                    .on_click(move |on, _, cx| {
                        this.update(cx, |v, cx| v.set_autoscroll(*on, cx)).ok();
                    })
            })
            .child({
                let this = this.clone();
                Button::new("log-copy")
                    .ghost()
                    .small()
                    .label(t!("log.copy_all").to_string())
                    .on_click(move |_, _, cx| {
                        this.update(cx, |v, cx| v.copy_all(cx)).ok();
                    })
            })
            .child(
                Button::new("log-open-folder")
                    .ghost()
                    .small()
                    .label(t!("log.open_folder").to_string())
                    .disabled(log_dir.is_none())
                    .on_click(move |_, _, cx| {
                        if let Some(dir) = &log_dir {
                            cx.open_with_system(dir);
                        }
                    }),
            )
    }

    fn banner(
        &self,
        id: &'static str,
        text: String,
        action: Option<Button>,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .id(id)
            .aria_label(text.clone())
            .test_support()
            .flex_none()
            .px(GUTTER)
            .py_1p5()
            .gap_2()
            .text_size(TEXT_SECONDARY)
            .text_color(theme.muted_foreground)
            .border_b_1()
            .border_color(theme.border)
            .child(div().flex_1().min_w_0().child(text))
            .children(action)
    }
}

impl Render for LogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted, warning, danger) = (
            theme.foreground,
            theme.muted_foreground,
            theme.warning,
            theme.danger,
        );
        let mono = theme.mono_font_family.clone();
        let buffer = self.buffer.read(cx);
        let dropped = buffer.dropped();
        let count = self.visible_len(cx);
        let row_h = ROW_H.to_pixels(window.rem_size());
        let key = (count, row_h);
        let sizes = match &self.sizes {
            Some((k, sizes)) if *k == key => sizes.clone(),
            _ => {
                // Only the heights are used; the list measures the width.
                let sizes = Rc::new(vec![size(Pixels::ZERO, row_h); count]);
                self.sizes = Some((key, sizes.clone()));
                sizes
            }
        };
        let crash = self.model.read(cx).crash_log(&self.id).map(Path::to_owned);
        // The scrollbar moved the list since the last frame.
        if self.scrollbar.moved.take() {
            self.autoscroll = self.at_bottom(window);
        }
        let ring = theme.ring;
        let this = cx.weak_entity();
        v_flex()
            .id("log-view")
            .size_full()
            .child(self.render_toolbar(cx))
            .when_some(crash, |d, path| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let open = Button::new("log-crash-open")
                    .ghost()
                    .small()
                    .label(t!("common.open_folder").to_string())
                    .on_click(move |_, _, cx| cx.reveal_path(&path));
                d.child(self.banner(
                    "log-crash-banner",
                    t!("log.crash_saved", name = name).to_string(),
                    Some(open),
                    cx,
                ))
            })
            .when(dropped > 0, |d| {
                d.child(self.banner(
                    "log-omitted",
                    t!("log.omitted", count = dropped).to_string(),
                    None,
                    cx,
                ))
            })
            .child(
                div()
                    .id("log-lines")
                    // Before `track_focus`, so tests see the focus.
                    .test_support()
                    .key_context(LOG_CONTEXT)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::page_up))
                    .on_action(cx.listener(Self::page_down))
                    .on_action(cx.listener(Self::scroll_top))
                    .on_action(cx.listener(Self::scroll_bottom))
                    .on_mouse_down(MouseButton::Left, {
                        let focus = self.focus.clone();
                        move |_, window, cx| focus.focus(window, cx)
                    })
                    .relative()
                    .flex_1()
                    .min_h_0()
                    // A 1px hairline that shows only while the keyboard is
                    // here.
                    .border_1()
                    .border_color(gpui_kit::transparent_black())
                    .focus_visible(move |s| s.border_color(ring))
                    .font_family(mono)
                    .text_size(TEXT_SECONDARY)
                    // The list handles the wheel first; then this sees where
                    // it ended up.
                    .on_scroll_wheel(move |_, window, cx| {
                        this.update(cx, |v, cx| v.follow_scroll(window, cx)).ok();
                    })
                    .child(
                        v_virtual_list(
                            cx.entity(),
                            "log-rows",
                            sizes,
                            move |this, range, _, cx| {
                                let buffer = this.buffer.read(cx);
                                range
                                    .filter_map(|row| {
                                        let ix = match &this.visible {
                                            Some(rows) => *rows.get(row)?,
                                            None => row,
                                        };
                                        let (level, text) = buffer.get(ix)?;
                                        let color = match level {
                                            LogLevel::Warn => warning,
                                            LogLevel::Error | LogLevel::Fatal => danger,
                                            LogLevel::Trace | LogLevel::Debug => muted,
                                            LogLevel::Info | LogLevel::Unknown => fg,
                                        };
                                        Some(
                                            div()
                                                .h(ROW_H)
                                                .px(GUTTER)
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                .whitespace_nowrap()
                                                .text_color(color)
                                                .child(text.clone()),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            },
                        )
                        .track_scroll(&self.scroll)
                        .size_full(),
                    )
                    .vertical_scrollbar(&self.scrollbar),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::{AppContext, TestAppContext};
    use patina_core::launch::Stream;

    use super::*;

    fn line(level: LogLevel, text: &str) -> LogLine {
        LogLine {
            level,
            text: text.to_string(),
            stream: Stream::Stdout,
        }
    }

    #[test]
    fn log_buffer_is_bounded() {
        let mut buffer = LogBuffer::new();
        for n in 0..300_000 {
            buffer.push(LogLevel::Info, format!("line {n}"));
        }
        assert_eq!(buffer.len(), LOG_VIEW_MAX_LINES);
        assert_eq!(buffer.dropped(), 280_000);
        assert_eq!(buffer.get(buffer.len() - 1).unwrap().1, "line 299999");
        assert_eq!(buffer.get(0).unwrap().1, "line 280000");
        buffer.clear();
        assert_eq!((buffer.len(), buffer.dropped()), (0, 0));
    }

    #[test]
    fn level_filter() {
        use LogLevel::*;
        for level in [Trace, Debug, Info, Unknown] {
            assert!(LevelFilter::All.admits(level));
            assert!(!LevelFilter::WarnUp.admits(level), "{level:?}");
        }
        for level in [Warn, Error, Fatal] {
            assert!(LevelFilter::All.admits(level));
            assert!(LevelFilter::WarnUp.admits(level), "{level:?}");
        }
    }

    #[test]
    fn warn_filter_keeps_stack_traces() {
        use LogLevel::*;
        let mut buffer = LogBuffer::new();
        for level in [Error, Unknown, Unknown, Info, Unknown] {
            buffer.push(level, "x");
        }
        assert_eq!(visible_rows(&buffer, LevelFilter::WarnUp), [0, 1, 2]);
        assert_eq!(visible_rows(&buffer, LevelFilter::All).len(), 5);
        // Nothing classified before: hidden.
        let mut buffer = LogBuffer::new();
        for level in [Unknown, Warn, Unknown] {
            buffer.push(level, "x");
        }
        assert_eq!(visible_rows(&buffer, LevelFilter::WarnUp), [1, 2]);
    }

    #[test]
    fn log_buffer_caps_bytes() {
        let mut buffer = LogBuffer::new();
        let line = "x".repeat(1024 * 1024);
        for _ in 0..40 {
            buffer.push(LogLevel::Info, line.clone());
        }
        assert_eq!(buffer.len(), 32);
        assert_eq!(buffer.dropped(), 8);
        // One oversized line is still kept.
        let mut buffer = LogBuffer::new();
        buffer.push(LogLevel::Info, "y".repeat(LOG_VIEW_MAX_BYTES + 1));
        assert_eq!(buffer.len(), 1);
        buffer.clear();
        assert_eq!(buffer.text(), "");
    }

    #[test]
    fn copy_text_joins_lines() {
        let mut buffer = LogBuffer::new();
        buffer.push(LogLevel::Info, "a");
        buffer.push(LogLevel::Error, "b");
        assert_eq!(buffer.text(), "a\nb");
        assert_eq!(LogBuffer::new().text(), "");
    }

    #[gpui_kit::test]
    fn batches_coalesce_notifies(cx: &mut TestAppContext) {
        let buffer = cx.new(|_| LogBuffer::new());
        let notifies = Rc::new(Cell::new(0));
        let counter = notifies.clone();
        let _sub = cx.update(|cx| cx.observe(&buffer, move |_, _| counter.set(counter.get() + 1)));
        let (tx, rx) = tokio::sync::mpsc::channel(4096);
        let task = cx.update(|cx| forward_logs(cx, buffer.downgrade(), rx));

        // A burst inside one window is one batch.
        for n in 0..1000 {
            tx.try_send(line(LogLevel::Info, &format!("burst {n}")))
                .unwrap();
        }
        cx.run_until_parked();
        cx.executor().advance_clock(LOG_BATCH_WINDOW);
        cx.run_until_parked();
        assert_eq!(notifies.get(), 1);
        assert_eq!(buffer.read_with(cx, |b, _| b.len()), 1000);

        // The next burst is the next batch, not one notify per line.
        for n in 0..3 {
            tx.try_send(line(LogLevel::Warn, &format!("again {n}")))
                .unwrap();
        }
        cx.run_until_parked();
        cx.executor().advance_clock(LOG_BATCH_WINDOW);
        cx.run_until_parked();
        assert_eq!(notifies.get(), 2);
        assert_eq!(buffer.read_with(cx, |b, _| b.len()), 1003);

        // Closing the channel flushes the last lines and ends the task.
        tx.try_send(line(LogLevel::Error, "last")).unwrap();
        drop(tx);
        cx.run_until_parked();
        cx.executor().advance_clock(LOG_BATCH_WINDOW);
        cx.run_until_parked();
        assert_eq!(notifies.get(), 3);
        assert_eq!(
            buffer.read_with(cx, |b, _| b.get(1003).map(|l| l.0)),
            Some(LogLevel::Error)
        );
        drop(task);
    }
}
