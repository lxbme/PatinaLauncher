//! Platform HTTP contract tests. All requests go to loopback fixtures, never public APIs.
use patina_core::content::online::*;
use patina_core::loader::LoaderKind;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn query(platform: Platform, kind: ContentKind) -> OnlineQuery {
    OnlineQuery {
        kind,
        text: "test".into(),
        platform,
        mc_version: Some("1.21.1".into()),
        loader: Some(LoaderKind::Fabric),
        category: None,
        sort: SortOrder::Downloads,
        offset: 20,
        limit: 20,
    }
}
fn target(kind: ContentKind) -> TargetCompatibility {
    TargetCompatibility::new(kind, "1.21.1", Some(LoaderKind::Fabric))
}
fn mr_version(id: &str, channel: &str) -> Value {
    json!({"id":id,"project_id":"p","name":id,"version_number":id,"version_type":channel,"date_published":"2026-10-01T00:00:00Z","game_versions":["1.21.1"],"loaders":["fabric"],"dependencies":[{"project_id":"dep","version_id":"dv","dependency_type":"required"}],"files":[{"filename":"test.jar","url":"https://cdn.modrinth.com/test.jar","size":4,"primary":true,"hashes":{"sha1":"abcd","sha512":"efgh"}}]})
}
fn cf_file(id: u64) -> Value {
    json!({"id":id,"modId":99,"displayName":"Test","fileName":"test.jar","fileDate":"2026-10-01T00:00:00Z","releaseType":1,"fileLength":4,"isAvailable":true,"downloadUrl":null,"gameVersions":["1.21.1","Fabric"],"hashes":[{"algo":1,"value":"abcd"}],"dependencies":[{"modId":100,"relationType":3}],"fileFingerprint":1234})
}
async fn get(server: &MockServer, route: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn platform_mapping_keeps_kind_and_compatibility() {
    let server = MockServer::start().await;
    let catalogs = ContentCatalogs::with_endpoints(
        &server.uri(),
        &format!("{}/v1", server.uri()),
        Some("fixture-only".into()),
    )
    .unwrap();
    for (kind, mr, cf) in [
        (ContentKind::Mod, "mod", "6"),
        (ContentKind::ResourcePack, "resourcepack", "12"),
        (ContentKind::Shader, "shader", "6552"),
        (ContentKind::Modpack, "modpack", "4471"),
    ] {
        Mock::given(method("GET")).and(path("/search")).and(query_param("offset","20")).and(query_param("index","downloads")).and(query_param("facets",serde_json::to_string(&if kind == ContentKind::Mod {vec![vec![format!("project_type:{mr}")],vec!["versions:1.21.1".into()],vec!["categories:fabric".into()]]} else {vec![vec![format!("project_type:{mr}")],vec!["versions:1.21.1".into()]]}).unwrap())).respond_with(ResponseTemplate::new(200).set_body_json(json!({"hits":[{"project_id":"p","title":"Test","author":"A","description":"Summary","downloads":7,"date_modified":"2026-10-01T00:00:00Z","categories":[]}],"total_hits":42}))).mount(&server).await;
        get(&server, "/v1/categories", json!({"data":[{"id":6,"name":"Mods","slug":"mc-mods","isClass":true},{"id":12,"name":"Resource Packs","slug":"texture-packs","isClass":true},{"id":6552,"name":"Shaders","slug":"shaders","isClass":true},{"id":4471,"name":"Modpacks","slug":"modpacks","isClass":true}]})).await;
        Mock::given(method("GET")).and(path("/v1/mods/search")).and(header("x-api-key","fixture-only")).and(query_param("classId",cf)).and(query_param("index","20")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":99,"name":"Test","authors":[{"name":"A"}],"summary":"Summary","downloadCount":7,"dateModified":"2026-10-01T00:00:00Z"}],"pagination":{"totalCount":42}}))).mount(&server).await;
        for platform in [Platform::Modrinth, Platform::CurseForge] {
            let page = catalogs.search(query(platform, kind)).await.unwrap();
            assert_eq!(page.total, 42);
            assert_eq!(page.hits[0].author, "A");
        }
    }
    get(&server,"/project/p",json!({"id":"p","slug":"test","title":"Test","description":"Summary","body":"Full description","project_type":"mod","team":"t","client_side":"required","donation_urls":[{"id":"patreon","url":"https://patreon.com/test"}]})).await;
    get(
        &server,
        "/team/t/members",
        json!([{"user":{"username":"Author"}}]),
    )
    .await;
    let p = catalogs
        .project(ProjectRef::new(Platform::Modrinth, "p"))
        .await
        .unwrap();
    assert_eq!(p.authors(), ["Author"]);
    assert_eq!(p.description(), "Full description");
    assert!(p.url().ends_with("/mod/test"));
    assert_eq!(p.support_urls().len(), 1);
    let mut wrong = mr_version("wrong", "release");
    wrong["loaders"] = json!(["forge"]);
    get(
        &server,
        "/project/p/version",
        json!([
            mr_version("beta", "beta"),
            wrong,
            mr_version("stable", "release")
        ]),
    )
    .await;
    let versions = catalogs
        .versions(p.reference().clone(), target(ContentKind::Mod))
        .await
        .unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].reference().id(), "stable");
    assert_eq!(
        versions[0].dependencies()[0].relation(),
        DependencyRelation::Required
    );
    assert_eq!(versions[0].dependencies()[0].version_id().unwrap(), "dv");
    let versions = catalogs
        .versions(
            p.reference().clone(),
            target(ContentKind::Mod)
                .with_channels(vec![ReleaseChannel::Release, ReleaseChannel::Beta]),
        )
        .await
        .unwrap();
    assert_eq!(versions.len(), 2);
}

#[tokio::test]
async fn restricted_file_is_not_downloadable() {
    let server = MockServer::start().await;
    get(&server,"/v1/mods/99",json!({"data":{"links":{"websiteUrl":"https://www.curseforge.com/minecraft/mc-mods/test"},"allowModDistribution":false}})).await;
    Mock::given(method("POST"))
        .and(path("/v1/mods/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json({
            let mut file = cf_file(3);
            file["downloadUrl"] = json!("https://edge.example/test.jar");
            json!({"data":[file]})
        }))
        .mount(&server)
        .await;
    let catalogs = ContentCatalogs::with_endpoints(
        &server.uri(),
        &format!("{}/v1", server.uri()),
        Some("fixture-only".into()),
    )
    .unwrap();
    let v = catalogs
        .version(Platform::CurseForge, "3".into())
        .await
        .unwrap();
    assert!(v.files()[0].download_url().is_none());
    assert!(v.files()[0].page_url().contains("/files/3"));
    assert_eq!(v.dependencies()[0].relation(), DependencyRelation::Required);
}

#[tokio::test]
async fn missing_cf_key_is_explicit() {
    let server = MockServer::start().await;
    let catalogs =
        ContentCatalogs::with_endpoints(&server.uri(), &format!("{}/v1", server.uri()), None)
            .unwrap();
    assert_eq!(
        catalogs
            .search(query(Platform::CurseForge, ContentKind::Mod))
            .await,
        Err(OnlineError::MissingApiKey)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn identify_preserves_unknowns() {
    let server = MockServer::start().await;
    let a = FileIdentity::from_bytes("a.jar", b"known");
    let b = FileIdentity::from_bytes("b.jar", b"unknown");
    Mock::given(method("POST"))
        .and(path("/version_files"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({a.sha1():mr_version("stable","release")})),
        )
        .mount(&server)
        .await;
    let catalogs =
        ContentCatalogs::with_endpoints(&server.uri(), &format!("{}/v1", server.uri()), None)
            .unwrap();
    let result = catalogs
        .identify(Platform::Modrinth, vec![a, b])
        .await
        .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].matches().len(), 1);
    assert!(result[1].matches().is_empty());
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["algorithm"], "sha1");
    assert!(body.get("files").is_none());
}

#[tokio::test]
async fn rate_limit_and_redirect_do_not_leak_key() {
    let server = MockServer::start().await;
    Mock::given(path("/search"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "17"))
        .mount(&server)
        .await;
    let catalogs = ContentCatalogs::with_endpoints(
        &server.uri(),
        &format!("{}/v1", server.uri()),
        Some("fixture-only".into()),
    )
    .unwrap();
    assert_eq!(
        catalogs
            .search(query(Platform::Modrinth, ContentKind::Mod))
            .await,
        Err(OnlineError::RateLimited(Some(17)))
    );
    let other = MockServer::start().await;
    Mock::given(path("/v1/mods/files"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("Location", format!("{}/leak", other.uri())),
        )
        .mount(&server)
        .await;
    assert!(
        catalogs
            .version(Platform::CurseForge, "3".into())
            .await
            .is_err()
    );
    assert!(other.received_requests().await.unwrap().is_empty());
    for request in server.received_requests().await.unwrap() {
        if request.url.path() == "/search" {
            assert!(request.headers.get("x-api-key").is_none());
        }
    }
}

#[tokio::test]
async fn cf_categories_versions_and_fingerprint_mapping() {
    let server = MockServer::start().await;
    get(&server,"/v1/mods/99",json!({"data":{"links":{"websiteUrl":"https://www.curseforge.com/minecraft/mc-mods/test"},"allowModDistribution":true}})).await;
    let catalogs = ContentCatalogs::with_endpoints(
        &server.uri(),
        &format!("{}/v1", server.uri()),
        Some("fixture-only".into()),
    )
    .unwrap();
    get(&server,"/v1/categories",json!({"data":[{"id":123,"slug":"shaders","name":"Shaders","isClass":true},{"id":124,"parentCategoryId":123,"name":"Realistic","isClass":false},{"id":125,"parentCategoryId":124,"name":"Nested","isClass":false}]})).await;
    let categories = catalogs
        .categories(Platform::CurseForge, ContentKind::Shader)
        .await
        .unwrap();
    assert_eq!(categories.len(), 2);
    assert_eq!(categories[0].id(), "124");
    let mut server_only = cf_file(4);
    server_only["isServerPack"] = json!(true);
    let mut beta = cf_file(5);
    beta["releaseType"] = json!(2);
    Mock::given(method("GET"))
        .and(path("/v1/mods/99/files"))
        .and(query_param("index", "0"))
        .and(query_param("gameVersion", "1.21.1"))
        .and(query_param("modLoaderType", "4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data":[cf_file(3),server_only,beta],"pagination":{"totalCount":4}}),
        ))
        .mount(&server)
        .await;
    let mut wrong = cf_file(6);
    wrong["gameVersions"] = json!(["1.20.1", "Forge"]);
    Mock::given(method("GET"))
        .and(path("/v1/mods/99/files"))
        .and(query_param("index", "3"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[wrong],"pagination":{"totalCount":4}})),
        )
        .mount(&server)
        .await;
    let versions = catalogs
        .versions(
            ProjectRef::new(Platform::CurseForge, "99"),
            target(ContentKind::Mod),
        )
        .await
        .unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].reference().id(), "3");
    let a = FileIdentity::from_bytes("known.jar", b"known");
    let b = FileIdentity::from_bytes("unknown.jar", b"unknown");
    let mut file = cf_file(3);
    file["fileFingerprint"] = json!(a.fingerprint());
    file["hashes"] = json!([{"algo":1,"value":a.sha1()}]);
    Mock::given(method("POST")).and(path("/v1/fingerprints/432")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{"exactMatches":[{"id":99,"file":file}],"unmatchedFingerprints":[b.fingerprint()]}}))).mount(&server).await;
    let identified = catalogs
        .identify(Platform::CurseForge, vec![a, b])
        .await
        .unwrap();
    assert_eq!(identified[0].matches().len(), 1);
    assert_eq!(
        identified[0].matches()[0].reference().project().platform(),
        Platform::CurseForge
    );
    assert!(identified[1].matches().is_empty());
}

#[tokio::test]
async fn server_only_project_has_no_client_versions() {
    let server = MockServer::start().await;
    get(&server,"/project/p",json!({"id":"p","slug":"test","title":"Test","body":"Server","project_type":"mod","team":"t","environment":["server_only"]})).await;
    get(&server, "/team/t/members", json!([])).await;
    let catalogs =
        ContentCatalogs::with_endpoints(&server.uri(), &format!("{}/v1", server.uri()), None)
            .unwrap();
    assert!(
        catalogs
            .versions(
                ProjectRef::new(Platform::Modrinth, "p"),
                target(ContentKind::Mod)
            )
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn platform_network_error_is_bounded_and_credential_free() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/mods/files"))
        .respond_with(ResponseTemplate::new(503).set_body_string("sensitive body"))
        .mount(&server)
        .await;
    let catalogs = ContentCatalogs::with_endpoints(
        &server.uri(),
        &format!("{}/v1", server.uri()),
        Some("fixture-only".into()),
    )
    .unwrap();
    let error = catalogs
        .version(Platform::CurseForge, "3".into())
        .await
        .unwrap_err();
    assert_eq!(error, OnlineError::Network("HTTP 503".into()));
    assert!(!error.to_string().contains("fixture-only"));
}

#[test]
fn fingerprint_normalizes_only_documented_whitespace() {
    let a = FileIdentity::from_bytes("a", b"a\tb\nc\rd e");
    let b = FileIdentity::from_bytes("b", b"abcde");
    assert_eq!(a.fingerprint(), b.fingerprint());
    assert_ne!(a.sha1(), b.sha1());
    assert_ne!(
        FileIdentity::from_bytes("c", b"ab\x0bcde").fingerprint(),
        b.fingerprint()
    );
}

#[tokio::test]
async fn curseforge_project_details_keep_author_and_original_links() {
    let server = MockServer::start().await;
    get(&server,"/v1/mods/99",json!({"data":{"id":99,"name":"Test","classId":6,"authors":[{"name":"Author"}],"links":{"websiteUrl":"https://www.curseforge.com/minecraft/mc-mods/test","issuesUrl":"https://example.org/issues"}}})).await;
    get(
        &server,
        "/v1/mods/99/description",
        json!({"data":"<p>Full description</p>"}),
    )
    .await;
    get(
        &server,
        "/v1/categories",
        json!({"data":[{"id":6,"slug":"mc-mods","isClass":true}]}),
    )
    .await;
    let catalogs = ContentCatalogs::with_endpoints(
        &server.uri(),
        &format!("{}/v1", server.uri()),
        Some("fixture-only".into()),
    )
    .unwrap();
    let project = catalogs
        .project(ProjectRef::new(Platform::CurseForge, "99"))
        .await
        .unwrap();
    assert_eq!(project.authors(), ["Author"]);
    assert_eq!(project.description(), "<p>Full description</p>");
    assert_eq!(
        project.url(),
        "https://www.curseforge.com/minecraft/mc-mods/test"
    );
    assert_eq!(project.support_urls(), ["https://example.org/issues"]);
}
