#[allow(dead_code)]
#[path = "fixtures/modpacks/support.rs"]
mod support;
use patina_core::modpack::{PackInstallChoices, ensure_ready, load_state};
use support::*;
#[tokio::test]
async fn preview_has_no_instance_side_effects() {
    let s = Setup::new().await;
    let _p = pack("1", &[("config/a", b"author")]);
    assert!(s.store.list().unwrap().instances.is_empty());
}
#[tokio::test]
async fn install_exact_manifest_without_dependency_requests() {
    let s = Setup::new().await;
    s.serve("/data/a/versions/v/a.jar", b"exact").await;
    let p = manifest_pack(vec![file(
        "mods/a.jar",
        b"exact",
        "https://cdn.modrinth.com/data/a/versions/v/a.jar",
    )]);
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    let out = s
        .service
        .install(p, PackInstallChoices::new("Exact"), cancel, None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(out.instance().game_dir().join("mods/a.jar")).unwrap(),
        b"exact"
    );
    assert!(
        !out.instance()
            .game_dir()
            .join("mods/dependency.jar")
            .exists()
    );
    assert!(
        s.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| !r.url.path().contains("/project/") && !r.url.path().contains("/version/"))
    );
    assert!(ensure_ready(&out.instance().dir).is_ok());
}
#[tokio::test]
async fn install_applies_overrides_and_records_original_hashes() {
    let s = Setup::new().await;
    let (_tx, c) = tokio::sync::watch::channel(false);
    let out = s
        .service
        .install(
            pack("1", &[("config/a", b"author")]),
            PackInstallChoices::new("Pack"),
            c,
            None,
        )
        .await
        .unwrap();
    let state = load_state(&out.instance().dir).unwrap().unwrap();
    assert_eq!(state.version(), "1");
    assert_eq!(state.files().len(), 1);
    assert_eq!(
        std::fs::read(out.instance().game_dir().join("config/a")).unwrap(),
        b"author"
    );
    assert!(
        s.launcher
            .data
            .versions()
            .join("fixture/fixture.jar")
            .is_file()
    );
}
#[tokio::test]
async fn optional_client_choice_persists() {
    let s = Setup::new().await;
    let mut f = file("mods/o.jar", b"optional", "https://cdn.modrinth.com/o");
    f["env"] = serde_json::json!({"client":"optional","server":"required"});
    let (_tx, c) = tokio::sync::watch::channel(false);
    let out = s
        .service
        .install(
            manifest_pack(vec![f]),
            PackInstallChoices::new("Optional"),
            c,
            None,
        )
        .await
        .unwrap();
    assert!(!out.instance().game_dir().join("mods/o.jar").exists());
    assert_eq!(
        load_state(&out.instance().dir)
            .unwrap()
            .unwrap()
            .optional_choices()
            .get("mods/o.jar"),
        Some(&false)
    );
}
#[tokio::test]
async fn same_name_never_overwrites_instance() {
    let s = Setup::new().await;
    s.store.create("Pack", "fixture").unwrap();
    let (_tx, c) = tokio::sync::watch::channel(false);
    assert!(
        s.service
            .install(pack("1", &[]), PackInstallChoices::new("Pack"), c, None)
            .await
            .is_err()
    );
    assert_eq!(s.store.list().unwrap().instances.len(), 1);
}
#[tokio::test]
async fn cancel_retry_and_restart_keep_pending_honest() {
    let s = Setup::new().await;
    let (_tx, c) = tokio::sync::watch::channel(false);
    let p = manifest_pack(vec![file(
        "mods/f.jar",
        b"missing",
        "https://cdn.modrinth.com/f",
    )]);
    assert!(
        s.service
            .install(p.clone(), PackInstallChoices::new("Retry"), c.clone(), None)
            .await
            .is_err()
    );
    let i = s.store.load("Retry").unwrap();
    assert!(ensure_ready(&i.dir).is_err());
    s.serve("/f", b"missing").await;
    s.service
        .retry_install("Retry".into(), p, PackInstallChoices::new("Retry"), c, None)
        .await
        .unwrap();
    assert!(ensure_ready(&i.dir).is_ok());
    let (_tx, c) = tokio::sync::watch::channel(true);
    assert!(
        s.service
            .install(
                pack("1", &[]),
                PackInstallChoices::new("Cancelled"),
                c,
                None
            )
            .await
            .is_err()
    );
    assert!(s.store.load("Cancelled").is_err());
}
#[tokio::test]
async fn cleanup_preserves_shared_assets() {
    let s = Setup::new().await;
    std::fs::create_dir_all(s.launcher.data.assets()).unwrap();
    std::fs::write(s.launcher.data.assets().join("shared"), b"keep").unwrap();
    let i = s.store.create("Remove", "fixture").unwrap();
    s.store.delete(&i.id).unwrap();
    assert_eq!(
        std::fs::read(s.launcher.data.assets().join("shared")).unwrap(),
        b"keep"
    );
}
#[tokio::test]
async fn corrupt_pack_state_is_not_an_unmanaged_instance() {
    let s = Setup::new().await;
    let i = s.store.create("Corrupt", "fixture").unwrap();
    std::fs::write(i.dir.join("modpack-state.json"), b"broken").unwrap();
    assert!(load_state(&i.dir).is_err());
    assert!(ensure_ready(&i.dir).is_err());
}

#[tokio::test]
async fn retry_rejects_different_archive_even_when_name_and_version_match() {
    let s = Setup::new().await;
    let (_tx, c) = tokio::sync::watch::channel(false);
    let original = manifest_pack(vec![file(
        "mods/f.jar",
        b"missing",
        "https://cdn.modrinth.com/f",
    )]);
    assert!(
        s.service
            .install(original, PackInstallChoices::new("Retry"), c.clone(), None)
            .await
            .is_err()
    );
    let changed = manifest_pack(vec![]);
    assert!(
        s.service
            .retry_install(
                "Retry".into(),
                changed,
                PackInstallChoices::new("Retry"),
                c,
                None
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn manifest_with_two_files_from_same_project_is_not_rejected_by_provenance() {
    let s = Setup::new().await;
    s.serve("/data/a/versions/v/a.jar", b"one").await;
    s.serve("/data/a/versions/v2/a.jar", b"two").await;
    let p = manifest_pack(vec![
        file(
            "mods/one.jar",
            b"one",
            "https://cdn.modrinth.com/data/a/versions/v/a.jar",
        ),
        file(
            "mods/two.jar",
            b"two",
            "https://cdn.modrinth.com/data/a/versions/v2/a.jar",
        ),
    ]);
    let (_tx, c) = tokio::sync::watch::channel(false);
    let out = s
        .service
        .install(p, PackInstallChoices::new("Author composition"), c, None)
        .await
        .unwrap();
    assert!(out.instance().game_dir().join("mods/one.jar").is_file());
    assert!(out.instance().game_dir().join("mods/two.jar").is_file());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn pending_pack_is_blocked_in_actual_core_prepare_before_network_or_authentication() {
    let s = Setup::new().await;
    let i = s.store.create("Pending core", "fixture").unwrap();
    std::fs::write(i.dir.join("modpack-pending"), b"interrupted apply").unwrap();
    let account = patina_core::launch::test_account("Isolated test identity");
    let e = s
        .launcher
        .prepare(&i, &Default::default(), &account, None)
        .await
        .unwrap_err();
    assert!(matches!(
        e,
        patina_core::prepare::PrepareError::PackNotReady(_)
    ));
    assert!(s.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn pending_online_retry_accepts_exact_original_local_archive_without_losing_source() {
    let s = Setup::new().await;
    let p = online_pack(&s, "projectA", "v1").await;
    let mut global = patina_core::config::GlobalConfig::default();
    global.java.java_path = Some(s.tmp.path().join("no-java"));
    let failing = patina_core::modpack::PackService::new(s.launcher.clone(), global);
    let (_tx, c) = tokio::sync::watch::channel(false);
    assert!(
        failing
            .install(p, PackInstallChoices::new("Online retry"), c.clone(), None)
            .await
            .is_err()
    );
    let local = patina_core::modpack::inspect_bytes(
        online_archive("v1"),
        patina_core::modpack::ArchiveLimits::default(),
    )
    .unwrap();
    let out = s
        .service
        .retry_install(
            "Online retry".into(),
            local,
            PackInstallChoices::new("Online retry"),
            c,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        load_state(&out.instance().dir)
            .unwrap()
            .unwrap()
            .source()
            .unwrap()
            .project()
            .id(),
        "projectA"
    );
}

#[tokio::test]
async fn download_failure_is_not_reported_as_a_hash_mismatch() {
    let s = Setup::new().await;
    let p = manifest_pack(vec![file(
        "mods/missing.jar",
        b"missing",
        "https://cdn.modrinth.com/missing",
    )]);
    let (_tx, c) = tokio::sync::watch::channel(false);
    let error = s
        .service
        .install(p, PackInstallChoices::new("Network failure"), c, None)
        .await
        .unwrap_err();
    assert!(!matches!(
        error,
        patina_core::modpack::PackError::Verification
    ));
}

#[tokio::test]
async fn hidden_game_content_never_addresses_launcher_metadata_outside_minecraft() {
    let setup = Setup::new().await;
    let preview = pack(
        "1",
        &[
            ("config/euphoria_patcher/.data.json", b"author dotfile"),
            (".config/cache.dat", b"hidden directory"),
            ("instance.toml", b"game content, not launcher config"),
            ("modpack-state.json", b"game content, not launcher state"),
        ],
    );
    let (_sender, cancel) = tokio::sync::watch::channel(false);
    let outcome = setup
        .service
        .install(
            preview,
            PackInstallChoices::new("Hidden content"),
            cancel,
            None,
        )
        .await
        .unwrap();
    let instance = outcome.instance();
    assert_eq!(
        std::fs::read(
            instance
                .game_dir()
                .join("config/euphoria_patcher/.data.json")
        )
        .unwrap(),
        b"author dotfile"
    );
    assert_eq!(
        setup.store.load(&instance.id).unwrap().config.name,
        "Hidden content"
    );
    assert!(load_state(&instance.dir).unwrap().unwrap().is_ready());
    assert_eq!(
        std::fs::read(instance.game_dir().join("modpack-state.json")).unwrap(),
        b"game content, not launcher state"
    );
}

#[tokio::test]
async fn online_archive_preview_reports_real_download_bytes() {
    use patina_core::content::online::*;
    use sha1::{Digest, Sha1};
    let setup = Setup::new().await;
    let bytes = online_archive("progress");
    setup.serve("/progress.mrpack", &bytes).await;
    let version = ContentVersion::new(
        VersionRef::new(ProjectRef::new(Platform::Modrinth, "progress"), "progress"),
        "Progress pack",
        ReleaseChannel::Release,
    )
    .with_files(vec![ContentFile::new(
        "progress.mrpack",
        bytes.len() as u64,
        FileHashes::new(Some(hex::encode(Sha1::digest(&bytes))), None, None),
        Some("https://cdn.modrinth.com/progress.mrpack".into()),
        "https://modrinth.com/modpack/progress",
    )]);
    let (_sender, cancel) = tokio::sync::watch::channel(false);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    setup
        .service
        .preview_online(version, cancel, Some(tx))
        .await
        .unwrap();
    let mut ticks = vec![];
    while let Ok(tick) = rx.try_recv() {
        ticks.push(tick);
    }
    assert!(
        ticks
            .iter()
            .any(|p| p.unit == patina_core::progress::Unit::Bytes
                && p.done == bytes.len() as u64
                && p.total == bytes.len() as u64),
        "large archive download must report actual bytes, not remain a generic metadata spinner: {ticks:?}"
    );
}

#[tokio::test]
async fn manifest_size_hint_may_differ_when_both_author_hashes_match() {
    let setup = Setup::new().await;
    let body = b"official author bytes";
    setup.serve("/author.jar", body).await;
    let mut entry = file(
        "mods/author.jar",
        body,
        "https://cdn.modrinth.com/author.jar",
    );
    entry["fileSize"] = serde_json::json!(body.len() - 1);
    let (_sender, cancel) = tokio::sync::watch::channel(false);
    let outcome = setup
        .service
        .install(
            manifest_pack(vec![entry]),
            PackInstallChoices::new("Size hint"),
            cancel,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(outcome.instance().game_dir().join("mods/author.jar")).unwrap(),
        body
    );
}

#[tokio::test]
async fn online_project_logo_is_persisted_without_cropping_and_updates_keep_user_icon() {
    use std::io::Cursor;
    let s = Setup::new().await;
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(64, 32)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let bytes = png.into_inner();
    s.serve("/project-logo.png", &bytes).await;
    let p = online_pack(&s, "logo-project", "1")
        .await
        .with_project_icon(Some("https://cdn.modrinth.com/project-logo.png".into()));
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    let out = s
        .service
        .install(p, PackInstallChoices::new("Logo sample"), cancel, None)
        .await
        .unwrap();
    let i = s.store.load(&out.instance().id).unwrap();
    assert_eq!(
        std::fs::read(i.dir.join("icon.png")).unwrap(),
        bytes,
        "persist original official raster bytes without crop/repaint"
    );
    assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 64);
    assert!(i.config.icon.is_some());
    s.store.set_builtin_icon(&i.id, "sword").unwrap();
    let p = online_pack(&s, "logo-project", "2")
        .await
        .with_project_icon(Some("https://cdn.modrinth.com/new-logo.png".into()));
    let plan = s
        .service
        .plan_update(i.id.clone(), p, Default::default())
        .await
        .unwrap();
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    s.service
        .apply_update(plan, Default::default(), cancel, None)
        .await
        .unwrap();
    assert_eq!(
        s.store.load(&i.id).unwrap().config.icon,
        Some(patina_core::instance::InstanceIcon::Builtin("sword".into()))
    );
    assert!(
        s.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() != "/new-logo.png")
    );
}

#[tokio::test]
async fn bad_project_logo_is_nonfatal_and_local_pack_never_requests_it() {
    let s = Setup::new().await;
    s.serve("/bad-logo", b"not an image").await;
    let p = online_pack(&s, "logo-project", "1")
        .await
        .with_project_icon(Some("https://cdn.modrinth.com/bad-logo".into()));
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    let out = s
        .service
        .install(p, PackInstallChoices::new("Bad logo"), cancel, None)
        .await
        .unwrap();
    assert!(
        s.store
            .load(&out.instance().id)
            .unwrap()
            .config
            .icon
            .is_none()
    );
    assert!(
        patina_core::modpack::load_state(&out.instance().dir)
            .unwrap()
            .unwrap()
            .is_ready()
    );
    let before = s
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/bad-logo")
        .count();
    let p = pack("local", &[]).with_project_icon(Some("https://cdn.modrinth.com/bad-logo".into()));
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    s.service
        .install(
            p,
            PackInstallChoices::new("Local logo default"),
            cancel,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        s.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/bad-logo")
            .count(),
        before,
        "local packages must not guess or fetch an online logo"
    );
}
