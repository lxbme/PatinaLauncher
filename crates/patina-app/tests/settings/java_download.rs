//! Synthetic HTTP/archive UI integration; not live vendor acceptance.
use super::*;
use patina_core::java::catalog::JavaCatalog;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

fn open_download(app: &Booted, cx: &mut TestAppContext) {
    activate(app, cx);
    click(app, cx, "rail-settings");
    let page = cx.update(|cx| app.root.read(cx).settings().clone());
    settle(cx, |cx| page.read_with(cx, |p, _| p.config().is_some()));
    cx.update_window(app.handle, |_, _, cx| {
        page.update(cx, |p, cx| {
            p.scroll_to(patina_app::settings_page::Section::Java, cx)
        })
    })
    .unwrap();
    cx.run_until_parked();
    click(app, cx, "prefs-java-download");
}
fn choose(app: &Booted, cx: &mut TestAppContext, id: &'static str, down: usize) {
    let unselected = element(app, cx, id).is_some_and(|e| e.value() == Some("Please select"));
    click(app, cx, id);
    for _ in 0..down + usize::from(unselected) {
        press(app, cx, "down");
    }
    press(app, cx, "enter");
    cx.run_until_parked();
}

#[gpui_kit::test]
fn changing_vendor_clears_stale_builds_and_late_response_cannot_enable_download(
    cx: &mut TestAppContext,
) {
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(path("/info/available_releases"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"available_releases":[21]})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/assets/feature_releases/21/ga"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(300))
                    .set_body_raw(
                        include_str!("../../../patina-core/tests/fixtures/java/temurin-live.json"),
                        "application/json",
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(path("/microsoft"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_str!("../../../patina-core/tests/fixtures/java/microsoft-download.md"),
                "text/markdown",
            ))
            .mount(&server)
            .await;
    });
    let tmp = tempfile::tempdir().unwrap();
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        let mut core = testing::core(rt, data, Arc::new(FakeCatalog::new()));
        core.java_catalog = JavaCatalog::with_endpoints(
            reqwest::Client::new(),
            &server.uri(),
            &server.uri(),
            &format!("{}/microsoft", server.uri()),
        );
        core
    });
    open_download(&app, cx);
    settle(cx, |cx| {
        element(&app, cx, "java-download-loading").is_none()
    });
    choose(&app, cx, "java-download-major", 0);
    assert_eq!(
        element(&app, cx, "java-download-install")
            .unwrap()
            .disabled(),
        Some(true)
    );
    choose(&app, cx, "java-download-vendor", 2);
    settle(cx, |cx| {
        element(&app, cx, "java-download-loading").is_none()
    });
    std::thread::sleep(Duration::from_millis(350));
    cx.run_until_parked();
    assert_eq!(
        element(&app, cx, "java-download-install")
            .unwrap()
            .disabled(),
        Some(true),
        "old Temurin build must not be downloadable under Microsoft"
    );
    choose(&app, cx, "java-download-major", 1);
    settle(cx, |cx| {
        element(&app, cx, "java-download-loading").is_none()
    });
    assert_eq!(
        element(&app, cx, "java-download-install")
            .unwrap()
            .disabled(),
        None
    );
    assert!(element(&app, cx, "java-download-error").is_none());
}

#[gpui_kit::test]
fn download_failure_is_visible_and_retry_keeps_automatic_default(cx: &mut TestAppContext) {
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    let mut metadata: serde_json::Value = serde_json::from_str(include_str!(
        "../../../patina-core/tests/fixtures/java/temurin-live.json"
    ))
    .unwrap();
    metadata[0]["binaries"][0]["package"]["link"] =
        format!("{}/archive.tar.gz", server.uri()).into();
    rt.block_on(async {
        Mock::given(path("/info/available_releases"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"available_releases":[21]})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/assets/feature_releases/21/ga"))
            .and(query_param("page", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/archive.tar.gz"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
    });
    let tmp = tempfile::tempdir().unwrap();
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        let mut core = testing::core(rt, data, Arc::new(FakeCatalog::new()));
        core.java_catalog = JavaCatalog::with_endpoints(
            reqwest::Client::new(),
            &server.uri(),
            &server.uri(),
            &server.uri(),
        );
        core.java_downloader = Arc::new(
            patina_core::download::Downloader::new(
                reqwest::Client::new(),
                patina_core::download::Mirror::official(),
            )
            .with_retry(1, Duration::ZERO),
        );
        core
    });
    open_download(&app, cx);
    settle(cx, |cx| {
        element(&app, cx, "java-download-loading").is_none()
    });
    choose(&app, cx, "java-download-major", 0);
    settle(cx, |cx| {
        element(&app, cx, "java-download-loading").is_none()
    });
    click(&app, cx, "java-download-install");
    settle(cx, |cx| element(&app, cx, "java-download-error").is_some());
    assert!(element(&app, cx, "java-download-retry").is_some());
    assert!(element(&app, cx, "java-download-default").is_none());
    let config = rt.block_on(async {
        patina_core::config::GlobalConfig::load(&tmp.path().join("config.toml"))
    });
    assert!(config.unwrap().java.java_path.is_none());
    click(&app, cx, "java-download-install");
    settle(cx, |cx| element(&app, cx, "java-download-error").is_some());
    assert!(
        patina_core::java::distributions::DistributionStore::new(
            tmp.path().join("java-distributions")
        )
        .list()
        .unwrap()
        .is_empty()
    );
}

#[gpui_kit::test]
fn cancel_retry_install_requires_explicit_default_and_preserves_instance_override(
    cx: &mut TestAppContext,
) {
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    let mut metadata: serde_json::Value = serde_json::from_str(include_str!(
        "../../../patina-core/tests/fixtures/java/temurin-live.json"
    ))
    .unwrap();
    metadata.as_array_mut().unwrap().truncate(1);
    metadata[0]["binaries"][0]["package"]["link"] =
        format!("{}/archive.tar.gz", server.uri()).into();
    metadata[0]["binaries"][0]["package"]["checksum"] =
        include_str!("../../../patina-core/tests/fixtures/java/synthetic-jdk21.sha256").into();
    let archive = include_bytes!("../../../patina-core/tests/fixtures/java/synthetic-jdk21.tar.gz");
    metadata[0]["binaries"][0]["package"]["size"] = (archive.len() as u64).into();
    rt.block_on(async {
        Mock::given(path("/info/available_releases"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"available_releases":[21]})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/assets/feature_releases/21/ga"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
            .mount(&server)
            .await;
        Mock::given(path("/archive.tar.gz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(300))
                    .set_body_bytes(archive),
            )
            .mount(&server)
            .await;
    });
    let tmp = tempfile::tempdir().unwrap();
    let instances = InstanceStore::new(tmp.path().join("instances"));
    let mut inst = instances.create("Explicit Java", "1.21.1").unwrap();
    inst.config.java_override = Some(patina_core::config::JavaSettings {
        java_path: Some("/isolated/explicit-java".into()),
        ..Default::default()
    });
    instances.save(&inst).unwrap();
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        let mut core = testing::core(rt, data, Arc::new(FakeCatalog::new()));
        core.java_catalog = JavaCatalog::with_endpoints(
            reqwest::Client::new(),
            &server.uri(),
            &server.uri(),
            &server.uri(),
        );
        core.java_downloader = Arc::new(patina_core::download::Downloader::new(
            reqwest::Client::new(),
            patina_core::download::Mirror::official(),
        ));
        core
    });
    open_download(&app, cx);
    settle(cx, |cx| {
        element(&app, cx, "java-download-loading").is_none()
    });
    choose(&app, cx, "java-download-major", 0);
    settle(cx, |cx| {
        element(&app, cx, "java-download-loading").is_none()
    });
    click(&app, cx, "java-download-install");
    assert!(element(&app, cx, "java-download-cancel").is_some());
    assert!(
        element(&app, cx, "java-download-install").is_none(),
        "no duplicate active submission"
    );
    click(&app, cx, "java-download-cancel");
    settle(cx, |cx| element(&app, cx, "java-download-error").is_some());
    let managed = patina_core::java::distributions::DistributionStore::new(
        tmp.path().join("java-distributions"),
    );
    assert!(managed.list().unwrap().is_empty());
    click(&app, cx, "java-download-install");
    settle(cx, |cx| {
        element(&app, cx, "java-download-default").is_some()
    });
    let status = cx.update(|cx| app.root.read(cx).status_bar().clone());
    settle(cx, |cx| {
        status.read_with(cx, |s, _| s.runtimes() == Some(1))
    });
    let installed = managed.list().unwrap().remove(0);
    assert!(
        patina_core::config::GlobalConfig::load(&tmp.path().join("config.toml"))
            .unwrap()
            .java
            .java_path
            .is_none()
    );
    click(&app, cx, "java-download-default");
    let page = cx.update(|cx| app.root.read(cx).settings().clone());
    settle(cx, |cx| {
        page.read_with(cx, |p, _| {
            !p.is_saving()
                && p.config().and_then(|c| c.java.java_path.as_ref())
                    == Some(&installed.java().path)
        })
    });
    assert_eq!(
        patina_core::config::GlobalConfig::load(&tmp.path().join("config.toml"))
            .unwrap()
            .java
            .java_path,
        Some(installed.java().path.clone())
    );
    assert_eq!(
        instances
            .load(&inst.id)
            .unwrap()
            .config
            .java_override
            .unwrap()
            .java_path,
        Some("/isolated/explicit-java".into())
    );
    click(&app, cx, "rail-instances");
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    sclick(&app, cx, "tab-settings");
    settle(cx, |cx| radio(&app, cx, "settings-java", 1).is_some());
    click_radio(&app, cx, "settings-java", 1);
    settle(cx, |_| {
        instances
            .load(&inst.id)
            .unwrap()
            .config
            .java_override
            .as_ref()
            .and_then(|j| j.java_path.as_ref())
            == Some(&installed.java().path)
    });
    click(&app, cx, "rail-settings");
    settle(cx, |cx| {
        page.read_with(cx, |p, _| {
            p.config().and_then(|c| c.java.java_path.as_ref()) == Some(&installed.java().path)
        })
    });
}

/// Real production download/store/UI with an explicitly synthetic HTTP archive.
fn downloaded_fixture(
    cx: &mut TestAppContext,
    dir: &Path,
) -> (
    Booted,
    tokio::runtime::Runtime,
    MockServer,
    patina_core::java::distributions::InstalledDistribution,
) {
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    let mut metadata: serde_json::Value = serde_json::from_str(include_str!(
        "../../../patina-core/tests/fixtures/java/temurin-live.json"
    ))
    .unwrap();
    metadata.as_array_mut().unwrap().truncate(1);
    metadata[0]["binaries"][0]["package"]["link"] =
        format!("{}/archive.tar.gz", server.uri()).into();
    metadata[0]["binaries"][0]["package"]["checksum"] =
        include_str!("../../../patina-core/tests/fixtures/java/synthetic-jdk21.sha256").into();
    rt.block_on(async {
        Mock::given(path("/info/available_releases"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"available_releases":[21]})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/assets/feature_releases/21/ga"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
            .mount(&server)
            .await;
        Mock::given(path("/archive.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(include_bytes!(
                "../../../patina-core/tests/fixtures/java/synthetic-jdk21.tar.gz"
            )))
            .mount(&server)
            .await;
    });
    let system_home = dir.join("system-jdk");
    std::fs::create_dir_all(system_home.join("bin")).unwrap();
    std::fs::write(system_home.join("bin/java"), b"system fixture").unwrap();
    std::fs::write(
        system_home.join("release"),
        b"JAVA_VERSION=\"17.0.12\"\nOS_ARCH=\"x86_64\"\n",
    )
    .unwrap();
    let system = patina_core::java::probe(&system_home.join("bin/java")).unwrap();
    let app = boot_core(cx, dir, AppSettings::default(), |rt, data| {
        let owned =
            patina_core::java::distributions::DistributionStore::new(data.java_distributions());
        let alias = data.root().join("detector-owned-alias");
        let mut core = testing::core(rt, data, Arc::new(FakeCatalog::new()));
        core.detect_java = Arc::new(move || {
            let mut found = vec![system.clone()];
            if let Some(installed) = owned.list().unwrap().first() {
                if !alias.exists() {
                    let _ = std::fs::remove_file(&alias);
                    std::os::unix::fs::symlink(&installed.java().path, &alias).unwrap();
                }
                let mut java = installed.java().clone();
                java.path = alias.clone();
                found.push(java);
            }
            found
        });
        core.java_catalog = JavaCatalog::with_endpoints(
            reqwest::Client::new(),
            &server.uri(),
            &server.uri(),
            &server.uri(),
        );
        core.java_downloader = Arc::new(patina_core::download::Downloader::new(
            reqwest::Client::new(),
            patina_core::download::Mirror::official(),
        ));
        core
    });
    open_download(&app, cx);
    settle(cx, |cx| {
        element(&app, cx, "java-download-loading").is_none()
    });
    choose(&app, cx, "java-download-major", 0);
    settle(cx, |cx| {
        element(&app, cx, "java-download-loading").is_none()
    });
    click(&app, cx, "java-download-install");
    settle(cx, |cx| {
        element(&app, cx, "java-download-default").is_some()
    });
    let installed =
        patina_core::java::distributions::DistributionStore::new(dir.join("java-distributions"))
            .list()
            .unwrap()
            .remove(0);
    click(&app, cx, "java-download-close");
    (app, rt, server, installed)
}

fn click_java_setting(app: &Booted, cx: &mut TestAppContext, target: gpui_kit::ElementId) {
    for _ in 0..40 {
        let ready = cx
            .update_window(app.handle, |_, window, cx| {
                window.render_frame(cx);
                let found = window.find(target.clone());
                let y = found.bounds().center().y;
                if found.visible() && y > px(50.) && y < window.viewport_size().height - px(40.) {
                    return true;
                }
                window.scroll(
                    "prefs-content",
                    gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
                        px(0.),
                        if y < px(50.) { px(150.) } else { px(-150.) },
                    )),
                    cx,
                );
                false
            })
            .unwrap();
        cx.run_until_parked();
        if ready {
            break;
        }
    }
    click(app, cx, target);
}

#[gpui_kit::test]
fn downloaded_java_stays_owned_not_system_after_refresh_and_reopen(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _rt, _server, installed) = downloaded_fixture(cx, tmp.path());
    let owned =
        gpui_kit::ElementId::Name(format!("java-owned-{}", installed.release().id()).into());
    settle(cx, |cx| element(&app, cx, owned.clone()).is_some());
    click_java_setting(&app, cx, "prefs-redetect".into());
    settle(cx, |cx| {
        element(&app, cx, "prefs-redetect").is_some_and(|e| e.disabled() != Some(true))
    });
    let system = gpui_kit::ElementId::Name(
        format!("java-system-{}", installed.java().path.display()).into(),
    );
    assert!(
        element(&app, cx, system.clone()).is_none(),
        "a managed download must never be classified as read-only system Java"
    );
    let alias = gpui_kit::ElementId::Name(
        format!(
            "java-system-{}",
            tmp.path().join("detector-owned-alias").display()
        )
        .into(),
    );
    assert!(
        element(&app, cx, alias).is_none(),
        "canonical detector alias must remain owned"
    );
    let real_system = gpui_kit::ElementId::Name(
        format!(
            "java-system-{}",
            tmp.path().join("system-jdk/bin/java").display()
        )
        .into(),
    );
    assert!(
        element(&app, cx, real_system).is_some(),
        "genuinely detected system Java remains read-only"
    );
    click(&app, cx, "rail-instances");
    click(&app, cx, "rail-settings");
    settle(cx, |cx| element(&app, cx, owned.clone()).is_some());
    assert!(
        element(&app, cx, system).is_none(),
        "ownership must survive page reopen"
    );
}

#[gpui_kit::test]
fn unreferenced_download_can_be_deleted_through_production_settings_after_reopen(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _rt, _server, installed) = downloaded_fixture(cx, tmp.path());
    click(&app, cx, "rail-instances");
    click(&app, cx, "rail-settings");
    let id = installed.release().id();
    let delete = gpui_kit::ElementId::Name(format!("java-actions-{id}").into());
    settle(cx, |cx| element(&app, cx, delete.clone()).is_some());
    click_java_setting(&app, cx, delete);
    press(&app, cx, "down");
    press(&app, cx, "down");
    press(&app, cx, "enter");
    click(&app, cx, "java-delete-confirm");
    settle(cx, |_| !installed.java().path.exists());
    let owned = gpui_kit::ElementId::Name(format!("java-owned-{id}").into());
    let system = gpui_kit::ElementId::Name(
        format!("java-system-{}", installed.java().path.display()).into(),
    );
    settle(cx, |cx| {
        element(&app, cx, owned.clone()).is_none() && element(&app, cx, system.clone()).is_none()
    });
    assert!(element(&app, cx, "java-delete-reason").is_none());
    assert!(
        tmp.path().join("system-jdk/bin/java").is_file(),
        "system Java must remain untouched"
    );
    click(&app, cx, "rail-instances");
    click(&app, cx, "rail-settings");
    cx.run_until_parked();
    assert!(element(&app, cx, owned).is_none());
    assert!(element(&app, cx, system).is_none());
}

#[gpui_kit::test]
fn unified_java_list_exposes_owned_actions_without_duplicate_rows_or_selection(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _rt, _server, installed) = downloaded_fixture(cx, tmp.path());
    let menu =
        gpui_kit::ElementId::Name(format!("java-actions-{}", installed.release().id()).into());
    settle(cx, |cx| element(&app, cx, menu.clone()).is_some());
    assert!(element(&app, cx, "prefs-detected").is_none());
    assert!(element(&app, cx, "prefs-runtimes").is_none());
    assert!(
        element(
            &app,
            cx,
            gpui_kit::ElementId::Name(format!("java-default-{}", installed.release().id()).into())
        )
        .is_none()
    );
    click_java_setting(&app, cx, menu);
    assert!(
        patina_core::config::GlobalConfig::load(&tmp.path().join("config.toml"))
            .unwrap()
            .java
            .java_path
            .is_none(),
        "opening row actions must not select it"
    );
    press(&app, cx, "down");
    press(&app, cx, "enter");
    assert!(
        element(&app, cx, "java-download-dialog").is_some(),
        "first owned action checks same-vendor updates"
    );
    assert!(
        patina_core::config::GlobalConfig::load(&tmp.path().join("config.toml"))
            .unwrap()
            .java
            .java_path
            .is_none()
    );
}

#[gpui_kit::test]
fn unified_alias_selection_blocks_delete_and_instance_has_selection_only(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(tmp.path().join("instances"));
    let inst = store.create("Unified instance", "1.21.1").unwrap();
    let (app, _rt, _server, installed) = downloaded_fixture(cx, tmp.path());
    let id = installed.release().id();
    let owned = gpui_kit::ElementId::Name(format!("java-owned-{id}").into());
    let menu = gpui_kit::ElementId::Name(format!("java-actions-{id}").into());
    settle(cx, |cx| element(&app, cx, menu.clone()).is_some());
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(w.find_all(owned.clone()).len(), 1);
        assert_eq!(
            w.within("prefs-java-choice")
                .find_all_by_role(gpui_kit::Role::RadioButton)
                .len(),
            3,
            "automatic + one owned + one system, no alias duplicates"
        );
        assert_eq!(
            w.within("prefs-java-choice").find(0usize).checked(),
            Some(true)
        );
    })
    .unwrap();
    let source = gpui_kit::ElementId::Name(format!("java-owned-{id}-source").into());
    assert_eq!(
        element(&app, cx, source).unwrap().label(),
        Some("Downloaded")
    );
    let alias = tmp.path().join("detector-owned-alias");
    let page = cx.update(|cx| app.root.read(cx).settings().clone());
    cx.update_window(app.handle, |_, w, cx| {
        page.update(cx, |p, cx| p.use_java_file(alias.clone(), w, cx).detach());
    })
    .unwrap();
    settle(cx, |cx| {
        page.read_with(cx, |p, _| {
            !p.is_saving() && p.config().and_then(|c| c.java.java_path.as_ref()) == Some(&alias)
        })
    });
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(
            w.within("prefs-java-choice")
                .find_all_by_role(gpui_kit::Role::RadioButton)
                .len(),
            3
        );
        assert_eq!(
            w.within("prefs-java-choice").find(1usize).checked(),
            Some(true)
        );
    })
    .unwrap();
    click_java_setting(&app, cx, menu);
    press(&app, cx, "down");
    press(&app, cx, "down");
    press(&app, cx, "enter");
    click(&app, cx, "java-delete-confirm");
    settle(cx, |cx| element(&app, cx, "java-delete-reason").is_some());
    assert!(installed.java().path.exists());
    assert!(
        element(&app, cx, "java-delete-reason")
            .unwrap()
            .label()
            .unwrap()
            .contains("Global default")
    );
    press(&app, cx, "escape");
    click(&app, cx, "rail-instances");
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    sclick(&app, cx, "tab-settings");
    sclick(&app, cx, "settings-use-global");
    settle(cx, |_| {
        store.load(&inst.id).unwrap().config.java_override.is_some()
    });
    settle(cx, |cx| radio(&app, cx, "settings-java", 1).is_some());
    click_radio(&app, cx, "settings-java", 1);
    assert!(
        element(
            &app,
            cx,
            gpui_kit::ElementId::Name(format!("java-actions-{id}").into())
        )
        .is_none(),
        "instance chooser never exposes global management"
    );
    assert!(element(&app, cx, owned).is_some());
}

#[gpui_kit::test]
fn unified_manual_row_has_vendor_version_and_no_destructive_action(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _rt, _server, _) = downloaded_fixture(cx, tmp.path());
    let home = tmp.path().join("manual-jdk");
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::write(home.join("bin/java"), b"manual fixture").unwrap();
    std::fs::write(
        home.join("release"),
        b"JAVA_VERSION=\"17.0.12\"\nOS_ARCH=\"x86_64\"\nIMPLEMENTOR=\"ManualVendor\"\n",
    )
    .unwrap();
    let path = home.join("bin/java");
    let page = cx.update(|cx| app.root.read(cx).settings().clone());
    cx.update_window(app.handle, |_, w, cx| {
        page.update(cx, |p, cx| p.use_java_file(path.clone(), w, cx).detach())
    })
    .unwrap();
    settle(cx, |cx| {
        page.read_with(cx, |p, _| {
            !p.is_saving() && p.config().and_then(|c| c.java.java_path.as_ref()) == Some(&path)
        })
    });
    let row = gpui_kit::ElementId::Name(format!("java-manual-{}", path.display()).into());
    assert_eq!(
        element(&app, cx, row).unwrap().label(),
        Some("ManualVendor 17.0.12")
    );
    let source = gpui_kit::ElementId::Name(format!("java-manual-{}-source", path.display()).into());
    assert_eq!(
        element(&app, cx, source).unwrap().label(),
        Some("Local file")
    );
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        let group = w.within("prefs-java-choice");
        assert_eq!(group.find_all_by_role(gpui_kit::Role::RadioButton).len(), 4);
        assert_eq!(
            group.find_all_by_role(gpui_kit::Role::Button).len(),
            1,
            "only the owned vendor row has a management trigger; system/manual rows do not"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn unified_list_reloads_mojang_management_added_after_initial_inventory(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    activate(&app, cx);
    click(&app, cx, "rail-settings");
    let page = cx.update(|cx| app.root.read(cx).settings().clone());
    settle(cx, |cx| page.read_with(cx, |p, _| p.runtimes().is_some()));
    let home = tmp.path().join("runtimes/java-runtime-new");
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::write(home.join("bin/java"), b"Mojang fixture").unwrap();
    std::fs::write(
        home.join("release"),
        b"JAVA_VERSION=\"21.0.13\"\nOS_ARCH=\"x86_64\"\n",
    )
    .unwrap();
    std::fs::write(home.join(".patina-installed"), b"21.0.13").unwrap();
    click(&app, cx, "rail-instances");
    click(&app, cx, "rail-settings");
    settle(cx, |cx| {
        element(&app, cx, "prefs-runtime-delete-java-runtime-new").is_some()
    });
    assert!(
        element(&app, cx, "prefs-runtimes").is_none(),
        "management is part of the unified list, not a duplicate lower section"
    );
}

#[gpui_kit::test]
fn unified_delete_waits_for_pending_default_save(cx: &mut TestAppContext) {
    use gpui_kit::test::TestQueryExt;
    let tmp = tempfile::tempdir().unwrap();
    let (app, _rt, _server, installed) = downloaded_fixture(cx, tmp.path());
    let (started, wait_started) = std::sync::mpsc::channel();
    let (release, wait_release) = std::sync::mpsc::channel();
    let held = cx.update(|cx| {
        Core::update_config(cx, move |_| {
            started.send(()).unwrap();
            wait_release.recv().unwrap();
        })
    });
    wait_started.recv_timeout(Duration::from_secs(5)).unwrap();
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        w.within("prefs-java-choice").click(1usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    let menu =
        gpui_kit::ElementId::Name(format!("java-actions-{}", installed.release().id()).into());
    click_java_setting(&app, cx, menu);
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        let id = w.find_by_label("Delete…").path().last().unwrap().clone();
        w.within("popup-menu").click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
    let opened = element(&app, cx, "java-delete-confirm").is_some();
    release.send(()).unwrap();
    held.detach();
    assert!(
        !opened,
        "a pending radio/save intent must not race a destructive delete"
    );
    assert!(installed.java().path.is_file());
}
