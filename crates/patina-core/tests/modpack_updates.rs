#[allow(dead_code)]
#[path = "fixtures/modpacks/support.rs"]
mod support;
use patina_core::modpack::{
    PackChangeKind, PackDecision, PackDecisions, PackInstallChoices, load_state,
};
use std::collections::BTreeMap;
use support::*;
async fn install(
    s: &Setup,
    p: patina_core::modpack::PackPreview,
) -> patina_core::instance::Instance {
    let (_tx, c) = tokio::sync::watch::channel(false);
    s.service
        .install(p, PackInstallChoices::new("Pack"), c, None)
        .await
        .unwrap()
        .instance()
        .clone()
}
#[tokio::test]
async fn update_matrix_preserves_local_intent() {
    for (old, disk, new, want) in [
        (Some("a"), Some("a"), Some("b"), PackChangeKind::FollowPack),
        (
            Some("a"),
            Some("local"),
            Some("a"),
            PackChangeKind::Preserve,
        ),
        (
            Some("a"),
            Some("local"),
            Some("b"),
            PackChangeKind::Conflict,
        ),
        (Some("a"), Some("local"), None, PackChangeKind::Conflict),
        (Some("a"), None, Some("b"), PackChangeKind::Conflict),
        (None, Some("local"), Some("b"), PackChangeKind::Conflict),
        (None, Some("b"), Some("b"), PackChangeKind::Unchanged),
        (Some("a"), Some("b"), Some("b"), PackChangeKind::Unchanged),
        (Some("a"), Some("a"), None, PackChangeKind::FollowPack),
    ] {
        let s = Setup::new().await;
        let entries: Vec<_> = old
            .map(|v| ("config/a", v.as_bytes()))
            .into_iter()
            .collect();
        let i = install(&s, pack("1", &entries)).await;
        let path = i.game_dir().join("config/a");
        match disk {
            Some(b) => {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, b).unwrap();
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        std::fs::write(i.game_dir().join("user.txt"), b"keep").unwrap();
        let entries: Vec<_> = new
            .map(|v| ("config/a", v.as_bytes()))
            .into_iter()
            .collect();
        let p = s
            .service
            .plan_update(i.id.clone(), pack("2", &entries), BTreeMap::new())
            .await
            .unwrap();
        let change = p.changes().iter().find(|c| c.id() == "config/a").unwrap();
        assert_eq!(change.kind(), want, "old={old:?} disk={disk:?} new={new:?}");
        assert_eq!(
            change.default_decision(),
            if want == PackChangeKind::Conflict || want == PackChangeKind::Preserve {
                PackDecision::KeepLocal
            } else {
                PackDecision::UsePack
            }
        );
        assert!(p.preserved_paths().contains(&"user.txt".into()));
    }
}
#[tokio::test]
async fn second_update_does_not_promote_local_content_to_author_baseline() {
    let s = Setup::new().await;
    let i = install(
        &s,
        pack("1", &[("config/a", b"a"), ("config/deleted", b"a")]),
    )
    .await;
    std::fs::write(i.game_dir().join("config/a"), b"local").unwrap();
    std::fs::remove_file(i.game_dir().join("config/deleted")).unwrap();
    let (_tx, c) = tokio::sync::watch::channel(false);
    let p = s
        .service
        .plan_update(
            i.id.clone(),
            pack("2", &[("config/a", b"b"), ("config/deleted", b"b")]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    s.service
        .apply_update(p, PackDecisions::default(), c.clone(), None)
        .await
        .unwrap();
    let state = load_state(&i.dir).unwrap().unwrap();
    assert_ne!(
        state.files()["config/a"].sha512(),
        hex::encode(sha2::Sha512::digest(b"local"))
    );
    let p = s
        .service
        .plan_update(
            i.id.clone(),
            pack("3", &[("config/a", b"c"), ("config/deleted", b"c")]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(p.conflict_count(), 2);
    s.service
        .apply_update(p, PackDecisions::default(), c, None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(i.game_dir().join("config/a")).unwrap(),
        b"local"
    );
    assert!(!i.game_dir().join("config/deleted").exists());
}
use sha2::Digest;
#[tokio::test]
async fn tracked_single_mod_update_rename_never_duplicates_jar() {
    let s = Setup::new().await;
    s.serve("/data/a/versions/v/old.jar", b"old").await;
    let i = install(
        &s,
        manifest_pack(vec![file(
            "mods/old.jar",
            b"old",
            "https://cdn.modrinth.com/data/a/versions/v/old.jar",
        )]),
    )
    .await;
    std::fs::remove_file(i.game_dir().join("mods/old.jar")).unwrap();
    std::fs::write(i.game_dir().join("mods/user-new.jar"), b"user").unwrap();
    let source = std::fs::read_to_string(i.dir.join("content-sources.toml"))
        .unwrap()
        .replace("mods/old.jar", "mods/user-new.jar")
        .replace(
            &hex::encode(sha1::Sha1::digest(b"old")),
            &hex::encode(sha1::Sha1::digest(b"user")),
        );
    std::fs::write(i.dir.join("content-sources.toml"), source).unwrap();
    s.serve("/data/a/versions/v2/pack.jar", b"pack").await;
    let p = s
        .service
        .plan_update(
            i.id.clone(),
            manifest_pack(vec![file(
                "mods/pack.jar",
                b"pack",
                "https://cdn.modrinth.com/data/a/versions/v2/pack.jar",
            )]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(p.conflict_count(), 1);
    let (_tx, c) = tokio::sync::watch::channel(false);
    s.service
        .apply_update(p, PackDecisions::default(), c, None)
        .await
        .unwrap();
    assert!(i.game_dir().join("mods/user-new.jar").is_file());
    assert!(!i.game_dir().join("mods/pack.jar").exists());
}
#[tokio::test]
async fn unknown_external_rename_is_not_guessed_or_deleted() {
    let s = Setup::new().await;
    let i = install(&s, pack("1", &[("mods/old.jar", b"old")])).await;
    std::fs::rename(
        i.game_dir().join("mods/old.jar"),
        i.game_dir().join("mods/external.jar"),
    )
    .unwrap();
    let p = s
        .service
        .plan_update(
            i.id.clone(),
            pack("2", &[("mods/old.jar", b"new")]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    assert!(p.preserved_paths().contains(&"mods/external.jar".into()));
    let (_tx, c) = tokio::sync::watch::channel(false);
    s.service
        .apply_update(p, PackDecisions::default(), c, None)
        .await
        .unwrap();
    assert!(i.game_dir().join("mods/external.jar").is_file());
    assert!(!i.game_dir().join("mods/old.jar").exists());
}
#[tokio::test]
async fn local_pack_without_baseline_requires_new_instance() {
    let s = Setup::new().await;
    let i = s.store.create("Plain", "fixture").unwrap();
    assert!(
        s.service
            .plan_update(i.id, pack("2", &[]), BTreeMap::new())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn component_changes_are_explicit_not_file_choices() {
    let s = Setup::new().await;
    let i = install(&s, pack("1", &[])).await;
    let p = s
        .service
        .plan_update(i.id, pack("2", &[]), BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(p.old_components()[0].version, "fixture");
    assert_eq!(p.preview().mc_version(), "fixture");
    assert_eq!(p.conflict_count(), 0);
}

#[tokio::test]
async fn online_update_cannot_change_project() {
    let s = Setup::new().await;
    let p = online_pack(&s, "projectA", "v1").await;
    let i = install(&s, p).await;
    let foreign = online_pack(&s, "projectB", "v2").await;
    assert!(
        s.service
            .plan_update(i.id.clone(), foreign, BTreeMap::new())
            .await
            .is_err()
    );
    let local = pack("v2", &[]);
    assert!(
        s.service
            .plan_update(i.id.clone(), local, BTreeMap::new())
            .await
            .is_err()
    );
    let same = online_pack(&s, "projectA", "v2").await;
    assert!(
        s.service
            .plan_update(i.id, same, BTreeMap::new())
            .await
            .is_ok()
    );
}
#[tokio::test]
async fn explicit_use_pack_restores_deliberate_deletion_and_deletes_removed_modified_file() {
    let s = Setup::new().await;
    let i = install(
        &s,
        pack(
            "1",
            &[("config/deleted", b"old"), ("config/removed", b"old")],
        ),
    )
    .await;
    std::fs::remove_file(i.game_dir().join("config/deleted")).unwrap();
    std::fs::write(i.game_dir().join("config/removed"), b"local").unwrap();
    let p = s
        .service
        .plan_update(
            i.id.clone(),
            pack("2", &[("config/deleted", b"new")]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let mut d = PackDecisions::default();
    for c in p.changes() {
        d.set(c.id(), PackDecision::UsePack)
    }
    let (_tx, c) = tokio::sync::watch::channel(false);
    s.service.apply_update(p, d, c, None).await.unwrap();
    assert_eq!(
        std::fs::read(i.game_dir().join("config/deleted")).unwrap(),
        b"new"
    );
    assert!(!i.game_dir().join("config/removed").exists());
}

#[tokio::test]
async fn selected_optional_file_keeps_selection_across_verified_author_rename() {
    let s = Setup::new().await;
    s.serve("/data/a/versions/v/a.jar", b"old").await;
    s.serve("/data/a/versions/v2/a.jar", b"new").await;
    let mut a = file(
        "mods/old.jar",
        b"old",
        "https://cdn.modrinth.com/data/a/versions/v/a.jar",
    );
    a["env"] = serde_json::json!({"client":"optional","server":"required"});
    let (_tx, c) = tokio::sync::watch::channel(false);
    let i = s
        .service
        .install(
            manifest_pack(vec![a]),
            PackInstallChoices::new("Pack").with_optional("mods/old.jar", true),
            c,
            None,
        )
        .await
        .unwrap()
        .instance()
        .clone();
    let mut b = file(
        "mods/new.jar",
        b"new",
        "https://cdn.modrinth.com/data/a/versions/v2/a.jar",
    );
    b["env"] = serde_json::json!({"client":"optional","server":"required"});
    let p = s
        .service
        .plan_update(i.id, manifest_pack(vec![b]), BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(p.changes()[0].target_path(), Some("mods/new.jar"));
}

#[tokio::test]
async fn unchanged_manifest_files_are_verified_and_reused_without_download_requests() {
    let s = Setup::new().await;
    s.serve("/data/a/versions/v/a.jar", b"unchanged").await;
    let p = manifest_pack(vec![file(
        "mods/a.jar",
        b"unchanged",
        "https://cdn.modrinth.com/data/a/versions/v/a.jar",
    )]);
    let i = install(&s, p.clone()).await;
    s.server.reset().await;
    let p = s
        .service
        .plan_update(i.id, p, BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(p.conflict_count(), 0);
    assert!(s.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn unchanged_author_deletion_defaults_to_keep_and_can_restore_after_repeated_update() {
    use patina_core::modpack::PackConflictReason;
    let setup = Setup::new().await;
    let instance = install(&setup, pack("1", &[("config/deleted", b"author")])).await;
    std::fs::remove_file(instance.game_dir().join("config/deleted")).unwrap();
    let (_sender, cancel) = tokio::sync::watch::channel(false);
    for version in ["2", "3"] {
        let plan = setup
            .service
            .plan_update(
                instance.id.clone(),
                pack(version, &[("config/deleted", b"author")]),
                BTreeMap::new(),
            )
            .await
            .unwrap();
        let change = plan
            .changes()
            .iter()
            .find(|change| change.id() == "config/deleted")
            .unwrap();
        assert_eq!(
            change.kind(),
            PackChangeKind::Conflict,
            "unchanged author hash must still expose the restore choice"
        );
        assert_eq!(change.reason(), Some(PackConflictReason::DeletedLocally));
        assert_eq!(change.default_decision(), PackDecision::KeepLocal);
        setup
            .service
            .apply_update(plan, PackDecisions::default(), cancel.clone(), None)
            .await
            .unwrap();
        assert!(!instance.game_dir().join("config/deleted").exists());
        assert_eq!(
            load_state(&instance.dir).unwrap().unwrap().version(),
            version
        );
    }
    let plan = setup
        .service
        .plan_update(
            instance.id.clone(),
            pack("4", &[("config/deleted", b"author")]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let mut decisions = PackDecisions::default();
    decisions.set("config/deleted", PackDecision::UsePack);
    setup
        .service
        .apply_update(plan, decisions, cancel, None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(instance.game_dir().join("config/deleted")).unwrap(),
        b"author"
    );
    let plan = setup
        .service
        .plan_update(
            instance.id,
            pack("5", &[("config/deleted", b"author")]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(plan.changes()[0].kind(), PackChangeKind::Unchanged);
    assert_eq!(plan.conflict_count(), 0);
}
