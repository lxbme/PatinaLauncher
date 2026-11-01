//! Headless tests of the production downloads page with an isolated catalog.
use super::*;
use futures::future::BoxFuture;
use patina_app::downloads::Detail;
use patina_core::content::online::*;
use std::sync::Mutex;

struct BrowserSource {
    held: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}
impl ContentSource for BrowserSource {
    fn search(&self, q: OnlineQuery) -> BoxFuture<'static, Result<OnlinePage, OnlineError>> {
        Box::pin(async move {
            if q.platform == Platform::CurseForge {
                return Err(OnlineError::MissingApiKey);
            }
            Ok(OnlinePage {
                hits: vec![hit("a"), hit("b")],
                total: 2,
            })
        })
    }
    fn project(
        &self,
        reference: ProjectRef,
    ) -> BoxFuture<'static, Result<ContentProject, OnlineError>> {
        let held = if reference.id() == "a" {
            self.held.lock().unwrap().take()
        } else {
            None
        };
        Box::pin(async move {
            if let Some(held) = held {
                let _ = held.await;
            }
            Ok(ContentProject::new(
                reference,
                ContentKind::Mod,
                "Browser project",
                "https://modrinth.com/mod/test",
            )
            .with_description("Full description")
            .with_authors(vec!["Author".into()]))
        })
    }
    fn versions(
        &self,
        reference: ProjectRef,
        target: TargetCompatibility,
    ) -> BoxFuture<'static, Result<Vec<ContentVersion>, OnlineError>> {
        Box::pin(async move {
            Ok(vec![
                ContentVersion::new(
                    VersionRef::new(reference, "stable"),
                    "Stable file",
                    ReleaseChannel::Release,
                )
                .with_compatibility(vec![target.mc_version().into()], vec![LoaderKind::Fabric]),
            ])
        })
    }
    fn categories(
        &self,
        _: Platform,
        _: ContentKind,
    ) -> BoxFuture<'static, Result<Vec<ContentCategory>, OnlineError>> {
        Box::pin(async { Ok(vec![ContentCategory::new("real-category", "Real category")]) })
    }
}
fn browser(
    cx: &mut TestAppContext,
    dir: &Path,
    held: Option<tokio::sync::oneshot::Receiver<()>>,
) -> (Booted, Entity<DownloadsModel>) {
    let store = InstanceStore::new(DataDirs::new(dir).instances());
    store.create("Fab", "1.21.1").unwrap();
    set_loader(&store, "Fab", ComponentUid::Fabric);
    let app = boot_core(cx, dir, AppSettings::default(), |rt, data| {
        testing::core_with_source(
            rt,
            data,
            Arc::new(FakeCatalog::new()),
            Arc::new(BrowserSource {
                held: Mutex::new(held),
            }),
        )
    });
    list_settled(&instances_model(&app, cx), cx);
    let m = model(&app, cx);
    (app, m)
}
#[gpui_kit::test]
fn search_details_file_selection_and_keyboard(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, m) = browser(cx, tmp.path(), None);
    click(&app, cx, "rail-downloads");
    m.update(cx, |m, cx| {
        m.set_target(Some("Fab"), cx);
    });
    settle(cx, |cx| matches!(state(&m, cx), Results::Loaded(_)));
    click(&app, cx, hit_row_id("b"));
    settle(cx, |cx| {
        matches!(
            m.read_with(cx, |m, _| m.detail().clone()),
            Detail::Loaded { .. }
        )
    });
    m.read_with(cx, |m, _| {
        assert_eq!(m.selected_version().unwrap().reference().id(), "stable");
        assert_eq!(m.categories().len(), 1);
    });
    assert!(shows(&app, cx, "downloads-project-page"));
    assert!(shows(&app, cx, "downloads-file-version"));
    click(&app, cx, "downloads-file-version");
    press(&app, cx, "escape");
    assert_eq!(
        m.read_with(cx, |m, _| m
            .selected_version()
            .unwrap()
            .reference()
            .id()
            .to_owned()),
        "stable"
    );
}
#[gpui_kit::test]
fn late_project_response_does_not_replace_new_selection(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (send, receive) = tokio::sync::oneshot::channel();
    let (app, m) = browser(cx, tmp.path(), Some(receive));
    click(&app, cx, "rail-downloads");
    m.update(cx, |m, cx| {
        m.set_target(Some("Fab"), cx);
    });
    settle(cx, |cx| matches!(state(&m, cx), Results::Loaded(_)));
    click(&app, cx, hit_row_id("a"));
    cx.run_until_parked();
    click(&app, cx, hit_row_id("b"));
    settle(cx, |cx| {
        matches!(
            m.read_with(cx, |m, _| m.detail().clone()),
            Detail::Loaded { .. }
        )
    });
    let _ = send.send(());
    cx.run_until_parked();
    m.read_with(cx, |m, _| {
        let Detail::Loaded { project, .. } = m.detail() else {
            panic!("new project missing")
        };
        assert_eq!(project.reference().id(), "b");
    });
}
#[gpui_kit::test]
fn missing_key_is_visible_and_platform_switch_clears_detail(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, m) = browser(cx, tmp.path(), None);
    click(&app, cx, "rail-downloads");
    settle(cx, |cx| matches!(state(&m, cx), Results::Loaded(_)));
    click(&app, cx, hit_row_id("b"));
    m.update(cx, |m, cx| m.set_platform(Platform::CurseForge, cx));
    settle(cx, |cx| matches!(state(&m, cx), Results::Failed(_)));
    m.read_with(cx, |m, _| {
        assert!(m.selected().is_none());
        assert!(matches!(m.detail(), Detail::Idle));
        let Results::Failed(error) = m.results() else {
            panic!()
        };
        assert!(error.contains("CurseForge"));
    });
    assert!(shows(&app, cx, "downloads-failed"));
    assert!(disabled(&app, cx, "downloads-install"));
}

struct InstallSource(ContentVersion);
impl ContentSource for InstallSource {
    fn search(&self, _: OnlineQuery) -> BoxFuture<'static, Result<OnlinePage, OnlineError>> {
        Box::pin(async {
            Ok(OnlinePage {
                hits: vec![hit("manual")],
                total: 1,
            })
        })
    }
    fn project(
        &self,
        reference: ProjectRef,
    ) -> BoxFuture<'static, Result<ContentProject, OnlineError>> {
        Box::pin(async move {
            Ok(ContentProject::new(
                reference,
                ContentKind::Mod,
                "Manual project",
                "https://modrinth.com/mod/test",
            ))
        })
    }
    fn versions(
        &self,
        _: ProjectRef,
        _: TargetCompatibility,
    ) -> BoxFuture<'static, Result<Vec<ContentVersion>, OnlineError>> {
        let version = self.0.clone();
        Box::pin(async move { Ok(vec![version]) })
    }
    fn version(
        &self,
        _: Platform,
        _: String,
    ) -> BoxFuture<'static, Result<ContentVersion, OnlineError>> {
        let version = self.0.clone();
        Box::pin(async move { Ok(version) })
    }
}
#[gpui_kit::test]
fn restricted_file_validates_then_continues_without_partial_install(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let instance = store.create("Fab", "1.21.1").unwrap();
    set_loader(&store, "Fab", ComponentUid::Fabric);
    let manual = tmp.path().join("fixture.jar");
    fabric_jar(tmp.path(), "fixture.jar", "Manual");
    let bytes = std::fs::read(&manual).unwrap();
    let hash = patina_core::download::sha1_file(&manual).unwrap();
    let version = ContentVersion::new(
        VersionRef::new(ProjectRef::new(Platform::Modrinth, "manual"), "file"),
        "Manual file",
        ReleaseChannel::Release,
    )
    .with_compatibility(vec!["1.21.1".into()], vec![LoaderKind::Fabric])
    .with_files(vec![ContentFile::new(
        "fixture.jar",
        bytes.len() as u64,
        FileHashes::new(Some(hash), None, None),
        None,
        "https://modrinth.com/mod/test",
    )]);
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_source(
            rt,
            data,
            Arc::new(FakeCatalog::new()),
            Arc::new(InstallSource(version)),
        )
    });
    list_settled(&instances_model(&app, cx), cx);
    let m = model(&app, cx);
    click(&app, cx, "rail-downloads");
    m.update(cx, |m, cx| {
        m.set_target(Some("Fab"), cx);
    });
    settle(cx, |cx| matches!(state(&m, cx), Results::Loaded(_)));
    click(&app, cx, hit_row_id("manual"));
    settle(cx, |cx| {
        m.read_with(cx, |m, _| m.selected_version().is_some())
    });
    click(&app, cx, "downloads-install");
    settle(cx, |cx| shows(&app, cx, "content-manual-file:file"));
    assert!(!instance.game_dir().join("mods/fixture.jar").exists());
    assert!(disabled(&app, cx, "content-install-continue"));
    let junk = tmp.path().join("wrong.jar");
    std::fs::write(&junk, b"wrong").unwrap();
    click(&app, cx, "content-manual-file:file");
    cx.simulate_path_prompt_response(move |_| Some(vec![junk]));
    settle(cx, |cx| shows(&app, cx, "content-install-error"));
    assert!(disabled(&app, cx, "content-install-continue"));
    click(&app, cx, "content-manual-file:file");
    cx.simulate_path_prompt_response(move |_| Some(vec![manual]));
    settle(cx, |cx| !disabled(&app, cx, "content-install-continue"));
    click(&app, cx, "content-install-continue");
    settle(cx, |_| {
        instance.game_dir().join("mods/fixture.jar").is_file()
    });
    assert_eq!(
        std::fs::read(instance.game_dir().join("mods/fixture.jar")).unwrap(),
        bytes
    );
}

#[gpui_kit::test]
fn outer_install_only_confirms_then_inner_install_shows_progress_and_local_content(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap();
    fabric_jar(tmp.path(), "fixture.jar", "Installed");
    let file = tmp.path().join("fixture.jar");
    let bytes = std::fs::read(&file).unwrap();
    let hash = patina_core::download::sha1_file(&file).unwrap();
    let network = patina_app::runtime::start();
    let server = network.block_on(wiremock::MockServer::start());
    network.block_on(async {
        wiremock::Mock::given(wiremock::matchers::path("/fixture.jar"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_bytes(bytes.clone())
                    .set_delay(Duration::from_millis(900)),
            )
            .mount(&server)
            .await;
    });
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    store.create("Fab", "1.21.1").unwrap();
    set_loader(&store, "Fab", ComponentUid::Fabric);
    let version = ContentVersion::new(
        VersionRef::new(ProjectRef::new(Platform::Modrinth, "manual"), "file"),
        "Fixture file",
        ReleaseChannel::Release,
    )
    .with_compatibility(vec!["1.21.1".into()], vec![LoaderKind::Fabric])
    .with_files(vec![ContentFile::new(
        "fixture.jar",
        bytes.len() as u64,
        FileHashes::new(Some(hash), None, None),
        Some(format!("{}/fixture.jar", server.uri())),
        "https://modrinth.com/mod/test",
    )]);
    let source = Arc::new(InstallSource(version));
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        let mut core =
            testing::core_with_source(rt, data, Arc::new(FakeCatalog::new()), source.clone());
        core.content_service = Arc::new(ContentService::new(
            core.store.clone(),
            source,
            Arc::new(patina_core::download::Downloader::new(
                reqwest::Client::new(),
                patina_core::download::Mirror::official(),
            )),
        ));
        core
    });
    list_settled(&instances_model(&app, cx), cx);
    let m = model(&app, cx);
    click(&app, cx, "rail-downloads");
    m.update(cx, |m, cx| {
        m.set_target(Some("Fab"), cx);
    });
    settle(cx, |cx| matches!(state(&m, cx), Results::Loaded(_)));
    click(&app, cx, hit_row_id("manual"));
    settle(cx, |cx| {
        m.read_with(cx, |m, _| m.selected_version().is_some())
    });
    click(&app, cx, "downloads-install");
    settle(cx, |cx| shows(&app, cx, "content-root:file"));
    assert!(
        network
            .block_on(server.received_requests())
            .unwrap()
            .is_empty(),
        "opening confirmation must not download files"
    );
    assert!(
        !store
            .load("Fab")
            .unwrap()
            .game_dir()
            .join("mods/fixture.jar")
            .exists()
    );
    click(&app, cx, "content-root:file");
    assert!(disabled(&app, cx, "content-install-continue"));
    click(&app, cx, "content-root:file");
    click(&app, cx, "content-install-continue");
    settle(cx, |cx| shows(&app, cx, "content-install-progress"));
    assert!(
        !shows(&app, cx, "content-install-continue"),
        "running state must not offer a second Install"
    );
    assert!(shows(&app, cx, "content-install-cancel"));
    settle(cx, |cx| shows(&app, cx, "content-install-done"));
    assert!(!shows(&app, cx, "content-install-continue"));
    click(&app, cx, "content-install-cancel");
    click(&app, cx, "rail-instances");
    click(&app, cx, row_id("Fab"));
    click(&app, cx, "tab-mods");
    let tab = mods_tab(&app, cx);
    mods_settled(&tab, cx);
    assert!(shows(&app, cx, mod_row_id("fixture.jar")));
}

#[gpui_kit::test]
fn cancelling_manual_wait_changes_no_instance_files(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let instance = store.create("Fab", "1.21.1").unwrap();
    set_loader(&store, "Fab", ComponentUid::Fabric);
    let version = ContentVersion::new(
        VersionRef::new(ProjectRef::new(Platform::Modrinth, "manual"), "file"),
        "Manual",
        ReleaseChannel::Release,
    )
    .with_compatibility(vec!["1.21.1".into()], vec![LoaderKind::Fabric])
    .with_files(vec![ContentFile::new(
        "manual.jar",
        4,
        FileHashes::new(Some("abcd".into()), None, None),
        None,
        "https://modrinth.com/mod/test",
    )]);
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_source(
            rt,
            data,
            Arc::new(FakeCatalog::new()),
            Arc::new(InstallSource(version)),
        )
    });
    list_settled(&instances_model(&app, cx), cx);
    let m = model(&app, cx);
    click(&app, cx, "rail-downloads");
    m.update(cx, |m, cx| {
        m.set_target(Some("Fab"), cx);
    });
    settle(cx, |cx| matches!(state(&m, cx), Results::Loaded(_)));
    click(&app, cx, hit_row_id("manual"));
    settle(cx, |cx| {
        m.read_with(cx, |m, _| m.selected_version().is_some())
    });
    click(&app, cx, "downloads-install");
    settle(cx, |cx| shows(&app, cx, "content-manual-file:file"));
    let guard = store.lock("Fab").unwrap();
    drop(guard);
    press(&app, cx, "escape");
    assert!(!instance.game_dir().join("mods/manual.jar").exists());
    assert!(!instance.dir.join("content-sources.toml").exists());
}

struct UpdateSource(Vec<ContentVersion>);
impl ContentSource for UpdateSource {
    fn search(&self, _: OnlineQuery) -> BoxFuture<'static, Result<OnlinePage, OnlineError>> {
        Box::pin(async { Err(OnlineError::Unavailable) })
    }
    fn project(
        &self,
        reference: ProjectRef,
    ) -> BoxFuture<'static, Result<ContentProject, OnlineError>> {
        Box::pin(async move {
            Ok(ContentProject::new(
                reference,
                ContentKind::Mod,
                "Updates",
                "https://modrinth.com/mod/test",
            ))
        })
    }
    fn version(
        &self,
        _: Platform,
        id: String,
    ) -> BoxFuture<'static, Result<ContentVersion, OnlineError>> {
        let version = self.0.iter().find(|v| v.reference().id() == id).cloned();
        Box::pin(async move { version.ok_or(OnlineError::NotFound) })
    }
    fn versions(
        &self,
        _: ProjectRef,
        _: TargetCompatibility,
    ) -> BoxFuture<'static, Result<Vec<ContentVersion>, OnlineError>> {
        let versions = self.0.clone();
        Box::pin(async move { Ok(versions) })
    }
}
#[gpui_kit::test]
fn local_check_select_update_refreshes_without_duplicate_jar(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let network = patina_app::runtime::start();
    let server = network.block_on(wiremock::MockServer::start());
    let mut versions = vec![];
    for (id, published) in [("old", 1), ("new", 2)] {
        fabric_jar(tmp.path(), &format!("{id}.jar"), &format!("Fixture {id}"));
        let file = tmp.path().join(format!("{id}.jar"));
        let bytes = std::fs::read(&file).unwrap();
        let hash = patina_core::download::sha1_file(&file).unwrap();
        network.block_on(async {
            wiremock::Mock::given(wiremock::matchers::path(format!("/{id}.jar")))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
                .mount(&server)
                .await;
        });
        versions.push(
            ContentVersion::new(
                VersionRef::new(ProjectRef::new(Platform::Modrinth, "manual"), id),
                id,
                ReleaseChannel::Release,
            )
            .with_published(published)
            .with_compatibility(vec!["1.21.1".into()], vec![LoaderKind::Fabric])
            .with_files(vec![ContentFile::new(
                format!("{id}.jar"),
                bytes.len() as u64,
                FileHashes::new(Some(hash), None, None),
                Some(format!("{}/{id}.jar", server.uri())),
                "https://modrinth.com/mod/test",
            )]),
        );
    }
    let source = Arc::new(UpdateSource(versions));
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let instance = store.create("Fab", "1.21.1").unwrap();
    set_loader(&store, "Fab", ComponentUid::Fabric);
    let service = Arc::new(ContentService::new(
        store.clone(),
        source.clone(),
        Arc::new(patina_core::download::Downloader::new(
            reqwest::Client::new(),
            patina_core::download::Mirror::official(),
        )),
    ));
    network.block_on(async {
        let request = InstallRequest::new(
            "Fab",
            VersionRef::new(ProjectRef::new(Platform::Modrinth, "manual"), "old"),
            TargetCompatibility::new(ContentKind::Mod, "1.21.1", Some(LoaderKind::Fabric)),
        );
        let plan = service.plan_install(request).await.unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        service
            .apply(plan, InstallChoices::default(), rx, None)
            .await
            .unwrap();
    });
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        let mut core = testing::core_with_source(rt, data, Arc::new(FakeCatalog::new()), source);
        core.content_service = service;
        core
    });
    list_settled(&instances_model(&app, cx), cx);
    click(&app, cx, row_id("Fab"));
    click(&app, cx, "tab-mods");
    let tab = mods_tab(&app, cx);
    mods_settled(&tab, cx);
    assert_eq!(
        label_of(&app, cx, "content-origin:mods/old.jar").as_deref(),
        Some("Modrinth")
    );
    click(&app, cx, "mods-check-updates");
    settle(cx, |cx| shows(&app, cx, "content-update:manual:new"));
    assert!(disabled(&app, cx, "content-update-apply"));
    assert!(instance.game_dir().join("mods/old.jar").exists());
    click(&app, cx, "content-update:manual:new");
    click(&app, cx, "content-update-apply");
    settle(cx, |cx| shows(&app, cx, "content-root:new"));
    assert!(instance.game_dir().join("mods/old.jar").exists());
    click(&app, cx, "content-install-continue");
    settle(cx, |cx| shows(&app, cx, "content-install-done"));
    click(&app, cx, "content-install-cancel");
    mods_settled(&tab, cx);
    assert!(shows(&app, cx, mod_row_id("new.jar")));
    assert!(!instance.game_dir().join("mods/old.jar").exists());
    assert!(!shows(&app, cx, "content-origin:mods/old.jar"));
    assert_eq!(
        label_of(&app, cx, "content-origin:mods/new.jar").as_deref(),
        Some("Modrinth")
    );
    click(&app, cx, "content-origin:mods/new.jar");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://modrinth.com/project/manual")
    );
}

#[gpui_kit::test]
fn browsing_pagination_uses_source_offset_and_resets_on_filter_change(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let source = Arc::new(FakeSource::with(|q| {
        Ok(OnlinePage {
            hits: (q.offset..q.offset + 20)
                .map(|n| hit(&format!("p{n}")))
                .collect(),
            total: 60,
        })
    }));
    let (app, m) = app_with(cx, tmp.path(), Some(source.clone()));
    click(&app, cx, "rail-downloads");
    settle(cx, |cx| matches!(state(&m, cx), Results::Loaded(_)));
    click(&app, cx, "downloads-next-page");
    settle(cx, |_| {
        source.queries().last().is_some_and(|q| q.offset == 20)
    });
    settle(cx, |cx| shows(&app, cx, hit_row_id("p20")));
    click(&app, cx, "downloads-prev-page");
    settle(cx, |cx| shows(&app, cx, hit_row_id("p0")));
    m.update(cx, |m, cx| m.set_sort(SortOrder::Updated, cx));
    settle(cx, |_| {
        source
            .queries()
            .last()
            .is_some_and(|q| q.offset == 0 && q.sort == SortOrder::Updated)
    });
}

struct ConfirmationSource(Vec<ContentVersion>);
impl ContentSource for ConfirmationSource {
    fn search(&self, _: OnlineQuery) -> BoxFuture<'static, Result<OnlinePage, OnlineError>> {
        Box::pin(async {
            Ok(OnlinePage {
                hits: vec![hit("manual")],
                total: 1,
            })
        })
    }
    fn project(
        &self,
        reference: ProjectRef,
    ) -> BoxFuture<'static, Result<ContentProject, OnlineError>> {
        let title = format!("{} project", reference.id());
        Box::pin(async move {
            Ok(ContentProject::new(
                reference,
                ContentKind::Mod,
                title,
                "https://modrinth.com/project/test",
            ))
        })
    }
    fn version(
        &self,
        _: Platform,
        id: String,
    ) -> BoxFuture<'static, Result<ContentVersion, OnlineError>> {
        let version = self.0.iter().find(|v| v.reference().id() == id).cloned();
        Box::pin(async move { version.ok_or(OnlineError::NotFound) })
    }
    fn versions(
        &self,
        project: ProjectRef,
        target: TargetCompatibility,
    ) -> BoxFuture<'static, Result<Vec<ContentVersion>, OnlineError>> {
        let versions = self
            .0
            .iter()
            .filter(|v| v.reference().project() == &project && target.accepts(v))
            .cloned()
            .collect();
        Box::pin(async move { Ok(versions) })
    }
}
fn confirmation_version(server: &wiremock::MockServer, project: &str, id: &str) -> ContentVersion {
    let bytes = id.as_bytes();
    let file = ContentFile::new(
        format!("{id}.jar"),
        bytes.len() as u64,
        FileHashes::new(
            Some({
                use std::io::Write;
                let mut temp = tempfile::NamedTempFile::new().unwrap();
                temp.write_all(bytes).unwrap();
                patina_core::download::sha1_file(temp.path()).unwrap()
            }),
            None,
            None,
        ),
        Some(format!("{}/{id}", server.uri())),
        "https://modrinth.com/project/test",
    );
    ContentVersion::new(
        VersionRef::new(ProjectRef::new(Platform::Modrinth, project), id),
        id,
        ReleaseChannel::Release,
    )
    .with_compatibility(vec!["1.21.1".into()], vec![LoaderKind::Fabric])
    .with_files(vec![file])
}
fn confirmation_app(
    cx: &mut TestAppContext,
    dir: &Path,
    versions: Vec<ContentVersion>,
) -> (Booted, InstanceStore) {
    let store = InstanceStore::new(DataDirs::new(dir).instances());
    store.create("Fab", "1.21.1").unwrap();
    set_loader(&store, "Fab", ComponentUid::Fabric);
    let source = Arc::new(ConfirmationSource(versions));
    let app = boot_core(cx, dir, AppSettings::default(), |rt, data| {
        let mut core =
            testing::core_with_source(rt, data, Arc::new(FakeCatalog::new()), source.clone());
        core.content_service = Arc::new(ContentService::new(
            core.store.clone(),
            source,
            Arc::new(
                patina_core::download::Downloader::new(
                    reqwest::Client::new(),
                    patina_core::download::Mirror::official(),
                )
                .with_retry(1, Duration::ZERO),
            ),
        ));
        core
    });
    list_settled(&instances_model(&app, cx), cx);
    (app, store)
}
fn open_confirmation(app: &Booted, cx: &mut TestAppContext) {
    let m = model(app, cx);
    click(app, cx, "rail-downloads");
    m.update(cx, |m, cx| {
        m.set_target(Some("Fab"), cx);
    });
    settle(cx, |cx| matches!(state(&m, cx), Results::Loaded(_)));
    click(app, cx, hit_row_id("manual"));
    settle(cx, |cx| {
        m.read_with(cx, |m, _| m.selected_version().is_some())
    });
    click(app, cx, "downloads-install");
}
#[gpui_kit::test]
fn confirmation_links_required_dependencies_and_cancelled_choices_download_nothing(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap();
    let network = patina_app::runtime::start();
    let server = network.block_on(wiremock::MockServer::start());
    for id in ["main-file", "req-file", "opt-file", "child-file"] {
        network.block_on(async {
            wiremock::Mock::given(wiremock::matchers::path(format!("/{id}")))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .set_body_bytes(id.as_bytes())
                        .set_delay(Duration::from_millis(700)),
                )
                .mount(&server)
                .await;
        });
    }
    let dependency = |project: &str, id: &str, relation| {
        Dependency::new(
            Some(ProjectRef::new(Platform::Modrinth, project)),
            Some(id.into()),
            relation,
        )
    };
    let root = confirmation_version(&server, "manual", "main-file").with_dependencies(vec![
        dependency("required", "req-file", DependencyRelation::Required),
        dependency("optional", "opt-file", DependencyRelation::Optional),
    ]);
    let optional =
        confirmation_version(&server, "optional", "opt-file").with_dependencies(vec![dependency(
            "child",
            "child-file",
            DependencyRelation::Required,
        )]);
    let (app, store) = confirmation_app(
        cx,
        tmp.path(),
        vec![
            root,
            confirmation_version(&server, "required", "req-file"),
            optional,
            confirmation_version(&server, "child", "child-file"),
        ],
    );
    open_confirmation(&app, cx);
    settle(cx, |cx| shows(&app, cx, "content-root:main-file"));
    assert!(
        network
            .block_on(server.received_requests())
            .unwrap()
            .is_empty()
    );
    assert!(disabled(&app, cx, "content-required:req-file"));
    assert_eq!(
        element(&app, cx, "content-required:req-file")
            .unwrap()
            .checked(),
        Some(true)
    );
    assert_eq!(
        element(&app, cx, "content-required:child-file")
            .unwrap()
            .checked(),
        Some(false)
    );
    click(&app, cx, "content-root:main-file");
    assert!(disabled(&app, cx, "content-install-continue"));
    assert_eq!(
        element(&app, cx, "content-required:req-file")
            .unwrap()
            .checked(),
        Some(false)
    );
    click(&app, cx, "content-root:main-file");
    click(&app, cx, "content-optional:optional");
    assert_eq!(
        element(&app, cx, "content-required:child-file")
            .unwrap()
            .checked(),
        Some(true)
    );
    click(&app, cx, "content-install-cancel");
    assert!(
        network
            .block_on(server.received_requests())
            .unwrap()
            .is_empty()
    );
    assert!(
        !store
            .load("Fab")
            .unwrap()
            .dir
            .join("content-sources.toml")
            .exists()
    );
    click(&app, cx, "downloads-install");
    settle(cx, |cx| shows(&app, cx, "content-root:main-file"));
    assert_eq!(
        element(&app, cx, "content-optional:optional")
            .unwrap()
            .checked(),
        Some(false)
    );
    click(&app, cx, "content-optional:optional");
    click(&app, cx, "content-install-continue");
    settle(cx, |cx| shows(&app, cx, "content-install-progress"));
    assert!(!shows(&app, cx, "content-install-continue"));
    settle(cx, |cx| shows(&app, cx, "content-install-done"));
    let mods = store.load("Fab").unwrap().game_dir().join("mods");
    for id in ["main-file", "req-file", "opt-file", "child-file"] {
        assert!(mods.join(format!("{id}.jar")).exists());
    }
    assert_eq!(
        network.block_on(server.received_requests()).unwrap().len(),
        4
    );
}
#[gpui_kit::test]
fn failed_install_can_retry_and_later_batch_item_requires_its_own_confirmation(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap();
    let network = patina_app::runtime::start();
    let server = network.block_on(wiremock::MockServer::start());
    network.block_on(async {
        wiremock::Mock::given(wiremock::matchers::path("/first"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;
    });
    let first = confirmation_version(&server, "manual", "first");
    let second = confirmation_version(&server, "second", "second");
    let (app, store) = confirmation_app(cx, tmp.path(), vec![first.clone(), second.clone()]);
    let instances = instances_model(&app, cx);
    cx.update_window(app.handle, |_, window, cx| {
        patina_app::downloads::install::open_installs(
            instances,
            vec![first, second]
                .into_iter()
                .map(|v| {
                    InstallRequest::new(
                        "Fab",
                        v.reference().clone(),
                        TargetCompatibility::new(
                            ContentKind::Mod,
                            "1.21.1",
                            Some(LoaderKind::Fabric),
                        ),
                    )
                })
                .collect(),
            window,
            cx,
        );
    })
    .unwrap();
    settle(cx, |cx| shows(&app, cx, "content-root:first"));
    assert!(
        network
            .block_on(server.received_requests())
            .unwrap()
            .is_empty()
    );
    click(&app, cx, "content-install-continue");
    settle(cx, |cx| shows(&app, cx, "content-install-error"));
    assert_eq!(
        label_of(&app, cx, "content-install-continue").as_deref(),
        Some("Retry")
    );
    assert!(
        !store
            .load("Fab")
            .unwrap()
            .game_dir()
            .join("mods/first.jar")
            .exists()
    );
    network.block_on(async {
        server.reset().await;
        wiremock::Mock::given(wiremock::matchers::path("/first"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"first"))
            .mount(&server)
            .await;
    });
    click(&app, cx, "content-install-continue");
    settle(cx, |cx| shows(&app, cx, "content-root:second"));
    assert!(
        store
            .load("Fab")
            .unwrap()
            .game_dir()
            .join("mods/first.jar")
            .exists()
    );
    assert!(
        !store
            .load("Fab")
            .unwrap()
            .game_dir()
            .join("mods/second.jar")
            .exists()
    );
    assert_eq!(
        network.block_on(server.received_requests()).unwrap().len(),
        1,
        "later selected updates must not auto-commit"
    );
    click(&app, cx, "content-install-cancel");
}
