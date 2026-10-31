//! UI integration tests: the real app bootstrap in headless windows.

use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::Theme;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Pixels, TestAppContext, px};
use patina_app::core_bridge::testing::{self, FakeCatalog};
use patina_app::core_bridge::{AccountsChanged, Core, LoadError};
use patina_app::i18n::Language;
use patina_app::instances::{
    Filter, IconChoice, IconPicked, IconPicker, IconSource, InstanceList, InstancesChanged,
    InstancesModel, RunState, forget_cached_image, icon_for, instance_icon, pick_id, row_id,
    save_icon,
};
use patina_app::launch::{OpError, OpKind, OpState};
use patina_app::settings::{self, AppSettings, SettingsStore};
use patina_app::shell::{AccountLabel, Nav, RootView, StatusBar};
use patina_app::single_instance;
use patina_app::theme::{self, ProductColors};
use patina_core::accounts::AccountStore;
use patina_core::config::SourceChoice;
use patina_core::instance::{Component, ComponentUid, InstanceIcon, InstanceStore};
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
    let language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
    patina_app::i18n::set_language(Language::En);
    let rt = patina_app::runtime::start();
    let core = testing::core(rt.handle().clone(), DataDirs::new(dir), catalog);
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
    model.update(cx, |m, cx| {
        m.set_run_state("Forg", Some(RunState::Running), cx)
    });
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
        m.set_run_state("I05", Some(RunState::Running), cx);
        m.set_run_state("I07", Some(RunState::Running), cx);
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
        // The gutter on both sides, so nothing needs scrolling.
        let gutter = first.left() - row.left();
        assert!(gutter > px(0.));
        assert!(
            last.right() + gutter <= row.right(),
            "last chip ends at {:?}, row at {:?}",
            last.right(),
            row.right()
        );
        assert!(window.find(Filter::CHIPS[5].chip_id()).visible());
    })
    .unwrap();
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
    model.read_with(cx, |m, _| m.op(id).cloned())
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
        model.read_with(cx, |m, _| m.op("1.21.1").is_none()),
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
            model.op(id).cloned()
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
