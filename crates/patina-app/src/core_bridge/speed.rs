//! Speed tests of the download sources: what probes them ([`SpeedTest`],
//! injectable so tests can probe local servers), and the result the
//! [`Core`](super::Core) keeps of the last one ([`SpeedTestResult`]).

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use patina_core::download::{Mirror, PROBE_URL, SPEEDTEST_TIMEOUT, measure_all};
use patina_core::prepare::Launcher;
use rust_i18n::t;

/// Probes the download sources. Views reach it through `Core.speed_test`
/// (see [`Core::speed_test_now`](super::Core::speed_test_now)).
pub trait SpeedTest: Send + Sync + 'static {
    /// The latency of each source, in candidate order (`None`: it failed or
    /// timed out), probed through `launcher`'s HTTP client. Network I/O:
    /// poll it on the runtime.
    fn measure(
        &self,
        launcher: Arc<Launcher>,
    ) -> BoxFuture<'static, Vec<(Mirror, Option<Duration>)>>;
}

/// The real probe: the official source and BMCLAPI, each fetching the
/// version manifest within [`SPEEDTEST_TIMEOUT`].
pub struct MirrorProbe;

impl SpeedTest for MirrorProbe {
    fn measure(
        &self,
        launcher: Arc<Launcher>,
    ) -> BoxFuture<'static, Vec<(Mirror, Option<Duration>)>> {
        async move {
            let candidates = [Mirror::official(), Mirror::bmclapi()];
            let client = launcher.downloader.client();
            measure_all(client, &candidates, PROBE_URL, SPEEDTEST_TIMEOUT).await
        }
        .boxed()
    }
}

/// One speed test: when it ran and what each source answered.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeedTestResult {
    pub at: chrono::DateTime<chrono::Local>,
    /// Mirror id and latency, in the order probed; `None`: no answer in
    /// time.
    pub latencies: Vec<(String, Option<Duration>)>,
}

impl SpeedTestResult {
    /// The result of the probes in `results`, timed now.
    pub fn new(results: &[(Mirror, Option<Duration>)]) -> Self {
        Self {
            at: chrono::Local::now(),
            latencies: (results.iter())
                .map(|(mirror, latency)| (mirror.id.clone(), *latency))
                .collect(),
        }
    }

    /// What source `id` answered: `None` when it was not probed,
    /// `Some(None)` when it did not answer in time.
    pub fn latency(&self, id: &str) -> Option<Option<Duration>> {
        (self.latencies.iter())
            .find(|(mirror, _)| mirror == id)
            .map(|(_, latency)| *latency)
    }

    /// The source that answered fastest, if any did.
    pub fn fastest(&self) -> Option<&str> {
        (self.latencies.iter())
            .filter_map(|(id, latency)| latency.map(|l| (id, l)))
            .min_by_key(|(_, latency)| *latency)
            .map(|(id, _)| id.as_str())
    }
}

/// The name of download source `id` (its id when it is not a built-in one).
pub fn mirror_name(id: &str) -> String {
    match id {
        "official" => t!("mirror.official").to_string(),
        "bmclapi" => t!("mirror.bmclapi").to_string(),
        other => other.to_string(),
    }
}

/// `85 ms`, or that the source did not answer in time.
pub fn latency_text(latency: Option<Duration>) -> String {
    match latency {
        Some(latency) => t!("mirror.latency_ms", ms = latency.as_millis()).to_string(),
        None => t!("mirror.timeout").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_reads_latencies_and_the_fastest() {
        let ms = Duration::from_millis;
        let mirror = |id: &str| Mirror {
            id: id.into(),
            rules: Vec::new(),
        };
        let result = SpeedTestResult::new(&[
            (mirror("official"), Some(ms(300))),
            (mirror("bmclapi"), Some(ms(80))),
            (mirror("other"), None),
        ]);
        assert_eq!(result.latency("bmclapi"), Some(Some(ms(80))));
        assert_eq!(result.latency("other"), Some(None));
        assert_eq!(result.latency("nowhere"), None);
        assert_eq!(result.fastest(), Some("bmclapi"));
        let none = SpeedTestResult::new(&[(mirror("official"), None)]);
        assert_eq!(none.fastest(), None);
    }
}
