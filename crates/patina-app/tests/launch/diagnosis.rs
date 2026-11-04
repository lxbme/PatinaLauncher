//! Synthetic failures, never real resource exhaustion or authenticated game claims.
use super::*;
use patina_core::launch::diagnosis::DiagnosisKind;

#[gpui_kit::test]
fn diagnosis_safe_prepare_failure_and_accepted_retry(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Diagnosis fixture", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let app = boot_games(
        cx,
        tmp.path(),
        FakeGames::failing(|| {
            SessionError::Prepare(patina_core::prepare::PrepareError::Io {
                path: "/synthetic".into(),
                error: "Permission denied (os error 13)".into(),
            })
        }),
    );
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "launch");
    settle_game(cx, |cx| {
        model.read_with(cx, |m, cx| m.diagnosis(&a, cx).is_some())
    });
    assert_eq!(
        model.read_with(cx, |m, cx| m.diagnosis(&a, cx).map(|d| d.kind())),
        Some(DiagnosisKind::PermissionDenied)
    );
    assert!(shows(&app, cx, "instance-diagnosis"));
    assert!(!shows(&app, cx, "instance-op-error"));
    assert!(
        !shows(&app, cx, "diagnosis-view-log"),
        "no current log for prepare failure"
    );
    assert!(shows(&app, cx, "instance-op-retry"));
    assert!(shows(&app, cx, "instance-op-dismiss"));
    model.update(cx, |m, cx| {
        assert!(m.launch(&a, cx));
        assert!(
            m.diagnosis(&a, cx).is_none(),
            "clears synchronously before prepare/spawn"
        );
    });
    settle_game(cx, |cx| {
        model.read_with(cx, |m, cx| m.diagnosis(&a, cx).is_some())
    });
    model.update(cx, |m, cx| m.dismiss_op(&a, cx));
    assert!(model.read_with(cx, |m, cx| m.diagnosis(&a, cx).is_none()));
}

#[gpui_kit::test]
fn diagnosis_crashed_session_uses_own_saved_log(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Diagnosis fixture", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let app = boot_games(
        cx,
        tmp.path(),
        FakeGames::running(
            "echo 'Exception in thread \"main\" java.lang.OutOfMemoryError: Java heap space'; exit 3",
        ),
    );
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "launch");
    settle_game(cx, |cx| {
        model.read_with(cx, |m, cx| m.diagnosis(&a, cx).is_some())
    });
    assert_eq!(
        model.read_with(cx, |m, cx| m.diagnosis(&a, cx).map(|d| d.kind())),
        Some(DiagnosisKind::HeapExhausted)
    );
    assert!(shows(&app, cx, "instance-diagnosis"));
    assert!(
        !shows(&app, cx, "instance-crash"),
        "no duplicate failure card"
    );
    assert!(!shows(&app, cx, "diagnosis-evidence"));
    assert_eq!(
        app.root.read_with(cx, |root, cx| root
            .instances()
            .read(cx)
            .detail()
            .read(cx)
            .tab()),
        patina_app::instances::DetailTab::Overview
    );
    let trigger = element(&app, cx, "diagnosis-evidence-toggle").unwrap();
    let region = element(&app, cx, "instance-diagnosis").unwrap();
    assert!(
        trigger.bounds().size.width < region.bounds().size.width / 2.,
        "disclosure retains a compact leading-aligned target"
    );
    click(&app, cx, "diagnosis-evidence-toggle");
    assert!(shows(&app, cx, "diagnosis-evidence"));
    tab_to(&app, cx, "diagnosis-evidence-toggle");
    cx.update_window(app.handle, |_, window, cx| {
        window.press("space", cx);
    })
    .unwrap();
    assert!(!shows(&app, cx, "diagnosis-evidence"));
    click(&app, cx, "diagnosis-view-log");
    assert!(shows(&app, cx, "log-crash-banner"));
    let saved = model.read_with(cx, |m, cx| m.crash_log(&a, cx).unwrap().to_path_buf());
    assert!(
        saved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("-crash")
    );
    model.update(cx, |m, cx| m.dismiss_exit(&a, cx));
    assert!(model.read_with(cx, |m, cx| m.diagnosis(&a, cx).is_none()));
}

#[gpui_kit::test]
fn diagnosis_normal_exit_skips_analysis(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Diagnosis fixture", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let app = boot_games(
        cx,
        tmp.path(),
        FakeGames::running("echo 'java.lang.OutOfMemoryError: Java heap space'; exit 0"),
    );
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    model.update(cx, |m, cx| assert!(m.launch(&a, cx)));
    settle_game(cx, |cx| model.read_with(cx, |m, cx| !m.is_busy(&a, cx)));
    assert!(model.read_with(cx, |m, cx| m.diagnosis(&a, cx).is_none()));
}

#[gpui_kit::test]
fn diagnosis_preserves_focus_and_fits_themes_locales_and_scale(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Diagnosis fixture", "1.21.1").unwrap().id;
    let b = store.create("Other fixture", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let app = boot_games(
        cx,
        tmp.path(),
        FakeGames::running(&format!(
            "echo 'Exception in thread \"main\" java.nio.file.AccessDeniedException: /synthetic/{}'; exit 3",
            "long-path-".repeat(150)
        )),
    );
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    model.update(cx, |m, cx| m.select(&a, cx));
    press(&app, cx, "/");
    let before = cx
        .update_window(app.handle, |_, window, cx| window.focused(cx))
        .unwrap();
    model.update(cx, |m, cx| assert!(m.launch(&a, cx)));
    settle_game(cx, |cx| {
        model.read_with(cx, |m, cx| m.diagnosis(&a, cx).is_some())
    });
    assert_eq!(
        cx.update_window(app.handle, |_, window, cx| window.focused(cx))
            .unwrap(),
        before
    );
    assert_eq!(nav(&app, cx), Nav::Instances);
    press(&app, cx, "escape");
    for language in [Language::En, Language::ZhCn] {
        app.language(language);
        for preference in [
            settings::ThemePreference::Light,
            settings::ThemePreference::Dark,
        ] {
            cx.update_window(app.handle, |_, window, cx| {
                theme::set_theme(preference, window, cx);
                Theme::update(cx, |theme| theme.font_size = px(17.6));
            })
            .unwrap();
            cx.simulate_window_resize(app.handle, gpui_kit::size(px(1056.), px(660.)));
            for id in [
                "diagnosis-title",
                "diagnosis-explanation",
                "diagnosis-evidence-toggle",
                "diagnosis-view-log",
                "instance-crash-dismiss",
                "launch",
            ] {
                assert!(
                    inside_window(&app, cx, id),
                    "{id} {language:?} {preference:?}"
                );
            }
            click(&app, cx, "diagnosis-evidence-toggle");
            assert!(inside_window(&app, cx, "diagnosis-evidence"));
            assert!(inside_window(&app, cx, "instance-crash-dismiss"));
            click(&app, cx, "diagnosis-evidence-toggle");
        }
    }
    model.update(cx, |m, cx| m.select(&b, cx));
    assert!(!shows(&app, cx, "instance-diagnosis"));
    model.update(cx, |m, cx| m.select(&a, cx));
    assert!(!shows(&app, cx, "diagnosis-evidence"));
    assert!(shows(&app, cx, "diagnosis-view-log"));
}

#[gpui_kit::test]
fn diagnosis_mod_only_failure_keeps_generic_ui(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Unknown fixture", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let app = boot_games(
        cx,
        tmp.path(),
        FakeGames::running(
            "echo 'Exception in thread \"main\" org.spongepowered.asm.mixin.throwables.MixinApplyError: synthetic'; exit 3",
        ),
    );
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "launch");
    settle_game(cx, |cx| {
        matches!(run_state(&model, cx, &a), Some(RunState::Exited(_)))
    });
    assert!(model.read_with(cx, |m, cx| m.diagnosis(&a, cx).is_none()));
    assert!(shows(&app, cx, "instance-crash"));
    assert!(!shows(&app, cx, "instance-diagnosis"));
}

#[gpui_kit::test]
fn diagnosis_requested_stop_skips_analysis(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let a = store.create("Stop fixture", "1.21.1").unwrap().id;
    with_account(tmp.path());
    let app = boot_games(
        cx,
        tmp.path(),
        FakeGames::running("echo 'java.lang.OutOfMemoryError: Java heap space'; sleep 30 & wait"),
    );
    let games = StopGames::new(app._rt.handle());
    let model = instances_model(&app, cx);
    list_settled(&model, cx);
    click(&app, cx, "launch");
    games.add(wait_running(&model, cx, &a));
    model.update(cx, |m, cx| m.force_stop(&a, cx));
    settle_game(cx, |cx| run_state(&model, cx, &a).is_none());
    assert!(model.read_with(cx, |m, cx| m.diagnosis(&a, cx).is_none()));
    assert!(!shows(&app, cx, "instance-diagnosis"));
}
