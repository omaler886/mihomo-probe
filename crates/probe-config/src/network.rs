//! Bounded configuration for DNS expansion, exit verification and round budgets.

use std::collections::{BTreeMap, HashSet};

use probe_domain::DomainPass;
use serde::Serialize;
use serde_json::Value;

const MAX_DNS_VIEWS: usize = 16;
const MAX_URL_LENGTH: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DnsView {
    pub resolver: String,
    pub ecs: Option<String>,
    pub ecs_prefix: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DnsSection {
    pub enabled: bool,
    pub views: BTreeMap<String, DnsView>,
    pub timeout_s: u64,
    pub cache_hours: u64,
    pub concurrency: usize,
    pub max_addresses_per_domain: usize,
    pub max_queries: usize,
}

impl Default for DnsSection {
    fn default() -> Self {
        let views = [
            ("cn", "https://doh.pub/dns-query", "114.114.114.0/24"),
            (
                "overseas",
                "https://cloudflare-dns.com/dns-query",
                "8.8.8.8/24",
            ),
        ]
        .into_iter()
        .map(|(label, resolver, ecs)| {
            (
                label.to_string(),
                DnsView {
                    resolver: resolver.to_string(),
                    ecs: Some(ecs.to_string()),
                    ecs_prefix: 24,
                },
            )
        })
        .collect();
        Self {
            enabled: true,
            views,
            timeout_s: 8,
            cache_hours: 6,
            concurrency: 8,
            max_addresses_per_domain: 64,
            max_queries: 4096,
        }
    }
}

impl DnsSection {
    /// Read legacy DNS settings plus bounded resource controls from a JSON section.
    pub(crate) fn from_value(value: &Value) -> Self {
        let defaults = Self::default();
        let views = match value.get("views") {
            None => defaults.views,
            Some(views) => views
                .as_object()
                .map(|map| {
                    map.iter()
                        .filter_map(|(label, view)| parse_view(label, view))
                        .take(MAX_DNS_VIEWS)
                        .collect()
                })
                .unwrap_or_default(),
        };
        Self {
            enabled: bool_field(value, "enabled", true),
            views,
            timeout_s: integer_field(value, "timeout_s", 8, 1, 120),
            cache_hours: integer_field(value, "cache_hours", 6, 0, 168),
            concurrency: integer_field(value, "concurrency", 8, 1, 64) as usize,
            max_addresses_per_domain: integer_field(value, "max_addresses_per_domain", 64, 1, 1024)
                as usize,
            max_queries: integer_field(value, "max_queries", 4096, 1, 65_536) as usize,
        }
    }

    /// Maximum accepted upstream TTL in seconds; zero explicitly disables caching.
    pub fn cache_limit_secs(&self) -> u64 {
        self.cache_hours.saturating_mul(3600)
    }
}

/// Parse one named view, keeping invalid ECS visible to the DNS query validator.
fn parse_view(label: &str, value: &Value) -> Option<(String, DnsView)> {
    let label = label.trim();
    if label.is_empty() || label.len() > 64 {
        return None;
    }
    let resolver = value.get("resolver")?.as_str()?.trim();
    if resolver.is_empty() || resolver.len() > MAX_URL_LENGTH {
        return None;
    }
    let ecs = value
        .get("ecs")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let prefix = integer_field(value, "ecs_prefix", 24, 0, 255) as u8;
    Some((
        label.to_string(),
        DnsView {
            resolver: resolver.to_string(),
            ecs,
            ecs_prefix: prefix,
        },
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PayloadSection {
    pub enabled: bool,
    pub url: String,
    pub timeout_s: u64,
    pub max_bytes: usize,
    /// Which roles prove a real payload. Python only did this for nodes that
    /// dial through a front (`chain_payload`); `All` extends the same proof to
    /// direct nodes, where a delay-passing node whose TLS handshakes all fail
    /// otherwise looks healthy.
    pub scope: PayloadScope,
}

/// Which nodes pay for a real payload fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PayloadScope {
    /// Python `chain_payload`: fronts and chained nodes only.
    Chain,
    /// Every alive node, direct ones included.
    All,
}

impl Default for PayloadSection {
    fn default() -> Self {
        Self {
            enabled: true,
            url: "https://www.google.com/".into(),
            timeout_s: 15,
            max_bytes: 64 * 1024,
            scope: PayloadScope::Chain,
        }
    }
}

impl PayloadSection {
    /// Read bounded real-payload verification settings; disabled means no traffic.
    fn from_value(value: &Value) -> Self {
        let defaults = Self::default();
        Self {
            enabled: bool_field(value, "enabled", defaults.enabled),
            url: text_field(value, "url", &defaults.url),
            timeout_s: integer_field(value, "timeout_s", 15, 1, 120),
            max_bytes: integer_field(value, "max_bytes", 64 * 1024, 1, 1024 * 1024) as usize,
            scope: match value.get("scope").and_then(Value::as_str) {
                Some(scope) if scope.eq_ignore_ascii_case("all") => PayloadScope::All,
                _ => PayloadScope::Chain,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifySection {
    pub enabled: bool,
    pub entry_check: bool,
    pub exclude_entry_countries: Vec<String>,
    pub domain_pass: DomainPass,
    pub strip_ech: bool,
    pub trace_url: String,
    pub exclude_countries: Vec<String>,
    pub max_nodes: usize,
    pub timeout_s: u64,
    pub chain_payload: PayloadSection,
    pub geo_url: Option<String>,
    pub geo_cache_hours: u64,
    pub geo_timeout_s: u64,
}

impl Default for VerifySection {
    fn default() -> Self {
        Self {
            enabled: true,
            entry_check: true,
            exclude_entry_countries: vec!["CN".into()],
            domain_pass: DomainPass::Any,
            strip_ech: true,
            trace_url: "https://www.cloudflare.com/cdn-cgi/trace".into(),
            exclude_countries: vec!["CN".into()],
            max_nodes: 0,
            timeout_s: 15,
            chain_payload: PayloadSection::default(),
            geo_url: None,
            geo_cache_hours: 168,
            geo_timeout_s: 45,
        }
    }
}

impl VerifySection {
    /// Decode verification policy without coupling entry filtering to DNS expansion.
    pub(crate) fn from_value(value: &Value) -> Self {
        let defaults = Self::default();
        Self {
            enabled: bool_field(value, "enabled", true),
            entry_check: bool_field(value, "entry_check", true),
            exclude_entry_countries: countries(value, "exclude_entry_countries", &["CN"]),
            domain_pass: match value.get("domain_pass").and_then(Value::as_str) {
                Some(mode) if mode.eq_ignore_ascii_case("all") => DomainPass::All,
                _ => DomainPass::Any,
            },
            strip_ech: bool_field(value, "strip_ech", true),
            trace_url: text_field(value, "trace_url", &defaults.trace_url),
            exclude_countries: countries(value, "exclude_countries", &["CN"]),
            max_nodes: integer_field(value, "max_nodes", 0, 0, 100_000) as usize,
            timeout_s: integer_field(value, "timeout_s", 15, 1, 120),
            chain_payload: PayloadSection::from_value(
                value.get("chain_payload").unwrap_or(&Value::Null),
            ),
            geo_url: value
                .get("geo_url")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|url| !url.is_empty() && url.len() <= MAX_URL_LENGTH)
                .map(str::to_string),
            geo_cache_hours: integer_field(value, "geo_cache_hours", 168, 0, 8760),
            geo_timeout_s: integer_field(value, "geo_timeout_s", 45, 1, 120),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatchdogSection {
    pub round_timeout_minutes: u64,
    pub stage_timeout_seconds: u64,
}

impl Default for WatchdogSection {
    fn default() -> Self {
        Self {
            round_timeout_minutes: 20,
            stage_timeout_seconds: 300,
        }
    }
}

impl WatchdogSection {
    /// Bound whole-round and per-stage budgets, including legacy numeric strings.
    pub(crate) fn from_value(value: &Value) -> Self {
        Self {
            round_timeout_minutes: integer_field(value, "round_timeout_minutes", 20, 1, 1440),
            stage_timeout_seconds: integer_field(value, "stage_timeout_seconds", 300, 1, 3600),
        }
    }
}

/// Read a finite numeric value, clamp it to an explicit range, and truncate fractions.
fn integer_field(value: &Value, key: &str, default: u64, low: u64, high: u64) -> u64 {
    value
        .get(key)
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
        })
        .filter(|number| number.is_finite())
        .map(|number| number.clamp(low as f64, high as f64) as u64)
        .unwrap_or(default)
}

/// Read a strict boolean; malformed or absent settings retain the documented default.
fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// Read a nonempty bounded endpoint string without putting it in diagnostics.
fn text_field(value: &Value, key: &str, default: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty() && text.len() <= MAX_URL_LENGTH)
        .unwrap_or(default)
        .to_string()
}

/// Normalize an optional country-code array; an explicit null/empty list disables it.
fn countries(value: &Value, key: &str, default: &[&str]) -> Vec<String> {
    let Some(raw) = value.get(key) else {
        return default.iter().map(|text| text.to_string()).collect();
    };
    if raw.is_null() {
        return Vec::new();
    }
    let Some(codes) = raw.as_array() else {
        return default.iter().map(|text| text.to_string()).collect();
    };
    let mut seen = HashSet::new();
    codes
        .iter()
        .filter_map(Value::as_str)
        .map(|code| code.trim().to_ascii_uppercase())
        .filter(|code| code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_alphabetic()))
        .filter(|code| seen.insert(code.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `scope` decides who pays for a real payload fetch; unknown values keep
    /// Python's rule (chain/front only) instead of silently probing everything.
    #[test]
    fn payload_scope_defaults_to_chain_and_parses_all() {
        assert_eq!(PayloadSection::default().scope, PayloadScope::Chain);
        assert_eq!(
            PayloadSection::from_value(&json!({"scope": "all"})).scope,
            PayloadScope::All
        );
        assert_eq!(
            PayloadSection::from_value(&json!({"scope": "ALL"})).scope,
            PayloadScope::All,
            "the value is case-insensitive"
        );
        assert_eq!(
            PayloadSection::from_value(&json!({"scope": "sometimes"})).scope,
            PayloadScope::Chain,
            "an unknown scope must not silently widen real traffic"
        );
    }

    /// Legacy defaults retain the original resolver, retry and classification policy.
    #[test]
    fn legacy_network_defaults_are_present() {
        let dns = DnsSection::from_value(&Value::Null);
        assert_eq!(dns, DnsSection::default());
        assert_eq!(dns.cache_limit_secs(), 21_600);
        assert_eq!(
            VerifySection::from_value(&Value::Null),
            VerifySection::default()
        );
        assert_eq!(
            WatchdogSection::from_value(&Value::Null),
            WatchdogSection::default()
        );
    }

    /// Disabling classification does not silently disable DNS/address coverage.
    #[test]
    fn dns_and_entry_filter_are_independent() {
        let verify = VerifySection::from_value(&json!({
            "entry_check": false, "domain_pass": "ALL", "strip_ech": false,
            "exclude_entry_countries": ["cn", " CN ", "bad", "jp"],
            "exclude_countries": null
        }));
        assert!(!verify.entry_check);
        assert!(!verify.strip_ech);
        assert_eq!(verify.domain_pass, DomainPass::All);
        assert_eq!(verify.exclude_entry_countries, ["CN", "JP"]);
        assert!(verify.exclude_countries.is_empty());
        assert!(DnsSection::default().enabled);
    }

    /// Work limits cannot become zero semaphores, unbounded downloads or infinite waits.
    #[test]
    fn resource_controls_have_hard_bounds() {
        let dns = DnsSection::from_value(&json!({
            "enabled": false, "views": null, "timeout_s": 0,
            "cache_hours": 0, "concurrency": 100000, "max_addresses_per_domain": -4
        }));
        assert!(!dns.enabled);
        assert!(dns.views.is_empty());
        assert_eq!(dns.timeout_s, 1);
        assert_eq!(dns.cache_limit_secs(), 0);
        assert_eq!(dns.concurrency, 64);
        assert_eq!(dns.max_addresses_per_domain, 1);
        let payload = PayloadSection::from_value(&json!({"max_bytes": 99999999}));
        assert_eq!(payload.max_bytes, 1024 * 1024);
    }

    /// Custom v6 ECS views are retained with their precise prefix and stable label.
    #[test]
    fn custom_views_preserve_ecs_configuration() {
        let dns = DnsSection::from_value(&json!({"views": {
            "local": {"resolver": "http://127.0.0.1:8053/dns-query",
                      "ecs": "2001:db8::/56", "ecs_prefix": 56},
            "disabled": {"resolver": ""}
        }, "timeout_s": "12"}));
        assert_eq!(dns.views.len(), 1);
        assert_eq!(dns.views["local"].ecs_prefix, 56);
        assert_eq!(dns.timeout_s, 12);
    }
}
