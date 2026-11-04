use super::*;
use patina_app::modpacks::{open_preview, open_update_plan};
use patina_core::modpack::{ArchiveLimits, PackPreview, PackService, inspect_bytes};
use std::io::{Cursor, Write};
fn preview(version: &str, count: usize) -> PackPreview {
    preview_with_mc(version, count, "1.21.1")
}
fn preview_with_mc(version: &str, count: usize, mc: &str) -> PackPreview {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file(
        "modrinth.index.json",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    z.write_all(&serde_json::to_vec(&serde_json::json!({"formatVersion":1,"game":"minecraft","versionId":version,"name":"Fixture pack","dependencies":{"minecraft":mc},"files":[]})).unwrap()).unwrap();
    for n in 0..count {
        z.start_file(
            format!("overrides/config/file-{n:03}.txt"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        z.write_all(b"new").unwrap();
    }
    inspect_bytes(z.finish().unwrap().into_inner(), ArchiveLimits::default()).unwrap()
}
fn update_plan(
    app: &Booted,
    cx: &mut TestAppContext,
    count: usize,
) -> patina_core::modpack::PackUpdatePlan {
    let (store, launcher) = cx.update(|cx| {
        let c = Core::global(cx);
        (c.store.clone(), c.launcher())
    });
    let i = store.create("Pack", "1.21.1").unwrap();
    std::fs::create_dir_all(i.game_dir().join("config")).unwrap();
    let mut files = serde_json::Map::new();
    // SHA512("old"), independent known fixture baseline, disk deliberately differs.
    let old = "a6d96fa05fb243a13082a21195ebd126fccf9d176d1d8c7eafef5e234c4591936be0cd6b4816d75a7069b3c871f2f4211935d3ec3c4a6718a0b214fcfdbab9f8";
    for n in 0..count {
        let p = format!("config/file-{n:03}.txt");
        std::fs::write(i.game_dir().join(&p), b"local").unwrap();
        files.insert(p, serde_json::json!({"sha512":old,"source":null}));
    }
    std::fs::write(i.dir.join("modpack-state.json"),serde_json::to_vec(&serde_json::json!({"schema":1,"name":"Fixture pack","version":"1","source":null,"components":i.config.components,"files":files,"optional":{},"ready":true,"retained":{},"bindings":{}})).unwrap()).unwrap();
    app._rt
        .block_on(PackService::new(launcher, Default::default()).plan_update(
            i.id,
            preview("2", count),
            Default::default(),
        ))
        .unwrap()
}
fn instances(app: &Booted, cx: &mut TestAppContext) -> Entity<InstancesModel> {
    instances_model(app, cx)
}
#[gpui_kit::test]
fn modpack_preview_does_not_create_instance_and_cancel_is_safe(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances(&app, cx);
    cx.update_window(app.handle, |_, w, cx| {
        open_preview(model, preview("1", 2), w, cx);
    })
    .unwrap();
    assert!(shows(&app, cx, "modpack-confirm"));
    assert!(shows(&app, cx, "modpack-name"));
    assert!(
        InstanceStore::new(tmp.path().join("instances"))
            .list()
            .unwrap()
            .instances
            .is_empty()
    );
    click(&app, cx, "modpack-cancel");
    assert!(!shows(&app, cx, "modpack-confirm"));
}
#[gpui_kit::test]
fn modpack_conflicts_scroll_without_growing_dialog(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let p = update_plan(&app, cx, 80);
    let m = instances(&app, cx);
    cx.update_window(app.handle, |_, w, cx| {
        open_update_plan(m, p, w, cx);
    })
    .unwrap();
    click(&app, cx, "modpack-conflicts");
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(w.find("modpack-bulk-pack").visible());
        assert!(w.find("modpack-list-done").visible());
        let list = w.find("modpack-list").bounds();
        assert!(list.size.height < px(600.));
        w.scroll(
            "modpack-list",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-1800.))),
            cx,
        );
        assert!(w.find("modpack-list-done").visible());
    })
    .unwrap();
}
#[gpui_kit::test]
fn modpack_batch_decisions_survive_navigation_and_scroll(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let p = update_plan(&app, cx, 38);
    let m = instances(&app, cx);
    let view = cx
        .update_window(app.handle, |_, w, cx| open_update_plan(m, p, w, cx))
        .unwrap();
    click(&app, cx, "modpack-conflicts");
    click(&app, cx, "modpack-bulk-pack");
    assert_eq!(view.read_with(cx, |v, _| v.use_pack_count()), 38);
    click(&app, cx, "modpack-list-done");
    click(&app, cx, "modpack-conflicts");
    assert_eq!(view.read_with(cx, |v, _| v.use_pack_count()), 38);
    click(&app, cx, "modpack-bulk-local");
    assert_eq!(view.read_with(cx, |v, _| v.use_pack_count()), 0);
    click(&app, cx, "modpack-list-done");
    assert!(shows(&app, cx, "modpack-submit"));
}
#[gpui_kit::test]
fn modpack_focus_escape_and_late_plan_are_safe(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let p = update_plan(&app, cx, 38);
    let m = instances(&app, cx);
    cx.update_window(app.handle, |_, w, cx| {
        open_update_plan(m, p, w, cx);
    })
    .unwrap();
    click(&app, cx, "modpack-conflicts");
    press(&app, cx, "escape");
    assert!(shows(&app, cx, "modpack-confirm"));
    assert!(!shows(&app, cx, "modpack-list-done"));
    press(&app, cx, "escape");
    assert!(!shows(&app, cx, "modpack-confirm"));
}

#[gpui_kit::test]
fn modpack_pending_install_is_not_launchable_after_reload(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(tmp.path().join("instances"));
    let i = store.create("Pending", "1.21.1").unwrap();
    std::fs::write(i.dir.join("modpack-pending"), b"installing").unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances(&app, cx);
    list_settled(&model, cx);
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(w.find("launch").disabled(), Some(true));
        assert!(w.find("modpack-not-ready").visible());
    })
    .unwrap();
    assert!(!model.update(cx, |m, cx| m.launch("Pending", cx)));
}

#[gpui_kit::test]
fn modpack_confirmation_is_compact(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let m = instances(&app, cx);
    cx.update_window(app.handle, |_, w, cx| {
        open_preview(m, preview("1", 2), w, cx);
        w.render_frame(cx);
        assert!(w.find("modpack-surface").bounds().size.height < px(360.));
    })
    .unwrap();
}

fn write_archive(path: &Path, version: &str) {
    let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    z.start_file(
        "modrinth.index.json",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    z.write_all(&serde_json::to_vec(&serde_json::json!({"formatVersion":1,"game":"minecraft","versionId":version,"name":format!("Pack {version}"),"dependencies":{"minecraft":"1.21.1"},"files":[]})).unwrap()).unwrap();
    z.finish().unwrap();
}
#[gpui_kit::test]
fn modpack_native_picker_result_uses_the_same_preview(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("local.mrpack");
    write_archive(&path, "local");
    let app = boot(cx, tmp.path(), AppSettings::default());
    click(&app, cx, "instances-add");
    click(&app, cx, "add-instance-import-pack");
    cx.simulate_path_prompt_response(move |_| Some(vec![path]));
    settle(cx, |cx| shows(&app, cx, "modpack-name"));
    assert!(
        InstanceStore::new(tmp.path().join("instances"))
            .list()
            .unwrap()
            .instances
            .is_empty()
    );
}
#[gpui_kit::test]
fn modpack_same_name_error_does_not_turn_into_success(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(tmp.path().join("instances"));
    store.create("Fixture pack", "1.21.1").unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let m = instances(&app, cx);
    cx.update_window(app.handle, |_, w, cx| {
        open_preview(m, preview("1", 0), w, cx);
    })
    .unwrap();
    click(&app, cx, "modpack-submit");
    settle(cx, |cx| shows(&app, cx, "modpack-error"));
    assert!(!shows(&app, cx, "modpack-complete"));
    assert_eq!(store.list().unwrap().instances.len(), 1);
}
#[gpui_kit::test]
fn modpack_late_preview_cannot_replace_new_selection(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let path1 = tmp.path().join("first.mrpack");
    let path2 = tmp.path().join("second.mrpack");
    write_archive(&path1, "first");
    write_archive(&path2, "second");
    let app = boot(cx, tmp.path(), AppSettings::default());
    let m = instances(&app, cx);
    let v = cx
        .update_window(app.handle, |_, w, cx| {
            open_preview(m, preview("initial", 0), w, cx)
        })
        .unwrap();
    cx.update_window(app.handle, |_, w, cx| {
        v.update(cx, |v, cx| v.preview_path(path1, w, cx));
        v.update(cx, |v, cx| v.preview_path(path2, w, cx));
    })
    .unwrap();
    settle(cx, |cx| shows(&app, cx, "modpack-name"));
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(w.find("modpack-name").value(), Some("Pack second"));
    })
    .unwrap();
}
#[gpui_kit::test]
fn modpack_conflicts_remain_operable_at_narrow_window_and_zoom(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    cx.update(|cx| Theme::update(cx, |theme| theme.font_size = px(20.)));
    let p = update_plan(&app, cx, 80);
    let m = instances(&app, cx);
    cx.update_window(app.handle, |_, w, cx| {
        open_update_plan(m, p, w, cx);
    })
    .unwrap();
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        w.scroll(
            "modpack-confirm-scroll",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-280.))),
            cx,
        );
    })
    .unwrap();
    click(&app, cx, "modpack-conflicts");
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(w.find("modpack-bulk-local").visible());
        assert!(w.find("modpack-list-done").visible());
        assert!(w.find("modpack-surface").bounds().bottom() < w.viewport_size().height);
    })
    .unwrap();
}

struct PackCatalog;
impl patina_core::content::online::ContentSource for PackCatalog {
    fn search(
        &self,
        q: patina_core::content::online::OnlineQuery,
    ) -> futures::future::BoxFuture<
        'static,
        Result<patina_core::content::online::OnlinePage, patina_core::content::online::OnlineError>,
    > {
        use patina_core::content::online::*;
        Box::pin(async move {
            if q.platform == Platform::CurseForge {
                return Err(OnlineError::MissingApiKey);
            }
            Ok(OnlinePage {
                hits: vec![OnlineHit {
                    id: "pack".into(),
                    title: "Pack".into(),
                    author: "Author".into(),
                    summary: "A pack".into(),
                    downloads: 1,
                    updated: 0,
                    icon_url: None,
                    tags: vec![],
                }],
                total: 1,
            })
        })
    }
    fn project(
        &self,
        p: patina_core::content::online::ProjectRef,
    ) -> futures::future::BoxFuture<
        'static,
        Result<
            patina_core::content::online::ContentProject,
            patina_core::content::online::OnlineError,
        >,
    > {
        use patina_core::content::online::*;
        Box::pin(async move {
            Ok(ContentProject::new(
                p,
                ContentKind::Modpack,
                "Pack",
                "https://modrinth.com/modpack/pack",
            ))
        })
    }
    fn pack_versions(
        &self,
        p: patina_core::content::online::ProjectRef,
    ) -> futures::future::BoxFuture<
        'static,
        Result<
            Vec<patina_core::content::online::ContentVersion>,
            patina_core::content::online::OnlineError,
        >,
    > {
        use patina_core::content::online::*;
        Box::pin(async move {
            Ok(vec![
                ContentVersion::new(
                    VersionRef::new(p, "v"),
                    "Pack version",
                    ReleaseChannel::Release,
                )
                .with_compatibility(vec!["1.20.1".into()], vec![]),
            ])
        })
    }
}
#[gpui_kit::test]
fn modpack_online_install_needs_no_target_and_cf_remains_unavailable(cx: &mut TestAppContext) {
    use patina_core::content::online::{ContentKind, Platform};
    let tmp = tempfile::tempdir().unwrap();
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_source(
            rt,
            data,
            Arc::new(FakeCatalog::new()),
            Arc::new(PackCatalog),
        )
    });
    click(&app, cx, "rail-downloads");
    let m = app
        .root
        .read_with(cx, |r, cx| r.downloads().read(cx).model().clone());
    m.update(cx, |m, cx| m.set_kind(ContentKind::Modpack, cx));
    settle(cx, |cx| {
        shows(&app, cx, patina_app::downloads::hit_row_id("pack"))
    });
    click(&app, cx, patina_app::downloads::hit_row_id("pack"));
    settle(cx, |cx| {
        m.read_with(cx, |m, _| m.selected_version().is_some())
    });
    assert_eq!(m.read_with(cx, |m, _| m.target().map(str::to_string)), None);
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        assert_ne!(w.find("downloads-install").disabled(), Some(true));
    })
    .unwrap();
    m.update(cx, |m, cx| m.set_platform(Platform::CurseForge, cx));
    settle(cx, |cx| !m.read_with(cx, |m, _| m.is_searching()));
    cx.update_window(app.handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(w.find("downloads-install").disabled(), Some(true));
    })
    .unwrap();
}

#[gpui_kit::test]
fn modpack_cross_minecraft_update_warns_without_forcing_new_instance(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let initial = update_plan(&app, cx, 0);
    let launcher = cx.update(|cx| Core::global(cx).launcher());
    let plan = app
        ._rt
        .block_on(PackService::new(launcher, Default::default()).plan_update(
            initial.instance().id.clone(),
            preview_with_mc("3", 0, "1.20.1"),
            Default::default(),
        ))
        .unwrap();
    let m = instances(&app, cx);
    cx.update_window(app.handle, |_, w, cx| {
        open_update_plan(m, plan, w, cx);
        w.render_frame(cx);
        assert!(w.find("modpack-world-warning").visible());
        assert!(!w.try_find("modpack-name").is_some_and(|s| s.visible()));
        assert_ne!(w.find("modpack-submit").disabled(), Some(true));
    })
    .unwrap();
}

#[gpui_kit::test]
fn modpack_failed_planning_can_refresh_or_switch_to_fresh_install(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let plan = update_plan(&app, cx, 0);
    let m = instances(&app, cx);
    let v = cx
        .update_window(app.handle, |_, w, cx| open_update_plan(m, plan, w, cx))
        .unwrap();
    cx.update(|cx| {
        Core::global(cx).set_source(patina_core::prepare::ResolvedSource {
            mirror: patina_core::download::Mirror {
                id: "loopback-only".into(),
                rules: vec![(
                    "https://cdn.modrinth.com".into(),
                    "http://127.0.0.1:1".into(),
                )],
            },
            fallback: None,
        })
    });
    let path = tmp.path().join("failed-plan.mrpack");
    let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    z.start_file(
        "modrinth.index.json",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    z.write_all(&serde_json::to_vec(&serde_json::json!({"formatVersion":1,"game":"minecraft","versionId":"next","name":"Retry plan","dependencies":{"minecraft":"1.21.1"},"files":[{"path":"mods/a.jar","hashes":{"sha1":"0".repeat(40),"sha512":"0".repeat(128)},"fileSize":1,"downloads":["https://cdn.modrinth.com/fixture-unavailable"]}]})).unwrap()).unwrap();
    z.finish().unwrap();
    cx.update_window(app.handle, |_, w, cx| {
        v.update(cx, |v, cx| v.preview_path(path, w, cx))
    })
    .unwrap();
    settle(cx, |cx| shows(&app, cx, "modpack-error"));
    assert!(shows(&app, cx, "modpack-refresh-preview"));
    cx.update_window(app.handle, |_, window, cx| {
        window.scroll(
            "modpack-confirm-scroll",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-1000.))),
            cx,
        );
    })
    .unwrap();
    click(&app, cx, "modpack-fresh");
    assert!(shows(&app, cx, "modpack-name"));
    assert!(
        !shows(&app, cx, "modpack-error"),
        "a failure of the previous in-place update must not describe a new installation"
    );
    click(&app, cx, "modpack-name");
    press(&app, cx, "ctrl-a");
    type_text(&app, cx, "Pack");
    click(&app, cx, "modpack-submit");
    settle(cx, |cx| shows(&app, cx, "modpack-error"));
    assert!(
        shows(&app, cx, "modpack-error"),
        "a new fresh-install validation error must remain visible"
    );
    assert!(!shows(&app, cx, "modpack-complete"));
    assert_eq!(
        InstanceStore::new(tmp.path().join("instances"))
            .list()
            .unwrap()
            .instances
            .len(),
        1
    );
}

#[gpui_kit::test]
fn modpack_optional_replan_failure_invalidates_old_plan_until_successful_retry(
    cx: &mut TestAppContext,
) {
    use patina_core::prepare::ResolvedSource;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let old = update_plan(&app, cx, 0);
    let instance = old.instance().clone();
    let original_state = std::fs::read(instance.dir.join("modpack-state.json")).unwrap();
    let server = app._rt.block_on(MockServer::start());
    cx.update(|cx| {
        Core::global(cx).set_source(ResolvedSource {
            mirror: patina_core::download::Mirror {
                id: "optional-loopback".into(),
                rules: vec![("https://cdn.modrinth.com".into(), server.uri())],
            },
            fallback: None,
        })
    });
    let archive = tmp.path().join("optional.mrpack");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    zip.start_file(
        "modrinth.index.json",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zip.write_all(&serde_json::to_vec(&serde_json::json!({
        "formatVersion":1,"game":"minecraft","versionId":"2","name":"Optional pack",
        "dependencies":{"minecraft":"1.21.1"},
        "files":[{"path":"mods/optional.jar","fileSize":8,
            "hashes":{"sha1":"48a7b8889e1542650266c14a18c8708488fa1951","sha512":"57828d8e451378e564e3eddcd500b3da1afc9a6003c427429b309da6df0ef4f0fc1ea45f99f589fe4d3dbb82285e3320a7c8222f736303e4776021bbc16ac0a3"},
            "downloads":["https://cdn.modrinth.com/optional.jar"],"env":{"client":"optional","server":"required"}}]
    })).unwrap()).unwrap();
    zip.finish().unwrap();
    let preview = patina_core::modpack::inspect_archive(&archive).unwrap();
    let launcher = cx.update(|cx| Core::global(cx).launcher());
    let plan = app
        ._rt
        .block_on(PackService::new(launcher, Default::default()).plan_update(
            instance.id.clone(),
            preview,
            Default::default(),
        ))
        .unwrap();
    assert_eq!(
        plan.optional_choices().get("mods/optional.jar"),
        Some(&false)
    );
    assert_eq!(plan.normal_change_count(), 0);
    let model = instances(&app, cx);
    cx.update_window(app.handle, |_, window, cx| {
        open_update_plan(model, plan, window, cx);
    })
    .unwrap();
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        window.scroll(
            "modpack-confirm-scroll",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-600.))),
            cx,
        );
    })
    .unwrap();
    click(&app, cx, "modpack-optional:mods/optional.jar");
    settle(cx, |cx| shows(&app, cx, "modpack-error"));
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("modpack-submit").disabled(),
            Some(true),
            "the prior optional=false plan must not remain executable"
        );
        window.click("modpack-submit", cx);
    })
    .unwrap();
    assert_eq!(
        std::fs::read(instance.dir.join("modpack-state.json")).unwrap(),
        original_state
    );
    assert!(!instance.game_dir().join("mods/optional.jar").exists());
    app._rt.block_on(
        Mock::given(path("/optional.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"optional"))
            .mount(&server),
    );
    cx.update_window(app.handle, |_, window, cx| {
        window.scroll(
            "modpack-confirm-scroll",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(600.))),
            cx,
        );
    })
    .unwrap();
    click(&app, cx, "modpack-refresh-preview");
    settle(cx, |cx| {
        !shows(&app, cx, "modpack-error")
            && element(&app, cx, "modpack-submit").is_some_and(|e| e.disabled() != Some(true))
    });
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("modpack-normal").label(),
            Some("1 normal changes…"),
            "replacement plan includes the requested optional file"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn modpack_online_preview_failure_retries_same_version_without_local_picker(
    cx: &mut TestAppContext,
) {
    use patina_core::content::online::*;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join("online.mrpack");
    write_archive(&archive, "online-v");
    let bytes = std::fs::read(&archive).unwrap();
    let sha1 = patina_core::download::sha1_file(&archive).unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    let server = app._rt.block_on(MockServer::start());
    cx.update(|cx| {
        Core::global(cx).set_source(patina_core::prepare::ResolvedSource {
            mirror: patina_core::download::Mirror {
                id: "online-preview-loopback".into(),
                rules: vec![("https://cdn.modrinth.com".into(), server.uri())],
            },
            fallback: None,
        })
    });
    let version = ContentVersion::new(
        VersionRef::new(
            ProjectRef::new(Platform::Modrinth, "online-project"),
            "online-v",
        ),
        "Online project v1",
        ReleaseChannel::Release,
    )
    .with_files(vec![ContentFile::new(
        "online.mrpack",
        bytes.len() as u64,
        FileHashes::new(Some(sha1), None, None),
        Some("https://cdn.modrinth.com/online.mrpack".into()),
        "https://modrinth.com/modpack/online-project",
    )]);
    let model = instances(&app, cx);
    cx.update_window(app.handle, |_, window, cx| {
        patina_app::modpacks::open_online(model, version, window, cx);
    })
    .unwrap();
    settle(cx, |cx| shows(&app, cx, "modpack-error"));
    assert!(
        !shows(&app, cx, "modpack-local"),
        "an online failure must not switch the user to local import"
    );
    let failed_requests = app._rt.block_on(server.received_requests()).unwrap();
    assert!(!failed_requests.is_empty());
    app._rt.block_on(
        Mock::given(path("/online.mrpack"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(&server),
    );
    click(&app, cx, "modpack-request-retry");
    settle(cx, |cx| shows(&app, cx, "modpack-name"));
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("modpack-name").value(), Some("Pack online-v"));
    })
    .unwrap();
    let requests = app._rt.block_on(server.received_requests()).unwrap();
    assert_eq!(requests.len(), failed_requests.len() + 1);
    assert!(requests.iter().all(|r| r.url.path() == "/online.mrpack"));
    assert!(
        InstanceStore::new(tmp.path().join("instances"))
            .list()
            .unwrap()
            .instances
            .is_empty()
    );
}

struct RetryPackVersions(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl patina_core::content::online::ContentSource for RetryPackVersions {
    fn search(
        &self,
        _: patina_core::content::online::OnlineQuery,
    ) -> futures::future::BoxFuture<
        'static,
        Result<patina_core::content::online::OnlinePage, patina_core::content::online::OnlineError>,
    > {
        Box::pin(async { Err(patina_core::content::online::OnlineError::Unavailable) })
    }
    fn pack_versions(
        &self,
        project: patina_core::content::online::ProjectRef,
    ) -> futures::future::BoxFuture<
        'static,
        Result<
            Vec<patina_core::content::online::ContentVersion>,
            patina_core::content::online::OnlineError,
        >,
    > {
        use patina_core::content::online::*;
        let fail = self.0.load(std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if fail {
                Err(OnlineError::Network(
                    "versions temporarily unavailable".into(),
                ))
            } else {
                Ok(vec![ContentVersion::new(
                    VersionRef::new(project, "retry-release"),
                    "Retry release",
                    ReleaseChannel::Release,
                )])
            }
        })
    }
}
#[gpui_kit::test]
fn modpack_online_version_list_failure_retries_online_route(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let source = Arc::new(RetryPackVersions(fail.clone()));
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_source(rt, data, Arc::new(FakeCatalog::new()), source)
    });
    let old = update_plan(&app, cx, 0);
    let instance = old.instance();
    let path = instance.dir.join("modpack-state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    state["source"] = serde_json::json!({"project":{"platform":"Modrinth","id":"online-project"},"id":"old-release"});
    std::fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
    let model = instances(&app, cx);
    cx.update_window(app.handle, |_, window, cx| {
        patina_app::modpacks::open_update(model, instance.id.clone(), window, cx);
    })
    .unwrap();
    settle(cx, |cx| shows(&app, cx, "modpack-error"));
    assert!(!shows(&app, cx, "modpack-local"));
    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    click(&app, cx, "modpack-request-retry");
    settle(cx, |cx| shows(&app, cx, "modpack-version:retry-release"));
    assert!(!shows(&app, cx, "modpack-local"));
}
#[gpui_kit::test]
fn modpack_failed_local_import_retains_file_selection_route(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let bad = tmp.path().join("bad.mrpack");
    std::fs::write(&bad, b"not an archive").unwrap();
    let good = tmp.path().join("good.mrpack");
    write_archive(&good, "local-retry");
    let app = boot(cx, tmp.path(), AppSettings::default());
    let model = instances(&app, cx);
    cx.update_window(app.handle, |_, window, cx| {
        patina_app::modpacks::open_local(model, window, cx);
    })
    .unwrap();
    cx.simulate_path_prompt_response(move |_| Some(vec![bad]));
    settle(cx, |cx| shows(&app, cx, "modpack-error"));
    assert!(shows(&app, cx, "modpack-local"));
    click(&app, cx, "modpack-local");
    cx.simulate_path_prompt_response(move |_| Some(vec![good]));
    settle(cx, |cx| shows(&app, cx, "modpack-name"));
}
