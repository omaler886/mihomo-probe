//! DNS views, persistent TTL caches and per-address preparation for real rounds.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use probe_config::{DnsSection, DnsView, VerifySection};
use probe_dns::geo::{GeoClient, GeoRow};
use probe_dns::resolver::{DohResolver, ViewConfig};
use probe_dns::wire::{DnsAnswer, TYPE_A, TYPE_AAAA};
use probe_domain::{DomainError, DomainResult};
use probe_source::Role;
use probe_storage::{DnsCacheRecord, GeoCacheRecord, Storage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{Collected, RoundCtx};

const MAX_DOMAINS: usize = 10_000;
const MAX_EXPANDED_JOBS: usize = 20_000;
const GEO_BATCH_SIZE: usize = 90;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One validated family/view query; implementations must bound their transport.
pub trait DnsLookup: Send + Sync {
    fn lookup<'a>(
        &'a self,
        name: &'a str,
        qtype: u16,
        view: &'a DnsView,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<DnsAnswer, String>>;
}

impl DnsLookup for DohResolver {
    fn lookup<'a>(
        &'a self,
        name: &'a str,
        qtype: u16,
        view: &'a DnsView,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<DnsAnswer, String>> {
        Box::pin(async move {
            self.query_answer(
                name,
                qtype,
                &ViewConfig {
                    resolver: view.resolver.clone(),
                    ecs: view.ecs.clone(),
                    ecs_prefix: view.ecs_prefix,
                },
                timeout,
            )
            .await
        })
    }
}

/// Batched entry classification, kept separate from the actual exit identity.
pub trait CountryLookup: Send + Sync {
    fn lookup<'a>(&'a self, ips: &'a [String]) -> BoxFuture<'a, Result<Vec<GeoRow>, String>>;
}

impl CountryLookup for GeoClient {
    fn lookup<'a>(&'a self, ips: &'a [String]) -> BoxFuture<'a, Result<Vec<GeoRow>, String>> {
        Box::pin(async move { self.fetch_batch(ips).await })
    }
}

#[derive(Clone)]
pub struct NetworkServices {
    pub storage: Arc<Mutex<Storage>>,
    pub dns: Arc<dyn DnsLookup>,
    pub countries: Arc<dyn CountryLookup>,
}

impl NetworkServices {
    /// Build real DNS/geo clients sharing the round's ledger and optional geo endpoint.
    pub fn new(storage: Arc<Mutex<Storage>>, verify: &VerifySection) -> Self {
        let countries = verify
            .geo_url
            .as_deref()
            .map(GeoClient::with_url)
            .unwrap_or_default();
        Self {
            storage,
            dns: Arc::new(DohResolver::new()),
            countries: Arc::new(countries),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NetworkSummary {
    pub domains: usize,
    pub addresses: usize,
    pub queries: usize,
    pub cache_hits: usize,
    pub cache_errors: usize,
    pub query_failures: usize,
    pub geo_failures: usize,
    pub excluded_nodes: usize,
    pub incomplete_domains: usize,
}

#[derive(Default)]
struct NameResolution {
    ips: Vec<String>,
    incomplete: bool,
}

#[derive(Default)]
struct RankedAnswers {
    ips: BTreeMap<(usize, usize), String>,
    ranks: HashMap<String, (usize, usize)>,
    incomplete: bool,
}

impl RankedAnswers {
    /// Keep only the earliest planned unique addresses, independent of completion order.
    fn add(&mut self, query_index: usize, answer: DnsAnswer, limit: usize) -> usize {
        let before = self.ips.len();
        for (address_index, address) in answer.ips.into_iter().enumerate() {
            let Ok(ip) = address.parse::<IpAddr>() else {
                self.incomplete = true;
                continue;
            };
            let (ip, rank) = (ip.to_string(), (query_index, address_index));
            if let Some(previous) = self.ranks.get(&ip).copied() {
                if previous <= rank {
                    continue;
                }
                self.ips.remove(&previous);
            }
            self.ranks.insert(ip.clone(), rank);
            self.ips.insert(rank, ip);
            if self.ips.len() > limit {
                let (_, removed) = self.ips.pop_last().expect("overfull address set");
                self.ranks.remove(&removed);
                self.incomplete = true;
            }
        }
        self.ips.len().saturating_sub(before)
    }
}

struct Query {
    name: String,
    label: String,
    qtype: u16,
    view: DnsView,
}

struct QueryResult {
    name: String,
    answer: Option<DnsAnswer>,
    cached: bool,
    cache_error: bool,
}

#[derive(Serialize, Deserialize)]
struct CachedAnswer {
    ips: Vec<String>,
    cname_chain: Vec<String>,
    ttl_secs: u32,
    negative: bool,
}

/// Hash every answer-affecting input, including resolver/ECS, without storing secrets.
pub fn dns_cache_key(name: &str, qtype: u16, label: &str, view: &DnsView) -> String {
    let normalized = probe_dns::wire::canonical_name(name)
        .unwrap_or_else(|_| name.trim_end_matches('.').to_ascii_lowercase());
    let identity = json!([
        normalized,
        qtype,
        label,
        view.resolver,
        view.ecs,
        view.ecs_prefix
    ]);
    format!("{:x}", Sha256::digest(identity.to_string().as_bytes()))
}

/// Execute a synchronous cache operation off the async executor, returning explicit errors.
async fn access_storage<T: Send + 'static>(
    storage: Arc<Mutex<Storage>>,
    operation: impl FnOnce(&Storage) -> DomainResult<T> + Send + 'static,
) -> DomainResult<T> {
    tokio::task::spawn_blocking(move || {
        let storage = storage
            .lock()
            .map_err(|_| DomainError::Storage("cache lock poisoned".into()))?;
        operation(&storage)
    })
    .await
    .map_err(|_| DomainError::Storage("cache worker failed".into()))?
}

/// Decode cached metadata only if its IP family and positive/negative shape are valid.
fn decode_cached(record: DnsCacheRecord, qtype: u16) -> Option<DnsAnswer> {
    let mut answer: CachedAnswer = serde_json::from_str(&record.value_json).ok()?;
    if answer.negative != answer.ips.is_empty()
        || answer.ips.len() > 1024
        || answer.cname_chain.len() > 128
        || answer.ttl_secs == 0
        || record.expires_at - record.stored_at > i64::from(answer.ttl_secs)
    {
        return None;
    }
    if answer.ips.iter().any(|ip| {
        ip.parse::<IpAddr>()
            .ok()
            .is_none_or(|ip| ip.is_ipv4() != (qtype == TYPE_A))
    }) {
        return None;
    }
    let mut seen = HashSet::new();
    answer.ips = answer
        .ips
        .iter()
        .filter_map(|ip| ip.parse::<IpAddr>().ok())
        .map(|ip| ip.to_string())
        .filter(|ip| seen.insert(ip.clone()))
        .collect();
    Some(DnsAnswer {
        ips: answer.ips,
        cname_chain: answer.cname_chain,
        ttl_secs: answer.ttl_secs,
        negative: answer.negative,
    })
}

/// Read a TTL-bound cache entry; invalid cached payloads are explicit misses.
async fn read_answer(
    services: &NetworkServices,
    key: &str,
    qtype: u16,
    limit_secs: u64,
) -> DomainResult<Option<DnsAnswer>> {
    let key = key.to_string();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let record = access_storage(services.storage.clone(), move |storage| {
        storage.read_dns_cache(&key, now)
    })
    .await?;
    let Some(record) = record else {
        return Ok(None);
    };
    if now.saturating_sub(record.stored_at) as u64 >= limit_secs {
        return Ok(None);
    }
    decode_cached(record, qtype)
        .map(Some)
        .ok_or_else(|| DomainError::Storage("invalid cached DNS answer".into()))
}

/// Cache a complete answer for at most its authoritative TTL and configured cap.
async fn cache_answer(
    services: &NetworkServices,
    key: String,
    answer: &DnsAnswer,
    limit_secs: u64,
) -> DomainResult<()> {
    let ttl = u64::from(answer.ttl_secs).min(limit_secs);
    if ttl == 0 {
        return Ok(());
    }
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let value = CachedAnswer {
        ips: answer.ips.clone(),
        cname_chain: answer.cname_chain.clone(),
        ttl_secs: answer.ttl_secs,
        negative: answer.negative,
    };
    let record = DnsCacheRecord {
        value_json: serde_json::to_string(&value)
            .map_err(|_| DomainError::Storage("DNS cache serialization failed".into()))?,
        stored_at: now,
        expires_at: now.saturating_add(ttl as i64),
    };
    access_storage(services.storage.clone(), move |storage| {
        storage.write_dns_cache(&key, &record)
    })
    .await
}

/// Resolve one query with a cache fallback; diagnostics never retain resolver URLs.
async fn resolve_query(query: Query, cfg: DnsSection, services: NetworkServices) -> QueryResult {
    if probe_dns::wire::canonical_name(&query.name).is_err() {
        return QueryResult {
            name: query.name,
            answer: None,
            cached: false,
            cache_error: false,
        };
    }
    let key = dns_cache_key(&query.name, query.qtype, &query.label, &query.view);
    let mut cache_error = false;
    if cfg.cache_limit_secs() > 0 {
        match read_answer(&services, &key, query.qtype, cfg.cache_limit_secs()).await {
            Ok(Some(answer)) => {
                return QueryResult {
                    name: query.name,
                    answer: Some(answer),
                    cached: true,
                    cache_error: false,
                }
            }
            Ok(None) => {}
            Err(_) => cache_error = true,
        }
    }
    let answer = services
        .dns
        .lookup(
            &query.name,
            query.qtype,
            &query.view,
            Duration::from_secs(cfg.timeout_s),
        )
        .await
        .ok();
    if let Some(answer) = &answer {
        cache_error |= cache_answer(&services, key, answer, cfg.cache_limit_secs())
            .await
            .is_err();
    }
    QueryResult {
        name: query.name,
        answer,
        cached: false,
        cache_error,
    }
}

/// Run bounded concurrent view/family queries, retaining deterministic plan order.
async fn resolve_names(
    names: &BTreeSet<String>,
    cfg: &DnsSection,
    services: &NetworkServices,
    ctx: &RoundCtx,
    summary: &mut NetworkSummary,
) -> DomainResult<HashMap<String, NameResolution>> {
    let count = names
        .len()
        .saturating_mul(cfg.views.len())
        .saturating_mul(2);
    if count > cfg.max_queries.min(65_536) {
        return Err(DomainError::Kernel("DNS query budget exceeded".into()));
    }
    let names: Vec<_> = names.iter().cloned().collect();
    let views: Vec<_> = cfg
        .views
        .iter()
        .map(|(label, view)| (label.clone(), view.clone()))
        .collect();
    let queries_per_name = views.len() * 2;
    let mut next_query = 0;
    let mut tasks = tokio::task::JoinSet::new();
    let mut resolved: HashMap<String, RankedAnswers> = HashMap::new();
    let mut addresses = 0;
    loop {
        while tasks.len() < cfg.concurrency.clamp(1, 64) {
            if next_query == count {
                break;
            }
            let index = next_query;
            next_query += 1;
            let (label, view) = &views[(index % queries_per_name) / 2];
            let query = Query {
                name: names[index / queries_per_name].clone(),
                label: label.clone(),
                view: view.clone(),
                qtype: [TYPE_A, TYPE_AAAA][index % 2],
            };
            let (cfg, services) = (cfg.clone(), services.clone());
            tasks.spawn(async move { (index, resolve_query(query, cfg, services).await) });
        }
        if tasks.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = ctx.cancel_token().cancelled() => {
                tasks.shutdown().await;
                return Err(DomainError::Kernel("cancelled during DNS preparation".into()));
            }
            joined = tasks.join_next() => match joined {
                None => break,
                Some(Ok((index, query))) => {
                    summary.queries += 1;
                    summary.cache_hits += usize::from(query.cached);
                    summary.cache_errors += usize::from(query.cache_error);
                    let entry = resolved.entry(query.name).or_default();
                    match query.answer {
                        Some(answer) => addresses += entry.add(index, answer, cfg.max_addresses_per_domain.clamp(1, 1024)),
                        None => {
                            entry.incomplete = true;
                            summary.query_failures += 1;
                        }
                    }
                    if addresses > MAX_EXPANDED_JOBS {
                        tasks.shutdown().await;
                        return Err(DomainError::Kernel("resolved address budget exceeded".into()));
                    }
                },
                Some(Err(_)) => {
                    tasks.shutdown().await;
                    return Err(DomainError::Kernel("DNS preparation worker failed".into()));
                }
            }
        }
    }
    Ok(resolved
        .into_iter()
        .map(|(name, entry)| {
            summary.incomplete_domains += usize::from(entry.incomplete);
            (
                name,
                NameResolution {
                    ips: entry.ips.into_values().collect(),
                    incomplete: entry.incomplete,
                },
            )
        })
        .collect())
}

/// Load valid cached countries and fetch unknown addresses in bounded batches.
async fn lookup_countries(
    ips: &[String],
    verify: &VerifySection,
    services: &NetworkServices,
    ctx: &RoundCtx,
    summary: &mut NetworkSummary,
) -> DomainResult<HashMap<String, String>> {
    if !verify.entry_check || verify.exclude_entry_countries.is_empty() || ips.is_empty() {
        return Ok(HashMap::new());
    }
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let (addresses, age) = (ips.to_vec(), verify.geo_cache_hours.saturating_mul(3600));
    let cached = access_storage(services.storage.clone(), move |storage| {
        storage.read_ip_geo(&addresses, now, age)
    })
    .await;
    let mut countries = HashMap::new();
    match cached {
        Ok(rows) => {
            for row in rows {
                if let Some(country) = row.country {
                    countries.insert(row.ip, country);
                }
            }
        }
        Err(_) => summary.cache_errors += 1,
    }
    let missing: Vec<_> = ips
        .iter()
        .filter(|ip| !countries.contains_key(*ip))
        .cloned()
        .collect();
    for chunk in missing.chunks(GEO_BATCH_SIZE) {
        let fetched = tokio::select! {
            _ = ctx.cancel_token().cancelled() => {
                return Err(DomainError::Kernel("cancelled during entry classification".into()));
            }
            response = tokio::time::timeout(Duration::from_secs(verify.geo_timeout_s), services.countries.lookup(chunk)) => response,
        };
        let Ok(Ok(rows)) = fetched else {
            summary.geo_failures += chunk.len();
            continue;
        };
        let records = normalize_countries(rows, chunk);
        summary.geo_failures += chunk.len().saturating_sub(records.len());
        for row in &records {
            countries.insert(
                row.ip.clone(),
                row.country.clone().expect("normalized country"),
            );
        }
        if verify.geo_cache_hours > 0
            && access_storage(services.storage.clone(), move |storage| {
                storage.write_ip_geo(&records)
            })
            .await
            .is_err()
        {
            summary.cache_errors += 1;
        }
    }
    Ok(countries)
}

/// Accept only requested IPs with real two-letter country codes; do not cache failures.
fn normalize_countries(rows: Vec<GeoRow>, requested: &[String]) -> Vec<GeoCacheRecord> {
    let allowed: HashSet<_> = requested.iter().map(String::as_str).collect();
    let mut found: HashMap<String, GeoCacheRecord> = HashMap::new();
    let mut ambiguous = HashSet::new();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    for row in rows {
        let (Ok(ip), Some(country)) = (row.ip.parse::<IpAddr>(), row.country) else {
            continue;
        };
        let (ip, country) = (ip.to_string(), country.trim().to_ascii_uppercase());
        if !allowed.contains(ip.as_str())
            || ambiguous.contains(&ip)
            || !probe_storage::cache::is_country_code(&country)
        {
            continue;
        }
        if found
            .get(&ip)
            .is_some_and(|previous| previous.country.as_deref() != Some(&country))
        {
            found.remove(&ip);
            ambiguous.insert(ip);
            continue;
        }
        found.insert(
            ip.clone(),
            GeoCacheRecord {
                ip,
                country: Some(country),
                isp: row.isp,
                checked_at: now,
            },
        );
    }
    requested.iter().filter_map(|ip| found.remove(ip)).collect()
}

/// Parse a literal server without confusing bracketed IPv6 with a DNS hostname.
fn literal_ip(server: &str) -> Option<String> {
    let server = server.trim();
    let server = server
        .strip_prefix('[')
        .and_then(|server| server.strip_suffix(']'))
        .unwrap_or(server);
    server.parse::<IpAddr>().ok().map(|ip| ip.to_string())
}

/// Return the effective TLS name before pinning; existing WebSocket Host wins when used.
fn implicit_tls_name(proxy: &Value, server: &str) -> String {
    let authority = (proxy.get("network").and_then(Value::as_str) == Some("ws"))
        .then(|| proxy.pointer("/ws-opts/headers"))
        .flatten()
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("host"))
        })
        .and_then(|(_, value)| value.as_str())
        .filter(|host| !host.is_empty())
        .unwrap_or(server);
    probe_dns::wire::canonical_name(authority).unwrap_or_else(|_| authority.to_string())
}

/// Pin only the dial address, preserving explicitly configured TLS and HTTP authorities.
pub fn pin_server(proxy: &Value, ip: &str) -> Value {
    let mut pinned = proxy.clone();
    let Some(server) = proxy.get("server").and_then(Value::as_str) else {
        return pinned;
    };
    let protocol = proxy.get("type").and_then(Value::as_str).unwrap_or("");
    let name_key = match protocol {
        "vmess" | "vless" => Some("servername"),
        "trojan" | "hysteria" | "hysteria2" | "tuic" | "anytls" | "http" | "socks5" => Some("sni"),
        _ => None,
    };
    let Some(map) = pinned.as_object_mut() else {
        return pinned;
    };
    if literal_ip(server).is_none() {
        if let Some(key) = name_key {
            let is_missing = map
                .get(key)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty);
            if is_missing {
                let name = if key == "servername" {
                    implicit_tls_name(proxy, server)
                } else {
                    probe_dns::wire::canonical_name(server).unwrap_or_else(|_| server.to_string())
                };
                map.insert(key.into(), Value::String(name));
            }
        }
    }
    map.insert("server".into(), Value::String(ip.into()));
    pinned
}

/// Generate a collision-free temporary alias without changing the node's fingerprint.
fn endpoint_alias(base: &str, ip: &str, used: &mut HashSet<String>) -> String {
    let candidate = format!("{base} @{ip}");
    let mut name = candidate.clone();
    let mut suffix = 2;
    while !used.insert(name.clone()) {
        name = format!("{candidate} #{suffix}");
        suffix += 1;
    }
    name
}

/// Build endpoint jobs and exclusions, retaining original proxies for later exports.
fn expand_collected(
    mut collected: Collected,
    resolved: &HashMap<String, NameResolution>,
    countries: &HashMap<String, String>,
    verify: &VerifySection,
) -> DomainResult<Collected> {
    let mut used: HashSet<String> = collected
        .jobs
        .iter()
        .map(|job| job.proxy_name.clone())
        .collect();
    let mut proxies = Vec::new();
    let mut jobs = Vec::new();
    let banned: HashSet<_> = verify
        .exclude_entry_countries
        .iter()
        .map(String::as_str)
        .collect();
    for (proxy, mut job) in collected.proxies.into_iter().zip(collected.jobs) {
        let server = job.original_server.clone().unwrap_or_default();
        if job.role == Role::Front {
            job.test_ip = literal_ip(&server);
            proxies.push(proxy);
            jobs.push(job);
            continue;
        }
        let resolution = resolved.get(&server);
        let literal = literal_ip(&server);
        let ips = literal.clone().map(|ip| vec![ip]).unwrap_or_else(|| {
            resolution
                .map(|entry| entry.ips.clone())
                .unwrap_or_default()
        });
        job.dns_incomplete = resolution.is_some_and(|entry| entry.incomplete);
        let testable: Vec<_> = ips
            .iter()
            .filter(|ip| {
                !verify.entry_check
                    || countries
                        .get(*ip)
                        .is_none_or(|country| !banned.contains(country.as_str()))
            })
            .collect();
        if !ips.is_empty() && testable.is_empty() && !job.dns_incomplete {
            job.excluded_reason = Some("entry_cn".into());
            jobs.push(job);
        } else if testable.is_empty() {
            proxies.push(proxy);
            jobs.push(job);
        } else {
            for ip in testable {
                let mut variant = job.clone();
                let mut pinned = pin_server(&proxy, ip);
                variant.test_ip = Some(ip.clone());
                if literal.is_none() {
                    variant.proxy_name = endpoint_alias(&job.proxy_name, ip, &mut used);
                    pinned["name"] = Value::String(variant.proxy_name.clone());
                }
                if variant.role != Role::Chain {
                    variant.server_ip = ip.clone();
                }
                proxies.push(pinned);
                jobs.push(variant);
            }
        }
        if jobs.len() > MAX_EXPANDED_JOBS {
            return Err(DomainError::Kernel("expanded node budget exceeded".into()));
        }
    }
    collected.proxies = proxies;
    collected.jobs = jobs;
    collected.network.excluded_nodes = collected
        .jobs
        .iter()
        .filter(|job| job.excluded_reason.is_some())
        .map(|job| (&job.source_id, &job.fingerprint))
        .collect::<HashSet<_>>()
        .len();
    Ok(collected)
}

/// Resolve views, classify entries and expand actual candidates before config rendering.
pub async fn prepare_network(
    mut collected: Collected,
    dns: &DnsSection,
    verify: &VerifySection,
    services: &NetworkServices,
    ctx: &RoundCtx,
) -> DomainResult<Collected> {
    if ctx.is_cancelled() {
        return Err(DomainError::Kernel(
            "cancelled before network preparation".into(),
        ));
    }
    if collected.proxies.len() != collected.jobs.len() {
        return Err(DomainError::Config(
            "unaligned collection cannot be expanded".into(),
        ));
    }
    let names: BTreeSet<_> = collected
        .jobs
        .iter()
        .filter(|job| job.role != Role::Front)
        .filter_map(|job| job.original_server.clone())
        .filter(|server| !server.is_empty() && literal_ip(server).is_none())
        .collect();
    if names.len() > MAX_DOMAINS {
        return Err(DomainError::Kernel("DNS domain budget exceeded".into()));
    }
    let mut summary = NetworkSummary {
        domains: names.len(),
        ..Default::default()
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    if access_storage(services.storage.clone(), move |storage| {
        storage.prune_dns_cache(now)
    })
    .await
    .is_err()
    {
        summary.cache_errors += 1;
    }
    let resolved = if dns.enabled && !dns.views.is_empty() {
        resolve_names(&names, dns, services, ctx, &mut summary).await?
    } else {
        HashMap::new()
    };
    let mut ips = BTreeSet::new();
    for job in &collected.jobs {
        if job.role == Role::Front {
            continue;
        }
        if let Some(ip) = job.original_server.as_deref().and_then(literal_ip) {
            ips.insert(ip);
        }
    }
    for resolution in resolved.values() {
        ips.extend(resolution.ips.iter().cloned());
    }
    let ips: Vec<_> = ips.into_iter().collect();
    summary.addresses = ips.len();
    let countries = lookup_countries(&ips, verify, services, ctx, &mut summary).await?;
    collected.network = summary;
    expand_collected(collected, &resolved, &countries, verify)
}

#[cfg(test)]
mod tests;
