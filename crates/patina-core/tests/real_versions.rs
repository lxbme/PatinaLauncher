//! Network test: deserialize real Mojang version JSONs. Run with `--ignored`.

use std::time::Duration;

use patina_core::meta::{MANIFEST_URL, VersionJson, VersionManifest};

#[tokio::test]
#[ignore]
async fn real_versions_deserialize() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let manifest: VersionManifest = serde_json::from_slice(
        &client
            .get(MANIFEST_URL)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    let latest = manifest.latest.release.clone();
    for id in [latest.as_str(), "1.20.1", "1.12.2", "1.8.9"] {
        let entry = manifest
            .find(id)
            .unwrap_or_else(|| panic!("{id} not in manifest"));
        let bytes = client
            .get(&entry.url)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let v: VersionJson = serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(v.id, id);
        println!(
            "{id}: java {:?}, {} libraries",
            v.java_requirement(),
            v.libraries.len()
        );
    }
}
