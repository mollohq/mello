//! SFU region choice (spec 10 section 7.6).
//!
//! The backend lists each SFU region and its public `/health` URL
//! (`get_sfu_regions`). At login and every 10 minutes the client measures the
//! HTTP round trip to each region on a Tokio task, off the command loop. The
//! fastest region that answered goes to `voice_join` and `start_stream` as
//! `preferred_region`. With no answer from any region the client sends no
//! preference and the backend uses its default.
//!
//! Method: three GET requests per region on one HTTP client, one after the
//! other, each with a 2 s timeout. The time of a request is from the send to
//! the response headers. The first request also opens the TCP and TLS
//! connection; the next two reuse it. The median of the requests that got a
//! 2xx answer is the region's round trip. Regions are measured at the same
//! time.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::events::Event;

/// Requests per region in one measurement.
pub const PROBES_PER_REGION: usize = 3;
/// Time limit of one request.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Time between two measurements.
pub const MEASURE_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// One SFU region from `get_sfu_regions`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SfuRegionEndpoint {
    pub region: String,
    pub health_url: String,
}

/// The measured round trip to one region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionRtt {
    pub region: String,
    pub rtt_ms: u32,
}

/// The regions that answered, fastest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionRanking {
    entries: Vec<RegionRtt>,
}

impl RegionRanking {
    /// Rank regions by the median of their successful requests. A region
    /// with no successful request is left out.
    pub fn from_samples(samples: Vec<(String, Vec<Duration>)>) -> Self {
        let mut entries: Vec<RegionRtt> = samples
            .into_iter()
            .filter_map(|(region, rtts)| {
                median(&rtts).map(|m| RegionRtt {
                    region,
                    rtt_ms: u32::try_from(m.as_millis()).unwrap_or(u32::MAX),
                })
            })
            .collect();
        // A tie keeps the backend's (alphabetical) order: the sort is stable.
        entries.sort_by_key(|e| e.rtt_ms);
        Self { entries }
    }

    /// The region to send as `preferred_region`: the fastest one. `None`
    /// when no region answered.
    pub fn preferred(&self) -> Option<&str> {
        self.entries.first().map(|e| e.region.as_str())
    }

    /// All regions that answered, fastest first.
    pub fn entries(&self) -> &[RegionRtt] {
        &self.entries
    }

    /// One log line: `eu-west 38 ms, us-east 112 ms`.
    pub fn describe(&self) -> String {
        if self.entries.is_empty() {
            return "no region answered".to_string();
        }
        self.entries
            .iter()
            .map(|e| format!("{} {} ms", e.region, e.rtt_ms))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The median of `samples`. With an even count, the higher of the two
/// middle values, so one fast outlier cannot make a region look closer.
pub fn median(samples: &[Duration]) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort();
    Some(sorted[sorted.len() / 2])
}

/// Measure one region: up to [`PROBES_PER_REGION`] requests, one after the
/// other. Returns the round trip of each request that got a 2xx answer.
pub async fn probe_region(http: &reqwest::Client, health_url: &str) -> Vec<Duration> {
    let mut rtts = Vec::with_capacity(PROBES_PER_REGION);
    for _ in 0..PROBES_PER_REGION {
        let started = Instant::now();
        match http.get(health_url).timeout(PROBE_TIMEOUT).send().await {
            Ok(resp) => {
                let rtt = started.elapsed();
                let ok = resp.status().is_success();
                // Read the body, so the next request can reuse the connection.
                let _ = resp.bytes().await;
                if ok {
                    rtts.push(rtt);
                } else {
                    log::debug!("SFU region probe {}: HTTP not 2xx", health_url);
                }
            }
            Err(e) => log::debug!("SFU region probe {} failed: {}", health_url, e),
        }
    }
    rtts
}

/// Measure every region at the same time.
pub async fn measure(http: &reqwest::Client, regions: &[SfuRegionEndpoint]) -> RegionRanking {
    let probes = regions
        .iter()
        .map(|r| async move { (r.region.clone(), probe_region(http, &r.health_url).await) });
    RegionRanking::from_samples(futures::future::join_all(probes).await)
}

/// The ranking that the measurer writes and the command loop reads.
pub type SharedRanking = Arc<Mutex<RegionRanking>>;

/// The preferred region in `ranking` now, if any.
pub fn preferred_region(ranking: &SharedRanking) -> Option<String> {
    ranking
        .lock()
        .ok()
        .and_then(|r| r.preferred().map(str::to_string))
}

/// Measures the regions now and every [`MEASURE_INTERVAL`] on a Tokio task.
/// Dropping it stops the task.
pub struct RegionMeasurer {
    task: tokio::task::JoinHandle<()>,
}

impl RegionMeasurer {
    /// Start measuring `regions`. Each measurement replaces `ranking`, is
    /// logged once at info level and goes to `event_tx` as
    /// `Event::SfuRegionsMeasured`.
    ///
    /// Must be called inside a Tokio runtime.
    pub fn start(
        regions: Vec<SfuRegionEndpoint>,
        ranking: SharedRanking,
        event_tx: std::sync::mpsc::Sender<Event>,
    ) -> Self {
        let task = tokio::spawn(async move {
            let http = match reqwest::Client::builder().build() {
                Ok(http) => http,
                Err(e) => {
                    log::warn!("SFU region measurement: no HTTP client: {}", e);
                    return;
                }
            };
            loop {
                let measured = measure(&http, &regions).await;
                log::info!(
                    "SFU region ranking: {} (preferred: {})",
                    measured.describe(),
                    measured.preferred().unwrap_or("none")
                );
                let entries = measured.entries().to_vec();
                if let Ok(mut shared) = ranking.lock() {
                    *shared = measured;
                }
                let _ = event_tx.send(Event::SfuRegionsMeasured { regions: entries });
                tokio::time::sleep(MEASURE_INTERVAL).await;
            }
        });
        Self { task }
    }
}

impl Drop for RegionMeasurer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn the_median_ignores_one_outlier() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[ms(40)]), Some(ms(40)));
        // The first request also opens the connection: it is the slow one.
        assert_eq!(median(&[ms(120), ms(40), ms(42)]), Some(ms(42)));
        assert_eq!(median(&[ms(40), ms(90)]), Some(ms(90)));
    }

    #[test]
    fn the_fastest_region_that_answered_is_preferred() {
        let ranking = RegionRanking::from_samples(vec![
            ("eu-west".into(), vec![ms(150), ms(110), ms(112)]),
            ("us-east".into(), vec![ms(90), ms(31), ms(30)]),
            ("ap-south".into(), vec![]),
        ]);
        assert_eq!(ranking.preferred(), Some("us-east"));
        assert_eq!(
            ranking.entries(),
            &[
                RegionRtt {
                    region: "us-east".into(),
                    rtt_ms: 31
                },
                RegionRtt {
                    region: "eu-west".into(),
                    rtt_ms: 112
                },
            ]
        );
        assert_eq!(ranking.describe(), "us-east 31 ms, eu-west 112 ms");
    }

    #[test]
    fn no_answer_means_no_preference() {
        let ranking = RegionRanking::from_samples(vec![
            ("eu-west".into(), vec![]),
            ("us-east".into(), vec![]),
        ]);
        assert_eq!(ranking.preferred(), None);
        assert_eq!(ranking.describe(), "no region answered");
    }

    /// An HTTP server on a local port that answers each request with
    /// `status` after `delay`, on a kept-alive connection.
    async fn health_server(status: &'static str, delay: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let url = format!(
            "http://{}/health",
            listener.local_addr().expect("listener has an address")
        );
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let mut pending = Vec::new();
                    loop {
                        let n = match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        pending.extend_from_slice(&buf[..n]);
                        while let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            pending.drain(..end + 4);
                            tokio::time::sleep(delay).await;
                            let body = r#"{"status":"ok"}"#;
                            let reply = format!(
                                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                                body.len()
                            );
                            if socket.write_all(reply.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        url
    }

    #[tokio::test]
    async fn the_measurement_ranks_by_round_trip_and_skips_failures() {
        let near = health_server("200 OK", ms(5)).await;
        let far = health_server("200 OK", ms(80)).await;
        let unhealthy = health_server("503 Service Unavailable", ms(0)).await;
        // A port with no server: every request fails at connect.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            format!("http://{}/health", l.local_addr().expect("addr"))
        };
        let regions = vec![
            SfuRegionEndpoint {
                region: "far".into(),
                health_url: far,
            },
            SfuRegionEndpoint {
                region: "near".into(),
                health_url: near,
            },
            SfuRegionEndpoint {
                region: "unhealthy".into(),
                health_url: unhealthy,
            },
            SfuRegionEndpoint {
                region: "closed".into(),
                health_url: closed,
            },
        ];
        let http = reqwest::Client::new();
        let ranking = measure(&http, &regions).await;
        let names: Vec<&str> = ranking
            .entries()
            .iter()
            .map(|e| e.region.as_str())
            .collect();
        assert_eq!(names, ["near", "far"], "{}", ranking.describe());
        assert!(ranking.entries()[1].rtt_ms >= 80, "{}", ranking.describe());
        assert_eq!(ranking.preferred(), Some("near"));
    }

    #[tokio::test]
    async fn a_slow_region_times_out_after_two_seconds() {
        let slow = health_server("200 OK", Duration::from_secs(5)).await;
        let http = reqwest::Client::new();
        let started = Instant::now();
        let rtts = probe_region(&http, &slow).await;
        assert!(rtts.is_empty());
        // Three requests, each cut at the timeout.
        let took = started.elapsed();
        assert!(
            took < PROBE_TIMEOUT * PROBES_PER_REGION as u32 + ms(1500),
            "{took:?}"
        );
    }

    /// Measure real regions: `MELLO_TEST_REGIONS=eu-west=https://...,us-east=https://...`.
    ///
    /// ```bash
    /// MELLO_TEST_REGIONS=eu-west=https://sfu-eu.m3llo.app:8080/health,us-east=https://sfu-us.m3llo.app:8080/health     ///   cargo test -p mello-core --lib -- --ignored --nocapture region_ranking_live
    /// ```
    #[tokio::test]
    #[ignore = "needs network: MELLO_TEST_REGIONS"]
    async fn region_ranking_live() {
        let Ok(spec) = std::env::var("MELLO_TEST_REGIONS") else {
            println!("[region-live] skipped: MELLO_TEST_REGIONS is not set");
            return;
        };
        let regions: Vec<SfuRegionEndpoint> = spec
            .split(',')
            .filter_map(|r| r.split_once('='))
            .map(|(region, url)| SfuRegionEndpoint {
                region: region.to_string(),
                health_url: url.to_string(),
            })
            .collect();
        let http = reqwest::Client::new();
        for _ in 0..3 {
            for r in &regions {
                let rtts: Vec<u128> = probe_region(&http, &r.health_url)
                    .await
                    .iter()
                    .map(|d| d.as_millis())
                    .collect();
                println!("[region-live] {} probes (ms): {:?}", r.region, rtts);
            }
            let ranking = measure(&http, &regions).await;
            println!(
                "[region-live] ranking: {} (preferred: {})",
                ranking.describe(),
                ranking.preferred().unwrap_or("none")
            );
        }
    }

    #[tokio::test]
    async fn the_measurer_stores_the_ranking_and_reports_it() {
        let near = health_server("200 OK", ms(1)).await;
        let ranking: SharedRanking = Arc::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let measurer = RegionMeasurer::start(
            vec![SfuRegionEndpoint {
                region: "eu-west".into(),
                health_url: near,
            }],
            Arc::clone(&ranking),
            tx,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let event = loop {
            if let Ok(ev) = rx.try_recv() {
                break ev;
            }
            assert!(Instant::now() < deadline, "no SfuRegionsMeasured event");
            tokio::time::sleep(ms(5)).await;
        };
        match event {
            Event::SfuRegionsMeasured { regions } => {
                assert_eq!(regions.len(), 1);
                assert_eq!(regions[0].region, "eu-west");
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert_eq!(preferred_region(&ranking).as_deref(), Some("eu-west"));
        drop(measurer);
    }
}
