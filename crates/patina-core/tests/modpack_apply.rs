#[allow(dead_code)]
#[path = "fixtures/modpacks/support.rs"]
mod support;
use patina_core::modpack::{PackDecision, PackDecisions, PackInstallChoices, ensure_ready};
use std::collections::BTreeMap;
use support::*;
async fn installed(s: &Setup) -> patina_core::instance::Instance {
    let (_tx, c) = tokio::sync::watch::channel(false);
    s.service
        .install(
            pack("1", &[("config/a", b"old")]),
            PackInstallChoices::new("Pack"),
            c,
            None,
        )
        .await
        .unwrap()
        .instance()
        .clone()
}
#[tokio::test]
async fn stale_disk_snapshot_rejects_before_mutation() {
    let s = Setup::new().await;
    let i = installed(&s).await;
    let p = s
        .service
        .plan_update(
            i.id.clone(),
            pack("2", &[("config/a", b"new")]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    std::fs::write(i.game_dir().join("config/a"), b"external").unwrap();
    let (_tx, c) = tokio::sync::watch::channel(false);
    assert!(
        s.service
            .apply_update(p, PackDecisions::default(), c, None)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(i.game_dir().join("config/a")).unwrap(),
        b"external"
    );
    assert!(ensure_ready(&i.dir).is_ok());
}
#[tokio::test]
async fn running_or_busy_instance_cannot_update() {
    let s = Setup::new().await;
    let i = installed(&s).await;
    let p = s
        .service
        .plan_update(i.id.clone(), pack("2", &[]), BTreeMap::new())
        .await
        .unwrap();
    let _guard = s.store.lock(&i.id).unwrap();
    let (_tx, c) = tokio::sync::watch::channel(false);
    assert!(
        s.service
            .apply_update(p, PackDecisions::default(), c, None)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(i.game_dir().join("config/a")).unwrap(),
        b"old"
    );
}
#[tokio::test]
async fn all_downloads_validate_before_original_changes() {
    let s = Setup::new().await;
    let i = installed(&s).await;
    s.serve("/bad", b"corrupt").await;
    let p = manifest_pack(vec![file(
        "mods/bad.jar",
        b"expected",
        "https://cdn.modrinth.com/bad",
    )]);
    assert!(
        s.service
            .plan_update(i.id.clone(), p, BTreeMap::new())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(i.game_dir().join("config/a")).unwrap(),
        b"old"
    );
    assert!(ensure_ready(&i.dir).is_ok());
}
#[tokio::test]
async fn saves_collision_never_silently_overwrites_or_recursively_deletes() {
    let s = Setup::new().await;
    let i = installed(&s).await;
    std::fs::create_dir_all(i.game_dir().join("saves/world")).unwrap();
    std::fs::write(i.game_dir().join("saves/world/level.dat"), b"player").unwrap();
    let p = s
        .service
        .plan_update(
            i.id.clone(),
            pack("2", &[("saves/world/level.dat", b"author")]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let mut d = PackDecisions::default();
    for change in p.changes() {
        d.set(change.id(), PackDecision::UsePack)
    }
    let (_tx, c) = tokio::sync::watch::channel(false);
    assert!(s.service.apply_update(p, d, c, None).await.is_err());
    assert_eq!(
        std::fs::read(i.game_dir().join("saves/world/level.dat")).unwrap(),
        b"player"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn symlink_swapped_after_preview_cannot_escape() {
    let s = Setup::new().await;
    let i = installed(&s).await;
    let p = s
        .service
        .plan_update(
            i.id.clone(),
            pack("2", &[("config/a", b"new")]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    std::fs::remove_dir_all(i.game_dir().join("config")).unwrap();
    let outside = s.tmp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("a"), b"safe").unwrap();
    std::os::unix::fs::symlink(&outside, i.game_dir().join("config")).unwrap();
    let (_tx, c) = tokio::sync::watch::channel(false);
    assert!(
        s.service
            .apply_update(p, PackDecisions::default(), c, None)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(outside.join("a")).unwrap(), b"safe");
}
#[tokio::test]
async fn restart_with_incomplete_apply_never_looks_ready() {
    let s = Setup::new().await;
    let i = installed(&s).await;
    std::fs::write(i.dir.join("modpack-pending"), b"applying").unwrap();
    assert!(ensure_ready(&i.dir).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn hidden_directory_symlink_still_cannot_write_outside_game_content() {
    let setup = Setup::new().await;
    let instance = installed(&setup).await;
    let outside = setup.tmp.path().join("hidden-outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("data.json"), b"outside bytes").unwrap();
    std::os::unix::fs::symlink(&outside, instance.game_dir().join(".config")).unwrap();
    assert!(
        setup
            .service
            .plan_update(
                instance.id,
                pack("2", &[(".config/data.json", b"replacement")]),
                BTreeMap::new()
            )
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(outside.join("data.json")).unwrap(),
        b"outside bytes"
    );
}
