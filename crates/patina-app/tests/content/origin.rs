//! Installed provenance is presented from records, never a local file/project name guess.
use super::*;
use patina_core::content::online::{ContentKind, Platform};

fn sources(store: &InstanceStore, records: &[(Platform, &str, ContentKind, &str)]) {
    let instance = store.load("Alpha").unwrap();
    let records=records.iter().map(|(platform,id,kind,path)|serde_json::json!({"version":{"project":{"platform":platform,"id":id},"id":"file"},"kind":kind,"path":path,"sha1":patina_core::download::sha1_file(&instance.game_dir().join(path)).unwrap()})).collect::<Vec<_>>();
    let text = toml::to_string(&serde_json::json!({"schema_version":1,"records":records})).unwrap();
    std::fs::write(instance.dir.join("content-sources.toml"), text).unwrap();
}

#[gpui_kit::test]
fn mod_origin_tracks_records_refresh_and_disabled_rename_without_guessing(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let instance = store.create("Alpha", "1.21.1").unwrap();
    let mods = instance.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    fabric_jar(&mods, "known.jar", "Same Name");
    fabric_jar(&mods, "unknown.jar", "Same Name");
    sources(
        &store,
        &[(
            Platform::Modrinth,
            "123",
            ContentKind::Mod,
            "mods/known.jar",
        )],
    );
    let app = boot(cx, tmp.path(), AppSettings::default());
    list_settled(&instances_model(&app, cx), cx);
    click(&app, cx, row_id("Alpha"));
    click(&app, cx, "tab-mods");
    let tab = mods_tab(&app, cx);
    mods_settled(&tab, cx);
    assert_eq!(
        label_of(&app, cx, "content-origin:mods/known.jar").as_deref(),
        Some("Modrinth")
    );
    assert_eq!(
        label_of(&app, cx, "content-origin:mods/unknown.jar").as_deref(),
        Some("Local")
    );
    click(&app, cx, mod_row_id("unknown.jar"));
    press(&app, cx, "alt-enter");
    assert_eq!(
        cx.opened_url(),
        None,
        "unknown metadata/title does not identify a platform project"
    );
    click(&app, cx, "content-origin:mods/known.jar");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://modrinth.com/project/123")
    );
    // A source-record update, with the same local name/id, changes platform and target coherently.
    sources(
        &store,
        &[(
            Platform::CurseForge,
            "123",
            ContentKind::Mod,
            "mods/known.jar",
        )],
    );
    click(&app, cx, "content-refresh");
    mods_settled(&tab, cx);
    assert_eq!(
        label_of(&app, cx, "content-origin:mods/known.jar").as_deref(),
        Some("CurseForge")
    );
    click(&app, cx, mod_row_id("known.jar"));
    press(&app, cx, "alt-enter");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://www.curseforge.com/projects/123")
    );
    press(&app, cx, "space");
    settle(cx, |_| mods.join("known.jar.disabled").exists());
    settle(cx, |cx| {
        shows(&app, cx, "content-origin:mods/known.jar.disabled")
    });
    assert_eq!(
        label_of(&app, cx, "content-origin:mods/known.jar.disabled").as_deref(),
        Some("CurseForge")
    );
    press(&app, cx, "alt-enter");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://www.curseforge.com/projects/123")
    );
    assert!(!disabled(&app, cx, mod_delete_id("known.jar.disabled")));
    let model = instances_model(&app, cx);
    model.update(cx, |m, cx| m.set_run_state("Alpha", Some(elsewhere()), cx));
    cx.run_until_parked();
    assert!(disabled(&app, cx, mod_switch_id("known.jar.disabled")));
    assert!(disabled(&app, cx, mod_delete_id("known.jar.disabled")));
    click(&app, cx, "content-origin:mods/known.jar.disabled");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://www.curseforge.com/projects/123"),
        "viewing source is allowed while mutation is locked"
    );
    click(&app, cx, mod_row_id("known.jar.disabled"));
    press(&app, cx, "space");
    assert!(mods.join("known.jar.disabled").exists());
    assert_eq!(tab.read_with(cx, |t, _| t.changes_in_flight()), 0);
    model.update(cx, |m, cx| m.set_run_state("Alpha", None, cx));
    mods_settled(&tab, cx);
    assert_eq!(
        label_of(&app, cx, "content-origin:mods/known.jar.disabled").as_deref(),
        Some("CurseForge")
    );
}

#[gpui_kit::test]
fn pack_and_shader_origins_open_record_identity_and_unknown_folders_stay_local(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(DataDirs::new(tmp.path()).instances());
    let instance = store.create("Alpha", "1.21.1").unwrap();
    let packs = instance.game_dir().join("resourcepacks");
    let shaders = instance.game_dir().join("shaderpacks");
    std::fs::create_dir_all(&packs).unwrap();
    std::fs::create_dir_all(&shaders).unwrap();
    jar_with(
        &packs,
        "known.zip",
        &[("pack.mcmeta", mcmeta("Pack").as_bytes())],
    );
    jar_with(&shaders, "known.zip", &[("shaders/test.fsh", b"shader")]);
    let local = packs.join("123");
    std::fs::create_dir(&local).unwrap();
    std::fs::write(local.join("pack.mcmeta"), mcmeta("Local")).unwrap();
    sources(
        &store,
        &[
            (
                Platform::Modrinth,
                "123",
                ContentKind::ResourcePack,
                "resourcepacks/known.zip",
            ),
            (
                Platform::CurseForge,
                "456",
                ContentKind::Shader,
                "shaderpacks/known.zip",
            ),
        ],
    );
    let app = boot(cx, tmp.path(), AppSettings::default());
    list_settled(&instances_model(&app, cx), cx);
    click(&app, cx, row_id("Alpha"));
    click(&app, cx, "tab-resource-packs");
    let tab = local_tab_of(&app, cx, LocalKind::ResourcePacks);
    local_settled(&tab, cx);
    assert_eq!(
        label_of(&app, cx, "content-origin:resourcepacks/known.zip").as_deref(),
        Some("Modrinth")
    );
    assert_eq!(
        label_of(&app, cx, "content-origin:resourcepacks/123").as_deref(),
        Some("Local")
    );
    click(&app, cx, "content-origin:resourcepacks/known.zip");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://modrinth.com/project/123")
    );
    click(&app, cx, "tab-shaders");
    let tab = local_tab_of(&app, cx, LocalKind::Shaders);
    local_settled(&tab, cx);
    assert_eq!(
        label_of(&app, cx, "content-origin:shaderpacks/known.zip").as_deref(),
        Some("CurseForge")
    );
    click(&app, cx, local_row_id(LocalKind::Shaders, "known.zip"));
    press(&app, cx, "alt-enter");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://www.curseforge.com/projects/456")
    );
}
