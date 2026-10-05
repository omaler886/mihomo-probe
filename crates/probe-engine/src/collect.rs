//! Fetch -> flatten -> fronts -> expand -> prepare -> jobs: the collection
//! half of a round.
//!
//! Before this module the collection lived in two places at once -- or rather
//! in neither: `probe-cli::cmd_round` and `probe-api::start_round` both built
//! a `RoundPlan` with `jobs: Vec::new()`, and the `probe-source` crate had no
//! caller. Assembling the plan in exactly one place is what keeps the two
//! entry points from drifting apart again (the same drift that produced the
//! twin placeholder round paths `round.rs` replaced).
//!
//! ## The front pool (R6)
//!
//! A chained node dials through a *front*, and the front pool is its own
//! collection pass (Python `engine.collect_fronts`). Three inputs fill it:
//! the pasted `front_text` (materialised into a Sub-Store sub and read back
//! rendered, because Sub-Store already parses every share-link dialect),
//! `front_source` narrowed by `front_pick`, and the resource whole. Fronts
//! get reserved kernel names (`__FRONT{i}__`) so a chained variant's
//! `dialer-proxy` can never resolve to a user node that happens to share the
//! display name.
//!
//! `expand_chains` then gives every chained node one variant per front, all
//! folding into one ledger row (`fingerprint_proxy` ignores `dialer-proxy`):
//! one front carrying the traffic is enough, and every front failing fails
//! the node. A node measured *both* ways gets a derived fingerprint on its
//! direct twin (`variant_fingerprint`) so the two measurements cannot
//! overwrite each other.
//!
//! ## Deliberately out of scope (later batches)
//!
//! * `strip_ech` follows the Python default (`verify.strip_ech = true`); the
//!   Rust `Config` does not model the `verify` section yet.
//! * One node yields one job keyed on the domain form (R4 adds per-address
//!   variants); `server_ip` is the `server` field as written, falling back to
//!   the kernel proxy name when it is missing (a node without a server is
//!   dropped by `prepare`, so the fallback only fires for hand-built entries).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use probe_config::{ChainSection, SourceSpec};
use probe_source::{collect_entries, prepare, FetchedSource, Fetcher, RawEntry, Role, SubAdmin};
use serde_json::Value;

use crate::limits::Job;

/// Python `engine.FRONT_SOURCE_KEY`: the front pool's own source key.
pub const FRONT_SOURCE_KEY: &str = "__front__";
/// Python `engine.FRONT_NAME_PREFIX`; the kernel name is `{prefix}{i}__`.
pub const FRONT_NAME_PREFIX: &str = "__FRONT";

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
    /// The front pool this round collected (empty without chaining).
    pub fronts: Vec<Front>,
    /// Front-pool entries dropped for being past `max_fronts`.
    pub fronts_over_cap: usize,
    /// Chained nodes that got front variants (one count per node, not per
    /// variant).
    pub chained: usize,
}

/// What `collect_fronts` produced.
#[derive(Debug, Clone, Default)]
pub struct FrontPool {
    pub fronts: Vec<Front>,
    /// Entries ignored for being past `chain.max_fronts`.
    pub dropped_over_cap: usize,
}

/// One front: the hop a chained node dials through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Front {
    /// The name the operator picked; logs and the panel show this.
    pub display: String,
    /// The reserved kernel name (`__FRONT{i}__`) the chained variants'
    /// `dialer-proxy` points at.
    pub kernel_name: String,
    /// The front's own dial address, for the per-server gate.
    pub server: Option<String>,
    /// The kernel-facing proxy (name already rewritten).
    pub proxy: Value,
    /// The ledger identity of the *original* front proxy. A front is itself a
    /// node somewhere subscribed; its own verdict lands on this identity.
    pub fingerprint: String,
}

/// The Sub-Store backend, mirroring `config.DEFAULTS["substore"]["backend"]`:
/// the environment wins, the loopback default keeps a fresh install working.
pub fn backend_from_env() -> String {
    std::env::var("SUBSTORE_BACKEND")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

/// Process-level memory of what the manual-front sub was last written from
/// (Python `_MANUAL_FRONT_SYNCED` / `_MANUAL_FRONT_CLEANED`). An unchanged
/// paste costs no Sub-Store write; losing the memory costs one idempotent
/// upsert.
#[derive(Clone, Default)]
pub struct ManualFrontCache {
    synced: Arc<Mutex<HashMap<String, String>>>,
    cleaned: Arc<Mutex<HashSet<String>>>,
}

/// The chain half of a round's inputs. `ChainContext::off` restores the
/// pre-R6 shape: no pool, every node measured direct.
pub struct ChainContext<'a> {
    pub section: &'a ChainSection,
    pub publish_prefix: &'a str,
    /// Sub-Store admin access (upsert/delete), for the pasted front text.
    pub admin: &'a dyn SubAdmin,
    pub manual: &'a ManualFrontCache,
    /// `Some("direct")` = the 直连测活 button (pool skipped on purpose);
    /// `Some("chain")` = the 链式测活 button; `None` = the scheduler, which
    /// chains exactly as configured.
    pub mode: Option<&'a str>,
}

impl<'a> ChainContext<'a> {
    /// No chain inputs at all: a round that measures everything direct.
    pub fn off(mode: Option<&'a str>) -> Self {
        use std::sync::OnceLock;
        static DISABLED: OnceLock<ChainSection> = OnceLock::new();
        static NO_MANUAL: OnceLock<ManualFrontCache> = OnceLock::new();
        static NO_ADMIN: NoAdmin = NoAdmin;
        Self {
            section: DISABLED.get_or_init(|| ChainSection {
                enabled: false,
                front_source_kind: String::new(),
                front_source_name: String::new(),
                front_pick: Vec::new(),
                front_text: String::new(),
                max_fronts: 8,
                test_plain_nodes: false,
            }),
            publish_prefix: "probe",
            admin: &NO_ADMIN,
            manual: NO_MANUAL.get_or_init(ManualFrontCache::default),
            mode,
        }
    }

    /// Python `round_uses_chains`: a 直连测活 round never chains; every other
    /// mode chains exactly when the chain block is configured.
    fn chained_round(&self) -> bool {
        self.mode != Some("direct") && self.section.is_configured()
    }
}

/// A stand-in used by [`ChainContext::off`]; never called, because a disabled
/// section never reaches the admin path.
struct NoAdmin;

impl SubAdmin for NoAdmin {
    fn upsert_sub<'a>(
        &'a self,
        _name: &'a str,
        _payload: &'a Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async { Err("chaining is off".to_string()) })
    }

    fn delete_sub<'a>(
        &'a self,
        _name: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>> {
        Box::pin(async { Ok(false) })
    }
}

/// Fetch every enabled source, flatten to entries, expand chains, prepare
/// kernel proxies, and derive one [`Job`] per proxy.
pub async fn collect(
    fetcher: &dyn Fetcher,
    sources: &[SourceSpec],
    strip_ech: bool,
    chain: ChainContext<'_>,
) -> Collected {
    let mut fetched = Vec::new();
    let mut errors = Vec::new();
    for source in sources.iter().filter(|s| s.enabled) {
        match fetcher.fetch(source).await {
            Ok(proxies) => fetched.push(FetchedSource::from_spec(source, proxies)),
            Err(err) => errors.push(format!("{}: {err}", source.key)),
        }
    }

    let chained_round = chain.chained_round();
    let pool = if chained_round {
        collect_fronts(
            chain.section,
            chain.publish_prefix,
            fetcher,
            chain.admin,
            chain.manual,
        )
        .await
    } else {
        FrontPool::default()
    };
    let fronts = pool.fronts;
    let front_names: Vec<String> = fronts.iter().map(|f| f.kernel_name.clone()).collect();

    let mut entries = collect_entries(&fetched);
    let has_chained_nodes = entries.iter().any(|e| has_dialer(&e.proxy));
    if chained_round {
        if !fronts.is_empty() && !has_chained_nodes {
            tracing::warn!(
                fronts = fronts.len(),
                "前置池有前置，但没有任何数据源提供带 dialer-proxy 的节点，链式测活本轮无事可做"
            );
        } else if has_chained_nodes && fronts.is_empty() {
            tracing::error!(
                "有节点带 dialer-proxy，但前置池为空，这些节点本轮全部判失败（原因 front_dead，未拨号；检查 chain.front_source、front_pick 与 front_text）"
            );
        }
    }
    // The pool joins the entries as first-class nodes: each front is tested
    // like any other node in the first phase, and mihomo needs the
    // `__FRONT*__` proxies in the config for `dialer-proxy` to resolve.
    for (i, front) in fronts.iter().enumerate() {
        entries.push(RawEntry {
            source: FRONT_SOURCE_KEY.to_string(),
            index: i,
            name: front.display.clone(),
            proxy: front.proxy.clone(),
            fingerprint: front.fingerprint.clone(),
            category: probe_source::CAT_RELAY.to_string(),
            role: Role::Front,
            front: None,
        });
    }

    // A 直连测活 round passes None on purpose: that button means "measure
    // everything direct", so it must not be re-split by the per-source
    // switches. Every other mode honours them.
    let source_flags = if chain.mode == Some("direct") {
        None
    } else {
        Some(sources)
    };
    let plain_too = chain.section.test_plain_nodes;
    if plain_too && !chain.section.is_configured() {
        tracing::warn!("chain.test_plain_nodes 已开启但链式未生效，普通节点本轮仍按直连测");
    }
    let (expanded, chained) = expand_chains(
        entries,
        &front_names,
        source_flags,
        // `fail_without_front` is set only when this round *is* a chain
        // round: an empty pool there is a failure to report (`front_dead`),
        // not an invitation to measure the chained nodes direct -- that
        // would report nodes alive on a path their owner never uses.
        chained_round,
        plain_too,
    );

    let prepared = prepare(&expanded, &front_names, strip_ech);
    let dropped = prepared.dropped.len();

    // A chain variant's job is keyed on the front's address, not the landing:
    // the kernel dials the front from this host, and the per-server gate must
    // bound that dial (see `limits`' module docs).
    let front_servers: HashMap<&str, Option<&str>> = fronts
        .iter()
        .map(|f| (f.kernel_name.as_str(), f.server.as_deref()))
        .collect();

    let mut proxies = Vec::with_capacity(prepared.nodes.len());
    let mut jobs = Vec::with_capacity(prepared.nodes.len());
    for node in prepared.nodes {
        let server_ip = match (node.role, node.front.as_deref()) {
            (Role::Chain, Some(front)) => front_servers
                .get(front)
                .copied()
                .flatten()
                .map(str::to_string)
                .unwrap_or_else(|| front.to_string()),
            _ => node
                .server
                .clone()
                .unwrap_or_else(|| node.proxy_name.clone()),
        };
        jobs.push(
            Job::new(
                node.source.clone(),
                node.fingerprint.clone(),
                node.proxy_name.clone(),
                node.category.clone(),
                server_ip,
            )
            .with_role(node.role, node.front.clone()),
        );
        proxies.push(node.proxy);
    }
    Collected {
        proxies,
        jobs,
        errors,
        dropped,
        fronts,
        fronts_over_cap: pool.dropped_over_cap,
        chained,
    }
}

/// `DIALER_FIELD in proxy` -- Python checks the hyphen spelling only here;
/// mihomo normalises `_` to `-` on the way in, and upstream uses both.
fn has_dialer(proxy: &Value) -> bool {
    proxy.get("dialer-proxy").is_some() || proxy.get("dialer_proxy").is_some()
}

/// Port of `engine.collect_fronts`: fetch the pool of fronts a chained node
/// dials through.
///
/// Three inputs fill it, in this order, and `max_fronts` caps the result:
/// pasted text first (the operator's explicit, just-typed choice), then the
/// resource narrowed by `front_pick` when set, then the resource whole. A
/// failed source logs and yields nothing -- a broken pool is a round with
/// every chain `front_dead`, reported by the runner, not a crash here.
pub async fn collect_fronts(
    chain: &ChainSection,
    publish_prefix: &str,
    fetcher: &dyn Fetcher,
    admin: &dyn SubAdmin,
    manual: &ManualFrontCache,
) -> FrontPool {
    let manual_name = chain.manual_sub_name(publish_prefix);
    // Before the early return: clearing the paste *and* the front source in
    // one edit makes `chain_block` None, and a `-front-manual` sub left
    // behind would keep serving fronts the operator removed (its content is
    // embedded).
    if chain.front_text.is_empty() {
        drop_manual_front(admin, &manual_name, manual).await;
    }
    if !chain.is_configured() {
        return FrontPool::default();
    }

    let mut manual_fronts = manual_fronts(
        chain.front_text.as_str(),
        &manual_name,
        fetcher,
        admin,
        manual,
    )
    .await;

    let mut resource = Vec::new();
    let picked: Vec<String> = chain.front_pick.clone();
    if !chain.front_source_name.is_empty() {
        let spec = SourceSpec {
            key: FRONT_SOURCE_KEY.to_string(),
            kind: chain.front_source_kind.clone(),
            name: chain.front_source_name.clone(),
            label: chain.front_source_name.clone(),
            enabled: true,
            relay: false,
            direct: true,
            chain: true,
        };
        match fetcher.fetch(&spec).await {
            Ok(proxies) => resource = proxies,
            Err(err) => {
                tracing::error!(
                    source = %chain.front_source_name,
                    "前置来源拉取失败: {err}"
                );
            }
        }
        if !picked.is_empty() {
            let wanted: HashSet<&str> = picked.iter().map(String::as_str).collect();
            let before = resource.len();
            resource.retain(|p| {
                p.get("name")
                    .and_then(Value::as_str)
                    .map(|n| wanted.contains(n))
                    .unwrap_or(false)
            });
            let missing = picked.len().saturating_sub(resource.len());
            let note = if missing > 0 {
                "（部分名单节点不在该来源里）"
            } else {
                ""
            };
            tracing::info!(
                found = resource.len(),
                total = before,
                "前置来源按名单取 {}/{} 条{}",
                resource.len(),
                before,
                note
            );
        }
    } else if !picked.is_empty() {
        tracing::warn!("chain.front_pick 有名单但没有前置来源，已忽略");
    }

    let proxies: Vec<Value> = manual_fronts
        .drain(..)
        .chain(resource.into_iter())
        .collect();

    let cap = chain.max_fronts_cap();
    let mut fronts = Vec::new();
    let mut dropped = 0usize;
    for (i, proxy) in proxies.into_iter().enumerate() {
        if fronts.len() >= cap {
            dropped += 1;
            continue;
        }
        let display = proxy
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("front-{i}"));
        // The kernel-facing name is reserved and position-derived: a chained
        // variant's `dialer-proxy` must resolve to the front and never to a
        // user's node that happens to share its display name.
        let kernel_name = format!("{FRONT_NAME_PREFIX}{}__", fronts.len());
        let server = proxy
            .get("server")
            .and_then(Value::as_str)
            .map(str::to_string);
        let fingerprint = probe_source::fingerprint_proxy(&proxy);
        let mut named = proxy.clone();
        if let Some(map) = named.as_object_mut() {
            map.insert("name".to_string(), Value::from(kernel_name.clone()));
        }
        fronts.push(Front {
            display,
            kernel_name,
            server,
            proxy: named,
            fingerprint,
        });
    }
    if dropped > 0 {
        tracing::warn!(
            "前置池有 {} 条超出 chain.max_fronts={}，已忽略",
            dropped,
            cap
        );
    }
    if fronts.is_empty() {
        tracing::error!("前置池没有可用前置，链式节点本轮全部判失败（front_dead）");
    } else {
        tracing::info!(pool = fronts.len(), "前置池 {} 条", fronts.len());
    }
    FrontPool {
        fronts,
        dropped_over_cap: dropped,
    }
}

/// The pasted front list, materialised through Sub-Store (`engine.manual_fronts`).
///
/// The text is either share links (`vless://…`, one per line) or a base64
/// subscription body, and Sub-Store already parses every dialect of both.
/// Writing our own link parser would be a second and worse implementation of
/// a job that is one HTTP call away. The upsert is skipped when the text is
/// unchanged since the last one, so a steady pool costs no writes.
async fn manual_fronts(
    text: &str,
    name: &str,
    fetcher: &dyn Fetcher,
    admin: &dyn SubAdmin,
    manual: &ManualFrontCache,
) -> Vec<Value> {
    if text.is_empty() {
        drop_manual_front(admin, name, manual).await;
        return Vec::new();
    }
    let digest = probe_source::content_digest(text);
    let needs_write = {
        let mut synced = manual.synced.lock().unwrap();
        if synced.get(name) != Some(&digest) {
            synced.insert(name.to_string(), digest);
            true
        } else {
            false
        }
    };
    if needs_write {
        let payload = serde_json::json!({
            "name": name,
            "displayName": "手动前置（测活中心）",
            "source": "local",
            "url": "",
            "content": text,
            "mergeSources": "",
            "ignoreFailedRemoteSub": "quiet",
            "passThroughUA": false,
            "process": [],
        });
        match admin.upsert_sub(name, &payload).await {
            Ok(action) => {
                manual.cleaned.lock().unwrap().remove(name);
                tracing::info!(sub = name, %action, "手动前置已写入 Sub-Store 订阅");
            }
            Err(err) => {
                // The in-memory digest must not remember a failed write: the
                // next round would see the paste as "already synced" and read
                // a sub that was never updated.
                manual.synced.lock().unwrap().remove(name);
                tracing::error!(sub = name, "手动前置写入 Sub-Store 失败: {err}");
                return Vec::new();
            }
        }
    }
    let spec = SourceSpec {
        key: FRONT_SOURCE_KEY.to_string(),
        kind: "sub".to_string(),
        name: name.to_string(),
        label: name.to_string(),
        enabled: true,
        relay: false,
        direct: true,
        chain: true,
    };
    match fetcher.fetch(&spec).await {
        Ok(proxies) => proxies,
        Err(err) => {
            tracing::error!(sub = name, "手动前置订阅解析失败: {err}");
            Vec::new()
        }
    }
}

/// Remove the materialised manual pool once the pasted text is cleared.
///
/// The `cleaned` set keeps this from repeating: the content is embedded at
/// write time, so a leftover copy would go on offering fronts the operator
/// removed. A 404 means there was nothing to clean, which is the common case.
async fn drop_manual_front(admin: &dyn SubAdmin, name: &str, manual: &ManualFrontCache) {
    if manual.cleaned.lock().unwrap().contains(name) {
        return;
    }
    manual.cleaned.lock().unwrap().insert(name.to_string());
    manual.synced.lock().unwrap().remove(name);
    match admin.delete_sub(name).await {
        Ok(true) => tracing::info!(sub = name, "手动前置已清空，删除 Sub-Store 订阅"),
        Ok(false) => {}
        Err(err) => tracing::warn!(sub = name, "清理手动前置订阅失败: {err}"),
    }
}

/// Port of `engine.expand_chains`: give every chained node one test variant
/// per front.
///
/// * A source with `chain` off gets no front variants at all -- its dialer is
///   stripped and it is measured as its own server.
/// * A source with `direct` on *in addition to* chain gets a stripped variant
///   alongside the chained ones, keyed by a *derived* fingerprint
///   (`variant_fingerprint`): both variants describe the same node, so
///   without a distinct identity the ledger would fold them into one row and
///   the two measurements would overwrite each other.
/// * A chained node's variants all keep the node's fingerprint, so the
///   per-fingerprint aggregation decides it without knowing anything about
///   chains: one front carrying the traffic is enough.
/// * `plain_too` extends the expansion to nodes the upstream ships *without*
///   a `dialer-proxy`: each gets front variants (carrying the pool's dialer)
///   and deliberately no direct twin, because the mode exists to make the
///   ledger's verdict the *client's* verdict.
/// * An empty pool with `fail_without_front` set (a chain round whose pool
///   came back empty) emits the chained node with `front: None` -- the runner
///   fails it `front_dead` without dialling. An empty pool *without*
///   `fail_without_front` (chaining off, or a 直连测活 round) falls through
///   to a direct measurement, which is the documented behaviour.
pub fn expand_chains(
    entries: Vec<RawEntry>,
    front_names: &[String],
    source_flags: Option<&[SourceSpec]>,
    fail_without_front: bool,
    plain_too: bool,
) -> (Vec<RawEntry>, usize) {
    if front_names.is_empty() && !(source_flags.is_some() && fail_without_front) {
        return (entries, 0);
    }
    let mut out = Vec::new();
    let mut chained = 0usize;
    for entry in entries {
        let plain = !has_dialer(&entry.proxy);
        if entry.role == Role::Front {
            out.push(entry);
            continue;
        }
        if plain && !plain_too {
            out.push(entry);
            continue;
        }
        // A node from a relay-flagged source is a *front*, not a passenger:
        // expanding it would give it a `dialer-proxy` pointing at another
        // front, and the ledger folds by fingerprint, so the variant's
        // `chain` category would overwrite the relay classification.
        if entry.category == probe_source::CAT_RELAY {
            out.push(entry);
            continue;
        }
        let (mut direct, chain_flag) = measure_flags(&entry.source, source_flags);
        if plain {
            // Client-path mode: measured exactly the way the consuming client
            // uses it -- dialled through the front pool, no direct twin.
            if !chain_flag {
                out.push(entry);
                continue;
            }
            direct = false;
        }
        if chain_flag {
            chained += 1;
            if front_names.is_empty() {
                // Chain round, empty pool: no dial, no verdict from a dial --
                // the runner fails it with `front_dead` instead. The dialer
                // stays on the proxy on purpose: `prepare` strips whatever is
                // not in `keep_dialer`, and here that is everything, so the
                // kernel never sees a dangling reference.
                out.push(RawEntry {
                    role: Role::Chain,
                    front: None,
                    category: probe_source::CAT_CHAIN.to_string(),
                    ..entry.clone()
                });
            }
            for front in front_names {
                let mut variant = entry.proxy.clone();
                if let Some(map) = variant.as_object_mut() {
                    map.insert("dialer-proxy".to_string(), Value::from(front.clone()));
                }
                // The variant ends up chained, whatever the original node was
                // classified as: once a dialer is attached, the node is
                // reachable only through it. The fingerprint is deliberately
                // *not* changed, so all variants fold into one ledger row.
                out.push(RawEntry {
                    proxy: variant,
                    role: Role::Chain,
                    front: Some(front.clone()),
                    category: probe_source::CAT_CHAIN.to_string(),
                    ..entry.clone()
                });
            }
        }
        if direct {
            // Measured as its own server: the dialer is the whole difference
            // between the two. Only when *both* ways are measured does the
            // twin get a derived fingerprint -- with `chain` off there is a
            // single variant, and re-keying it would orphan every ledger row
            // the source already has.
            let mut stripped = entry.proxy.clone();
            if let Some(map) = stripped.as_object_mut() {
                map.remove("dialer-proxy");
                map.remove("dialer_proxy");
            }
            let mut twin = RawEntry {
                proxy: stripped,
                role: Role::Direct,
                front: None,
                category: probe_source::CAT_DIRECT.to_string(),
                ..entry.clone()
            };
            if chain_flag {
                twin.fingerprint = probe_source::variant_fingerprint(&entry.fingerprint, "direct");
            }
            out.push(twin);
        }
    }
    (out, chained)
}

/// Port of `engine._measure_flags`: the (direct, chain) pair for one source.
///
/// No policy (`source_flags` is None) keeps the historical chain-only
/// behaviour. Both-off falls back to direct rather than testing nothing: a
/// node this round never measures loses its ledger row to pruning, and an
/// operator un-ticking both boxes would silently delete the source's history.
fn measure_flags(source: &str, flags: Option<&[SourceSpec]>) -> (bool, bool) {
    let Some(flags) = flags else {
        return (false, true);
    };
    let spec = flags
        .iter()
        .find(|s| s.key == source)
        .map(|s| (s.direct, s.chain))
        .unwrap_or((true, true));
    if !spec.0 && !spec.1 {
        return (true, false);
    }
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use probe_config::ChainSection;
    use probe_source::CAT_DIRECT;
    use serde_json::json;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

    struct StubFetcher {
        /// `name -> proxies`, plus names that fail.
        ok: Vec<(String, Vec<Value>)>,
        fail: Vec<String>,
    }

    impl Fetcher for StubFetcher {
        fn fetch<'a>(&'a self, source: &'a SourceSpec) -> BoxFut<'a, Result<Vec<Value>, String>> {
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

    /// Records every admin call; `upsert` returns the action and `delete`
    /// reports "was there" when at least one upsert succeeded.
    #[derive(Default, Clone)]
    struct FakeAdmin {
        upserts: Arc<Mutex<Vec<(String, String)>>>,
        deletes: Arc<Mutex<Vec<String>>>,
        fail_upsert: bool,
    }

    impl SubAdmin for FakeAdmin {
        fn upsert_sub<'a>(
            &'a self,
            name: &'a str,
            _payload: &'a Value,
        ) -> BoxFut<'a, Result<String, String>> {
            Box::pin(async move {
                self.upserts
                    .lock()
                    .unwrap()
                    .push(("upsert".into(), name.into()));
                if self.fail_upsert {
                    return Err("backend down".into());
                }
                Ok("created".into())
            })
        }
        fn delete_sub<'a>(&'a self, name: &'a str) -> BoxFut<'a, Result<bool, String>> {
            let had = !self.upserts.lock().unwrap().is_empty();
            Box::pin(async move {
                self.deletes.lock().unwrap().push(name.into());
                Ok(had)
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
            direct: true,
            chain: true,
        }
    }

    fn ss(name: &str, server: &str) -> Value {
        json!({"name": name, "type": "ss", "server": server, "port": 8388,
               "cipher": "aes-128-gcm", "password": "pw"})
    }

    fn chain_section(text: &str, source: &str, pick: &[&str], max_fronts: i64) -> ChainSection {
        ChainSection {
            enabled: true,
            front_source_kind: "sub".into(),
            front_source_name: source.into(),
            front_pick: pick.iter().map(|s| s.to_string()).collect(),
            front_text: text.into(),
            max_fronts,
            test_plain_nodes: false,
        }
    }

    fn manual() -> ManualFrontCache {
        ManualFrontCache::default()
    }

    // --- collect_fronts ---

    #[tokio::test]
    async fn pasted_fronts_come_first_and_get_reserved_kernel_names() {
        let fetcher = StubFetcher {
            ok: vec![
                ("probe-front-manual".into(), vec![ss("pasted", "7.7.7.7")]),
                (
                    "pool".into(),
                    vec![ss("front-a", "9.9.9.9"), ss("front-b", "8.8.8.8")],
                ),
            ],
            fail: vec![],
        };
        let admin = FakeAdmin::default();
        let section = chain_section("vless://pasted", "pool", &[], 8);
        let pool = collect_fronts(&section, "probe", &fetcher, &admin, &manual()).await;
        let fronts = pool.fronts;
        assert_eq!(fronts.len(), 3, "paste first, then the resource");
        assert_eq!(fronts[0].kernel_name, "__FRONT0__");
        assert_eq!(fronts[1].kernel_name, "__FRONT1__");
        assert_eq!(fronts[2].display, "front-b");
        // The kernel-facing proxy is renamed; the display name is kept aside.
        assert_eq!(fronts[1].proxy["name"], json!("__FRONT1__"));
        assert_eq!(fronts[1].display, "front-a");
    }

    #[tokio::test]
    async fn front_pick_narrows_the_resource_instead_of_supplementing_it() {
        let fetcher = StubFetcher {
            ok: vec![(
                "pool".into(),
                vec![ss("a", "9.9.9.9"), ss("b", "8.8.8.8"), ss("c", "7.7.7.7")],
            )],
            fail: vec![],
        };
        let admin = FakeAdmin::default();
        let section = chain_section("", "pool", &["b", "gone", "a"], 8);
        let pool = collect_fronts(&section, "probe", &fetcher, &admin, &manual()).await;
        let fronts = pool.fronts;
        let displays: Vec<&str> = fronts.iter().map(|f| f.display.as_str()).collect();
        assert_eq!(
            displays,
            vec!["a", "b"],
            "narrowed to the pick, kept in the resource's own order"
        );
    }

    #[tokio::test]
    async fn the_pool_is_capped_at_max_fronts() {
        let fetcher = StubFetcher {
            ok: vec![(
                "pool".into(),
                (0..10).map(|i| ss(&format!("n{i}"), "9.9.9.9")).collect(),
            )],
            fail: vec![],
        };
        let admin = FakeAdmin::default();
        let section = chain_section("", "pool", &[], 3);
        let pool = collect_fronts(&section, "probe", &fetcher, &admin, &manual()).await;
        let fronts = pool.fronts;
        assert_eq!(fronts.len(), 3);
    }

    #[tokio::test]
    async fn a_healthy_paste_is_upserted_once_and_not_again() {
        let fetcher = StubFetcher {
            ok: vec![],
            fail: vec![],
        };
        let admin = FakeAdmin::default();
        let cache = manual();
        let section = chain_section("vless://x", "", &[], 8);
        collect_fronts(&section, "probe", &fetcher, &admin, &cache).await;
        collect_fronts(&section, "probe", &fetcher, &admin, &cache).await;
        assert_eq!(
            admin.upserts.lock().unwrap().len(),
            1,
            "the digest cache skips the rewrite"
        );
        assert_eq!(admin.upserts.lock().unwrap()[0].1, "probe-front-manual");
    }

    #[tokio::test]
    async fn a_failed_upsert_is_not_cached_as_synced() {
        let fetcher = StubFetcher {
            ok: vec![],
            fail: vec![],
        };
        let admin = FakeAdmin {
            fail_upsert: true,
            ..Default::default()
        };
        let cache = manual();
        let section = chain_section("vless://x", "", &[], 8);
        let pool = collect_fronts(&section, "probe", &fetcher, &admin, &cache).await;
        assert!(pool.fronts.is_empty());
        // Round two retries the write: a cached "synced" would read a sub
        // that was never updated.
        collect_fronts(&section, "probe", &fetcher, &admin, &cache).await;
        assert_eq!(admin.upserts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn clearing_the_paste_deletes_the_materialised_sub_once() {
        let fetcher = StubFetcher {
            ok: vec![],
            fail: vec![],
        };
        let admin = FakeAdmin::default();
        let cache = manual();
        let with_paste = chain_section("vless://x", "", &[], 8);
        collect_fronts(&with_paste, "probe", &fetcher, &admin, &cache).await;
        let cleared = chain_section("", "pool", &[], 8);
        collect_fronts(&cleared, "probe", &fetcher, &admin, &cache).await;
        collect_fronts(&cleared, "probe", &fetcher, &admin, &cache).await;
        assert_eq!(
            admin.deletes.lock().unwrap().len(),
            1,
            "one cleanup, not one per round"
        );
    }

    #[tokio::test]
    async fn a_disabled_section_collects_nothing_and_touches_nothing() {
        let fetcher = StubFetcher {
            ok: vec![],
            fail: vec![],
        };
        let admin = FakeAdmin::default();
        let section = ChainSection {
            enabled: false,
            front_text: "vless://x".into(),
            ..chain_section("", "pool", &[], 8)
        };
        let cache = manual();
        let pool = collect_fronts(&section, "probe", &fetcher, &admin, &cache).await;
        assert!(pool.fronts.is_empty());
        assert!(admin.upserts.lock().unwrap().is_empty());
        // Python deletes the manual sub before the configured check; with a
        // non-empty paste nothing is deleted either.
        assert!(admin.deletes.lock().unwrap().is_empty());
    }

    // --- expand_chains ---

    fn raw(name: &str, proxy: Value) -> RawEntry {
        RawEntry {
            source: "air".into(),
            index: 0,
            name: name.into(),
            fingerprint: probe_source::fingerprint_proxy(&proxy),
            proxy,
            category: CAT_DIRECT.into(),
            role: Role::Direct,
            front: None,
        }
    }

    fn chained(name: &str, server: &str) -> RawEntry {
        let mut proxy = ss(name, server);
        proxy["dialer-proxy"] = json!("upstream-front");
        raw(name, proxy)
    }

    fn source_list(flags: &[(&str, bool, bool)]) -> Vec<SourceSpec> {
        flags
            .iter()
            .map(|(key, direct, chain)| SourceSpec {
                key: key.to_string(),
                kind: "sub".into(),
                name: key.to_string(),
                label: key.to_string(),
                enabled: true,
                relay: false,
                direct: *direct,
                chain: *chain,
            })
            .collect()
    }

    #[test]
    fn a_chained_node_gets_one_variant_per_front_with_its_own_fingerprint() {
        let entry = chained("node", "1.2.3.4");
        let fronts = vec!["__FRONT0__".to_string(), "__FRONT1__".to_string()];
        let (out, chained_n) = expand_chains(vec![entry], &fronts, None, false, false);
        assert_eq!(chained_n, 1);
        assert_eq!(out.len(), 2, "one variant per front, no direct twin");
        let fp = probe_source::fingerprint_proxy(&chained("node", "1.2.3.4").proxy);
        for variant in &out {
            assert_eq!(variant.role, Role::Chain);
            assert_eq!(variant.category, "chain");
            assert_eq!(variant.fingerprint, fp, "variants fold into one ledger row");
        }
        assert_eq!(out[0].front.as_deref(), Some("__FRONT0__"));
        assert_eq!(out[1].front.as_deref(), Some("__FRONT1__"));
        assert_eq!(out[0].proxy["dialer-proxy"], json!("__FRONT0__"));
    }

    #[test]
    fn a_direct_twin_gets_a_derived_fingerprint_only_when_both_ways_are_measured() {
        let fronts = vec!["__FRONT0__".to_string()];
        let specs = source_list(&[("air", true, true)]);
        let (out, _) = expand_chains(
            vec![chained("n", "1.2.3.4")],
            &fronts,
            Some(&specs),
            false,
            false,
        );
        assert_eq!(out.len(), 2, "chain variant + direct twin");
        assert_eq!(out[1].role, Role::Direct);
        assert_eq!(out[1].category, "direct");
        assert!(
            out[1].proxy.get("dialer-proxy").is_none(),
            "the twin is the node sans dialer"
        );
        let base = probe_source::fingerprint_proxy(&chained("n", "1.2.3.4").proxy);
        assert_ne!(
            out[1].fingerprint, base,
            "the twin must not fold into the chain row"
        );
        assert_eq!(
            out[1].fingerprint,
            probe_source::variant_fingerprint(&base, "direct"),
            "the twin's identity derives from the node's own"
        );

        // With `chain` off there is a single variant: re-keying it would
        // orphan the ledger rows the source already has.
        let specs = source_list(&[("air", true, false)]);
        let (out, chained_n) = expand_chains(
            vec![chained("n", "1.2.3.4")],
            &fronts,
            Some(&specs),
            false,
            false,
        );
        assert_eq!(chained_n, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].fingerprint, base,
            "unchanged identity when chain is off"
        );
    }

    #[test]
    fn a_source_with_chain_off_gets_no_variants() {
        let fronts = vec!["__FRONT0__".to_string()];
        let specs = source_list(&[("air", true, false)]);
        let (out, chained_n) = expand_chains(
            vec![chained("n", "1.2.3.4")],
            &fronts,
            Some(&specs),
            false,
            false,
        );
        assert_eq!(chained_n, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::Direct);
    }

    #[test]
    fn a_relay_entry_is_never_expanded() {
        let fronts = vec!["__FRONT0__".to_string()];
        let mut entry = chained("hop", "1.2.3.4");
        entry.category = "relay".into();
        let (out, chained_n) = expand_chains(vec![entry], &fronts, None, false, false);
        assert_eq!(chained_n, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].category, "relay");
        assert!(
            out[0].proxy.get("dialer-proxy") == Some(&json!("upstream-front")),
            "the relay keeps the dialer it was classified with"
        );
    }

    #[test]
    fn a_front_entry_passes_through_untouched() {
        let fronts = vec!["__FRONT0__".to_string()];
        let mut entry = chained("front", "9.9.9.9");
        entry.role = Role::Front;
        let (out, chained_n) = expand_chains(vec![entry], &fronts, None, false, false);
        assert_eq!(chained_n, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::Front);
    }

    #[test]
    fn an_empty_pool_only_matters_on_a_chain_round() {
        // A 直连测活 round (source_flags None) with no pool: untouched.
        let (out, chained_n) =
            expand_chains(vec![chained("n", "1.2.3.4")], &[], None, false, false);
        assert_eq!(chained_n, 0);
        assert_eq!(out.len(), 1);

        // Chaining off but the round is shaped by per-source flags: fall
        // through to direct.
        let specs = source_list(&[("air", true, true)]);
        let (out, _) = expand_chains(
            vec![chained("n", "1.2.3.4")],
            &[],
            Some(&specs),
            false,
            false,
        );
        assert_eq!(out.len(), 1);

        // A chain round whose pool came back empty: the chain variant is
        // *failed* (`front: None`), never measured direct. The direct twin
        // (the source is direct+chain) is still there.
        let (out, chained_n) = expand_chains(
            vec![chained("n", "1.2.3.4")],
            &[],
            Some(&specs),
            true,
            false,
        );
        assert_eq!(chained_n, 1);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, Role::Chain);
        assert_eq!(out[0].front, None, "the runner fails this front_dead");
        assert_eq!(out[1].role, Role::Direct);
        assert!(
            out[0].proxy.get("dialer-proxy").is_some(),
            "the dialer stays; prepare strips what keep_dialer does not name"
        );
    }

    #[test]
    fn plain_too_expands_plain_nodes_without_a_direct_twin() {
        let fronts = vec!["__FRONT0__".to_string()];
        // Without the switch a plain node passes through.
        let (out, _) = expand_chains(
            vec![raw("plain", ss("plain", "1.2.3.4"))],
            &fronts,
            None,
            false,
            false,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::Direct);

        // With it the plain node gets chained variants and no direct twin:
        // the ledger verdict IS the client's verdict.
        let (out, chained_n) = expand_chains(
            vec![raw("plain", ss("plain", "1.2.3.4"))],
            &fronts,
            None,
            false,
            true,
        );
        assert_eq!(chained_n, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::Chain);
        assert_eq!(out[0].front.as_deref(), Some("__FRONT0__"));
        assert_eq!(out[0].proxy["dialer-proxy"], json!("__FRONT0__"));

        // A source with its chain switch off keeps the old direct-only
        // measurement even in client-path mode.
        let specs = source_list(&[("air", true, false)]);
        let (out, _) = expand_chains(
            vec![raw("plain", ss("plain", "1.2.3.4"))],
            &fronts,
            Some(&specs),
            false,
            true,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::Direct);
    }

    #[test]
    fn both_switches_off_fall_back_to_direct() {
        let specs = source_list(&[("air", false, false)]);
        assert_eq!(measure_flags("air", Some(&specs)), (true, false));
        assert_eq!(
            measure_flags("air", Some(&source_list(&[("air", true, false)]))),
            (true, false)
        );
        assert_eq!(
            measure_flags("air", Some(&source_list(&[("air", false, true)]))),
            (false, true)
        );
        assert_eq!(
            measure_flags("air", Some(&source_list(&[("air", true, true)]))),
            (true, true)
        );
        // No policy at all: the historical chain-only behaviour.
        assert_eq!(measure_flags("air", None), (false, true));
    }

    // --- collect, end to end ---

    #[tokio::test]
    async fn enabled_sources_yield_aligned_proxies_and_jobs() {
        let fetcher = StubFetcher {
            ok: vec![("air".into(), vec![ss("a", "1.1.1.1"), ss("b", "2.2.2.2")])],
            fail: vec![],
        };
        let sources = vec![spec("air", "collection", "air", true)];
        let out = collect(&fetcher, &sources, true, ChainContext::off(None)).await;
        assert!(out.errors.is_empty());
        assert_eq!(out.dropped, 0);
        assert_eq!(out.proxies.len(), 2);
        assert_eq!(out.jobs.len(), 2);
        assert_eq!(out.jobs[0].source_id, "air");
        assert_eq!(out.jobs[0].proxy_name, "a");
        assert_eq!(out.jobs[0].server_ip, "1.1.1.1");
        assert_eq!(out.jobs[0].variant, "direct");
        assert_eq!(out.jobs[0].role, Role::Direct);
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
        let out = collect(&fetcher, &sources, true, ChainContext::off(None)).await;
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
        let out = collect(&fetcher, &sources, true, ChainContext::off(None)).await;
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
            true,
            ChainContext::off(None),
        )
        .await;
        let names: Vec<&str> = out.jobs.iter().map(|j| j.proxy_name.as_str()).collect();
        assert_eq!(names, vec!["dup", "dup #2"]);
        // Same name but different servers: different ledger identities.
        assert_ne!(out.jobs[0].fingerprint, out.jobs[1].fingerprint);
    }

    #[tokio::test]
    async fn a_chain_round_tests_the_fronts_first_class_and_chains_through_them() {
        let fetcher = StubFetcher {
            ok: vec![
                ("air".into(), vec![chained("n", "1.2.3.4").proxy]),
                ("pool".into(), vec![ss("front-a", "9.9.9.9")]),
            ],
            fail: vec![],
        };
        let admin = FakeAdmin::default();
        let cache = manual();
        let section = chain_section("", "pool", &[], 8);
        let context = ChainContext {
            section: &section,
            publish_prefix: "probe",
            admin: &admin,
            manual: &cache,
            mode: None,
        };
        // The front resource lives behind the same fetcher, keyed by name.
        let sources = vec![spec("air", "collection", "air", true)];
        let out = collect(&fetcher, &sources, true, context).await;

        assert_eq!(out.fronts.len(), 1);
        // One chain variant + the node's direct twin (the source is
        // direct+chain) + the front job, in entries-then-fronts order. The
        // "fronts first" rule is about *phases* in the runner, not this list.
        assert_eq!(out.jobs.len(), 3);
        let chain_job = &out.jobs[0];
        assert_eq!(chain_job.role, Role::Chain);
        assert_eq!(chain_job.variant, "chain");
        assert_eq!(chain_job.front.as_deref(), Some("__FRONT0__"));
        // The chain job is gated on the FRONT's address, not the landing.
        assert_eq!(chain_job.server_ip, "9.9.9.9");
        let twin = &out.jobs[1];
        assert_eq!(twin.role, Role::Direct);
        assert_eq!(
            twin.server_ip, "1.2.3.4",
            "the twin dials the landing itself"
        );
        assert_ne!(twin.fingerprint, chain_job.fingerprint);
        let front_job = &out.jobs[2];
        assert_eq!(front_job.role, Role::Front);
        assert_eq!(front_job.proxy_name, "__FRONT0__");
        assert_eq!(front_job.server_ip, "9.9.9.9");
        // The kernel config holds the front proxy too, or the dialer dangles.
        assert!(out.proxies.iter().any(|p| p["name"] == json!("__FRONT0__")));
        assert_eq!(out.chained, 1);
    }

    #[tokio::test]
    async fn a_direct_round_skips_the_pool_on_purpose() {
        let fetcher = StubFetcher {
            ok: vec![("air".into(), vec![chained("n", "1.2.3.4").proxy])],
            fail: vec![],
        };
        let admin = FakeAdmin::default();
        let cache = manual();
        let section = chain_section("vless://x", "pool", &[], 8);
        let context = ChainContext {
            section: &section,
            publish_prefix: "probe",
            admin: &admin,
            manual: &cache,
            mode: Some("direct"),
        };
        let sources = vec![spec("air", "collection", "air", true)];
        let out = collect(&fetcher, &sources, true, context).await;
        assert!(out.fronts.is_empty(), "the 直连测活 button skips the pool");
        assert_eq!(out.jobs.len(), 1);
        assert_eq!(out.jobs[0].role, Role::Direct);
        // The dialer is stripped by prepare (keep_dialer is empty).
        assert!(out.proxies[0].get("dialer-proxy").is_none());
    }
}
