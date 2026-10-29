//! Installing a Fabric loader profile.

pub use super::InstalledProfile;
use super::list::LoaderError;
use super::{InstallPhase, ProgressSink, report};
use crate::download::{Downloader, Progress};
use crate::fsutil::{is_safe_name, safe_join, write_atomic};
use crate::meta::{PartialVersionJson, RuleEnv};
use crate::paths::DataDirs;
use crate::resolve::resolve_libraries;

/// Fetches the Fabric profile for `mc` + `loader_version`, downloads its
/// libraries and, as the very last step, writes `versions/<id>/<id>.json`.
pub async fn install_fabric(
    dl: &Downloader,
    data: &DataDirs,
    mc: &str,
    loader_version: &str,
    progress: &ProgressSink,
) -> Result<InstalledProfile, LoaderError> {
    report(progress, InstallPhase::FetchMetadata, 0, 1);
    let url =
        format!("https://meta.fabricmc.net/v2/versions/loader/{mc}/{loader_version}/profile/json");
    let bytes = dl.fetch_bytes(&url, None).await?;
    let profile: PartialVersionJson = serde_json::from_slice(&bytes)?;
    if !is_safe_name(&profile.id) {
        return Err(LoaderError::UnsafeMetadata(profile.id));
    }
    report(progress, InstallPhase::FetchMetadata, 1, 1);

    let resolved = resolve_libraries(&profile.libraries, &RuleEnv::current(), &data.libraries());
    let total = resolved.jobs.len();
    report(progress, InstallPhase::DownloadLibraries, 0, total);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
    let run = dl.run(resolved.jobs, Some(tx));
    let forward = async {
        while let Some(p) = rx.recv().await {
            report(progress, InstallPhase::DownloadLibraries, p.done, p.total);
        }
    };
    let (res, ()) = tokio::join!(run, forward);
    res?;

    report(progress, InstallPhase::Finalize, 0, 1);
    let dir = safe_join(&data.versions(), &profile.id)
        .ok_or_else(|| LoaderError::UnsafeMetadata(profile.id.clone()))?;
    write_atomic(&dir.join(format!("{}.json", profile.id)), &bytes)?;
    report(progress, InstallPhase::Finalize, 1, 1);
    Ok(InstalledProfile {
        profile_id: profile.id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::Mirror;
    use crate::loader::InstallProgress;
    use crate::loader::forge::test_support::{FABRIC_HOSTS, offline_client};
    use serde_json::{Value, json};
    use sha1::{Digest, Sha1};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PROFILE: &str = include_str!("../../tests/fixtures/fabric_profile_1.21.1.json");

    /// Fabric hosts map to `server`; the real ones are unreachable.
    fn downloader(server: &MockServer) -> Downloader {
        let rules = FABRIC_HOSTS
            .iter()
            .map(|h| (h.to_string(), server.uri()))
            .collect();
        let mirror = Mirror {
            id: "fake".into(),
            rules,
        };
        Downloader::new(offline_client(), mirror).with_retry(1, Duration::ZERO)
    }

    /// Serves the profile and every library; library `fail` (if any) gets a 500.
    async fn serve(server: &MockServer, fail: Option<usize>) -> Value {
        let mut v: Value = serde_json::from_str(PROFILE).unwrap();
        for (i, lib) in v["libraries"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            let body = format!("fabric lib {i}").into_bytes();
            lib["sha1"] = json!(hex::encode(Sha1::digest(&body)));
            lib["size"] = json!(body.len());
            lib.as_object_mut().unwrap().remove("md5");
            lib.as_object_mut().unwrap().remove("sha256");
            lib.as_object_mut().unwrap().remove("sha512");
            let name = lib["name"].as_str().unwrap();
            let mut it = name.split(':');
            let (g, a, ver) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
            let p = format!("/{}/{a}/{ver}/{a}-{ver}.jar", g.replace('.', "/"));
            let resp = if fail == Some(i) {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200).set_body_bytes(body)
            };
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(resp)
                .mount(server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/v2/versions/loader/1.21.1/0.16.10/profile/json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(serde_json::to_vec(&v).unwrap()),
            )
            .mount(server)
            .await;
        v
    }

    #[tokio::test]
    async fn installs_fabric_profile() {
        let server = MockServer::start().await;
        let v = serve(&server, None).await;
        let id = v["id"].as_str().unwrap().to_string();
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = install_fabric(&downloader(&server), &data, "1.21.1", "0.16.10", &Some(tx))
            .await
            .unwrap();
        assert_eq!(out.profile_id, id);
        assert!(
            data.versions()
                .join(&id)
                .join(format!("{id}.json"))
                .is_file()
        );
        let asm = data.libraries().join("org/ow2/asm/asm/9.7.1/asm-9.7.1.jar");
        assert!(asm.is_file());
        let mut last: Option<InstallProgress> = None;
        while let Ok(p) = rx.try_recv() {
            last = Some(p);
        }
        assert_eq!(last.unwrap().phase, InstallPhase::Finalize);
    }

    #[tokio::test]
    async fn fabric_failure_writes_no_profile() {
        let server = MockServer::start().await;
        let v = serve(&server, Some(1)).await;
        let id = v["id"].as_str().unwrap().to_string();
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let r = install_fabric(&downloader(&server), &data, "1.21.1", "0.16.10", &None).await;
        assert!(r.is_err());
        assert!(
            !data
                .versions()
                .join(&id)
                .join(format!("{id}.json"))
                .exists()
        );
    }
}
