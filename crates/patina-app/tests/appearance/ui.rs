use super::*;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use patina_core::accounts::{
    AccountService,
    error::AuthStage,
    testing::{AuthClock, AuthHttp, MemoryCredentials, MsaClient, credential_store, seed_verified},
};
use std::io::Cursor;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
const ID: &str = "0123456789abcdef0123456789abcdef";
struct Clock;
impl AuthClock for Clock {
    fn unix_seconds(&self) -> u64 {
        1_800_000_000
    }
}
fn png() -> Vec<u8> {
    let skin = RgbaImage::from_fn(64, 64, |x, y| Rgba([x as u8 * 3, y as u8 * 3, 180, 255]));
    let mut out = Cursor::new(vec![]);
    DynamicImage::ImageRgba8(skin)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    out.into_inner()
}
fn fixture(cx: &mut TestAppContext, dir: &Path) -> (Booted, MockServer, Arc<AccountService>) {
    let rt = patina_app::runtime::start();
    let server = rt.block_on(MockServer::start());
    let data = DataDirs::new(dir);
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
        (stages[i], format!("{}/{i}", server.uri()))
    }))
    .unwrap();
    let mut service = AccountService::for_test(
        data.clone(),
        MsaClient::for_test(http, Arc::new(Clock)),
        credential_store(data, Arc::new(MemoryCredentials::default())),
        Arc::new(Clock),
    );
    service.set_skin_endpoint_for_test(&server.uri()).unwrap();
    rt.block_on(seed_verified(
        &service,
        "Steve",
        ID.parse().unwrap(),
        "FAKE",
        1_800_000_000,
        1_800_003_600,
    ))
    .unwrap();
    rt.block_on(async {
  Mock::given(path("/6")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":ID,"skins":[{"state":"ACTIVE","url":"https://textures.minecraft.net/texture/abcdef","variant":"CLASSIC"}],"capes":[{"id":"cape-one","state":"INACTIVE","url":"https://textures.minecraft.net/texture/cdefab","alias":"Migrator"}]}))).mount(&server).await;
  Mock::given(path("/texture/abcdef")).respond_with(ResponseTemplate::new(200).set_body_bytes(png())).mount(&server).await;
 });
    let service = Arc::new(service);
    let accounts = service.clone();
    let app = boot_core(cx, dir, AppSettings::default(), move |rt, data| {
        testing::core_with_accounts(rt, data, Arc::new(FakeCatalog::new()), accounts)
    });
    drop(rt);
    activate(&app, cx);
    click(&app, cx, "rail-appearance");
    settle(cx, |cx| {
        cx.update(|cx| {
            app.root
                .read(cx)
                .appearance()
                .read(cx)
                .model()
                .skin()
                .is_some()
        })
    });
    (app, server, service)
}
fn choose(app: &Booted, cx: &mut TestAppContext, dir: &Path) {
    let image = dir.join("chosen.png");
    std::fs::write(&image, png()).unwrap();
    click(app, cx, "appearance-choose");
    cx.simulate_path_prompt_response(move |_| Some(vec![image]));
    settle(cx, |cx| {
        cx.update(|cx| {
            app.root
                .read(cx)
                .appearance()
                .read(cx)
                .model()
                .has_unapplied_changes()
        })
    });
}
#[gpui_kit::test]
fn appearance_empty_has_login_not_offline_upload(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let app = boot(cx, tmp.path(), AppSettings::default());
    activate(&app, cx);
    click(&app, cx, "rail-appearance");
    settle(cx, |cx| shows(&app, cx, "appearance-login"));
    assert!(!shows(&app, cx, "appearance-choose"));
}
#[gpui_kit::test]
fn appearance_dirty_all_navigation_and_close_are_guarded_before_commit(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _server, _service) = fixture(cx, tmp.path());
    choose(&app, cx, tmp.path());
    click(&app, cx, "rail-settings");
    assert_eq!(nav(&app, cx), Nav::Appearance);
    assert!(shows(&app, cx, "appearance-leave-stay"));
    press(&app, cx, "escape");
    assert_eq!(nav(&app, cx), Nav::Appearance);
    click(&app, cx, "title-downloads");
    assert_eq!(nav(&app, cx), Nav::Appearance);
    click(&app, cx, "appearance-leave-stay");
    cx.update_window(app.handle, |_, window, cx| {
        window.dispatch_action(Box::new(patina_app::shell::CloseWindow), cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(shows(&app, cx, "appearance-leave-discard"));
    click(&app, cx, "appearance-leave-stay");
    cx.update_window(app.handle, |_, window, cx| {
        window.dispatch_action(
            Box::new(patina_app::shell::OpenDownloads {
                target: None,
                query: Some("fixture query".into()),
            }),
            cx,
        )
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(nav(&app, cx), Nav::Appearance);
    assert_eq!(
        cx.update(|cx| app
            .root
            .read(cx)
            .downloads()
            .read(cx)
            .model()
            .read(cx)
            .query()
            .text),
        ""
    );
    click(&app, cx, "appearance-leave-discard");
    assert_eq!(nav(&app, cx), Nav::Downloads);
    assert_eq!(
        cx.update(|cx| app
            .root
            .read(cx)
            .downloads()
            .read(cx)
            .model()
            .read(cx)
            .query()
            .text),
        "fixture query"
    );
}
#[gpui_kit::test]
fn appearance_preview_controls_clean_and_shared_edges_geometry(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _server, _service) = fixture(cx, tmp.path());
    click(&app, cx, "appearance-back");
    click(&app, cx, "appearance-outer");
    assert!(!cx.update(|cx| {
        app.root
            .read(cx)
            .appearance()
            .read(cx)
            .model()
            .has_unapplied_changes()
    }));
    let bounds = |id, cx: &mut TestAppContext| element(&app, cx, id).unwrap().bounds();
    let header = bounds("appearance-header", cx);
    let body = bounds("appearance-body", cx);
    let footer = bounds("appearance-footer", cx);
    assert_eq!(header.left(), body.left());
    assert_eq!(body.left(), footer.left());
    assert_eq!(header.right(), body.right());
    assert_eq!(body.right(), footer.right());
    let before = bounds("appearance-preview", cx);
    cx.update_window(app.handle, |_, window, cx| {
        window.within("appearance-model").click(1usize, cx);
    })
    .unwrap();
    let after = bounds("appearance-preview", cx);
    assert_eq!(before, after);
}
fn nav(app: &Booted, cx: &mut TestAppContext) -> Nav {
    cx.update(|cx| app.root.read(cx).shell().read(cx).nav())
}

#[gpui_kit::test]
fn appearance_file_bytes_are_fixed_and_active_apply_then_unknown_has_explicit_exit(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, server, _service) = fixture(cx, tmp.path());
    choose(&app, cx, tmp.path());
    let original = png();
    std::fs::write(tmp.path().join("chosen.png"), b"replaced on disk").unwrap();
    assert_eq!(
        cx.update(|cx| app
            .root
            .read(cx)
            .appearance()
            .read(cx)
            .model()
            .draft_skin()
            .unwrap()
            .bytes()
            .to_vec()),
        original
    );
    app._rt.block_on(async {
        Mock::given(path("/6/skins"))
            .respond_with(ResponseTemplate::new(502).set_delay(Duration::from_millis(350)))
            .expect(1)
            .mount(&server)
            .await;
    });
    click(&app, cx, "appearance-apply");
    assert!(
        element(&app, cx, "appearance-choose")
            .unwrap()
            .disabled()
            .unwrap()
    );
    click(&app, cx, "rail-settings");
    assert_eq!(nav(&app, cx), Nav::Appearance);
    assert!(!shows(&app, cx, "appearance-leave-discard"));
    click(&app, cx, "appearance-leave-stay");
    settle(cx, |cx| {
        cx.update(|cx| {
            app.root.read(cx).appearance().read(cx).model().state()
                == patina_app::appearance::AppearanceState::Uncertain
        })
    });
    assert!(
        element(&app, cx, "appearance-apply")
            .unwrap()
            .disabled()
            .unwrap()
    );
    click(&app, cx, "rail-settings");
    assert!(shows(&app, cx, "appearance-leave-discard"));
    click(&app, cx, "appearance-leave-discard");
    assert_eq!(nav(&app, cx), Nav::Settings);
}
#[gpui_kit::test]
fn appearance_dirty_account_switch_preserves_selector_until_confirmed(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _server, service) = fixture(cx, tmp.path());
    let second = app
        ._rt
        .block_on(seed_verified(
            &service,
            "Alex",
            "1123456789abcdef0123456789abcdef".parse().unwrap(),
            "FAKE2",
            1_800_000_000,
            1_800_003_600,
        ))
        .unwrap();
    app._rt.block_on(service.set_current(ID)).unwrap();
    settle(cx, |cx| {
        cx.update(|cx| {
            app.root
                .read(cx)
                .appearance()
                .read(cx)
                .model()
                .accounts()
                .len()
                == 2
        })
    });
    choose(&app, cx, tmp.path());
    click(&app, cx, "appearance-account");
    press(&app, cx, "down");
    press(&app, cx, "enter");
    settle(cx, |cx| shows(&app, cx, "appearance-leave-stay"));
    assert_eq!(
        cx.update(|cx| app
            .root
            .read(cx)
            .appearance()
            .read(cx)
            .model()
            .selected_id()
            .map(str::to_owned)),
        Some(ID.into())
    );
    press(&app, cx, "escape");
    click(&app, cx, "appearance-account");
    press(&app, cx, "down");
    press(&app, cx, "enter");
    settle(cx, |cx| shows(&app, cx, "appearance-leave-discard"));
    click(&app, cx, "appearance-leave-discard");
    assert_eq!(
        cx.update(|cx| app
            .root
            .read(cx)
            .appearance()
            .read(cx)
            .model()
            .selected_id()
            .map(str::to_owned)),
        Some(second.id)
    );
    assert_eq!(
        app._rt
            .block_on(service.snapshot())
            .unwrap()
            .current()
            .unwrap()
            .id,
        ID
    );
}
#[gpui_kit::test]
fn appearance_geometry_themes_narrow_zoom_and_idle_preview_jobs_are_stable(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _server, _service) = fixture(cx, tmp.path());
    for light in [false, true] {
        for scale in [1., 1.1, 1.25] {
            cx.update_window(app.handle, |_, window, cx| {
                patina_app::scale::apply_scale(scale, cx);
                gpui_kit::component::Theme::change(
                    if light {
                        gpui_kit::component::ThemeMode::Light
                    } else {
                        gpui_kit::component::ThemeMode::Dark
                    },
                    Some(window),
                    cx,
                );
            })
            .unwrap();
            cx.simulate_window_resize(
                app.handle,
                gpui_kit::size(px(1056. * scale), px(660. * scale)),
            );
            cx.run_until_parked();
            let a = element(&app, cx, "appearance-header").unwrap().bounds();
            let b = element(&app, cx, "appearance-body").unwrap().bounds();
            let f = element(&app, cx, "appearance-footer").unwrap().bounds();
            assert_eq!(a.left(), b.left());
            assert_eq!(b.left(), f.left());
            assert_eq!(a.right(), f.right());
            assert!(b.bottom() <= f.top());
            assert!(inside_window(&app, cx, "appearance-apply"));
        }
    }
    settle(cx, |cx| shows(&app, cx, "appearance-raster"));
    let jobs = cx.update(|cx| {
        app.root
            .read(cx)
            .appearance()
            .read(cx)
            .preview_jobs_started()
    });
    for _ in 0..10 {
        cx.update_window(app.handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
    }
    assert_eq!(
        jobs,
        cx.update(|cx| app
            .root
            .read(cx)
            .appearance()
            .read(cx)
            .preview_jobs_started())
    );
}

#[gpui_kit::test]
fn appearance_expired_session_offers_existing_sign_in_flow(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, server, _service) = fixture(cx, tmp.path());
    app._rt.block_on(async {
        server.reset().await;
        Mock::given(path("/6"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
    });
    click(&app, cx, "rail-settings");
    click(&app, cx, "rail-appearance");
    settle(cx, |cx| shows(&app, cx, "appearance-sign-in"));
    click(&app, cx, "appearance-sign-in");
    assert!(
        shows(&app, cx, "login-dialog")
            || shows(&app, cx, "login-cancel")
            || shows(&app, cx, "login-title")
    );
}

#[gpui_kit::test]
fn appearance_many_capes_and_long_filename_keep_footer_and_columns_inside_window(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, server, _service) = fixture(cx, tmp.path());
    let capes:Vec<_>=(0..128).map(|ix|serde_json::json!({"id":format!("cape-{ix}"),"state":"INACTIVE","url":"https://textures.minecraft.net/texture/cdefab","alias":format!("Cape {ix} with a long owned name for narrow windows")})).collect();
    app._rt.block_on(async {
        server.reset().await;
        Mock::given(path("/6"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id":ID,"skins":[],"capes":capes})),
            )
            .mount(&server)
            .await;
    });
    click(&app, cx, "rail-settings");
    click(&app, cx, "rail-appearance");
    settle(cx, |cx| shows(&app, cx, "appearance-choose"));
    let name = format!("{}.png", "long-skin-name-".repeat(12));
    let image = tmp.path().join(&name);
    std::fs::write(&image, png()).unwrap();
    click(&app, cx, "appearance-choose");
    cx.simulate_path_prompt_response(move |_| Some(vec![image]));
    settle(cx, |cx| {
        cx.update(|cx| {
            app.root
                .read(cx)
                .appearance()
                .read(cx)
                .model()
                .has_unapplied_changes()
        })
    });
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(1056.), px(660.)));
    assert!(inside_window(&app, cx, "appearance-apply"));
    assert!(inside_window(&app, cx, "appearance-filename"));
    let footer = element(&app, cx, "appearance-footer").unwrap().bounds();
    let filename = element(&app, cx, "appearance-filename").unwrap().bounds();
    assert!(filename.right() <= footer.right());
    cx.update_window(app.handle, |_, window, cx| {
        window.scroll(
            "appearance-capes",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-100000.))),
            cx,
        )
    })
    .unwrap();
    assert!(inside_window(&app, cx, "appearance-apply"));
    assert!(shows(&app, cx, "appearance-cape-cape-127"));
}
#[gpui_kit::test]
fn appearance_instance_navigation_payload_waits_for_discard(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let first = store.create("FirstGuard", "1.21.1").unwrap();
    let second = store.create("SecondGuard", "1.21.1").unwrap();
    let (app, _server, _service) = fixture(cx, tmp.path());
    let model = cx.update(|cx| app.root.read(cx).instances().read(cx).model().clone());
    model.update(cx, |model, cx| model.select(&first.id, cx));
    choose(&app, cx, tmp.path());
    cx.update_window(app.handle, |_, window, cx| {
        window.dispatch_action(
            Box::new(patina_app::shell::OpenInstance::new(second.id.clone())),
            cx,
        )
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(nav(&app, cx), Nav::Appearance);
    assert_eq!(
        model.read_with(cx, |m, _| m.selected().map(str::to_owned)),
        Some(first.id.clone())
    );
    click(&app, cx, "appearance-leave-discard");
    assert_eq!(nav(&app, cx), Nav::Instances);
    assert_eq!(
        model.read_with(cx, |m, _| m.selected().map(str::to_owned)),
        Some(second.id.clone())
    );
}

fn cape_png() -> Vec<u8> {
    let image = RgbaImage::from_fn(64, 32, |x, y| {
        if (1..11).contains(&x) && (1..17).contains(&y) {
            Rgba([180, (x * 17) as u8, (y * 13) as u8, 255])
        } else {
            Rgba([0, 0, 0, 0])
        }
    });
    let mut bytes = Cursor::new(vec![]);
    DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn owned_capes_fixture(
    app: &Booted,
    cx: &mut TestAppContext,
    server: &MockServer,
    choices: Vec<(String, String)>,
) {
    use wiremock::matchers::method;
    let active = Arc::new(std::sync::Mutex::new(
        choices.first().map(|(id, _)| id.clone()),
    ));
    app._rt.block_on(async {
        server.reset().await;
        let current = active.clone();
        Mock::given(path("/6")).and(method("GET")).respond_with(move |_: &wiremock::Request| {
            let active = current.lock().unwrap();
            let capes: Vec<_> = choices.iter().map(|(id, name)| serde_json::json!({"id":id,"alias":name,"state":if active.as_ref()==Some(id){"ACTIVE"}else{"INACTIVE"},"url":"https://textures.minecraft.net/texture/cdefab"})).collect();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":ID,"skins":[{"state":"ACTIVE","url":"https://textures.minecraft.net/texture/abcdef","variant":"CLASSIC"}],"capes":capes}))
        }).mount(server).await;
        Mock::given(path("/texture/abcdef")).respond_with(ResponseTemplate::new(200).set_body_bytes(png())).mount(server).await;
        Mock::given(path("/texture/cdefab")).respond_with(ResponseTemplate::new(200).set_body_bytes(cape_png())).mount(server).await;
        let current = active.clone();
        Mock::given(path("/6/capes/active")).and(method("PUT")).respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            *current.lock().unwrap()=Some(body["capeId"].as_str().unwrap().to_owned());
            ResponseTemplate::new(200)
        }).mount(server).await;
        Mock::given(path("/6/capes/active")).and(method("DELETE")).respond_with(move |_: &wiremock::Request| {
            *active.lock().unwrap()=None;
            ResponseTemplate::new(204)
        }).mount(server).await;
    });
    click(app, cx, "rail-settings");
    click(app, cx, "rail-appearance");
    settle(cx, |cx| shows(app, cx, "appearance-choose"));
}

fn five_cape_choices() -> Vec<(String, String)> {
    [
        ("copper", "Copper"),
        ("purple-heart", "Purple Heart"),
        ("home", "Home"),
        ("pan", "Pan"),
    ]
    .into_iter()
    .map(|(id, name)| (id.into(), name.into()))
    .collect()
}

#[gpui_kit::test]
fn appearance_cape_scroll_viewport_stays_above_independent_footer(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, server, _service) = fixture(cx, tmp.path());
    owned_capes_fixture(&app, cx, &server, five_cape_choices());
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    cx.run_until_parked();
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.viewport_size(), gpui_kit::size(px(960.), px(600.)));
        let viewport = window.find("appearance-capes").bounds();
        let body = window.find("appearance-body").bounds();
        let footer = window.find("appearance-footer").bounds();
        let pan = window.find("appearance-cape-pan");
        eprintln!(
            "cape viewport={viewport:?}; body={body:?}; footer={footer:?}; Pan={:?}, visible={}",
            pan.bounds(),
            pan.visible()
        );
        assert!(
            viewport.bottom() <= body.bottom(),
            "cape scroll viewport must fit the body's remaining height"
        );
        assert!(
            viewport.bottom() <= footer.top(),
            "cape viewport cannot paint across the footer separator"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn appearance_cape_tiles_wrap_with_uniform_thumbnail_name_slots(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, server, _service) = fixture(cx, tmp.path());
    owned_capes_fixture(&app, cx, &server, five_cape_choices());
    for (width, height) in [(960., 600.), (1280., 800.)] {
        cx.simulate_window_resize(app.handle, gpui_kit::size(px(width), px(height)));
        cx.update_window(app.handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.viewport_size(),
                gpui_kit::size(px(width), px(height))
            );
            let none = window.find("appearance-cape-none").bounds();
            let copper = window.find("appearance-cape-copper").bounds();
            let purple = window.find("appearance-cape-purple-heart");
            let pan = window.find("appearance-cape-pan").bounds();
            assert_eq!(
                none.size, copper.size,
                "None reserves the same tile as an owned cape"
            );
            assert_eq!(
                none.top(),
                copper.top(),
                "choices share a row when width allows"
            );
            assert!(
                copper.left() > none.left(),
                "cape choices are tiles, not full-width list rows"
            );
            assert_eq!(purple.label(), Some("Purple Heart"));
            let thumbnail = window
                .find("appearance-cape-purple-heart-thumbnail")
                .bounds();
            let name = window.find("appearance-cape-purple-heart-name").bounds();
            assert!(
                thumbnail.bottom() <= name.top(),
                "thumbnail must be above the readable name"
            );
            assert_eq!(none.size, pan.size);
            if width == 960. {
                assert!(
                    pan.top() > none.top(),
                    "narrow viewport wraps the fifth tile"
                );
            } else {
                assert_eq!(
                    pan.top(),
                    none.top(),
                    "wide viewport uses available width, not a fixed column count"
                );
            }
        })
        .unwrap();
    }
}

#[gpui_kit::test]
fn appearance_cape_grid_clips_scrolls_and_preserves_explicit_apply(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, server, service) = fixture(cx, tmp.path());
    let choices = (0..48)
        .map(|ix| (format!("cape-{ix}"), format!("Synthetic cape {ix}")))
        .collect();
    owned_capes_fixture(&app, cx, &server, choices);
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let viewport = window.find("appearance-capes").bounds();
        let footer = window.find("appearance-footer").bounds();
        assert!(viewport.bottom() < footer.top());
        assert!(
            !window.find("appearance-cape-cape-47").visible(),
            "offscreen bottom choices are clipped, not painted through the footer"
        );
        let mut checked_masks = 0;
        for ix in 0..48 {
            let tile = window
                .find(format!("appearance-cape-cape-{ix}"))
                .bounds()
                .scale(window.scale_factor());
            for quad in window
                .painted_quads()
                .iter()
                .filter(|quad| quad.bounds == tile)
            {
                assert!(
                    quad.content_mask.bounds.bottom()
                        <= viewport.scale(window.scale_factor()).bottom(),
                    "cape paint mask must end at its own viewport"
                );
                checked_masks += 1;
            }
        }
        assert!(
            checked_masks > 0,
            "test must inspect actual painted cape backgrounds"
        );
    })
    .unwrap();
    for _ in 0..40 {
        if element(&app, cx, "appearance-cape-cape-1")
            .unwrap()
            .focused()
            == Some(true)
        {
            break;
        }
        press(&app, cx, "tab");
    }
    assert_eq!(
        element(&app, cx, "appearance-cape-cape-1")
            .unwrap()
            .focused(),
        Some(true)
    );
    press(&app, cx, "enter");
    assert!(cx.update(|cx| {
        app.root
            .read(cx)
            .appearance()
            .read(cx)
            .model()
            .has_unapplied_changes()
    }));
    assert!(
        app._rt
            .block_on(server.received_requests())
            .unwrap()
            .iter()
            .all(|r| r.method == "GET"),
        "selection does not write"
    );
    click(&app, cx, "appearance-discard");
    assert_eq!(
        cx.update(|cx| app
            .root
            .read(cx)
            .appearance()
            .read(cx)
            .model()
            .selected_cape()
            .unwrap()
            .as_str()
            .to_owned()),
        "cape-0"
    );
    let footer = element(&app, cx, "appearance-footer").unwrap().bounds();
    for _ in 0..60 {
        if element(&app, cx, "appearance-cape-cape-47")
            .unwrap()
            .focused()
            == Some(true)
        {
            break;
        }
        press(&app, cx, "tab");
    }
    cx.update_window(app.handle, |_, window, cx| {
        window.render_frame(cx);
        let last = window.find("appearance-cape-cape-47");
        let viewport = window.find("appearance-capes").bounds();
        assert_eq!(last.focused(), Some(true));
        assert!(
            last.visible()
                && last.bounds().left() >= viewport.left()
                && last.bounds().right() <= viewport.right()
                && last.bounds().top() >= viewport.top()
                && last.bounds().bottom() <= viewport.bottom(),
            "keyboard focus must reveal the complete bottom tile, not an invisible tab stop"
        );
        let focus_inset = window.rem_size() * 0.25;
        assert!(
            last.bounds().top() >= viewport.top() + focus_inset
                && last.bounds().bottom() <= viewport.bottom() - focus_inset,
            "focused tile must retain viewport clearance for Kit's focus ring"
        );
        window.scroll(
            "appearance-capes",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(100000.))),
            cx,
        );
    })
    .unwrap();
    cx.update_window(app.handle, |_, window, cx| {
        window.scroll(
            "appearance-capes",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-100000.))),
            cx,
        );
        let viewport = window.find("appearance-capes").bounds();
        let last = window.find("appearance-cape-cape-47");
        assert!(last.visible());
        assert!(
            last.bounds().left() >= viewport.left()
                && last.bounds().right() <= viewport.right()
                && last.bounds().top() >= viewport.top()
                && last.bounds().bottom() <= viewport.bottom(),
            "bottom item is fully reachable inside the viewport"
        );
        assert!(!window.find("appearance-cape-cape-0").visible());
        assert_eq!(
            window.find("appearance-footer").bounds(),
            footer,
            "scroll cannot move the footer"
        );
        window.click("appearance-cape-cape-47", cx);
    })
    .unwrap();
    click(&app, cx, "appearance-apply");
    settle(cx, |cx| {
        cx.update(|cx| {
            app.root.read(cx).appearance().read(cx).model().state()
                == patina_app::appearance::AppearanceState::Ready
                && !app
                    .root
                    .read(cx)
                    .appearance()
                    .read(cx)
                    .model()
                    .has_unapplied_changes()
        })
    });
    let requests = app._rt.block_on(server.received_requests()).unwrap();
    let writes: Vec<_> = requests.iter().filter(|r| r.method != "GET").collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, "PUT");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&writes[0].body).unwrap(),
        serde_json::json!({"capeId":"cape-47"})
    );
    assert_eq!(
        app._rt
            .block_on(service.snapshot())
            .unwrap()
            .current()
            .unwrap()
            .id,
        ID
    );
    cx.update_window(app.handle, |_, window, cx| {
        window.scroll(
            "appearance-capes",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(100000.))),
            cx,
        )
    })
    .unwrap();
    click(&app, cx, "appearance-cape-none");
    click(&app, cx, "appearance-apply");
    settle(cx, |cx| {
        cx.update(|cx| {
            app.root.read(cx).appearance().read(cx).model().state()
                == patina_app::appearance::AppearanceState::Ready
                && !app
                    .root
                    .read(cx)
                    .appearance()
                    .read(cx)
                    .model()
                    .has_unapplied_changes()
        })
    });
    assert!(cx.update(|cx| {
        app.root
            .read(cx)
            .appearance()
            .read(cx)
            .model()
            .selected_cape()
            .is_none()
    }));
    let requests = app._rt.block_on(server.received_requests()).unwrap();
    let methods: Vec<_> = requests
        .iter()
        .filter(|r| r.method != "GET")
        .map(|r| r.method.to_string())
        .collect();
    assert_eq!(
        methods,
        ["PUT", "DELETE"],
        "None disables only the cape; no skin upload"
    );
}

#[gpui_kit::test]
fn appearance_cape_grid_masks_partial_tiles_at_each_minimum_zoom_theme_locale(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, server, _service) = fixture(cx, tmp.path());
    owned_capes_fixture(&app, cx, &server, five_cape_choices());
    for language in [Language::En, Language::ZhCn] {
        for light in [false, true] {
            for scale in [1., 1.1, 1.25, 1.5] {
                app.language(language);
                cx.update_window(app.handle, |_, window, cx| {
                    patina_app::scale::apply_scale(scale, cx);
                    Theme::change(
                        if light {
                            gpui_kit::component::ThemeMode::Light
                        } else {
                            gpui_kit::component::ThemeMode::Dark
                        },
                        Some(window),
                        cx,
                    );
                })
                .unwrap();
                let size = gpui_kit::size(px(960. * scale), px(600. * scale));
                cx.simulate_window_resize(app.handle, size);
                cx.update_window(app.handle,|_,window,cx| {
            window.render_frame(cx);assert_eq!(window.viewport_size(),size);
            window.scroll("appearance-capes",gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.),px(100000.))),cx);
            let footer=window.find("appearance-footer").bounds();
            let viewport=window.find("appearance-capes").bounds();
            assert!(viewport.bottom()<=window.find("appearance-body").bounds().bottom());
            assert!(viewport.bottom()<footer.top());
            assert!(!window.find("appearance-cape-pan").visible(),"fifth tile starts beyond this constrained viewport");
            window.scroll("appearance-capes",gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.),px(-64.*scale))),cx);
            let mut partial_masks=0;
            for id in ["appearance-cape-none","appearance-cape-copper","appearance-cape-purple-heart","appearance-cape-home","appearance-cape-pan"] {
                let tile=window.find(id);
                let bounds=tile.bounds();
                if tile.visible()&&(bounds.top()<viewport.top()||bounds.bottom()>viewport.bottom()) {
                    for quad in window.painted_quads().iter().filter(|q|q.bounds==bounds.scale(window.scale_factor())) {
                        assert!(quad.content_mask.bounds.top()>=viewport.scale(window.scale_factor()).top());
                        assert!(quad.content_mask.bounds.bottom()<=viewport.scale(window.scale_factor()).bottom());
                        partial_masks+=1;
                    }
                }
            }
            assert!(partial_masks>0,"must inspect actual partially clipped cape paint, not just container bounds");
            window.scroll("appearance-capes",gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.),px(-100000.))),cx);
            let pan=window.find("appearance-cape-pan");
            assert!(pan.visible()&&pan.bounds().top()>=viewport.top()&&pan.bounds().bottom()<=viewport.bottom());
            assert_eq!(window.find("appearance-footer").bounds(),footer);
            assert!(window.find("appearance-apply").visible());
        }).unwrap();
            }
        }
    }
}

#[gpui_kit::test]
fn appearance_cape_keyboard_reveal_keeps_ring_inset_after_zoom(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, server, _service) = fixture(cx, tmp.path());
    owned_capes_fixture(
        &app,
        cx,
        &server,
        (0..48)
            .map(|ix| (format!("cape-{ix}"), format!("Synthetic cape {ix}")))
            .collect(),
    );
    // Reuse existing keyed focus scopes as rem changes, in both directions.
    for scale in [1.5, 1., 1.1, 1.25] {
        for (language, mode) in [
            (Language::En, gpui_kit::component::ThemeMode::Dark),
            (Language::ZhCn, gpui_kit::component::ThemeMode::Light),
        ] {
            app.language(language);
            cx.update_window(app.handle, |_, window, cx| {
                patina_app::scale::apply_scale(scale, cx);
                Theme::change(mode, Some(window), cx);
            })
            .unwrap();
            cx.simulate_window_resize(
                app.handle,
                gpui_kit::size(px(960. * scale), px(600. * scale)),
            );
            for (target, key) in [
                ("appearance-cape-cape-47", "tab"),
                ("appearance-cape-none", "shift-tab"),
            ] {
                for _ in 0..100 {
                    if element(&app, cx, target).unwrap().focused() == Some(true) {
                        break;
                    }
                    press(&app, cx, key);
                }
                cx.update_window(app.handle, |_, window, cx| {
                    window.render_frame(cx);
                    let tile = window.find(target);
                    let viewport = window.find("appearance-capes").bounds();
                    // Taffy snaps padding and absolute edges separately. Allow
                    // one device pixel, but never less than Kit's 3px outer ring.
                    let inset = (window.pixel_snap(window.rem_size() * 0.25)
                        - px(1.) / window.scale_factor()).max(px(3.));
                    assert_eq!(tile.focused(), Some(true));
                    assert!(tile.visible());
                    assert!(
                        tile.bounds().top() >= viewport.top() + inset
                            && tile.bounds().bottom() <= viewport.bottom() - inset,
                        "{target} at scale {scale}: {:?} must retain ring inset within {viewport:?}",
                        tile.bounds()
                    );
                    assert!(tile.bounds().left() >= viewport.left() + inset);
                    assert!(tile.bounds().right() <= viewport.right() - inset);
                    assert!(window.find("appearance-apply").visible());
                })
                .unwrap();
            }
        }
    }
    assert!(
        app._rt
            .block_on(server.received_requests())
            .unwrap()
            .iter()
            .all(|r| r.method == "GET")
    );
}

#[gpui_kit::test]
fn appearance_focused_cape_stays_revealed_when_zoom_changes(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let (app, server, _service) = fixture(cx, tmp.path());
    owned_capes_fixture(&app, cx, &server, five_cape_choices());
    cx.simulate_window_resize(app.handle, gpui_kit::size(px(960.), px(600.)));
    for _ in 0..40 {
        if element(&app, cx, "appearance-cape-pan").unwrap().focused() == Some(true) {
            break;
        }
        press(&app, cx, "tab");
    }
    for scale in [1., 1.5, 1.1, 1.25, 1.] {
        cx.update_window(app.handle, |_, _, cx| {
            patina_app::scale::apply_scale(scale, cx)
        })
        .unwrap();
        cx.simulate_window_resize(
            app.handle,
            gpui_kit::size(px(960. * scale), px(600. * scale)),
        );
        cx.update_window(app.handle, |_, window, cx| {
            window.render_frame(cx);
            let tile = window.find("appearance-cape-pan");
            let viewport = window.find("appearance-capes").bounds();
            // Taffy snaps padding and absolute edges separately. Allow
            // one device pixel, but never less than Kit's 3px outer ring.
            let inset = (window.pixel_snap(window.rem_size() * 0.25)
                - px(1.) / window.scale_factor())
            .max(px(3.));
            assert_eq!(tile.focused(), Some(true));
            assert!(
                tile.visible()
                    && tile.bounds().top() >= viewport.top() + inset
                    && tile.bounds().bottom() <= viewport.bottom() - inset,
                "focused Pan at zoom {scale}: {:?}, viewport {viewport:?}",
                tile.bounds()
            );
        })
        .unwrap();
    }
}
