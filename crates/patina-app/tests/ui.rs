//! UI integration tests: the real app bootstrap in headless windows.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::Theme;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Pixels, TestAppContext, px};
use patina_app::core_bridge::testing::{self, FakeCatalog};
use patina_app::core_bridge::{AccountsChanged, Core, LoadError};
use patina_app::settings::{self, AppSettings, SettingsStore};
use patina_app::shell::{AccountLabel, Nav, RootView, StatusBar};
use patina_app::single_instance;
use patina_app::theme::{self, ProductColors};
use patina_core::accounts::AccountStore;
use patina_core::config::SourceChoice;
use patina_core::instance::InstanceStore;
use patina_core::paths::DataDirs;

/// The booted app: its main window, root view, and the runtime the core
/// services run on (keep it alive for the test).
struct Booted {
    handle: AnyWindowHandle,
    root: Entity<RootView>,
    _rt: tokio::runtime::Runtime,
}

/// Boots the app as `main` does, with its data dir and settings in `dir`
/// and an offline core, and opens the main window.
fn boot(cx: &mut TestAppContext, dir: &Path, start: AppSettings) -> Booted {
    let rt = patina_app::runtime::start();
    let core = testing::core(
        rt.handle().clone(),
        DataDirs::new(dir),
        Arc::new(FakeCatalog::new()),
    );
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
