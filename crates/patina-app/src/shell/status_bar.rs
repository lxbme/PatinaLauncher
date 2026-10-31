//! The status bar: download source, Java runtimes, instance count and disk
//! use, version. Small, muted, `status_bar` fill.
//!
//! Everything but the version is read in the background; an item shows once
//! it is known. The config and the runtime count are re-read at startup and
//! whenever the window becomes active. The disk-use scan is heavier: one at a
//! time, and on activation at most once per [`SCAN_INTERVAL`]; code that
//! adds or removes instances calls [`StatusBar::refresh_now`].

use std::time::{Duration, Instant};

use gpui_kit::component::separator::Separator;
use gpui_kit::component::status_bar::StatusBar as Bar;
use gpui_kit::component::{ActiveTheme, h_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Rems, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Window,
    div, rems,
};
use patina_core::config::SourceChoice;
use patina_core::instance::{InstanceError, InstanceStore};
use patina_core::java::ManagedRuntimes;
use rust_i18n::t;

use crate::core_bridge::{Core, Revision, run_blocking};
use crate::disk_usage::{Walk, format_size};
use crate::layout::{GUTTER, TEXT_SECONDARY};

/// Status bar height at UI scale 1.0 (26 px).
pub const STATUS_H: Rems = Rems(1.625);

/// Shortest time between the starts of two disk-use scans that window
/// activation asks for. [`StatusBar::refresh_now`] is not throttled.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(60);

/// Most directory entries one disk-use scan visits, shared out between the
/// instances ([`instance_budget`]).
const MAX_SCAN_ENTRIES: usize = 1_000_000;
/// Fewest entries any one instance may visit, however many there are.
const MIN_INSTANCE_ENTRIES: usize = 10_000;

/// How many entries the walk of each of `count` instances may visit: an
/// equal share of [`MAX_SCAN_ENTRIES`], so one huge instance cannot use up
/// the others' budget, but never fewer than [`MIN_INSTANCE_ENTRIES`].
fn instance_budget(count: usize) -> usize {
    (MAX_SCAN_ENTRIES / count.max(1)).max(MIN_INSTANCE_ENTRIES)
}

/// How many instances there are and how much disk they take. Only healthy
/// instances count (the ones the instance list shows); a directory whose
/// config is broken is left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct InstanceUsage {
    pub count: usize,
    /// Bytes of the regular files under their directories; symbolic links
    /// are neither followed nor counted.
    pub bytes: u64,
    /// False when some instance's walk stopped at its entry budget: `bytes`
    /// is then a lower bound.
    pub complete: bool,
}

impl InstanceUsage {
    /// Lists the instances in `store` and adds up their directories. Blocking.
    pub fn scan(store: &InstanceStore) -> Result<Self, InstanceError> {
        Self::scan_with(store, instance_budget)
    }

    /// As [`InstanceUsage::scan`], each instance walking at most
    /// `budget(count)` entries.
    fn scan_with(
        store: &InstanceStore,
        budget: impl Fn(usize) -> usize,
    ) -> Result<Self, InstanceError> {
        let listed = store.list()?;
        let count = listed.instances.len();
        let per_instance = budget(count);
        let (mut bytes, mut complete) = (0, true);
        for instance in &listed.instances {
            let mut walk = Walk::new(per_instance);
            complete &= walk.add_tree(&instance.dir);
            bytes += walk.bytes();
        }
        Ok(Self {
            count,
            bytes,
            complete,
        })
    }
}

/// When a disk-use scan may start: one at a time, and an unforced request
/// only [`SCAN_INTERVAL`] after the last scan started. A forced request that
/// arrives during a scan runs right after it.
#[derive(Debug, Default)]
struct ScanGate {
    running: bool,
    last_start: Option<Instant>,
    again: bool,
}

impl ScanGate {
    /// Whether a scan asked for at `now` starts now (and is then running).
    fn request(&mut self, now: Instant, forced: bool) -> bool {
        if self.running {
            self.again |= forced;
            return false;
        }
        let recent = (self.last_start).is_some_and(|t| now.duration_since(t) < SCAN_INTERVAL);
        if recent && !forced {
            return false;
        }
        self.running = true;
        self.last_start = Some(now);
        true
    }

    /// The running scan ended at `now`; true if a forced request came in
    /// meanwhile and the next scan starts now.
    fn finish(&mut self, now: Instant) -> bool {
        self.running = false;
        if std::mem::take(&mut self.again) {
            self.running = true;
            self.last_start = Some(now);
            return true;
        }
        false
    }
}

/// The status bar view; owns what it shows.
pub struct StatusBar {
    source: Option<SourceChoice>,
    runtimes: Option<usize>,
    usage: Option<InstanceUsage>,
    /// Guards the config and runtime reads.
    revision: Revision,
    quick: Task<()>,
    gate: ScanGate,
    scan: Task<()>,
    _activation: Subscription,
}

impl StatusBar {
    /// Loads its data now and whenever the window becomes active.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.refresh(cx);
            }
        });
        let mut this = Self {
            source: None,
            runtimes: None,
            usage: None,
            revision: Revision::default(),
            quick: Task::ready(()),
            gate: ScanGate::default(),
            scan: Task::ready(()),
            _activation,
        };
        this.refresh(cx);
        this
    }

    /// The configured download-source preference, once read.
    pub fn source(&self) -> Option<SourceChoice> {
        self.source
    }

    /// How many managed Java runtimes are installed, once counted.
    pub fn runtimes(&self) -> Option<usize> {
        self.runtimes
    }

    /// Instance count and disk use from the last scan that succeeded.
    pub fn usage(&self) -> Option<InstanceUsage> {
        self.usage
    }

    /// Whether a disk-use scan is running.
    pub fn is_scanning(&self) -> bool {
        self.gate.running
    }

    /// Re-reads the config and the runtime count, and scans the disk use
    /// unless a scan is running or one started within [`SCAN_INTERVAL`].
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.read_quick(cx);
        self.request_scan(false, cx);
    }

    /// As [`StatusBar::refresh`], but scans regardless of when the last scan
    /// ran (right after the running one, if any). For code that has just
    /// created, deleted or changed instances.
    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        self.read_quick(cx);
        self.request_scan(true, cx);
    }

    /// The config and the runtime count; a newer read supersedes this one.
    fn read_quick(&mut self, cx: &mut Context<Self>) {
        let id = self.revision.start();
        let config = Core::read_config(cx);
        let runtimes = Core::global(cx).data.runtimes();
        let runtimes = run_blocking(cx, move || {
            ManagedRuntimes::new(runtimes).list_installed().len()
        });
        self.quick = cx.spawn(async move |this, cx| {
            let source = config
                .await
                .inspect_err(
                    |error| tracing::warn!(%error, "status bar: reading the config failed"),
                )
                .ok()
                .map(|config| config.download.source);
            let runtimes = runtimes
                .await
                .inspect_err(|error| tracing::warn!(%error, "status bar: counting runtimes failed"))
                .ok();
            this.update(cx, |this, cx| {
                if this.revision.is_current(id) {
                    // A failed read keeps what was shown.
                    this.source = source.or(this.source);
                    this.runtimes = runtimes.or(this.runtimes);
                    cx.notify();
                }
            })
            .ok();
        });
    }

    fn request_scan(&mut self, forced: bool, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        if self.gate.request(now, forced) {
            self.start_scan(cx);
        }
    }

    /// Starts a scan; the gate already counts it as running. Only one runs
    /// at a time, so the stored task is never a live scan being replaced.
    fn start_scan(&mut self, cx: &mut Context<Self>) {
        let store = Core::global(cx).store.clone();
        let scan = run_blocking(cx, move || InstanceUsage::scan(&store));
        self.scan = cx.spawn(async move |this, cx| {
            let usage = match scan.await {
                Ok(Ok(usage)) => Some(usage),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "status bar: listing the instances failed");
                    None
                }
                Err(error) => {
                    tracing::warn!(%error, "status bar: the disk scan failed");
                    None
                }
            };
            this.update(cx, |this, cx| {
                // A failed scan keeps the last good value.
                if let Some(usage) = usage {
                    this.usage = Some(usage);
                }
                let now = cx.background_executor().now();
                if this.gate.finish(now) {
                    // After this task, which `start_scan` would replace.
                    let this = cx.weak_entity();
                    cx.defer(move |cx| {
                        this.update(cx, |this, cx| this.start_scan(cx)).ok();
                    });
                }
                cx.notify();
            })
            .ok();
        });
    }
}

/// One status item, with its text as its accessible name.
fn item(id: &'static str, text: String) -> impl IntoElement {
    let text = SharedString::from(text);
    div()
        .id(id)
        .aria_label(text.clone())
        .flex_none()
        .child(text)
        .test_support()
}

impl Render for StatusBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, border, muted) = (
            theme.status_bar,
            theme.status_bar_border,
            theme.muted_foreground,
        );
        let sep = || Separator::vertical().h(rems(0.75));

        let mut left: Vec<AnyElement> = Vec::new();
        if let Some(source) = self.source {
            let text = t!("shell.status.source", source = source.display_name()).to_string();
            left.push(item("status-source", text).into_any_element());
        }
        if let Some(n) = self.runtimes {
            let text = t!("shell.status.runtimes", count = n).to_string();
            left.push(item("status-runtimes", text).into_any_element());
        }
        if let Some(u) = self.usage {
            let size = format_size(u.bytes);
            let text = if u.complete {
                t!("shell.status.instances", count = u.count, size = size)
            } else {
                t!(
                    "shell.status.instances_partial",
                    count = u.count,
                    size = size
                )
            };
            left.push(item("status-instances", text.to_string()).into_any_element());
        }
        let mut items = h_flex().gap_2p5();
        for (i, element) in left.into_iter().enumerate() {
            items = items.when(i > 0, |d| d.child(sep())).child(element);
        }
        let version = t!("shell.status.version", version = env!("CARGO_PKG_VERSION")).to_string();

        Bar::new()
            .h(STATUS_H)
            .py_0()
            .px(GUTTER)
            .gap_2p5()
            .bg(bg)
            .border_color(border)
            .whitespace_nowrap()
            .overflow_hidden()
            .text_size(TEXT_SECONDARY)
            .text_color(muted)
            .left(items)
            .right(item("status-version", version))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Bytes of the regular files under `dir`, not following links.
    fn regular_bytes(dir: &Path) -> u64 {
        let mut walk = Walk::new(usize::MAX);
        assert!(walk.add_tree(dir));
        walk.bytes()
    }

    #[test]
    fn each_instance_has_its_own_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path());
        let big = store.create("Big", "1.21.1").unwrap();
        for i in 0..20 {
            std::fs::write(big.dir.join(format!("f{i}")), [0u8; 10]).unwrap();
        }
        let small = store.create("Small", "1.21.1").unwrap();
        std::fs::write(small.dir.join("only"), [0u8; 7]).unwrap();
        // A broken instance is not counted.
        let broken = tmp.path().join("Broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("instance.toml"), "garbage[[").unwrap();
        std::fs::write(broken.join("data"), [0u8; 5000]).unwrap();

        let full = InstanceUsage::scan(&store).unwrap();
        assert!(full.complete);
        assert_eq!(full.count, 2);
        assert_eq!(
            full.bytes,
            regular_bytes(&big.dir) + regular_bytes(&small.dir)
        );

        // Five entries each: the big one stops short, the small one (two
        // entries) is still counted in full.
        let cut = InstanceUsage::scan_with(&store, |count| {
            assert_eq!(count, 2);
            5
        })
        .unwrap();
        assert!(!cut.complete);
        assert_eq!(cut.count, 2);
        assert!(cut.bytes >= regular_bytes(&small.dir));
        assert!(cut.bytes < full.bytes);
    }

    #[test]
    fn budget_is_shared_with_a_floor() {
        assert_eq!(instance_budget(0), MAX_SCAN_ENTRIES);
        assert_eq!(instance_budget(1), MAX_SCAN_ENTRIES);
        assert_eq!(instance_budget(4), MAX_SCAN_ENTRIES / 4);
        assert_eq!(instance_budget(1_000_000), MIN_INSTANCE_ENTRIES);
    }

    #[test]
    fn scans_are_single_flight_and_throttled() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut gate = ScanGate::default();

        assert!(gate.request(at(0), false), "the first scan starts");
        assert!(!gate.request(at(1), false), "one at a time");
        assert!(!gate.finish(at(2)), "nothing forced meanwhile");
        assert!(!gate.request(at(30), false), "throttled after it started");
        assert!(
            gate.request(at(60), false),
            "allowed again after the interval"
        );
        assert!(!gate.finish(at(61)));

        // A forced request skips the throttle...
        assert!(gate.request(at(62), true));
        // ...and, during a scan, runs right after it, once.
        assert!(!gate.request(at(63), true));
        assert!(!gate.request(at(63), true));
        assert!(!gate.request(at(63), false));
        assert!(gate.finish(at(64)), "the forced rescan starts");
        assert!(gate.running);
        assert!(!gate.finish(at(65)), "only once");
        assert!(
            !gate.request(at(100), false),
            "the rescan restarted the throttle"
        );
        assert!(gate.request(at(124), false));
    }
}
