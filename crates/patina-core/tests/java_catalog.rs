use patina_core::java::catalog::{
    JavaCatalog, JavaVendor, parse_microsoft, parse_temurin, parse_zulu,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

#[test]
fn official_fixtures_keep_only_stable_linux_x64_jdks() {
    let temurin = include_str!("fixtures/java/temurin-live.json");
    let releases = parse_temurin(temurin, 21).unwrap();
    assert_eq!(releases[0].version(), "21.0.13+9");
    assert_eq!(releases[0].sha256().unwrap().len(), 64);
    assert_eq!(releases[0].size(), Some(207858352));
    for (old, new) in [
        ("\"x64\"", "\"aarch64\""),
        ("\"jdk\"", "\"jre\""),
        ("\"ga\"", "\"ea\""),
    ] {
        assert!(
            parse_temurin(&temurin.replace(old, new), 21)
                .unwrap()
                .is_empty()
        );
    }
    assert!(parse_temurin(temurin, 8).unwrap().is_empty());
    let zulu = include_str!("fixtures/java/zulu-detail.json");
    let release = parse_zulu(zulu, 21).unwrap().unwrap();
    assert_eq!(release.vendor(), JavaVendor::Zulu);
    assert_eq!(release.size(), Some(213609000));
    assert!(
        parse_zulu(&zulu.replace("\"hw_bitness\":64", "\"hw_bitness\":32"), 21)
            .unwrap()
            .is_none()
    );
    assert!(
        parse_zulu(&zulu.replace("\"ga\"", "\"ea\""), 21)
            .unwrap()
            .is_none()
    );
    let microsoft = parse_microsoft(include_str!("fixtures/java/microsoft-download.md")).unwrap();
    assert_eq!(
        microsoft.iter().map(|r| r.major()).collect::<Vec<_>>(),
        [25, 21, 17, 11]
    );
    assert!(microsoft.iter().all(|r| r.size().is_none()));
    assert!(parse_microsoft("no releases").is_err());
    assert!(
        parse_microsoft(
            &include_str!("fixtures/java/microsoft-download.md")
                .replace(".sha256sum.txt", ".missing")
        )
        .is_err()
    );
}

#[tokio::test]
async fn temurin_queries_platform_package_and_paginates() {
    let server = MockServer::start().await;
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/java/temurin-live.json")).unwrap();
    let full =
        serde_json::to_string(&(0..100).map(|n| rows[n % 15].clone()).collect::<Vec<_>>()).unwrap();
    for (page, body) in [(0, full.as_str()), (1, "[]")] {
        Mock::given(method("GET"))
            .and(path("/assets/feature_releases/21/ga"))
            .and(query_param("architecture", "x64"))
            .and(query_param("os", "linux"))
            .and(query_param("image_type", "jdk"))
            .and(query_param("page", page.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
            .expect(1)
            .mount(&server)
            .await;
    }
    let catalog = JavaCatalog::with_endpoints(
        reqwest::Client::new(),
        &server.uri(),
        &server.uri(),
        &server.uri(),
    );
    let releases = catalog.releases(JavaVendor::Temurin, 21).await.unwrap();
    assert_eq!(releases.len(), 15);
    assert_eq!(
        releases[0].version(),
        "21.0.13+9",
        "a GA 21+35 must not sort above 21.0.13+9"
    );
}

#[tokio::test]
async fn catalog_propagates_network_failure_not_empty_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let catalog = JavaCatalog::with_endpoints(
        reqwest::Client::new(),
        &server.uri(),
        &server.uri(),
        &server.uri(),
    );
    assert!(catalog.releases(JavaVendor::Temurin, 21).await.is_err());
}

#[tokio::test]
async fn temurin_official_404_means_no_builds_not_network_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let c = JavaCatalog::with_endpoints(
        reqwest::Client::new(),
        &server.uri(),
        &server.uri(),
        &server.uri(),
    );
    assert!(c.releases(JavaVendor::Temurin, 8).await.unwrap().is_empty());
}

#[test]
fn updates_keep_vendor_major_and_distinguish_zulu_vendor_builds() {
    let original = include_str!("fixtures/java/zulu-detail.json");
    let old = parse_zulu(original, 21).unwrap().unwrap();
    let mut v: serde_json::Value = serde_json::from_str(original).unwrap();
    v["distro_version"][2] = 110.into();
    let newer = parse_zulu(&v.to_string(), 21).unwrap().unwrap();
    assert!(newer.is_newer_than(&old));
    assert_ne!(
        newer.id(),
        old.id(),
        "same Java version with newer Zulu build must not overwrite old runtime"
    );
    v["distro_version"][2] = 109.into();
    v["openjdk_build_number"] = 2.into();
    let rebuild = parse_zulu(&v.to_string(), 21).unwrap().unwrap();
    assert!(rebuild.is_newer_than(&old));
    assert_ne!(rebuild.id(), old.id());
    let temurin = parse_temurin(include_str!("fixtures/java/temurin-live.json"), 21)
        .unwrap()
        .remove(0);
    assert!(!temurin.is_newer_than(&old));
    assert!(!old.is_newer_than(&old));
    v["java_version"][0] = 25.into();
    let next_major = parse_zulu(&v.to_string(), 25).unwrap().unwrap();
    assert!(!next_major.is_newer_than(&old));
}

#[test]
fn microsoft_download_rows_require_stable_heading_and_matching_platform() {
    let actual = include_str!("fixtures/java/microsoft-download.md");
    assert!(
        parse_microsoft(
            &actual.replace("| Linux | x64 | `tar.gz` |", "| Windows | x64 | `tar.gz` |")
        )
        .is_err()
    );
    assert!(parse_microsoft(&actual.replace(" LTS |", " Early Access |")).is_err());
    let changed = actual.replace(
        "microsoft-jdk-21.0.13.0.2-linux-x64.tar.gz)",
        "different-build.tar.gz)",
    );
    assert!(parse_microsoft(&changed).is_err());
}

#[tokio::test]
async fn zulu_requests_explicit_platform_and_resolves_official_package_details() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/zulu/"))
        .and(query_param("os", "linux"))
        .and(query_param("arch", "x86"))
        .and(query_param("hw_bitness", "64"))
        .and(query_param("java_package_type", "jdk"))
        .and(query_param("archive_type", "tar.gz"))
        .and(query_param("release_status", "ga"))
        .and(query_param("page", "1"))
        .and(query_param("java_version", "21"))
        .and(query_param("javafx_bundled", "false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!([{"package_uuid":"3a7c000f-2eb1-4e4c-b2d0-b9680efe2b95"}]),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/zulu/3a7c000f-2eb1-4e4c-b2d0-b9680efe2b95"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/java/zulu-detail.json"),
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let catalog = JavaCatalog::with_endpoints(
        reqwest::Client::new(),
        &server.uri(),
        &format!("{}/zulu/", server.uri()),
        &server.uri(),
    );
    let releases = catalog.releases(JavaVendor::Zulu, 21).await.unwrap();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].version(), "21.0.13.0.3+1-zulu21.54.109.0");
    assert_eq!(
        releases[0].sha256(),
        Some("abe544103f1c4f0cc4b8997c0cac847eb1e842382eef57ce361c982fb0a3de11")
    );
}
