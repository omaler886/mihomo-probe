//! The DoH transport: one GET per query, `?dns=<base64url>` wire format.

use std::time::Duration;

use base64::Engine as _;
use reqwest::header::{ACCEPT, CONTENT_TYPE};

use crate::wire::{
    build_query, parse_response, DnsAnswer, MAX_DNS_MESSAGE_SIZE, TYPE_A, TYPE_AAAA,
};

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

/// A DoH transport backed by reqwest's pooled HTTP client.
pub struct DohResolver {
    http: reqwest::Client,
}

impl Default for DohResolver {
    /// Construct the default transport; takes no arguments and returns a resolver.
    fn default() -> Self {
        Self::new()
    }
}

impl DohResolver {
    /// Construct a shared HTTP client that never follows redirects.
    pub fn new() -> Self {
        let builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
        // Local mocks must not depend on a developer's system/environment proxy.
        #[cfg(test)]
        let builder = builder.no_proxy();
        Self {
            http: builder.build().expect("static client config"),
        }
    }

    /// Query `name`/`qtype` from `view` within `timeout`, returning a verified answer.
    ///
    /// The timeout covers connection, headers, and the entire bounded body read.
    /// Errors never contain the resolver URL, query parameters, or response body.
    pub async fn query_answer(
        &self,
        name: &str,
        qtype: u16,
        view: &ViewConfig,
        timeout: Duration,
    ) -> Result<DnsAnswer, String> {
        let id = rand_id()?;
        let packet = build_query(name, qtype, view.ecs.as_deref(), view.ecs_prefix, id)
            .map_err(|e| e.to_string())?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(packet);
        let url = query_url(&view.resolver, &token)?;
        let response = self
            .http
            .get(url)
            .timeout(timeout)
            .header(ACCEPT, "application/dns-message")
            .send()
            .await
            .map_err(transport_error)?;
        validate_response_headers(&response)?;
        let body = read_bounded_body(response).await?;
        parse_response(&body, id, name, qtype).map_err(|e| e.to_string())
    }

    /// Compatibility wrapper for `query_answer`, returning only IPs for the same inputs.
    ///
    /// Valid negative responses return an empty list; malformed replies remain errors.
    pub async fn query(
        &self,
        name: &str,
        qtype: u16,
        view: &ViewConfig,
        timeout: Duration,
    ) -> Result<Vec<String>, String> {
        Ok(self.query_answer(name, qtype, view, timeout).await?.ips)
    }

    /// Resolve `name` through each `views` entry, allowing `timeout` per query.
    /// Return labeled, deduplicated IP lists in view and address order. A view
    /// that fails yields an empty list -- the round still tests the other
    /// vantage, and if both fail the caller falls back to letting the kernel
    /// resolve the domain itself. Both views may return the same address;
    /// dedupe but keep order.
    pub async fn resolve_views(
        &self,
        name: &str,
        views: &[(String, ViewConfig)],
        timeout: Duration,
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

/// Produce a fresh OS-random query ID, returning a sanitized error on RNG failure.
fn rand_id() -> Result<u16, String> {
    let mut buf = [0u8; 2];
    getrandom::fill(&mut buf).map_err(|_| "DNS query ID generation failed".to_string())?;
    Ok(u16::from_be_bytes(buf))
}

/// Parse `endpoint` and replace its DNS parameter with `token`, retaining other pairs.
fn query_url(endpoint: &str, token: &str) -> Result<reqwest::Url, String> {
    let mut url =
        reqwest::Url::parse(endpoint).map_err(|_| "Invalid DoH resolver URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("Invalid DoH resolver URL".to_string());
    }
    let pairs: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key != "dns")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(pairs)
        .append_pair("dns", token);
    Ok(url)
}

/// Validate `response`'s status, unique DNS media type, and declared body size.
fn validate_response_headers(response: &reqwest::Response) -> Result<(), String> {
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    let mut types = response.headers().get_all(CONTENT_TYPE).iter();
    let media_type = types
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next());
    if types.next().is_some()
        || !media_type
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/dns-message"))
    {
        return Err("Invalid DoH response media type".to_string());
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DNS_MESSAGE_SIZE as u64)
    {
        return Err("DoH response exceeds 65535 bytes".to_string());
    }
    Ok(())
}

/// Read `response` incrementally, bounding retained bytes even without Content-Length.
async fn read_bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if chunk.len() > MAX_DNS_MESSAGE_SIZE - body.len() {
            return Err("DoH response exceeds 65535 bytes".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Classify `error` without exposing reqwest's URL-bearing Display/source chain.
fn transport_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        return "DoH request timed out".to_string();
    }
    if error.is_connect() {
        return "DoH connection failed".to_string();
    }
    "DoH transport failed".to_string()
}

#[cfg(test)]
mod tests;
