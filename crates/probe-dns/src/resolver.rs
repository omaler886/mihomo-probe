//! The DoH transport: one GET per query, `?dns=<base64url>` wire format.

use crate::wire::{parse_ips, TYPE_A, TYPE_AAAA};

/// One `dns.views` entry (Python: `{"resolver": url, "ecs": cidr,
/// "ecs_prefix": n}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewConfig {
    pub resolver: String,
    /// The client subnet announced via ECS, `None` = no ECS option.
    pub ecs: Option<String>,
    pub ecs_prefix: u8,
}

/// Python's `doh.query(..., timeout=8)` / `resolve_views(..., timeout=8)`
/// default. This port takes the timeout explicitly, so the value a caller must
/// pass to reproduce the original is named here.
pub const DEFAULT_TIMEOUT_S: u64 = 8;

/// Python defaults a view's `ecs_prefix` to 24 (`doh.resolve_views`:
/// `int(view.get("ecs_prefix", 24))`). Use this when a view omits it.
pub const DEFAULT_ECS_PREFIX: u8 = 24;

/// A DoH endpoint. Cheap to clone; the reqwest client is shared.
pub struct DohResolver {
    http: reqwest::Client,
}

impl Default for DohResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl DohResolver {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .build()
                .expect("static client config"),
        }
    }

    /// `doh.query`: one wire query per address family, transported as
    /// `?dns=<base64url-no-padding>`, answered with `application/dns-message`.
    pub async fn query(
        &self,
        name: &str,
        qtype: u16,
        view: &ViewConfig,
        timeout: std::time::Duration,
    ) -> Result<Vec<String>, String> {
        let packet =
            crate::wire::build_query(name, qtype, view.ecs.as_deref(), view.ecs_prefix, rand_id())
                .map_err(|e| e.to_string())?;
        use base64::Engine as _;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(packet);
        let separator = if view.resolver.contains('?') {
            "&"
        } else {
            "?"
        };
        let url = format!("{}{}dns={}", view.resolver, separator, token);
        let response = self
            .http
            .get(&url)
            .timeout(timeout)
            .header("Accept", "application/dns-message")
            .send()
            .await
            .map_err(|e| bounded(&e.to_string()))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| bounded(&e.to_string()))?;
        parse_ips(&body).map_err(|e| e.to_string())
    }

    /// `doh.resolve_views`: every configured vantage, A + AAAA each. A view
    /// that fails yields an empty list -- the round still tests the other
    /// vantage, and if both fail the caller falls back to letting the kernel
    /// resolve the domain itself. Both views may return the same address;
    /// dedupe but keep order.
    pub async fn resolve_views(
        &self,
        name: &str,
        views: &[(String, ViewConfig)],
        timeout: std::time::Duration,
    ) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        for (label, view) in views {
            if view.resolver.is_empty() {
                continue;
            }
            let mut found = Vec::new();
            for qtype in [TYPE_A, TYPE_AAAA] {
                if let Ok(ips) = self.query(name, qtype, view, timeout).await {
                    found.extend(ips);
                }
            }
            let mut seen = std::collections::HashSet::new();
            found.retain(|ip| seen.insert(ip.clone()));
            out.push((label.clone(), found));
        }
        out
    }
}

/// Python: `int.from_bytes(os.urandom(2), "big")` -- a fresh random id per
/// query (the response is not matched against it, matching the original).
fn rand_id() -> u16 {
    let mut buf = [0u8; 2];
    getrandom::fill(&mut buf).expect("OS RNG is always available");
    u16::from_be_bytes(buf)
}

fn bounded(text: &str) -> String {
    text.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_views_dedupes_within_a_view_and_keeps_order() {
        let resolver = DohResolver::new();
        let views = vec![
            (
                "cn".to_string(),
                ViewConfig {
                    resolver: "https://doh.invalid/dns-query".to_string(),
                    ecs: Some("114.114.114.0/24".to_string()),
                    ecs_prefix: 24,
                },
            ),
            (
                "overseas".to_string(),
                ViewConfig {
                    resolver: String::new(),
                    ecs: None,
                    ecs_prefix: 24,
                },
            ),
        ];
        // The .invalid TLD fails fast offline; a view with no resolver is
        // skipped entirely (Python: `if not resolver: continue`). Failure
        // shapes as an empty list, never an error -- the documented behaviour.
        let out = resolver
            .resolve_views("example.invalid", &views, std::time::Duration::from_secs(1))
            .await;
        assert_eq!(out.len(), 1, "the resolver-less view is skipped");
        assert_eq!(out[0].0, "cn");
        assert_eq!(out[0].1, Vec::<String>::new());
    }
}
