//! L2 verification through the same partitioned lanes as the rendered kernel config.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use probe_config::{CoreConfig, PayloadScope, QualitySection, VerifySection};
use probe_dns::GeoClient;
use probe_domain::ExitIdentity;
use probe_mihomo::{exit, lanes, Controller};
use probe_source::Role;
use serde_json::Value;

use crate::{Job, NodeTester, NodeVerdict, Verdict};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Injectable traffic boundary; every data request must use its assigned proxy lane.
pub trait TrafficProbe: Send + Sync {
    fn select<'a>(&'a self, lane: u16, name: &'a str) -> BoxFuture<'a, Result<(), String>>;
    fn payload<'a>(
        &'a self,
        port: u16,
        url: &'a str,
        timeout: Duration,
        cap: usize,
    ) -> BoxFuture<'a, Result<exit::PayloadMeasurement, String>>;
    fn trace<'a>(
        &'a self,
        port: u16,
        url: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<ExitIdentity, String>>;
    fn target_available<'a>(&'a self, url: &'a str, timeout: Duration) -> BoxFuture<'a, bool>;
}

struct MihomoTraffic(Controller);

impl TrafficProbe for MihomoTraffic {
    fn select<'a>(&'a self, lane: u16, name: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.0
                .select(&lanes::lane_group(lane), name)
                .await
                .map_err(|_| "lane selection failed".into())
        })
    }
    fn payload<'a>(
        &'a self,
        port: u16,
        url: &'a str,
        timeout: Duration,
        cap: usize,
    ) -> BoxFuture<'a, Result<exit::PayloadMeasurement, String>> {
        Box::pin(exit::fetch_measurement(port, url, timeout, cap))
    }
    fn trace<'a>(
        &'a self,
        port: u16,
        url: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<ExitIdentity, String>> {
        Box::pin(exit::egress(port, url, timeout))
    }
    fn target_available<'a>(&'a self, url: &'a str, timeout: Duration) -> BoxFuture<'a, bool> {
        Box::pin(exit::target_available(url, timeout))
    }
}

/// One bounded throughput measurement (G07 quality sampling).
#[derive(Debug, Clone, PartialEq)]
pub struct ThroughputSample {
    pub source: String,
    pub fingerprint: String,
    pub bytes: u64,
    pub elapsed_ms: f64,
}

pub struct VerifiedTester {
    inner: Arc<dyn NodeTester>,
    traffic: Arc<dyn TrafficProbe>,
    config: VerifySection,
    assignments: HashMap<String, (u16, u16)>,
    lane_locks: Vec<tokio::sync::Mutex<()>>,
    target_health: Mutex<HashMap<String, Arc<tokio::sync::OnceCell<bool>>>>,
    exit_budget: AtomicUsize,
    geo: GeoClient,
    /// G07 quality sampling policy; disabled unless the caller opts in.
    quality: QualitySection,
}

impl VerifiedTester {
    /// Bind real traffic checks to the exact proxy ordering rendered by build_config.
    pub fn new(
        inner: Arc<dyn NodeTester>,
        controller: Controller,
        core: &CoreConfig,
        proxies: &[Value],
        config: &VerifySection,
    ) -> Self {
        Self::with_traffic(
            inner,
            Arc::new(MihomoTraffic(controller)),
            core,
            proxies,
            config,
        )
    }

    /// Construct the same verifier against an injected traffic implementation for tests.
    pub fn with_traffic(
        inner: Arc<dyn NodeTester>,
        traffic: Arc<dyn TrafficProbe>,
        core: &CoreConfig,
        proxies: &[Value],
        config: &VerifySection,
    ) -> Self {
        let ports = lanes::lane_ports(core);
        let assignments = proxies
            .iter()
            .enumerate()
            .filter_map(|(index, proxy)| {
                let name = proxy.get("name")?.as_str()?.to_string();
                let lane = index % ports.len();
                Some((name, (lane as u16, ports[lane])))
            })
            .collect();
        Self {
            inner,
            traffic,
            config: config.clone(),
            assignments,
            lane_locks: ports.iter().map(|_| tokio::sync::Mutex::new(())).collect(),
            target_health: Mutex::new(HashMap::new()),
            exit_budget: AtomicUsize::new(0),
            geo: config
                .geo_url
                .as_deref()
                .map(GeoClient::with_url)
                .unwrap_or_default(),
            quality: QualitySection::default(),
        }
    }

    /// Opt into bounded throughput sampling (`quality` section).
    pub fn with_quality(mut self, quality: QualitySection) -> Self {
        self.quality = quality;
        self
    }

    /// Memoize target reachability per round without holding a mutex during IO.
    async fn target_is_up(&self, url: &str, timeout: Duration) -> bool {
        let state = {
            let mut targets = self.target_health.lock().unwrap();
            targets
                .entry(url.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
                .clone()
        };
        *state
            .get_or_init(|| self.traffic.target_available(url, timeout))
            .await
    }

    /// Verify one L1 success while holding only its own lane's selector lock.
    async fn verify(&self, mut verdict: NodeVerdict) -> NodeVerdict {
        if !verdict.is_alive() {
            return verdict;
        }
        // `scope = chain` is Python's rule (only nodes that dial through a
        // front); `scope = all` also makes a direct node prove a real TLS
        // page, which is the only way a delay-passing node whose handshakes
        // all fail stops looking healthy.
        let payload = self.config.chain_payload.enabled
            && (self.config.chain_payload.scope == PayloadScope::All
                || matches!(verdict.job.role, Role::Front | Role::Chain));
        let trace = self.config.enabled && verdict.job.role != Role::Front;
        if !payload && !trace {
            return verdict;
        }
        if trace
            && self.config.max_nodes > 0
            && self.exit_budget.fetch_add(1, Ordering::Relaxed) >= self.config.max_nodes
        {
            verdict.verification_error = Some("verification_limit".into());
            return verdict;
        }
        let Some(&(lane, port)) = self.assignments.get(&verdict.job.proxy_name) else {
            verdict.verification_error = Some("lane_missing".into());
            return verdict;
        };
        let _guard = self.lane_locks[lane as usize].lock().await;
        if self
            .traffic
            .select(lane, &verdict.job.proxy_name)
            .await
            .is_err()
        {
            verdict.verification_error = Some("lane_selection_failed".into());
            return verdict;
        }
        if payload && !self.verify_payload(port, &mut verdict).await {
            return verdict;
        }
        if trace {
            match self
                .traffic
                .trace(
                    port,
                    &self.config.trace_url,
                    Duration::from_secs(self.config.timeout_s),
                )
                .await
            {
                Ok(identity) => {
                    verdict.exit = Some(identity);
                    self.apply_country_policy(&mut verdict);
                }
                Err(_) => verdict.verification_error = Some("exit_unverified".into()),
            }
        }
        verdict
    }

    /// Distinguish a broken proxy payload path from an unavailable verification target.
    async fn verify_payload(&self, port: u16, verdict: &mut NodeVerdict) -> bool {
        let config = &self.config.chain_payload;
        let timeout = Duration::from_secs(config.timeout_s);
        match self
            .traffic
            .payload(port, &config.url, timeout, config.max_bytes)
            .await
        {
            Ok(measured) => {
                verdict.verification_bytes += measured.bytes;
                true
            }
            Err(_) => {
                if self.target_is_up(&config.url, timeout).await {
                    let reason = if verdict.job.role == Role::Front {
                        "front_dead"
                    } else {
                        "payload_fail"
                    };
                    verdict.verdict = Verdict::Dead {
                        kind: reason.into(),
                        message: "real payload verification failed".into(),
                    };
                } else {
                    verdict.verification_error = Some("payload_target_unavailable".into());
                }
                false
            }
        }
    }

    /// Country filtering applies to exits, not transit nodes; missing metadata stays unknown.
    fn apply_country_policy(&self, verdict: &mut NodeVerdict) {
        let Some(identity) = verdict.exit.as_ref() else {
            return;
        };
        if identity
            .ip
            .as_deref()
            .and_then(|ip| ip.parse::<std::net::IpAddr>().ok())
            .is_none()
        {
            verdict.verification_error = Some("exit_unverified".into());
            return;
        }
        if verdict.job.variant == probe_source::CAT_RELAY
            || self.config.exclude_countries.is_empty()
        {
            verdict.verification_error = None;
            return;
        }
        let Some(country) = identity
            .country
            .as_deref()
            .filter(|country| probe_domain::geo::is_country_code(country))
        else {
            verdict.verification_error = Some("country_unverified".into());
            return;
        };
        if self
            .config
            .exclude_countries
            .iter()
            .any(|excluded| excluded == country)
        {
            verdict.verdict = Verdict::Dead {
                kind: format!("exit_{}", country.to_ascii_lowercase()),
                message: "exit country excluded by policy".into(),
            };
        }
        verdict.verification_error = None;
    }
}

impl VerifiedTester {
    /// G07 quality sampling: one bounded transfer for a spread subset of the
    /// round's alive nodes.
    ///
    /// "Spread" means every k-th node of the sorted alive list, not the first
    /// k: the first k nodes of a subscription are usually one provider's
    /// cheapest machines, and a sample that only ever sees those is not a
    /// sample of the round. Failures are skipped, never turned into verdicts --
    /// a throughput probe that times out says nothing about whether the node
    /// works, and the round already decided that.
    pub async fn measure_throughput(&self, verdicts: &[NodeVerdict]) -> Vec<ThroughputSample> {
        let quality = &self.quality;
        if !quality.enabled || quality.sample_size == 0 {
            return Vec::new();
        }
        let mut candidates: Vec<&NodeVerdict> = verdicts
            .iter()
            .filter(|verdict| verdict.is_alive())
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }
        candidates.sort_by(|left, right| {
            (left.job.source_id.as_str(), left.job.fingerprint.as_str())
                .cmp(&(right.job.source_id.as_str(), right.job.fingerprint.as_str()))
        });
        let step = (candidates.len() / quality.sample_size).max(1);
        let mut samples = Vec::new();
        for verdict in candidates.iter().step_by(step) {
            if samples.len() >= quality.sample_size {
                break;
            }
            let Some(&(lane, port)) = self.assignments.get(&verdict.job.proxy_name) else {
                continue;
            };
            let _guard = self.lane_locks[lane as usize].lock().await;
            if self
                .traffic
                .select(lane, &verdict.job.proxy_name)
                .await
                .is_err()
            {
                continue;
            }
            if let Ok(measured) = self
                .traffic
                .payload(
                    port,
                    &self.config.chain_payload.url,
                    Duration::from_secs(quality.timeout_s),
                    quality.max_bytes,
                )
                .await
            {
                if measured.complete && measured.bytes > 0 {
                    samples.push(ThroughputSample {
                        source: verdict.job.source_id.clone(),
                        fingerprint: verdict.job.fingerprint.clone(),
                        bytes: measured.bytes,
                        elapsed_ms: measured.elapsed_ms,
                    });
                }
            }
        }
        samples
    }
}

impl NodeTester for VerifiedTester {
    fn test<'a>(&'a self, job: Job) -> BoxFuture<'a, NodeVerdict> {
        Box::pin(async move { self.verify(self.inner.test(job).await).await })
    }

    fn sample_throughput<'a>(
        &'a self,
        verdicts: &'a [NodeVerdict],
    ) -> BoxFuture<'a, Vec<ThroughputSample>> {
        Box::pin(async move { self.measure_throughput(verdicts).await })
    }

    /// Batch unique exits instead of issuing one rate-limited geo request per node.
    fn enrich<'a>(&'a self, verdicts: &'a mut [NodeVerdict]) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if !self.config.enabled {
                return;
            }
            let ips: BTreeSet<_> = verdicts
                .iter()
                .filter_map(|verdict| verdict.exit.as_ref())
                .filter(|identity| identity.asn.is_none() || identity.country.is_none())
                .filter_map(|identity| identity.ip.clone())
                .collect();
            let ips: Vec<_> = ips.into_iter().collect();
            let mut metadata = HashMap::new();
            for batch in ips.chunks(90) {
                match tokio::time::timeout(
                    Duration::from_secs(self.config.geo_timeout_s),
                    self.geo.fetch_batch(batch),
                )
                .await
                {
                    Ok(Ok(rows)) => {
                        for row in rows {
                            if batch.contains(&row.ip) {
                                metadata.insert(row.ip.clone(), row);
                            }
                        }
                    }
                    _ => tracing::warn!(addresses = batch.len(), "exit metadata lookup incomplete"),
                }
            }
            for verdict in verdicts {
                let Some(identity) = verdict.exit.as_mut() else {
                    continue;
                };
                if let Some(row) = identity.ip.as_ref().and_then(|ip| metadata.get(ip)) {
                    if identity.country.is_none() {
                        identity.country = row
                            .country
                            .as_ref()
                            .map(|country| country.to_ascii_uppercase())
                            .filter(|country| probe_domain::geo::is_country_code(country));
                    }
                    if identity.asn.is_none() {
                        identity.asn = row
                            .asn
                            .as_deref()
                            .and_then(probe_domain::geo::normalize_asn);
                    }
                }
                if verdict.verification_error.as_deref() == Some("country_unverified") {
                    self.apply_country_policy(verdict);
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
