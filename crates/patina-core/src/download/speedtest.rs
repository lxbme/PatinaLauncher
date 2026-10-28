use std::time::{Duration, Instant};

use super::mirror::Mirror;
use crate::config::{DownloadConfig, SourceChoice};

pub const PROBE_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

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

/// Measure all candidates concurrently; fastest wins, all-fail yields official.
pub async fn pick_fastest(
    client: &reqwest::Client,
    candidates: &[Mirror],
    probe_url: &str,
    timeout: Duration,
) -> Mirror {
    let times = futures::future::join_all(
        candidates
            .iter()
            .map(|m| measure(client, m, probe_url, timeout)),
    )
    .await;
    candidates
        .iter()
        .zip(times)
        .filter_map(|(m, t)| t.map(|t| (m, t)))
        .min_by_key(|(_, t)| *t)
        .map(|(m, _)| m.clone())
        .unwrap_or_else(Mirror::official)
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
        assert_eq!(pick_fastest(&c, &[ms, mf], probe, t).await.id, "fast");

        let bad1 = server(500, Duration::ZERO).await;
        let bad2 = server(500, Duration::ZERO).await;
        let cands = [mirror("b1", &bad1), mirror("b2", &bad2)];
        assert_eq!(pick_fastest(&c, &cands, probe, t).await.id, "official");
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
