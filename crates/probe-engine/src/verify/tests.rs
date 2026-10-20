use super::*;
use serde_json::json;

struct Passing;
impl NodeTester for Passing {
    fn test<'a>(&'a self, job: Job) -> BoxFuture<'a, NodeVerdict> {
        Box::pin(async { NodeVerdict::alive(job, 20) })
    }
}

#[derive(Default)]
struct FakeTraffic {
    selected: Mutex<HashMap<u16, String>>,
    calls: AtomicUsize,
    fail_trace: bool,
    fail_payload: bool,
    target_up: bool,
    banned: bool,
}

impl TrafficProbe for FakeTraffic {
    fn select<'a>(&'a self, lane: u16, name: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.selected.lock().unwrap().insert(lane, name.to_string());
            Ok(())
        })
    }
    fn payload<'a>(
        &'a self,
        _port: u16,
        _url: &'a str,
        _timeout: Duration,
        _cap: usize,
    ) -> BoxFuture<'a, Result<exit::PayloadMeasurement, String>> {
        Box::pin(async move {
            if self.fail_payload {
                return Err("payload failed".into());
            }
            Ok(exit::PayloadMeasurement {
                status: 200,
                bytes: 100,
                elapsed_ms: 5.0,
                complete: true,
            })
        })
    }
    fn trace<'a>(
        &'a self,
        port: u16,
        _url: &'a str,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<ExitIdentity, String>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if self.fail_trace {
                return Err("trace failed".into());
            }
            let names = self.selected.lock().unwrap();
            let name = &names[&(port - 20000)];
            Ok(ExitIdentity {
                ip: Some(
                    if name == "a" {
                        "203.0.113.1"
                    } else {
                        "203.0.113.2"
                    }
                    .into(),
                ),
                country: Some(if self.banned { "CN" } else { "US" }.into()),
                asn: Some("AS64512".into()),
                colo: None,
            })
        })
    }
    fn target_available<'a>(&'a self, _url: &'a str, _timeout: Duration) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.target_up })
    }
}

/// Construct a verifier with the exact lane membership implied by these proxy names.
fn verifier(traffic: Arc<FakeTraffic>, config: VerifySection) -> VerifiedTester {
    let core = CoreConfig {
        lanes: 1,
        base_port: 20000,
        ..CoreConfig::defaults()
    };
    VerifiedTester::with_traffic(
        Arc::new(Passing),
        traffic,
        &core,
        &[json!({"name":"a"}), json!({"name":"b"})],
        &config,
    )
}

/// Return a direct fixture job.
fn job(name: &str) -> Job {
    Job::new("air", name, name, "direct", "192.0.2.1")
}

/// Selector switches and trace reads cannot interleave on a shared lane.
#[tokio::test]
async fn concurrent_checks_do_not_cross_contaminate_a_lane() {
    let verify = verifier(Arc::new(FakeTraffic::default()), VerifySection::default());
    let (first, second) = tokio::join!(verify.test(job("a")), verify.test(job("b")));
    assert!(first.is_alive() && second.is_alive());
    assert_eq!(first.exit.unwrap().ip.as_deref(), Some("203.0.113.1"));
    assert_eq!(second.exit.unwrap().ip.as_deref(), Some("203.0.113.2"));
}

/// Missing exit proof remains inconclusive even after a passing delay request.
#[tokio::test]
async fn a_trace_failure_is_not_a_trusted_alive_verdict() {
    let verify = verifier(
        Arc::new(FakeTraffic {
            fail_trace: true,
            ..Default::default()
        }),
        VerifySection::default(),
    );
    let verdict = verify.test(job("a")).await;
    assert!(!verdict.is_alive());
    assert_eq!(
        verdict.verification_error.as_deref(),
        Some("exit_unverified")
    );
    assert_eq!(
        verdict.to_result_row().verdict.as_deref(),
        Some("unverified")
    );
}

/// `scope = all`: a direct node proves a real page, and a failed fetch is a
/// verdict (payload_fail), not an "uncertain" flag.
///
/// This is the gap Python's `chain_payload` left: a node whose delay probe
/// answers but whose TLS handshakes all fail used to look healthy.
#[tokio::test]
async fn payload_scope_all_checks_a_direct_node_and_fails_it_honestly() {
    let mut config = VerifySection::default();
    config.chain_payload.scope = PayloadScope::All;
    let verdict = verifier(
        Arc::new(FakeTraffic {
            fail_payload: true,
            target_up: true,
            ..Default::default()
        }),
        config,
    )
    .test(job("a"))
    .await;
    assert_eq!(verdict.reason(), Some("payload_fail"));
    assert!(
        verdict.verification_error.is_none(),
        "a broken payload path is measured, not unknown"
    );
}

/// The default scope keeps Python's rule: direct nodes are not payload-checked.
#[tokio::test]
async fn payload_scope_chain_leaves_a_direct_node_unprobed() {
    let config = VerifySection::default();
    assert_eq!(config.chain_payload.scope, PayloadScope::Chain);
    let verdict = verifier(
        Arc::new(FakeTraffic {
            fail_payload: true,
            target_up: true,
            ..Default::default()
        }),
        config,
    )
    .test(job("a"))
    .await;
    assert!(
        verdict.is_alive(),
        "scope=chain must not spend real traffic on a direct node"
    );
}

/// Sampling spreads across the alive set and never probes a dead node.
#[tokio::test]
async fn throughput_sampling_spreads_and_skips_dead_nodes() {
    let traffic = Arc::new(FakeTraffic::default());
    let config = VerifySection::default();
    // The helper only knows two lanes' worth of names; this sample needs
    // four nodes, so build the tester with all four assignments.
    let core = CoreConfig {
        lanes: 1,
        base_port: 20000,
        ..CoreConfig::defaults()
    };
    let probes: Arc<dyn TrafficProbe> = Arc::clone(&traffic) as Arc<dyn TrafficProbe>;
    let tester = VerifiedTester::with_traffic(
        Arc::new(Passing),
        probes,
        &core,
        &[
            json!({"name":"a"}),
            json!({"name":"b"}),
            json!({"name":"c"}),
            json!({"name":"d"}),
        ],
        &config,
    )
    .with_quality(probe_config::QualitySection {
        enabled: true,
        sample_size: 2,
        max_bytes: 4096,
        timeout_s: 5,
    });
    let alive: Vec<NodeVerdict> = ["a", "b", "c", "d"]
        .iter()
        .map(|name| NodeVerdict::alive(job(name), 20))
        .collect();
    let mut dead = NodeVerdict::alive(job("e"), 20);
    dead.verdict = Verdict::Dead {
        kind: "timeout".into(),
        message: "nope".into(),
    };
    let mut verdicts = alive.clone();
    verdicts.push(dead);
    let samples = tester.sample_throughput(&verdicts).await;
    assert_eq!(samples.len(), 2, "sample_size caps the sample: {samples:?}");
    assert!(
        !samples.iter().any(|s| s.fingerprint == "e"),
        "a dead node is never probed"
    );
    assert!(samples.iter().all(|s| s.bytes > 0 && s.elapsed_ms > 0.0));
    let names: Vec<&str> = samples.iter().map(|s| s.fingerprint.as_str()).collect();
    assert_eq!(names, vec!["a", "c"], "every k-th node, not the first k");
    // Disabled means no traffic at all.
    let off = verifier(Arc::new(FakeTraffic::default()), config).with_quality(
        probe_config::QualitySection {
            enabled: false,
            sample_size: 2,
            max_bytes: 4096,
            timeout_s: 5,
        },
    );
    assert!(off.sample_throughput(&alive).await.is_empty());
}

/// An explicitly excluded exit is a measured policy failure, not missing metadata.
#[tokio::test]
async fn excluded_exit_countries_are_rejected() {
    let verify = verifier(
        Arc::new(FakeTraffic {
            banned: true,
            ..Default::default()
        }),
        VerifySection::default(),
    );
    let verdict = verify.test(job("a")).await;
    assert_eq!(verdict.reason(), Some("exit_cn"));
    assert!(verdict.verification_error.is_none());
    assert_eq!(verdict.to_result_row().country.as_deref(), Some("CN"));
}

/// Target outages do not become false failures of every front/chain.
#[tokio::test]
async fn payload_failures_require_a_healthy_control_target() {
    for target_up in [false, true] {
        let verify = verifier(
            Arc::new(FakeTraffic {
                fail_payload: true,
                target_up,
                ..Default::default()
            }),
            VerifySection::default(),
        );
        let verdict = verify.test(job("a").with_role(Role::Front, None)).await;
        assert!(!verdict.is_alive());
        if target_up {
            assert_eq!(verdict.reason(), Some("front_dead"));
            assert!(verdict.verification_error.is_none());
        } else {
            assert_eq!(
                verdict.verification_error.as_deref(),
                Some("payload_target_unavailable")
            );
        }
    }
}

/// Exceeding the explicit budget leaves the candidate unverified, never silently accepted.
#[tokio::test]
async fn exit_budget_cannot_publish_unverified_candidates() {
    let traffic = Arc::new(FakeTraffic::default());
    let verify = verifier(
        traffic.clone(),
        VerifySection {
            max_nodes: 1,
            ..Default::default()
        },
    );
    assert!(verify.test(job("a")).await.is_alive());
    let second = verify.test(job("b")).await;
    assert_eq!(
        second.verification_error.as_deref(),
        Some("verification_limit")
    );
    assert_eq!(traffic.calls.load(Ordering::SeqCst), 1);
}

/// Disabling verification causes no lane/trace traffic.
#[tokio::test]
async fn disabled_verification_preserves_the_l1_contract() {
    let traffic = Arc::new(FakeTraffic::default());
    let verify = verifier(
        traffic.clone(),
        VerifySection {
            enabled: false,
            ..Default::default()
        },
    );
    assert!(verify.test(job("a")).await.is_alive());
    assert_eq!(traffic.calls.load(Ordering::SeqCst), 0);
}

/// An assignment mismatch is an infrastructure issue rather than a node failure.
#[tokio::test]
async fn missing_lane_assignments_are_inconclusive() {
    let verify = verifier(Arc::new(FakeTraffic::default()), VerifySection::default());
    let verdict = verify.test(job("absent")).await;
    assert_eq!(verdict.verification_error.as_deref(), Some("lane_missing"));
}
