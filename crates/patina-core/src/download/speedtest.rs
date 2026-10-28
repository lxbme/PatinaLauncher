use std::time::{Duration, Instant};

use super::mirror::Mirror;
use crate::config::{DownloadConfig, SourceChoice};

pub const PROBE_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
/// Per-mirror probe timeout used by the launcher and the dev CLI.
pub const SPEEDTEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Time a full GET of the rewritten probe URL; `None` on non-2xx, error or timeout.
pub async fn measure(
    client: &reqwest::Client,
    mirror: &Mirror,
    probe_url: &str,
    timeout: Duration,
) -> Option<Duration> {
    let url = mirror.rewrite(probe_url);
    let start = Instant::now();
    let fut = async {
        let resp = client.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.bytes().await.ok()?;
        Some(())
    };
    tokio::time::timeout(timeout, fut).await.ok()??;
    Some(start.elapsed())
}

/// Measure all candidates concurrently, in candidate order.
pub async fn measure_all(
    client: &reqwest::Client,
    candidates: &[Mirror],
    probe_url: &str,
    timeout: Duration,
) -> Vec<(Mirror, Option<Duration>)> {
    let times = futures::future::join_all(
        candidates
            .iter()
            .map(|m| measure(client, m, probe_url, timeout)),
    )
    .await;
    candidates.iter().cloned().zip(times).collect()
}

/// The fastest successful measurement; `None` if every probe failed.
pub fn fastest(results: &[(Mirror, Option<Duration>)]) -> Option<Mirror> {
    results
        .iter()
        .filter_map(|(m, t)| t.map(|t| (m, t)))
        .min_by_key(|(_, t)| *t)
        .map(|(m, _)| m.clone())
}

/// Measure all candidates concurrently; fastest wins, `None` if all fail.
pub async fn pick_fastest(
    client: &reqwest::Client,
    candidates: &[Mirror],
    probe_url: &str,
    timeout: Duration,
) -> Option<Mirror> {
    fastest(&measure_all(client, candidates, probe_url, timeout).await)
}

/// `None` means Auto without a stored pick: the caller must speed-test first.
pub fn resolve_choice(cfg: &DownloadConfig) -> Option<Mirror> {
    match cfg.source {
        SourceChoice::Official => Some(Mirror::official()),
        SourceChoice::Bmclapi => Some(Mirror::bmclapi()),
        SourceChoice::Auto => cfg.auto_pick.as_deref().and_then(Mirror::by_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn server(status: u16, delay: Duration) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/probe"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string("ok")
                    .set_delay(delay),
            )
            .mount(&s)
            .await;
        s
    }

    fn mirror(id: &str, s: &MockServer) -> Mirror {
        Mirror {
            id: id.into(),
            rules: vec![("https://probe.invalid".into(), s.uri())],
        }
    }

    #[tokio::test]
    async fn picks_fastest_and_falls_back() {
        let slow = server(200, Duration::from_millis(300)).await;
        let fast = server(200, Duration::ZERO).await;
        let probe = "https://probe.invalid/probe";
        let ms = mirror("slow", &slow);
        let mf = mirror("fast", &fast);
        let c = reqwest::Client::new();
        let t = Duration::from_secs(2);
        let picked = pick_fastest(&c, &[ms.clone(), mf.clone()], probe, t).await;
        assert_eq!(picked.unwrap().id, "fast");
        let all = measure_all(&c, &[ms, mf], probe, t).await;
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0.id, "slow");
        assert!(all.iter().all(|(_, t)| t.is_some()));

        let bad1 = server(500, Duration::ZERO).await;
        let bad2 = server(500, Duration::ZERO).await;
        let cands = [mirror("b1", &bad1), mirror("b2", &bad2)];
        assert_eq!(pick_fastest(&c, &cands, probe, t).await, None);
    }

    #[tokio::test]
    async fn measure_times_out() {
        let slow = server(200, Duration::from_millis(1500)).await;
        let c = reqwest::Client::new();
        let r = measure(
            &c,
            &mirror("s", &slow),
            "https://probe.invalid/probe",
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(r, None);
    }

    #[test]
    fn resolves_config_choice() {
        let mut c = DownloadConfig::default();
        assert_eq!(resolve_choice(&c), None);
        c.auto_pick = Some("bmclapi".into());
        assert_eq!(resolve_choice(&c).unwrap().id, "bmclapi");
        c.source = SourceChoice::Official;
        assert_eq!(resolve_choice(&c).unwrap().id, "official");
    }
}
