// Real-service account flows through the production components.
use super::*;
use patina_core::accounts::{
    AccountService,
    error::AuthStage,
    testing::{AuthClock, AuthHttp, MemoryCredentials, MsaClient, credential_store},
};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, header, method, path},
};

struct Clock(std::sync::atomic::AtomicU64);
impl Default for Clock {
    fn default() -> Self {
        Self(std::sync::atomic::AtomicU64::new(1_800_000_000))
    }
}
impl AuthClock for Clock {
    fn unix_seconds(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn service(data: DataDirs, server: &MockServer) -> Arc<AccountService> {
    service_with_clock(data, server, Arc::new(Clock::default()))
}
fn service_with_clock(
    data: DataDirs,
    server: &MockServer,
    clock: Arc<Clock>,
) -> Arc<AccountService> {
    service_with_token_endpoint(data, server, clock, None)
}
fn service_with_token_endpoint(
    data: DataDirs,
    server: &MockServer,
    clock: Arc<Clock>,
    token_endpoint: Option<String>,
) -> Arc<AccountService> {
    let stages = [
        AuthStage::DeviceCode,
        AuthStage::MicrosoftToken,
        AuthStage::XboxLive,
        AuthStage::Xsts,
        AuthStage::MinecraftToken,
        AuthStage::Entitlement,
        AuthStage::Profile,
    ];
    let http = AuthHttp::for_test(std::array::from_fn(|i| {
        (
            stages[i],
            if i == 1 {
                token_endpoint
                    .clone()
                    .unwrap_or_else(|| format!("{}/1", server.uri()))
            } else {
                format!("{}/{i}", server.uri())
            },
        )
    }))
    .unwrap();
    let keyring = Arc::new(MemoryCredentials::default());
    keyring.set_error(Some(
        patina_core::accounts::testing::CredentialError::Unavailable,
    ));
    let mut service = AccountService::for_test(
        data.clone(),
        MsaClient::for_test(http, clock.clone()),
        credential_store(data, keyring),
        clock,
    );
    service.set_skin_endpoint_for_test(&server.uri()).unwrap();
    Arc::new(service)
}
async fn chain(server: &MockServer) {
    let bodies = [
        json!({"device_code":"DEVICE_SYNTHETIC", "user_code":"ABCD-EFGH", "verification_uri":"https://microsoft.com/link", "expires_in":120,"interval":1}),
        json!({"access_token":"MS_SYNTHETIC","refresh_token":"REFRESH_SYNTHETIC","expires_in":3600}),
        json!({"Token":"XBL_SYNTHETIC","NotAfter":"2030-01-01T00:00:00Z","DisplayClaims":{"xui":[{"uhs":"HASH_SYNTHETIC","xid":"123456"}]}}),
        json!({"Token":"XSTS_SYNTHETIC","NotAfter":"2030-01-01T00:00:00Z","DisplayClaims":{"xui":[{"uhs":"HASH_SYNTHETIC","xid":"123456"}]}}),
        json!({"access_token":"MC_SYNTHETIC","expires_in":3600,"token_type":"Bearer"}),
        json!({"items":[{"name":"game_minecraft"}]}),
        json!({"id":"0123456789abcdef0123456789abcdef","name":"Steve"}),
    ];
    for (i, body) in bodies.into_iter().enumerate() {
        let mock =
            Mock::given(path(format!("/{i}"))).and(method(if i >= 5 { "GET" } else { "POST" }));
        let mock = match i {
            0 => mock.and(body_string_contains("scope=XboxLive.signin+offline_access")),
            1 => mock.and(body_string_contains("device_code=DEVICE_SYNTHETIC")),
            2 => mock.and(body_string_contains("MS_SYNTHETIC")),
            3 => mock.and(body_string_contains("XBL_SYNTHETIC")),
            4 => mock.and(body_string_contains("XSTS_SYNTHETIC")),
            _ => mock.and(header("authorization", "Bearer MC_SYNTHETIC")),
        };
        mock.respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(0..)
            .mount(server)
            .await;
    }
}

#[gpui_kit::test]
fn real_login_commits_before_done_and_updates_switcher(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    let accounts = service(DataDirs::new(tmp.path()), &server);
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts.clone())
    });
    open_page(&app, cx);
    click(&app, cx, "accounts-add-msa");
    assert_eq!(wait_code(&app, cx), "ABCD-EFGH");
    click(&app, cx, "login-copy");
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("ABCD-EFGH")
    );
    click(&app, cx, "login-open");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://microsoft.com/link")
    );
    settle(cx, |cx| label_of(&app, cx, "login-done").is_some());
    assert_eq!(
        label_of(&app, cx, "login-done").as_deref(),
        Some("Signed in as Steve")
    );
    assert!(shows(&app, cx, "login-storage"));
    assert!(accounts_file(tmp.path()).contains("Steve"));
    assert_eq!(
        rt.block_on(accounts.snapshot()).unwrap().accounts().len(),
        1
    );
    click(&app, cx, "login-close");
    settle(cx, |cx| title_label(&app, cx) == signed_in("Steve"));
    assert_eq!(names(&app, cx), Some(vec!["Steve".into()]));
    assert!(!disabled(&app, cx, "accounts-add-offline"));
    assert!(Arc::ptr_eq(
        &cx.update(|cx| Core::global(cx).accounts()),
        &accounts
    ));
    // Same UUID re-sign-in updates one row, never an event-derived duplicate.
    click(&app, cx, "accounts-add-msa");
    settle(cx, |cx| shows(&app, cx, "login-done"));
    click(&app, cx, "login-close");
    assert_eq!(
        rt.block_on(accounts.snapshot()).unwrap().accounts().len(),
        1
    );
    let requests = rt.block_on(server.received_requests()).unwrap();
    assert_eq!(
        requests.len(),
        14,
        "two complete seven-stage chains, no watch-driven authentication"
    );
    click(&app, cx, "accounts-add-offline");
    click(&app, cx, "offline-name");
    cx.update_window(app.handle, |_, window, cx| window.input("LocalPlayer", cx))
        .unwrap();
    click(&app, cx, "offline-add");
    settle(cx, |cx| {
        names(&app, cx).is_some_and(|names| names.len() == 2)
    });
    let snapshot = rt.block_on(accounts.snapshot()).unwrap();
    let offline = snapshot
        .accounts()
        .iter()
        .find(|a| a.username == "LocalPlayer")
        .unwrap();
    assert_eq!(
        offline.offline_origin(),
        Some(patina_core::accounts::OfflineOrigin::Entitled)
    );
    let owner = snapshot
        .accounts()
        .iter()
        .find(|a| a.username == "Steve")
        .unwrap();
    rt.block_on(accounts.remove(&owner.id)).unwrap();
    settle(cx, |cx| disabled(&app, cx, "accounts-add-offline"));
    assert_eq!(names(&app, cx), Some(vec!["LocalPlayer".into()]));
    assert!(disabled(&app, cx, set_current_id(&offline.id)));
}

#[gpui_kit::test]
fn production_offline_button_follows_eligibility(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    open_page(&app, cx);
    assert!(shows(&app, cx, "accounts-add-offline"));
    assert!(disabled(&app, cx, "accounts-add-offline"));
    assert!(shows(&app, cx, "offline-requires-entitlement"));
    click(&app, cx, "accounts-add-offline");
    assert!(!shows(&app, cx, "dialog"));
    assert!(!tmp.path().join("accounts.toml").exists());
}

#[gpui_kit::test]
fn close_retry_discards_old_attempt(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    // The raw loopback responder acknowledges entry and waits for an explicit
    // release. No elapsed-time assumption can turn precommit into postcommit.
    let (endpoint, mut entered, responder) = rt.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/1", listener.local_addr().unwrap());
        let (entered, requests) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let entered = entered.clone();
                tokio::spawn(async move {
                    tokio::time::timeout(Duration::from_secs(5), async {
                        let mut request = Vec::new();
                        let mut byte = [0];
                        while !request.ends_with(b"\r\n\r\n") {
                            stream.read_exact(&mut byte).await.unwrap();
                            request.push(byte[0]);
                            assert!(request.len() < 8192);
                        }
                        let headers = String::from_utf8(request).unwrap();
                        assert!(headers.starts_with("POST /1 HTTP/1.1\r\n"));
                        let length: usize = headers.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                        }).unwrap();
                        assert!(length < 8192);
                        // Drain the request before replying: closing a socket
                        // with unread data can reset an otherwise valid response.
                        let mut body = vec![0; length];
                        stream.read_exact(&mut body).await.unwrap();
                        assert!(String::from_utf8(body).unwrap().contains("device_code=DEVICE_SYNTHETIC"));
                    }).await.expect("token request deadline");
                    let (release, released) = tokio::sync::oneshot::channel();
                    let (finished, finish) = tokio::sync::oneshot::channel();
                    entered.send((release, finish)).unwrap();
                    if matches!(tokio::time::timeout(Duration::from_secs(10), released).await, Ok(Ok(()))) {
                        let body = json!({"access_token":"MS_SYNTHETIC","refresh_token":"REFRESH_SYNTHETIC","expires_in":3600}).to_string();
                        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                        // The cancelled client may already have closed its socket.
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                    let _ = finished.send(());
                });
            }
        });
        (endpoint, requests, task)
    });
    let accounts = service_with_token_endpoint(
        DataDirs::new(tmp.path()),
        &server,
        Arc::new(Clock::default()),
        Some(endpoint),
    );
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts.clone())
    });
    open_page(&app, cx);
    click(&app, cx, "accounts-add-msa");
    wait_code(&app, cx);
    for dismissal in 0..3 {
        let mut barrier = None;
        settle(cx, |_| {
            barrier = entered.try_recv().ok();
            barrier.is_some()
        });
        let (release, finished) = barrier.unwrap();
        match dismissal {
            0 => click(&app, cx, "login-cancel"),
            1 => press(&app, cx, "escape"),
            _ => dialog_click(&app, cx, "close"),
        }
        assert!(!shows(&app, cx, "dialog"));
        assert!(
            rt.block_on(accounts.snapshot())
                .unwrap()
                .accounts()
                .is_empty()
        );
        click(&app, cx, "accounts-add-msa");
        wait_code(&app, cx);
        release.send(()).unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), finished)
                .await
                .unwrap()
                .unwrap()
        });
        assert!(!shows(&app, cx, "login-done"));
        assert!(
            rt.block_on(accounts.snapshot())
                .unwrap()
                .accounts()
                .is_empty()
        );
    }
    let mut barrier = None;
    settle(cx, |_| {
        barrier = entered.try_recv().ok();
        barrier.is_some()
    });
    let (release, finished) = barrier.unwrap();
    release.send(()).unwrap();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), finished)
            .await
            .unwrap()
            .unwrap()
    });
    settle(cx, |cx| shows(&app, cx, "login-done"));
    assert_eq!(
        rt.block_on(accounts.snapshot()).unwrap().accounts().len(),
        1
    );
    assert_eq!(
        label_of(&app, cx, "login-done").as_deref(),
        Some("Signed in as Steve")
    );
    press(&app, cx, "escape");
    assert!(!shows(&app, cx, "dialog"));
    // Dismissal after commit is not rollback.
    assert_eq!(
        rt.block_on(accounts.snapshot()).unwrap().accounts().len(),
        1
    );
    settle(cx, |cx| title_label(&app, cx) == signed_in("Steve"));
    responder.abort();
}

#[gpui_kit::test]
fn login_storage_failure_has_no_success(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("accounts.toml"), "# preserved fixture\n").unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    let accounts = service(DataDirs::new(tmp.path()), &server);
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts.clone())
    });
    open_page(&app, cx);
    click(&app, cx, "accounts-add-msa");
    wait_code(&app, cx);
    let before = accounts_file(tmp.path());
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    settle(cx, |cx| shows(&app, cx, "login-failed"));
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!shows(&app, cx, "login-done"));
    assert!(shows(&app, cx, "login-retry"));
    assert_eq!(accounts_file(tmp.path()), before);
    click(&app, cx, "login-retry");
    settle(cx, |cx| shows(&app, cx, "login-done"));
    assert_eq!(
        rt.block_on(accounts.snapshot()).unwrap().accounts().len(),
        1
    );
}

#[gpui_kit::test]
fn missing_reference_row_offers_reauthentication(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("accounts.toml"),
        ACCOUNT.replace("token_ref = \"00000000-0000-0000-0000-000000000001\"\n", ""),
    )
    .unwrap();
    let login = Arc::new(HeldLogin::new("REAUTH"));
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_login(rt, data, Arc::new(FakeCatalog::new()), login)
    });
    open_page(&app, cx);
    assert_eq!(names(&app, cx), Some(vec!["Steve".into()]));
    click(&app, cx, set_current_id("msa-1"));
    assert_eq!(wait_code(&app, cx), "REAUTH");
    assert_eq!(
        stored_current(tmp.path()).as_deref(),
        Some("msa-1"),
        "failed selection never rewrites metadata"
    );
    press(&app, cx, "escape");
}

#[gpui_kit::test]
fn account_rows_and_switcher_load_and_invalidate_real_heads(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    rt.block_on(Mock::given(path("/6")).and(method("GET")).and(header("authorization", "Bearer MC_SYNTHETIC"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"0123456789abcdef0123456789abcdef","name":"Steve","skins":[{"state":"ACTIVE","url":"https://textures.minecraft.net/texture/abcdef"}]})))
        .with_priority(1).mount(&server));
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        64,
        64,
        image::Rgba([20, 80, 160, 255]),
    ))
    .write_to(&mut png, image::ImageFormat::Png)
    .unwrap();
    rt.block_on(
        Mock::given(path("/texture/abcdef"))
            .and(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png.into_inner()))
            .expect(1)
            .mount(&server),
    );
    let accounts = service(DataDirs::new(tmp.path()), &server);
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts.clone())
    });
    open_page(&app, cx);
    click(&app, cx, "accounts-add-msa");
    settle(cx, |cx| shows(&app, cx, "login-done"));
    click(&app, cx, "login-close");
    settle(cx, |cx| {
        cx.update_window(app.handle, |_, window, cx| {
            window.render_frame(cx);
            window
                .within("accounts")
                .try_find("account-head-0123456789abcdef0123456789abcdef")
                .is_some()
        })
        .unwrap()
    });
    assert!(shows(&app, cx, "switcher-head"));
    let account = rt.block_on(accounts.snapshot()).unwrap().accounts()[0].clone();
    let image = cx
        .update(|cx| testing::cached_avatar(&account, cx))
        .unwrap();
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.has_image_atlas_entry(&image));
    })
    .unwrap();
    let metadata = accounts_file(tmp.path());
    std::fs::write(tmp.path().join("accounts.toml"), "not [valid").unwrap();
    assert!(rt.block_on(accounts.reload()).is_err());
    settle(cx, |cx| {
        title_label(&app, cx) == AccountLabel::SignedOut && !shows(&app, cx, "switcher-head")
    });
    cx.update_window(app.handle, |_, window, _| {
        assert!(!window.has_image_atlas_entry(&image))
    })
    .unwrap();
    std::fs::write(tmp.path().join("accounts.toml"), metadata).unwrap();
    rt.block_on(accounts.reload()).unwrap();
    settle(cx, |cx| shows(&app, cx, "switcher-head"));
    let id = rt.block_on(accounts.snapshot()).unwrap().accounts()[0]
        .id
        .clone();
    rt.block_on(accounts.remove(&id)).unwrap();
    settle(cx, |cx| names(&app, cx) == Some(vec![]));
    assert!(!shows(
        &app,
        cx,
        "account-head-0123456789abcdef0123456789abcdef"
    ));
    assert!(!shows(&app, cx, "switcher-head"));
    cx.update_window(app.handle, |_, window, _| {
        assert!(!window.has_image_atlas_entry(&image))
    })
    .unwrap();
}

/// Prepares a deliberately nonexistent executable; even a failed spawn guard
/// cannot start a process. A current.log side effect proves entry to spawn.
struct NoSpawnPreparation;
impl LaunchBackend for NoSpawnPreparation {
    fn prepare(
        &self,
        inst: Instance,
        account: LaunchAccount,
        _: ProgressSink,
    ) -> futures::future::BoxFuture<'static, Result<PendingLaunch, SessionError>> {
        Box::pin(async move {
            let cmd = LaunchCommand {
                program: inst.dir.join("nonexistent-java"),
                args: vec![],
                cwd: inst.dir.clone(),
            };
            GameSession::prepare_launch_with_account_and_warnings_for_test(
                &inst,
                cmd,
                &account,
                vec![PrepareWarning::LowMemory {
                    requested_mib: 4096,
                    available_mib: 1024,
                }],
            )
        })
    }
}

#[gpui_kit::test]
fn removed_or_expired_grant_never_spawns(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    let clock = Arc::new(Clock::default());
    let accounts = service_with_clock(DataDirs::new(tmp.path()), &server, clock.clone());
    let inst = InstanceStore::new(DataDirs::new(tmp.path()).instances())
        .create("Grant", "1.21.1")
        .unwrap();
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        let mut core =
            testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts.clone());
        core.launch_backend = Arc::new(NoSpawnPreparation);
        core
    });
    open_page(&app, cx);
    click(&app, cx, "accounts-add-msa");
    settle(cx, |cx| shows(&app, cx, "login-done"));
    click(&app, cx, "login-close");
    click(&app, cx, "rail-instances");
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "launch");
    settle(cx, |cx| shows(&app, cx, "prelaunch-title"));
    clock
        .0
        .store(1_800_003_601, std::sync::atomic::Ordering::SeqCst);
    dialog_click(&app, cx, "ok");
    settle(cx, |cx| failed_message(&model, cx, &inst.id).is_some());
    assert!(!inst.dir.join("patina-logs/current.log").exists());
    assert!(shows(&app, cx, "instance-op-retry"));
    // Retry must resolve afresh, not reuse the expired grant. The refresh
    // request is a different form from the device-code request above.
    rt.block_on(Mock::given(path("/1")).and(method("POST")).and(body_string_contains("grant_type=refresh_token")).and(body_string_contains("refresh_token=REFRESH_SYNTHETIC"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"MS_SYNTHETIC","refresh_token":"REFRESH_SYNTHETIC","expires_in":3600}))).expect(1).with_priority(1).mount(&server));
    click(&app, cx, "instance-op-retry");
    settle(cx, |cx| shows(&app, cx, "prelaunch-title"));
    let id = rt.block_on(accounts.snapshot()).unwrap().accounts()[0]
        .id
        .clone();
    rt.block_on(accounts.remove(&id)).unwrap();
    dialog_click(&app, cx, "ok");
    settle(cx, |cx| failed_message(&model, cx, &inst.id).is_some());
    assert!(!inst.dir.join("patina-logs/current.log").exists());
    assert!(run_state(&model, cx, &inst.id).is_none());
}

fn assert_contained(
    container: gpui_kit::Bounds<gpui_kit::Pixels>,
    child: gpui_kit::Bounds<gpui_kit::Pixels>,
    name: &str,
) {
    assert!(
        child.size.width > px(0.) && child.size.height > px(0.),
        "empty {name}: {child:?}"
    );
    assert!(
        child.left() >= container.left()
            && child.right() <= container.right()
            && child.top() >= container.top()
            && child.bottom() <= container.bottom(),
        "{name} {child:?} outside {container:?}"
    );
}
fn assert_separate(a: gpui_kit::Bounds<gpui_kit::Pixels>, b: gpui_kit::Bounds<gpui_kit::Pixels>) {
    assert!(
        a.right() <= b.left()
            || b.right() <= a.left()
            || a.bottom() <= b.top()
            || b.bottom() <= a.top(),
        "overlap: {a:?} / {b:?}"
    );
}
fn assert_dialog_geometry(app: &Booted, cx: &mut TestAppContext, ids: &[&'static str]) {
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let dialog = window.within("dialog").find(0usize).bounds();
        let viewport = gpui_kit::Bounds::new(Default::default(), window.viewport_size());
        assert_contained(viewport, dialog, "dialog");
        let mut bounds = vec![];
        for id in ids {
            let target = window.find(*id).bounds();
            assert_contained(dialog, target, id);
            for other in &bounds {
                assert_separate(*other, target);
            }
            bounds.push(target);
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn login_locales_scale_and_focus_use_real_components(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    let accounts = service(DataDirs::new(tmp.path()), &server);
    rt.block_on(patina_core::accounts::testing::seed_verified(
        &accounts,
        "LongPlayer123456",
        "00000000-0000-0000-0000-000000000001".parse().unwrap(),
        "SYNTHETIC",
        1_800_000_000,
        1_800_003_600,
    ))
    .unwrap();
    rt.block_on(patina_core::accounts::testing::seed_verified(
        &accounts,
        "SecondPlayer1234",
        "00000000-0000-0000-0000-000000000002".parse().unwrap(),
        "SYNTHETIC",
        1_800_000_000,
        1_800_003_600,
    ))
    .unwrap();
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts)
    });
    open_page(&app, cx);
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    for language in [Language::En, Language::ZhCn] {
        app.language(language);
        for scale in [0.8, 1., 2.] {
            cx.update_window(app.handle, |_, window, cx| {
                patina_app::scale::apply_scale(scale, cx);
                window.render_frame(cx);
            })
            .unwrap();
            cx.update_window(app.handle, |_, window, cx| {
                window.render_frame(cx);
                let page = window.find("accounts").bounds();
                let toolbar = window.find("accounts-toolbar").bounds();
                assert_contained(page, toolbar, "toolbar");
                let mut ids = vec![
                    "accounts-heading",
                    "accounts-add-offline",
                    "accounts-add-msa",
                ];
                if cfg!(feature = "dev-offline") {
                    ids.push("accounts-add-dev-offline");
                }
                let mut bounds = vec![];
                for id in ids {
                    let b = window.find(id).bounds();
                    assert_contained(toolbar, b, id);
                    for other in &bounds {
                        assert_separate(*other, b);
                    }
                    bounds.push(b);
                }
                let id = "00000000000000000000000000000001";
                let row = window.find(account_row_id(id)).bounds();
                assert_contained(page, row, "account row");
                let identity = window
                    .find(gpui_kit::ElementId::Name(
                        format!("account-identity-{id}").into(),
                    ))
                    .bounds();
                let select = window.find(set_current_id(id)).bounds();
                let remove = window.find(remove_id(id)).bounds();
                for (name, b) in [
                    ("identity", identity),
                    ("selection", select),
                    ("removal", remove),
                ] {
                    assert_contained(row, b, name);
                }
                assert_separate(identity, select);
                assert_separate(select, remove);
            })
            .unwrap();
            tab_to(&app, cx, "accounts-add-msa");
            press(&app, cx, "enter");
            wait_code(&app, cx);
            assert_dialog_geometry(
                &app,
                cx,
                &["login-code", "login-copy", "login-open", "login-cancel"],
            );
            tab_to(&app, cx, "login-copy");
            press(&app, cx, "enter");
            assert_eq!(
                cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
                Some("ABCD-EFGH")
            );
            tab_to(&app, cx, "login-open");
            press(&app, cx, "enter");
            assert_eq!(
                cx.opened_url().as_deref(),
                Some("https://microsoft.com/link")
            );
            press(&app, cx, "escape");
            assert!(!shows(&app, cx, "dialog"));
            assert_eq!(
                element(&app, cx, "accounts-add-msa").unwrap().focused(),
                Some(true)
            );
            click(&app, cx, "accounts-add-msa");
            settle(cx, |cx| shows(&app, cx, "login-done"));
            assert_dialog_geometry(&app, cx, &["login-done", "login-storage", "login-close"]);
            click(&app, cx, "login-close");
        }
    }
}

#[gpui_kit::test]
fn no_entitlement_explains_requirement_without_retry_loop(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    rt.block_on(
        Mock::given(path("/5"))
            .and(method("GET"))
            .and(header("authorization", "Bearer MC_SYNTHETIC"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[]})))
            .with_priority(1)
            .mount(&server),
    );
    let accounts = service(DataDirs::new(tmp.path()), &server);
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts.clone())
    });
    open_page(&app, cx);
    click(&app, cx, "accounts-add-msa");
    settle(cx, |cx| shows(&app, cx, "login-failed"));
    assert!(!shows(&app, cx, "login-retry"));
    assert!(!shows(&app, cx, "login-done"));
    assert!(
        rt.block_on(accounts.snapshot())
            .unwrap()
            .accounts()
            .is_empty()
    );
}

#[gpui_kit::test]
fn refresh_failure_does_not_change_current(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    let clock = Arc::new(Clock::default());
    let accounts = service_with_clock(DataDirs::new(tmp.path()), &server, clock.clone());
    let inst = InstanceStore::new(DataDirs::new(tmp.path()).instances())
        .create("Refresh", "1.21.1")
        .unwrap();
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts.clone())
    });
    open_page(&app, cx);
    click(&app, cx, "accounts-add-msa");
    settle(cx, |cx| shows(&app, cx, "login-done"));
    click(&app, cx, "login-close");
    let before = rt
        .block_on(accounts.snapshot())
        .unwrap()
        .current()
        .unwrap()
        .id
        .clone();
    clock
        .0
        .store(1_800_003_601, std::sync::atomic::Ordering::SeqCst);
    rt.block_on(
        Mock::given(path("/1"))
            .and(method("POST"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .with_priority(1)
            .mount(&server),
    );
    click(&app, cx, "rail-instances");
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "launch");
    settle(cx, |cx| failed_message(&model, cx, &inst.id).is_some());
    assert!(shows(&app, cx, "instance-op-retry"));
    assert!(!shows(&app, cx, "instance-op-fix"));
    assert_eq!(
        rt.block_on(accounts.snapshot())
            .unwrap()
            .current()
            .unwrap()
            .id,
        before
    );
    assert_eq!(title_label(&app, cx), signed_in("Steve"));
    assert!(!inst.dir.join("patina-logs/current.log").exists());
    open_page(&app, cx);
    assert!(!disabled(&app, cx, "accounts-add-offline"));
}

#[gpui_kit::test]
fn offline_save_failure_keeps_input_and_recovers(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    let accounts = service(DataDirs::new(tmp.path()), &server);
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts.clone())
    });
    open_page(&app, cx);
    click(&app, cx, "accounts-add-msa");
    settle(cx, |cx| shows(&app, cx, "login-done"));
    click(&app, cx, "login-close");
    settle(cx, |cx| !disabled(&app, cx, "accounts-add-offline"));
    click(&app, cx, "accounts-add-offline");
    cx.update_window(app.handle, |_, window, cx| window.input("LocalPlayer", cx))
        .unwrap();
    let before = accounts_file(tmp.path());
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    click(&app, cx, "offline-add");
    settle(cx, |cx| shows(&app, cx, "offline-save-error"));
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        element(&app, cx, "offline-name").unwrap().value(),
        Some("LocalPlayer")
    );
    assert!(!disabled(&app, cx, "offline-add"));
    assert_eq!(accounts_file(tmp.path()), before);
    click(&app, cx, "offline-add");
    settle(cx, |cx| !shows(&app, cx, "dialog"));
    assert_eq!(
        rt.block_on(accounts.snapshot()).unwrap().accounts().len(),
        2
    );
}

#[gpui_kit::test]
fn changing_selection_during_warnings_keeps_captured_identity(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    let accounts = service(DataDirs::new(tmp.path()), &server);
    let inst = InstanceStore::new(DataDirs::new(tmp.path()).instances())
        .create("Captured", "1.21.1")
        .unwrap();
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        let mut core =
            testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts.clone());
        core.launch_backend = Arc::new(NoSpawnPreparation);
        core
    });
    open_page(&app, cx);
    click(&app, cx, "accounts-add-msa");
    settle(cx, |cx| shows(&app, cx, "login-done"));
    click(&app, cx, "login-close");
    click(&app, cx, "rail-instances");
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "launch");
    settle(cx, |cx| shows(&app, cx, "prelaunch-title"));
    let other = rt.block_on(accounts.add_offline("OtherPlayer")).unwrap();
    rt.block_on(accounts.set_current(&other.id)).unwrap();
    settle(cx, |cx| title_label(&app, cx) == signed_in("OtherPlayer"));
    dialog_click(&app, cx, "ok");
    settle(cx, |cx| failed_message(&model, cx, &inst.id).is_some());
    // The nonexistent executable fails only after validation. Re-resolving the
    // new selection would mismatch the prepared Steve identity and never enter spawn.
    assert!(inst.dir.join("patina-logs/current.log").exists());
    assert!(running_game(&inst).is_none());
    assert_eq!(
        rt.block_on(accounts.snapshot())
            .unwrap()
            .current()
            .unwrap()
            .id,
        other.id
    );
}

#[gpui_kit::test]
fn superseded_error_delivery_does_not_poison_recovered_avatar_revision(cx: &mut TestAppContext) {
    use patina_app::core_bridge::AccountsChanged;

    let tmp = tempfile::tempdir().unwrap();
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    rt.block_on(chain(&server));
    rt.block_on(Mock::given(path("/6")).and(method("GET")).and(header("authorization", "Bearer MC_SYNTHETIC"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"0123456789abcdef0123456789abcdef","name":"Steve","skins":[{"state":"ACTIVE","url":"https://textures.minecraft.net/texture/abcdef"}]})))
        .with_priority(1).mount(&server));
    for (hash, pixel) in [
        ("abcdef", [20, 80, 160, 255]),
        ("fedcba", [160, 80, 20, 255]),
    ] {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(64, 64, image::Rgba(pixel)))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        rt.block_on(
            Mock::given(path(format!("/texture/{hash}")))
                .and(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(png.into_inner()))
                .expect(1)
                .mount(&server),
        );
    }
    let accounts = service(DataDirs::new(tmp.path()), &server);
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts.clone())
    });
    open_page(&app, cx);
    click(&app, cx, "accounts-add-msa");
    settle(cx, |cx| shows(&app, cx, "login-done"));
    click(&app, cx, "login-close");
    settle(cx, |cx| shows(&app, cx, "switcher-head"));
    let initial = rt.block_on(accounts.snapshot()).unwrap();
    let old_image = cx
        .update(|cx| testing::cached_avatar(initial.current().unwrap(), cx))
        .unwrap();
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.has_image_atlas_entry(&old_image));
    })
    .unwrap();
    let metadata = accounts_file(tmp.path());
    let recovered_metadata = metadata.replace("skin_hash = \"abcdef\"", "skin_hash = \"fedcba\"");
    assert_ne!(metadata, recovered_metadata);

    let (mut captured, release) = cx.update(testing::hold_next_account_snapshot_error);
    std::fs::write(tmp.path().join("accounts.toml"), "not [valid").unwrap();
    assert!(rt.block_on(accounts.reload()).is_err());
    let error_revision = *accounts.changes().borrow();
    assert_eq!(error_revision, initial.revision() + 1);
    // The barrier acknowledges only an Err returned by the real service, not a
    // request that might subsequently read the recovered successful snapshot.
    settle(cx, |_| {
        captured
            .try_recv()
            .is_ok_and(|revision| revision == error_revision)
    });
    assert!(rt.block_on(accounts.snapshot()).is_err());
    assert_eq!(
        cx.update(|cx| testing::account_snapshot_delivery_count(cx)),
        1
    );
    assert_eq!(
        cx.update(|cx| AccountsChanged::epoch(cx).read(cx).get()),
        initial.revision()
    );

    std::fs::write(tmp.path().join("accounts.toml"), recovered_metadata).unwrap();
    rt.block_on(accounts.reload()).unwrap();
    let recovered = rt.block_on(accounts.snapshot()).unwrap();
    assert_eq!(recovered.revision(), error_revision + 1);
    let account = recovered.current().unwrap();
    assert_eq!(account.skin_hash(), Some("fedcba"));
    release.send(()).unwrap();
    settle(cx, |cx| {
        cx.update(|cx| testing::account_snapshot_delivery_count(cx)) == 2
            && cx.update(|cx| AccountsChanged::epoch(cx).read(cx).get()) == recovered.revision()
    });
    let mut loaded = None;
    for _ in 0..500 {
        cx.run_until_parked();
        loaded = cx.update(|cx| testing::cached_avatar(account, cx));
        if loaded.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let image = loaded.expect("recovered revision must load its head after obsolete Err delivery");
    assert!(!Arc::ptr_eq(&image, &old_image));
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .within("accounts")
                .find("account-head-0123456789abcdef0123456789abcdef")
                .visible()
        );
        assert!(window.find("switcher-head").visible());
        assert!(window.has_image_atlas_entry(&image));
        assert!(!window.has_image_atlas_entry(&old_image));
    })
    .unwrap();

    // No extra mutation/revision is needed to load the recovered head. Repeated
    // follow and unchanged reloads must leave the settled follower asleep.
    cx.update(|cx| {
        AccountsChanged::follow_core(cx);
        AccountsChanged::follow_core(cx);
    });
    for _ in 0..3 {
        rt.block_on(accounts.reload()).unwrap();
        cx.run_until_parked();
    }
    assert_eq!(*accounts.changes().borrow(), recovered.revision());
    assert_eq!(
        cx.update(|cx| AccountsChanged::epoch(cx).read(cx).get()),
        recovered.revision()
    );
    assert_eq!(
        cx.update(|cx| testing::account_snapshot_delivery_count(cx)),
        2
    );
    assert!(Arc::ptr_eq(
        &image,
        &cx.update(|cx| testing::cached_avatar(account, cx)).unwrap()
    ));
}

#[gpui_kit::test]
fn login_long_error_content_is_contained_at_supported_scales(cx: &mut TestAppContext) {
    use patina_core::accounts::login::{CancelToken, DeviceCodeLogin, LoginError, LoginEvent};
    // Presentation-only injection: no claim of authentication or durable storage.
    struct LongError;
    impl DeviceCodeLogin for LongError {
        fn start(
            &self,
            events: tokio::sync::mpsc::UnboundedSender<LoginEvent>,
            _: CancelToken,
        ) -> futures::future::BoxFuture<'static, ()> {
            Box::pin(async move {
                let message = "The connection could not be completed. Check the network and try signing in again. 无法完成连接，请检查网络后重新登录。";
                events
                    .send(LoginEvent::Failed(LoginError::Other(message.repeat(3))))
                    .unwrap();
            })
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let app = boot_core(cx, tmp.path(), AppSettings::default(), |rt, data| {
        testing::core_with_login(rt, data, Arc::new(FakeCatalog::new()), Arc::new(LongError))
    });
    open_page(&app, cx);
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    for language in [Language::En, Language::ZhCn] {
        app.language(language);
        for scale in [0.8, 1., 2.] {
            cx.update_window(app.handle, |_, window, cx| {
                patina_app::scale::apply_scale(scale, cx);
                window.render_frame(cx);
            })
            .unwrap();
            click(&app, cx, "accounts-add-msa");
            settle(cx, |cx| shows(&app, cx, "login-failed"));
            assert_dialog_geometry(
                &app,
                cx,
                &["login-title", "login-failed", "login-close", "login-retry"],
            );
            press(&app, cx, "escape");
            assert!(!shows(&app, cx, "dialog"));
        }
    }
}
