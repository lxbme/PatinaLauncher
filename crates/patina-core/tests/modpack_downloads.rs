//! Pack download scheduling uses the existing downloader's concurrency and verified destinations.
#[allow(dead_code)]
#[path = "fixtures/modpacks/support.rs"]
mod support;
use patina_core::{
    download::Downloader,
    modpack::{PackInstallChoices, PackService},
    prepare::Launcher,
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use support::*;
use wiremock::{Mock, Request, Respond, ResponseTemplate, matchers::path};
#[derive(Clone)]
struct Delayed {
    arrivals: Arc<Mutex<Vec<Instant>>>,
    body: Vec<u8>,
    delay: Duration,
}
impl Respond for Delayed {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.arrivals.lock().unwrap().push(Instant::now());
        ResponseTemplate::new(200)
            .set_body_bytes(self.body.clone())
            .set_delay(self.delay)
    }
}
async fn baseline(setup: &Setup) -> String {
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    setup
        .service
        .install(
            pack("baseline", &[]),
            PackInstallChoices::new("Download target"),
            cancel,
            None,
        )
        .await
        .unwrap()
        .instance()
        .id
        .clone()
}
fn service(setup: &Setup, concurrency: usize) -> PackService {
    PackService::new(
        Arc::new(Launcher {
            data: setup.launcher.data.clone(),
            downloader: Downloader::new(
                setup.launcher.downloader.client().clone(),
                setup.launcher.downloader.mirror().clone(),
            )
            .with_concurrency(concurrency)
            .with_retry(1, Duration::ZERO),
        }),
        Default::default(),
    )
}
#[tokio::test]
async fn pack_transfer_overlaps_requests_without_exceeding_configured_concurrency() {
    let setup = Setup::new().await;
    let id = baseline(&setup).await;
    let arrivals = Arc::new(Mutex::new(vec![]));
    let delay = Duration::from_millis(250);
    let mut files = vec![];
    for ix in 0..4 {
        let body = format!("parallel body {ix}").into_bytes();
        let url = format!("/parallel/{ix}");
        Mock::given(path(url.clone()))
            .respond_with(Delayed {
                arrivals: arrivals.clone(),
                body: body.clone(),
                delay,
            })
            .mount(&setup.server)
            .await;
        files.push(file(
            &format!("mods/{ix}.jar"),
            &body,
            &format!("https://cdn.modrinth.com{url}"),
        ));
    }
    let start = Instant::now();
    let plan = service(&setup, 2)
        .plan_update(id, manifest_pack(files), Default::default())
        .await
        .unwrap();
    let times = arrivals.lock().unwrap().clone();
    let max = times
        .iter()
        .map(|t| {
            times
                .iter()
                .filter(|other| **other <= *t && t.duration_since(**other) < delay)
                .count()
        })
        .max()
        .unwrap();
    println!(
        "controlled scheduling: requests={} max_overlapping={} elapsed_ms={} expected_per_request_delay_ms=250 configured_concurrency=2",
        times.len(),
        max,
        start.elapsed().as_millis()
    );
    assert_eq!(times.len(), 4);
    assert_eq!(plan.normal_change_count(), 4);
    assert!(
        max >= 2,
        "pack stage must not serially await independent files"
    );
    assert!(max <= 2, "must respect the existing configured limit");
}
fn completed_body_exists(dir: &Path, body: &[u8]) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let p = entry.path();
            if p.is_dir() {
                completed_body_exists(&p, body)
            } else {
                !p.to_string_lossy().ends_with(".part")
                    && std::fs::read(&p).ok().as_deref() == Some(body)
            }
        })
}
fn has_part(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let p = entry.path();
            if p.is_dir() {
                has_part(&p)
            } else {
                p.to_string_lossy().ends_with(".part")
            }
        })
}
#[tokio::test]
async fn cancelled_pack_retry_reuses_completed_bytes_and_removes_incomplete_parts() {
    let setup = Setup::new().await;
    let id = baseline(&setup).await;
    setup.server.reset().await;
    let body = b"completed before cancellation";
    setup.serve("/fast.jar", body).await;
    Mock::given(path("/slow.jar"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"slow")
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&setup.server)
        .await;
    let fast = file("mods/fast.jar", body, "https://cdn.modrinth.com/fast.jar");
    let slow = file(
        "mods/slow.jar",
        b"slow",
        "https://cdn.modrinth.com/slow.jar",
    );
    let service = service(&setup, 2);
    let job = {
        let service = service.clone();
        let id = id.clone();
        let fast = fast.clone();
        tokio::spawn(async move {
            service
                .plan_update(id, manifest_pack(vec![fast, slow]), Default::default())
                .await
        })
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    while !completed_body_exists(&setup.launcher.data.cache(), body) {
        assert!(Instant::now() < deadline, "fast file did not complete");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    job.abort();
    assert!(matches!(job.await,Err(e) if e.is_cancelled()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!has_part(&setup.launcher.data.cache()));
    assert!(
        !setup
            .store
            .load(&id)
            .unwrap()
            .game_dir()
            .join("mods/fast.jar")
            .exists()
    );
    setup.server.reset().await;
    let retry = service
        .plan_update(id, manifest_pack(vec![fast]), Default::default())
        .await
        .unwrap();
    assert_eq!(retry.normal_change_count(), 1);
    assert!(
        setup.server.received_requests().await.unwrap().is_empty(),
        "completed, verified file must not be fetched again after cancellation"
    );
}

#[tokio::test]
async fn rate_limited_pack_does_not_automatically_retry_or_switch_source() {
    let setup = Setup::new().await;
    let id = baseline(&setup).await;
    setup.server.reset().await;
    Mock::given(path("/limited.jar"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "120"))
        .mount(&setup.server)
        .await;
    let mut entry = file(
        "mods/limited.jar",
        b"limited",
        "https://cdn.modrinth.com/limited.jar",
    );
    entry["downloads"] = serde_json::json!([
        "https://cdn.modrinth.com/limited.jar",
        "https://cdn.modrinth.com/other.jar"
    ]);
    let result = setup
        .service
        .plan_update(id, manifest_pack(vec![entry]), Default::default())
        .await;
    assert!(result.is_err());
    let requests = setup.server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "429 must be surfaced, not bypassed through retries or alternate URLs"
    );
}

#[tokio::test]
async fn repeated_online_preview_reuses_verified_archive_without_refetch() {
    let setup = Setup::new().await;
    let first = online_pack(&setup, "archive-project", "v1").await;
    let second = online_pack(&setup, "archive-project", "v1").await;
    assert_eq!(first.digest(), second.digest());
    let requests = setup.server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "unchanged hash-addressed archive should not download again on preview retry"
    );
}

#[tokio::test]
async fn author_alternate_is_tried_without_accepting_wrong_content() {
    let setup = Setup::new().await;
    let id = baseline(&setup).await;
    setup.server.reset().await;
    setup.serve("/bad.jar", b"wrong").await;
    setup.serve("/good.jar", b"correct").await;
    let mut entry = file(
        "mods/alternate.jar",
        b"correct",
        "https://cdn.modrinth.com/bad.jar",
    );
    entry["downloads"] = serde_json::json!([
        "https://cdn.modrinth.com/bad.jar",
        "https://cdn.modrinth.com/good.jar"
    ]);
    let plan = setup
        .service
        .plan_update(id, manifest_pack(vec![entry]), Default::default())
        .await
        .unwrap();
    assert_eq!(plan.normal_change_count(), 1);
    assert_eq!(setup.server.received_requests().await.unwrap().len(), 2);
}
#[tokio::test]
async fn cached_bytes_never_bypass_the_manifest_sha512() {
    let setup = Setup::new().await;
    let id = baseline(&setup).await;
    setup.server.reset().await;
    setup.serve("/sha512.jar", b"actual").await;
    let mut entry = file(
        "mods/sha512.jar",
        b"actual",
        "https://cdn.modrinth.com/sha512.jar",
    );
    entry["hashes"]["sha512"] = serde_json::json!("0".repeat(128));
    assert!(
        setup
            .service
            .plan_update(
                id.clone(),
                manifest_pack(vec![entry.clone()]),
                Default::default()
            )
            .await
            .is_err()
    );
    assert!(
        !setup
            .store
            .load(&id)
            .unwrap()
            .game_dir()
            .join("mods/sha512.jar")
            .exists()
    );
    assert!(
        setup
            .service
            .plan_update(id, manifest_pack(vec![entry]), Default::default())
            .await
            .is_err()
    );
}
