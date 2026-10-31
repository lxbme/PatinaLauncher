//! UI integration tests: the real app bootstrap in headless windows.

use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::Theme;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Pixels, TestAppContext, px};
use patina_app::core_bridge::testing::{self, FakeCatalog};
use patina_app::core_bridge::{AccountsChanged, Core, LaunchBackend, LoadError};
use patina_app::i18n::Language;
use patina_app::instances::{
    Filter, IconChoice, IconPicked, IconPicker, IconSource, InstanceList, InstancesChanged,
    InstancesModel, NOTICE_TEXT, RunState, VersionsEvent, forget_cached_image, icon_for,
    instance_icon, pick_id, row_id, save_icon,
};
use patina_app::launch::{LevelFilter, LogView, OpError, OpKind, OpState};
use patina_app::settings::{self, AppSettings, SettingsStore};
use patina_app::shell::{AccountLabel, Nav, RootView, StatusBar};
use patina_app::single_instance;
use patina_app::theme::{self, ProductColors};
use patina_core::accounts::AccountStore;
use patina_core::config::SourceChoice;
use patina_core::instance::{Component, ComponentUid, InstanceIcon, InstanceStore};
use patina_core::launch::LogLevel;
use patina_core::paths::DataDirs;

/// The booted app: its main window, root view, and the runtime the core
/// services run on (keep it alive for the test).
struct Booted {
    handle: AnyWindowHandle,
    root: Entity<RootView>,
    _rt: tokio::runtime::Runtime,
    /// Holds the process-global interface language for this test; declared
    /// last so it is released after everything else is gone.
    _language: std::sync::MutexGuard<'static, ()>,
}

impl Booted {
    /// Switches the interface language within the lock this test holds.
    fn language(&self, language: Language) {
        patina_app::i18n::set_language(language);
    }
}

/// Serializes the tests that boot the app, because the interface language is
/// process-global; every test starts in English.
static LANGUAGE: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    /// Whether this test thread holds a booted app (and so [`LANGUAGE`]): a
    /// second boot would wait for its own lock forever.
    static BOOTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Drop for Booted {
    fn drop(&mut self) {
        BOOTED.with(|b| b.set(false));
    }
}

/// Boots the app as `main` does, with its data dir and settings in `dir`
/// and an offline core, and opens the main window.
fn boot(cx: &mut TestAppContext, dir: &Path, start: AppSettings) -> Booted {
    boot_with(cx, dir, start, Arc::new(FakeCatalog::new()))
}

/// [`boot`] with the version lists of `catalog`.
fn boot_with(
    cx: &mut TestAppContext,
    dir: &Path,
    start: AppSettings,
    catalog: Arc<FakeCatalog>,
) -> Booted {
    boot_core(cx, dir, start, |rt, data| testing::core(rt, data, catalog))
}

/// [`boot`] with launches prepared by `games` (fake games).
fn boot_games(cx: &mut TestAppContext, dir: &Path, games: Arc<dyn LaunchBackend>) -> Booted {
    boot_core(cx, dir, AppSettings::default(), |rt, data| {
        testing::core_with_launch(rt, data, Arc::new(FakeCatalog::new()), games)
    })
}

/// Boots as [`boot`] does, with the core from `make_core`.
fn boot_core(
    cx: &mut TestAppContext,
    dir: &Path,
    start: AppSettings,
    make_core: impl FnOnce(tokio::runtime::Handle, DataDirs) -> Core,
) -> Booted {
    assert!(
        !BOOTED.with(|b| b.replace(true)),
        "boot() called twice in one test: the first app still holds the language \
         lock; boot once per test (Booted::language switches the language)"
    );
    let language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
    patina_app::i18n::set_language(Language::En);
    let rt = patina_app::runtime::start();
    let core = make_core(rt.handle().clone(), DataDirs::new(dir));
    // Core work finishes on the runtime's threads, not the test's.
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    let (handle, root) = cx.update(|cx| {
        cx.set_global(core);
        patina_app::init(SettingsStore::new(dir.to_path_buf(), start), cx);
        patina_app::open_main_window(cx).expect("open the main window")
    });
    Booted {
        handle,
        root,
        _rt: rt,
        _language: language,
    }
}

/// Runs gpui until `done` holds, giving work on the runtime real time to
/// finish (at most 5 s).
fn settle(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    for _ in 0..500 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("background work did not finish within 5 s");
}

fn font_size(cx: &mut TestAppContext) -> Pixels {
    cx.update(|cx| Theme::global(cx).font_size)
}

#[gpui_kit::test]
fn app_boots_headless(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    cx.update_window(app.handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update(|cx| {
        let t = Theme::global(cx);
        assert!(t.mode.is_dark(), "Patina has a dark theme only");
        // Patina's tokens, not the stock dark theme's.
        assert_eq!(t.background, theme::background());
        assert_eq!(t.progress_bar, ProductColors::get(cx).signature());
        assert_eq!(cx.windows().len(), 1);
    });
}

#[gpui_kit::test]
#[should_panic(expected = "boot() called twice in one test")]
fn booting_twice_in_one_test_panics_instead_of_hanging(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let _app = boot(cx, tmp.path(), AppSettings::default());
    let _again = boot(cx, tmp.path(), AppSettings::default());
}

#[gpui_kit::test]
fn zoom_keys_scale_the_ui_and_persist(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let mut start = AppSettings::default();
    start.ui_scale = 1.25;
    let app = boot(cx, tmp.path(), start);
    let handle = app.handle;
    // The saved scale is applied before the first frame.
    let base = font_size(cx) / 1.25;
    assert_eq!(base, px(16.));

    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(handle, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    };
    let scale = |cx: &mut TestAppContext| cx.update(|cx| SettingsStore::get(cx).ui_scale);

    press(cx, "ctrl-=");
    assert_eq!(scale(cx), 1.5);
    assert_eq!(font_size(cx), base * 1.5);

    press(cx, "ctrl-0");
    assert_eq!(scale(cx), 1.0);
    assert_eq!(font_size(cx), base);

    press(cx, "ctrl--");
    press(cx, "ctrl--");
    press(cx, "ctrl--");
    assert_eq!(scale(cx), 0.8, "clamped at the smallest step");
    assert_eq!(font_size(cx), base * 0.8);

    press(cx, "ctrl-+");
    assert_eq!(scale(cx), 0.9);

    // Quitting (which runs the app's quit handlers) writes the change.
    cx.quit();
    assert_eq!(settings::load(tmp.path()).ui_scale, 0.9);
}

/// The window's size as the platform has it.
fn window_size(app: &Booted, cx: &mut TestAppContext) -> gpui_kit::Size<Pixels> {
    cx.update_window(app.handle, |_, window, _| window.bounds().size)
        .unwrap()
}

/// Whether element `id` lies wholly inside the window (and is drawn).
fn inside_window(app: &Booted, cx: &mut TestAppContext, id: &'static str) -> bool {
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let size = window.viewport_size();
        let e = window.find(id);
        let b = e.bounds();
        e.visible()
            && b.left() >= px(0.)
            && b.top() >= px(0.)
            && b.right() <= size.width
            && b.bottom() <= size.height
    })
    .unwrap()
}

#[gpui_kit::test]
fn the_title_bar_fits_the_smallest_window(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    cx.run_until_parked();
    for id in [
        "search",
        "title-downloads",
        "title-account",
        "title-settings",
    ] {
        assert!(inside_window(&app, cx, id), "{id} is cut off at 960×600");
    }
}

#[gpui_kit::test]
fn a_scale_too_large_for_the_display_is_not_offered(cx: &mut TestAppContext) {
    // The test display is 1920×1080: 200 % would need 1920×1200.
    let tmp = tempfile::tempdir().unwrap();
    let mut start = AppSettings::default();
    start.ui_scale = 2.0;
    let app = boot(cx, tmp.path(), start);
    let handle = app.handle;
    let scale = |cx: &mut TestAppContext| cx.update(|cx| SettingsStore::get(cx).ui_scale);
    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(handle, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    };

    // Opened at the largest step that fits, with that step's minimum.
    assert_eq!(font_size(cx), px(16.) * 1.75);
    assert_eq!(cx.update(|cx| patina_app::scale::applied_scale(cx)), 1.75);
    let options = cx.update(|cx| patina_app::main_window_options(1.75, cx));
    assert_eq!(
        options.window_min_size,
        Some(gpui_kit::size(px(1680.), px(1050.)))
    );
    let size = window_size(&app, cx);
    assert!(
        size.width >= px(1680.) && size.height >= px(1050.),
        "{size:?}"
    );

    // Zooming in stops there and leaves the saved setting alone.
    press(cx, "ctrl-=");
    assert_eq!(font_size(cx), px(16.) * 1.75);
    assert_eq!(scale(cx), 2.0, "the saved setting is not rewritten");

    // A real change is saved.
    press(cx, "ctrl--");
    assert_eq!(font_size(cx), px(16.) * 1.5);
    assert_eq!(scale(cx), 1.5);
}

#[gpui_kit::test]
fn zooming_in_grows_the_window_to_the_new_minimum(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let handle = app.handle;
    cx.simulate_window_resize(handle, gpui_kit::size(px(960.), px(600.)));
    for _ in 0..10 {
        cx.update_window(handle, |_, window, cx| window.press("ctrl-=", cx))
            .unwrap();
        cx.run_until_parked();
    }
    assert_eq!(cx.update(|cx| SettingsStore::get(cx).ui_scale), 1.75);
    let size = window_size(&app, cx);
    assert!(
        size.width >= px(1680.) && size.height >= px(1050.),
        "{size:?}"
    );
}

/// The test window reports server decorations (no tiling state, as on
/// Linux with server decorations): a zoom step grows it, but a size set
/// afterwards (by the user or a tiling compositor) stands.
#[gpui_kit::test]
fn a_resize_without_a_tiling_state_is_not_fought(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let handle = app.handle;
    cx.simulate_window_resize(handle, gpui_kit::size(px(960.), px(600.)));
    cx.run_until_parked();
    for _ in 0..2 {
        cx.update_window(handle, |_, window, cx| window.press("ctrl-=", cx))
            .unwrap();
        cx.run_until_parked();
    }
    assert_eq!(cx.update(|cx| SettingsStore::get(cx).ui_scale), 1.25);
    assert_eq!(window_size(&app, cx), gpui_kit::size(px(1200.), px(750.)));
    cx.simulate_window_resize(handle, gpui_kit::size(px(1000.), px(640.)));
    cx.run_until_parked();
    assert_eq!(window_size(&app, cx), gpui_kit::size(px(1000.), px(640.)));
}

/// Opens the already-running notice as the second instance does.
fn open_notice(cx: &mut TestAppContext) -> AnyWindowHandle {
    cx.update(gpui_kit::init);
    let handle = cx.update(|cx| {
        patina_app::fonts::init(cx);
        single_instance::open_notice(1.0, cx)
            .expect("open the notice")
            .0
    });
    cx.update_window(handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    handle
}

fn window_count(cx: &mut TestAppContext) -> usize {
    cx.run_until_parked();
    cx.update(|cx| cx.windows().len())
}

#[gpui_kit::test]
fn already_running_notice_closes_on_click(cx: &mut TestAppContext) {
    let handle = open_notice(cx);
    cx.update_window(handle, |_, window, cx| {
        let close = window.find("close");
        assert!(close.visible());
        assert!(close.bounds().size.width > px(0.));
        window.click("close", cx);
    })
    .unwrap();
    assert_eq!(window_count(cx), 0);
}

#[gpui_kit::test]
fn already_running_notice_closes_on_escape(cx: &mut TestAppContext) {
    let handle = open_notice(cx);
    // Other keys leave it open.
    cx.update_window(handle, |_, window, cx| window.press("a", cx))
        .unwrap();
    assert_eq!(window_count(cx), 1);
    // The notice has focus from the start: no click before the key.
    cx.update_window(handle, |_, window, cx| window.press("escape", cx))
        .unwrap();
    assert_eq!(window_count(cx), 0);
}

#[gpui_kit::test]
fn already_running_notice_closes_on_enter(cx: &mut TestAppContext) {
    let handle = open_notice(cx);
    cx.update_window(handle, |_, window, cx| window.press("enter", cx))
        .unwrap();
    assert_eq!(window_count(cx), 0);
}

#[gpui_kit::test]
fn startup_failure_notice_says_why_and_closes(cx: &mut TestAppContext) {
    use patina_app::core_bridge::InitError;
    cx.update(gpui_kit::init);
    let error = InitError::HttpClient("no TLS backend".into());
    let handle = cx.update(|cx| {
        patina_app::fonts::init(cx);
        patina_app::notice::open_startup_failure(&error, 1.0, cx)
            .expect("open the notice")
            .0
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("close", cx);
    })
    .unwrap();
    assert_eq!(window_count(cx), 0);
    assert!(error.to_string().contains("no TLS backend"));
}

/// Whether the rail's selection bar sits beside the `rail_id` button. (A
/// Button's selected look is not exposed to accessibility, so the test reads
/// the geometry.)
fn rail_marks(window: &gpui_kit::Window, rail_id: &'static str) -> bool {
    let bar = window.find("rail-selection").bounds();
    let button = window.find(rail_id).bounds();
    let y = bar.center().y;
    bar.right() <= button.left() && y > button.top() && y < button.bottom()
}

#[gpui_kit::test]
fn rail_click_switches_page(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let nav = |cx: &mut TestAppContext| cx.update(|cx| app.root.read(cx).shell().read(cx).nav());

    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("page-instances").visible());
        assert!(window.try_find("page-accounts").is_none());
        assert!(rail_marks(window, "rail-instances"));
        window.click("rail-accounts", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(nav(cx), Nav::Accounts);
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let page = window.find("page-accounts");
        assert!(page.visible());
        // The page fills the area right of the rail.
        assert!(page.bounds().left() >= window.find("rail-accounts").bounds().right());
        assert!(window.try_find("page-instances").is_none());
        assert!(rail_marks(window, "rail-accounts"));
        assert_eq!(window.find_all("rail-selection").len(), 1);
        // The title bar's settings button leads to the same pages.
        window.click("title-settings", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(nav(cx), Nav::Settings);
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("page-settings").visible());
        assert!(window.try_find("page-accounts").is_none());
        assert!(rail_marks(window, "rail-settings"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn slash_focuses_search_but_not_inside_input(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_ne!(window.find("search").focused(), Some(true));
        window.press("/", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let search = window.find("search");
        assert_eq!(search.focused(), Some(true), "`/` focuses the search");
        assert_eq!(search.value().unwrap_or(""), "", "and is not typed");
        // Inside the input, `/` is text.
        window.press("/", cx);
        window.press("/", cx);
        let search = window.find("search");
        assert_eq!(search.focused(), Some(true));
        assert_eq!(search.value(), Some("//"));
    })
    .unwrap();
}

/// Bytes of the regular files under `dir`, not following symbolic links.
fn regular_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let meta = std::fs::symlink_metadata(entry.path()).unwrap();
        if meta.is_dir() {
            total += regular_bytes(&entry.path());
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

#[gpui_kit::test]
fn status_bar_counts_instances_and_size(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let store = InstanceStore::new(data.instances());
    let alpha = store.create("Alpha", "1.21.1").unwrap();
    let beta = store.create("Beta", "1.20.1").unwrap();
    std::fs::create_dir_all(alpha.game_dir()).unwrap();
    std::fs::write(alpha.game_dir().join("a.bin"), vec![1u8; 1500]).unwrap();
    std::fs::create_dir_all(beta.game_dir().join("mods")).unwrap();
    std::fs::write(beta.game_dir().join("mods/b.jar"), vec![2u8; 2000]).unwrap();
    // Links to big content outside the instances are not counted.
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("big.bin"), vec![0u8; 100_000]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(outside.join("big.bin"), alpha.game_dir().join("link.bin")).unwrap();
        symlink(&outside, beta.game_dir().join("linked-dir")).unwrap();
    }
    // A broken instance is neither counted nor sized.
    let broken = data.instances().join("Broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("instance.toml"), "garbage[[").unwrap();
    std::fs::write(broken.join("data.bin"), vec![0u8; 50_000]).unwrap();
    let expected = regular_bytes(&alpha.dir) + regular_bytes(&beta.dir);
    assert!((3500..100_000).contains(&expected));

    let app = boot(cx, tmp.path(), AppSettings::default());
    let status = cx.update(|cx| app.root.read(cx).status_bar().clone());
    settle(cx, |cx| status.read_with(cx, |s, _| s.usage().is_some()));
    status.read_with(cx, |s, _| {
        let usage = s.usage().unwrap();
        assert_eq!(usage.count, 2);
        assert_eq!(usage.bytes, expected);
        assert!(usage.complete);
        assert_eq!(s.runtimes(), Some(0));
        assert_eq!(s.source(), Some(SourceChoice::Auto));
    });
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        for id in [
            "status-source",
            "status-runtimes",
            "status-instances",
            "status-version",
        ] {
            assert!(window.find(id).visible(), "{id} shows");
        }
        let label = window.find("status-version").label().unwrap().to_string();
        assert!(label.ends_with(env!("CARGO_PKG_VERSION")), "{label}");
    })
    .unwrap();
}

fn usage_count(status: &Entity<StatusBar>, cx: &mut TestAppContext) -> Option<usize> {
    status.read_with(cx, |s, _| s.usage().map(|u| u.count))
}

#[gpui_kit::test]
fn status_scan_is_throttled_and_keeps_its_value_on_failure(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let store = InstanceStore::new(data.instances());
    store.create("Alpha", "1.21.1").unwrap();

    let app = boot(cx, tmp.path(), AppSettings::default());
    let status = cx.update(|cx| app.root.read(cx).status_bar().clone());
    let idle = |cx: &mut TestAppContext| status.read_with(cx, |s, _| !s.is_scanning());
    settle(cx, |cx| idle(cx) && usage_count(&status, cx).is_some());
    assert_eq!(usage_count(&status, cx), Some(1));

    // Activation-style refreshes within the interval do not rescan...
    store.create("Beta", "1.21.1").unwrap();
    status.update(cx, |s, cx| s.refresh(cx));
    assert!(status.read_with(cx, |s, _| !s.is_scanning()), "throttled");
    // ...until the interval has passed (gpui's clock).
    cx.executor()
        .advance_clock(patina_app::shell::SCAN_INTERVAL);
    status.update(cx, |s, cx| s.refresh(cx));
    assert!(status.read_with(cx, |s, _| s.is_scanning()));
    // One at a time: a second refresh while it runs starts nothing new.
    status.update(cx, |s, cx| s.refresh(cx));
    settle(cx, idle);
    assert_eq!(usage_count(&status, cx), Some(2));

    // A task-driven refresh is not throttled.
    store.create("Gamma", "1.21.1").unwrap();
    status.update(cx, |s, cx| s.refresh_now(cx));
    assert!(status.read_with(cx, |s, _| s.is_scanning()));
    settle(cx, idle);
    assert_eq!(usage_count(&status, cx), Some(3));

    // Listing fails (the instances dir became a file): the last value stays.
    std::fs::rename(data.instances(), tmp.path().join("moved")).unwrap();
    std::fs::write(data.instances(), "not a directory").unwrap();
    status.update(cx, |s, cx| s.refresh_now(cx));
    settle(cx, idle);
    assert_eq!(usage_count(&status, cx), Some(3));
}

const ACCOUNT: &str = r#"
current = "msa-1"

[[accounts]]
id = "msa-1"
kind = "msa"
username = "Steve"
uuid = "00000000-0000-0000-0000-000000000001"
"#;

#[gpui_kit::test]
async fn title_bar_follows_accounts_changes(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let app = boot(cx, tmp.path(), AppSettings::default());
    let title = cx.update(|cx| app.root.read(cx).title_bar().clone());
    let label = |cx: &mut TestAppContext| title.read_with(cx, |t, _| t.account().clone());
    settle(cx, |cx| label(cx) != AccountLabel::Loading);
    assert_eq!(label(cx), AccountLabel::SignedOut);

    // A writer changes the accounts, then signals it.
    let reload = |cx: &mut TestAppContext, text: &str| {
        std::fs::write(tmp.path().join("accounts.toml"), text).unwrap();
        let data = data.clone();
        cx.update(|cx| {
            Core::with_accounts(cx, move |accounts| {
                *accounts = AccountStore::load(&data).map_err(|e| LoadError::Failed(Arc::new(e)));
            })
        })
    };
    reload(cx, ACCOUNT).await.unwrap();
    // Nothing changes until the signal.
    cx.run_until_parked();
    assert_eq!(label(cx), AccountLabel::SignedOut);
    cx.update(AccountsChanged::bump);
    settle(cx, |cx| label(cx) != AccountLabel::SignedOut);
    assert_eq!(label(cx), AccountLabel::SignedIn("Steve".into()));

    // Accounts that failed to load read as signed out.
    let fail =
        cx.update(|cx| Core::with_accounts(cx, |accounts| *accounts = Err(LoadError::NotLoaded)));
    fail.await.unwrap();
    cx.update(AccountsChanged::bump);
    settle(cx, |cx| label(cx) == AccountLabel::SignedOut);

    reload(cx, ACCOUNT).await.unwrap();
    cx.update(AccountsChanged::bump);
    settle(cx, |cx| label(cx) == AccountLabel::SignedIn("Steve".into()));

    // No accounts left: signed out.
    reload(cx, "").await.unwrap();
    cx.update(AccountsChanged::bump);
    settle(cx, |cx| label(cx) == AccountLabel::SignedOut);
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("title-account").visible());
    })
    .unwrap();
}

// ---- Instances page ----

fn instances_model(app: &Booted, cx: &mut TestAppContext) -> Entity<InstancesModel> {
    cx.update(|cx| app.root.read(cx).instances().read(cx).model().clone())
}

fn instance_list(app: &Booted, cx: &mut TestAppContext) -> Entity<InstanceList> {
    cx.update(|cx| app.root.read(cx).instances().read(cx).list().clone())
}

/// Waits for the model's refresh in flight (if any) to land.
fn list_settled(model: &Entity<InstancesModel>, cx: &mut TestAppContext) {
    settle(cx, |cx| {
        model.read_with(cx, |m, _| m.is_loaded() && !m.is_refreshing())
    });
}

fn refresh(model: &Entity<InstancesModel>, cx: &mut TestAppContext) {
    model.update(cx, |m, cx| m.refresh(cx));
    list_settled(model, cx);
}

fn selected(model: &Entity<InstancesModel>, cx: &mut TestAppContext) -> Option<String> {
    model.read_with(cx, |m, _| m.selected().map(str::to_owned))
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

fn shows(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) -> bool {
    let id = id.into();
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some_and(|e| e.visible())
    })
    .unwrap()
}

/// A running state for list tests: only a state, nothing follows or signals
/// it.
fn elsewhere() -> RunState {
    RunState::HeldElsewhere { pid: 0 }
}

/// Writes a config `instance.toml` that does not parse.
fn break_config(dir: &Path) {
    std::fs::write(dir.join("instance.toml"), "garbage[[").unwrap();
}

fn set_loader(store: &InstanceStore, id: &str, uid: ComponentUid) {
    let mut inst = store.load(id).unwrap();
    inst.config
        .set_loader(Some(Component {
            uid,
            version: "1.0".into(),
            profile: None,
        }))
        .unwrap();
    store.save(&inst).unwrap();
}

#[gpui_kit::test]
fn selection_survives_refresh(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    for name in ["Alpha", "Beta", "Gamma"] {
        store.create(name, "1.21.1").unwrap();
    }
    let beta = store.load("Beta").unwrap();
    std::fs::create_dir_all(beta.game_dir().join("mods")).unwrap();
    std::fs::write(beta.game_dir().join("mods/a.jar"), [0u8; 10]).unwrap();

    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    assert_eq!(selected(&model, cx).as_deref(), Some("Alpha"), "the first");

    click(&app, cx, row_id("Beta"));
    assert_eq!(selected(&model, cx).as_deref(), Some("Beta"));
    assert!(shows(&app, cx, "instance-detail"));

    // A new instance played just now goes to the top; the selection stays.
    store.create("Aaron", "1.20.1").unwrap();
    store.touch_played("Aaron").unwrap();
    refresh(&model, cx);
    let ids: Vec<String> = model.read_with(cx, |m, _| {
        m.instances().iter().map(|i| i.id.clone()).collect()
    });
    assert_eq!(ids, ["Aaron", "Alpha", "Beta", "Gamma"]);
    assert_eq!(selected(&model, cx).as_deref(), Some("Beta"));

    // The overview's stats are read in the background for the selection.
    let detail = cx.update(|cx| app.root.read(cx).instances().read(cx).detail().clone());
    settle(cx, |cx| {
        detail.read_with(cx, |d, _| d.stats("Beta").is_some())
    });
    detail.read_with(cx, |d, _| {
        let stats = d.stats("Beta").unwrap();
        assert_eq!(stats.mods, Some(1));
        assert_eq!(stats.worlds, Some(0));
    });
    assert!(shows(&app, cx, "stat-mods"));

    // The selected instance disappears: the first one is selected.
    store.delete("Beta").unwrap();
    refresh(&model, cx);
    assert_eq!(selected(&model, cx).as_deref(), Some("Aaron"));
    assert!(!shows(&app, cx, row_id("Beta")));
}

#[gpui_kit::test]
fn a_failed_refresh_keeps_the_list_and_says_so(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let instances = DataDirs::new(tmp.path()).instances();
    InstanceStore::new(instances.clone())
        .create("Alpha", "1.21.1")
        .unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    assert_eq!(notification_count(&app, cx), 0);

    // The directory becomes unreadable (a file in its place).
    let aside = tmp.path().join("aside");
    std::fs::rename(&instances, &aside).unwrap();
    std::fs::write(&instances, "not a directory").unwrap();
    refresh(&model, cx);
    assert!(model.read_with(cx, |m, _| m.error().is_some()));
    assert_eq!(
        model.read_with(cx, |m, _| m.instances().len()),
        1,
        "the last list stays"
    );
    assert!(shows(&app, cx, row_id("Alpha")));
    settle(cx, |cx| notification_count(&app, cx) == 1);
    // Failing again (every window activation refreshes) does not pile up.
    refresh(&model, cx);
    cx.run_until_parked();
    assert_eq!(notification_count(&app, cx), 1);

    // Readable again: the error goes.
    std::fs::remove_file(&instances).unwrap();
    std::fs::rename(&aside, &instances).unwrap();
    refresh(&model, cx);
    assert!(model.read_with(cx, |m, _| m.error().is_none()));
}

#[gpui_kit::test]
fn filter_chips_filter_by_loader_and_running(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let store = InstanceStore::new(data.instances());
    for name in ["Fab", "Forg", "Neo", "Van"] {
        store.create(name, "1.21.1").unwrap();
    }
    set_loader(&store, "Fab", ComponentUid::Fabric);
    set_loader(&store, "Forg", ComponentUid::Forge);
    set_loader(&store, "Neo", ComponentUid::Neoforge);
    let broken = data.instances().join("Zed");
    std::fs::create_dir_all(&broken).unwrap();
    break_config(&broken);

    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    let list = instance_list(&app, cx);
    list_settled(&model, cx);
    let rows = |cx: &mut TestAppContext| list.read_with(cx, |l, _| l.rows().to_vec());
    assert_eq!(
        rows(cx),
        ["Fab", "Forg", "Neo", "Van", "Zed"],
        "broken last"
    );

    let expect = |cx: &mut TestAppContext, chip: Filter, ids: &[&str]| {
        click(&app, cx, chip.chip_id());
        assert_eq!(list.read_with(cx, |l, _| l.filter()), chip);
        assert_eq!(rows(cx), ids, "{chip:?}");
        for id in ["Fab", "Forg", "Neo", "Van", "Zed"] {
            assert_eq!(
                shows(&app, cx, row_id(id)),
                ids.contains(&id),
                "{chip:?} {id}"
            );
        }
    };
    use patina_core::loader::LoaderKind;
    expect(cx, Filter::Loader(LoaderKind::Fabric), &["Fab"]);
    expect(cx, Filter::Loader(LoaderKind::Forge), &["Forg"]);
    expect(cx, Filter::Loader(LoaderKind::Neoforge), &["Neo"]);
    expect(cx, Filter::Vanilla, &["Van"]);
    expect(cx, Filter::Running, &[]);

    // Running instances show under 运行中 as soon as they run.
    model.update(cx, |m, cx| m.set_run_state("Forg", Some(elsewhere()), cx));
    cx.run_until_parked();
    assert_eq!(rows(cx), ["Forg"]);
    assert!(shows(&app, cx, row_id("Forg")));
    model.update(cx, |m, cx| m.set_run_state("Forg", None, cx));
    cx.run_until_parked();
    assert!(rows(cx).is_empty());

    expect(cx, Filter::All, &["Fab", "Forg", "Neo", "Van", "Zed"]);
}

#[gpui_kit::test]
fn external_damage_shows_broken_entry(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let one = store.create("One", "1.21.1").unwrap();
    let two = store.create("Two", "1.21.1").unwrap();

    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, row_id("Two"));
    assert_eq!(selected(&model, cx).as_deref(), Some("Two"));

    // Another program damages One and removes Two.
    break_config(&one.dir);
    std::fs::remove_dir_all(&two.dir).unwrap();
    refresh(&model, cx);

    let (healthy, broken, error) = model.read_with(cx, |m, _| {
        (
            m.instances().len(),
            m.broken().iter().map(|b| b.id.clone()).collect::<Vec<_>>(),
            m.broken().first().map(|b| b.error.to_string()),
        )
    });
    assert_eq!(healthy, 0);
    assert_eq!(broken, ["One"]);
    assert_eq!(
        selected(&model, cx).as_deref(),
        Some("One"),
        "a valid entry"
    );
    assert!(shows(&app, cx, row_id("One")));
    assert!(!shows(&app, cx, row_id("Two")));
    // The detail is the broken entry's: the core's error and its actions.
    assert!(shows(&app, cx, "broken-detail"));
    assert!(!shows(&app, cx, "instance-detail"));
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("broken-error").label(), error.as_deref());
        assert!(window.find("broken-open-folder").visible());
        assert!(window.find("broken-delete").visible());
    })
    .unwrap();
    // (The test platform cannot open folders, so "打开文件夹" is not clicked.)
}

#[gpui_kit::test]
fn add_instance_action_opens_dialog(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    let title = |cx: &mut TestAppContext| {
        cx.update_window(app.handle, |_, window, cx| {
            window.render_frame(cx);
            (window.within("dialog"))
                .try_find("add-instance-title")
                .and_then(|t| t.label().map(str::to_owned))
        })
        .unwrap()
    };

    assert!(shows(&app, cx, "add-instance-empty"));
    assert!(!shows(&app, cx, "instance-detail"));
    assert!(!shows(&app, cx, "dialog"));
    click(&app, cx, "add-instance-empty");
    assert_eq!(
        title(cx).as_deref(),
        Some("Add instance"),
        "the empty state's button"
    );
    // Escape leaves without creating anything.
    cx.update_window(app.handle, |_, window, cx| window.press("escape", cx))
        .unwrap();
    cx.run_until_parked();
    assert!(!shows(&app, cx, "dialog"));

    click(&app, cx, "instances-add");
    assert_eq!(title(cx).as_deref(), Some("Add instance"), "the list's ＋");
    click(&app, cx, "add-instance-cancel");
    assert!(!shows(&app, cx, "dialog"));

    // With an instance the detail replaces the empty state; ＋ still opens it.
    InstanceStore::new(DataDirs::new(tmp.path()).instances())
        .create("Alpha", "1.21.1")
        .unwrap();
    refresh(&model, cx);
    assert!(!shows(&app, cx, "add-instance-empty"));
    assert!(shows(&app, cx, "instance-detail"));
    click(&app, cx, "instances-add");
    assert!(title(cx).is_some());
    assert_eq!(
        model.read_with(cx, |m, _| m.instances().len()),
        1,
        "nothing created by opening and closing"
    );
}

#[gpui_kit::test]
fn broken_delete_requires_confirmation(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let store = InstanceStore::new(data.instances());
    store.create("Alpha", "1.21.1").unwrap();
    let broken = data.instances().join("Broken");
    std::fs::create_dir_all(&broken).unwrap();
    break_config(&broken);
    std::fs::write(broken.join("data.bin"), [0u8; 100]).unwrap();

    let app = boot(cx, tmp.path(), AppSettings::default());
    app.language(Language::ZhCn);
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    let changed = Rc::new(std::cell::Cell::new(0));
    let _subscription = cx.update(|cx| {
        let changed = changed.clone();
        cx.subscribe(&model, move |_, _: &InstancesChanged, _| {
            changed.set(changed.get() + 1)
        })
    });

    click(&app, cx, row_id("Broken"));
    assert!(shows(&app, cx, "broken-detail"));
    click(&app, cx, "broken-delete");
    assert!(shows(&app, cx, "dialog"), "asks first");
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let title = window.within("dialog").find("delete-title");
        assert_eq!(title.label(), Some("删除\"Broken\"？"));
    })
    .unwrap();
    cx.run_until_parked();
    assert!(broken.exists(), "nothing deleted before the answer");

    // 取消 keeps the folder.
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click("cancel", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!shows(&app, cx, "dialog"));
    std::thread::sleep(Duration::from_millis(50));
    cx.run_until_parked();
    assert!(broken.exists());
    assert_eq!(changed.get(), 0);

    // 删除 deletes it, refreshes the list and tells the status bar.
    click(&app, cx, "broken-delete");
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click("ok", cx);
    })
    .unwrap();
    settle(cx, |cx| {
        model.read_with(cx, |m, _| {
            m.broken().is_empty() && !m.is_refreshing() && !m.is_deleting("Broken")
        })
    });
    assert!(!broken.exists());
    assert_eq!(changed.get(), 1);
    assert_eq!(selected(&model, cx).as_deref(), Some("Alpha"));
    assert!(!shows(&app, cx, "dialog"));
    assert!(!shows(&app, cx, row_id("Broken")));
    assert!(data.instances().join("Alpha").exists());
}

#[gpui_kit::test]
fn arrow_keys_move_the_selection_through_the_list(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let ids: Vec<String> = (0..20).map(|i| format!("I{i:02}")).collect();
    for id in &ids {
        store.create(id, "1.21.1").unwrap();
    }

    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    // Pointer selection hands the keyboard to the list.
    click(&app, cx, row_id("I00"));
    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(app.handle, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    };
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("instance-rows-region").focused(), Some(true));
        // The last rows are below the fold at first.
        assert!(!window.try_find(row_id("I19")).is_some_and(|r| r.visible()));
    })
    .unwrap();

    press(cx, "up");
    assert_eq!(selected(&model, cx).as_deref(), Some("I00"), "top stays");
    press(cx, "down");
    press(cx, "down");
    assert_eq!(selected(&model, cx).as_deref(), Some("I02"));

    // Down to the last row: it is scrolled into view.
    for _ in 0..30 {
        press(cx, "down");
    }
    assert_eq!(selected(&model, cx).as_deref(), Some("I19"), "bottom stays");
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let region = window.find("instance-rows-region").bounds();
        let row = window.find(row_id("I19"));
        assert!(row.visible());
        assert!(row.bounds().bottom() <= region.bottom() + px(1.), "in view");
        assert!(row.bounds().top() >= region.top());
    })
    .unwrap();

    // The keys follow the filter's rows, not the whole list.
    let list = instance_list(&app, cx);
    list.update(cx, |l, cx| l.set_filter(Filter::Running, cx));
    model.update(cx, |m, cx| {
        m.set_run_state("I05", Some(elsewhere()), cx);
        m.set_run_state("I07", Some(elsewhere()), cx);
    });
    cx.run_until_parked();
    press(cx, "down");
    assert_eq!(
        selected(&model, cx).as_deref(),
        Some("I05"),
        "first visible row"
    );
    press(cx, "down");
    assert_eq!(selected(&model, cx).as_deref(), Some("I07"));
}

#[gpui_kit::test]
fn english_filter_chips_fit(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let mut start = AppSettings::default();
    start.language = Language::En;
    let app = boot(cx, tmp.path(), start);
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let row = window.find("instance-filters").bounds();
        let first = window.find(Filter::CHIPS[0].chip_id()).bounds();
        let last = window.find(Filter::CHIPS[5].chip_id()).bounds();
        // The gutter on both sides, so nothing needs scrolling, and half a
        // rem to spare for a font that sets the labels a little wider.
        let gutter = first.left() - row.left();
        assert!(gutter > px(0.));
        let spare = px(8.);
        assert!(
            last.right() + gutter + spare <= row.right(),
            "last chip ends at {:?}, row at {:?}",
            last.right(),
            row.right()
        );
        assert!(window.find(Filter::CHIPS[5].chip_id()).visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn every_tab_is_reachable_in_the_smallest_window(cx: &mut TestAppContext) {
    use patina_app::instances::DetailTab;
    let tmp = tempfile::tempdir().unwrap();
    InstanceStore::new(DataDirs::new(tmp.path()).instances())
        .create("Alpha", "1.21.1")
        .unwrap();
    let mut start = AppSettings::default();
    start.language = Language::En;
    let app = boot(cx, tmp.path(), start);
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    let detail = cx.update(|cx| app.root.read(cx).instances().read(cx).detail().clone());
    let scroll_tabs = |cx: &mut TestAppContext, dx: f32| {
        cx.update_window(app.handle, |_, window, cx| {
            window.render_frame(cx);
            // The wheel over a tab in the middle, which stays in view.
            window.scroll(
                "tab-mods",
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(dx), px(0.))),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
    };

    // English labels need more than the narrowest detail pane: the strip
    // scrolls instead of cutting the last tabs off.
    assert!(shows(&app, cx, "tab-overview"));
    assert!(!shows(&app, cx, "tab-log"), "this test needs an overflow");
    scroll_tabs(cx, -1000.);
    assert!(shows(&app, cx, "tab-log"));
    click(&app, cx, "tab-log");
    assert_eq!(detail.read_with(cx, |d, _| d.tab()), DetailTab::Log);

    // Choosing a tab out of view brings it into view.
    scroll_tabs(cx, 1000.);
    assert!(!shows(&app, cx, "tab-log"));
    detail.update(cx, |d, cx| d.set_tab(DetailTab::Settings, cx));
    detail.update(cx, |d, cx| d.set_tab(DetailTab::Log, cx));
    cx.run_until_parked();
    assert!(shows(&app, cx, "tab-log"));
}

/// Waits for the save task and returns its result.
fn finish_save(
    task: gpui_kit::Task<Result<(), gpui_kit::SharedString>>,
    cx: &mut TestAppContext,
) -> Result<(), gpui_kit::SharedString> {
    let slot = Rc::new(std::cell::RefCell::new(None));
    let out = slot.clone();
    cx.update(|cx| {
        cx.spawn(async move |_| *out.borrow_mut() = Some(task.await))
            .detach()
    });
    settle(cx, |_| slot.borrow().is_some());
    slot.take().unwrap()
}

/// A builtin PNG as a stand-in for a user's upload.
fn sample_png(dir: &Path) -> std::path::PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/instance-icons/sword.png");
    let to = dir.join("upload.png");
    std::fs::copy(from, &to).unwrap();
    to
}

#[gpui_kit::test]
fn invalid_upload_keeps_old_icon(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    store.create("Alpha", "1.21.1").unwrap();
    store.set_builtin_icon("Alpha", "stone").unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);

    let junk = tmp.path().join("junk.png");
    std::fs::write(&junk, b"definitely not a png").unwrap();
    let saved = cx.update(|cx| save_icon(&model, "Alpha".into(), IconChoice::Custom(junk), cx));
    let error = finish_save(saved, cx).unwrap_err();
    assert!(!error.is_empty());
    assert_eq!(
        store.load("Alpha").unwrap().config.icon,
        Some(InstanceIcon::Builtin("stone".into())),
        "the config is unchanged"
    );
    assert!(!store.load("Alpha").unwrap().dir.join("icon.png").exists());

    // The picker shows the error and clears it on the next pick.
    let window = cx.add_window(|_, cx| IconPicker::new(IconChoice::Builtin("stone".into()), cx));
    let picker = window.root(cx).unwrap();
    picker.update(cx, |p, cx| p.set_error(Some(error.clone()), cx));
    let shown = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, w, cx| {
            w.render_frame(cx);
            w.try_find("icon-error").is_some()
        })
        .unwrap()
    };
    assert!(shown(cx));
    cx.update_window(window.into(), |_, w, cx| w.click(pick_id("sword"), cx))
        .unwrap();
    cx.run_until_parked();
    assert!(!shown(cx));
}

#[gpui_kit::test]
fn custom_upload_saves_then_clears(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    store.create("Alpha", "1.21.1").unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    let icon_of = |cx: &mut TestAppContext| model.read_with(cx, |m, _| icon_for(&m.instances()[0]));
    assert!(matches!(icon_of(cx), IconSource::Letter { ch: 'A', .. }));

    let png = sample_png(tmp.path());
    let task = cx.update(|cx| save_icon(&model, "Alpha".into(), IconChoice::Custom(png), cx));
    finish_save(task, cx).unwrap();
    list_settled(&model, cx);
    let IconSource::Custom(path) = icon_of(cx) else {
        panic!("the list shows the custom icon");
    };
    assert!(path.is_file());

    let task = cx.update(|cx| {
        save_icon(
            &model,
            "Alpha".into(),
            IconChoice::Builtin("axe".into()),
            cx,
        )
    });
    finish_save(task, cx).unwrap();
    list_settled(&model, cx);
    assert_eq!(icon_of(cx), IconSource::Builtin("axe"));
    assert!(!path.exists(), "the uploaded file is gone");

    let task = cx.update(|cx| save_icon(&model, "Alpha".into(), IconChoice::None, cx));
    finish_save(task, cx).unwrap();
    list_settled(&model, cx);
    assert!(matches!(icon_of(cx), IconSource::Letter { .. }));
}

#[gpui_kit::test]
fn picker_emits_the_chosen_icon(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let _app = boot(cx, tmp.path(), AppSettings::default());
    let window = cx.add_window(|_, cx| IconPicker::new(IconChoice::None, cx));
    let picker = window.root(cx).unwrap();
    let got = Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = got.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&picker, move |_, IconPicked(choice): &IconPicked, _| {
            sink.borrow_mut().push(choice.clone())
        })
    });
    for id in [pick_id("lava"), "icon-letter".into()] {
        cx.update_window(window.into(), |_, w, cx| {
            w.render_frame(cx);
            w.click(id, cx)
        })
        .unwrap();
        cx.run_until_parked();
    }
    assert_eq!(
        *got.borrow(),
        [IconChoice::Builtin("lava".into()), IconChoice::None]
    );
}

fn press_in(window: gpui_kit::WindowHandle<IconPicker>, cx: &mut TestAppContext, key: &str) {
    cx.update_window(window.into(), |_, w, cx| {
        w.render_frame(cx);
        w.press(key, cx)
    })
    .unwrap();
    cx.run_until_parked();
}

#[gpui_kit::test]
fn picker_grid_is_keyboard_reachable(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let _app = boot(cx, tmp.path(), AppSettings::default());
    let window = cx.add_window(|_, cx| IconPicker::new(IconChoice::None, cx));
    let picker = window.root(cx).unwrap();
    let got = Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = got.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&picker, move |_, IconPicked(choice): &IconPicked, _| {
            sink.borrow_mut().push(choice.clone())
        })
    });
    let grid_focused = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, w, cx| {
            w.render_frame(cx);
            w.find("icon-grid").focused()
        })
        .unwrap()
    };
    let cursor = |cx: &mut TestAppContext| picker.read_with(cx, |p, _| p.cursor_name());
    assert_ne!(grid_focused(cx), Some(true));

    // What the Tab key does inside an app window's root (a bare test window
    // has no root to bind it).
    cx.update_window(window.into(), |_, w, cx| {
        w.render_frame(cx);
        w.focus_next(cx)
    })
    .unwrap();
    assert_eq!(grid_focused(cx), Some(true), "tab reaches the grid");
    assert_eq!(cursor(cx), "grass-block");
    press_in(window, cx, "left");
    press_in(window, cx, "up");
    assert_eq!(cursor(cx), "grass-block", "stays at the corner");
    press_in(window, cx, "right");
    press_in(window, cx, "down");
    assert_eq!(cursor(cx), "lava", "second row, second column");
    assert!(got.borrow().is_empty());

    press_in(window, cx, "enter");
    assert_eq!(*got.borrow(), [IconChoice::Builtin("lava".into())]);
    assert_eq!(
        picker.read_with(cx, |p, _| p.current().clone()),
        IconChoice::Builtin("lava".into())
    );
    press_in(window, cx, "right");
    press_in(window, cx, "space");
    assert_eq!(got.borrow()[1], IconChoice::Builtin("ice".into()));

    // A failed save restores the mark from before the pick.
    picker.update(cx, |p, cx| p.set_error(Some("nope".into()), cx));
    assert_eq!(
        picker.read_with(cx, |p, _| p.current().clone()),
        IconChoice::Builtin("lava".into())
    );
}

/// Draws a custom icon, as a list row does.
struct IconProbe {
    model: Entity<InstancesModel>,
    path: std::path::PathBuf,
}

impl gpui_kit::Render for IconProbe {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        instance_icon(
            &IconSource::Custom(self.path.clone()),
            gpui_kit::rems(2.),
            cx,
        )
    }
}

#[gpui_kit::test]
fn reupload_and_replacing_evict_the_cached_image(cx: &mut TestAppContext) {
    use gpui_kit::ImageSource;
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    store.create("Alpha", "1.21.1").unwrap();
    let first = sample_png(tmp.path());
    let path = store
        .set_custom_icon("Alpha", &first)
        .unwrap()
        .icon_path()
        .unwrap();

    // The core and a model, but no app window that would redraw the icon
    // by itself.
    let rt = patina_app::runtime::start();
    let core = testing::core(
        rt.handle().clone(),
        DataDirs::new(tmp.path()),
        Arc::new(FakeCatalog::new()),
    );
    cx.executor().allow_parking();
    cx.update(|cx| cx.set_global(core));
    let window = cx.add_window(|window, cx| IconProbe {
        model: cx.new(|cx| InstancesModel::new(window, cx)),
        path: path.clone(),
    });
    let model = window
        .root(cx)
        .unwrap()
        .read_with(cx, |p, _| p.model.clone());
    list_settled(&model, cx);

    let cached = |cx: &mut TestAppContext| {
        cx.update(|cx| ImageSource::from(path.as_path()).is_asset_cached(cx))
    };
    let draw = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, w, cx| w.render_frame(cx))
            .unwrap();
    };
    draw(cx);
    settle(cx, |cx| cached(cx));

    // A re-upload keeps the path: the cached image must go.
    let second = tmp.path().join("second.png");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/instance-icons/axe.png"),
        &second,
    )
    .unwrap();
    let task = cx.update(|cx| save_icon(&model, "Alpha".into(), IconChoice::Custom(second), cx));
    finish_save(task, cx).unwrap();
    assert!(!cached(cx), "re-upload evicts the old image");
    draw(cx);
    settle(cx, |cx| cached(cx));

    // Replacing the custom icon with a built-in one evicts the old path too.
    let task = cx.update(|cx| {
        save_icon(
            &model,
            "Alpha".into(),
            IconChoice::Builtin("axe".into()),
            cx,
        )
    });
    finish_save(task, cx).unwrap();
    assert!(!cached(cx), "built-in replaces the custom image");
    assert!(!path.exists());

    // Same for clearing.
    store.set_custom_icon("Alpha", &first).unwrap();
    draw(cx);
    settle(cx, |cx| cached(cx));
    let task = cx.update(|cx| save_icon(&model, "Alpha".into(), IconChoice::None, cx));
    finish_save(task, cx).unwrap();
    assert!(!cached(cx), "clearing evicts the custom image");
    // The helper alone does it as well.
    draw(cx);
    cx.update(|cx| forget_cached_image(&path, cx));
    assert!(!cached(cx));
}

// ---- Operation runner ----

use futures::FutureExt as _;
use patina_core::loader::LoaderError;
use patina_core::locks::{Busy, BusyKind};
use patina_core::progress::{Phase, ProgressSink, TaskProgress, Unit};
use tokio::sync::oneshot;

/// A model over a data dir with the named instances; returns the model, the
/// instance ids (the directory names) and the booted app.
fn op_model(
    cx: &mut TestAppContext,
    dir: &Path,
    names: &[&str],
) -> (Booted, Entity<InstancesModel>, Vec<String>) {
    let store = InstanceStore::new(DataDirs::new(dir).instances());
    let ids = names
        .iter()
        .map(|name| store.create(name, "1.21.1").unwrap().id)
        .collect();
    let app = boot(cx, dir, AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    (app, model, ids)
}

/// Starts a gated operation: it reports `progress` when told to and ends
/// with `end` once the returned sender fires.
fn start_gated(
    model: &Entity<InstancesModel>,
    cx: &mut TestAppContext,
    id: &str,
    end: impl FnOnce() -> Result<(), OpError> + Send + 'static,
    done: Rc<std::cell::Cell<u32>>,
) -> (bool, oneshot::Sender<()>, oneshot::Sender<TaskProgress>) {
    let (gate, wait) = oneshot::channel::<()>();
    let (tick, ticked) = oneshot::channel::<TaskProgress>();
    let started = model.update(cx, |m, cx| {
        m.start_op(
            id,
            OpKind::Reinstall,
            move |sink: ProgressSink| {
                async move {
                    if let Ok(p) = ticked.await {
                        sink.unwrap().send(p).ok();
                    }
                    wait.await.ok();
                    end()
                }
                .boxed()
            },
            move |_, (), _| done.set(done.get() + 1),
            None,
            cx,
        )
    });
    (started, gate, tick)
}

fn items(phase: Phase, done: u64, total: u64) -> TaskProgress {
    TaskProgress {
        phase,
        done,
        total,
        unit: Unit::Items,
    }
}

fn op_of(model: &Entity<InstancesModel>, cx: &mut TestAppContext, id: &str) -> Option<OpState> {
    model.read_with(cx, |m, cx| m.op(id, cx).cloned())
}

#[gpui_kit::test]
fn progress_follows_its_instance(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, model, ids) = op_model(cx, tmp.path(), &["Alpha", "Beta"]);
    let done = Rc::new(std::cell::Cell::new(0));
    let (a_ok, a_gate, a_tick) = start_gated(&model, cx, &ids[0], || Ok(()), done.clone());
    let (b_ok, b_gate, b_tick) = start_gated(&model, cx, &ids[1], || Ok(()), done.clone());
    assert!(a_ok && b_ok, "different instances run side by side");

    const MIB: u64 = 1024 * 1024;
    a_tick
        .send(TaskProgress {
            phase: Phase::DownloadGameFiles,
            done: 3 * MIB,
            total: 10 * MIB,
            unit: Unit::Bytes,
        })
        .unwrap();
    settle(cx, |cx| {
        matches!(
            op_of(&model, cx, &ids[0]),
            Some(OpState::Running {
                progress: Some(_),
                ..
            })
        )
    });
    // The header draws the running bar and "step · amount".
    model.update(cx, |m, cx| m.select(&ids[0], cx));
    let text = cx
        .update_window(app.handle, |_, window, cx| {
            window.render_frame(cx);
            window.find("instance-op-text").label().map(str::to_owned)
        })
        .unwrap();
    let text = text.expect("a label");
    assert!(text.ends_with("3/10 MB"), "{text}");
    assert!(text.contains(" · "), "{text}");
    let Some(OpState::Running { progress, .. }) = op_of(&model, cx, &ids[1]) else {
        panic!("the other instance is still running");
    };
    assert!(
        progress.is_none(),
        "no progress leaks to the other instance"
    );

    b_tick.send(items(Phase::RunProcessor, 1, 2)).unwrap();
    settle(cx, |cx| {
        matches!(
            op_of(&model, cx, &ids[1]),
            Some(OpState::Running {
                progress: Some(_),
                ..
            })
        )
    });
    let Some(OpState::Running {
        progress: Some(a), ..
    }) = op_of(&model, cx, &ids[0])
    else {
        panic!("running");
    };
    assert_eq!(a.fraction, Some(0.3), "a keeps its own value");

    // Finishing one leaves the other running.
    a_gate.send(()).unwrap();
    settle(cx, |cx| op_of(&model, cx, &ids[0]).is_none());
    assert_eq!(done.get(), 1);
    assert!(matches!(
        op_of(&model, cx, &ids[1]),
        Some(OpState::Running { .. })
    ));
    b_gate.send(()).unwrap();
    settle(cx, |cx| op_of(&model, cx, &ids[1]).is_none());
    assert_eq!(done.get(), 2);
}

#[gpui_kit::test]
fn a_progress_tick_notifies_neither_the_list_nor_the_model(cx: &mut TestAppContext) {
    use std::cell::Cell;

    let tmp = tempfile::tempdir().unwrap();
    let (app, model, ids) = op_model(cx, tmp.path(), &["Alpha", "Beta"]);
    let list = instance_list(&app, cx);
    let done = Rc::new(Cell::new(0));
    let (ok, gate, tick) = start_gated(&model, cx, &ids[0], || Ok(()), done.clone());
    assert!(ok);
    cx.run_until_parked();

    // The versions tab of the selected instance (the one busy) shows only
    // whether it is busy: not re-rendered by a tick either.
    model.update(cx, |m, cx| m.select(&ids[0], cx));
    cx.run_until_parked();
    let versions = cx.update(|cx| {
        let page = app.root.read(cx).instances().read(cx);
        page.detail().read(cx).versions_view().cloned().unwrap()
    });
    let (list_notes, model_notes) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let tab_notes = Rc::new(Cell::new(0));
    let _watch = cx.update(|cx| {
        let (l, m, t) = (list_notes.clone(), model_notes.clone(), tab_notes.clone());
        [
            cx.observe(&list, move |_, _| l.set(l.get() + 1)),
            cx.observe(&model, move |_, _| m.set(m.get() + 1)),
            cx.observe(&versions, move |_, _| t.set(t.get() + 1)),
        ]
    });
    tick.send(items(Phase::RunProcessor, 1, 2)).unwrap();
    settle(cx, |cx| {
        matches!(
            op_of(&model, cx, &ids[0]),
            Some(OpState::Running {
                progress: Some(_),
                ..
            })
        )
    });
    assert_eq!(list_notes.get(), 0, "the list is not notified by a tick");
    assert_eq!(model_notes.get(), 0, "nor is the list's model");
    assert_eq!(tab_notes.get(), 0, "nor the versions tab");

    // A run state change (rare) still reaches the list: the running filter
    // and the rows' dots follow it.
    model.update(cx, |m, cx| m.set_run_state(&ids[1], Some(elsewhere()), cx));
    cx.run_until_parked();
    assert!(list_notes.get() > 0, "a game starting re-renders the list");

    gate.send(()).unwrap();
    settle(cx, |_| done.get() == 1);
}

#[gpui_kit::test]
fn second_op_on_same_instance_is_rejected(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (_app, model, ids) = op_model(cx, tmp.path(), &["Alpha"]);
    let done = Rc::new(std::cell::Cell::new(0));
    let (first, gate, tick) = start_gated(&model, cx, &ids[0], || Ok(()), done.clone());
    assert!(first);
    drop(tick);
    let (second, _gate2, _tick2) = start_gated(&model, cx, &ids[0], || Ok(()), done.clone());
    assert!(!second, "one operation per instance");
    gate.send(()).unwrap();
    settle(cx, |cx| op_of(&model, cx, &ids[0]).is_none());
    assert_eq!(done.get(), 1, "only the first ran");
}

#[gpui_kit::test]
fn busy_message_uses_display_name(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    // Rename after creating, so the id (directory) and the name differ.
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let id = store.create("Alpha", "1.21.1").unwrap().id;
    store.rename(&id, "My Pack").unwrap();
    let (_app, model, _) = op_model(cx, tmp.path(), &[]);
    assert_ne!(id, "My Pack", "the id differs from the display name");
    let busy = {
        let id = id.clone();
        move || {
            Err(OpError::Loader(LoaderError::Busy(Busy {
                what: BusyKind::Instance(id),
            })))
        }
    };
    let done = Rc::new(std::cell::Cell::new(0));
    let (_, gate, tick) = start_gated(&model, cx, &id, busy, done.clone());
    drop(tick);
    gate.send(()).unwrap();
    settle(cx, |cx| {
        matches!(op_of(&model, cx, &id), Some(OpState::Failed { .. }))
    });
    let Some(OpState::Failed { message, .. }) = op_of(&model, cx, &id) else {
        panic!("failed");
    };
    assert_eq!(message, "“My Pack” is busy with another operation");
    assert!(!message.contains(&id), "{message}");
    // A game left running (no lock held) reads the same way.
    let running = OpError::Loader(LoaderError::Busy(Busy {
        what: BusyKind::Running(id.clone()),
    }));
    assert_eq!(
        model.read_with(cx, |m, _| running.message(|id| m.display_name(id))),
        "“My Pack” is running; stop the game first"
    );
    assert_eq!(done.get(), 0);
    // Dismissing clears it, and a new operation may start.
    model.update(cx, |m, cx| m.dismiss_op(&id, cx));
    assert!(op_of(&model, cx, &id).is_none());
}

#[gpui_kit::test]
fn result_for_a_deleted_instance_is_dropped(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (_app, model, ids) = op_model(cx, tmp.path(), &["Alpha", "Beta"]);
    let done = Rc::new(std::cell::Cell::new(0));
    let (_, gate, tick) = start_gated(&model, cx, &ids[0], || Ok(()), done.clone());
    drop(tick);
    let (_, gate_failing, tick2) = start_gated(
        &model,
        cx,
        &ids[1],
        || Err(OpError::Message("boom".into())),
        done.clone(),
    );
    drop(tick2);
    for id in &ids {
        std::fs::remove_dir_all(DataDirs::new(tmp.path()).instances().join(id)).unwrap();
    }
    refresh(&model, cx);
    gate.send(()).unwrap();
    gate_failing.send(()).unwrap();
    settle(cx, |cx| {
        op_of(&model, cx, &ids[0]).is_none() && op_of(&model, cx, &ids[1]).is_none()
    });
    cx.run_until_parked();
    assert_eq!(done.get(), 0, "no completion for an instance that is gone");
    assert!(
        op_of(&model, cx, &ids[1]).is_none(),
        "no failure left behind"
    );
}

#[gpui_kit::test]
fn a_failure_of_a_deleted_instance_goes_on_refresh(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (_app, model, ids) = op_model(cx, tmp.path(), &["Alpha", "Beta"]);
    let done = Rc::new(std::cell::Cell::new(0));
    for id in &ids {
        let (_, gate, tick) = start_gated(
            &model,
            cx,
            id,
            || Err(OpError::Message("boom".into())),
            done.clone(),
        );
        drop(tick);
        gate.send(()).unwrap();
    }
    let failed = |cx: &mut TestAppContext, id: &str| {
        matches!(op_of(&model, cx, id), Some(OpState::Failed { .. }))
    };
    settle(cx, |cx| failed(cx, &ids[0]) && failed(cx, &ids[1]));
    list_settled(&model, cx);

    std::fs::remove_dir_all(DataDirs::new(tmp.path()).instances().join(&ids[0])).unwrap();
    refresh(&model, cx);
    assert!(
        op_of(&model, cx, &ids[0]).is_none(),
        "nothing to hang the failure on"
    );
    assert!(failed(cx, &ids[1]), "a listed instance keeps its failure");
}

#[gpui_kit::test]
fn failed_op_offers_retry_then_dismiss(cx: &mut TestAppContext) {
    use patina_app::launch::RetryAction;
    use std::cell::{Cell, RefCell};

    let tmp = tempfile::tempdir().unwrap();
    let (app, model, ids) = op_model(cx, tmp.path(), &["Alpha"]);
    let id = ids[0].clone();
    model.update(cx, |m, cx| m.select(&id, cx));

    let retries = Rc::new(Cell::new(0u32));
    let gate: Rc<RefCell<Option<oneshot::Sender<()>>>> = Rc::default();
    let retry: RetryAction = {
        let (retries, gate, id) = (retries.clone(), gate.clone(), id.clone());
        Rc::new(move |m, cx| {
            retries.set(retries.get() + 1);
            let (tx, rx) = oneshot::channel::<()>();
            *gate.borrow_mut() = Some(tx);
            m.start_op(
                &id,
                OpKind::Reinstall,
                move |_| {
                    async move {
                        rx.await.ok();
                        Err::<(), _>(OpError::Message("again".into()))
                    }
                    .boxed()
                },
                |_, (), _| {},
                None,
                cx,
            );
        })
    };
    model.update(cx, |m, cx| {
        m.start_op(
            &id,
            OpKind::Reinstall,
            |_| async { Err::<(), _>(OpError::Message("boom".into())) }.boxed(),
            |_, (), _| {},
            Some(retry),
            cx,
        );
    });
    settle(cx, |cx| {
        matches!(op_of(&model, cx, &id), Some(OpState::Failed { .. }))
    });

    // The header shows the message and both buttons.
    let label = |cx: &mut TestAppContext| {
        cx.update_window(app.handle, |_, window, cx| {
            window.render_frame(cx);
            window
                .try_find("instance-op-error")
                .and_then(|e| e.label().map(str::to_owned))
        })
        .unwrap()
    };
    assert_eq!(label(cx).as_deref(), Some("boom"));
    assert!(shows(&app, cx, "instance-op-retry"));
    assert!(shows(&app, cx, "instance-op-dismiss"));

    click(&app, cx, "instance-op-retry");
    assert_eq!(retries.get(), 1);
    assert!(matches!(
        op_of(&model, cx, &id),
        Some(OpState::Running { .. })
    ));
    // Retry only applies to a failed op: no second run while it is going.
    model.update(cx, |m, cx| m.retry_op(&id, cx));
    assert_eq!(retries.get(), 1);

    // It fails again, this time without a retry; dismiss clears the area.
    gate.borrow_mut().take().unwrap().send(()).unwrap();
    settle(cx, |cx| {
        matches!(op_of(&model, cx, &id), Some(OpState::Failed { .. }))
    });
    assert_eq!(label(cx).as_deref(), Some("again"));
    assert!(!shows(&app, cx, "instance-op-retry"));
    click(&app, cx, "instance-op-dismiss");
    assert!(op_of(&model, cx, &id).is_none());
    assert!(!shows(&app, cx, "instance-op-error"));
    assert!(!shows(&app, cx, "instance-op-dismiss"));
}

#[gpui_kit::test]
fn panicking_op_fails_with_a_message(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (_app, model, ids) = op_model(cx, tmp.path(), &["Alpha"]);
    model.update(cx, |m, cx| {
        m.start_op(
            &ids[0],
            OpKind::Reinstall,
            |_| {
                async {
                    if true {
                        panic!("test panic in an operation");
                    }
                    Ok::<(), OpError>(())
                }
                .boxed()
            },
            |_, (), _| {},
            None,
            cx,
        );
    });
    settle(cx, |cx| {
        matches!(op_of(&model, cx, &ids[0]), Some(OpState::Failed { .. }))
    });
    let Some(OpState::Failed { message, .. }) = op_of(&model, cx, &ids[0]) else {
        panic!("failed");
    };
    assert!(!message.trim().is_empty());
}

// ---- Add-instance dialog ----

use gpui_kit::component::WindowExt as _;
use gpui_kit::test::ElementSnapshot;
use patina_app::instances::{loader_row_id, loader_segment_id, version_row_id};
use patina_core::loader::{LoaderKind, LoaderVersion};
use patina_core::meta::{Latest, ManifestEntry, VersionKind, VersionManifest};

fn manifest_entry(id: &str, kind: VersionKind) -> ManifestEntry {
    ManifestEntry {
        id: id.into(),
        kind,
        url: String::new(),
        sha1: String::new(),
        release_time: String::new(),
    }
}

fn loader_version(
    kind: LoaderKind,
    mc: &str,
    version: &str,
    recommended: bool,
    latest: bool,
) -> LoaderVersion {
    LoaderVersion {
        kind,
        mc: mc.into(),
        version: version.into(),
        maven_version: version.into(),
        stable: true,
        recommended,
        latest,
    }
}

/// Releases 1.21.1 (latest), 1.21 and 1.20.1, two snapshots and an old
/// beta. Fabric has versions for 1.21.1 (0.16.9 latest, 0.16.5
/// recommended) and 1.20.1 (0.14.0, then 0.15.0 latest, none recommended);
/// Forge for 1.20.1 only; NeoForge for none.
fn catalog() -> Arc<FakeCatalog> {
    use LoaderKind::{Fabric, Forge};
    let manifest = VersionManifest {
        latest: Latest {
            release: "1.21.1".into(),
            snapshot: "24w45a".into(),
        },
        versions: vec![
            manifest_entry("24w45a", VersionKind::Snapshot),
            manifest_entry("1.21.1", VersionKind::Release),
            manifest_entry("1.21", VersionKind::Release),
            manifest_entry("24w14a", VersionKind::Snapshot),
            manifest_entry("1.20.1", VersionKind::Release),
            manifest_entry("b1.7.3", VersionKind::OldBeta),
        ],
    };
    Arc::new(
        FakeCatalog::new()
            .with_manifest(manifest)
            .with_loader_versions([
                loader_version(Fabric, "1.21.1", "0.16.9", false, true),
                loader_version(Fabric, "1.21.1", "0.16.5", true, false),
                loader_version(Fabric, "1.20.1", "0.14.0", false, false),
                loader_version(Fabric, "1.20.1", "0.15.0", false, true),
                loader_version(Forge, "1.20.1", "47.3.0", true, false),
                loader_version(Forge, "1.20.1", "47.4.0", false, true),
            ]),
    )
}

/// The element `id` of the last frame, if drawn.
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

fn name_value(app: &Booted, cx: &mut TestAppContext) -> String {
    element(app, cx, "add-instance-name")
        .and_then(|e| e.value().map(str::to_owned))
        .unwrap_or_default()
}

fn wait_name(app: &Booted, cx: &mut TestAppContext, expected: &str) {
    settle(cx, |cx| name_value(app, cx) == expected);
}

/// Whether row `id` is marked as the chosen one.
fn chosen(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) -> bool {
    element(app, cx, id).and_then(|e| e.selected()) == Some(true)
}

/// Whether control `id` is drawn disabled (an enabled one reports no flag).
fn disabled(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) -> bool {
    let control = element(app, cx, id).expect("the control is drawn");
    control.disabled() == Some(true)
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

/// Boots over an empty data dir with [`catalog`] and opens the dialog from
/// the empty state; returns once the list has loaded and the latest
/// release is chosen.
fn open_dialog(cx: &mut TestAppContext, dir: &Path) -> Booted {
    let app = boot_with(cx, dir, AppSettings::default(), catalog());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "add-instance-empty");
    assert!(shows(&app, cx, "dialog"));
    wait_name(&app, cx, "1.21.1");
    app
}

#[gpui_kit::test]
fn default_name_follows_choices_until_edited(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = open_dialog(cx, tmp.path());
    assert!(
        chosen(&app, cx, version_row_id("1.21.1")),
        "the latest release"
    );

    click(&app, cx, loader_segment_id(Some(LoaderKind::Fabric)));
    wait_name(&app, cx, "Fabric 1.21.1");
    click(&app, cx, version_row_id("1.20.1"));
    wait_name(&app, cx, "Fabric 1.20.1");
    click(&app, cx, loader_segment_id(None));
    wait_name(&app, cx, "1.20.1");

    // Once the user has typed a name, it stays.
    click(&app, cx, "add-instance-name");
    press(&app, cx, "ctrl-a");
    type_text(&app, cx, "My pack");
    assert_eq!(name_value(&app, cx), "My pack");
    click(&app, cx, version_row_id("1.21.1"));
    click(&app, cx, loader_segment_id(Some(LoaderKind::Fabric)));
    settle(cx, |cx| chosen(&app, cx, loader_row_id("0.16.5")));
    assert_eq!(name_value(&app, cx), "My pack");
}

#[gpui_kit::test]
fn release_filter_and_search(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = open_dialog(cx, tmp.path());
    let rows = |cx: &mut TestAppContext| -> Vec<&'static str> {
        ["24w45a", "1.21.1", "1.21", "24w14a", "1.20.1", "b1.7.3"]
            .into_iter()
            .filter(|id| shows(&app, cx, version_row_id(id)))
            .collect()
    };
    assert_eq!(rows(cx), ["1.21.1", "1.21", "1.20.1"], "releases only");
    let tag = element(&app, cx, version_row_id("1.21.1")).unwrap();
    assert_eq!(tag.label(), Some("1.21.1"));

    click(&app, cx, "version-filter-snapshot");
    assert_eq!(
        rows(cx),
        ["24w45a", "1.21.1", "1.21", "24w14a", "1.20.1", "b1.7.3"]
    );
    click(&app, cx, "version-filter-release");
    assert_eq!(rows(cx), ["24w45a", "24w14a", "b1.7.3"]);

    click(&app, cx, "version-search");
    type_text(&app, cx, "24W4");
    assert_eq!(rows(cx), ["24w45a"], "a case-insensitive substring");
    // Filtering does not change the choice.
    assert_eq!(name_value(&app, cx), "1.21.1");
}

#[gpui_kit::test]
fn loader_without_versions_is_disabled(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = open_dialog(cx, tmp.path());
    let fabric = loader_segment_id(Some(LoaderKind::Fabric));
    let forge = loader_segment_id(Some(LoaderKind::Forge));
    let neoforge = loader_segment_id(Some(LoaderKind::Neoforge));
    let hint = |cx: &mut TestAppContext| {
        element(&app, cx, "loader-unsupported").and_then(|e| e.label().map(str::to_owned))
    };

    settle(cx, |cx| disabled(&app, cx, forge));
    assert!(disabled(&app, cx, neoforge));
    assert!(!disabled(&app, cx, fabric));
    assert!(!disabled(&app, cx, loader_segment_id(None)));
    assert_eq!(
        hint(cx).as_deref(),
        Some("Greyed-out loaders don’t support Minecraft 1.21.1")
    );
    click(&app, cx, forge);
    assert_eq!(
        name_value(&app, cx),
        "1.21.1",
        "a greyed-out loader is not chosen"
    );
    assert!(!shows(&app, cx, "loader-version-list"));

    click(&app, cx, version_row_id("1.20.1"));
    settle(cx, |cx| !disabled(&app, cx, forge));
    assert!(disabled(&app, cx, neoforge));
    assert_eq!(
        hint(cx).as_deref(),
        Some("Greyed-out loaders don’t support Minecraft 1.20.1")
    );
    click(&app, cx, forge);
    wait_name(&app, cx, "Forge 1.20.1");
    settle(cx, |cx| chosen(&app, cx, loader_row_id("47.3.0")));

    // A version no loader supports takes the choice back to "None".
    click(&app, cx, version_row_id("1.21"));
    wait_name(&app, cx, "1.21");
    assert!(disabled(&app, cx, fabric));
    assert!(disabled(&app, cx, forge));
    assert!(!shows(&app, cx, "loader-version-list"));
    assert!(!disabled(&app, cx, "add-instance-create"));
}

#[gpui_kit::test]
fn recommended_is_preselected(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = open_dialog(cx, tmp.path());
    click(&app, cx, loader_segment_id(Some(LoaderKind::Fabric)));
    settle(cx, |cx| chosen(&app, cx, loader_row_id("0.16.5")));
    assert!(shows(&app, cx, loader_row_id("0.16.9")));
    assert!(!chosen(&app, cx, loader_row_id("0.16.9")), "not the latest");
    assert!(!disabled(&app, cx, "add-instance-create"));

    // Without a recommended version, the latest.
    click(&app, cx, version_row_id("1.20.1"));
    settle(cx, |cx| chosen(&app, cx, loader_row_id("0.15.0")));
    assert!(!chosen(&app, cx, loader_row_id("0.14.0")));

    // The user's pick, by pointer and by keyboard.
    click(&app, cx, loader_row_id("0.14.0"));
    assert!(chosen(&app, cx, loader_row_id("0.14.0")));
    press(&app, cx, "down");
    assert!(chosen(&app, cx, loader_row_id("0.15.0")));
}

#[gpui_kit::test]
fn create_closes_dialog_and_selects_new_instance(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let app = open_dialog(cx, tmp.path());
    let model = instances_model(&app, cx);
    let status = cx.update(|cx| app.root.read(cx).status_bar().clone());

    click(&app, cx, "add-instance-create");
    assert!(!shows(&app, cx, "dialog"), "closes at once");
    settle(cx, |cx| selected(&model, cx).as_deref() == Some("1.21.1"));
    let created = store.load("1.21.1").unwrap();
    assert_eq!(created.config.name, "1.21.1");
    assert_eq!(created.config.mc_version(), "1.21.1");
    assert!(created.config.loader().is_none());
    assert_eq!(created.config.icon, None);
    assert!(shows(&app, cx, "instance-detail"));
    assert!(
        model.read_with(cx, |m, cx| m.op("1.21.1", cx).is_none()),
        "nothing to install"
    );
    settle(cx, |cx| usage_count(&status, cx) == Some(1));

    // Enter is "Create": the name has the keyboard when the dialog opens.
    click(&app, cx, "instances-add");
    wait_name(&app, cx, "1.21.1");
    press(&app, cx, "enter");
    assert!(!shows(&app, cx, "dialog"));
    settle(cx, |cx| {
        selected(&model, cx).as_deref() == Some("1.21.1 (2)")
    });
    assert_eq!(store.load("1.21.1 (2)").unwrap().config.name, "1.21.1");
}

#[gpui_kit::test]
fn the_add_dialog_fits_the_smallest_window(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = open_dialog(cx, tmp.path());
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    cx.run_until_parked();
    for id in [
        "add-instance-title",
        "add-instance-name",
        "version-search",
        "version-list",
        "loader-none",
        "loader-neoforge",
        "add-instance-cancel",
        "add-instance-create",
    ] {
        assert!(inside_window(&app, cx, id), "{id} is cut off at 960×600");
    }
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let model = instances_model(&app, cx);
    click(&app, cx, "add-instance-create");
    settle(cx, |cx| selected(&model, cx).as_deref() == Some("1.21.1"));
    assert!(store.load("1.21.1").is_ok());
}

#[gpui_kit::test]
fn failed_install_leaves_vanilla_instance_with_retry(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let app = open_dialog(cx, tmp.path());
    let model = instances_model(&app, cx);
    let id = "Fabric 1.21.1";
    // Holding the lock the install takes makes it fail at once, before it
    // would reach the network.
    let held = store.lock(id).unwrap();

    click(&app, cx, loader_segment_id(Some(LoaderKind::Fabric)));
    wait_name(&app, cx, id);
    settle(cx, |cx| chosen(&app, cx, loader_row_id("0.16.5")));
    click(&app, cx, "add-instance-create");
    assert!(!shows(&app, cx, "dialog"));
    settle(cx, |cx| {
        matches!(
            op_of(&model, cx, id),
            Some(OpState::Failed {
                kind: OpKind::InstallLoader,
                ..
            })
        )
    });
    assert_eq!(selected(&model, cx).as_deref(), Some(id));
    assert!(
        store.load(id).unwrap().config.loader().is_none(),
        "still vanilla"
    );
    let error = element(&app, cx, "instance-op-error").and_then(|e| e.label().map(str::to_owned));
    assert_eq!(
        error.as_deref(),
        Some("“Fabric 1.21.1” is busy with another operation")
    );
    assert!(shows(&app, cx, "instance-op-retry"));

    // Retry installs again.
    let state = cx
        .update_window(app.handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("instance-op-retry", cx);
            let model = model.read(cx);
            model.op(id, cx).cloned()
        })
        .unwrap();
    assert!(
        matches!(
            state,
            Some(OpState::Running {
                kind: OpKind::InstallLoader,
                ..
            })
        ),
        "{state:?}"
    );
    settle(cx, |cx| {
        matches!(op_of(&model, cx, id), Some(OpState::Failed { .. }))
    });
    assert!(store.load(id).unwrap().config.loader().is_none());
    drop(held);
}

#[gpui_kit::test]
fn tab_reaches_icon_grid_and_enter_there_picks_without_creating(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let app = open_dialog(cx, tmp.path());
    let model = instances_model(&app, cx);
    let grid_focused = |cx: &mut TestAppContext| {
        element(&app, cx, "icon-grid").and_then(|e| e.focused()) == Some(true)
    };

    // The name has the keyboard; Tab (bound by the window's root) moves on
    // through the dialog to the grid.
    assert_eq!(
        element(&app, cx, "add-instance-name").and_then(|e| e.focused()),
        Some(true)
    );
    let mut reached = false;
    for _ in 0..4 {
        press(&app, cx, "tab");
        if grid_focused(cx) {
            reached = true;
            break;
        }
    }
    assert!(reached, "Tab reaches the icon grid");

    // Enter picks the icon under the cursor and does not create.
    press(&app, cx, "right");
    press(&app, cx, "enter");
    assert!(shows(&app, cx, "dialog"));
    std::thread::sleep(Duration::from_millis(50));
    cx.run_until_parked();
    assert!(
        store.list().unwrap().instances.is_empty(),
        "nothing created"
    );

    click(&app, cx, "add-instance-create");
    settle(cx, |cx| selected(&model, cx).as_deref() == Some("1.21.1"));
    assert_eq!(
        store.load("1.21.1").unwrap().config.icon,
        Some(InstanceIcon::Builtin("dirt".into()))
    );
}

#[gpui_kit::test]
fn icon_save_failure_is_reported_and_the_instance_kept(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let junk = tmp.path().join("junk.png");
    std::fs::write(&junk, b"definitely not a png").unwrap();
    let app = open_dialog(cx, tmp.path());
    let model = instances_model(&app, cx);
    let notifications = |cx: &mut TestAppContext| {
        cx.update_window(app.handle, |_, window, cx| window.notifications(cx).len())
            .unwrap()
    };

    click(&app, cx, "icon-upload");
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(move |_| Some(vec![junk]));
    cx.run_until_parked();
    assert_eq!(notifications(cx), 0);

    click(&app, cx, "add-instance-create");
    settle(cx, |cx| selected(&model, cx).as_deref() == Some("1.21.1"));
    settle(cx, |cx| notifications(cx) == 1);
    let created = store.load("1.21.1").unwrap();
    assert_eq!(created.config.icon, None, "the letter tile, as before");
    assert!(created.icon_path().is_none());
}

#[gpui_kit::test]
fn version_list_failure_offers_retry(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = Arc::new(FakeCatalog::new().with_manifest_error("no network here"));
    let app = boot_with(cx, tmp.path(), AppSettings::default(), catalog.clone());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "add-instance-empty");
    settle(cx, |cx| shows(&app, cx, "version-error"));
    let error = element(&app, cx, "version-error").and_then(|e| e.label().map(str::to_owned));
    assert_eq!(
        error.as_deref(),
        Some("Couldn’t load the version list: no network here")
    );
    assert!(disabled(&app, cx, "add-instance-create"));
    assert!(
        disabled(&app, cx, loader_segment_id(Some(LoaderKind::Fabric))),
        "no loader before a game version"
    );
    // Enter does not create from an incomplete form.
    press(&app, cx, "enter");
    assert!(shows(&app, cx, "dialog"));

    let before = catalog.manifest_requests();
    click(&app, cx, "version-retry");
    settle(cx, |_| catalog.manifest_requests() == before + 1);
    settle(cx, |cx| shows(&app, cx, "version-error"));
    assert!(model.read_with(cx, |m, _| m.instances().is_empty()));
}

/// Presses Tab until `id` has the keyboard (at most 20 times).
fn tab_to(app: &Booted, cx: &mut TestAppContext, id: &'static str) {
    for _ in 0..20 {
        press(app, cx, "tab");
        if element(app, cx, id).and_then(|e| e.focused()) == Some(true) {
            return;
        }
    }
    panic!("Tab never reached {id}");
}

/// Gives background work a moment, then checks nothing was created.
fn nothing_created(store: &InstanceStore, cx: &mut TestAppContext) {
    std::thread::sleep(Duration::from_millis(50));
    cx.run_until_parked();
    assert!(
        store.list().unwrap().instances.is_empty(),
        "nothing created"
    );
}

#[gpui_kit::test]
fn enter_commits_only_from_the_name_and_the_lists(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let app = open_dialog(cx, tmp.path());
    let model = instances_model(&app, cx);

    // Enter in the search box searches nothing and creates nothing.
    click(&app, cx, "version-search");
    press(&app, cx, "enter");
    assert!(shows(&app, cx, "dialog"));
    nothing_created(&store, cx);

    // Enter on a loader segment chooses that loader.
    let fabric = loader_segment_id(Some(LoaderKind::Fabric));
    tab_to(&app, cx, fabric);
    press(&app, cx, "enter");
    wait_name(&app, cx, "Fabric 1.21.1");
    assert!(shows(&app, cx, "dialog"));
    nothing_created(&store, cx);
    // Space too.
    let none = loader_segment_id(None);
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("shift-tab", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        element(&app, cx, none).and_then(|e| e.focused()),
        Some(true)
    );
    press(&app, cx, "space");
    wait_name(&app, cx, "1.21.1");
    nothing_created(&store, cx);

    // Enter on 取消 cancels.
    tab_to(&app, cx, "add-instance-cancel");
    press(&app, cx, "enter");
    assert!(!shows(&app, cx, "dialog"));
    nothing_created(&store, cx);

    // Enter in the version list creates.
    click(&app, cx, "instances-add");
    wait_name(&app, cx, "1.21.1");
    click(&app, cx, version_row_id("1.20.1"));
    wait_name(&app, cx, "1.20.1");
    press(&app, cx, "enter");
    assert!(!shows(&app, cx, "dialog"));
    settle(cx, |cx| selected(&model, cx).as_deref() == Some("1.20.1"));
}

#[gpui_kit::test]
fn enter_in_the_name_creates(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = open_dialog(cx, tmp.path());
    let model = instances_model(&app, cx);
    click(&app, cx, "add-instance-name");
    press(&app, cx, "ctrl-a");
    type_text(&app, cx, "Named");
    press(&app, cx, "enter");
    assert!(!shows(&app, cx, "dialog"));
    settle(cx, |cx| selected(&model, cx).as_deref() == Some("Named"));
}

/// A refresh that fails right after creating: the add dialog says what
/// happened, in place of the page's generic notice.
#[cfg(unix)]
#[gpui_kit::test]
fn a_failed_refresh_after_creating_is_reported_once(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = tempfile::tempdir().unwrap();
    let instances = DataDirs::new(tmp.path()).instances();
    std::fs::create_dir_all(&instances).unwrap();
    let app = open_dialog(cx, tmp.path());
    let model = instances_model(&app, cx);
    let mode = |m| std::fs::set_permissions(&instances, std::fs::Permissions::from_mode(m));
    // From here the directory takes new instances but cannot be listed.
    mode(0o300).unwrap();
    if std::fs::read_dir(&instances).is_ok() {
        // Permissions do not apply (root): nothing to simulate.
        mode(0o755).unwrap();
        return;
    }
    click(&app, cx, "add-instance-create");
    assert!(!shows(&app, cx, "dialog"));
    settle(cx, |cx| model.read_with(cx, |m, _| m.error().is_some()));
    cx.run_until_parked();
    let count = notification_count(&app, cx);
    let text = label_of(&app, cx, NOTICE_TEXT);
    mode(0o755).unwrap();
    assert_eq!(count, 1, "one notice for one failure");
    let text = text.expect("the notice's text");
    assert!(
        text.starts_with("Created “1.21.1”, but couldn’t read the instance list: "),
        "the add dialog's notice, not the page's: {text}"
    );
    refresh(&model, cx);
    assert!(model.read_with(cx, |m, _| m.entry("1.21.1").is_some()));
}

#[gpui_kit::test]
fn create_failure_is_reported(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    // A file where the instances directory belongs: nothing can be created.
    let instances = DataDirs::new(tmp.path()).instances();
    std::fs::create_dir_all(instances.parent().unwrap()).unwrap();
    std::fs::write(&instances, b"not a directory").unwrap();
    let app = boot_with(cx, tmp.path(), AppSettings::default(), catalog());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    let notifications = |cx: &mut TestAppContext| {
        cx.update_window(app.handle, |_, window, cx| window.notifications(cx).len())
            .unwrap()
    };

    click(&app, cx, "instances-add");
    wait_name(&app, cx, "1.21.1");
    click(&app, cx, "add-instance-create");
    assert!(!shows(&app, cx, "dialog"));
    settle(cx, |cx| notifications(cx) == 1);
    assert!(instances.is_file());
    assert!(model.read_with(cx, |m, _| m.instances().is_empty()));
}

#[gpui_kit::test]
fn the_list_scrolls_to_the_new_instance(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    for n in 0..40 {
        store.create(&format!("A {n:02}"), "1.21.1").unwrap();
    }
    let app = boot_with(cx, tmp.path(), AppSettings::default(), catalog());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    assert!(!shows(&app, cx, row_id("A 39")), "the list overflows");

    click(&app, cx, "instances-add");
    wait_name(&app, cx, "1.21.1");
    click(&app, cx, "add-instance-name");
    press(&app, cx, "ctrl-a");
    type_text(&app, cx, "Zed");
    click(&app, cx, "add-instance-create");
    settle(cx, |cx| selected(&model, cx).as_deref() == Some("Zed"));
    // Sorted last (never played, by name); scrolled into view.
    assert!(shows(&app, cx, row_id("Zed")));
}

// ---- Log tab ----

fn log_view(app: &Booted, cx: &mut TestAppContext) -> Entity<LogView> {
    cx.update(|cx| {
        let page = app.root.read(cx).instances().read(cx);
        page.detail()
            .read(cx)
            .log_view()
            .expect("log tab shown")
            .clone()
    })
}

#[gpui_kit::test]
fn the_english_log_toolbar_fits_the_smallest_window(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    InstanceStore::new(DataDirs::new(tmp.path()).instances())
        .create("Logs", "1.21.1")
        .unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "tab-log");
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    cx.run_until_parked();
    for id in [
        "log-filter-all",
        "log-filter-warn",
        "log-autoscroll",
        "log-copy",
        "log-open-folder",
    ] {
        let fits = cx
            .update_window(app.handle, |_, window, cx| {
                window.render_frame(cx);
                let pane = window.find("instance-detail").bounds();
                let e = window.find(id);
                let b = e.bounds();
                e.visible() && b.left() >= pane.left() && b.right() <= pane.right()
            })
            .unwrap();
        assert!(fits, "{id} is cut off at 960×600");
        assert!(inside_window(&app, cx, id), "{id} is outside the window");
    }
}

#[gpui_kit::test]
fn the_log_tab_shows_follows_and_copies(cx: &mut TestAppContext) {
    use gpui_kit::{ScrollDelta, point};

    let tmp = tempfile::tempdir().unwrap();
    InstanceStore::new(DataDirs::new(tmp.path()).instances())
        .create("Logs", "1.21.1")
        .unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "tab-log");
    let view = log_view(&app, cx);
    assert!(shows(&app, cx, "log-lines"));
    assert!(!shows(&app, cx, "log-omitted"));

    // Output arrives; the view follows it.
    let buffer = model.update(cx, |m, cx| m.log_buffer("Logs", cx));
    buffer.update(cx, |b, cx| {
        for n in 0..500 {
            let level = if n % 50 == 0 {
                LogLevel::Warn
            } else {
                LogLevel::Info
            };
            b.push(level, format!("line {n}"));
        }
        cx.notify();
    });
    cx.run_until_parked();
    assert!(shows(&app, cx, "log-lines"));
    assert!(view.read_with(cx, |v, _| v.autoscroll()));

    // Scrolling up leaves autoscroll off; the switch turns it back on.
    cx.update_window(app.handle, |_, window, cx| {
        window.scroll(
            "log-lines",
            ScrollDelta::Pixels(point(gpui_kit::px(0.), gpui_kit::px(400.))),
            cx,
        )
    })
    .unwrap();
    assert!(!view.read_with(cx, |v, _| v.autoscroll()), "scrolled up");
    click(&app, cx, "log-autoscroll");
    assert!(view.read_with(cx, |v, _| v.autoscroll()));

    // Scrolling back down to the end turns it on by itself.
    cx.update_window(app.handle, |_, window, cx| {
        window.scroll(
            "log-lines",
            ScrollDelta::Pixels(point(gpui_kit::px(0.), gpui_kit::px(400.))),
            cx,
        )
    })
    .unwrap();
    assert!(!view.read_with(cx, |v, _| v.autoscroll()));
    cx.update_window(app.handle, |_, window, cx| {
        window.scroll(
            "log-lines",
            ScrollDelta::Pixels(point(gpui_kit::px(0.), gpui_kit::px(-100000.))),
            cx,
        )
    })
    .unwrap();
    assert!(
        view.read_with(cx, |v, _| v.autoscroll()),
        "back at the bottom"
    );

    // The filter narrows the rows; copy takes the whole buffer anyway.
    click(&app, cx, "log-filter-warn");
    assert_eq!(view.read_with(cx, |v, cx| v.visible_len(cx)), 10);
    assert_eq!(view.read_with(cx, |v, _| v.filter()), LevelFilter::WarnUp);
    click(&app, cx, "log-copy");
    let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
    assert_eq!(copied.lines().count(), 500);
    assert!(copied.starts_with("line 0\nline 1\n"));
    click(&app, cx, "log-filter-all");
    assert_eq!(view.read_with(cx, |v, cx| v.visible_len(cx)), 500);

    // Overflow shows the omitted banner.
    buffer.update(cx, |b, cx| {
        for n in 0..25_000 {
            b.push(LogLevel::Info, format!("more {n}"));
        }
        cx.notify();
    });
    cx.run_until_parked();
    assert!(shows(&app, cx, "log-omitted"));
    let omitted = cx
        .update_window(app.handle, |_, window, _| {
            window.find("log-omitted").label().map(str::to_owned)
        })
        .unwrap();
    assert_eq!(
        omitted.as_deref(),
        Some("5500 earlier lines omitted. See the log file for the full output.")
    );

    // A saved crash log shows its file name.
    assert!(!shows(&app, cx, "log-crash-banner"));
    let crash = tmp.path().join("patina-logs/20260101-000000-crash.log");
    model.update(cx, |m, cx| m.set_crash_log("Logs", Some(crash), cx));
    assert!(shows(&app, cx, "log-crash-banner"));
    assert!(shows(&app, cx, "log-crash-open"));

    // The next launch starts clean.
    model.update(cx, |m, cx| {
        m.begin_log("Logs", cx);
    });
    assert_eq!(buffer.read_with(cx, |b, _| (b.len(), b.dropped())), (0, 0));
    assert!(!shows(&app, cx, "log-crash-banner"));
}

#[gpui_kit::test]
fn the_log_scrolls_by_keyboard_and_scrollbar(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    InstanceStore::new(DataDirs::new(tmp.path()).instances())
        .create("Logs", "1.21.1")
        .unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "tab-log");
    let view = log_view(&app, cx);
    let buffer = model.update(cx, |m, cx| m.log_buffer("Logs", cx));
    buffer.update(cx, |b, cx| {
        for n in 0..500 {
            b.push(LogLevel::Info, format!("line {n}"));
        }
        cx.notify();
    });
    cx.run_until_parked();
    let autoscroll = |cx: &mut TestAppContext| view.read_with(cx, |v, _| v.autoscroll());
    assert!(autoscroll(cx));

    // A click on the lines gives them the keyboard.
    click(&app, cx, "log-lines");
    assert_eq!(
        element(&app, cx, "log-lines").unwrap().focused(),
        Some(true)
    );

    press(&app, cx, "pageup");
    assert!(!autoscroll(cx), "a page up leaves the end");
    press(&app, cx, "pagedown");
    assert!(autoscroll(cx), "a page down returns to it");
    press(&app, cx, "home");
    assert!(!autoscroll(cx));
    assert!(shows(&app, cx, "log-lines"));
    press(&app, cx, "end");
    assert!(autoscroll(cx));

    // Dragging the scrollbar's thumb from the bottom to the top. The bar
    // shows once the list scrolls.
    cx.update_window(app.handle, |_, window, cx| {
        window.scroll(
            "log-lines",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(40.))),
            cx,
        )
    })
    .unwrap();
    press(&app, cx, "end");
    assert!(autoscroll(cx));
    let lines = element(&app, cx, "log-lines").unwrap().bounds();
    // The middle of the thumb's 6px width, 4px in from the edge.
    let x = lines.right() - px(8.);
    cx.update_window(app.handle, |_, window, cx| {
        window.drag(
            gpui_kit::point(x, lines.bottom() - px(16.)),
            gpui_kit::point(x, lines.top()),
            cx,
        )
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(app.handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert!(!autoscroll(cx), "dragged away from the end");
    cx.update_window(app.handle, |_, window, cx| {
        window.drag(
            gpui_kit::point(x, lines.top() + px(16.)),
            gpui_kit::point(x, lines.bottom() + px(50.)),
            cx,
        )
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(app.handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert!(autoscroll(cx), "dragged back to the end");
}

#[gpui_kit::test]
fn a_deleted_instance_forgets_its_log(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    store.create("Gone", "1.21.1").unwrap();
    store.create("Kept", "1.21.1").unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    model.update(cx, |m, cx| {
        for id in ["Gone", "Kept"] {
            m.log_buffer(id, cx);
            m.set_crash_log(id, Some(format!("{id}.log").into()), cx);
        }
    });
    std::fs::remove_dir_all(DataDirs::new(tmp.path()).instances().join("Gone")).unwrap();
    refresh(&model, cx);
    model.read_with(cx, |m, cx| {
        assert!(m.crash_log("Gone", cx).is_none());
        assert!(m.crash_log("Kept", cx).is_some());
        assert!(!m.has_log("Gone", cx));
        assert!(m.has_log("Kept", cx));
    });
}

// ---- Launch ----

use patina_app::launch::prelaunch_dialog::warning_id;
use patina_core::accounts::AccountError;
use patina_core::instance::Instance;
use patina_core::launch::{
    GameSession, KillHandle, LaunchAccount, LaunchCommand, PendingLaunch, SessionError,
    running_game, test_account,
};
use patina_core::prepare::PrepareWarning;

/// A fake game of instance `id`: `sh -c <script>` with the instance marker
/// the core's identity checks look for. Scripts that run on keep the shell
/// as the group leader (`… & wait`, never `exec`), so the core's stop and
/// its checks see the game as it would a real one.
fn fake_game(id: &str, script: &str) -> LaunchCommand {
    LaunchCommand {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            script.into(),
            "fake-game".into(),
            format!("-Dpatina.instance={id}"),
        ],
        cwd: std::env::temp_dir(),
    }
}

/// Launches fake games running `script`, with `warnings` found while
/// "preparing", for any account the store has; or fails every prepare with
/// `fail`. Counts the prepares.
struct FakeGames {
    script: String,
    warnings: Vec<PrepareWarning>,
    fail: Option<fn() -> SessionError>,
    prepares: std::sync::atomic::AtomicUsize,
    /// The Java settings of each instance prepared.
    seen: std::sync::Mutex<Vec<Option<patina_core::config::JavaSettings>>>,
}

impl FakeGames {
    fn running(script: &str) -> Arc<dyn LaunchBackend> {
        Self::warning(script, Vec::new())
    }

    fn warning(script: &str, warnings: Vec<PrepareWarning>) -> Arc<dyn LaunchBackend> {
        Arc::new(Self::with(script, warnings, None))
    }

    fn failing(fail: fn() -> SessionError) -> Arc<FakeGames> {
        Arc::new(Self::with("exit 0", Vec::new(), Some(fail)))
    }

    fn with(
        script: &str,
        warnings: Vec<PrepareWarning>,
        fail: Option<fn() -> SessionError>,
    ) -> Self {
        FakeGames {
            script: script.into(),
            warnings,
            fail,
            prepares: Default::default(),
            seen: Default::default(),
        }
    }

    fn seen(&self) -> Vec<Option<patina_core::config::JavaSettings>> {
        self.seen.lock().unwrap().clone()
    }

    fn prepares(&self) -> usize {
        self.prepares.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl LaunchBackend for FakeGames {
    fn launch_account(&self, _: &AccountStore, _: &str) -> Result<LaunchAccount, AccountError> {
        Ok(test_account("Steve"))
    }

    fn prepare(
        &self,
        inst: Instance,
        _: LaunchAccount,
        _: ProgressSink,
    ) -> futures::future::BoxFuture<'static, Result<PendingLaunch, SessionError>> {
        self.seen
            .lock()
            .unwrap()
            .push(inst.config.java_override.clone());
        self.prepares
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(fail) = self.fail {
            return futures::future::ready(Err(fail())).boxed();
        }
        let (cmd, warnings) = (fake_game(&inst.id, &self.script), self.warnings.clone());
        async move { GameSession::prepare_launch_with_command(&inst, cmd, warnings) }.boxed()
    }
}

/// Stops the fake games this test started when it ends, also when it
/// fails. Declare it after the app so it runs first. Each handle is a
/// game's own: it signals only that game, and only while its pid still
/// carries the game's identity.
struct StopGames {
    rt: tokio::runtime::Handle,
    games: std::cell::RefCell<Vec<KillHandle>>,
}

impl StopGames {
    fn new(rt: &tokio::runtime::Handle) -> Self {
        Self {
            rt: rt.clone(),
            games: Default::default(),
        }
    }

    fn add(&self, game: KillHandle) {
        self.games.borrow_mut().push(game);
    }
}

impl Drop for StopGames {
    fn drop(&mut self) {
        for game in self.games.take() {
            self.rt.block_on(game.kill()).ok();
        }
    }
}

/// Runs gpui until `done` holds, for at most 20 s (games take real time).
/// gpui's clock moves along with the real one, so its timers (the log
/// forwarding's batching, the poll of a game held elsewhere) fire.
fn settle_game(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    for _ in 0..2000 {
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the game did not get there within 20 s");
}

fn run_state(
    model: &Entity<InstancesModel>,
    cx: &mut TestAppContext,
    id: &str,
) -> Option<RunState> {
    model.read_with(cx, |m, cx| m.run_state(id, cx).cloned())
}

/// The stop handle of `id`'s game once this app runs it.
fn wait_running(model: &Entity<InstancesModel>, cx: &mut TestAppContext, id: &str) -> KillHandle {
    settle_game(cx, |cx| {
        matches!(run_state(model, cx, id), Some(RunState::Running { .. }))
    });
    match run_state(model, cx, id) {
        Some(RunState::Running { killer, .. }) => killer,
        state => panic!("not running: {state:?}"),
    }
}

/// The label of element `id` inside the open dialog.
fn dialog_label(
    app: &Booted,
    cx: &mut TestAppContext,
    id: impl Into<gpui_kit::ElementId>,
) -> String {
    let id = id.into();
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

fn failed_message(
    model: &Entity<InstancesModel>,
    cx: &mut TestAppContext,
    id: &str,
) -> Option<String> {
    match op_of(model, cx, id) {
        Some(OpState::Failed { message, .. }) => Some(message),
        _ => None,
    }
}

/// An account in the store, so launching gets past the account check.
fn with_account(dir: &Path) {
    std::fs::write(dir.join("accounts.toml"), ACCOUNT).unwrap();
}

#[gpui_kit::test]
fn second_launch_is_busy_with_display_name(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    store.rename(&a, "My Pack").unwrap();
    let b = store.create("Beta", "1.21.1").unwrap().id;
    store.rename(&b, "Other Pack").unwrap();
    with_account(tmp.path());
    let app = boot_games(cx, tmp.path(), FakeGames::running("sleep 30 & wait"));
    let games = StopGames::new(app._rt.handle());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);

    click(&app, cx, row_id(&a));
    click(&app, cx, "launch");
    games.add(wait_running(&model, cx, &a));
    // Running: 启动 is greyed and the model refuses a second launch.
    assert!(disabled(&app, cx, "launch"));
    assert!(shows(&app, cx, "instance-stop"));
    assert!(model.read_with(cx, |m, cx| m.is_busy(&a, cx)));
    assert!(!model.update(cx, |m, cx| m.launch(&a, cx)));
    assert!(op_of(&model, cx, &a).is_none());

    // Another operation holds Beta (here: its lock, held by this test),
    // and no game runs: the launch fails, naming Beta by its display name,
    // and may be retried.
    let lock = store.lock(&b).unwrap();
    click(&app, cx, row_id(&b));
    click(&app, cx, "launch");
    settle_game(cx, |cx| failed_message(&model, cx, &b).is_some());
    let message = failed_message(&model, cx, &b).unwrap();
    assert_eq!(message, "“Other Pack” is busy with another operation");
    assert!(!message.contains(&b), "{message}");
    // Looking for a game finds none; the failure stays, with retry.
    settle_game(cx, |cx| {
        model.read_with(cx, |m, cx| !m.is_following(&b, cx))
    });
    assert!(run_state(&model, cx, &b).is_none());
    assert!(failed_message(&model, cx, &b).is_some());
    assert!(shows(&app, cx, "instance-op-retry"));
    drop(lock);

    // Someone else launched Beta's game (here: a session this test holds).
    // The launch finds the instance busy, then the game: the header shows
    // it running elsewhere, with no failure and no retry.
    let beta = store.load(&b).unwrap();
    let held = app._rt.block_on(async {
        let pending = GameSession::prepare_launch_with_command(
            &beta,
            fake_game(&b, "sleep 30 & wait"),
            vec![],
        )
        .unwrap();
        pending.launch(None).await.unwrap()
    });
    games.add(held.killer());
    click(&app, cx, "launch");
    settle_game(cx, |cx| {
        matches!(
            run_state(&model, cx, &b),
            Some(RunState::HeldElsewhere { pid }) if pid == held.pid()
        )
    });
    assert!(op_of(&model, cx, &b).is_none(), "the failed launch is gone");
    assert!(!shows(&app, cx, "instance-op-retry"));
    assert!(shows(&app, cx, "instance-elsewhere"));
    assert!(shows(&app, cx, "instance-stop"));
    assert!(disabled(&app, cx, "launch"));
    // Alpha's game was not disturbed.
    assert!(matches!(
        run_state(&model, cx, &a),
        Some(RunState::Running { .. })
    ));
    drop(held);
}

#[gpui_kit::test]
fn a_launch_does_not_announce_changed_instances(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let app = boot_games(cx, tmp.path(), FakeGames::running("sleep 30 & wait"));
    let games = StopGames::new(app._rt.handle());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    let changed = Rc::new(std::cell::Cell::new(0));
    let _count = {
        let changed = changed.clone();
        cx.update(|cx| {
            cx.subscribe(&model, move |_, _: &InstancesChanged, _| {
                changed.set(changed.get() + 1)
            })
        })
    };

    click(&app, cx, "launch");
    games.add(wait_running(&model, cx, &a));
    settle_game(cx, |cx| op_of(&model, cx, &a).is_none());
    assert_eq!(changed.get(), 0, "no status-bar disk scan per launch");
}

#[gpui_kit::test]
fn warnings_dialog_cancel_releases_locks(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let store = InstanceStore::new(data.instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let warning = PrepareWarning::LowMemory {
        requested_mib: 4096,
        available_mib: 1024,
    };
    let games_backend = FakeGames::warning("sleep 30 & wait", vec![warning.clone()]);
    let app = boot_games(cx, tmp.path(), games_backend);
    let games = StopGames::new(app._rt.handle());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);

    click(&app, cx, "launch");
    settle_game(cx, |cx| shows(&app, cx, "dialog"));
    assert_eq!(dialog_label(&app, cx, "prelaunch-title"), "Play “Alpha”?");
    assert_eq!(dialog_label(&app, cx, warning_id(0)), warning.to_string());
    assert!(matches!(
        run_state(&model, cx, &a),
        Some(RunState::Starting)
    ));
    assert!(
        store.lock(&a).is_err(),
        "the prepared launch holds the instance"
    );

    // 取消: nothing starts and the locks are free again.
    dialog_click(&app, cx, "cancel");
    assert!(!shows(&app, cx, "dialog"));
    assert!(run_state(&model, cx, &a).is_none());
    assert!(!disabled(&app, cx, "launch"));
    drop(store.lock(&a).expect("the instance lock is released"));
    drop(
        data.lock_version_exclusive("1.21.1")
            .expect("the version lock is released"),
    );
    let inst = store.load(&a).unwrap();
    assert!(running_game(&inst).is_none());
    assert!(
        !inst.dir.join("patina-logs/current.log").exists(),
        "never spawned"
    );

    // 继续启动 starts it.
    click(&app, cx, "launch");
    settle_game(cx, |cx| shows(&app, cx, "dialog"));
    dialog_click(&app, cx, "ok");
    games.add(wait_running(&model, cx, &a));
    assert!(!shows(&app, cx, "dialog"));
    assert!(running_game(&inst).is_some());
}

#[gpui_kit::test]
fn crash_shows_saved_log(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let app = boot_games(cx, tmp.path(), FakeGames::running("echo boom; exit 3"));
    let model = instances_model(&app, cx);
    list_settled(&model, cx);

    click(&app, cx, "launch");
    settle_game(cx, |cx| {
        matches!(run_state(&model, cx, &a), Some(RunState::Exited(_)))
    });
    let Some(RunState::Exited(exit)) = run_state(&model, cx, &a) else {
        unreachable!()
    };
    assert!(exit.crashed);
    assert_eq!(exit.code, Some(3));
    let saved = exit.saved_log.clone().expect("the log is saved");
    assert!(std::fs::read_to_string(&saved).unwrap().contains("boom"));
    assert_eq!(
        model.read_with(cx, |m, cx| m.crash_log(&a, cx).map(Path::to_owned)),
        Some(saved)
    );
    // The header says so, with the folder one click away.
    let text = element(&app, cx, "instance-crash-text").expect("the crash notice");
    assert_eq!(
        text.label(),
        Some("The game quit unexpectedly. Its log was saved.")
    );
    assert!(shows(&app, cx, "instance-crash"));
    assert!(shows(&app, cx, "instance-crash-open"));
    assert!(!disabled(&app, cx, "launch"), "it can be launched again");
    // So does the log tab, which got the run's output.
    click(&app, cx, "tab-log");
    assert!(shows(&app, cx, "log-crash-banner"));
    let buffer = model.update(cx, |m, cx| m.log_buffer(&a, cx));
    settle_game(cx, |cx| {
        buffer.read_with(cx, |b, _| b.text().contains("boom"))
    });
    // Dismissing clears the header, not the log tab.
    click(&app, cx, "instance-crash-dismiss");
    assert!(!shows(&app, cx, "instance-crash"));
    assert!(run_state(&model, cx, &a).is_none());
    assert!(shows(&app, cx, "log-crash-banner"));
}

#[gpui_kit::test]
fn force_stop_requires_confirmation_and_is_not_a_crash(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let app = boot_games(cx, tmp.path(), FakeGames::running("sleep 30 & wait"));
    let games = StopGames::new(app._rt.handle());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    let inst = store.load(&a).unwrap();

    click(&app, cx, "launch");
    games.add(wait_running(&model, cx, &a));

    // Asks first; 取消 leaves the game running.
    click(&app, cx, "instance-stop");
    assert!(shows(&app, cx, "dialog"));
    assert_eq!(dialog_label(&app, cx, "stop-title"), "Force stop “Alpha”?");
    dialog_click(&app, cx, "cancel");
    assert!(!shows(&app, cx, "dialog"));
    std::thread::sleep(Duration::from_millis(300));
    cx.run_until_parked();
    assert!(matches!(
        run_state(&model, cx, &a),
        Some(RunState::Running { .. })
    ));
    assert!(running_game(&inst).is_some());

    // 强制关闭 stops it; a stop the user asked for is no crash.
    app.language(Language::ZhCn);
    click(&app, cx, "instance-stop");
    assert_eq!(dialog_label(&app, cx, "stop-title"), "强制关闭\"Alpha\"？");
    dialog_click(&app, cx, "ok");
    settle_game(cx, |cx| run_state(&model, cx, &a).is_none());
    assert!(running_game(&inst).is_none());
    assert!(model.read_with(cx, |m, cx| m.crash_log(&a, cx).is_none()));
    assert!(!shows(&app, cx, "instance-crash"));
    assert!(!shows(&app, cx, "instance-stop"));
    let logs = std::fs::read_dir(inst.dir.join("patina-logs")).unwrap();
    let crash_logs = logs
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("crash"))
        .count();
    assert_eq!(crash_logs, 0, "no crash log saved");
}

#[gpui_kit::test]
fn reattach_on_startup_restores_running_state(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    let inst = store.load(&a).unwrap();
    // An earlier launcher session started the game, then went away: its
    // session is dropped, the game and its running record stay.
    let earlier = patina_app::runtime::start();
    let killer = earlier.block_on(async {
        let pending = GameSession::prepare_launch_with_command(
            &inst,
            fake_game(&a, "sleep 30 & wait"),
            vec![],
        )
        .unwrap();
        let session = pending.launch(None).await.unwrap();
        let killer = session.killer();
        drop(session);
        killer
    });
    // On the earlier runtime, which outlives the app (declared before it).
    let games = StopGames::new(earlier.handle());
    games.add(killer);
    assert!(running_game(&inst).is_some());

    // A new start of the app finds it after the first list load.
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    wait_running(&model, cx, &a);
    assert!(matches!(
        run_state(&model, cx, &a),
        Some(RunState::Running {
            reattached: true,
            ..
        })
    ));
    assert!(disabled(&app, cx, "launch"));

    // And it can be stopped from here.
    click(&app, cx, "instance-stop");
    dialog_click(&app, cx, "ok");
    settle_game(cx, |cx| run_state(&model, cx, &a).is_none());
    assert!(running_game(&inst).is_none());
    assert!(model.read_with(cx, |m, cx| m.crash_log(&a, cx).is_none()));
    drop(app);
    drop(games);
}

#[gpui_kit::test]
async fn no_account_prompts_to_accounts_page(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let store = InstanceStore::new(data.instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    // The real launch backend: it is never reached without an account.
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    let shell = cx.update(|cx| app.root.read(cx).shell().clone());

    click(&app, cx, "launch");
    settle(cx, |cx| failed_message(&model, cx, &a).is_some());
    assert_eq!(
        failed_message(&model, cx, &a).as_deref(),
        Some("No account to play with yet")
    );
    assert!(run_state(&model, cx, &a).is_none());
    assert!(!shows(&app, cx, "instance-op-retry"));
    let fix = element(&app, cx, "instance-op-fix").expect("a way to add one");
    assert_eq!(fix.label(), Some("Add account"));
    click(&app, cx, "instance-op-fix");
    assert_eq!(shell.read_with(cx, |s, _| s.nav()), Nav::Accounts);
    assert!(op_of(&model, cx, &a).is_none());

    // A Microsoft account cannot play yet: the same way out.
    std::fs::write(tmp.path().join("accounts.toml"), ACCOUNT).unwrap();
    cx.update(|cx| {
        Core::with_accounts(cx, move |accounts| {
            *accounts = AccountStore::load(&data).map_err(|e| LoadError::Failed(Arc::new(e)));
        })
    })
    .await
    .unwrap();
    shell.update(cx, |s, cx| s.navigate(Nav::Instances, cx));
    settle(cx, |cx| shows(&app, cx, "launch"));
    click(&app, cx, "launch");
    settle(cx, |cx| failed_message(&model, cx, &a).is_some());
    assert_eq!(
        failed_message(&model, cx, &a).as_deref(),
        Some("Microsoft sign-in isn’t available yet")
    );
    assert!(shows(&app, cx, "instance-op-fix"));
    click(&app, cx, "instance-op-fix");
    assert_eq!(shell.read_with(cx, |s, _| s.nav()), Nav::Accounts);
}

#[gpui_kit::test]
fn external_game_is_found_on_refresh(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    let app = boot(cx, tmp.path(), AppSettings::default());
    let games = StopGames::new(app._rt.handle());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    assert!(run_state(&model, cx, &a).is_none());

    // Another process starts the game after the app has loaded (here: a
    // session this test holds).
    let inst = store.load(&a).unwrap();
    let held = app._rt.block_on(async {
        let pending = GameSession::prepare_launch_with_command(
            &inst,
            fake_game(&a, "sleep 30 & wait"),
            vec![],
        )
        .unwrap();
        pending.launch(None).await.unwrap()
    });
    games.add(held.killer());
    // The next refresh (the window becoming active) finds it.
    refresh(&model, cx);
    settle_game(cx, |cx| {
        matches!(
            run_state(&model, cx, &a),
            Some(RunState::HeldElsewhere { pid }) if pid == held.pid()
        )
    });
    assert!(shows(&app, cx, "instance-elsewhere"));
    assert!(disabled(&app, cx, "launch"));
    // Its log is the other process's; the log tab says so.
    click(&app, cx, "tab-log");
    assert!(shows(&app, cx, "log-elsewhere"));

    // Once the other session lets go, this app takes the game over.
    drop(held);
    wait_running(&model, cx, &a);
    assert!(matches!(
        run_state(&model, cx, &a),
        Some(RunState::Running {
            reattached: true,
            ..
        })
    ));
    assert!(!shows(&app, cx, "instance-elsewhere"));
    assert!(!shows(&app, cx, "log-elsewhere"));
}

#[gpui_kit::test]
fn failed_prepare_returns_to_idle_with_retry(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let backend = FakeGames::failing(|| SessionError::Io {
        path: "/nowhere".into(),
        error: "disk on fire".into(),
    });
    let app = boot_games(cx, tmp.path(), backend.clone());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);

    click(&app, cx, "launch");
    settle_game(cx, |cx| failed_message(&model, cx, &a).is_some());
    assert!(
        failed_message(&model, cx, &a)
            .unwrap()
            .contains("disk on fire")
    );
    assert!(run_state(&model, cx, &a).is_none(), "back to idle");
    assert!(!disabled(&app, cx, "launch"));
    assert!(!shows(&app, cx, "instance-op-fix"));
    assert_eq!(backend.prepares(), 1);

    // Retry launches again.
    click(&app, cx, "instance-op-retry");
    settle_game(cx, |cx| {
        backend.prepares() == 2 && failed_message(&model, cx, &a).is_some()
    });
    assert!(run_state(&model, cx, &a).is_none());
}

#[gpui_kit::test]
fn loader_needing_reinstall_offers_reinstall(cx: &mut TestAppContext) {
    use patina_core::prepare::PrepareError;

    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    set_loader(&store, &a, ComponentUid::Fabric);
    with_account(tmp.path());
    let backend = FakeGames::failing(|| {
        SessionError::Prepare(PrepareError::LoaderNeedsReinstall {
            kind: LoaderKind::Fabric,
            version: "1.0".into(),
        })
    });
    let app = boot_games(cx, tmp.path(), backend);
    let model = instances_model(&app, cx);
    list_settled(&model, cx);

    click(&app, cx, "launch");
    settle_game(cx, |cx| failed_message(&model, cx, &a).is_some());
    assert!(run_state(&model, cx, &a).is_none());
    let fix = element(&app, cx, "instance-op-fix").expect("a reinstall button");
    assert_eq!(fix.label(), Some("Reinstall"));
    assert!(
        !shows(&app, cx, "instance-op-retry"),
        "retrying would fail again"
    );
    app.language(Language::ZhCn);
    let fix = element(&app, cx, "instance-op-fix").unwrap();
    assert_eq!(fix.label(), Some("重新安装"));

    // It reinstalls the loader (the offline core fails that at once).
    click(&app, cx, "instance-op-fix");
    assert_eq!(
        op_of(&model, cx, &a).map(|op| op.kind()),
        Some(OpKind::Reinstall)
    );
    settle_game(cx, |cx| {
        matches!(
            op_of(&model, cx, &a),
            Some(OpState::Failed {
                kind: OpKind::Reinstall,
                ..
            })
        )
    });
}

#[gpui_kit::test]
fn the_reinstall_fix_goes_through_version_ops(cx: &mut TestAppContext) {
    use patina_core::prepare::PrepareError;

    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Alpha", "1.21.1").unwrap().id;
    set_loader(&store, &a, ComponentUid::Fabric);
    with_account(tmp.path());
    let games = FakeGames::failing(|| {
        SessionError::Prepare(PrepareError::LoaderNeedsReinstall {
            kind: LoaderKind::Fabric,
            version: "1.0".into(),
        })
    });
    let ops = Arc::new(FakeVersionOps::new(store.clone()));
    let app = {
        let (games, ops) = (games.clone(), ops.clone());
        boot_core(cx, tmp.path(), AppSettings::default(), move |rt, data| {
            testing::core_with_launch_and_ops(rt, data, Arc::new(FakeCatalog::new()), games, ops)
        })
    };
    let model = instances_model(&app, cx);
    list_settled(&model, cx);

    click(&app, cx, "launch");
    settle_game(cx, |cx| failed_message(&model, cx, &a).is_some());
    assert_eq!(games.prepares(), 1);
    click(&app, cx, "instance-op-fix");
    // The fake reinstall succeeds; the launch then runs again.
    settle_game(cx, |_| games.prepares() == 2);
    assert_eq!(ops.calls(), vec![OpCall::Reinstall { id: a.clone() }]);
}

// --- versions tab ---------------------------------------------------------

use patina_app::core_bridge::testing::{FakeVersionOps, OpCall};
use patina_core::loader::VersionPick;
use patina_core::loader::ops::LoaderAction;

/// Instance `Pack` on Minecraft `mc` with Fabric `version`, a booted app
/// whose loader changes go to the returned fake, the versions tab open.
fn versions_app(
    cx: &mut TestAppContext,
    dir: &Path,
    mc: &str,
    version: &str,
) -> (Booted, Entity<InstancesModel>, Arc<FakeVersionOps>) {
    versions_app_with(cx, dir, mc, Some(version))
}

/// [`versions_app`] with Fabric `loader` or, with `None`, no loader.
fn versions_app_with(
    cx: &mut TestAppContext,
    dir: &Path,
    mc: &str,
    loader: Option<&str>,
) -> (Booted, Entity<InstancesModel>, Arc<FakeVersionOps>) {
    let store = InstanceStore::new(DataDirs::new(dir).instances());
    store.create("Pack", mc).unwrap();
    if let Some(version) = loader {
        let mut inst = store.load("Pack").unwrap();
        inst.config
            .set_loader(Some(Component {
                uid: ComponentUid::Fabric,
                version: version.into(),
                profile: None,
            }))
            .unwrap();
        store.save(&inst).unwrap();
    }
    let ops = Arc::new(FakeVersionOps::new(store));
    let app = {
        let ops = ops.clone();
        boot_core(cx, dir, AppSettings::default(), move |rt, data| {
            testing::core_with_ops(rt, data, catalog(), ops)
        })
    };
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "tab-versions");
    assert!(shows(&app, cx, "versions-row-minecraft"));
    (app, model, ops)
}

fn label_of(
    app: &Booted,
    cx: &mut TestAppContext,
    id: impl Into<gpui_kit::ElementId>,
) -> Option<String> {
    element(app, cx, id).and_then(|e| e.label().map(str::to_owned))
}

fn wait_calls(ops: &FakeVersionOps, cx: &mut TestAppContext, n: usize) {
    settle(cx, |_| ops.calls().len() >= n);
    // The operation ends and the list refreshes.
    cx.run_until_parked();
}

#[gpui_kit::test]
fn repair_calls_reinstall(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, model, ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.5");

    // The Minecraft row has no repair; the loader row does.
    assert!(!shows(&app, cx, "versions-repair"));
    click(&app, cx, "versions-row-loader");
    click(&app, cx, "versions-repair");
    wait_calls(&ops, cx, 1);
    assert_eq!(ops.calls(), [OpCall::Reinstall { id: "Pack".into() }]);
    settle(cx, |cx| op_of(&model, cx, "Pack").is_none());
}

#[gpui_kit::test]
fn the_overview_walks_the_disk_only_when_the_size_may_have_changed(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, model, ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.5");
    let detail = cx.update(|cx| app.root.read(cx).instances().read(cx).detail().clone());
    let stats = |cx: &mut TestAppContext| detail.read_with(cx, |d, _| d.stats("Pack").cloned());
    settle(cx, |cx| stats(cx).is_some());
    let first = stats(cx).unwrap();
    assert_eq!(first.mods, Some(0));

    // Another program adds a mod and a large file: a refresh counts the mod
    // but does not walk the whole directory again.
    let game = InstanceStore::new(DataDirs::new(tmp.path()).instances())
        .load("Pack")
        .unwrap()
        .game_dir();
    std::fs::create_dir_all(game.join("mods")).unwrap();
    std::fs::write(game.join("mods/a.jar"), [0u8; 10]).unwrap();
    std::fs::write(game.join("big.bin"), vec![0u8; 1 << 20]).unwrap();
    refresh(&model, cx);
    settle(cx, |cx| stats(cx).is_some_and(|s| s.mods == Some(1)));
    assert_eq!(stats(cx).unwrap().size, first.size, "no walk");

    // One of this app's operations on it walks it at once.
    click(&app, cx, "versions-row-loader");
    click(&app, cx, "versions-repair");
    wait_calls(&ops, cx, 1);
    settle(cx, |cx| {
        stats(cx).is_some_and(|s| s.size >= first.size + (1 << 20))
    });
}

#[gpui_kit::test]
fn actions_disabled_while_running(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, model, ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.5");
    click(&app, cx, "versions-row-loader");
    for id in [
        "versions-change-loader-version",
        "versions-switch-loader",
        "versions-repair",
        "versions-remove-loader",
    ] {
        assert!(!disabled(&app, cx, id), "{id} is enabled when idle");
    }
    assert!(!shows(&app, cx, "versions-reason"));

    model.update(cx, |m, cx| m.set_run_state("Pack", Some(elsewhere()), cx));
    for id in [
        "versions-change-loader-version",
        "versions-switch-loader",
        "versions-repair",
        "versions-remove-loader",
    ] {
        assert!(disabled(&app, cx, id), "{id} is disabled while running");
    }
    assert_eq!(
        label_of(&app, cx, "versions-reason").as_deref(),
        Some("The game is running")
    );
    click(&app, cx, "versions-repair");
    assert!(ops.calls().is_empty(), "a disabled button does nothing");

    // Also while an operation runs.
    model.update(cx, |m, cx| m.set_run_state("Pack", None, cx));
    let (gate, wait) = oneshot::channel::<()>();
    let started = model.update(cx, |m, cx| {
        m.start_op(
            "Pack",
            OpKind::Reinstall,
            move |_| {
                async move {
                    wait.await.ok();
                    Ok(())
                }
                .boxed()
            },
            |_, (), _| {},
            None,
            cx,
        )
    });
    assert!(started);
    cx.run_until_parked();
    assert!(disabled(&app, cx, "versions-repair"));
    assert_eq!(
        label_of(&app, cx, "versions-reason").as_deref(),
        Some("An operation is in progress")
    );
    click(&app, cx, "versions-row-minecraft");
    assert!(disabled(&app, cx, "versions-change-mc"));
    gate.send(()).unwrap();
    settle(cx, |cx| !disabled(&app, cx, "versions-change-mc"));
    assert!(ops.calls().is_empty());
}

#[gpui_kit::test]
fn switch_loader_requires_confirmation(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, ops) = versions_app(cx, tmp.path(), "1.20.1", "0.14.0");
    click(&app, cx, "versions-row-loader");
    click(&app, cx, "versions-switch-loader");
    assert!(shows(&app, cx, "dialog"));
    assert!(
        disabled(&app, cx, "versions-dialog-confirm"),
        "nothing chosen yet"
    );

    click(&app, cx, loader_segment_id(Some(LoaderKind::Forge)));
    settle(cx, |cx| chosen(&app, cx, loader_row_id("47.3.0")));
    assert!(!disabled(&app, cx, "versions-dialog-confirm"));
    click(&app, cx, "versions-dialog-confirm");
    // The pick is not the answer: a confirmation comes first.
    assert!(shows(&app, cx, "dialog"));
    assert_eq!(
        label_of(&app, cx, "versions-confirm-title").as_deref(),
        Some("Switch loader to Forge?")
    );
    cx.run_until_parked();
    assert!(ops.calls().is_empty(), "nothing runs before the answer");

    // Cancel changes nothing.
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click("cancel", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!shows(&app, cx, "dialog"));
    assert!(ops.calls().is_empty());

    // Confirm installs with `replace_existing`.
    click(&app, cx, "versions-switch-loader");
    click(&app, cx, loader_segment_id(Some(LoaderKind::Forge)));
    settle(cx, |cx| chosen(&app, cx, loader_row_id("47.3.0")));
    click(&app, cx, "versions-dialog-confirm");
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click("ok", cx);
    })
    .unwrap();
    wait_calls(&ops, cx, 1);
    assert_eq!(
        ops.calls(),
        [OpCall::InstallLoader {
            id: "Pack".into(),
            kind: LoaderKind::Forge,
            pick: VersionPick::Exact("47.3.0".into()),
            replace_existing: true,
        }]
    );
}

/// Changes the Minecraft version of the open instance to 1.21 in the
/// dialog.
fn change_game_version_to_1_21(app: &Booted, cx: &mut TestAppContext) {
    click(app, cx, "versions-row-minecraft");
    click(app, cx, "versions-change-mc");
    assert!(shows(app, cx, "dialog"));
    assert!(
        disabled(app, cx, "versions-dialog-confirm"),
        "the current version is not a change"
    );
    settle(cx, |cx| shows(app, cx, version_row_id("1.21")));
    click(app, cx, version_row_id("1.21"));
    assert!(!disabled(app, cx, "versions-dialog-confirm"));
    click(app, cx, "versions-dialog-confirm");
}

#[gpui_kit::test]
fn incompatible_offers_replace_only_when_possible(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.5");

    // A loader some version of which supports 1.21: "换成推荐版" is offered.
    ops.then(Err(LoaderError::Incompatible {
        kind: LoaderKind::Fabric,
        version: "0.16.5".into(),
        can_replace: true,
    }));
    change_game_version_to_1_21(&app, cx);
    wait_calls(&ops, cx, 1);
    settle(cx, |cx| shows(&app, cx, "incompatible-title"));
    assert_eq!(
        label_of(&app, cx, "incompatible-title").as_deref(),
        Some("Fabric 0.16.5 doesn’t support Minecraft 1.21")
    );
    assert!(shows(&app, cx, "incompatible-replace"));
    assert!(shows(&app, cx, "incompatible-remove"));
    assert!(shows(&app, cx, "incompatible-cancel"));
    click(&app, cx, "incompatible-replace");
    wait_calls(&ops, cx, 2);

    // None does: only "移除加载器" and "取消".
    ops.then(Err(LoaderError::Incompatible {
        kind: LoaderKind::Fabric,
        version: "0.16.5".into(),
        can_replace: false,
    }));
    settle(cx, |cx| !disabled(&app, cx, "versions-change-mc"));
    change_game_version_to_1_21(&app, cx);
    wait_calls(&ops, cx, 3);
    settle(cx, |cx| shows(&app, cx, "incompatible-title"));
    assert!(!shows(&app, cx, "incompatible-replace"));
    click(&app, cx, "incompatible-remove");
    wait_calls(&ops, cx, 4);

    // "取消" runs nothing.
    ops.then(Err(LoaderError::Incompatible {
        kind: LoaderKind::Fabric,
        version: "0.16.5".into(),
        can_replace: true,
    }));
    settle(cx, |cx| !disabled(&app, cx, "versions-change-mc"));
    change_game_version_to_1_21(&app, cx);
    wait_calls(&ops, cx, 5);
    settle(cx, |cx| shows(&app, cx, "incompatible-cancel"));
    click(&app, cx, "incompatible-cancel");
    cx.run_until_parked();
    assert!(!shows(&app, cx, "dialog"));

    let set = |action| OpCall::SetMcVersion {
        id: "Pack".into(),
        mc: "1.21".into(),
        action,
    };
    assert_eq!(
        ops.calls(),
        [
            set(None),
            set(Some(LoaderAction::Replace)),
            set(None),
            set(Some(LoaderAction::Remove)),
            set(None),
        ]
    );
}

#[gpui_kit::test]
fn removing_the_loader_asks_first(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.5");
    click(&app, cx, "versions-row-loader");
    click(&app, cx, "versions-remove-loader");
    assert_eq!(
        label_of(&app, cx, "versions-confirm-title").as_deref(),
        Some("Remove Fabric 0.16.5 from “Pack”?")
    );
    cx.run_until_parked();
    assert!(ops.calls().is_empty());
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click("ok", cx);
    })
    .unwrap();
    wait_calls(&ops, cx, 1);
    assert_eq!(ops.calls(), [OpCall::RemoveLoader { id: "Pack".into() }]);
}

fn notification_count(app: &Booted, cx: &mut TestAppContext) -> usize {
    cx.update_window(app.handle, |_, window, cx| window.notifications(cx).len())
        .unwrap()
}

#[gpui_kit::test]
fn the_java_line_is_visible_on_the_minecraft_row(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = DataDirs::new(tmp.path()).versions().join("1.21.1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("1.21.1.json"),
        r#"{"javaVersion":{"majorVersion":21}}"#,
    )
    .unwrap();
    let (app, _model, _ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.5");
    settle(cx, |cx| shows(&app, cx, "versions-java"));
    assert_eq!(
        label_of(&app, cx, "versions-java").as_deref(),
        Some("Needs Java 21")
    );
}

#[gpui_kit::test]
fn the_loader_row_is_tagged_recommended_or_latest(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, _ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.5");
    settle(cx, |cx| shows(&app, cx, "versions-tag-recommended"));
    assert!(!shows(&app, cx, "versions-tag-latest"));
}

#[gpui_kit::test]
fn the_latest_loader_row_is_tagged_latest(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, _ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.9");
    settle(cx, |cx| shows(&app, cx, "versions-tag-latest"));
    assert!(!shows(&app, cx, "versions-tag-recommended"));
}

#[gpui_kit::test]
fn the_loader_version_dialog_shows_only_that_loader(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.5");
    click(&app, cx, "versions-row-loader");
    click(&app, cx, "versions-change-loader-version");
    settle(cx, |cx| shows(&app, cx, loader_row_id("0.16.9")));
    assert!(chosen(&app, cx, loader_row_id("0.16.5")), "the current one");
    assert!(!shows(
        &app,
        cx,
        loader_segment_id(Some(LoaderKind::Fabric))
    ));
    assert!(!shows(&app, cx, loader_segment_id(None)));
    assert!(disabled(&app, cx, "versions-dialog-confirm"));
    click(&app, cx, loader_row_id("0.16.9"));
    click(&app, cx, "versions-dialog-confirm");
    wait_calls(&ops, cx, 1);
    assert_eq!(
        ops.calls(),
        [OpCall::InstallLoader {
            id: "Pack".into(),
            kind: LoaderKind::Fabric,
            pick: VersionPick::Exact("0.16.9".into()),
            replace_existing: false,
        }]
    );
}

#[gpui_kit::test]
fn a_vanilla_instance_installs_a_loader(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, model, ops) = versions_app_with(cx, tmp.path(), "1.21.1", None);
    assert!(!shows(&app, cx, "versions-row-loader"));
    assert!(shows(&app, cx, "versions-hint"));
    click(&app, cx, "versions-install");
    assert!(disabled(&app, cx, "versions-dialog-confirm"));
    click(&app, cx, loader_segment_id(Some(LoaderKind::Fabric)));
    settle(cx, |cx| chosen(&app, cx, loader_row_id("0.16.5")));
    click(&app, cx, "versions-dialog-confirm");
    wait_calls(&ops, cx, 1);
    assert_eq!(
        ops.calls(),
        [OpCall::InstallLoader {
            id: "Pack".into(),
            kind: LoaderKind::Fabric,
            pick: VersionPick::Exact("0.16.5".into()),
            replace_existing: false,
        }]
    );

    // Busy: the button is disabled and says why.
    model.update(cx, |m, cx| m.set_run_state("Pack", Some(elsewhere()), cx));
    assert!(disabled(&app, cx, "versions-install"));
    assert_eq!(
        label_of(&app, cx, "versions-install-reason").as_deref(),
        Some("The game is running")
    );
}

#[gpui_kit::test]
fn a_busy_instance_takes_no_picked_change(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, model, ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.5");
    click(&app, cx, "versions-row-minecraft");
    click(&app, cx, "versions-change-mc");
    settle(cx, |cx| shows(&app, cx, version_row_id("1.21")));
    click(&app, cx, version_row_id("1.21"));
    assert!(!disabled(&app, cx, "versions-dialog-confirm"));

    // The game starts while the dialog is open.
    model.update(cx, |m, cx| m.set_run_state("Pack", Some(elsewhere()), cx));
    cx.run_until_parked();
    assert!(disabled(&app, cx, "versions-dialog-confirm"));
    press(&app, cx, "enter");
    click(&app, cx, "versions-dialog-confirm");
    cx.run_until_parked();
    assert!(ops.calls().is_empty(), "{:?}", ops.calls());
}

#[gpui_kit::test]
fn a_refused_rerun_says_why(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, model, ops) = versions_app(cx, tmp.path(), "1.21.1", "0.16.5");
    ops.then(Err(LoaderError::Incompatible {
        kind: LoaderKind::Fabric,
        version: "0.16.5".into(),
        can_replace: true,
    }));
    change_game_version_to_1_21(&app, cx);
    settle(cx, |cx| shows(&app, cx, "incompatible-replace"));
    assert_eq!(notification_count(&app, cx), 0);

    model.update(cx, |m, cx| m.set_run_state("Pack", Some(elsewhere()), cx));
    click(&app, cx, "incompatible-replace");
    settle(cx, |cx| notification_count(&app, cx) == 1);
    assert_eq!(ops.calls().len(), 1, "the rerun did not start");
}

#[gpui_kit::test]
fn enter_in_the_switch_list_opens_the_confirmation(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, ops) = versions_app(cx, tmp.path(), "1.20.1", "0.14.0");
    click(&app, cx, "versions-row-loader");
    click(&app, cx, "versions-switch-loader");
    assert!(!shows(
        &app,
        cx,
        loader_segment_id(Some(LoaderKind::Fabric))
    ));
    assert!(!shows(&app, cx, loader_segment_id(None)));
    click(&app, cx, loader_segment_id(Some(LoaderKind::Forge)));
    settle(cx, |cx| chosen(&app, cx, loader_row_id("47.3.0")));

    // Enter on a control that is not the list is that control's own key.
    tab_to(&app, cx, "versions-dialog-cancel");
    press(&app, cx, "shift-tab");
    assert!(shows(&app, cx, "versions-dialog-confirm"));
    assert!(!shows(&app, cx, "versions-confirm-title"));

    // Enter in the list closes the picker, then asks.
    click(&app, cx, loader_row_id("47.4.0"));
    press(&app, cx, "enter");
    assert!(shows(&app, cx, "versions-confirm-title"));
    assert!(!shows(&app, cx, "versions-dialog-confirm"), "picker gone");
    cx.run_until_parked();
    assert!(ops.calls().is_empty());
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click("ok", cx);
    })
    .unwrap();
    wait_calls(&ops, cx, 1);
    assert_eq!(
        ops.calls(),
        [OpCall::InstallLoader {
            id: "Pack".into(),
            kind: LoaderKind::Forge,
            pick: VersionPick::Exact("47.4.0".into()),
            replace_existing: true,
        }]
    );
}

#[gpui_kit::test]
fn switching_loader_warns_about_mods(cx: &mut TestAppContext) {
    use patina_core::loader::ops::{LoaderNotice, OpOutcome};
    let tmp = tempfile::tempdir().unwrap();
    let (app, model, ops) = versions_app(cx, tmp.path(), "1.20.1", "0.14.0");
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    ops.then(Ok(OpOutcome {
        instance: store.load("Pack").unwrap(),
        notices: vec![LoaderNotice::ModsMayBeIncompatible],
    }));
    // The notice comes as an event of the model; the page shows it.
    let events: Rc<std::cell::RefCell<Vec<VersionsEvent>>> = Rc::default();
    let _subscription = cx.update(|cx| {
        let events = events.clone();
        cx.subscribe(&model, move |_, event: &VersionsEvent, _| {
            events.borrow_mut().push(event.clone())
        })
    });
    click(&app, cx, "versions-row-loader");
    click(&app, cx, "versions-switch-loader");
    click(&app, cx, loader_segment_id(Some(LoaderKind::Forge)));
    settle(cx, |cx| chosen(&app, cx, loader_row_id("47.3.0")));
    click(&app, cx, "versions-dialog-confirm");
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click("ok", cx);
    })
    .unwrap();
    settle(cx, |cx| notification_count(&app, cx) == 1);
    assert_eq!(
        *events.borrow(),
        [VersionsEvent::Warn(vec![
            LoaderNotice::ModsMayBeIncompatible.to_string()
        ])]
    );
}

#[gpui_kit::test]
fn creating_with_a_loader_installs_through_version_ops(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let ops = Arc::new(FakeVersionOps::new(store));
    let app = {
        let ops = ops.clone();
        boot_core(cx, tmp.path(), AppSettings::default(), move |rt, data| {
            testing::core_with_ops(rt, data, catalog(), ops)
        })
    };
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "add-instance-empty");
    wait_name(&app, cx, "1.21.1");
    click(&app, cx, loader_segment_id(Some(LoaderKind::Fabric)));
    wait_name(&app, cx, "Fabric 1.21.1");
    settle(cx, |cx| chosen(&app, cx, loader_row_id("0.16.5")));
    click(&app, cx, "add-instance-create");
    wait_calls(&ops, cx, 1);
    assert_eq!(
        ops.calls(),
        [OpCall::InstallLoader {
            id: "Fabric 1.21.1".into(),
            kind: LoaderKind::Fabric,
            pick: VersionPick::Exact("0.16.5".into()),
            replace_existing: false,
        }]
    );
}

// ---- Settings tab ----

use patina_app::instances::{SettingsForm, seed_java_list};
use patina_core::config::JavaSettings;
use patina_core::instance::{InstanceConfig, WindowSize};
use patina_core::java::JavaInstall;
use patina_core::jvm::GcPreset;

/// Scrolls the settings tab until element `id` (if drawn at all) is in view.
fn reveal(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) {
    let id = id.into();
    for _ in 0..40 {
        let Some(found) = element(app, cx, id.clone()) else {
            return;
        };
        if found.visible() {
            return;
        }
        let top = element(app, cx, "settings-tab").map_or(px(0.), |tab| tab.bounds().top());
        let up = found.bounds().center().y < top;
        let dy = if up { px(120.) } else { px(-120.) };
        cx.update_window(app.handle, |_, window, cx| {
            window.scroll(
                "settings-tab",
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), dy)),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
    }
}

/// [`click`], after scrolling to the element.
fn sclick(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) {
    let id = id.into();
    reveal(app, cx, id.clone());
    click(app, cx, id);
}

/// [`shows`], after scrolling to the element.
fn sshows(app: &Booted, cx: &mut TestAppContext, id: impl Into<gpui_kit::ElementId>) -> bool {
    let id = id.into();
    reveal(app, cx, id.clone());
    shows(app, cx, id)
}

/// Makes the main window the active one: focus changes (a field losing the
/// keyboard) reach the fields only in an active window.
fn activate(app: &Booted, cx: &mut TestAppContext) {
    cx.update_window(app.handle, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    cx.update_window(app.handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

fn java_install(path: &str, major: u32) -> JavaInstall {
    JavaInstall {
        path: path.into(),
        major,
        version: format!("{major}.0.1"),
        is_64bit: true,
    }
}

/// Boots over `names` (the first is selected), with the detected Java
/// installs `installs`, and shows the settings tab.
fn settings_app(
    cx: &mut TestAppContext,
    dir: &Path,
    names: &[&str],
    installs: Vec<JavaInstall>,
) -> (Booted, Entity<InstancesModel>, InstanceStore) {
    let store = InstanceStore::new(DataDirs::new(dir).instances());
    for name in names {
        store.create(name, "1.21.1").unwrap();
    }
    let app = boot(cx, dir, AppSettings::default());
    let model = open_settings(&app, cx, installs);
    (app, model, store)
}

/// Shows the settings tab of the booted app.
fn open_settings(
    app: &Booted,
    cx: &mut TestAppContext,
    installs: Vec<JavaInstall>,
) -> Entity<InstancesModel> {
    cx.update(|cx| seed_java_list(installs, cx));
    let model = instances_model(app, cx);
    list_settled(&model, cx);
    activate(app, cx);
    sclick(app, cx, "tab-settings");
    assert!(sshows(app, cx, "settings-tab"));
    model
}

fn settings_form(app: &Booted, cx: &mut TestAppContext) -> Entity<SettingsForm> {
    cx.update(|cx| {
        let page = app.root.read(cx).instances().read(cx);
        page.detail().read(cx).settings_view().cloned().unwrap()
    })
}

fn saved(store: &InstanceStore, id: &str) -> InstanceConfig {
    store.load(id).unwrap().config
}

fn saved_java(store: &InstanceStore, id: &str) -> Option<JavaSettings> {
    saved(store, id).java_override
}

/// Turns "使用全局设置" off and waits for the override.
fn override_on(app: &Booted, cx: &mut TestAppContext, store: &InstanceStore, id: &str) {
    sclick(app, cx, "settings-use-global");
    settle(cx, |_| saved_java(store, id).is_some());
    cx.run_until_parked();
}

/// Replaces the text of the input `id`.
fn replace_text(app: &Booted, cx: &mut TestAppContext, id: &'static str, text: &str) {
    sclick(app, cx, id);
    press(app, cx, "secondary-a");
    type_text(app, cx, text);
}

/// Moves the keyboard away from the field being edited, so it commits.
fn blur(app: &Booted, cx: &mut TestAppContext) {
    sclick(app, cx, "settings-name");
    // The focus change reaches the fields they lost it on the next frame.
    cx.update_window(app.handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

fn radio(
    app: &Booted,
    cx: &mut TestAppContext,
    group: &'static str,
    ix: usize,
) -> Option<ElementSnapshot> {
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within(group).try_find(ix)
    })
    .unwrap()
}

fn click_radio(app: &Booted, cx: &mut TestAppContext, group: &'static str, ix: usize) {
    reveal(app, cx, group);
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within(group).click(ix, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn radio_disabled(app: &Booted, cx: &mut TestAppContext, group: &'static str, ix: usize) -> bool {
    radio(app, cx, group, ix)
        .expect("the radio is drawn")
        .disabled()
        == Some(true)
}

#[gpui_kit::test]
fn override_toggle_writes_java_override(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    // The global config the override starts from.
    std::fs::write(
        DataDirs::new(tmp.path()).config_file(),
        "[java]\nmin_mem_mib = 2048\nextra_jvm_args = \"-Dglobal=1\"\n",
    )
    .unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);
    assert_eq!(saved_java(&store, "Alpha"), None);
    assert!(!sshows(&app, cx, "settings-min-mem"), "nothing to edit yet");

    // Off: an override seeded from the global config.
    sclick(&app, cx, "settings-use-global");
    settle(cx, |_| saved_java(&store, "Alpha").is_some());
    let java = saved_java(&store, "Alpha").unwrap();
    assert_eq!(java.min_mem_mib, 2048);
    assert_eq!(java.extra_jvm_args, "-Dglobal=1");
    settle(cx, |cx| {
        element(&app, cx, "settings-min-mem").and_then(|e| e.value().map(str::to_owned))
            == Some("2048".into())
    });

    // On: the override goes.
    sclick(&app, cx, "settings-use-global");
    settle(cx, |_| saved_java(&store, "Alpha").is_none());
    settle(cx, |cx| !sshows(&app, cx, "settings-min-mem"));
}

#[gpui_kit::test]
fn invalid_jvm_args_not_saved(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);
    override_on(&app, cx, &store, "Alpha");

    sclick(&app, cx, "settings-args");
    type_text(&app, cx, "-Xmx4G");
    assert!(
        sshows(&app, cx, "settings-args-error"),
        "said next to the field"
    );
    blur(&app, cx);
    cx.run_until_parked();
    assert_eq!(saved_java(&store, "Alpha").unwrap().extra_jvm_args, "");
    assert!(sshows(&app, cx, "settings-args-error"));

    // Fixed: the error goes and the value is saved.
    sclick(&app, cx, "settings-args");
    press(&app, cx, "secondary-a");
    type_text(&app, cx, "-Dfoo=bar");
    assert!(!sshows(&app, cx, "settings-args-error"));
    blur(&app, cx);
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().extra_jvm_args == "-Dfoo=bar"
    });
}

#[gpui_kit::test]
fn min_over_max_rejected(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);
    override_on(&app, cx, &store, "Alpha");

    replace_text(&app, cx, "settings-max-mem", "2048");
    replace_text(&app, cx, "settings-min-mem", "4096");
    blur(&app, cx);
    assert!(sshows(&app, cx, "settings-memory-error"));
    cx.run_until_parked();
    let java = saved_java(&store, "Alpha").unwrap();
    assert_eq!(
        (java.min_mem_mib, java.max_mem_mib),
        (512, Some(2048)),
        "only the maximum"
    );

    // Not a number is rejected too.
    replace_text(&app, cx, "settings-min-mem", "lots");
    blur(&app, cx);
    assert!(sshows(&app, cx, "settings-memory-error"));

    // A consistent pair saves, and "自动" clears the maximum.
    replace_text(&app, cx, "settings-min-mem", "1024");
    blur(&app, cx);
    assert!(!sshows(&app, cx, "settings-memory-error"));
    settle(cx, |_| {
        let j = saved_java(&store, "Alpha").unwrap();
        (j.min_mem_mib, j.max_mem_mib) == (1024, Some(2048))
    });
    sclick(&app, cx, "settings-max-auto");
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().max_mem_mib.is_none()
    });
}

#[gpui_kit::test]
fn gc_incompatible_disabled_for_java8(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let installs = vec![
        java_install("/opt/j8/bin/java", 8),
        java_install("/opt/j21/bin/java", 21),
    ];
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], installs);
    override_on(&app, cx, &store, "Alpha");

    // Choose Java 8 (managed is 0, then the detected ones).
    click_radio(&app, cx, "settings-java", 1);
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().java_path == Some("/opt/j8/bin/java".into())
    });
    cx.run_until_parked();
    assert!(!radio_disabled(&app, cx, "settings-gc", 0));
    assert!(!radio_disabled(&app, cx, "settings-gc", 1), "G1 runs on 8");
    assert!(radio_disabled(&app, cx, "settings-gc", 2), "ZGC needs 21");
    assert!(sshows(&app, cx, "settings-gc-reason-2"));
    assert_eq!(
        label_of(&app, cx, "settings-gc-reason-2").as_deref(),
        Some("Generational ZGC needs Java 21 or newer")
    );

    // A disabled preset cannot be picked.
    click_radio(&app, cx, "settings-gc", 2);
    cx.run_until_parked();
    assert_eq!(
        saved_java(&store, "Alpha").unwrap().gc_preset,
        GcPreset::None
    );

    // Java 21 allows it; moving back to 8 resets it.
    click_radio(&app, cx, "settings-java", 2);
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().java_path == Some("/opt/j21/bin/java".into())
    });
    cx.run_until_parked();
    assert!(!radio_disabled(&app, cx, "settings-gc", 2));
    click_radio(&app, cx, "settings-gc", 2);
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().gc_preset == GcPreset::GenerationalZgc
    });
    click_radio(&app, cx, "settings-java", 1);
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().gc_preset == GcPreset::None
    });
}

#[gpui_kit::test]
fn window_size_bounds(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);
    assert!(!sshows(&app, cx, "settings-width"), "default has no inputs");

    // 自定义 starts from the game's own default size.
    click_radio(&app, cx, "settings-window-mode", 1);
    settle(cx, |_| saved(&store, "Alpha").window.is_some());
    assert_eq!(
        saved(&store, "Alpha").window,
        Some(WindowSize {
            width: 854,
            height: 480
        })
    );
    settle(cx, |cx| sshows(&app, cx, "settings-width"));

    // Out of range: said next to the field, not saved.
    replace_text(&app, cx, "settings-width", "100");
    blur(&app, cx);
    assert!(sshows(&app, cx, "settings-window-error"));
    replace_text(&app, cx, "settings-width", "20000");
    blur(&app, cx);
    assert!(sshows(&app, cx, "settings-window-error"));
    replace_text(&app, cx, "settings-height", "200");
    blur(&app, cx);
    cx.run_until_parked();
    assert_eq!(saved(&store, "Alpha").window.unwrap().width, 854);
    assert_eq!(saved(&store, "Alpha").window.unwrap().height, 480);

    // In range on both: saved.
    replace_text(&app, cx, "settings-width", "1280");
    replace_text(&app, cx, "settings-height", "720");
    blur(&app, cx);
    assert!(!sshows(&app, cx, "settings-window-error"));
    settle(cx, |_| {
        saved(&store, "Alpha").window
            == Some(WindowSize {
                width: 1280,
                height: 720,
            })
    });

    // 默认 drops it.
    click_radio(&app, cx, "settings-window-mode", 0);
    settle(cx, |_| saved(&store, "Alpha").window.is_none());
}

#[gpui_kit::test]
fn neoforge_section_only_for_neoforge(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let store = InstanceStore::new(data.instances());
    for name in ["Alpha", "Beta", "Gamma"] {
        store.create(name, "1.21.1").unwrap();
    }
    set_loader(&store, "Beta", ComponentUid::Neoforge);
    set_loader(&store, "Gamma", ComponentUid::Neoforge);
    let gamma_fml = store
        .load("Gamma")
        .unwrap()
        .game_dir()
        .join("config/fml.toml");
    std::fs::create_dir_all(gamma_fml.parent().unwrap()).unwrap();
    std::fs::write(&gamma_fml, "this is [not toml").unwrap();
    let beta_fml = store
        .load("Beta")
        .unwrap()
        .game_dir()
        .join("config/fml.toml");

    let app = boot(cx, tmp.path(), AppSettings::default());
    cx.update(|cx| seed_java_list(vec![], cx));
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    activate(&app, cx);
    sclick(&app, cx, "tab-settings");

    // Vanilla: no section.
    assert_eq!(selected(&model, cx).as_deref(), Some("Alpha"));
    assert!(sshows(&app, cx, "settings-danger"));
    assert!(!sshows(&app, cx, "settings-neoforge"));

    // NeoForge: the switch reads and writes fml.toml.
    sclick(&app, cx, row_id("Beta"));
    settle(cx, |cx| sshows(&app, cx, "settings-neoforge"));
    settle(cx, |cx| !disabled(&app, cx, "settings-early-window"));
    assert!(!sshows(&app, cx, "settings-neoforge-error"));
    sclick(&app, cx, "settings-early-window");
    settle(cx, |_| {
        std::fs::read_to_string(&beta_fml).is_ok_and(|t| t.contains("earlyWindowControl = false"))
    });

    // A file that does not parse: the error, and the switch is unavailable.
    sclick(&app, cx, row_id("Gamma"));
    settle(cx, |cx| sshows(&app, cx, "settings-neoforge-error"));
    assert!(disabled(&app, cx, "settings-early-window"));
    sclick(&app, cx, "settings-early-window");
    cx.run_until_parked();
    assert_eq!(
        std::fs::read_to_string(&gamma_fml).unwrap(),
        "this is [not toml"
    );
}

#[gpui_kit::test]
fn delete_disabled_while_running(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, model, store) = settings_app(cx, tmp.path(), &["Alpha", "Beta"], vec![]);
    assert_eq!(selected(&model, cx).as_deref(), Some("Alpha"));

    // Running: disabled, with the reason beside it; so is renaming.
    model.update(cx, |m, cx| m.set_run_state("Alpha", Some(elsewhere()), cx));
    cx.run_until_parked();
    assert!(disabled(&app, cx, "settings-delete"));
    assert_eq!(
        label_of(&app, cx, "settings-delete-reason").as_deref(),
        Some("The game is running")
    );
    assert_eq!(
        label_of(&app, cx, "settings-name-reason").as_deref(),
        Some("The game is running")
    );
    replace_text(&app, cx, "settings-name", "Nope");
    press(&app, cx, "enter");
    cx.run_until_parked();
    assert_eq!(
        saved(&store, "Alpha").name,
        "Alpha",
        "not renamed while running"
    );
    sclick(&app, cx, "settings-delete");
    assert!(!sshows(&app, cx, "dialog"));

    // Idle again: asks first, and 取消 keeps it.
    model.update(cx, |m, cx| m.set_run_state("Alpha", None, cx));
    cx.run_until_parked();
    assert!(!disabled(&app, cx, "settings-delete"));
    sclick(&app, cx, "settings-delete");
    assert!(sshows(&app, cx, "dialog"));
    assert_eq!(
        dialog_label(&app, cx, "settings-delete-title"),
        "Delete “Alpha”?"
    );
    dialog_click(&app, cx, "cancel");
    assert!(!sshows(&app, cx, "dialog"));
    cx.run_until_parked();
    assert!(store.load("Alpha").is_ok());

    // 删除 deletes it and selects the next one.
    let changed = Rc::new(std::cell::Cell::new(0));
    let _subscription = cx.update(|cx| {
        let changed = changed.clone();
        cx.subscribe(&model, move |_, _: &InstancesChanged, _| {
            changed.set(changed.get() + 1)
        })
    });
    sclick(&app, cx, "settings-delete");
    dialog_click(&app, cx, "ok");
    settle(cx, |cx| {
        selected(&model, cx).as_deref() == Some("Beta")
            && model.read_with(cx, |m, _| m.instances().len() == 1)
    });
    assert!(store.load("Alpha").is_err());
    assert!(store.load("Beta").is_ok());
    assert_eq!(changed.get(), 1, "the status bar is told");
}

/// Runs `task` to its end.
fn finish<T: 'static>(cx: &mut TestAppContext, task: gpui_kit::Task<T>) -> T {
    let slot = Rc::new(std::cell::RefCell::new(None));
    let fill = slot.clone();
    cx.update(|cx| {
        cx.spawn(async move |_| *fill.borrow_mut() = Some(task.await))
            .detach()
    });
    settle(cx, |_| slot.borrow().is_some());
    slot.borrow_mut().take().unwrap()
}

#[gpui_kit::test]
fn deleting_a_busy_instance_says_why(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let store = InstanceStore::new(data.instances());
    let id = store.create("Alpha", "1.21.1").unwrap().id;
    store.rename(&id, "My Pack").unwrap();
    let broken = data.instances().join("Broken");
    std::fs::create_dir_all(&broken).unwrap();
    break_config(&broken);
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = open_settings(&app, cx, vec![]);
    assert_eq!(selected(&model, cx).as_deref(), Some(id.as_str()));
    // Another operation (this test's) holds both.
    let held = (store.lock(&id).unwrap(), store.lock("Broken").unwrap());

    let deleted = model.update(cx, |m, cx| m.delete(&id, cx));
    // The reason follows the name in the notice ("Couldn’t delete “My
    // Pack”: …"), so it does not repeat it.
    assert_eq!(
        finish(cx, deleted).unwrap_err(),
        "An operation is in progress"
    );
    let entry = model.read_with(cx, |m, _| m.broken()[0].clone());
    let deleted = model.update(cx, |m, cx| m.delete_broken(entry, cx));
    assert_eq!(
        finish(cx, deleted).unwrap_err(),
        "An operation is in progress"
    );

    // The settings tab's delete says so too, and the instance stays.
    sclick(&app, cx, "settings-delete");
    dialog_click(&app, cx, "ok");
    settle(cx, |cx| notification_count(&app, cx) == 1);
    list_settled(&model, cx);
    assert!(store.load(&id).is_ok());
    assert!(broken.exists());
    drop(held);
}

#[gpui_kit::test]
fn name_and_icon_save(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);

    replace_text(&app, cx, "settings-name", "Renamed");
    press(&app, cx, "enter");
    settle(cx, |_| saved(&store, "Alpha").name == "Renamed");

    // An empty name is refused next to the field and the input stays.
    sclick(&app, cx, "settings-name");
    press(&app, cx, "secondary-a");
    press(&app, cx, "backspace");
    press(&app, cx, "enter");
    assert!(sshows(&app, cx, "settings-name-error"));
    assert_eq!(saved(&store, "Alpha").name, "Renamed");
    replace_text(&app, cx, "settings-name", "Again");
    press(&app, cx, "enter");
    sclick(&app, cx, "settings-icon-toggle");
    settle(cx, |cx| !sshows(&app, cx, "settings-name-error"));
    settle(cx, |_| saved(&store, "Alpha").name == "Again");

    // The icon picker saves through the model.
    sclick(&app, cx, pick_id("sword"));
    settle(cx, |_| {
        saved(&store, "Alpha").icon == Some(InstanceIcon::Builtin("sword".into()))
    });
}

#[gpui_kit::test]
fn typed_edits_are_saved_before_launch(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    store.create("Alpha", "1.21.1").unwrap();
    with_account(tmp.path());
    let backend = FakeGames::failing(|| SessionError::Io {
        path: "/nowhere".into(),
        error: "stop here".into(),
    });
    let app = boot_games(cx, tmp.path(), backend.clone());
    open_settings(&app, cx, vec![]);
    override_on(&app, cx, &store, "Alpha");

    // Typed, never committed: the launch saves it first and uses it.
    replace_text(&app, cx, "settings-max-mem", "3000");
    click(&app, cx, "launch");
    settle(cx, |_| backend.prepares() == 1);
    assert_eq!(saved_java(&store, "Alpha").unwrap().max_mem_mib, Some(3000));
    assert_eq!(backend.seen()[0].as_ref().unwrap().max_mem_mib, Some(3000));
}

#[gpui_kit::test]
fn typed_edits_are_saved_on_selection_and_tab_change(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha", "Beta"], vec![]);
    override_on(&app, cx, &store, "Alpha");

    replace_text(&app, cx, "settings-max-mem", "2048");
    click(&app, cx, row_id("Beta"));
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().max_mem_mib == Some(2048)
    });
    assert_eq!(saved(&store, "Beta").java_override, None);

    click(&app, cx, row_id("Alpha"));
    settle(cx, |cx| sshows(&app, cx, "settings-args"));
    replace_text(&app, cx, "settings-min-mem", "1024");
    sclick(&app, cx, "settings-args");
    type_text(&app, cx, "-Dx=1");
    click(&app, cx, "tab-overview");
    settle(cx, |_| {
        let java = saved_java(&store, "Alpha").unwrap();
        java.min_mem_mib == 1024 && java.extra_jvm_args == "-Dx=1"
    });

    // An invalid edit stays in its field, with its error, and is not saved.
    click(&app, cx, "tab-settings");
    sclick(&app, cx, "settings-args");
    press(&app, cx, "secondary-a");
    type_text(&app, cx, "-Xmx9G");
    click(&app, cx, "tab-overview");
    click(&app, cx, "tab-settings");
    assert!(sshows(&app, cx, "settings-args-error"));
    assert_eq!(saved_java(&store, "Alpha").unwrap().extra_jvm_args, "-Dx=1");
}

#[gpui_kit::test]
fn ctrl_tab_and_ctrl_page_keys_switch_the_detail_tabs(cx: &mut TestAppContext) {
    use patina_app::instances::DetailTab;
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);
    override_on(&app, cx, &store, "Alpha");
    let detail = cx.update(|cx| app.root.read(cx).instances().read(cx).detail().clone());
    let tab = |cx: &mut TestAppContext| detail.read_with(cx, |d, _| d.tab());

    // From inside the settings form: the typed edit is saved on the way.
    replace_text(&app, cx, "settings-max-mem", "2048");
    press(&app, cx, "ctrl-tab");
    assert_eq!(tab(cx), DetailTab::Log);
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().max_mem_mib == Some(2048)
    });
    // The keyboard left the form with its tab and the keys keep working.
    press(&app, cx, "ctrl-tab");
    assert_eq!(tab(cx), DetailTab::Overview, "wraps after the last tab");
    press(&app, cx, "ctrl-pagedown");
    assert_eq!(tab(cx), DetailTab::Versions);
    press(&app, cx, "ctrl-shift-tab");
    assert_eq!(tab(cx), DetailTab::Overview);
    press(&app, cx, "ctrl-pageup");
    assert_eq!(tab(cx), DetailTab::Log, "wraps before the first tab");
    assert!(shows(&app, cx, "tab-log"));

    // From the instance list too.
    click(&app, cx, row_id("Alpha"));
    press(&app, cx, "ctrl-tab");
    assert_eq!(tab(cx), DetailTab::Overview);
    press(&app, cx, "down");
    press(&app, cx, "ctrl-tab");
    assert_eq!(tab(cx), DetailTab::Versions);
}

/// Drags the vertical scrollbar of region `id` (shown once the region
/// scrolls) from near its top to past its bottom.
fn drag_scrollbar_down(app: &Booted, cx: &mut TestAppContext, id: &'static str) {
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.scroll(
            id,
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-20.))),
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    let region = element(app, cx, id).unwrap().bounds();
    // The middle of the thumb's 6px width, 4px in from the edge.
    let x = region.right() - px(7.);
    cx.update_window(app.handle, |_, window, cx| {
        window.drag(
            gpui_kit::point(x, region.top() + px(30.)),
            gpui_kit::point(x, region.bottom() + px(400.)),
            cx,
        )
    })
    .unwrap();
    cx.run_until_parked();
}

#[gpui_kit::test]
fn long_regions_scroll_by_their_scrollbar(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..40).map(|n| format!("I{n:02}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let (app, _model, _store) = settings_app(cx, tmp.path(), &names, vec![]);
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    cx.run_until_parked();

    // The settings form.
    assert!(
        !shows(&app, cx, "settings-delete"),
        "this test needs an overflow"
    );
    drag_scrollbar_down(&app, cx, "settings-tab");
    assert!(shows(&app, cx, "settings-delete"));

    // The instance list.
    assert!(
        !shows(&app, cx, row_id("I39")),
        "this test needs an overflow"
    );
    drag_scrollbar_down(&app, cx, "instance-rows-region");
    assert!(shows(&app, cx, row_id("I39")));
}

#[gpui_kit::test]
fn typed_edits_are_saved_before_the_window_closes(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);
    override_on(&app, cx, &store, "Alpha");
    replace_text(&app, cx, "settings-max-mem", "2048");
    assert_ne!(saved_java(&store, "Alpha").unwrap().max_mem_mib, Some(2048));

    let mut window = gpui_kit::VisualTestContext::from_window(app.handle, cx);
    assert!(!window.simulate_close(), "waits for the save");
    settle(cx, |cx| cx.update(|cx| cx.windows().is_empty()));
    assert_eq!(saved_java(&store, "Alpha").unwrap().max_mem_mib, Some(2048));
}

#[gpui_kit::test]
fn global_toggle_keeps_the_discarded_override(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);
    override_on(&app, cx, &store, "Alpha");
    replace_text(&app, cx, "settings-max-mem", "4096");
    blur(&app, cx);
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().max_mem_mib == Some(4096)
    });

    sclick(&app, cx, "settings-use-global");
    settle(cx, |_| saved_java(&store, "Alpha").is_none());
    // Off again: the same settings come back.
    sclick(&app, cx, "settings-use-global");
    settle(cx, |_| saved_java(&store, "Alpha").is_some());
    assert_eq!(saved_java(&store, "Alpha").unwrap().max_mem_mib, Some(4096));
}

#[gpui_kit::test]
fn gc_options_wait_for_a_known_java(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(
        cx,
        tmp.path(),
        &["Alpha"],
        vec![java_install("/opt/j21/bin/java", 21)],
    );
    override_on(&app, cx, &store, "Alpha");

    // The Minecraft version is not installed here, so the managed Java's
    // version stays unknown: only the preset needing more than Java 8 is off.
    settle(cx, |cx| sshows(&app, cx, "settings-gc-unknown"));
    assert!(!radio_disabled(&app, cx, "settings-gc", 0));
    assert!(
        !radio_disabled(&app, cx, "settings-gc", 1),
        "G1 runs on any Java"
    );
    assert!(radio_disabled(&app, cx, "settings-gc", 2), "ZGC needs 21");
    assert!(!radio_disabled(&app, cx, "settings-gc", 3));

    // A known Java opens them.
    click_radio(&app, cx, "settings-java", 1);
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().java_path.is_some()
    });
    settle(cx, |cx| !sshows(&app, cx, "settings-gc-unknown"));
    assert!(!radio_disabled(&app, cx, "settings-gc", 2));
}

#[gpui_kit::test]
fn a_saved_gc_preset_is_reset_when_the_java_turns_out_old(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let mut inst = store.create("Alpha", "1.21.1").unwrap();
    inst.config.java_override = Some(JavaSettings {
        java_path: Some("/opt/j8/bin/java".into()),
        gc_preset: GcPreset::GenerationalZgc,
        ..Default::default()
    });
    store.save(&inst).unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    // Not detected yet, and the file does not run: the major is unknown.
    open_settings(&app, cx, vec![]);
    cx.run_until_parked();
    assert_eq!(
        saved_java(&store, "Alpha").unwrap().gc_preset,
        GcPreset::GenerationalZgc
    );
    assert_eq!(notification_count(&app, cx), 0);

    // Detection finds it is Java 8: the preset cannot run on it.
    cx.update(|cx| seed_java_list(vec![java_install("/opt/j8/bin/java", 8)], cx));
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().gc_preset == GcPreset::None
    });
    settle(cx, |cx| notification_count(&app, cx) == 1);
}

#[gpui_kit::test]
fn a_file_that_is_no_java_is_refused(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);
    override_on(&app, cx, &store, "Alpha");
    let form = settings_form(&app, cx);
    cx.update_window(app.handle, |_, window, cx| {
        form.update(cx, |form, cx| {
            form.use_java_file("/nonexistent/not-a-java".into(), window, cx)
        })
    })
    .unwrap();
    settle(cx, |cx| sshows(&app, cx, "settings-java-error"));
    assert_eq!(
        label_of(&app, cx, "settings-java-error").as_deref(),
        Some("That file isn’t a usable Java")
    );
    assert_eq!(saved_java(&store, "Alpha").unwrap().java_path, None);
}

#[gpui_kit::test]
fn memory_has_an_upper_bound(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);
    override_on(&app, cx, &store, "Alpha");

    // 1,048,576 MiB (1 TiB) is the most accepted.
    replace_text(&app, cx, "settings-max-mem", "1048577");
    blur(&app, cx);
    assert_eq!(
        label_of(&app, cx, "settings-memory-error").as_deref(),
        Some("Can’t exceed 1048576 MiB")
    );
    assert_eq!(saved_java(&store, "Alpha").unwrap().max_mem_mib, None);
    replace_text(&app, cx, "settings-max-mem", "1048576");
    blur(&app, cx);
    settle(cx, |_| {
        saved_java(&store, "Alpha").unwrap().max_mem_mib == Some(1_048_576)
    });
}

#[gpui_kit::test]
fn a_failed_icon_save_shows_in_the_picker(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let junk = tmp.path().join("junk.png");
    std::fs::write(&junk, b"definitely not a png").unwrap();
    let (app, _model, store) = settings_app(cx, tmp.path(), &["Alpha"], vec![]);
    sclick(&app, cx, "settings-icon-toggle");
    sclick(&app, cx, "icon-upload");
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(move |_| Some(vec![junk]));
    settle(cx, |cx| sshows(&app, cx, "icon-error"));
    assert_eq!(saved(&store, "Alpha").icon, None);
}

// ---- Mods tab ----

use patina_app::content::{
    HIGHLIGHT_FOR, ModsChanged, ModsEvents, ModsTab, mod_delete_id, mod_disabled_id, mod_row_id,
    mod_switch_id,
};

/// Writes a Fabric mod jar `file` (mod `name`, version 1.0, by Alice and
/// Bob) into `dir`.
fn fabric_jar(dir: &Path, file: &str, name: &str) {
    use std::io::Write as _;
    let mut w = zip::ZipWriter::new(std::fs::File::create(dir.join(file)).unwrap());
    w.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default())
        .unwrap();
    let id = name.to_lowercase().replace(' ', "_");
    write!(
        w,
        r#"{{"schemaVersion":1,"id":"{id}","version":"1.0","name":"{name}","authors":["Alice","Bob"]}}"#
    )
    .unwrap();
    w.finish().unwrap();
}

/// A file named like a mod that is no zip at all.
fn broken_jar(dir: &Path, file: &str) {
    std::fs::write(dir.join(file), b"not a zip, just bytes").unwrap();
}

/// Instance "Alpha" with its mods folder filled by `fill`; the app booted,
/// Alpha selected and its Mods tab shown with the listing landed. Returns
/// the app, the tab and the mods folder.
fn mods_app(
    cx: &mut TestAppContext,
    dir: &Path,
    fill: impl FnOnce(&Path),
) -> (Booted, Entity<ModsTab>, std::path::PathBuf) {
    let store = InstanceStore::new(DataDirs::new(dir).instances());
    let alpha = store.create("Alpha", "1.21.1").unwrap();
    let mods = alpha.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    fill(&mods);
    let app = boot(cx, dir, AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, row_id("Alpha"));
    click(&app, cx, "tab-mods");
    let tab = mods_tab(&app, cx);
    mods_settled(&tab, cx);
    (app, tab, mods)
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

/// Waits until the tab's listing has landed and none is in flight.
fn mods_settled(tab: &Entity<ModsTab>, cx: &mut TestAppContext) {
    settle(cx, |cx| {
        tab.read_with(cx, |t, cx| {
            let list = t.list().read(cx);
            !list.is_reloading() && *list.state() == patina_app::content::LoadState::Loaded
        })
    });
}

fn mod_files(tab: &Entity<ModsTab>, cx: &mut TestAppContext) -> Vec<String> {
    tab.read_with(cx, |t, cx| {
        (t.list().read(cx).items().iter())
            .map(|m| m.file_name.clone())
            .collect()
    })
}

/// Counts the listings of `tab` that land.
fn count_listings(
    tab: &Entity<ModsTab>,
    cx: &mut TestAppContext,
) -> (Rc<std::cell::Cell<usize>>, gpui_kit::Subscription) {
    let count = Rc::new(std::cell::Cell::new(0));
    let list = tab.read_with(cx, |t, _| t.list().clone());
    let subscription = cx.update(|cx| {
        let count = count.clone();
        cx.subscribe(&list, move |_, _: &patina_app::content::Reloaded, _| {
            count.set(count.get() + 1)
        })
    });
    (count, subscription)
}

/// Counts the [`ModsChanged`] events.
fn count_mods_changed(
    cx: &mut TestAppContext,
) -> (Rc<std::cell::Cell<usize>>, gpui_kit::Subscription) {
    let count = Rc::new(std::cell::Cell::new(0));
    let subscription = cx.update(|cx| {
        let count = count.clone();
        cx.subscribe(&ModsEvents::entity(cx), move |_, event: &ModsChanged, _| {
            assert_eq!(event.instance_id, "Alpha");
            count.set(count.get() + 1)
        })
    });
    (count, subscription)
}

#[gpui_kit::test]
fn large_mod_folder_stays_responsive(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let mods = store
        .create("Alpha", "1.21.1")
        .unwrap()
        .game_dir()
        .join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    // 500 jars, every 25th of them (20) broken.
    for i in 0..500 {
        let file = format!("mod-{i:03}.jar");
        if i % 25 == 0 {
            broken_jar(&mods, &file);
        } else {
            fabric_jar(&mods, &file, &format!("Mod {i:03}"));
        }
    }
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, row_id("Alpha"));
    click(&app, cx, "tab-mods");
    let tab = mods_tab(&app, cx);
    mods_settled(&tab, cx);

    // A reload returns at once: the listing runs in the background while
    // the main thread goes on drawing and the clock moves.
    let list = tab.read_with(cx, |t, _| t.list().clone());
    list.update(cx, |l, cx| l.reload(cx));
    assert!(list.read_with(cx, |l, _| l.is_reloading()));
    cx.executor().advance_clock(Duration::from_millis(100));
    cx.update_window(app.handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    // The old listing stays on show meanwhile.
    assert_eq!(mod_files(&tab, cx).len(), 500);
    mods_settled(&tab, cx);

    let unreadable = tab.read_with(cx, |t, cx| {
        (t.list().read(cx).items().iter())
            .filter(|m| m.info.is_none())
            .count()
    });
    assert_eq!(mod_files(&tab, cx).len(), 500);
    assert_eq!(unreadable, 20);
    // Only the rows in view are drawn.
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(mod_row_id("mod-000.jar")).visible());
        assert!(window.try_find(mod_row_id("mod-499.jar")).is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn toggle_updates_row_and_suffix(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, mods) = mods_app(cx, tmp.path(), |dir| {
        fabric_jar(dir, "a.jar", "Alpha Mod");
        fabric_jar(dir, "b.jar", "Beta Mod");
    });
    let (listings, _l) = count_listings(&tab, cx);
    let (changed, _c) = count_mods_changed(cx);
    assert_eq!(
        element(&app, cx, mod_switch_id("a.jar")).and_then(|s| s.checked()),
        Some(true)
    );

    click(&app, cx, mod_switch_id("a.jar"));
    settle(cx, |cx| {
        mod_files(&tab, cx).contains(&"a.jar.disabled".into())
    });
    assert!(!mods.join("a.jar").exists());
    assert!(mods.join("a.jar.disabled").exists());
    // The row says so in words, not only by its colour.
    assert_eq!(
        label_of(&app, cx, mod_disabled_id("a.jar.disabled")).as_deref(),
        Some("Disabled")
    );
    assert_eq!(
        element(&app, cx, mod_switch_id("a.jar.disabled")).and_then(|s| s.checked()),
        Some(false)
    );
    assert!(element(&app, cx, mod_disabled_id("b.jar")).is_none());
    // Only the row changed: no listing ran; the search index is told.
    cx.run_until_parked();
    assert_eq!(listings.get(), 0);
    assert_eq!(changed.get(), 1);

    // And back.
    click(&app, cx, mod_switch_id("a.jar.disabled"));
    settle(cx, |cx| mod_files(&tab, cx).contains(&"a.jar".into()));
    assert!(mods.join("a.jar").exists());
    assert!(!mods.join("a.jar.disabled").exists());
    assert!(element(&app, cx, mod_disabled_id("a.jar")).is_none());
    assert_eq!(changed.get(), 2);
}

#[gpui_kit::test]
fn toggle_after_external_change_refreshes(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, mods) = mods_app(cx, tmp.path(), |dir| {
        fabric_jar(dir, "a.jar", "Alpha Mod");
        fabric_jar(dir, "b.jar", "Beta Mod");
        fabric_jar(dir, "c.jar", "Gamma Mod");
    });
    let (changed, _c) = count_mods_changed(cx);
    // Another program removes a.jar after the listing.
    std::fs::remove_file(mods.join("a.jar")).unwrap();
    click(&app, cx, mod_switch_id("a.jar"));
    settle(cx, |cx| label_of(&app, cx, NOTICE_TEXT).is_some());
    assert_eq!(
        label_of(&app, cx, NOTICE_TEXT).as_deref(),
        Some("The file was changed outside Patina")
    );
    mods_settled(&tab, cx);
    assert_eq!(mod_files(&tab, cx), ["b.jar", "c.jar"]);
    assert!(!mods.join("a.jar.disabled").exists());
    // The listing found the folder changed: the search index is told.
    assert_eq!(changed.get(), 1);

    // The same for a deletion.
    std::fs::remove_file(mods.join("b.jar")).unwrap();
    click(&app, cx, mod_delete_id("b.jar"));
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click("ok", cx);
    })
    .unwrap();
    settle(cx, |cx| mod_files(&tab, cx) == ["c.jar"]);
    assert_eq!(notification_count(&app, cx), 2);
}

#[gpui_kit::test]
fn delete_requires_confirmation(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, mods) = mods_app(cx, tmp.path(), |dir| {
        fabric_jar(dir, "a.jar", "Alpha Mod");
        fabric_jar(dir, "b.jar", "Beta Mod");
    });
    app.language(Language::ZhCn);
    let (changed, _c) = count_mods_changed(cx);

    click(&app, cx, mod_delete_id("a.jar"));
    assert!(shows(&app, cx, "dialog"), "asks first");
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let title = window.within("dialog").find("mods-delete-title");
        assert_eq!(title.label(), Some("删除\"Alpha Mod\"？"));
        let body = window.within("dialog").find("mods-delete-body");
        assert_eq!(body.label(), Some("文件会被永久删除。"));
    })
    .unwrap();
    assert!(
        mods.join("a.jar").exists(),
        "nothing deleted before the answer"
    );

    // 取消 keeps the file.
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click("cancel", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!shows(&app, cx, "dialog"));
    // A deletion starts (synchronously) only from 删除: none is in flight.
    assert_eq!(tab.read_with(cx, |t, _| t.changes_in_flight()), 0);
    assert!(mods.join("a.jar").exists());
    assert_eq!(changed.get(), 0);

    // Enter on the focused 取消 cancels too.
    click(&app, cx, mod_delete_id("a.jar"));
    tab_to(&app, cx, "cancel");
    press(&app, cx, "enter");
    assert!(!shows(&app, cx, "dialog"));
    assert_eq!(tab.read_with(cx, |t, _| t.changes_in_flight()), 0);
    assert!(mods.join("a.jar").exists());

    // 删除 deletes it and drops its row.
    click(&app, cx, mod_delete_id("a.jar"));
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("dialog").click("ok", cx);
    })
    .unwrap();
    settle(cx, |cx| mod_files(&tab, cx) == ["b.jar"]);
    assert!(!mods.join("a.jar").exists());
    assert!(!shows(&app, cx, mod_row_id("a.jar")));
    assert!(!shows(&app, cx, "dialog"));
    assert_eq!(changed.get(), 1);
    assert!(mods.join("b.jar").exists());
}

#[gpui_kit::test]
fn mutations_disabled_while_running(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, mods) = mods_app(cx, tmp.path(), |dir| {
        fabric_jar(dir, "a.jar", "Alpha Mod");
    });
    let model = instances_model(&app, cx);
    model.update(cx, |m, cx| m.set_run_state("Alpha", Some(elsewhere()), cx));
    cx.run_until_parked();

    assert!(disabled(&app, cx, mod_switch_id("a.jar")));
    assert!(disabled(&app, cx, mod_delete_id("a.jar")));
    // The reason is on show, not only implied by the disabled controls.
    assert_eq!(
        label_of(&app, cx, "mods-busy").as_deref(),
        Some("The game is running, so mods can’t be changed now")
    );
    // The keyboard is refused too, with the reason.
    click(&app, cx, mod_row_id("a.jar"));
    press(&app, cx, "space");
    press(&app, cx, "delete");
    assert!(!shows(&app, cx, "dialog"));
    // Refused on the spot: nothing started.
    assert_eq!(tab.read_with(cx, |t, _| t.changes_in_flight()), 0);
    assert!(mods.join("a.jar").exists());
    // Both keys said why.
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let notices = window.find_all(NOTICE_TEXT);
        assert_eq!(notices.len(), 2);
        for notice in notices {
            assert_eq!(
                notice.label(),
                Some("The game is running, so mods can’t be changed now")
            );
        }
    })
    .unwrap();

    // The game wrote a mod meanwhile; when it ends the tab lists again.
    fabric_jar(&mods, "c.jar", "Gamma Mod");
    model.update(cx, |m, cx| m.set_run_state("Alpha", None, cx));
    settle(cx, |cx| mod_files(&tab, cx) == ["a.jar", "c.jar"]);
    assert!(!disabled(&app, cx, mod_switch_id("a.jar")));
    assert!(!shows(&app, cx, "mods-busy"));
}

#[gpui_kit::test]
fn focus_file_scrolls_and_highlights(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let mods = store
        .create("Alpha", "1.21.1")
        .unwrap()
        .game_dir()
        .join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    for i in 0..100 {
        fabric_jar(&mods, &format!("mod-{i:03}.jar"), &format!("Mod {i:03}"));
    }
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, row_id("Alpha"));
    let detail = cx.update(|cx| app.root.read(cx).instances().read(cx).detail().clone());

    // From another tab, before the Mods tab has listed anything: the
    // search's jump waits for the listing.
    cx.update_window(app.handle, |_, window, cx| {
        detail.update(cx, |d, cx| d.show_mod("mod-090.jar", window, cx));
    })
    .unwrap();
    let tab = mods_tab(&app, cx);
    settle(cx, |cx| {
        tab.read_with(cx, |t, _| t.highlighted() == Some("mod-090.jar"))
    });
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let region = window.find("mod-rows-region").bounds();
        let row = window.find(mod_row_id("mod-090.jar"));
        assert!(row.visible());
        assert!(row.bounds().top() >= region.top());
        assert!(row.bounds().bottom() <= region.bottom() + px(1.));
        assert_eq!(row.selected(), Some(true));
        assert!(window.try_find(mod_row_id("mod-000.jar")).is_none());
    })
    .unwrap();

    // The highlight goes after two seconds; the selection stays.
    cx.executor().advance_clock(HIGHLIGHT_FOR);
    cx.run_until_parked();
    assert_eq!(
        tab.read_with(cx, |t, _| t.highlighted().map(str::to_owned)),
        None
    );
    assert_eq!(
        tab.read_with(cx, |t, _| t.selected().map(str::to_owned))
            .as_deref(),
        Some("mod-090.jar")
    );

    // A filter that hides the file is cleared for it.
    tab.update(cx, |t, cx| {
        t.list().update(cx, |l, cx| l.set_filter("mod-00", cx))
    });
    cx.update_window(app.handle, |_, window, cx| {
        detail.update(cx, |d, cx| d.show_mod("mod-042.jar", window, cx));
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        tab.read_with(cx, |t, _| t.highlighted().map(str::to_owned))
            .as_deref(),
        Some("mod-042.jar")
    );
    assert!(shows(&app, cx, mod_row_id("mod-042.jar")));
}

#[gpui_kit::test]
fn mod_rows_are_keyboard_reachable(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, mods) = mods_app(cx, tmp.path(), |dir| {
        fabric_jar(dir, "a.jar", "Alpha Mod");
        fabric_jar(dir, "b.jar", "Beta Mod");
        fabric_jar(dir, "c.jar", "Gamma Mod");
    });
    // A click hands the keyboard to the rows.
    click(&app, cx, mod_row_id("a.jar"));
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("mod-rows-region").focused(), Some(true));
    })
    .unwrap();
    let selected =
        |cx: &mut TestAppContext| tab.read_with(cx, |t, _| t.selected().map(str::to_owned));
    assert_eq!(selected(cx).as_deref(), Some("a.jar"));
    press(&app, cx, "up");
    assert_eq!(selected(cx).as_deref(), Some("a.jar"), "top stays");
    press(&app, cx, "down");
    assert_eq!(selected(cx).as_deref(), Some("b.jar"));

    // Space toggles the selected mod; the selection follows its new name.
    press(&app, cx, "space");
    settle(cx, |cx| {
        mods.join("b.jar.disabled").exists() && selected(cx).as_deref() == Some("b.jar.disabled")
    });

    // Delete asks first; Escape leaves it.
    press(&app, cx, "delete");
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.within("dialog").find("mods-delete-title").label(),
            Some("Delete “Beta Mod”?")
        );
    })
    .unwrap();
    press(&app, cx, "escape");
    assert!(!shows(&app, cx, "dialog"));
    assert!(mods.join("b.jar.disabled").exists());

    press(&app, cx, "end");
    assert_eq!(selected(cx).as_deref(), Some("c.jar"));
    press(&app, cx, "home");
    assert_eq!(selected(cx).as_deref(), Some("a.jar"));

    // Enter in the confirmation (no button chosen) deletes.
    press(&app, cx, "delete");
    assert!(shows(&app, cx, "dialog"));
    press(&app, cx, "enter");
    settle(cx, |_| !mods.join("a.jar").exists());
    assert!(!shows(&app, cx, "dialog"));
    settle(cx, |cx| !mod_files(&tab, cx).contains(&"a.jar".to_string()));
}

#[gpui_kit::test]
fn mods_toolbar_and_empty_states(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    // No mods folder at all yet.
    let (app, tab, mods) = mods_app(cx, tmp.path(), |dir| {
        std::fs::remove_dir(dir).unwrap();
    });
    for id in [
        "content-filter",
        "content-open-folder",
        "content-refresh",
        "mods-online",
    ] {
        assert!(shows(&app, cx, id), "{id} is on the toolbar");
    }
    assert_eq!(
        label_of(&app, cx, "content-empty").as_deref(),
        Some("No mods yet. Put jar files in the folder, or get some from Downloads.")
    );

    // A mod put in by hand shows after a refresh.
    std::fs::create_dir_all(&mods).unwrap();
    fabric_jar(&mods, "a.jar", "Alpha Mod");
    click(&app, cx, "content-refresh");
    mods_settled(&tab, cx);
    assert!(shows(&app, cx, mod_row_id("a.jar")));
    assert!(!shows(&app, cx, "content-empty"));

    // Refresh is disabled while a listing runs.
    let list = tab.read_with(cx, |t, _| t.list().clone());
    cx.update_window(app.handle, |_, window, cx| {
        list.update(cx, |l, cx| l.reload(cx));
        window.render_frame(cx);
        assert_eq!(window.find("content-refresh").disabled(), Some(true));
    })
    .unwrap();
    mods_settled(&tab, cx);
    assert!(!disabled(&app, cx, "content-refresh"));

    // A filter that matches nothing says so.
    click(&app, cx, "content-filter");
    type_text(&app, cx, "zzz");
    assert_eq!(
        label_of(&app, cx, "content-empty").as_deref(),
        Some("Nothing matches “zzz”.")
    );
    assert!(!shows(&app, cx, mod_row_id("a.jar")));

    // "Get online" goes to the downloads page.
    click(&app, cx, "mods-online");
    let nav = cx.update(|cx| app.root.read(cx).shell().read(cx).nav());
    assert_eq!(nav, Nav::Downloads);
}

/// Writes a jar `file` with `entries` into `dir`.
fn jar_with(dir: &Path, file: &str, entries: &[(&str, &[u8])]) {
    use std::io::Write as _;
    let mut w = zip::ZipWriter::new(std::fs::File::create(dir.join(file)).unwrap());
    for (name, body) in entries {
        w.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(body).unwrap();
    }
    w.finish().unwrap();
}

#[gpui_kit::test]
fn a_toggle_during_a_listing_keeps_its_row(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, mods) = mods_app(cx, tmp.path(), |dir| {
        for i in 0..200 {
            fabric_jar(dir, &format!("mod-{i:03}.jar"), &format!("Mod {i:03}"));
        }
    });
    let (changed, _c) = count_mods_changed(cx);
    // A listing is in flight when the toggle lands, whichever finishes
    // first.
    let list = tab.read_with(cx, |t, _| t.list().clone());
    list.update(cx, |l, cx| l.reload(cx));
    click(&app, cx, mod_switch_id("mod-000.jar"));
    settle(cx, |cx| {
        let listing = list.read_with(cx, |l, _| l.is_reloading());
        !listing && tab.read_with(cx, |t, _| t.changes_in_flight()) == 0
    });
    mods_settled(&tab, cx);
    assert!(mods.join("mod-000.jar.disabled").exists());
    let files = mod_files(&tab, cx);
    assert_eq!(files[0], "mod-000.jar.disabled");
    assert_eq!(files.len(), 200);
    assert_eq!(
        tab.read_with(cx, |t, _| t.selected().map(str::to_owned))
            .as_deref(),
        Some("mod-000.jar.disabled")
    );
    // The toggle told the search once; the listings found nothing else.
    assert_eq!(changed.get(), 1);
}

#[gpui_kit::test]
fn icons_load_only_for_rows_on_show(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let png = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/instance-icons/sword.png"),
    )
    .unwrap();
    let (app, tab, _mods) = mods_app(cx, tmp.path(), |dir| {
        for i in 0..150 {
            let json = format!(r#"{{"id":"m{i}","name":"Mod {i:03}","icon":"icon.png"}}"#);
            jar_with(
                dir,
                &format!("mod-{i:03}.jar"),
                &[("fabric.mod.json", json.as_bytes()), ("icon.png", &png)],
            );
        }
    });
    let cached = |cx: &mut TestAppContext| {
        cx.update(|cx| cx.global::<patina_app::content::ImageCache>().len())
    };
    let drawn = |cx: &mut TestAppContext| {
        cx.update_window(app.handle, |_, window, cx| {
            window.render_frame(cx);
            (0..150)
                .filter(|i| {
                    let id = mod_row_id(&format!("mod-{i:03}.jar"));
                    window.try_find(id).is_some()
                })
                .count()
        })
        .unwrap()
    };
    // The rows on show load their icons, read from their jars again.
    settle(cx, |cx| {
        let drawn = drawn(cx);
        drawn > 0 && cached(cx) == drawn
    });
    let shown = drawn(cx);
    assert!(shown < 50, "{shown} rows drawn");
    // Every item knows it has an icon, but only the drawn rows' icons are
    // held (in the capped image cache); the items keep no icon bytes.
    let with_icon = tab.read_with(cx, |t, cx| {
        (t.list().read(cx).items().iter())
            .filter(|m| m.info.as_ref().is_some_and(|i| i.has_icon))
            .count()
    });
    assert_eq!(with_icon, 150);
    assert_eq!(cached(cx), shown);
}

#[gpui_kit::test]
fn a_hidden_mods_tab_does_not_list(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, _mods) = mods_app(cx, tmp.path(), |dir| {
        fabric_jar(dir, "a.jar", "Alpha Mod");
    });
    let (listings, _l) = count_listings(&tab, cx);
    let refocus = |cx: &mut TestAppContext| {
        gpui_kit::VisualTestContext::from_window(app.handle, cx).deactivate_window();
        activate(&app, cx);
    };
    // In view, coming back to the window lists the folder again.
    refocus(cx);
    mods_settled(&tab, cx);
    assert_eq!(listings.get(), 1);

    // On another page, neither the window nor the instance coming free
    // lists it.
    click(&app, cx, "rail-downloads");
    refocus(cx);
    let model = instances_model(&app, cx);
    model.update(cx, |m, cx| m.set_run_state("Alpha", Some(elsewhere()), cx));
    model.update(cx, |m, cx| m.set_run_state("Alpha", None, cx));
    cx.run_until_parked();
    assert!(!tab.read_with(cx, |t, cx| t.list().read(cx).is_reloading()));
    assert_eq!(listings.get(), 1);

    // Back on the page it lists at once.
    click(&app, cx, "rail-instances");
    mods_settled(&tab, cx);
    assert_eq!(listings.get(), 2);

    // On another tab of the detail, the same.
    click(&app, cx, "tab-overview");
    refocus(cx);
    assert!(!tab.read_with(cx, |t, cx| t.list().read(cx).is_reloading()));
    assert_eq!(listings.get(), 2);
}

#[gpui_kit::test]
fn a_loader_change_reads_the_metadata_again(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let mods = store
        .create("Alpha", "1.21.1")
        .unwrap()
        .game_dir()
        .join("mods");
    set_loader(&store, "Alpha", ComponentUid::Fabric);
    std::fs::create_dir_all(&mods).unwrap();
    // One jar for both loaders, named differently by each.
    jar_with(
        &mods,
        "both.jar",
        &[
            (
                "fabric.mod.json",
                br#"{"id":"both","name":"Fabric Side","version":"1"}"#,
            ),
            (
                "META-INF/neoforge.mods.toml",
                b"modLoader=\"javafml\"\n[[mods]]\nmodId=\"both\"\ndisplayName=\"Neo Side\"\nversion=\"1\"\n",
            ),
        ],
    );
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, row_id("Alpha"));
    click(&app, cx, "tab-mods");
    let tab = mods_tab(&app, cx);
    mods_settled(&tab, cx);
    let name = |cx: &mut TestAppContext| {
        tab.read_with(cx, |t, cx| t.list().read(cx).items()[0].name().to_string())
    };
    assert_eq!(name(cx), "Fabric Side");

    set_loader(&store, "Alpha", ComponentUid::Neoforge);
    refresh(&model, cx);
    settle(cx, |cx| name(cx) == "Neo Side");
    mods_settled(&tab, cx);
    assert_eq!(name(cx), "Neo Side");
}

#[gpui_kit::test]
fn a_taken_name_has_its_own_message(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, mods) = mods_app(cx, tmp.path(), |dir| {
        fabric_jar(dir, "a.jar", "Alpha Mod");
        fabric_jar(dir, "a.jar.disabled", "Alpha Mod");
    });
    app.language(Language::ZhCn);
    // Disabling a.jar would take a.jar.disabled's name.
    click(&app, cx, mod_switch_id("a.jar"));
    settle(cx, |cx| label_of(&app, cx, NOTICE_TEXT).is_some());
    assert_eq!(
        label_of(&app, cx, NOTICE_TEXT).as_deref(),
        Some("同名文件已存在")
    );
    mods_settled(&tab, cx);
    assert_eq!(mod_files(&tab, cx), ["a.jar", "a.jar.disabled"]);
    assert!(mods.join("a.jar").exists());
}

#[gpui_kit::test]
fn the_first_listing_tells_no_one(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let mods = store
        .create("Alpha", "1.21.1")
        .unwrap()
        .game_dir()
        .join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    fabric_jar(&mods, "a.jar", "Alpha Mod");
    let app = boot(cx, tmp.path(), AppSettings::default());
    let (changed, _c) = count_mods_changed(cx);
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, row_id("Alpha"));
    click(&app, cx, "tab-mods");
    let tab = mods_tab(&app, cx);
    mods_settled(&tab, cx);
    assert_eq!(mod_files(&tab, cx), ["a.jar"]);
    assert_eq!(changed.get(), 0, "nothing changed since the search read it");

    // A listing that finds the same folder tells no one either.
    click(&app, cx, "content-refresh");
    mods_settled(&tab, cx);
    assert_eq!(changed.get(), 0);
}

// ---- Resource packs, shaders and saves tabs ----

use patina_app::content::{LocalKind, LocalTab, format_last_played, local_delete_id, local_row_id};

fn local_tab_of(app: &Booted, cx: &mut TestAppContext, kind: LocalKind) -> Entity<LocalTab> {
    cx.update(|cx| {
        let detail = app.root.read(cx).instances().read(cx).detail().clone();
        let detail = detail.read(cx);
        match kind {
            LocalKind::ResourcePacks => detail.packs_view(),
            LocalKind::Shaders => detail.shaders_view(),
            LocalKind::Saves => detail.saves_view(),
        }
        .cloned()
        .expect("the tab was shown")
    })
}

/// Instance "Alpha" whose `folder` of the game dir is filled by `fill`;
/// the app booted, Alpha selected and the tab of `kind` shown with the
/// listing landed. Returns the app, the tab and the folder.
fn local_app(
    cx: &mut TestAppContext,
    dir: &Path,
    kind: LocalKind,
    fill: impl FnOnce(&Path),
) -> (Booted, Entity<LocalTab>, std::path::PathBuf) {
    let store = InstanceStore::new(DataDirs::new(dir).instances());
    let alpha = store.create("Alpha", "1.21.1").unwrap();
    let folder = alpha.game_dir().join(match kind {
        LocalKind::ResourcePacks => "resourcepacks",
        LocalKind::Shaders => "shaderpacks",
        LocalKind::Saves => "saves",
    });
    std::fs::create_dir_all(&folder).unwrap();
    fill(&folder);
    let app = boot(cx, dir, AppSettings::default());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, row_id("Alpha"));
    click(
        &app,
        cx,
        match kind {
            LocalKind::ResourcePacks => "tab-resource-packs",
            LocalKind::Shaders => "tab-shaders",
            LocalKind::Saves => "tab-worlds",
        },
    );
    let tab = local_tab_of(&app, cx, kind);
    local_settled(&tab, cx);
    (app, tab, folder)
}

fn local_settled(tab: &Entity<LocalTab>, cx: &mut TestAppContext) {
    settle(cx, |cx| {
        tab.read_with(cx, |t, cx| {
            let list = t.list().read(cx);
            !list.is_reloading() && *list.state() == patina_app::content::LoadState::Loaded
        })
    });
}

fn local_keys(tab: &Entity<LocalTab>, cx: &mut TestAppContext) -> Vec<String> {
    tab.read_with(cx, |t, cx| {
        (t.list().read(cx).items().iter())
            .map(|i| i.key.clone())
            .collect()
    })
}

fn tiny_png() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/instance-icons/sword.png"))
        .unwrap()
}

fn mcmeta(description: &str) -> String {
    format!(r#"{{"pack":{{"pack_format":34,"description":"{description}"}}}}"#)
}

/// An uncompressed `level.dat` with the given name and last play.
fn level_dat(name: Option<&str>, last_played: Option<i64>) -> Vec<u8> {
    let mut v = vec![10, 0, 0, 10, 0, 4];
    v.extend(b"Data");
    if let Some(name) = name {
        v.extend([8, 0, 9]);
        v.extend(b"LevelName");
        v.extend((name.len() as u16).to_be_bytes());
        v.extend(name.as_bytes());
    }
    if let Some(ms) = last_played {
        v.extend([4, 0, 10]);
        v.extend(b"LastPlayed");
        v.extend(ms.to_be_bytes());
    }
    v.extend([0, 0]);
    v
}

fn world(saves: &Path, dir: &str, name: Option<&str>, last_played: Option<i64>) {
    let path = saves.join(dir);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("level.dat"), level_dat(name, last_played)).unwrap();
}

#[gpui_kit::test]
fn packs_show_description_and_icon(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let png = tiny_png();
    let (app, tab, _folder) = local_app(cx, tmp.path(), LocalKind::ResourcePacks, |dir| {
        // A folder pack and a zip pack, both with an icon; a plain zip.
        let folder = dir.join("Faithful");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("pack.mcmeta"), mcmeta("Looks like vanilla")).unwrap();
        std::fs::write(folder.join("pack.png"), &png).unwrap();
        jar_with(
            dir,
            "Zipped.zip",
            &[
                (
                    "pack.mcmeta",
                    mcmeta(&"A very long description ".repeat(20)).as_bytes(),
                ),
                ("pack.png", &png),
            ],
        );
        jar_with(dir, "Plain.zip", &[("assets/x", b"x")]);
        std::fs::write(dir.join("notes.txt"), "not a pack").unwrap();
    });
    assert_eq!(
        local_keys(&tab, cx),
        ["Faithful", "Plain.zip", "Zipped.zip"],
        "folders and zips only"
    );
    let text = |cx: &mut TestAppContext, id: String| label_of(&app, cx, id);
    assert_eq!(
        text(cx, "pack-detail-Faithful".into()).as_deref(),
        Some("Looks like vanilla")
    );
    assert_eq!(
        text(cx, "pack-aside-Faithful".into()).as_deref(),
        Some("Folder")
    );
    assert_eq!(
        text(cx, "pack-aside-Zipped.zip".into()).as_deref(),
        Some("Archive")
    );
    assert_eq!(
        text(cx, "pack-detail-Plain.zip".into()).as_deref(),
        Some("")
    );
    // The description is one line, whatever its length.
    assert!(
        text(cx, "pack-detail-Zipped.zip".into())
            .unwrap()
            .starts_with("A very long description A very")
    );

    // The icons of the rows on show are read on demand into the image
    // cache; the items keep only whether there is one.
    let cached = |cx: &mut TestAppContext| {
        cx.update(|cx| cx.global::<patina_app::content::ImageCache>().len())
    };
    settle(cx, |cx| cached(cx) == 2);
    let flags = tab.read_with(cx, |t, cx| {
        (t.list().read(cx).items().iter())
            .map(|i| i.has_icon)
            .collect::<Vec<_>>()
    });
    assert_eq!(flags, [true, false, true]);

    // Switching the language relabels the kind without a reload.
    app.language(Language::ZhCn);
    assert_eq!(
        text(cx, "pack-aside-Faithful".into()).as_deref(),
        Some("文件夹")
    );
    assert_eq!(
        text(cx, "pack-aside-Zipped.zip".into()).as_deref(),
        Some("压缩包")
    );
}

#[gpui_kit::test]
fn shader_option_files_hidden(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, _folder) = local_app(cx, tmp.path(), LocalKind::Shaders, |dir| {
        jar_with(dir, "Complementary.zip", &[("shaders/a.fsh", b"x")]);
        std::fs::write(dir.join("Complementary.zip.txt"), "opt=1").unwrap();
        std::fs::create_dir(dir.join("bsl")).unwrap();
        std::fs::write(dir.join("readme.md"), "x").unwrap();
    });
    assert_eq!(local_keys(&tab, cx), ["bsl", "Complementary.zip"]);
    assert!(shows(&app, cx, local_row_id(LocalKind::Shaders, "bsl")));
    assert!(shows(
        &app,
        cx,
        local_row_id(LocalKind::Shaders, "Complementary.zip")
    ));
    assert!(!shows(
        &app,
        cx,
        local_row_id(LocalKind::Shaders, "Complementary.zip.txt")
    ));
    assert_eq!(
        label_of(&app, cx, "shader-aside-bsl").as_deref(),
        Some("Folder")
    );
    assert_eq!(
        label_of(&app, cx, "shader-aside-Complementary.zip").as_deref(),
        Some("Archive")
    );
}

#[gpui_kit::test]
fn saves_sorted_and_formatted(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let older = 1_700_000_000_000;
    let newer = 1_793_363_696_789;
    let (app, tab, _folder) = local_app(cx, tmp.path(), LocalKind::Saves, |dir| {
        world(dir, "w-old", Some("Old World"), Some(older));
        world(dir, "w-new", Some("New 世界"), Some(newer));
        // No name: the folder's. No metadata at all: last.
        world(dir, "w-unnamed", None, Some(older + 1000));
        std::fs::create_dir(dir.join("w-bare")).unwrap();
        std::fs::write(dir.join("w-new").join("icon.png"), tiny_png()).unwrap();
        std::fs::write(dir.join("stray.txt"), "x").unwrap();
    });
    // Newest first, worlds without a last play after them.
    assert_eq!(
        local_keys(&tab, cx),
        ["w-new", "w-unnamed", "w-old", "w-bare"]
    );
    let titles = tab.read_with(cx, |t, cx| {
        (t.list().read(cx).items().iter())
            .map(|i| i.title.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(titles, ["New 世界", "w-unnamed", "Old World", "w-bare"]);

    // The last play is local time, `YYYY-MM-DD HH:mm`.
    let expect = |ms: i64| {
        use chrono::TimeZone as _;
        let t = chrono::Local.timestamp_millis_opt(ms).single().unwrap();
        format!("Last played {}", t.format("%Y-%m-%d %H:%M"))
    };
    assert_eq!(format_last_played(newer).len(), "2026-10-30 12:34".len());
    assert_eq!(label_of(&app, cx, "save-detail-w-new"), Some(expect(newer)));
    assert_eq!(label_of(&app, cx, "save-detail-w-old"), Some(expect(older)));
    // Nothing known, nothing shown.
    assert_eq!(
        label_of(&app, cx, "save-detail-w-bare").as_deref(),
        Some("")
    );
    // The folder name sits at the right.
    assert_eq!(
        label_of(&app, cx, "save-aside-w-new").as_deref(),
        Some("w-new")
    );
    // Only the world with an icon has one.
    let cached = |cx: &mut TestAppContext| {
        cx.update(|cx| cx.global::<patina_app::content::ImageCache>().len())
    };
    settle(cx, |cx| cached(cx) == 1);

    app.language(Language::ZhCn);
    assert_eq!(
        label_of(&app, cx, "save-detail-w-old"),
        Some(expect(older).replace("Last played", "上次游玩"))
    );
}

#[gpui_kit::test]
fn saves_have_no_delete(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, folder) = local_app(cx, tmp.path(), LocalKind::Saves, |dir| {
        world(dir, "w1", Some("One"), Some(1));
    });
    assert!(shows(&app, cx, local_row_id(LocalKind::Saves, "w1")));
    assert!(!shows(&app, cx, local_delete_id(LocalKind::Saves, "w1")));
    // The Delete key does nothing either; open folder and refresh stay.
    click(&app, cx, local_row_id(LocalKind::Saves, "w1"));
    press(&app, cx, "delete");
    assert!(!shows(&app, cx, "dialog"));
    tab.update(cx, |t, _| assert_eq!(t.changes_in_flight(), 0));
    assert!(folder.join("w1").exists());
    assert!(shows(&app, cx, "content-open-folder"));
    assert!(shows(&app, cx, "content-refresh"));
    // Not even while the instance is idle for the busy line.
    let model = instances_model(&app, cx);
    model.update(cx, |m, cx| m.set_run_state("Alpha", Some(elsewhere()), cx));
    cx.run_until_parked();
    assert!(!shows(&app, cx, "save-busy"));
}

#[gpui_kit::test]
fn pack_delete_asks_with_focus_on_cancel(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, folder) = local_app(cx, tmp.path(), LocalKind::ResourcePacks, |dir| {
        jar_with(dir, "a.zip", &[("pack.mcmeta", mcmeta("A").as_bytes())]);
        let b = dir.join("b-folder");
        std::fs::create_dir_all(b.join("assets/minecraft")).unwrap();
        std::fs::write(b.join("assets/minecraft/f"), "x").unwrap();
        jar_with(dir, "c.zip", &[]);
    });
    let id = |k: &str| local_row_id(LocalKind::ResourcePacks, k);
    let selected =
        |cx: &mut TestAppContext| tab.read_with(cx, |t, _| t.selected().map(str::to_owned));

    // The rows are keyboard reachable.
    click(&app, cx, id("a.zip"));
    assert_eq!(selected(cx).as_deref(), Some("a.zip"));
    press(&app, cx, "up");
    assert_eq!(selected(cx).as_deref(), Some("a.zip"), "top stays");
    press(&app, cx, "down");
    assert_eq!(selected(cx).as_deref(), Some("b-folder"));
    press(&app, cx, "end");
    assert_eq!(selected(cx).as_deref(), Some("c.zip"));
    press(&app, cx, "home");
    assert_eq!(selected(cx).as_deref(), Some("a.zip"));

    // Delete asks, naming the file; the keyboard is on 取消, and Enter
    // there cancels.
    press(&app, cx, "delete");
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.within("dialog").find("pack-delete-title").label(),
            Some("Delete “a.zip”?")
        );
        assert_eq!(
            window.within("dialog").find("pack-delete-body").label(),
            Some("The file will be deleted permanently.")
        );
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.within("dialog").find("cancel").focused(),
            Some(true),
            "focus starts on 取消"
        );
    })
    .unwrap();
    press(&app, cx, "enter");
    assert!(!shows(&app, cx, "dialog"));
    cx.executor().advance_clock(Duration::from_millis(50));
    cx.run_until_parked();
    assert!(folder.join("a.zip").exists(), "Enter on 取消 cancels");

    // Escape leaves it too.
    press(&app, cx, "delete");
    assert!(shows(&app, cx, "dialog"));
    press(&app, cx, "escape");
    assert!(!shows(&app, cx, "dialog"));
    assert!(folder.join("a.zip").exists());

    // 删除 on the folder pack says the folder goes with its contents.
    click(
        &app,
        cx,
        local_delete_id(LocalKind::ResourcePacks, "b-folder"),
    );
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.within("dialog").find("pack-delete-body").label(),
            Some("The folder and everything in it will be deleted permanently.")
        );
    })
    .unwrap();
    dialog_click(&app, cx, "ok");
    settle(cx, |_| !folder.join("b-folder").exists());
    settle(cx, |cx| {
        !local_keys(&tab, cx).contains(&"b-folder".to_string())
    });
    assert_eq!(local_keys(&tab, cx), ["a.zip", "c.zip"]);
    assert!(folder.join("a.zip").exists() && folder.join("c.zip").exists());
    assert_eq!(selected(cx).as_deref(), Some("c.zip"), "the next row");
}

#[gpui_kit::test]
fn pack_changes_are_refused_while_busy(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, folder) = local_app(cx, tmp.path(), LocalKind::Shaders, |dir| {
        jar_with(dir, "s.zip", &[("shaders/a.fsh", b"x")]);
    });
    let model = instances_model(&app, cx);
    model.update(cx, |m, cx| m.set_run_state("Alpha", Some(elsewhere()), cx));
    cx.run_until_parked();
    assert!(disabled(
        &app,
        cx,
        local_delete_id(LocalKind::Shaders, "s.zip")
    ));
    assert_eq!(
        label_of(&app, cx, "shader-busy").as_deref(),
        Some("The game is running, so this folder can’t be changed now")
    );
    click(&app, cx, local_row_id(LocalKind::Shaders, "s.zip"));
    press(&app, cx, "delete");
    assert!(!shows(&app, cx, "dialog"));
    assert_eq!(tab.read_with(cx, |t, _| t.changes_in_flight()), 0);
    assert!(folder.join("s.zip").exists());
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let notices = window.find_all(NOTICE_TEXT);
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].label(),
            Some("The game is running, so this folder can’t be changed now")
        );
    })
    .unwrap();

    // When the game ends, the tab lists again (it may have written).
    jar_with(&folder, "t.zip", &[("shaders/b.fsh", b"x")]);
    model.update(cx, |m, cx| m.set_run_state("Alpha", None, cx));
    settle(cx, |cx| local_keys(&tab, cx) == ["s.zip", "t.zip"]);
    assert!(!shows(&app, cx, "shader-busy"));
    assert!(!disabled(
        &app,
        cx,
        local_delete_id(LocalKind::Shaders, "s.zip")
    ));
}

#[gpui_kit::test]
fn local_toolbar_and_empty_states(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    // No folder at all yet.
    let (app, tab, folder) = local_app(cx, tmp.path(), LocalKind::Saves, |dir| {
        std::fs::remove_dir(dir).unwrap();
    });
    for id in ["content-filter", "content-open-folder", "content-refresh"] {
        assert!(shows(&app, cx, id), "{id} is on the toolbar");
    }
    assert_eq!(
        label_of(&app, cx, "content-empty").as_deref(),
        Some("No worlds yet. Worlds you create in the game appear here.")
    );

    // A world put in by hand shows after a refresh.
    std::fs::create_dir_all(&folder).unwrap();
    world(&folder, "w1", Some("One"), Some(5));
    click(&app, cx, "content-refresh");
    local_settled(&tab, cx);
    assert!(shows(&app, cx, local_row_id(LocalKind::Saves, "w1")));

    // A filter matches the world's name and its folder's.
    click(&app, cx, "content-filter");
    type_text(&app, cx, "zzz");
    assert_eq!(
        label_of(&app, cx, "content-empty").as_deref(),
        Some("Nothing matches “zzz”.")
    );
}

#[gpui_kit::test]
fn a_hidden_local_tab_does_not_list(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, tab, folder) = local_app(cx, tmp.path(), LocalKind::ResourcePacks, |dir| {
        jar_with(dir, "a.zip", &[]);
    });
    let list = tab.read_with(cx, |t, _| t.list().clone());
    let listings = Rc::new(std::cell::Cell::new(0));
    let _s = cx.update(|cx| {
        let listings = listings.clone();
        cx.subscribe(&list, move |_, _: &patina_app::content::Reloaded, _| {
            listings.set(listings.get() + 1)
        })
    });
    let refocus = |cx: &mut TestAppContext| {
        gpui_kit::VisualTestContext::from_window(app.handle, cx).deactivate_window();
        activate(&app, cx);
    };
    refocus(cx);
    local_settled(&tab, cx);
    assert_eq!(listings.get(), 1, "in view, the window listing again");

    // On another page: neither the window nor the instance coming free.
    click(&app, cx, "rail-downloads");
    refocus(cx);
    let model = instances_model(&app, cx);
    model.update(cx, |m, cx| m.set_run_state("Alpha", Some(elsewhere()), cx));
    model.update(cx, |m, cx| m.set_run_state("Alpha", None, cx));
    cx.run_until_parked();
    assert!(!list.read_with(cx, |l, _| l.is_reloading()));
    assert_eq!(listings.get(), 1);

    // Back on the page, at once.
    jar_with(&folder, "b.zip", &[]);
    click(&app, cx, "rail-instances");
    local_settled(&tab, cx);
    assert_eq!(listings.get(), 2);
    assert_eq!(local_keys(&tab, cx), ["a.zip", "b.zip"]);

    // On another tab of the detail, the same.
    click(&app, cx, "tab-overview");
    refocus(cx);
    assert!(!list.read_with(cx, |l, _| l.is_reloading()));
    assert_eq!(listings.get(), 2);
}

// ---- Global search ----

use patina_app::search::{
    DEBOUNCE, GlobalSearch, IndexState, ModGroup, REFRESH_GAP, SearchItem, SearchModel,
    SearchResults, search_row_id,
};

fn search_view(app: &Booted, cx: &mut TestAppContext) -> Entity<GlobalSearch> {
    cx.update(|cx| app.root.read(cx).title_bar().read(cx).search().clone())
}

fn search_model(app: &Booted, cx: &mut TestAppContext) -> Entity<SearchModel> {
    let view = search_view(app, cx);
    view.read_with(cx, |s, _| s.model().clone())
}

/// Waits until the index has been built once and no refresh is in flight.
fn indexed(model: &Entity<SearchModel>, cx: &mut TestAppContext) {
    settle(cx, |cx| {
        model.read_with(cx, |m, _| m.is_indexed() && !m.is_refreshing())
    });
}

fn results(model: &Entity<SearchModel>, cx: &mut TestAppContext) -> Option<SearchResults> {
    model.read_with(cx, |m, _| m.results().cloned())
}

/// Types `text` into the search box (which has the keyboard), lets the
/// debounce pass and waits for the results of exactly `text`.
fn search_for(app: &Booted, cx: &mut TestAppContext, text: &str) {
    type_text(app, cx, text);
    cx.executor().advance_clock(DEBOUNCE);
    let model = search_model(app, cx);
    settle(cx, |cx| {
        model.read_with(cx, |m, _| m.results().is_some_and(|r| r.query == m.query()))
    });
}

fn search_value(app: &Booted, cx: &mut TestAppContext) -> String {
    element(app, cx, "search")
        .and_then(|e| e.value().map(str::to_owned))
        .unwrap_or_default()
}

fn nav(app: &Booted, cx: &mut TestAppContext) -> Nav {
    cx.update(|cx| app.root.read(cx).shell().read(cx).nav())
}

fn mod_item(instance_id: &str, file_name: &str) -> SearchItem {
    SearchItem::Mod {
        instance_id: instance_id.into(),
        file_name: file_name.into(),
    }
}

#[gpui_kit::test]
fn groups_and_limits(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let packs: Vec<String> = (1..=7)
        .map(|i| store.create(&format!("Pack {i}"), "1.21.1").unwrap().id)
        .collect();
    let home = store.create("Mods Home", "1.21.1").unwrap();
    let mods = home.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    for i in 0..25 {
        fabric_jar(
            &mods,
            &format!("pack-mod-{i:02}.jar"),
            &format!("Pack Mod {i:02}"),
        );
    }
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = search_model(&app, cx);
    list_settled(&instances_model(&app, cx), cx);
    indexed(&model, cx);
    click(&app, cx, "rail-settings");

    press(&app, cx, "/");
    search_for(&app, cx, "pack");
    let found = results(&model, cx).unwrap();
    let names: Vec<_> = found.instances.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["Pack 1", "Pack 2", "Pack 3", "Pack 4", "Pack 5"]);
    let mods = found.mod_hits();
    assert_eq!(mods.len(), 20);
    assert_eq!(mods[0].hit.name, "Pack Mod 00");
    assert_eq!(mods[0].instance_name, "Mods Home");
    let items = found.items();
    assert_eq!(items.len(), 5 + 20 + 1);
    assert_eq!(items.last(), Some(&SearchItem::Online));

    // Labelled groups; the first result is chosen. The long list
    // scrolls: "online" is below the fold for now.
    for group in ["search-group-instances", "search-group-mods"] {
        assert!(shows(&app, cx, group), "{group}");
    }
    assert!(!shows(&app, cx, search_row_id(&SearchItem::Online)));
    assert!(!shows(&app, cx, "search-indexing"));
    let first = element(
        &app,
        cx,
        search_row_id(&SearchItem::Instance(packs[0].clone())),
    )
    .unwrap();
    assert_eq!(first.selected(), Some(true));
    let row = element(
        &app,
        cx,
        search_row_id(&mod_item(&home.id, "pack-mod-00.jar")),
    )
    .unwrap();
    assert_eq!(row.label(), Some("Pack Mod 00 1.0 — Mods Home"));

    // A click on an instance shows it on the instances page.
    click(
        &app,
        cx,
        search_row_id(&SearchItem::Instance(packs[2].clone())),
    );
    assert_eq!(nav(&app, cx), Nav::Instances);
    assert_eq!(
        selected(&instances_model(&app, cx), cx),
        Some(packs[2].clone())
    );
    assert!(!shows(&app, cx, "search-popup"));
    assert_eq!(search_value(&app, cx), "");

    // Up from the first result wraps to "online", which opens the
    // downloads page.
    press(&app, cx, "/");
    search_for(&app, cx, "pack");
    press(&app, cx, "up");
    let view = search_view(&app, cx);
    assert_eq!(
        view.read_with(cx, |v, cx| v.chosen(cx)),
        Some(SearchItem::Online)
    );
    // The chosen row is scrolled into view.
    assert!(shows(&app, cx, search_row_id(&SearchItem::Online)));
    press(&app, cx, "enter");
    assert_eq!(nav(&app, cx), Nav::Downloads);
    assert!(!shows(&app, cx, "search-popup"));

    // Nothing local matches: said so, online still offered.
    press(&app, cx, "/");
    search_for(&app, cx, "zzz");
    assert!(shows(&app, cx, "search-no-local"));
    assert!(shows(&app, cx, "search-group-online"));
    assert!(shows(&app, cx, search_row_id(&SearchItem::Online)));
}

#[gpui_kit::test]
fn stale_search_results_are_dropped(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let alpha = store.create("Alpha", "1.21.1").unwrap();
    store.create("Beta", "1.21.1").unwrap();
    let mods = alpha.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    fabric_jar(&mods, "sodium.jar", "Sodium");
    fabric_jar(&mods, "lithium.jar", "Lithium");
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = search_model(&app, cx);

    // Before the index is built, the mods group says it is being built.
    let held = model.read_with(cx, |m, _| m.hold_refreshes());
    list_settled(&instances_model(&app, cx), cx);
    press(&app, cx, "/");
    search_for(&app, cx, "a");
    assert_eq!(results(&model, cx).unwrap().mods, ModGroup::Indexing);
    assert!(shows(&app, cx, "search-indexing"));
    drop(held);
    indexed(&model, cx);
    // The index landing fills the group in.
    settle(cx, |cx| {
        results(&model, cx).is_some_and(|r| matches!(r.mods, ModGroup::Hits(_)))
    });
    assert!(!shows(&app, cx, "search-indexing"));

    // Every applied result is recorded.
    let applied = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let _observe = cx.update(|cx| {
        let applied = applied.clone();
        cx.observe(&model, move |model, cx| {
            if let Some(r) = model.read(cx).results() {
                let mut applied = applied.borrow_mut();
                if applied.last() != Some(&r.query) {
                    applied.push(r.query.clone());
                }
            }
        })
    });

    // Quick typing: only the last words are searched.
    press(&app, cx, "escape");
    press(&app, cx, "/");
    for ch in ["s", "o", "d"] {
        type_text(&app, cx, ch);
        cx.executor().advance_clock(DEBOUNCE / 3);
        cx.run_until_parked();
    }
    // A search still in flight when the words change is dropped: the
    // searches are held at their gate meanwhile.
    let held = model.read_with(cx, |m, _| m.hold_searches());
    cx.executor().advance_clock(DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(results(&model, cx), None, "\"sod\" is in flight");
    type_text(&app, cx, "i");
    cx.executor().advance_clock(DEBOUNCE);
    cx.run_until_parked();
    drop(held);
    settle(cx, |cx| {
        results(&model, cx).is_some_and(|r| r.query == "sodi")
    });
    cx.run_until_parked();
    let last = results(&model, cx).unwrap();
    let names: Vec<_> = last.mod_hits().iter().map(|m| m.hit.name.clone()).collect();
    assert_eq!(names, ["Sodium"]);
    assert!(
        !applied
            .borrow()
            .iter()
            .any(|q| q == "s" || q == "so" || q == "sod"),
        "applied: {:?}",
        applied.borrow()
    );
    assert_eq!(applied.borrow().last().map(String::as_str), Some("sodi"));
}

#[gpui_kit::test]
fn enter_on_mod_hit_navigates_and_focuses(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    store.create("Alpha", "1.21.1").unwrap();
    let beta = store.create("Beta", "1.21.1").unwrap();
    let mods = beta.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    for i in 0..60 {
        fabric_jar(&mods, &format!("mod-{i:03}.jar"), &format!("Mod {i:03}"));
    }
    fabric_jar(&mods, "sodium.jar", "Sodium");
    let app = boot(cx, tmp.path(), AppSettings::default());
    let instances = instances_model(&app, cx);
    list_settled(&instances, cx);
    let model = search_model(&app, cx);
    indexed(&model, cx);
    assert_eq!(selected(&instances, cx).as_deref(), Some("Alpha"));
    click(&app, cx, "rail-accounts");

    press(&app, cx, "/");
    search_for(&app, cx, "sodi");
    let sodium = mod_item(&beta.id, "sodium.jar");
    let view = search_view(&app, cx);
    assert_eq!(
        view.read_with(cx, |v, cx| v.chosen(cx)),
        Some(sodium.clone())
    );
    // Down and back up across the groups.
    press(&app, cx, "down");
    assert_eq!(
        view.read_with(cx, |v, cx| v.chosen(cx)),
        Some(SearchItem::Online)
    );
    press(&app, cx, "up");
    assert_eq!(view.read_with(cx, |v, cx| v.chosen(cx)), Some(sodium));
    press(&app, cx, "enter");

    assert_eq!(nav(&app, cx), Nav::Instances);
    assert_eq!(selected(&instances, cx), Some(beta.id.clone()));
    let detail = cx.update(|cx| app.root.read(cx).instances().read(cx).detail().clone());
    assert_eq!(
        detail.read_with(cx, |d, _| d.tab()),
        patina_app::instances::DetailTab::Mods
    );
    let tab = mods_tab(&app, cx);
    settle(cx, |cx| {
        tab.read_with(cx, |t, _| t.highlighted() == Some("sodium.jar"))
    });
    assert!(shows(&app, cx, mod_row_id("sodium.jar")));
    assert!(!shows(&app, cx, "search-popup"));
    assert_eq!(search_value(&app, cx), "");
    // The keyboard is in the Mods list, not the search box.
    assert_ne!(element(&app, cx, "search").unwrap().focused(), Some(true));
    assert_eq!(
        tab.read_with(cx, |t, _| t.selected().map(str::to_owned))
            .as_deref(),
        Some("sodium.jar")
    );
}

#[gpui_kit::test]
fn escape_restores_focus(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    store.create("Alpha", "1.21.1").unwrap();
    store.create("Beta", "1.21.1").unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    list_settled(&instances_model(&app, cx), cx);
    let model = search_model(&app, cx);
    indexed(&model, cx);
    click(&app, cx, row_id("Beta"));
    let focused = |cx: &mut TestAppContext| {
        cx.update_window(app.handle, |_, window, cx| window.focused(cx))
            .unwrap()
    };
    let before = focused(cx).expect("the list has the keyboard");

    press(&app, cx, "/");
    search_for(&app, cx, "al");
    assert!(shows(&app, cx, "search-popup"));
    assert_ne!(focused(cx).as_ref(), Some(&before));
    press(&app, cx, "escape");
    assert!(!shows(&app, cx, "search-popup"));
    assert_eq!(search_value(&app, cx), "");
    assert_eq!(focused(cx), Some(before.clone()), "the keyboard is back");

    // A click outside closes the popup too.
    press(&app, cx, "/");
    search_for(&app, cx, "al");
    assert!(shows(&app, cx, "search-popup"));
    click(&app, cx, "title-settings");
    assert!(!shows(&app, cx, "search-popup"));
}

#[gpui_kit::test]
fn index_refresh_is_throttled(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let alpha = store.create("Alpha", "1.21.1").unwrap();
    // A broken instance does not stop the index.
    let broken = store.create("Broken", "1.21.1").unwrap();
    break_config(&broken.dir);
    let mods = alpha.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    fabric_jar(&mods, "sodium.jar", "Sodium");
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = search_model(&app, cx);
    indexed(&model, cx);
    let refreshes = |cx: &mut TestAppContext| model.read_with(cx, |m, _| m.refreshes());
    // A refresh starts on gpui's thread, so right after parking one that
    // should not have started shows as in flight (or already counted).
    let idle = |cx: &mut TestAppContext| {
        cx.run_until_parked();
        assert!(!model.read_with(cx, |m, _| m.is_refreshing()));
    };
    // Let whatever startup queued run out, then leave a full gap.
    for _ in 0..2 {
        cx.executor().advance_clock(REFRESH_GAP);
        cx.run_until_parked();
        indexed(&model, cx);
    }
    cx.executor().advance_clock(REFRESH_GAP);
    idle(cx);
    let base = refreshes(cx);

    // A change after a full gap refreshes at once.
    fabric_jar(&mods, "lithium.jar", "Lithium");
    cx.update(|cx| ModsEvents::emit(&alpha.id, cx));
    indexed(&model, cx);
    settle(cx, |cx| refreshes(cx) == base + 1);
    let found = |cx: &mut TestAppContext, q: &str| {
        model.read_with(cx, |m, _| m.index().unwrap().search(q, 5).len())
    };
    assert_eq!(found(cx, "lithium"), 1);

    // More changes within the gap wait for its end, as one refresh.
    fabric_jar(&mods, "iris.jar", "Iris");
    cx.update(|cx| ModsEvents::emit(&alpha.id, cx));
    cx.update(|cx| ModsEvents::emit(&alpha.id, cx));
    idle(cx);
    assert_eq!(refreshes(cx), base + 1);
    cx.executor()
        .advance_clock(REFRESH_GAP - Duration::from_secs(1));
    idle(cx);
    assert_eq!(refreshes(cx), base + 1);
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    settle(cx, |cx| refreshes(cx) == base + 2);
    idle(cx);
    assert_eq!(found(cx, "iris"), 1);

    // Triggers while a refresh is in flight queue one rerun.
    let held = model.read_with(cx, |m, _| m.hold_refreshes());
    cx.executor().advance_clock(REFRESH_GAP);
    cx.update(|cx| ModsEvents::emit(&alpha.id, cx));
    cx.run_until_parked();
    assert!(model.read_with(cx, |m, _| m.is_refreshing()));
    cx.update(|cx| ModsEvents::emit(&alpha.id, cx));
    gpui_kit::VisualTestContext::from_window(app.handle, cx).deactivate_window();
    activate(&app, cx);
    cx.update(|cx| ModsEvents::emit(&alpha.id, cx));
    drop(held);
    settle(cx, |cx| refreshes(cx) == base + 3);
    idle(cx);
    cx.executor().advance_clock(REFRESH_GAP);
    cx.run_until_parked();
    settle(cx, |cx| refreshes(cx) == base + 4);
    cx.executor().advance_clock(REFRESH_GAP);
    idle(cx);
    assert_eq!(refreshes(cx), base + 4, "one rerun, not one per trigger");

    // Coming back to the window after a full gap refreshes.
    gpui_kit::VisualTestContext::from_window(app.handle, cx).deactivate_window();
    activate(&app, cx);
    settle(cx, |cx| refreshes(cx) == base + 5);
}

#[gpui_kit::test]
fn searches_never_wait_for_a_refresh(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let alpha = store.create("Alpha", "1.21.1").unwrap();
    let mods = alpha.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    fabric_jar(&mods, "sodium.jar", "Sodium");
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = search_model(&app, cx);
    list_settled(&instances_model(&app, cx), cx);
    indexed(&model, cx);

    // A refresh held in flight after a full gap.
    fabric_jar(&mods, "sodium-extra.jar", "Sodium Extra");
    cx.executor().advance_clock(REFRESH_GAP);
    cx.run_until_parked();
    indexed(&model, cx);
    let held = model.read_with(cx, |m, _| m.hold_refreshes());
    cx.executor().advance_clock(REFRESH_GAP);
    cx.update(|cx| ModsEvents::emit(&alpha.id, cx));
    cx.run_until_parked();
    assert!(model.read_with(cx, |m, _| m.is_refreshing()));

    // The search lands with the index as it was.
    press(&app, cx, "/");
    search_for(&app, cx, "sodium");
    let names = |cx: &mut TestAppContext| -> Vec<String> {
        let found = results(&model, cx).unwrap();
        found
            .mod_hits()
            .iter()
            .map(|m| m.hit.name.clone())
            .collect()
    };
    assert_eq!(names(cx), ["Sodium"]);
    assert!(model.read_with(cx, |m, _| m.is_refreshing()), "still held");

    // The refreshed copy replaces it and the words are searched again.
    drop(held);
    settle(cx, |cx| names(cx).len() == 2);
    assert_eq!(names(cx), ["Sodium", "Sodium Extra"]);
}

#[gpui_kit::test]
fn enter_acts_only_on_the_current_results(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    store.create("Alpha", "1.21.1").unwrap();
    let beta = store.create("Beta", "1.21.1").unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let instances = instances_model(&app, cx);
    list_settled(&instances, cx);
    let model = search_model(&app, cx);
    indexed(&model, cx);
    click(&app, cx, "rail-accounts");

    press(&app, cx, "/");
    search_for(&app, cx, "beta");
    let view = search_view(&app, cx);
    let beta_hit = SearchItem::Instance(beta.id.clone());
    assert_eq!(view.read_with(cx, |v, cx| v.chosen(cx)), Some(beta_hit));

    // New words, Enter before their results land: the old results stay on
    // show but nothing is opened.
    type_text(&app, cx, "x");
    assert_eq!(results(&model, cx).unwrap().query, "beta");
    assert!(shows(&app, cx, "search-popup"));
    assert_eq!(view.read_with(cx, |v, cx| v.chosen(cx)), None);
    press(&app, cx, "enter");
    assert_eq!(nav(&app, cx), Nav::Accounts);
    assert_eq!(selected(&instances, cx).as_deref(), Some("Alpha"));
    assert_eq!(search_value(&app, cx), "betax");

    // Once they land, Enter acts on them.
    cx.executor().advance_clock(DEBOUNCE);
    settle(cx, |cx| model.read_with(cx, |m, _| m.is_current()));
    assert_eq!(
        view.read_with(cx, |v, cx| v.chosen(cx)),
        Some(SearchItem::Online)
    );
    press(&app, cx, "enter");
    assert_eq!(nav(&app, cx), Nav::Downloads);
}

#[gpui_kit::test]
fn a_vanished_instance_says_so(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    store.create("Alpha", "1.21.1").unwrap();
    let beta = store.create("Beta", "1.21.1").unwrap();
    let mods = beta.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    fabric_jar(&mods, "sodium.jar", "Sodium");
    let app = boot(cx, tmp.path(), AppSettings::default());
    let instances = instances_model(&app, cx);
    list_settled(&instances, cx);
    let model = search_model(&app, cx);
    indexed(&model, cx);

    press(&app, cx, "/");
    search_for(&app, cx, "sodium");
    // Beta goes away behind the search's back.
    std::fs::remove_dir_all(&beta.dir).unwrap();
    refresh(&instances, cx);
    press(&app, cx, "enter");
    assert_eq!(
        label_of(&app, cx, NOTICE_TEXT).as_deref(),
        Some("Instance “Beta” no longer exists")
    );
    assert_eq!(selected(&instances, cx).as_deref(), Some("Alpha"));
    assert!(!shows(&app, cx, "search-popup"));
}

#[gpui_kit::test]
fn a_failed_index_says_so_and_retries(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    let store = InstanceStore::new(data.instances());
    let alpha = store.create("Alpha", "1.21.1").unwrap();
    let mods = alpha.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    fabric_jar(&mods, "sodium.jar", "Sodium");
    // The cache directory cannot be made: the index cannot be saved.
    std::fs::write(data.cache(), b"not a directory").unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = search_model(&app, cx);
    list_settled(&instances_model(&app, cx), cx);
    settle(cx, |cx| {
        model.read_with(cx, |m, _| {
            matches!(m.index_state(), IndexState::Failed(_)) && !m.is_refreshing()
        })
    });

    press(&app, cx, "/");
    search_for(&app, cx, "sodium");
    assert!(matches!(
        results(&model, cx).unwrap().mods,
        ModGroup::Failed(_)
    ));
    assert!(shows(&app, cx, "search-index-failed"));
    assert!(!shows(&app, cx, "search-no-local"));

    // The next trigger tries again, without waiting for the gap.
    std::fs::remove_file(data.cache()).unwrap();
    cx.update(|cx| ModsEvents::emit(&alpha.id, cx));
    indexed(&model, cx);
    settle(cx, |cx| {
        results(&model, cx).is_some_and(|r| r.mod_hits().len() == 1)
    });
    assert!(!shows(&app, cx, "search-index-failed"));
}
