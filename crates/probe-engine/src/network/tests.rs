use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

use probe_config::SourceSpec;
use probe_source::Fetcher;

use crate::collect::{collect, ChainContext};
use crate::{Job, Limits};

struct StaticSource(Vec<Value>);

impl Fetcher for StaticSource {
    /// Return fixture proxies without contacting an upstream service.
    fn fetch<'a>(&'a self, _source: &'a SourceSpec) -> BoxFuture<'a, Result<Vec<Value>, String>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

#[derive(Default)]
struct FakeDns {
    calls: AtomicUsize,
    fail_overseas: bool,
    fail_all: bool,
    negative: bool,
}

impl DnsLookup for FakeDns {
    /// Return distinct overlapping view answers, or an injected query failure.
    fn lookup<'a>(
        &'a self,
        _name: &'a str,
        qtype: u16,
        view: &'a DnsView,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<DnsAnswer, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let overseas = view.resolver.contains("cloudflare");
            if self.fail_all || (self.fail_overseas && overseas) {
                return Err("injected resolver failure".into());
            }
            let ips: Vec<String> = match (self.negative, qtype, overseas) {
                (true, _, _) => vec![],
                (false, TYPE_A, false) => vec!["192.0.2.1".into()],
                (false, TYPE_A, true) => vec!["192.0.2.1".into(), "192.0.2.2".into()],
                (false, TYPE_AAAA, true) => vec!["2001:db8::1".into()],
                _ => vec![],
            };
            Ok(DnsAnswer {
                negative: ips.is_empty(),
                ips,
                ttl_secs: 120,
                cname_chain: vec!["edge.example".into()],
            })
        })
    }
}

#[derive(Default)]
struct FakeCountries {
    calls: AtomicUsize,
    fail: bool,
}

impl CountryLookup for FakeCountries {
    /// Classify one documentation IP as restricted; unknown lookup failures stay errors.
    fn lookup<'a>(&'a self, ips: &'a [String]) -> BoxFuture<'a, Result<Vec<GeoRow>, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err("injected geo failure".into());
            }
            Ok(ips
                .iter()
                .map(|ip| GeoRow {
                    ip: ip.clone(),
                    country: Some(if ip == "192.0.2.1" { "CN" } else { "US" }.into()),
                    isp: Some("Documentation ISP".into()),
                    asn: None,
                })
                .collect())
        })
    }
}

/// Build a normal collection through the actual source preparation path.
async fn fixture_collection(servers: &[&str]) -> Collected {
    let proxies = servers
        .iter()
        .enumerate()
        .map(|(index, server)| {
            json!({
                "name": format!("node-{index}"), "type": "vless", "server": server, "port": 443,
                "uuid": "00000000-0000-0000-0000-000000000001", "tls": true,
            })
        })
        .collect();
    let source = SourceSpec {
        key: "test".into(),
        kind: "sub".into(),
        name: "fixture".into(),
        label: "fixture".into(),
        enabled: true,
        relay: false,
        direct: true,
        chain: true,
        export: true,
    };
    collect(
        &StaticSource(proxies),
        &[source],
        true,
        ChainContext::off(None),
    )
    .await
}

/// Supply an isolated ledger and fully local fake DNS/geo clients.
fn services(dns: Arc<FakeDns>, countries: Arc<FakeCountries>) -> NetworkServices {
    NetworkServices {
        storage: Arc::new(Mutex::new(Storage::open_in_memory().unwrap())),
        dns,
        countries,
    }
}

/// Disable the geographical policy while retaining all DNS views.
fn no_entry_filter() -> VerifySection {
    VerifySection {
        entry_check: false,
        ..Default::default()
    }
}

/// Resolve one node's three distinct addresses while preserving its original identity.
#[tokio::test]
async fn both_views_expand_to_unique_ipv4_and_ipv6_jobs() {
    let collected = fixture_collection(&["origin.example"]).await;
    let fingerprint = collected.jobs[0].fingerprint.clone();
    let network = services(
        Arc::new(FakeDns::default()),
        Arc::new(FakeCountries::default()),
    );
    let expanded = prepare_network(
        collected,
        &DnsSection::default(),
        &no_entry_filter(),
        &network,
        &RoundCtx::new(Limits::from_concurrency(4)),
    )
    .await
    .unwrap();
    let ips: Vec<_> = expanded
        .jobs
        .iter()
        .map(|job| job.test_ip.as_deref().unwrap())
        .collect();
    assert_eq!(ips, ["192.0.2.1", "192.0.2.2", "2001:db8::1"]);
    assert!(expanded
        .jobs
        .iter()
        .all(|job| job.fingerprint == fingerprint));
    assert!(expanded.jobs.iter().all(|job| job.display == "node-0"));
    assert!(expanded
        .jobs
        .iter()
        .all(|job| job.original_server.as_deref() == Some("origin.example")));
    assert!(expanded
        .proxies
        .iter()
        .all(|proxy| proxy["servername"] == "origin.example"));
    assert_eq!(
        expanded.originals[&("test".into(), fingerprint)]["server"],
        "origin.example"
    );
    assert_eq!(expanded.network.queries, 4);
}

/// Positive and negative per-family entries avoid repeated resolver requests.
#[tokio::test]
async fn cache_hits_cover_positive_and_negative_answers() {
    for negative in [false, true] {
        let dns = Arc::new(FakeDns {
            negative,
            ..Default::default()
        });
        let network = services(dns.clone(), Arc::new(FakeCountries::default()));
        for round in 0..2 {
            let expanded = prepare_network(
                fixture_collection(&["origin.example"]).await,
                &DnsSection::default(),
                &no_entry_filter(),
                &network,
                &RoundCtx::new(Limits::from_concurrency(4)),
            )
            .await
            .unwrap();
            assert_eq!(expanded.network.cache_hits, round * 4);
            assert!(expanded.jobs.iter().all(|job| !job.dns_incomplete));
        }
        assert_eq!(dns.calls.load(Ordering::SeqCst), 4);
    }
}

/// Disabling TTL caching does not accidentally reuse or refresh a previous entry.
#[tokio::test]
async fn zero_cache_window_forces_fresh_queries() {
    let dns = Arc::new(FakeDns::default());
    let network = services(dns.clone(), Arc::new(FakeCountries::default()));
    let cfg = DnsSection {
        cache_hours: 0,
        ..Default::default()
    };
    for _ in 0..2 {
        let expanded = prepare_network(
            fixture_collection(&["origin.example"]).await,
            &cfg,
            &no_entry_filter(),
            &network,
            &RoundCtx::new(Limits::from_concurrency(4)),
        )
        .await
        .unwrap();
        assert_eq!(expanded.network.cache_hits, 0);
    }
    assert_eq!(dns.calls.load(Ordering::SeqCst), 8);
}

/// Resolver and ECS changes cannot reuse answers from a different DNS perspective.
#[test]
fn cache_keys_include_every_answer_affecting_setting_without_plaintext() {
    let cfg = DnsSection::default();
    let view = &cfg.views["cn"];
    let base = dns_cache_key("origin.example", TYPE_A, "cn", view);
    let mut changed = view.clone();
    changed.resolver = "https://dns.example/dummy-private-path".into();
    assert_ne!(
        base,
        dns_cache_key("origin.example", TYPE_A, "cn", &changed)
    );
    changed = view.clone();
    changed.ecs = Some("192.0.2.0/24".into());
    assert_ne!(
        base,
        dns_cache_key("origin.example", TYPE_A, "cn", &changed)
    );
    assert_ne!(base, dns_cache_key("origin.example", TYPE_AAAA, "cn", view));
    assert_eq!(base.len(), 64);
    assert!(base.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(base, dns_cache_key("ORIGIN.EXAMPLE.", TYPE_A, "cn", view));
}

/// All-restricted literals are exclusions, not failed or dialled proxy candidates.
#[tokio::test]
async fn restricted_entries_have_no_kernel_proxy() {
    let dns = Arc::new(FakeDns::default());
    let countries = Arc::new(FakeCountries::default());
    let network = services(dns.clone(), countries.clone());
    let expanded = prepare_network(
        fixture_collection(&["192.0.2.1", "192.0.2.2"]).await,
        &DnsSection::default(),
        &VerifySection::default(),
        &network,
        &RoundCtx::new(Limits::from_concurrency(4)),
    )
    .await
    .unwrap();
    assert_eq!(expanded.jobs.len(), 2);
    assert_eq!(expanded.proxies.len(), 1);
    assert_eq!(
        expanded.jobs[0].excluded_reason.as_deref(),
        Some("entry_cn")
    );
    assert!(expanded.jobs[1].excluded_reason.is_none());
    assert_eq!(expanded.network.excluded_nodes, 1);
    assert_eq!(dns.calls.load(Ordering::SeqCst), 0);
    assert_eq!(countries.calls.load(Ordering::SeqCst), 1);
}

/// A country-provider outage cannot classify any otherwise testable entry as restricted.
#[tokio::test]
async fn failed_geolocation_leaves_entries_testable() {
    let network = services(
        Arc::new(FakeDns::default()),
        Arc::new(FakeCountries {
            fail: true,
            ..Default::default()
        }),
    );
    let expanded = prepare_network(
        fixture_collection(&["192.0.2.1"]).await,
        &DnsSection::default(),
        &VerifySection::default(),
        &network,
        &RoundCtx::new(Limits::from_concurrency(4)),
    )
    .await
    .unwrap();
    assert_eq!(expanded.proxies.len(), 1);
    assert!(expanded.jobs[0].excluded_reason.is_none());
    assert_eq!(expanded.network.geo_failures, 1);
}

/// Missing DNS views remain explicit uncertainty rather than a complete negative result.
#[tokio::test]
async fn partial_and_total_dns_outages_preserve_uncertainty() {
    for fail_all in [false, true] {
        let network = services(
            Arc::new(FakeDns {
                fail_overseas: true,
                fail_all,
                ..Default::default()
            }),
            Arc::new(FakeCountries::default()),
        );
        let expanded = prepare_network(
            fixture_collection(&["origin.example"]).await,
            &DnsSection::default(),
            &no_entry_filter(),
            &network,
            &RoundCtx::new(Limits::from_concurrency(4)),
        )
        .await
        .unwrap();
        assert!(expanded.jobs.iter().all(|job| job.dns_incomplete));
        assert_eq!(expanded.network.incomplete_domains, 1);
        if fail_all {
            assert_eq!(expanded.jobs.len(), 1);
            assert_eq!(expanded.proxies[0]["server"], "origin.example");
        }
    }
}

/// Hitting an address cap must not turn a partial set into an "all addresses passed" claim.
#[tokio::test]
async fn address_cap_keeps_the_missing_coverage_visible() {
    let network = services(
        Arc::new(FakeDns::default()),
        Arc::new(FakeCountries::default()),
    );
    let cfg = DnsSection {
        max_addresses_per_domain: 1,
        ..Default::default()
    };
    let expanded = prepare_network(
        fixture_collection(&["origin.example"]).await,
        &cfg,
        &no_entry_filter(),
        &network,
        &RoundCtx::new(Limits::from_concurrency(4)),
    )
    .await
    .unwrap();
    assert_eq!(expanded.jobs.len(), 1);
    assert!(expanded.jobs[0].dns_incomplete);
}

/// Front selectors stay stable and chain gate keys continue to use the front address.
#[tokio::test]
async fn fronts_are_not_expanded_and_chain_gates_stay_on_the_front() {
    let dns = Arc::new(FakeDns::default());
    let network = services(dns.clone(), Arc::new(FakeCountries::default()));
    let mut input = fixture_collection(&["front.example", "origin.example"]).await;
    input.jobs[0].role = Role::Front;
    input.jobs[1].role = Role::Chain;
    input.jobs[1].front = Some("node-0".into());
    input.jobs[1].server_ip = "front.example".into();
    input.proxies[1]["dialer-proxy"] = Value::String("node-0".into());
    let expanded = prepare_network(
        input,
        &DnsSection::default(),
        &no_entry_filter(),
        &network,
        &RoundCtx::new(Limits::from_concurrency(4)),
    )
    .await
    .unwrap();
    assert_eq!(expanded.jobs[0].proxy_name, "node-0");
    assert_eq!(expanded.jobs.len(), 4);
    assert!(expanded.jobs[1..]
        .iter()
        .all(|job| job.server_ip == "front.example"));
    assert!(expanded.proxies[1..]
        .iter()
        .all(|proxy| proxy["dialer-proxy"] == "node-0"));
    assert_eq!(dns.calls.load(Ordering::SeqCst), 4);
}

/// Pinning does not replace explicit SNI or the effective WebSocket authority.
#[test]
fn ip_pinning_preserves_tls_identity_and_original_input() {
    let proxy = json!({
        "type": "vless", "server": "origin.example", "tls": true, "network": "ws",
        "ws-opts": {"headers": {"Host": "edge.example"}},
    });
    let pinned = pin_server(&proxy, "2001:db8::1");
    assert_eq!(pinned["servername"], "edge.example");
    assert_eq!(pinned["ws-opts"], proxy["ws-opts"]);
    assert_eq!(proxy["server"], "origin.example");
    let explicit = json!({"type": "trojan", "server": "origin.example", "sni": "custom.example"});
    assert_eq!(pin_server(&explicit, "192.0.2.1")["sni"], "custom.example");
}

/// Malformed or unsolicited country rows cannot seed the ledger's entry cache.
#[test]
fn country_replies_are_scoped_to_the_requested_addresses() {
    let rows = vec![
        GeoRow {
            ip: "192.0.2.1".into(),
            country: None,
            isp: None,
            asn: None,
        },
        GeoRow {
            ip: "192.0.2.2".into(),
            country: Some("CN".into()),
            isp: None,
            asn: None,
        },
        GeoRow {
            ip: "192.0.2.3".into(),
            country: Some("invalid".into()),
            isp: None,
            asn: None,
        },
    ];
    assert!(normalize_countries(rows, &["192.0.2.1".into(), "192.0.2.3".into()]).is_empty());
}

struct PendingDns(Arc<tokio::sync::Notify>);

impl DnsLookup for PendingDns {
    /// Signal a running query, then await cancellation of the preparation task.
    fn lookup<'a>(
        &'a self,
        _name: &'a str,
        _qtype: u16,
        _view: &'a DnsView,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<DnsAnswer, String>> {
        Box::pin(async move {
            self.0.notify_one();
            std::future::pending().await
        })
    }
}

/// Cancelling while a resolver is blocked aborts all preparation jobs promptly.
#[tokio::test]
async fn cancellation_interrupts_queued_and_active_dns_work() {
    let started = Arc::new(tokio::sync::Notify::new());
    let network = NetworkServices {
        storage: Arc::new(Mutex::new(Storage::open_in_memory().unwrap())),
        dns: Arc::new(PendingDns(started.clone())),
        countries: Arc::new(FakeCountries::default()),
    };
    let ctx = RoundCtx::new(Limits::from_concurrency(4));
    let input = fixture_collection(&["one.example", "two.example"]).await;
    let cfg = DnsSection {
        cache_hours: 0,
        concurrency: 1,
        ..Default::default()
    };
    let verify = no_entry_filter();
    let prepare = prepare_network(input, &cfg, &verify, &network, &ctx);
    let cancel = async {
        started.notified().await;
        ctx.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(prepare, cancel)
    })
    .await
    .expect("DNS cancellation must be bounded");
    assert!(result.is_err());
}

/// An invalid positional collection must fail before mismatched jobs can be tested.
#[tokio::test]
async fn mismatched_proxy_job_lists_are_not_silently_zipped() {
    let mut input = Collected::default();
    input
        .jobs
        .push(Job::new("test", "fp", "orphan", "direct", "192.0.2.1"));
    let network = services(
        Arc::new(FakeDns::default()),
        Arc::new(FakeCountries::default()),
    );
    assert!(prepare_network(
        input,
        &DnsSection::default(),
        &no_entry_filter(),
        &network,
        &RoundCtx::new(Limits::from_concurrency(4))
    )
    .await
    .is_err());
}

/// Reject excessive query fanout before queuing work or sending traffic.
#[tokio::test]
async fn query_budget_is_enforced_before_network_requests() {
    let dns = Arc::new(FakeDns::default());
    let network = services(dns.clone(), Arc::new(FakeCountries::default()));
    let cfg = DnsSection {
        max_queries: 1,
        ..Default::default()
    };
    let result = prepare_network(
        fixture_collection(&["origin.example"]).await,
        &cfg,
        &no_entry_filter(),
        &network,
        &RoundCtx::new(Limits::from_concurrency(4)),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(dns.calls.load(Ordering::SeqCst), 0);
}

/// Capped selection respects query priority even when answers complete in reverse.
#[test]
fn bounded_answer_selection_is_independent_of_completion_order() {
    let mut answers = RankedAnswers::default();
    let response = |ips: &[&str]| DnsAnswer {
        ips: ips.iter().map(|ip| ip.to_string()).collect(),
        cname_chain: Vec::new(),
        ttl_secs: 60,
        negative: false,
    };
    answers.add(3, response(&["192.0.2.3", "192.0.2.4"]), 2);
    answers.add(0, response(&["192.0.2.1", "192.0.2.2"]), 2);
    assert!(answers.incomplete);
    assert_eq!(
        answers.ips.into_values().collect::<Vec<_>>(),
        ["192.0.2.1", "192.0.2.2"]
    );
}

/// DNS, cache lookup and implicit TLS names use the same canonical IDNA identity.
#[test]
fn unicode_domain_identity_is_shared_by_dns_cache_and_tls() {
    let name = "\u{4f8b}\u{3048}.jp";
    let canonical = "xn--r8jz45g.jp";
    let cfg = DnsSection::default();
    assert_eq!(
        dns_cache_key(name, TYPE_A, "cn", &cfg.views["cn"]),
        dns_cache_key(canonical, TYPE_A, "cn", &cfg.views["cn"])
    );
    assert_eq!(
        pin_server(&json!({"type": "trojan", "server": name}), "192.0.2.1")["sni"],
        canonical
    );
}

/// Unused transport options must not change the effective TCP/TLS identity.
#[test]
fn unused_websocket_headers_do_not_override_tcp_sni() {
    let proxy = json!({"type": "vless", "server": "origin.example", "network": "tcp",
        "ws-opts": {"headers": {"Host": "unused.example"}}});
    assert_eq!(
        pin_server(&proxy, "192.0.2.1")["servername"],
        "origin.example"
    );
}

/// Conflicting classifications are unknown, not whichever result arrived first.
#[test]
fn conflicting_geo_replies_are_not_cached_or_used_for_exclusion() {
    let rows = vec![
        GeoRow {
            ip: "192.0.2.1".into(),
            country: Some("US".into()),
            isp: None,
            asn: None,
        },
        GeoRow {
            ip: "192.0.2.1".into(),
            country: Some("CN".into()),
            isp: None,
            asn: None,
        },
        GeoRow {
            ip: "192.0.2.2".into(),
            country: Some("ZZ".into()),
            isp: None,
            asn: None,
        },
    ];
    assert!(normalize_countries(rows, &["192.0.2.1".into(), "192.0.2.2".into()]).is_empty());
}

/// A shorter configured window invalidates entries written with a longer lifetime.
#[tokio::test]
async fn shortened_cache_window_invalidates_old_entries() {
    let network = services(
        Arc::new(FakeDns::default()),
        Arc::new(FakeCountries::default()),
    );
    let cfg = DnsSection::default();
    let key = dns_cache_key("origin.example", TYPE_A, "cn", &cfg.views["cn"]);
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    network
        .storage
        .lock()
        .unwrap()
        .write_dns_cache(
            &key,
            &DnsCacheRecord {
                stored_at: now - 4000,
                expires_at: now + 4000,
                value_json:
                    json!({"ips":["192.0.2.1"],"cname_chain":[],"ttl_secs":8000,"negative":false})
                        .to_string(),
            },
        )
        .unwrap();
    assert!(read_answer(&network, &key, TYPE_A, 3600)
        .await
        .unwrap()
        .is_none());
    assert!(read_answer(&network, &key, TYPE_A, 21_600)
        .await
        .unwrap()
        .is_some());
}
