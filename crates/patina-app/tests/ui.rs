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
