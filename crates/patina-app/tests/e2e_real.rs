//! Real end-to-end acceptance: the app's views over the REAL core (real
//! downloads, real loader installs, real Minecraft), driven through the same
//! headless window API as `tests/ui.rs`. Mod jars are generated fixtures;
//! the settings-folder check intercepts the opener instead of opening a
//! desktop file manager. These are not screenshot or click-only evidence.
//!
//! The suite builds only with the `e2e` feature (which brings
//! `dev-offline` and `test-support`), and every test is `#[ignore]`d on top:
//! they need the network, a real Java, minutes of time, and
//! `PATINA_DATA_DIR` set to a disposable data dir that has
//!
//! - an offline account (`e2e` builds with `dev-offline`),
//! - the instance `forge-1201` with its game files (the launch tests start
//!   it; a real Minecraft window opens),
//! - the `patina-dev` binary built next to this test (`cargo build -p
//!   patina-dev --features dev-offline`) for the cross-process checks.
//!
//! Run them one at a time, in name order:
//!
//! ```text
//! PATINA_DATA_DIR=… cargo test -p patina-app --features e2e \
//!     --test e2e_real -- --ignored --test-threads=1 --nocapture <name>
//! ```
//!
//! Instances these tests create are named `E2E …`; `e2e_99_cleanup` deletes
//! whatever is left of them. Games are stopped only through the app's own
//! stop (the game's identity-checked `KillHandle`); `e2e_05a` deliberately
//! leaves its game running for `e2e_05b` to pick up.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use gpui_kit::test::{ElementSnapshot, TestWindowExt};
use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};
use patina_app::content::{
    ImageCache, ImageKey, LoadState, ModItem, ModsTab, mod_disabled_id, mod_row_id, mod_switch_id,
};
use patina_app::core_bridge::Core;
use patina_app::i18n::Language;
use patina_app::instances::{
    DetailTab, InstancesModel, RunState, loader_segment_id, pick_id, row_id, version_row_id,
};
use patina_app::launch::OpState;
use patina_app::search::{ModGroup, SearchItem, SearchModel, SearchResults, search_row_id};
use patina_app::settings::{AfterLaunch, AppSettings, SettingsStore};
use patina_app::settings_page::scale_step_id;
use patina_app::shell::{AccountLabel, Nav, RootView};
use patina_core::instance::{ComponentUid, InstanceIcon, InstanceStore, WindowSize};
use patina_core::jvm::GcPreset;
use patina_core::loader::LoaderKind;
use patina_core::paths::DataDirs;

const VANILLA: &str = "E2E Vanilla";
const FABRIC: &str = "E2E Fabric";
const FORGE: &str = "E2E Forge";
const NEOFORGE: &str = "E2E NeoForge";
/// The existing instance the launch tests start.
const FORGE_1201: &str = "forge-1201";

/// Longest a real install or a real game start may take.
const SLOW: Duration = Duration::from_secs(15 * 60);

/// The booted app over the real core.
struct Booted {
    handle: AnyWindowHandle,
    root: Entity<RootView>,
    _rt: tokio::runtime::Runtime,
    data: DataDirs,
    store: InstanceStore,
}

/// The data dir under test; refuses to run without an explicit one.
fn data_dir() -> PathBuf {
    let dir = std::env::var_os("PATINA_DATA_DIR")
        .expect("set PATINA_DATA_DIR to a disposable data dir for the real e2e tests");
    PathBuf::from(dir)
}

/// Boots the app as `main` does (the real `Core::init`), in `language`.
fn boot_in(cx: &mut TestAppContext, language: Language) -> Booted {
    patina_app::i18n::set_language(language);
    let dir = data_dir();
    let data = DataDirs::new(&dir);
    let rt = patina_app::runtime::start();
    let core = Core::init(rt.handle().clone(), data.clone()).expect("the real core starts");
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    let mut start = AppSettings::default();
    start.language = language;
    let (handle, root) = cx.update(|cx| {
        cx.set_global(core);
        patina_app::init(SettingsStore::new(dir.clone(), start), cx);
        patina_app::open_main_window(cx).expect("open the main window")
    });
    Booted {
        handle,
        root,
        _rt: rt,
        store: InstanceStore::new(data.instances()),
        data,
    }
}

fn boot(cx: &mut TestAppContext) -> Booted {
    boot_in(cx, Language::En)
}

/// Runs gpui (moving its clock along with the real one) until `done`
/// holds, for at most `limit`.
fn wait_for(
    cx: &mut TestAppContext,
    limit: Duration,
    what: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    let start = Instant::now();
    while start.elapsed() < limit {
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{what}: not within {limit:?}");
}

fn settle(cx: &mut TestAppContext, done: impl FnMut(&mut TestAppContext) -> bool) {
    wait_for(cx, Duration::from_secs(60), "settle", done);
}

/// Lets real time pass with gpui running.
fn idle(cx: &mut TestAppContext, d: Duration) {
    let start = Instant::now();
    wait_for(cx, d + Duration::from_secs(5), "idle", |_| {
        start.elapsed() >= d
    });
}

fn model(app: &Booted, cx: &mut TestAppContext) -> Entity<InstancesModel> {
    cx.update(|cx| app.root.read(cx).instances().read(cx).model().clone())
}

fn loaded(app: &Booted, cx: &mut TestAppContext) -> Entity<InstancesModel> {
    let m = model(app, cx);
    settle(cx, |cx| {
        m.read_with(cx, |m, _| m.is_loaded() && !m.is_refreshing())
    });
    m
}

fn element(
    app: &Booted,
    cx: &mut TestAppContext,
    id: impl Into<gpui_kit::ElementId>,
) -> Option<ElementSnapshot> {
    let id = id.into();
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id)
    })
    .unwrap()
}

fn shows(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) -> bool {
    element(app, cx, id).is_some_and(|e| e.visible())
}

fn label_of(
    app: &Booted,
    cx: &mut TestAppContext,
    id: impl Into<gpui_kit::ElementId>,
) -> Option<String> {
    element(app, cx, id).and_then(|e| e.label().map(str::to_owned))
}

fn value_of(
    app: &Booted,
    cx: &mut TestAppContext,
    id: impl Into<gpui_kit::ElementId>,
) -> Option<String> {
    element(app, cx, id).and_then(|e| e.value().map(str::to_owned))
}

fn disabled(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) -> bool {
    element(app, cx, id).and_then(|e| e.disabled()) == Some(true)
}

fn click(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) {
    let id = id.into();
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn press(app: &Booted, cx: &mut TestAppContext, key: &str) {
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.press(key, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn type_text(app: &Booted, cx: &mut TestAppContext, text: &str) {
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.input(text, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn dialog_label(app: &Booted, cx: &mut TestAppContext, id: &'static str) -> String {
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let found = window.within("dialog").find(id);
        found.label().map(str::to_owned).unwrap_or_default()
    })
    .unwrap()
}

fn dialog_click(app: &Booted, cx: &mut TestAppContext, id: &'static str) {
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn activate(app: &Booted, cx: &mut TestAppContext) {
    cx.update_window(app.handle, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    cx.update_window(app.handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

fn run_state(m: &Entity<InstancesModel>, cx: &mut TestAppContext, id: &str) -> Option<RunState> {
    m.read_with(cx, |m, cx| m.run_state(id, cx).cloned())
}

fn op_of(m: &Entity<InstancesModel>, cx: &mut TestAppContext, id: &str) -> Option<OpState> {
    m.read_with(cx, |m, cx| m.op(id, cx).cloned())
}

fn selected(m: &Entity<InstancesModel>, cx: &mut TestAppContext) -> Option<String> {
    m.read_with(cx, |m, _| m.selected().map(str::to_owned))
}

/// Selects `id` by clicking its row in the list.
fn select(app: &Booted, cx: &mut TestAppContext, id: &str) {
    let m = model(app, cx);
    click(app, cx, row_id(id));
    assert_eq!(selected(&m, cx).as_deref(), Some(id));
}

/// Waits for the operation on `id` to end, recording what its header said
/// on the way. Panics if it failed.
fn wait_op(app: &Booted, cx: &mut TestAppContext, id: &str, what: &str) -> BTreeSet<String> {
    let m = model(app, cx);
    let mut seen = BTreeSet::new();
    let mut bar = false;
    wait_for(cx, SLOW, what, |cx| {
        if let Some(text) = label_of(app, cx, "instance-op-text") {
            seen.insert(text);
        }
        bar |= shows(app, cx, "instance-op-bar");
        match op_of(&m, cx, id) {
            Some(OpState::Running { .. }) => false,
            Some(OpState::Failed { message, .. }) => panic!("{what} failed: {message}"),
            None => true,
        }
    });
    eprintln!("[{what}] progress bar shown: {bar}; header texts: {seen:?}");
    seen
}

/// Replaces the text of input `id`.
fn replace_text(app: &Booted, cx: &mut TestAppContext, id: &'static str, text: &str) {
    click(app, cx, id);
    press(app, cx, "secondary-a");
    type_text(app, cx, text);
}

/// Scrolls the settings tab until element `id` (if drawn) is wholly in
/// view: a click lands on its center, which must not be clipped.
fn reveal(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) {
    let id = id.into();
    for _ in 0..60 {
        let Some(found) = element(app, cx, id.clone()) else {
            return;
        };
        let Some(tab) = element(app, cx, "settings-tab").map(|t| t.bounds()) else {
            return;
        };
        let b = found.bounds();
        let dy = if b.top() < tab.top() {
            gpui_kit::px(60.)
        } else if b.bottom() > tab.bottom() {
            gpui_kit::px(-60.)
        } else {
            return;
        };
        cx.update_window(app.handle, |_, window, cx| {
            window.scroll(
                "settings-tab",
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(gpui_kit::px(0.), dy)),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
    }
}

fn sclick(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) {
    let id = id.into();
    reveal(app, cx, id.clone());
    click(app, cx, id);
}

fn sshows(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) -> bool {
    let id = id.into();
    reveal(app, cx, id.clone());
    shows(app, cx, id)
}

fn sreplace(app: &Booted, cx: &mut TestAppContext, id: &'static str, text: &str) {
    sclick(app, cx, id);
    press(app, cx, "secondary-a");
    type_text(app, cx, text);
}

fn blur(app: &Booted, cx: &mut TestAppContext) {
    sclick(app, cx, "settings-name");
    cx.update_window(app.handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

fn click_radio(app: &Booted, cx: &mut TestAppContext, group: &'static str, ix: usize) {
    reveal(app, cx, group);
    for _ in 0..20 {
        let seen = cx
            .update_window(app.handle, |_, window, cx| {
                window.render_frame(cx);
                window
                    .within(group)
                    .try_find(ix)
                    .is_some_and(|e| e.visible())
            })
            .unwrap();
        if seen {
            break;
        }
        cx.update_window(app.handle, |_, window, cx| {
            window.scroll(
                "settings-tab",
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
                    gpui_kit::px(0.),
                    gpui_kit::px(-60.),
                )),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
    }
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within(group).click(ix, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// The `patina-dev` binary built next to this test binary.
fn patina_dev() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let dev = dir.join("patina-dev");
    assert!(
        dev.is_file(),
        "build patina-dev first: cargo build -p patina-dev --features dev-offline"
    );
    dev
}

/// Runs `patina-dev args…` on the data dir; returns (success, output).
fn dev(args: &[&str]) -> (bool, String) {
    let out = Command::new(patina_dev())
        .args(args)
        .env("PATINA_DATA_DIR", data_dir())
        .output()
        .expect("run patina-dev");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!("$ patina-dev {} -> {}\n{text}", args.join(" "), out.status);
    (out.status.success(), text)
}

/// Clicks 启动 on the selected instance and gets it running here: answers
/// the pre-launch warnings (if any) with 继续启动. Returns the game's pid
/// and what the header said while starting.
fn launch_selected(app: &Booted, cx: &mut TestAppContext, id: &str) -> (u32, BTreeSet<String>) {
    let m = model(app, cx);
    assert!(!disabled(app, cx, "launch"));
    click(app, cx, "launch");
    let mut seen = BTreeSet::new();
    let mut bar = false;
    wait_for(cx, SLOW, "game start", |cx| {
        if let Some(text) = label_of(app, cx, "instance-op-text") {
            seen.insert(text);
        }
        bar |= shows(app, cx, "instance-op-bar");
        if shows(app, cx, "dialog") && element(app, cx, "prelaunch-title").is_some() {
            eprintln!(
                "pre-launch warnings: {}",
                dialog_label(app, cx, "prelaunch-title")
            );
            dialog_click(app, cx, "ok");
        }
        if let Some(OpState::Failed { message, .. }) = op_of(&m, cx, id) {
            panic!("launch failed: {message}");
        }
        matches!(run_state(&m, cx, id), Some(RunState::Running { .. }))
    });
    let Some(RunState::Running { pid, .. }) = run_state(&m, cx, id) else {
        unreachable!()
    };
    eprintln!("[launch {id}] pid {pid}; progress bar shown: {bar}; header texts: {seen:?}");
    (pid, seen)
}

/// Force-stops `id` through the header's 强制关闭 and its confirmation.
fn force_stop(app: &Booted, cx: &mut TestAppContext, id: &str) {
    let m = model(app, cx);
    click(app, cx, "instance-stop");
    assert!(shows(app, cx, "dialog"), "force stop asks first");
    eprintln!("stop dialog: {}", dialog_label(app, cx, "stop-title"));
    dialog_click(app, cx, "ok");
    wait_for(cx, Duration::from_secs(60), "game stop", |cx| {
        !matches!(
            run_state(&m, cx, id),
            Some(RunState::Running { .. } | RunState::HeldElsewhere { .. })
        )
    });
}

fn log_text(app: &Booted, cx: &mut TestAppContext, id: &str) -> String {
    let m = model(app, cx);
    let buffer = m.update(cx, |m, cx| m.log_buffer(id, cx));
    buffer.read_with(cx, |b, _| b.text())
}

fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|c| *c == 0)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether process `pid` is gone (or a zombie no longer running).
fn gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => {
            stat.rsplit(')')
                .next()
                .and_then(|r| r.split_whitespace().next())
                == Some("Z")
        }
    }
}

// ---- 1. list, overview, account ------------------------------------------

#[gpui_kit::test]
#[ignore = "real data dir"]
fn e2e_01_list_overview_and_filters(cx: &mut TestAppContext) {
    let app = boot(cx);
    let m = loaded(&app, cx);
    let ids: Vec<String> = m.read_with(cx, |m, _| {
        m.instances().iter().map(|i| i.id.clone()).collect()
    });
    eprintln!("instances: {ids:?}");
    for id in [
        "fab-new",
        "forge-1122",
        "forge-1201",
        "forge-164",
        "forge-1710",
        "neo-1211",
        "neo-new",
    ] {
        assert!(ids.iter().any(|i| i == id), "{id} listed");
        assert!(shows(&app, cx, row_id(id)), "{id} row drawn");
    }
    let title = cx.update(|cx| app.root.read(cx).title_bar().clone());
    settle(cx, |cx| {
        title.read_with(cx, |t, _| t.account().clone()) != AccountLabel::Loading
    });
    assert_eq!(
        title.read_with(cx, |t, _| t.account().clone()),
        AccountLabel::SignedIn("Dev".into())
    );

    // Selecting shows its overview.
    select(&app, cx, "neo-1211");
    assert!(shows(&app, cx, "instance-detail"));
    assert_eq!(
        m.read_with(cx, |m, _| m.display_name("neo-1211"))
            .as_deref(),
        Some("neo-1211")
    );
    assert!(shows(&app, cx, "launch"));
    let detail = cx.update(|cx| app.root.read(cx).instances().read(cx).detail().clone());
    settle(cx, |cx| {
        detail.read_with(cx, |d, _| {
            d.stats("neo-1211").is_some_and(|s| s.mods.is_some())
        })
    });
    let stats = detail.read_with(cx, |d, _| format!("{:?}", d.stats("neo-1211")));
    eprintln!("neo-1211 stats: {stats}");

    // Filters: NeoForge shows only the NeoForge ones.
    click(&app, cx, "filter-neoforge");
    for (id, neo) in [("neo-1211", true), ("neo-new", true), ("forge-1201", false)] {
        assert_eq!(shows(&app, cx, row_id(id)), neo, "{id} under NeoForge");
    }
    click(&app, cx, "filter-all");
    assert!(shows(&app, cx, row_id("forge-1201")));
}

// ---- 2. create instances with each loader --------------------------------

/// Opens the add dialog, picks Minecraft `mc` and `loader`, checks the
/// generated name, renames it to `name` and creates it; waits for the
/// install. Returns the header texts seen.
fn create(
    app: &Booted,
    cx: &mut TestAppContext,
    mc: &str,
    loader: Option<LoaderKind>,
    expected_name: &str,
    name: &str,
) -> BTreeSet<String> {
    let m = model(app, cx);
    click(app, cx, "instances-add");
    assert!(shows(app, cx, "dialog"));
    settle(cx, |cx| shows(app, cx, "version-search"));
    click(app, cx, "version-search");
    type_text(app, cx, mc);
    wait_for(cx, Duration::from_secs(120), "version list", |cx| {
        shows(app, cx, version_row_id(mc))
    });
    click(app, cx, version_row_id(mc));
    wait_for(cx, Duration::from_secs(120), "loader versions", |cx| {
        !disabled(app, cx, loader_segment_id(loader))
    });
    click(app, cx, loader_segment_id(loader));
    settle(cx, |cx| {
        value_of(app, cx, "add-instance-name").as_deref() == Some(expected_name)
    });
    // The recommended (or latest) loader version is preselected.
    wait_for(cx, Duration::from_secs(120), "create enabled", |cx| {
        !disabled(app, cx, "add-instance-create")
    });
    replace_text(app, cx, "add-instance-name", name);
    assert_eq!(
        value_of(app, cx, "add-instance-name").as_deref(),
        Some(name)
    );
    click(app, cx, "add-instance-create");
    assert!(!shows(app, cx, "dialog"));
    settle(cx, |cx| selected(&m, cx).as_deref() == Some(name));
    let seen = wait_op(app, cx, name, &format!("install {name}"));
    let inst = app.store.load(name).unwrap();
    assert_eq!(inst.config.mc_version(), mc);
    eprintln!("{name}: loader {:?}", inst.config.loader());
    seen
}

#[gpui_kit::test]
#[ignore = "real network installs"]
fn e2e_02_create_one_per_loader(cx: &mut TestAppContext) {
    let app = boot(cx);
    loaded(&app, cx);
    for id in [VANILLA, FABRIC, FORGE, NEOFORGE] {
        assert!(app.store.load(id).is_err(), "{id} left from an earlier run");
    }

    create(&app, cx, "1.21.1", None, "1.21.1", VANILLA);
    assert!(app.store.load(VANILLA).unwrap().config.loader().is_none());

    let kinds = [
        (
            FABRIC,
            "26.3",
            LoaderKind::Fabric,
            "Fabric 26.3",
            ComponentUid::Fabric,
        ),
        (
            FORGE,
            "1.20.1",
            LoaderKind::Forge,
            "Forge 1.20.1",
            ComponentUid::Forge,
        ),
        (
            NEOFORGE,
            "1.21.1",
            LoaderKind::Neoforge,
            "NeoForge 1.21.1",
            ComponentUid::Neoforge,
        ),
    ];
    for (name, mc, kind, generated, uid) in kinds {
        let seen = create(&app, cx, mc, Some(kind), generated, name);
        assert!(!seen.is_empty(), "the header showed the install of {name}");
        let loader = app
            .store
            .load(name)
            .unwrap()
            .config
            .loader()
            .map(|(_, c)| c.clone());
        assert_eq!(loader.map(|c| c.uid), Some(uid));
    }
}

// ---- 3. versions tab -----------------------------------------------------

/// In the open version dialog, chooses Minecraft `mc` and confirms.
fn choose_mc(app: &Booted, cx: &mut TestAppContext, mc: &str) {
    assert!(shows(app, cx, "dialog"));
    wait_for(cx, Duration::from_secs(120), "version list", |cx| {
        shows(app, cx, "version-search")
    });
    click(app, cx, "version-search");
    type_text(app, cx, mc);
    wait_for(cx, Duration::from_secs(120), "version row", |cx| {
        shows(app, cx, version_row_id(mc))
    });
    click(app, cx, version_row_id(mc));
    click(app, cx, "versions-dialog-confirm");
}

#[gpui_kit::test]
#[ignore = "real network installs"]
fn e2e_03_versions_tab(cx: &mut TestAppContext) {
    let app = boot(cx);
    let m = loaded(&app, cx);
    select(&app, cx, FABRIC);
    click(&app, cx, "tab-versions");
    assert!(shows(&app, cx, "versions-row-minecraft"));

    // Fabric → NeoForge, after a confirmation.
    click(&app, cx, "versions-row-loader");
    click(&app, cx, "versions-switch-loader");
    assert!(shows(&app, cx, "dialog"));
    wait_for(cx, Duration::from_secs(120), "neoforge versions", |cx| {
        !disabled(&app, cx, loader_segment_id(Some(LoaderKind::Neoforge)))
    });
    click(&app, cx, loader_segment_id(Some(LoaderKind::Neoforge)));
    wait_for(cx, Duration::from_secs(120), "a neoforge version", |cx| {
        !disabled(&app, cx, "versions-dialog-confirm")
    });
    click(&app, cx, "versions-dialog-confirm");
    assert!(shows(&app, cx, "dialog"));
    eprintln!(
        "switch confirmation: {:?}",
        label_of(&app, cx, "versions-confirm-title")
    );
    assert!(
        op_of(&m, cx, FABRIC).is_none(),
        "nothing runs before the answer"
    );
    dialog_click(&app, cx, "ok");
    wait_op(&app, cx, FABRIC, "switch to NeoForge");
    let loader = app
        .store
        .load(FABRIC)
        .unwrap()
        .config
        .loader()
        .map(|(_, c)| c.clone());
    eprintln!("after switch: {loader:?}");
    assert_eq!(loader.map(|c| c.uid), Some(ComponentUid::Neoforge));

    // 修复.
    click(&app, cx, "versions-row-loader");
    settle(cx, |cx| !disabled(&app, cx, "versions-repair"));
    click(&app, cx, "versions-repair");
    wait_op(&app, cx, FABRIC, "repair");

    // 更改游戏版本 to one this NeoForge does not support: Incompatible.
    click(&app, cx, "versions-row-minecraft");
    settle(cx, |cx| !disabled(&app, cx, "versions-change-mc"));
    click(&app, cx, "versions-change-mc");
    choose_mc(&app, cx, "1.21.1");
    wait_for(cx, SLOW, "incompatible", |cx| {
        if let Some(OpState::Failed { message, .. }) = op_of(&m, cx, FABRIC) {
            panic!("change version failed: {message}");
        }
        shows(&app, cx, "incompatible-title")
    });
    eprintln!(
        "incompatible: {:?}; replace offered: {}",
        label_of(&app, cx, "incompatible-title"),
        shows(&app, cx, "incompatible-replace")
    );
    click(&app, cx, "incompatible-cancel");
    settle(cx, |cx| !shows(&app, cx, "dialog"));
    settle(cx, |cx| op_of(&m, cx, FABRIC).is_none());
    assert_eq!(
        app.store.load(FABRIC).unwrap().config.mc_version(),
        "26.3",
        "cancel changes nothing"
    );

    // 移除加载器, after a confirmation.
    click(&app, cx, "versions-row-loader");
    settle(cx, |cx| !disabled(&app, cx, "versions-remove-loader"));
    click(&app, cx, "versions-remove-loader");
    eprintln!(
        "remove confirmation: {:?}",
        label_of(&app, cx, "versions-confirm-title")
    );
    dialog_click(&app, cx, "ok");
    wait_op(&app, cx, FABRIC, "remove loader");
    assert!(app.store.load(FABRIC).unwrap().config.loader().is_none());

    // A vanilla instance changes its game version without a question.
    select(&app, cx, VANILLA);
    click(&app, cx, "tab-versions");
    click(&app, cx, "versions-row-minecraft");
    settle(cx, |cx| !disabled(&app, cx, "versions-change-mc"));
    click(&app, cx, "versions-change-mc");
    choose_mc(&app, cx, "1.20.1");
    wait_op(&app, cx, VANILLA, "change version");
    assert_eq!(
        app.store.load(VANILLA).unwrap().config.mc_version(),
        "1.20.1"
    );
}

// ---- 4. launch, live log, force stop -------------------------------------

#[gpui_kit::test]
#[ignore = "starts real Minecraft"]
fn e2e_04_launch_log_and_force_stop(cx: &mut TestAppContext) {
    let app = boot(cx);
    let m = loaded(&app, cx);
    select(&app, cx, FORGE_1201);
    let (pid, seen) = launch_selected(&app, cx, FORGE_1201);
    assert!(!seen.is_empty(), "the header showed the launch steps");
    assert!(disabled(&app, cx, "launch"));
    assert!(shows(&app, cx, "instance-stop"));

    // The log tab follows the game as it writes.
    click(&app, cx, "tab-log");
    assert!(shows(&app, cx, "log-lines"));
    let mut sizes = Vec::new();
    wait_for(cx, Duration::from_secs(240), "game log", |cx| {
        let text = log_text(&app, cx, FORGE_1201);
        if sizes.last() != Some(&text.len()) {
            sizes.push(text.len());
        }
        text.contains("Setting user: Dev")
    });
    eprintln!(
        "log grew through {} sizes: {:?}",
        sizes.len(),
        &sizes[..sizes.len().min(12)]
    );
    assert!(sizes.len() >= 2, "the log grew while shown");
    // Let the game get to its window.
    idle(cx, Duration::from_secs(20));

    // Force stop: asks first; 取消 keeps it running.
    click(&app, cx, "instance-stop");
    assert!(shows(&app, cx, "dialog"));
    dialog_click(&app, cx, "cancel");
    idle(cx, Duration::from_secs(1));
    assert!(matches!(
        run_state(&m, cx, FORGE_1201),
        Some(RunState::Running { .. })
    ));
    force_stop(&app, cx, FORGE_1201);
    assert!(
        run_state(&m, cx, FORGE_1201).is_none(),
        "a stop is no crash"
    );
    assert!(m.read_with(cx, |m, cx| m.crash_log(FORGE_1201, cx).is_none()));
    assert!(!shows(&app, cx, "instance-crash"));
    settle(cx, |_| gone(pid));
}

// ---- 5. reattach, and patina-dev against the same instance ---------------

fn pid_file() -> PathBuf {
    std::env::temp_dir().join("patina-e2e-05.pid")
}

#[gpui_kit::test]
#[ignore = "starts real Minecraft and leaves it running for e2e_05b"]
fn e2e_05a_launch_then_quit_the_app(cx: &mut TestAppContext) {
    let app = boot(cx);
    loaded(&app, cx);
    select(&app, cx, FORGE_1201);
    let (pid, _) = launch_selected(&app, cx, FORGE_1201);
    std::fs::write(pid_file(), pid.to_string()).unwrap();
    wait_for(cx, Duration::from_secs(240), "game log", |cx| {
        log_text(&app, cx, FORGE_1201).contains("Setting user: Dev")
    });

    // patina-dev meanwhile: the game is this app's; dev must refuse.
    let (ok, out) = dev(&["status"]);
    assert!(ok, "{out}");
    let (ok, out) = dev(&["launch", FORGE_1201]);
    assert!(!ok, "dev launched a running instance: {out}");
    // The app ends here without stopping the game (its session is dropped).
}

#[gpui_kit::test]
#[ignore = "needs the game e2e_05a left running"]
fn e2e_05b_reattach_shows_running_and_stops(cx: &mut TestAppContext) {
    let pid: u32 = std::fs::read_to_string(pid_file())
        .expect("run e2e_05a first")
        .trim()
        .parse()
        .unwrap();
    assert!(!gone(pid), "the game of e2e_05a still runs");
    let app = boot(cx);
    let m = loaded(&app, cx);
    select(&app, cx, FORGE_1201);
    wait_for(cx, Duration::from_secs(30), "reattach", |cx| {
        matches!(
            run_state(&m, cx, FORGE_1201),
            Some(RunState::Running {
                reattached: true,
                ..
            })
        )
    });
    let Some(RunState::Running { pid: found, .. }) = run_state(&m, cx, FORGE_1201) else {
        unreachable!()
    };
    assert_eq!(found, pid);
    assert!(disabled(&app, cx, "launch"));

    // The log from its first line, not from where this app came in.
    click(&app, cx, "tab-log");
    wait_for(cx, Duration::from_secs(30), "reattached log", |cx| {
        log_text(&app, cx, FORGE_1201).contains("Setting user: Dev")
    });
    let text = log_text(&app, cx, FORGE_1201);
    let file = std::fs::read_to_string(
        app.data
            .instances()
            .join(FORGE_1201)
            .join("patina-logs/current.log"),
    )
    .unwrap();
    let first = file.lines().next().unwrap_or_default().trim_end();
    eprintln!(
        "first log line: {first:?}; shown starts with: {:?}",
        text.lines().next()
    );
    assert!(text.contains(first), "the first line is shown");

    force_stop(&app, cx, FORGE_1201);
    assert!(run_state(&m, cx, FORGE_1201).is_none());
    settle(cx, |_| gone(pid));
    std::fs::remove_file(pid_file()).ok();
}

#[gpui_kit::test]
#[ignore = "real installs and Minecraft; patina-dev in parallel"]
fn e2e_05c_dev_and_app_on_one_instance(cx: &mut TestAppContext) {
    // The app reinstalls E2E Forge's loader; dev, meanwhile, is told the
    // instance is busy.
    let app = boot(cx);
    let m = loaded(&app, cx);
    select(&app, cx, FORGE);
    click(&app, cx, "tab-versions");
    click(&app, cx, "versions-row-loader");
    settle(cx, |cx| !disabled(&app, cx, "versions-repair"));
    click(&app, cx, "versions-repair");
    settle(cx, |cx| {
        matches!(op_of(&m, cx, FORGE), Some(OpState::Running { .. }))
    });
    let (ok, out) = dev(&["launch", FORGE]);
    assert!(!ok, "dev launched while the app installs: {out}");
    wait_op(&app, cx, FORGE, "repair Forge");

    // The other way round: dev reinstalls E2E NeoForge's loader, and the
    // app's launch is refused as busy.
    let mut dev_op = Command::new(patina_dev())
        .args(["instance", "reinstall-loader", NEOFORGE])
        .env("PATINA_DATA_DIR", data_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    idle(cx, Duration::from_secs(5));
    assert!(
        dev_op.try_wait().unwrap().is_none(),
        "dev's reinstall still runs"
    );
    select(&app, cx, NEOFORGE);
    click(&app, cx, "launch");
    let mut said = None;
    wait_for(cx, Duration::from_secs(60), "busy", |cx| {
        if let Some(OpState::Failed { message, .. }) = op_of(&m, cx, NEOFORGE) {
            said = Some(message);
        }
        said.is_some()
    });
    let said = said.unwrap();
    eprintln!(
        "app while dev reinstalls: {said:?}; header: {:?}",
        label_of(&app, cx, "instance-op-error")
    );
    assert_eq!(said, "“E2E NeoForge” is busy with another operation");
    assert!(shows(&app, cx, "instance-op-retry"));
    let status = dev_op.wait().unwrap();
    eprintln!("dev reinstall ended: {status}");
    assert!(status.success());
    click(&app, cx, "instance-op-dismiss");

    // Dev launches a game: the app shows it held elsewhere and can stop it.
    let mut dev_launch = Command::new(patina_dev())
        .args(["launch", VANILLA])
        .env("PATINA_DATA_DIR", data_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    select(&app, cx, VANILLA);
    wait_for(cx, Duration::from_secs(300), "held elsewhere", |cx| {
        if matches!(
            run_state(&m, cx, VANILLA),
            Some(RunState::HeldElsewhere { .. })
        ) {
            return true;
        }
        assert!(
            dev_launch.try_wait().unwrap().is_none(),
            "dev's launch ended early"
        );
        m.update(cx, |m, cx| m.refresh(cx));
        false
    });
    assert!(shows(&app, cx, "instance-elsewhere"));
    eprintln!("elsewhere: {:?}", label_of(&app, cx, "instance-elsewhere"));
    assert!(disabled(&app, cx, "launch"));
    idle(cx, Duration::from_secs(15));
    force_stop(&app, cx, VANILLA);
    let start = Instant::now();
    while dev_launch.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let status = dev_launch.try_wait().unwrap();
    eprintln!("dev launch ended: {status:?}");
    assert!(status.is_some(), "dev's launch ended with its game");
}

// ---- 6. crash capture ----------------------------------------------------

#[gpui_kit::test]
#[ignore = "starts real Minecraft"]
fn e2e_06_crash_saves_the_log(cx: &mut TestAppContext) {
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).ok();
        }
    }
    let app = boot(cx);
    let m = loaded(&app, cx);
    let mods = app.data.instances().join(FORGE_1201).join("minecraft/mods");
    let bad = mods.join("bad.jar");
    std::fs::create_dir_all(&mods).unwrap();
    std::fs::write(&bad, b"this is not a jar").unwrap();
    let _remove = Remove(bad);

    select(&app, cx, FORGE_1201);
    click(&app, cx, "launch");
    wait_for(cx, SLOW, "crash", |cx| {
        if shows(&app, cx, "dialog") && element(&app, cx, "prelaunch-title").is_some() {
            dialog_click(&app, cx, "ok");
        }
        if let Some(OpState::Failed { message, .. }) = op_of(&m, cx, FORGE_1201) {
            panic!("launch failed before the game: {message}");
        }
        matches!(run_state(&m, cx, FORGE_1201), Some(RunState::Exited(_)))
    });
    let Some(RunState::Exited(exit)) = run_state(&m, cx, FORGE_1201) else {
        unreachable!()
    };
    eprintln!("exit: {exit:?}");
    assert!(exit.crashed);
    let saved = exit.saved_log.clone().expect("the log is saved");
    assert!(saved.is_file());
    assert_eq!(
        label_of(&app, cx, "instance-crash-text").as_deref(),
        Some("The game quit unexpectedly. Its log was saved.")
    );
    assert!(shows(&app, cx, "instance-crash-open"));
    assert!(!disabled(&app, cx, "instance-crash-open"));
    click(&app, cx, "tab-log");
    assert!(shows(&app, cx, "log-crash-banner"));
    assert!(shows(&app, cx, "log-crash-open"));
    click(&app, cx, "instance-crash-dismiss");
    assert!(!shows(&app, cx, "instance-crash"));
}

// ---- 7. instance settings ------------------------------------------------

#[gpui_kit::test]
#[ignore = "starts real Minecraft"]
fn e2e_07_settings(cx: &mut TestAppContext) {
    let app = boot(cx);
    let m = loaded(&app, cx);
    activate(&app, cx);
    select(&app, cx, VANILLA);
    sclick(&app, cx, "tab-settings");
    assert!(sshows(&app, cx, "settings-tab"));
    let java = |app: &Booted| app.store.load(VANILLA).unwrap().config.java_override;

    // Memory and JVM arguments: an override, checked before it is saved.
    assert!(java(&app).is_none(), "starts on the global settings");
    sclick(&app, cx, "settings-use-global");
    settle(cx, |_| java(&app).is_some());
    sreplace(&app, cx, "settings-max-mem", "3072");
    sreplace(&app, cx, "settings-min-mem", "4096");
    blur(&app, cx);
    assert!(sshows(&app, cx, "settings-memory-error"));
    sreplace(&app, cx, "settings-min-mem", "1024");
    blur(&app, cx);
    settle(cx, |_| {
        java(&app).is_some_and(|j| (j.min_mem_mib, j.max_mem_mib) == (1024, Some(3072)))
    });
    sclick(&app, cx, "settings-args");
    type_text(&app, cx, "-Xmx8G");
    assert!(sshows(&app, cx, "settings-args-error"));
    eprintln!(
        "args error: {:?}",
        label_of(&app, cx, "settings-args-error")
    );
    sreplace(&app, cx, "settings-args", "-Dpatina.e2e=1");
    assert!(!sshows(&app, cx, "settings-args-error"));
    blur(&app, cx);
    settle(cx, |_| {
        java(&app).is_some_and(|j| j.extra_jvm_args == "-Dpatina.e2e=1")
    });

    // Window size 1280×720.
    click_radio(&app, cx, "settings-window-mode", 1);
    settle(cx, |_| {
        app.store.load(VANILLA).unwrap().config.window.is_some()
    });
    cx.run_until_parked();
    settle(cx, |cx| sshows(&app, cx, "settings-width"));
    sreplace(&app, cx, "settings-width", "1280");
    sreplace(&app, cx, "settings-height", "720");
    blur(&app, cx);
    settle(cx, |_| {
        app.store.load(VANILLA).unwrap().config.window
            == Some(WindowSize {
                width: 1280,
                height: 720,
            })
    });

    // Icons: a built-in one, then an upload through the file dialog.
    if !sshows(&app, cx, pick_id("sword")) {
        sclick(&app, cx, "settings-icon-toggle");
    }
    sclick(&app, cx, pick_id("sword"));
    settle(cx, |_| {
        app.store.load(VANILLA).unwrap().config.icon == Some(InstanceIcon::Builtin("sword".into()))
    });
    let png = std::env::temp_dir().join("patina-e2e-upload.png");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/instance-icons/axe.png"),
        &png,
    )
    .unwrap();
    if !sshows(&app, cx, "icon-upload") {
        sclick(&app, cx, "settings-icon-toggle");
    }
    sclick(&app, cx, "icon-upload");
    assert!(cx.did_prompt_for_paths(), "upload opens the file dialog");
    let chosen = png.clone();
    cx.simulate_path_prompt_response(move |_| Some(vec![chosen]));
    settle(cx, |_| {
        app.store.load(VANILLA).unwrap().config.icon == Some(InstanceIcon::Custom)
    });
    std::fs::remove_file(&png).ok();

    // Rename.
    sreplace(&app, cx, "settings-name", "E2E Vanilla renamed");
    press(&app, cx, "enter");
    settle(cx, |_| {
        app.store.load(VANILLA).unwrap().config.name == "E2E Vanilla renamed"
    });

    // The game starts with that window size and those JVM settings.
    let (pid, _) = launch_selected(&app, cx, VANILLA);
    let mut args = Vec::new();
    wait_for(cx, Duration::from_secs(60), "java command line", |_| {
        args = cmdline(pid);
        args.iter().any(|a| a == "--width")
    });
    let after = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    };
    eprintln!(
        "game args: --width {:?} --height {:?} {:?}",
        after("--width"),
        after("--height"),
        args.iter()
            .filter(|a| a.starts_with("-Xm") || a.starts_with("-Dpatina"))
            .collect::<Vec<_>>()
    );
    assert_eq!(after("--width").as_deref(), Some("1280"));
    assert_eq!(after("--height").as_deref(), Some("720"));
    assert!(args.iter().any(|a| a.eq_ignore_ascii_case("-Xmx3072M")));
    assert!(args.iter().any(|a| a.eq_ignore_ascii_case("-Xms1024M")));
    assert!(args.iter().any(|a| a == "-Dpatina.e2e=1"));
    // Running: delete is unavailable.
    assert!(sshows(&app, cx, "settings-delete"));
    assert!(disabled(&app, cx, "settings-delete"));
    idle(cx, Duration::from_secs(25));
    force_stop(&app, cx, VANILLA);
    settle(cx, |_| gone(pid));

    // NeoForge: the early-window switch writes fml.toml.
    select(&app, cx, NEOFORGE);
    settle(cx, |cx| sshows(&app, cx, "settings-neoforge"));
    settle(cx, |cx| !disabled(&app, cx, "settings-early-window"));
    let fml = app
        .data
        .instances()
        .join(NEOFORGE)
        .join("minecraft/config/fml.toml");
    let before = std::fs::read_to_string(&fml).ok();
    sclick(&app, cx, "settings-early-window");
    settle(cx, |_| std::fs::read_to_string(&fml).ok() != before);
    eprintln!(
        "fml.toml earlyWindowControl: {:?}",
        std::fs::read_to_string(&fml)
            .unwrap()
            .lines()
            .find(|l| l.contains("earlyWindowControl"))
            .map(str::to_owned)
    );

    // Delete a test instance from here, after a confirmation.
    select(&app, cx, FORGE);
    sclick(&app, cx, "settings-delete");
    assert!(shows(&app, cx, "dialog"));
    eprintln!(
        "delete: {}",
        dialog_label(&app, cx, "settings-delete-title")
    );
    dialog_click(&app, cx, "ok");
    settle(cx, |cx| {
        m.read_with(cx, |m, _| m.instances().iter().all(|i| i.id != FORGE))
    });
    assert!(app.store.load(FORGE).is_err());
    assert!(!app.data.instances().join(FORGE).exists());
}

// ---- 8. Mods tab: manually placed jars, enable/disable, folders ---------

/// The instance the Mods and search checks fill with hand-made jars.
const MODS_HOME: &str = NEOFORGE;
/// The hand-made jars: (file, name, version) — one per metadata format,
/// three with an icon, one with no metadata at all.
const JARS: [(&str, &str, Option<&str>); 4] = [
    ("e2e-fabric.jar", "E2E Fabric Mod", Some("1.2.3")),
    ("e2e-neo.jar", "E2E Neo Mod", Some("2.0.0")),
    ("e2e-legacy.jar", "E2E Legacy Mod", Some("0.9")),
    ("e2e-plain.jar", "e2e-plain", None),
];

/// Writes the jar `file` with `entries` into `dir`.
fn jar(dir: &Path, file: &str, entries: &[(&str, &[u8])]) {
    use std::io::Write as _;
    let mut w = zip::ZipWriter::new(std::fs::File::create(dir.join(file)).unwrap());
    for (name, bytes) in entries {
        w.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap();
}

/// Puts the [`JARS`] into `mods` by hand (as a player would, outside the
/// downloads page), with the bundled icons as their logos.
fn place_jars(mods: &Path) {
    let icon = |name: &str| {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("assets/instance-icons/{name}.png")),
        )
        .unwrap()
    };
    std::fs::create_dir_all(mods).unwrap();
    let fabric = br#"{"schemaVersion":1,"id":"e2e_fabric","version":"1.2.3","name":"E2E Fabric Mod","authors":["Patina"],"icon":"assets/e2e/icon.png"}"#;
    jar(
        mods,
        JARS[0].0,
        &[
            ("fabric.mod.json", fabric),
            ("assets/e2e/icon.png", &icon("axe")),
        ],
    );
    let neo = br#"modLoader = "javafml"
loaderVersion = "[1,)"
license = "MIT"
[[mods]]
modId = "e2eneo"
version = "2.0.0"
displayName = "E2E Neo Mod"
authors = "Patina"
logoFile = "logo.png"
"#;
    jar(
        mods,
        JARS[1].0,
        &[
            ("META-INF/neoforge.mods.toml", neo),
            ("logo.png", &icon("sword")),
        ],
    );
    let legacy = br#"[{"modid":"e2elegacy","name":"E2E Legacy Mod","version":"0.9","authorList":["Patina"],"logoFile":"logo.png"}]"#;
    jar(
        mods,
        JARS[2].0,
        &[("mcmod.info", legacy), ("logo.png", &icon("pickaxe"))],
    );
    jar(mods, JARS[3].0, &[("readme.txt", b"no metadata here")]);
}

/// Removes the hand-made jars (enabled or not) when dropped.
struct RemoveJars(PathBuf);
impl Drop for RemoveJars {
    fn drop(&mut self) {
        for (file, _, _) in JARS {
            std::fs::remove_file(self.0.join(file)).ok();
            std::fs::remove_file(self.0.join(format!("{file}.disabled"))).ok();
        }
    }
}

fn mods_tab(app: &Booted, cx: &mut TestAppContext) -> Entity<ModsTab> {
    cx.update(|cx| {
        let detail = app.root.read(cx).instances().read(cx).detail().clone();
        detail
            .read(cx)
            .mods_view()
            .cloned()
            .expect("the Mods tab was shown")
    })
}

fn mods_settled(tab: &Entity<ModsTab>, cx: &mut TestAppContext) {
    settle(cx, |cx| {
        tab.read_with(cx, |t, cx| {
            let list = t.list().read(cx);
            !list.is_reloading() && *list.state() == LoadState::Loaded
        })
    });
}

fn mod_items(tab: &Entity<ModsTab>, cx: &mut TestAppContext) -> Vec<ModItem> {
    tab.read_with(cx, |t, cx| t.list().read(cx).items().to_vec())
}

fn nav(app: &Booted, cx: &mut TestAppContext) -> Nav {
    cx.update(|cx| app.root.read(cx).shell().read(cx).nav())
}

#[gpui_kit::test]
#[ignore = "real data dir"]
fn e2e_08_mods_tab_and_folders(cx: &mut TestAppContext) {
    let app = boot(cx);
    loaded(&app, cx);
    let mods = app.data.instances().join(MODS_HOME).join("minecraft/mods");
    let _remove = RemoveJars(mods.clone());
    place_jars(&mods);

    select(&app, cx, MODS_HOME);
    click(&app, cx, "tab-mods");
    let tab = mods_tab(&app, cx);
    mods_settled(&tab, cx);

    // Name, version and an icon of each, from its own metadata format.
    let items = mod_items(&tab, cx);
    for (file, name, version) in JARS {
        let item = items
            .iter()
            .find(|i| i.file_name == file)
            .unwrap_or_else(|| panic!("{file} listed"));
        eprintln!(
            "row {file}: name {:?} detail {:?} format {:?} icon {:?} enabled {}",
            item.name(),
            item.detail(),
            item.info.as_ref().map(|i| i.format),
            item.info.as_ref().map(|i| i.has_icon),
            item.enabled
        );
        assert_eq!(item.name(), name);
        assert_eq!(
            item.info.as_ref().and_then(|i| i.version.as_deref()),
            version
        );
        assert!(shows(&app, cx, mod_row_id(file)), "{file} drawn");
        let has_icon = version.is_some();
        assert_eq!(item.info.as_ref().is_some_and(|i| i.has_icon), has_icon);
        if has_icon {
            // The row on show reads its icon into the cache.
            let modified = (item.modified_nanos > 0)
                .then(|| std::time::UNIX_EPOCH + Duration::from_nanos(item.modified_nanos));
            let key = ImageKey::new(MODS_HOME, file, item.size, modified);
            settle(cx, |cx| {
                cx.update(|cx| cx.global_mut::<ImageCache>().get(&key).is_some())
            });
        }
    }

    // Disable and enable again with the row's switch.
    let file = JARS[0].0;
    let off = format!("{file}.disabled");
    click(&app, cx, mod_switch_id(file));
    settle(cx, |cx| {
        mod_items(&tab, cx).iter().any(|i| i.file_name == off)
    });
    assert!(mods.join(&off).is_file() && !mods.join(file).exists());
    eprintln!(
        "disabled: {:?} on disk, row says {:?}",
        off,
        label_of(&app, cx, mod_disabled_id(&off))
    );
    assert_eq!(
        label_of(&app, cx, mod_disabled_id(&off)).as_deref(),
        Some("Disabled")
    );
    click(&app, cx, mod_switch_id(&off));
    settle(cx, |cx| {
        mod_items(&tab, cx).iter().any(|i| i.file_name == file)
    });
    assert!(mods.join(file).is_file() && !mods.join(&off).exists());

    // A jar added outside the app shows after 刷新.
    jar(&mods, "e2e-late.jar", &[("readme.txt", b"late")]);
    click(&app, cx, "content-refresh");
    mods_settled(&tab, cx);
    assert!(
        mod_items(&tab, cx)
            .iter()
            .any(|i| i.file_name == "e2e-late.jar")
    );
    std::fs::remove_file(mods.join("e2e-late.jar")).unwrap();

    // Every folder button is there and can be used (the headless platform
    // cannot open a file manager: the real click is the user's).
    for tab_id in [
        "tab-mods",
        "tab-resource-packs",
        "tab-shaders",
        "tab-worlds",
    ] {
        click(&app, cx, tab_id);
        settle(cx, |cx| shows(&app, cx, "content-open-folder"));
        assert!(!disabled(&app, cx, "content-open-folder"), "{tab_id}");
        eprintln!(
            "{tab_id}: open folder {:?}",
            label_of(&app, cx, "content-open-folder")
        );
    }
    click(&app, cx, "tab-overview");
    settle(cx, |cx| shows(&app, cx, "open-instance-folder"));
    assert!(!disabled(&app, cx, "open-instance-folder"));

    // The data folder on the settings page opens through its opener.
    click(&app, cx, "rail-settings");
    let page = cx.update(|cx| app.root.read(cx).settings().clone());
    settle(cx, |cx| page.read_with(cx, |p, _| p.config().is_some()));
    let opened = std::rc::Rc::new(std::cell::RefCell::new(None));
    page.update(cx, |p, _| {
        let opened = opened.clone();
        p.set_opener(move |dir, _| *opened.borrow_mut() = Some(dir.to_path_buf()));
    });
    preveal(&app, cx, "prefs-data-open");
    click(&app, cx, "prefs-data-open");
    settle(cx, |_| opened.borrow().is_some());
    eprintln!("data folder opened: {:?}", opened.borrow());
    assert_eq!(opened.borrow().as_deref(), Some(data_dir().as_path()));

    // 在线获取 goes to the downloads page with this instance as target.
    click(&app, cx, "rail-instances");
    click(&app, cx, "tab-mods");
    click(&app, cx, "mods-online");
    assert_eq!(nav(&app, cx), Nav::Downloads);
    let downloads = cx.update(|cx| app.root.read(cx).downloads().read(cx).model().clone());
    let target = downloads.read_with(cx, |m, _| m.target().map(str::to_owned));
    eprintln!("downloads target: {target:?}");
    assert_eq!(target.as_deref(), Some(MODS_HOME));
}

// ---- 9. global search: the three groups jump --------------------------

fn search_model(app: &Booted, cx: &mut TestAppContext) -> Entity<SearchModel> {
    cx.update(|cx| {
        let view = app.root.read(cx).title_bar().read(cx).search().clone();
        view.read(cx).model().clone()
    })
}

/// Opens the search with `/`, types `text` and waits for its results.
fn search_for(app: &Booted, cx: &mut TestAppContext, text: &str) -> SearchResults {
    press(app, cx, "/");
    type_text(app, cx, text);
    let model = search_model(app, cx);
    wait_for(cx, Duration::from_secs(30), "search results", |cx| {
        model.read_with(cx, |m, _| {
            m.results()
                .is_some_and(|r| r.query == text && !matches!(r.mods, ModGroup::Indexing))
        })
    });
    model.read_with(cx, |m, _| m.results().cloned().unwrap())
}

#[gpui_kit::test]
#[ignore = "real data dir"]
fn e2e_09_global_search_jumps(cx: &mut TestAppContext) {
    let app = boot(cx);
    let m = loaded(&app, cx);
    let mods = app.data.instances().join(MODS_HOME).join("minecraft/mods");
    let _remove = RemoveJars(mods.clone());
    place_jars(&mods);
    let model = search_model(&app, cx);
    wait_for(cx, Duration::from_secs(60), "index", |cx| {
        model.read_with(cx, |m, _| m.is_indexed() && !m.is_refreshing())
    });
    // The jars were placed by hand: the index learns of them when asked.
    let refreshes = model.read_with(cx, |m, _| m.refreshes());
    model.update(cx, |m, cx| m.request_refresh(cx));
    wait_for(cx, Duration::from_secs(60), "index refresh", |cx| {
        // A throttled refresh can be scheduled but not yet in flight.
        model.read_with(cx, |m, _| m.refreshes() > refreshes && !m.is_refreshing())
    });
    select(&app, cx, FORGE_1201);
    click(&app, cx, "rail-accounts");

    // Instances: "E2E" finds the test instances.
    let found = search_for(&app, cx, "E2E");
    let names: Vec<_> = found.instances.iter().map(|i| i.name.clone()).collect();
    let hits: Vec<_> = found
        .mod_hits()
        .iter()
        .map(|h| format!("{} — {}", h.hit.name, h.instance_name))
        .collect();
    eprintln!("\"E2E\": instances {names:?}; mods {hits:?}");
    for group in [
        "search-group-instances",
        "search-group-mods",
        "search-group-online",
    ] {
        assert!(shows(&app, cx, group), "{group}");
    }
    assert!(names.iter().any(|n| n == MODS_HOME));
    click(
        &app,
        cx,
        search_row_id(&SearchItem::Instance(MODS_HOME.into())),
    );
    assert_eq!(nav(&app, cx), Nav::Instances);
    assert_eq!(selected(&m, cx).as_deref(), Some(MODS_HOME));
    assert!(!shows(&app, cx, "search-popup"));

    // Mods: the hit opens its instance's Mods tab on that row.
    select(&app, cx, FORGE_1201);
    click(&app, cx, "rail-settings");
    let found = search_for(&app, cx, "E2E Legacy");
    let hit = found.mod_hits().first().expect("the legacy mod is found");
    eprintln!(
        "\"E2E Legacy\": {} {:?} in {}",
        hit.hit.name, hit.hit.version, hit.instance_name
    );
    let item = SearchItem::Mod {
        instance_id: MODS_HOME.into(),
        file_name: JARS[2].0.into(),
    };
    click(&app, cx, search_row_id(&item));
    assert_eq!(nav(&app, cx), Nav::Instances);
    assert_eq!(selected(&m, cx).as_deref(), Some(MODS_HOME));
    let detail = cx.update(|cx| app.root.read(cx).instances().read(cx).detail().clone());
    assert_eq!(detail.read_with(cx, |d, _| d.tab()), DetailTab::Mods);
    let tab = mods_tab(&app, cx);
    settle(cx, |cx| {
        tab.read_with(cx, |t, _| t.highlighted() == Some(JARS[2].0))
    });
    assert!(shows(&app, cx, mod_row_id(JARS[2].0)));

    // Online: the downloads page, with the words.
    let found = search_for(&app, cx, "sodium");
    assert!(found.items().contains(&SearchItem::Online));
    click(&app, cx, search_row_id(&SearchItem::Online));
    assert_eq!(nav(&app, cx), Nav::Downloads);
    let downloads = cx.update(|cx| app.root.read(cx).downloads().read(cx).model().clone());
    let text = downloads.read_with(cx, |m, _| m.text().to_owned());
    eprintln!("downloads words: {text:?}");
    assert_eq!(text, "sodium");
}

// ---- 10. global settings, scale, language -------------------------------

/// Scrolls the settings page until `id` (if drawn) is in view.
fn preveal(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) {
    let id = id.into();
    preveal_found(app, cx, |app, cx| element(app, cx, id.clone()));
}

fn preveal_found(
    app: &Booted,
    cx: &mut TestAppContext,
    find: impl Fn(&Booted, &mut TestAppContext) -> Option<ElementSnapshot>,
) {
    for _ in 0..60 {
        let Some(found) = find(app, cx) else {
            return;
        };
        if found.visible() {
            return;
        }
        let column = element(app, cx, "prefs-content").expect("the page is drawn");
        let up = found.bounds().center().y < column.bounds().top();
        let dy = gpui_kit::px(if up { 120. } else { -120. });
        cx.update_window(app.handle, |_, window, cx| {
            window.scroll(
                "prefs-content",
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(gpui_kit::px(0.), dy)),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
    }
}

fn pradio(app: &Booted, cx: &mut TestAppContext, group: &'static str, ix: usize) {
    // The group's top can be visible while the requested radio is clipped.
    preveal_found(app, cx, |app, cx| {
        cx.update_window(app.handle, |_, window, cx| {
            window.render_frame(cx);
            window.within(group).try_find(ix)
        })
        .unwrap()
    });
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within(group).click(ix, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn on_disk_java(app: &Booted) -> patina_core::config::JavaSettings {
    patina_core::config::GlobalConfig::load(&app.data.config_file())
        .unwrap()
        .java
}

fn settings_saved(app: &Booted, cx: &mut TestAppContext) {
    let page = cx.update(|cx| app.root.read(cx).settings().clone());
    wait_for(cx, Duration::from_secs(10), "saves", |cx| {
        page.read_with(cx, |p, _| !p.is_saving())
    });
}

#[gpui_kit::test]
#[ignore = "real data dir"]
fn e2e_10_settings_scale_and_language(cx: &mut TestAppContext) {
    let gui = data_dir().join("gui.toml");
    let gui_before = std::fs::read(&gui).ok();
    let config_before = std::fs::read(data_dir().join("config.toml")).unwrap();
    let app = boot(cx);
    loaded(&app, cx);
    select(&app, cx, FORGE_1201);
    activate(&app, cx);
    click(&app, cx, "rail-settings");
    let page = cx.update(|cx| app.root.read(cx).settings().clone());
    settle(cx, |cx| page.read_with(cx, |p, _| p.config().is_some()));

    // A default changes on disk, and back.
    let min_before = on_disk_java(&app).min_mem_mib;
    preveal(&app, cx, "prefs-min-mem");
    click(&app, cx, "prefs-min-mem");
    press(&app, cx, "secondary-a");
    type_text(&app, cx, "768");
    press(&app, cx, "enter");
    settings_saved(&app, cx);
    wait_for(cx, Duration::from_secs(10), "min memory saved", |_| {
        on_disk_java(&app).min_mem_mib == 768
    });
    eprintln!("default min memory {min_before} -> 768 on disk");
    preveal(&app, cx, "prefs-min-mem");
    click(&app, cx, "prefs-min-mem");
    press(&app, cx, "secondary-a");
    type_text(&app, cx, &min_before.to_string());
    press(&app, cx, "enter");
    settings_saved(&app, cx);
    wait_for(cx, Duration::from_secs(10), "min memory back", |_| {
        on_disk_java(&app).min_mem_mib == min_before
    });

    // 启动后: minimize, then back to keep.
    pradio(&app, cx, "prefs-after-launch-choice", 1);
    assert_eq!(
        cx.update(|cx| SettingsStore::get(cx).after_launch),
        AfterLaunch::Minimize
    );
    pradio(&app, cx, "prefs-after-launch-choice", 0);
    assert_eq!(
        cx.update(|cx| SettingsStore::get(cx).after_launch),
        AfterLaunch::Keep
    );

    // Scale: 125 % zooms the interface and is kept.
    let base = cx.update(|cx| gpui_kit::component::Theme::global(cx).font_size);
    preveal(&app, cx, scale_step_id(1.25));
    click(&app, cx, scale_step_id(1.25));
    assert_eq!(cx.update(|cx| SettingsStore::get(cx).ui_scale), 1.25);
    let zoomed = cx.update(|cx| gpui_kit::component::Theme::global(cx).font_size);
    eprintln!("font size {base:?} -> {zoomed:?} at 125 %");
    assert_eq!(zoomed, base * 1.25);
    cx.update(|cx| SettingsStore::flush(cx));
    let saved = std::fs::read_to_string(&gui).unwrap();
    eprintln!("gui.toml: {saved:?}");
    assert!(saved.contains("ui_scale = 1.25"));
    preveal(&app, cx, scale_step_id(1.0));
    click(&app, cx, scale_step_id(1.0));
    assert_eq!(
        cx.update(|cx| gpui_kit::component::Theme::global(cx).font_size),
        base
    );

    // Language: 中文, then English again; every view follows.
    pradio(&app, cx, "prefs-language", 0);
    assert_eq!(
        cx.update(|cx| SettingsStore::get(cx).language),
        Language::ZhCn
    );
    click(&app, cx, "rail-instances");
    let zh = (
        label_of(&app, cx, "tab-overview"),
        label_of(&app, cx, "launch"),
        label_of(&app, cx, "status-source"),
    );
    eprintln!("zh: {zh:?}");
    assert_eq!(zh.0.as_deref(), Some("概览"));
    assert_eq!(zh.1.as_deref(), Some("启动"));
    assert!(zh.2.unwrap_or_default().starts_with("下载源"));
    click(&app, cx, "rail-settings");
    pradio(&app, cx, "prefs-language", 1);
    click(&app, cx, "rail-instances");
    let en = (
        label_of(&app, cx, "tab-overview"),
        label_of(&app, cx, "launch"),
    );
    eprintln!("en: {en:?}");
    assert_eq!(en.0.as_deref(), Some("Overview"));
    assert_eq!(en.1.as_deref(), Some("Play"));
    cx.update(|cx| SettingsStore::flush(cx));

    // Leave the data dir as it was.
    drop(app);
    match gui_before {
        Some(bytes) => std::fs::write(&gui, bytes).unwrap(),
        None => std::fs::remove_file(&gui).unwrap(),
    }
    assert_eq!(
        std::fs::read(data_dir().join("config.toml")).unwrap(),
        config_before,
        "the defaults went back through the page"
    );
}

// ---- 11. GC presets on Java 8, 21 and 25 ---------------------------------

/// What a launch through the app showed and ran.
struct Launched {
    pid: u32,
    warnings: Vec<String>,
    args: Vec<String>,
}

/// Clicks 启动 on the selected `id`, notes the pre-launch warnings (and
/// continues), waits for the game to get going, and reads its command line.
fn launch_noting(app: &Booted, cx: &mut TestAppContext, id: &str) -> Launched {
    let m = model(app, cx);
    assert!(!disabled(app, cx, "launch"));
    click(app, cx, "launch");
    let mut warnings = Vec::new();
    wait_for(cx, SLOW, "game start", |cx| {
        if shows(app, cx, "dialog") && element(app, cx, "prelaunch-title").is_some() {
            warnings = cx
                .update_window(app.handle, |_, window, cx| {
                    window.render_frame(cx);
                    let mut found = Vec::new();
                    let dialog = window.within("dialog");
                    for i in 0..8 {
                        match dialog.try_find(patina_app::launch::prelaunch_dialog::warning_id(i)) {
                            Some(w) => found.push(w.label().unwrap_or_default().to_owned()),
                            None => break,
                        }
                    }
                    found
                })
                .unwrap();
            dialog_click(app, cx, "ok");
        }
        if let Some(OpState::Failed { message, .. }) = op_of(&m, cx, id) {
            panic!("launch failed: {message}");
        }
        matches!(run_state(&m, cx, id), Some(RunState::Running { .. }))
    });
    let Some(RunState::Running { pid, .. }) = run_state(&m, cx, id) else {
        unreachable!()
    };
    let mut args = Vec::new();
    wait_for(cx, Duration::from_secs(60), "java command line", |_| {
        args = cmdline(pid);
        args.iter().any(|a| a.starts_with("-Xmx"))
    });
    // The JVM took its flags if the game gets as far as its user.
    let start = Instant::now();
    wait_for(cx, Duration::from_secs(300), "game log", |cx| {
        log_text(app, cx, id).contains("Setting user:")
            || (start.elapsed() > Duration::from_secs(150) && !gone(pid))
    });
    assert!(!gone(pid), "the game still runs with these flags");
    Launched {
        pid,
        warnings,
        args,
    }
}

fn gc_flags(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| a.starts_with("-XX:") || a.starts_with("-Xm"))
        .cloned()
        .collect()
}

/// The instance's GC preset and Java override, as saved.
fn saved_gc(app: &Booted, id: &str) -> Option<GcPreset> {
    app.store
        .load(id)
        .unwrap()
        .config
        .java_override
        .map(|j| j.gc_preset)
}

fn gc_disabled(app: &Booted, cx: &mut TestAppContext, ix: usize) -> bool {
    reveal(app, cx, "settings-gc");
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window
            .within("settings-gc")
            .try_find(ix)
            .and_then(|e| e.disabled())
            == Some(true)
    })
    .unwrap()
}

#[gpui_kit::test]
#[ignore = "starts real Minecraft six times"]
fn e2e_11_gc_presets_per_instance(cx: &mut TestAppContext) {
    let app = boot(cx);
    loaded(&app, cx);
    activate(&app, cx);
    for (id, java) in [("forge-1122", 8), ("neo-1211", 21), ("neo-new", 25)] {
        select(&app, cx, id);
        sclick(&app, cx, "tab-settings");
        assert!(
            saved_gc(&app, id).is_none(),
            "{id} starts on the global settings"
        );
        sclick(&app, cx, "settings-use-global");
        settle(cx, |_| saved_gc(&app, id).is_some());
        // The managed Java's version becomes known.
        wait_for(cx, Duration::from_secs(60), "java known", |cx| {
            !sshows(&app, cx, "settings-gc-unknown")
        });
        let off: Vec<bool> = (0..4).map(|ix| gc_disabled(&app, cx, ix)).collect();
        let reason = label_of(&app, cx, "settings-gc-reason-2");
        eprintln!(
            "[{id} Java {java}] presets disabled (none, G1, ZGC, custom): {off:?}; ZGC note: {reason:?}"
        );
        assert_eq!(off, [false, false, java < 21, false]);

        for (ix, preset) in [(1, GcPreset::G1Aikar), (2, GcPreset::GenerationalZgc)] {
            if off[ix] {
                // Greyed: a click preserves the previous preset (G1 was
                // selected and launched earlier in this instance's loop).
                let before = saved_gc(&app, id);
                click_radio(&app, cx, "settings-gc", ix);
                idle(cx, Duration::from_millis(500));
                assert_eq!(saved_gc(&app, id), before);
                eprintln!("[{id} Java {java}] {preset:?}: greyed out, not chosen");
                continue;
            }
            click_radio(&app, cx, "settings-gc", ix);
            settle(cx, |_| saved_gc(&app, id) == Some(preset));
            let run = launch_noting(&app, cx, id);
            eprintln!(
                "[{id} Java {java}] {preset:?}: pid {} java {:?}\n  warnings {:?}\n  flags {:?}",
                run.pid,
                run.args.first(),
                run.warnings,
                gc_flags(&run.args)
            );
            let has = |flag: &str| run.args.iter().any(|a| a == flag);
            match preset {
                GcPreset::G1Aikar => {
                    assert!(has("-XX:+UseG1GC") && has("-XX:G1HeapRegionSize=8M"));
                }
                _ => {
                    assert!(has("-XX:+UseZGC"));
                    assert_eq!(has("-XX:+ZGenerational"), java < 23);
                }
            }
            assert!(run.warnings.iter().all(|w| !w.contains("GC")));
            idle(cx, Duration::from_secs(10));
            force_stop(&app, cx, id);
            settle(cx, |_| gone(run.pid));
            sclick(&app, cx, "tab-settings");
        }
        // Back on the global settings.
        sclick(&app, cx, "settings-use-global");
        settle(cx, |_| saved_gc(&app, id).is_none());
    }
}

#[gpui_kit::test]
#[ignore = "starts real Minecraft"]
fn e2e_12_global_gc_default_ignored_on_java_8(cx: &mut TestAppContext) {
    let config_before = std::fs::read(data_dir().join("config.toml")).unwrap();
    let app = boot(cx);
    loaded(&app, cx);
    activate(&app, cx);

    // The global default offers every preset, with a note.
    click(&app, cx, "rail-settings");
    let page = cx.update(|cx| app.root.read(cx).settings().clone());
    settle(cx, |cx| page.read_with(cx, |p, _| p.config().is_some()));
    preveal(&app, cx, "prefs-gc");
    let off: Vec<bool> = (0..4)
        .map(|ix| {
            cx.update_window(app.handle, |_, window, cx| {
                window.render_frame(cx);
                window.within("prefs-gc").find(ix).disabled() == Some(true)
            })
            .unwrap()
        })
        .collect();
    eprintln!(
        "global presets disabled: {off:?}; note {:?}",
        label_of(&app, cx, "prefs-gc-note")
    );
    assert_eq!(off, [false; 4]);
    pradio(&app, cx, "prefs-gc", 2);
    settings_saved(&app, cx);
    wait_for(cx, Duration::from_secs(10), "ZGC default saved", |_| {
        on_disk_java(&app).gc_preset == GcPreset::GenerationalZgc
    });

    // Java 8 on the global settings: launches without it, and says so.
    click(&app, cx, "rail-instances");
    select(&app, cx, "forge-1122");
    assert!(saved_gc(&app, "forge-1122").is_none());
    let run = launch_noting(&app, cx, "forge-1122");
    eprintln!(
        "[forge-1122 Java 8, global ZGC] pid {}\n  warnings {:?}\n  flags {:?}",
        run.pid,
        run.warnings,
        gc_flags(&run.args)
    );
    assert!(
        run.warnings
            .iter()
            .any(|w| w.contains("Generational ZGC") && w.contains("ignored")),
        "{:?}",
        run.warnings
    );
    assert!(!run.args.iter().any(|a| a.contains("ZGC")));
    idle(cx, Duration::from_secs(10));
    force_stop(&app, cx, "forge-1122");
    settle(cx, |_| gone(run.pid));

    // Java 21 on the global default: it applies.
    select(&app, cx, "neo-1211");
    let run = launch_noting(&app, cx, "neo-1211");
    eprintln!(
        "[neo-1211 Java 21, global ZGC] pid {}\n  warnings {:?}\n  flags {:?}",
        run.pid,
        run.warnings,
        gc_flags(&run.args)
    );
    assert!(run.args.iter().any(|a| a == "-XX:+UseZGC"));
    idle(cx, Duration::from_secs(10));
    force_stop(&app, cx, "neo-1211");
    settle(cx, |_| gone(run.pid));

    // Back to no GC flags.
    click(&app, cx, "rail-settings");
    pradio(&app, cx, "prefs-gc", 0);
    settings_saved(&app, cx);
    wait_for(cx, Duration::from_secs(10), "default back", |_| {
        on_disk_java(&app).gc_preset == GcPreset::None
    });
    drop(app);
    assert_eq!(
        std::fs::read(data_dir().join("config.toml")).unwrap(),
        config_before
    );
}

// ---- language and cleanup ------------------------------------------------

#[gpui_kit::test]
#[ignore = "real data dir"]
fn e2e_98_chinese_labels(cx: &mut TestAppContext) {
    let app = boot_in(cx, Language::ZhCn);
    loaded(&app, cx);
    select(&app, cx, FORGE_1201);
    assert_eq!(label_of(&app, cx, "tab-overview").as_deref(), Some("概览"));
    assert_eq!(label_of(&app, cx, "launch").as_deref(), Some("启动"));
}

/// Deletes every `E2E …` instance left over.
#[gpui_kit::test]
#[ignore = "real data dir"]
fn e2e_99_cleanup(cx: &mut TestAppContext) {
    let app = boot(cx);
    let m = loaded(&app, cx);
    let ids: Vec<String> = m.read_with(cx, |m, _| {
        m.instances()
            .iter()
            .filter(|i| i.id.starts_with("E2E "))
            .map(|i| i.id.clone())
            .collect()
    });
    for id in ids {
        app.store.delete(&id).unwrap();
        eprintln!("deleted {id}");
    }
}
