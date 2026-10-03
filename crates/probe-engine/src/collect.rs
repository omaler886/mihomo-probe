//! Fetch -> flatten -> prepare -> jobs: the collection half of a round.
//!
//! Before this module the collection lived in two places at once -- or rather
//! in neither: `probe-cli::cmd_round` and `probe-api::start_round` both built
//! a `RoundPlan` with `jobs: Vec::new()`, and the `probe-source` crate had no
//! caller. Assembling the plan in exactly one place is what keeps the two
//! entry points from drifting apart again (the same drift that produced the
//! twin placeholder round paths `round.rs` replaced).
//!
//! ## Deliberately out of scope (later batches)
//!
//! * `keep_dialer` is empty: there is no front pool yet (R6), so every
//!   `dialer-proxy` is stripped and chained nodes are measured direct -- the
//!   same shape as a Python 直连测活 round.
//! * `strip_ech` follows the Python default (`verify.strip_ech = true`); the
//!   Rust `Config` does not model the `verify` section yet.
//! * One node yields one job keyed on the domain form (R4 adds per-address
//!   variants); `server_ip` is the `server` field as written, falling back to
//!   the kernel proxy name when it is missing (a node without a server is
//!   dropped by `prepare`, so the fallback only fires for hand-built entries).

use probe_config::SourceSpec;
use probe_source::{collect_entries, prepare, FetchedSource, Fetcher};
use serde_json::Value;

use crate::limits::Job;

/// What one collection pass produced.
#[derive(Debug, Default)]
pub struct Collected {
    /// Kernel-ready proxies, in `prepare` order (unique names).
    pub proxies: Vec<Value>,
    /// One job per proxy, aligned with `proxies` by index.
    pub jobs: Vec<Job>,
    /// Per-source fetch failures (`"<key>: <reason>"`). A failed source is
    /// skipped, never fatal: one dead subscription must not cost the round.
    pub errors: Vec<String>,
    /// Nodes `prepare` dropped (missing fields / bad port), with reasons on
    /// the dropped entries (counted here; details stay with the caller to log).
    pub dropped: usize,
}

/// The Sub-Store backend, mirroring `config.DEFAULTS["substore"]["backend"]`:
/// the environment wins, the loopback default keeps a fresh install working.
pub fn backend_from_env() -> String {
    std::env::var("SUBSTORE_BACKEND")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

/// Fetch every enabled source, flatten to entries, prepare kernel proxies,
/// and derive one [`Job`] per proxy.
pub async fn collect(
    fetcher: &dyn Fetcher,
    sources: &[SourceSpec],
    keep_dialer: &[String],
    strip_ech: bool,
) -> Collected {
    let mut fetched = Vec::new();
    let mut errors = Vec::new();
    for source in sources.iter().filter(|s| s.enabled) {
        match fetcher.fetch(source).await {
            Ok(proxies) => fetched.push(FetchedSource::from_spec(source, proxies)),
            Err(err) => errors.push(format!("{}: {err}", source.key)),
        }
    }
    let entries = collect_entries(&fetched);
    let prepared = prepare(&entries, keep_dialer, strip_ech);
    let dropped = prepared.dropped.len();
    let mut proxies = Vec::with_capacity(prepared.nodes.len());
    let mut jobs = Vec::with_capacity(prepared.nodes.len());
    for node in prepared.nodes {
        // The address the kernel dials from this host. For direct nodes this
        // is the `server` field as written (domain form until R4); the proxy
        // name fallback only fires for entries `prepare` would have dropped.
        let server_ip = node
            .server
            .clone()
            .unwrap_or_else(|| node.proxy_name.clone());
        jobs.push(Job::new(
            node.source.clone(),
            node.fingerprint.clone(),
            node.proxy_name.clone(),
            node.category.clone(),
            server_ip,
        ));
        proxies.push(node.proxy);
    }
    Collected {
        proxies,
        jobs,
        errors,
        dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::future::Future;
    use std::pin::Pin;

    struct StubFetcher {
        /// `name -> proxies`, plus names that fail.
        ok: Vec<(String, Vec<Value>)>,
        fail: Vec<String>,
    }

    impl Fetcher for StubFetcher {
        fn fetch<'a>(
            &'a self,
            source: &'a SourceSpec,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Value>, String>> + Send + 'a>> {
            Box::pin(async move {
                if self.fail.iter().any(|n| n == &source.name) {
                    return Err("boom".to_string());
                }
                Ok(self
                    .ok
                    .iter()
                    .find(|(n, _)| n == &source.name)
                    .map(|(_, p)| p.clone())
                    .unwrap_or_default())
            })
        }
    }

    fn spec(key: &str, kind: &str, name: &str, enabled: bool) -> SourceSpec {
        SourceSpec {
            key: key.into(),
            kind: kind.into(),
            name: name.into(),
            label: name.into(),
            enabled,
            relay: false,
        }
    }

    fn ss(name: &str, server: &str) -> Value {
        json!({"name": name, "type": "ss", "server": server, "port": 8388,
               "cipher": "aes-128-gcm", "password": "pw"})
    }

    #[tokio::test]
    async fn enabled_sources_yield_aligned_proxies_and_jobs() {
        let fetcher = StubFetcher {
            ok: vec![("air".into(), vec![ss("a", "1.1.1.1"), ss("b", "2.2.2.2")])],
            fail: vec![],
        };
        let sources = vec![spec("air", "collection", "air", true)];
        let out = collect(&fetcher, &sources, &[], true).await;
        assert!(out.errors.is_empty());
        assert_eq!(out.dropped, 0);
        assert_eq!(out.proxies.len(), 2);
        assert_eq!(out.jobs.len(), 2);
        assert_eq!(out.jobs[0].source_id, "air");
        assert_eq!(out.jobs[0].proxy_name, "a");
        assert_eq!(out.jobs[0].server_ip, "1.1.1.1");
        assert_eq!(out.jobs[0].variant, "direct");
        assert_eq!(
            out.jobs[0].fingerprint,
            probe_source::fingerprint_proxy(&ss("a", "1.1.1.1")),
            "the job carries the ORIGINAL proxy identity"
        );
        // The kernel proxy is the prepared copy (port coerced, name set).
        assert_eq!(out.proxies[0]["name"], json!("a"));
    }

    #[tokio::test]
    async fn a_disabled_source_is_never_fetched() {
        let fetcher = StubFetcher {
            ok: vec![("air".into(), vec![ss("a", "1.1.1.1")])],
            fail: vec!["quiet".into()],
        };
        let sources = vec![
            spec("air", "collection", "air", true),
            spec("quiet", "sub", "quiet", false),
        ];
        let out = collect(&fetcher, &sources, &[], true).await;
        assert!(out.errors.is_empty(), "disabled means not fetched: {out:?}");
        assert_eq!(out.jobs.len(), 1);
    }

    #[tokio::test]
    async fn a_failed_source_is_skipped_not_fatal() {
        let fetcher = StubFetcher {
            ok: vec![("air".into(), vec![ss("a", "1.1.1.1")])],
            fail: vec!["gone".into()],
        };
        let sources = vec![
            spec("air", "collection", "air", true),
            spec("gone", "sub", "gone", true),
        ];
        let out = collect(&fetcher, &sources, &[], true).await;
        assert_eq!(out.errors, vec!["gone: boom"]);
        assert_eq!(out.jobs.len(), 1, "the live source still yields its node");
    }

    #[tokio::test]
    async fn duplicate_names_stay_unique_into_the_kernel_config() {
        let fetcher = StubFetcher {
            ok: vec![(
                "air".into(),
                vec![ss("dup", "1.1.1.1"), ss("dup", "2.2.2.2")],
            )],
            fail: vec![],
        };
        let out = collect(
            &fetcher,
            &[spec("air", "collection", "air", true)],
            &[],
            true,
        )
        .await;
        let names: Vec<&str> = out.jobs.iter().map(|j| j.proxy_name.as_str()).collect();
        assert_eq!(names, vec!["dup", "dup #2"]);
        // Same name but different servers: different ledger identities.
        assert_ne!(out.jobs[0].fingerprint, out.jobs[1].fingerprint);
    }
}
