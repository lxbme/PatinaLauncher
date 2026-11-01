//! Real temp-instance application tests with a fixture catalog and loopback files.
use futures::future::BoxFuture;
use patina_core::content::online::*;
use patina_core::download::{Downloader, Mirror};
use patina_core::instance::{Component, ComponentUid, InstanceStore};
use patina_core::loader::LoaderKind;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::sync::Arc;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Catalog(HashMap<String, ContentVersion>);
impl ContentSource for Catalog {
    fn identify(
        &self,
        platform: Platform,
        files: Vec<FileIdentity>,
    ) -> BoxFuture<'static, Result<Vec<IdentifiedFile>, OnlineError>> {
        let values = files
            .into_iter()
            .map(|identity| {
                let matches = self
                    .0
                    .values()
                    .filter(|v| {
                        v.reference().project().platform() == platform
                            && v.files()
                                .iter()
                                .any(|f| f.hashes().sha1() == Some(identity.sha1()))
                    })
                    .cloned()
                    .collect();
                IdentifiedFile::new(identity, matches)
            })
            .collect();
        Box::pin(async move { Ok(values) })
    }
    fn search(&self, _: OnlineQuery) -> BoxFuture<'static, Result<OnlinePage, OnlineError>> {
        Box::pin(async { Err(OnlineError::Unavailable) })
    }
    fn version(
        &self,
        _: Platform,
        id: String,
    ) -> BoxFuture<'static, Result<ContentVersion, OnlineError>> {
        let value = self.0.get(&id).cloned();
        Box::pin(async move { value.ok_or(OnlineError::NotFound) })
    }
    fn project(
        &self,
        reference: ProjectRef,
    ) -> BoxFuture<'static, Result<ContentProject, OnlineError>> {
        let kind = match reference.id() {
            "resource" => ContentKind::ResourcePack,
            "shader" => ContentKind::Shader,
            _ => ContentKind::Mod,
        };
        Box::pin(async move {
            Ok(ContentProject::new(
                reference,
                kind,
                "Test",
                "https://modrinth.com/project/test",
            ))
        })
    }
    fn versions(
        &self,
        reference: ProjectRef,
        target: TargetCompatibility,
    ) -> BoxFuture<'static, Result<Vec<ContentVersion>, OnlineError>> {
        let values = self
            .0
            .values()
            .filter(|v| v.reference().project() == &reference && target.accepts(v))
            .cloned()
            .collect();
        Box::pin(async move { Ok(values) })
    }
}
fn project(id: &str) -> ProjectRef {
    ProjectRef::new(Platform::Modrinth, id)
}
fn version(server: &MockServer, p: &str, id: &str, bytes: &[u8]) -> ContentVersion {
    ContentVersion::new(VersionRef::new(project(p), id), id, ReleaseChannel::Release)
        .with_published(if id == "old" { 1 } else { 2 })
        .with_compatibility(vec!["1.21.1".into()], vec![LoaderKind::Fabric])
        .with_files(vec![ContentFile::new(
            format!("{id}.jar"),
            bytes.len() as u64,
            FileHashes::new(Some(hex::encode(Sha1::digest(bytes))), None, None),
            Some(format!("{}/{id}", server.uri())),
            "https://modrinth.com/project/test",
        )])
}
fn setup(versions: Vec<ContentVersion>) -> (tempfile::TempDir, InstanceStore, ContentService) {
    let temp = tempfile::tempdir().unwrap();
    let store = InstanceStore::new(temp.path());
    let mut instance = store.create("Test", "1.21.1").unwrap();
    instance.config.components.push(Component {
        uid: ComponentUid::Fabric,
        version: "0.16.0".into(),
        profile: Some("fabric-test".into()),
    });
    store.save(&instance).unwrap();
    let catalog = Arc::new(Catalog(
        versions
            .into_iter()
            .map(|v| (v.reference().id().into(), v))
            .collect(),
    ));
    let downloader = Arc::new(
        Downloader::new(reqwest::Client::new(), Mirror::official())
            .with_retry(1, std::time::Duration::ZERO),
    );
    let service = ContentService::new(store.clone(), catalog, downloader);
    (temp, store, service)
}
fn request(id: &str) -> InstallRequest {
    InstallRequest::new(
        "Test",
        VersionRef::new(project("main"), id),
        TargetCompatibility::new(ContentKind::Mod, "1.21.1", Some(LoaderKind::Fabric)),
    )
}
async fn serve(server: &MockServer, id: &str, body: &[u8]) {
    Mock::given(path(format!("/{id}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(server)
        .await;
}
async fn apply(
    service: &ContentService,
    request: InstallRequest,
    choices: InstallChoices,
) -> Result<InstallOutcome, ContentError> {
    let plan = service.plan_install(request).await?;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    service.apply(plan, choices, rx, None).await
}

#[tokio::test]
async fn required_dependencies_are_installed_once_optional_defaults_off() {
    let server = MockServer::start().await;
    for id in ["main", "required", "optional"] {
        serve(&server, id, id.as_bytes()).await;
    }
    let main = version(&server, "main", "main", b"main").with_dependencies(vec![
        Dependency::new(Some(project("dep")), None, DependencyRelation::Required),
        Dependency::new(
            Some(project("dep")),
            Some("required".into()),
            DependencyRelation::Required,
        ),
        Dependency::new(
            Some(project("optional")),
            None,
            DependencyRelation::Optional,
        ),
    ]);
    let (_temp, store, service) = setup(vec![
        main,
        version(&server, "dep", "required", b"required"),
        version(&server, "optional", "optional", b"optional"),
    ]);
    let result = apply(&service, request("main"), InstallChoices::default())
        .await
        .unwrap();
    assert_eq!(result.applied().len(), 2);
    let dir = store.load("Test").unwrap().game_dir().join("mods");
    assert!(dir.join("main.jar").is_file());
    assert!(dir.join("required.jar").is_file());
    assert!(!dir.join("optional.jar").exists());
    assert_eq!(
        load_sources(&store.load("Test").unwrap().dir)
            .unwrap()
            .records()
            .len(),
        2
    );
    apply(&service, request("main"), InstallChoices::default())
        .await
        .unwrap();
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "installed compatible files reused"
    );
}
#[tokio::test]
async fn failed_update_cancel_and_busy_keep_old_files_and_sources() {
    let server = MockServer::start().await;
    serve(&server, "old", b"old").await;
    let (_temp, store, service) = setup(vec![
        version(&server, "main", "old", b"old"),
        version(&server, "main", "new", b"new"),
    ]);
    apply(&service, request("old"), InstallChoices::default())
        .await
        .unwrap();
    let instance = store.load("Test").unwrap();
    let before = std::fs::read(instance.dir.join("content-sources.toml")).unwrap();
    assert!(
        apply(&service, request("new"), InstallChoices::default())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(instance.game_dir().join("mods/old.jar")).unwrap(),
        b"old"
    );
    assert_eq!(
        std::fs::read(instance.dir.join("content-sources.toml")).unwrap(),
        before
    );
    let plan = service.plan_install(request("new")).await.unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(true);
    assert!(matches!(
        service
            .apply(plan, InstallChoices::default(), rx, None)
            .await,
        Err(ContentError::Cancelled)
    ));
    let held = store.lock("Test").unwrap();
    assert!(service.plan_install(request("new")).await.is_err());
    drop(held);
    assert_eq!(
        std::fs::read(instance.dir.join("content-sources.toml")).unwrap(),
        before
    );
}
#[tokio::test]
async fn rename_update_preserves_disabled_and_modified_files_require_permission() {
    let server = MockServer::start().await;
    serve(&server, "old", b"old").await;
    serve(&server, "new", b"new").await;
    let (_temp, store, service) = setup(vec![
        version(&server, "main", "old", b"old"),
        version(&server, "main", "new", b"new"),
    ]);
    apply(&service, request("old"), InstallChoices::default())
        .await
        .unwrap();
    let mods = store.load("Test").unwrap().game_dir().join("mods");
    std::fs::rename(mods.join("old.jar"), mods.join("old.jar.disabled")).unwrap();
    std::fs::write(mods.join("old.jar.disabled"), b"modified").unwrap();
    let plan = service.plan_install(request("new")).await.unwrap();
    assert!(!plan.conflicts().is_empty());
    let (_tx, rx) = tokio::sync::watch::channel(false);
    assert!(
        service
            .apply(plan, InstallChoices::default(), rx, None)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(mods.join("old.jar.disabled")).unwrap(),
        b"modified"
    );
    apply(
        &service,
        request("new"),
        InstallChoices::default().with_replacement("mods/old.jar.disabled"),
    )
    .await
    .unwrap();
    assert!(!mods.join("old.jar.disabled").exists());
    assert!(!mods.join("new.jar").exists());
    assert_eq!(
        std::fs::read(mods.join("new.jar.disabled")).unwrap(),
        b"new"
    );
}
#[tokio::test]
async fn missing_required_and_corrupt_sources_are_not_silent_success() {
    let server = MockServer::start().await;
    let main = version(&server, "main", "main", b"main").with_dependencies(vec![Dependency::new(
        Some(project("missing")),
        None,
        DependencyRelation::Required,
    )]);
    let (_temp, store, service) = setup(vec![main]);
    assert!(service.plan_install(request("main")).await.is_err());
    std::fs::write(
        store.load("Test").unwrap().dir.join("content-sources.toml"),
        b"not valid toml",
    )
    .unwrap();
    assert!(service.plan_install(request("main")).await.is_err());
}
#[tokio::test]
async fn restricted_required_file_waits_for_valid_manual_choice() {
    let server = MockServer::start().await;
    let mut main = version(&server, "main", "main", b"main");
    main = main.with_files(vec![ContentFile::new(
        "manual.jar",
        4,
        FileHashes::new(Some(hex::encode(Sha1::digest(b"main"))), None, None),
        None,
        "https://modrinth.com/project/test",
    )]);
    let (temp, store, service) = setup(vec![main]);
    let file = temp.path().join("manual-download.jar");
    std::fs::write(&file, b"nope").unwrap();
    let plan = service.plan_install(request("main")).await.unwrap();
    assert_eq!(plan.restricted_files().len(), 1);
    let choices = InstallChoices::default()
        .with_manual_file(VersionRef::new(project("main"), "main"), file.clone());
    assert!(
        apply(&service, request("main"), choices.clone())
            .await
            .is_err()
    );
    assert!(
        !store
            .load("Test")
            .unwrap()
            .game_dir()
            .join("mods/manual.jar")
            .exists()
    );
    std::fs::write(file, b"main").unwrap();
    apply(&service, request("main"), choices).await.unwrap();
    assert!(
        store
            .load("Test")
            .unwrap()
            .game_dir()
            .join("mods/manual.jar")
            .exists()
    );
}

#[tokio::test]
async fn disabled_required_dependency_needs_explicit_enable_and_cycle_is_rejected() {
    let server = MockServer::start().await;
    for id in ["dep", "main"] {
        serve(&server, id, id.as_bytes()).await;
    }
    let dep = version(&server, "dep", "dep", b"dep");
    let main = version(&server, "main", "main", b"main").with_dependencies(vec![Dependency::new(
        Some(project("dep")),
        None,
        DependencyRelation::Required,
    )]);
    let (_temp, store, service) = setup(vec![dep, main]);
    let dep_request = InstallRequest::new(
        "Test",
        VersionRef::new(project("dep"), "dep"),
        TargetCompatibility::new(ContentKind::Mod, "1.21.1", Some(LoaderKind::Fabric)),
    );
    apply(&service, dep_request, InstallChoices::default())
        .await
        .unwrap();
    let mods = store.load("Test").unwrap().game_dir().join("mods");
    std::fs::rename(mods.join("dep.jar"), mods.join("dep.jar.disabled")).unwrap();
    assert!(
        apply(&service, request("main"), InstallChoices::default())
            .await
            .is_err()
    );
    assert!(!mods.join("main.jar").exists());
    apply(
        &service,
        request("main"),
        InstallChoices::default().with_enabled_dependency(project("dep")),
    )
    .await
    .unwrap();
    assert!(mods.join("dep.jar").exists());
    assert!(!mods.join("dep.jar.disabled").exists());
    let cyclic =
        version(&server, "main", "main", b"main").with_dependencies(vec![Dependency::new(
            Some(project("main")),
            Some("main".into()),
            DependencyRelation::Required,
        )]);
    let (_temp, _store, service) = setup(vec![cyclic]);
    assert!(matches!(
        service.plan_install(request("main")).await,
        Err(ContentError::Cycle(_))
    ));
}

#[tokio::test]
async fn foreign_file_and_changed_target_are_not_overwritten() {
    let server = MockServer::start().await;
    serve(&server, "main", b"main").await;
    let (_temp, store, service) = setup(vec![version(&server, "main", "main", b"main")]);
    let instance = store.load("Test").unwrap();
    let mods = instance.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    std::fs::write(mods.join("main.jar"), b"foreign").unwrap();
    assert!(
        apply(&service, request("main"), InstallChoices::default())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(mods.join("main.jar")).unwrap(), b"foreign");
    let plan = service.plan_install(request("main")).await.unwrap();
    std::fs::write(mods.join("main.jar"), b"changed after plan").unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    assert!(matches!(
        service
            .apply(
                plan,
                InstallChoices::default().with_replacement("mods/main.jar"),
                rx,
                None
            )
            .await,
        Err(ContentError::TargetChanged)
    ));
    assert_eq!(
        std::fs::read(mods.join("main.jar")).unwrap(),
        b"changed after plan"
    );
}

#[tokio::test]
async fn updates_keep_identity_and_compatibility_without_downgrading() {
    let server = MockServer::start().await;
    serve(&server, "old", b"old").await;
    serve(&server, "new", b"new").await;
    let (_temp, store, service) = setup(vec![
        version(&server, "main", "old", b"old"),
        version(&server, "main", "new", b"new"),
    ]);
    apply(&service, request("old"), InstallChoices::default())
        .await
        .unwrap();
    let instance = store.load("Test").unwrap();
    let mods = instance.game_dir().join("mods");
    std::fs::rename(mods.join("old.jar"), mods.join("old.jar.disabled")).unwrap();
    let updates = service
        .check_updates(
            "Test".into(),
            UpdateSelection::new(Platform::Modrinth, ContentKind::Mod),
        )
        .await
        .unwrap();
    assert_eq!(updates.updates().len(), 1);
    assert_eq!(
        updates.updates()[0].candidate().reference().project(),
        &project("main")
    );
    assert_eq!(updates.updates()[0].candidate().reference().id(), "new");
    apply(
        &service,
        updates.updates()[0].request().clone(),
        InstallChoices::default(),
    )
    .await
    .unwrap();
    assert!(mods.join("new.jar.disabled").exists());
    assert!(!mods.join("old.jar.disabled").exists());
    let updates = service
        .check_updates(
            "Test".into(),
            UpdateSelection::new(Platform::Modrinth, ContentKind::Mod),
        )
        .await
        .unwrap();
    assert!(updates.updates().is_empty());
}

#[tokio::test]
async fn manual_identification_associates_only_exact_single_matches_and_modified_updates_need_choice()
 {
    let server = MockServer::start().await;
    let (_temp, store, service) = setup(vec![
        version(&server, "main", "old", b"old"),
        version(&server, "main", "new", b"new"),
    ]);
    let instance = store.load("Test").unwrap();
    let mods = instance.game_dir().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    std::fs::write(mods.join("hand-added.jar"), b"old").unwrap();
    std::fs::write(mods.join("unknown.jar"), b"unknown").unwrap();
    let updates = service
        .check_updates(
            "Test".into(),
            UpdateSelection::new(Platform::Modrinth, ContentKind::Mod),
        )
        .await
        .unwrap();
    assert_eq!(updates.updates().len(), 1);
    assert_eq!(updates.unknown_files(), ["mods/unknown.jar"]);
    assert_eq!(
        load_sources(&instance.dir).unwrap().records()[0].path(),
        "mods/hand-added.jar"
    );
    std::fs::write(mods.join("hand-added.jar"), b"user changed").unwrap();
    let updates = service
        .check_updates(
            "Test".into(),
            UpdateSelection::new(Platform::CurseForge, ContentKind::Mod)
                .with_paths(vec!["mods/hand-added.jar".into()]),
        )
        .await
        .unwrap();
    assert!(updates.updates()[0].is_modified());
    assert_eq!(
        updates.updates()[0]
            .candidate()
            .reference()
            .project()
            .platform(),
        Platform::Modrinth,
        "known source wins over identification preference"
    );
}

#[tokio::test]
async fn unselected_optional_incompatibility_does_not_block_main_install() {
    let server = MockServer::start().await;
    serve(&server, "main", b"main").await;
    serve(&server, "optional", b"optional").await;
    let main = version(&server, "main", "main", b"main").with_dependencies(vec![Dependency::new(
        Some(project("optional")),
        None,
        DependencyRelation::Optional,
    )]);
    let optional = version(&server, "optional", "optional", b"optional").with_dependencies(vec![
        Dependency::new(
            Some(project("main")),
            None,
            DependencyRelation::Incompatible,
        ),
    ]);
    let (_temp, store, service) = setup(vec![main, optional]);
    let result = apply(&service, request("main"), InstallChoices::default()).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        !store
            .load("Test")
            .unwrap()
            .game_dir()
            .join("mods/optional.jar")
            .exists()
    );
    assert!(
        apply(
            &service,
            request("main"),
            InstallChoices::default().with_optional(project("optional"))
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn resourcepacks_and_shaders_install_and_update_without_mod_loader_filter() {
    let server = MockServer::start().await;
    let mut versions = vec![];
    for project_id in ["resource", "shader"] {
        for (channel, published) in [("old", 1), ("new", 2)] {
            let id = format!("{project_id}-{channel}");
            serve(&server, &id, id.as_bytes()).await;
            let v = version(&server, project_id, &id, id.as_bytes())
                .with_published(published)
                .with_compatibility(vec!["1.21.1".into()], vec![]);
            let file = v.files()[0].clone();
            versions.push(v.with_files(vec![ContentFile::new(
                format!("{id}.zip"),
                file.size(),
                file.hashes().clone(),
                file.download_url().map(str::to_owned),
                file.page_url(),
            )]));
        }
    }
    let (_temp, store, service) = setup(versions);
    for (p, kind, folder) in [
        ("resource", ContentKind::ResourcePack, "resourcepacks"),
        ("shader", ContentKind::Shader, "shaderpacks"),
    ] {
        let request = InstallRequest::new(
            "Test",
            VersionRef::new(project(p), format!("{p}-old")),
            TargetCompatibility::new(kind, "1.21.1", None),
        );
        apply(&service, request, InstallChoices::default())
            .await
            .unwrap();
        let updates = service
            .check_updates(
                "Test".into(),
                UpdateSelection::new(Platform::Modrinth, kind),
            )
            .await
            .unwrap();
        assert_eq!(updates.updates().len(), 1);
        apply(
            &service,
            updates.updates()[0].request().clone(),
            InstallChoices::default(),
        )
        .await
        .unwrap();
        let dir = store.load("Test").unwrap().game_dir().join(folder);
        assert!(dir.join(format!("{p}-new.zip")).exists());
        assert!(!dir.join(format!("{p}-old.zip")).exists());
    }
}
#[tokio::test]
async fn sha512_only_files_are_verified_before_application() {
    let server = MockServer::start().await;
    serve(&server, "main", b"main").await;
    let hash = hex::encode(sha2::Sha512::digest(b"main"));
    let main = version(&server, "main", "main", b"main").with_files(vec![ContentFile::new(
        "main.jar",
        4,
        FileHashes::new(None, Some(hash), None),
        Some(format!("{}/main", server.uri())),
        "https://modrinth.com/project/test",
    )]);
    let (_temp, store, service) = setup(vec![main]);
    apply(&service, request("main"), InstallChoices::default())
        .await
        .unwrap();
    assert!(
        store
            .load("Test")
            .unwrap()
            .game_dir()
            .join("mods/main.jar")
            .is_file()
    );
}

#[tokio::test]
async fn unavailable_required_child_of_unselected_optional_does_not_block_main() {
    let server = MockServer::start().await;
    serve(&server, "main", b"main").await;
    let main = version(&server, "main", "main", b"main").with_dependencies(vec![Dependency::new(
        Some(project("addon")),
        None,
        DependencyRelation::Optional,
    )]);
    let addon =
        version(&server, "addon", "addon", b"addon").with_dependencies(vec![Dependency::new(
            Some(project("missing")),
            None,
            DependencyRelation::Required,
        )]);
    let (_temp, store, service) = setup(vec![main, addon]);
    assert!(
        apply(&service, request("main"), InstallChoices::default())
            .await
            .is_ok()
    );
    assert!(
        apply(
            &service,
            request("main"),
            InstallChoices::default().with_optional(project("addon"))
        )
        .await
        .is_err()
    );
    assert!(
        !store
            .load("Test")
            .unwrap()
            .game_dir()
            .join("mods/addon.jar")
            .exists()
    );
}

#[tokio::test]
async fn shader_mod_dependency_uses_existing_instance_loader_without_changing_it() {
    let server = MockServer::start().await;
    serve(&server, "shader", b"shader").await;
    serve(&server, "renderer", b"renderer").await;
    let shader = version(&server, "shader", "shader", b"shader")
        .with_compatibility(vec!["1.21.1".into()], vec![])
        .with_files(vec![ContentFile::new(
            "shader.zip",
            6,
            FileHashes::new(Some(hex::encode(Sha1::digest(b"shader"))), None, None),
            Some(format!("{}/shader", server.uri())),
            "https://modrinth.com/shader/test",
        )])
        .with_dependencies(vec![Dependency::new(
            Some(project("renderer")),
            None,
            DependencyRelation::Required,
        )]);
    let (_temp, store, service) = setup(vec![
        shader,
        version(&server, "renderer", "renderer", b"renderer"),
    ]);
    let request = InstallRequest::new(
        "Test",
        VersionRef::new(project("shader"), "shader"),
        TargetCompatibility::new(ContentKind::Shader, "1.21.1", None),
    );
    assert!(
        apply(&service, request, InstallChoices::default())
            .await
            .is_ok()
    );
    let instance = store.load("Test").unwrap();
    assert_eq!(
        instance.config.loader().map(|(kind, _)| kind),
        Some(LoaderKind::Fabric)
    );
    assert!(instance.game_dir().join("mods/renderer.jar").exists());
}

#[tokio::test]
async fn explicit_foreign_replacement_removes_only_displaced_provenance() {
    let server = MockServer::start().await;
    serve(&server, "cf-old", b"old").await;
    serve(&server, "mr-new", b"new").await;
    let old = ContentVersion::new(
        VersionRef::new(ProjectRef::new(Platform::CurseForge, "main"), "cf-old"),
        "old",
        ReleaseChannel::Release,
    )
    .with_compatibility(vec!["1.21.1".into()], vec![LoaderKind::Fabric])
    .with_files(vec![ContentFile::new(
        "shared.jar",
        3,
        FileHashes::new(Some(hex::encode(Sha1::digest(b"old"))), None, None),
        Some(format!("{}/cf-old", server.uri())),
        "https://www.curseforge.com/projects/1",
    )]);
    let new = version(&server, "main", "mr-new", b"new");
    let file = new.files()[0].clone();
    let new = new.with_files(vec![ContentFile::new(
        "shared.jar",
        3,
        file.hashes().clone(),
        file.download_url().map(str::to_owned),
        file.page_url(),
    )]);
    let (_temp, store, service) = setup(vec![old, new]);
    let old_request = InstallRequest::new(
        "Test",
        VersionRef::new(ProjectRef::new(Platform::CurseForge, "main"), "cf-old"),
        TargetCompatibility::new(ContentKind::Mod, "1.21.1", Some(LoaderKind::Fabric)),
    );
    apply(&service, old_request, InstallChoices::default())
        .await
        .unwrap();
    assert!(
        apply(&service, request("mr-new"), InstallChoices::default())
            .await
            .is_err()
    );
    apply(
        &service,
        request("mr-new"),
        InstallChoices::default().with_replacement("mods/shared.jar"),
    )
    .await
    .unwrap();
    let sources = load_sources(&store.load("Test").unwrap().dir).unwrap();
    assert_eq!(sources.records().len(), 1);
    assert_eq!(
        sources.records()[0].version().project().platform(),
        Platform::Modrinth
    );
}
