//! Traffic verification through a lane's loopback inbound (Python
//! `core.egress` / `core.fetch`).
//!
//! Two reads, two different questions:
//!
//! * `egress` pulls the Cloudflare trace endpoint through the lane -- the
//!   answer proves *which exit* carried the traffic (ip / loc / colo).
//! * `fetch` pulls a real page -- a 204 delay answer does not prove the path
//!   carries a real TLS session (the 2026-09-28 home-vantage comparison
//!   caught nodes alive on the delay check and dead on every real fetch).
//!
//! Any completed HTTP response counts for both (a 403 still proves the TLS
//! path delivered); what fails a node is a dial error, timeout or TLS reset,
//! which surface as Err. Reading is capped so one check cannot become a
//! download.

use std::collections::BTreeMap;
use std::time::Duration;

/// Fields a trace response exposes, parsed from its `k=v` lines.
pub fn parse_trace(text: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        // Python's `line.partition("=")` split: a key is required, an empty
        // value is fine, and a line without '=' carries nothing.
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                fields.insert(key.to_string(), value.trim().to_string());
            }
        }
    }
    fields
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitIdentity {
    pub ip: Option<String>,
    pub country: Option<String>,
    pub colo: Option<String>,
}

fn bounded(err: impl ToString) -> String {
    err.to_string().chars().take(160).collect()
}

async fn client_via(port: u16, timeout: Duration) -> reqwest::Client {
    // `Proxy::all` (http and https) through the lane's loopback inbound --
    // the same proxy shape urllib's ProxyHandler builds on the Python side.
    let proxy =
        reqwest::Proxy::all(format!("http://127.0.0.1:{port}")).expect("static proxy config");
    reqwest::Client::builder()
        .proxy(proxy)
        .timeout(timeout)
        .build()
        .expect("static client config")
}

/// Pull the trace URL through one lane; Ok(identity) on any completed HTTP
/// response, Err(bounded reason) on dial errors / timeouts / resets.
pub async fn egress(port: u16, trace_url: &str, timeout: Duration) -> Result<ExitIdentity, String> {
    let client = client_via(port, timeout).await;
    let resp = client.get(trace_url).send().await.map_err(bounded)?;
    let text = resp.text().await.map_err(bounded)?;
    let fields = parse_trace(&text);
    if fields.is_empty() {
        return Err("empty trace response".into());
    }
    Ok(ExitIdentity {
        ip: fields.get("ip").cloned(),
        country: fields.get("loc").cloned(),
        colo: fields.get("colo").cloned(),
    })
}

/// Pull a real page through one lane with a read cap; Ok((status, bytes)) on
/// any completed HTTP response.
pub async fn fetch(
    port: u16,
    url: &str,
    timeout: Duration,
    cap: usize,
) -> Result<(u16, u64), String> {
    let client = client_via(port, timeout).await;
    let mut resp = client.get(url).send().await.map_err(bounded)?;
    let status = resp.status().as_u16();
    let mut nbytes = 0u64;
    while nbytes < cap as u64 {
        match resp.chunk().await {
            Ok(Some(chunk)) => nbytes += chunk.len() as u64,
            Ok(None) => break,
            Err(err) => return Err(bounded(err)),
        }
    }
    Ok((status, nbytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;

    /// A mock "exit" reached *through the proxy stack*: reqwest treats the
    /// axum server as an HTTP proxy, so the request arrives in proxy form
    /// (absolute URI) but routes by path -- the same shape a real mihomo
    /// lane inbound sees.
    async fn spawn_mock_exit(body: &'static str, repeat: usize) -> u16 {
        let owned: std::sync::Arc<String> = std::sync::Arc::new(body.repeat(repeat));
        let trace_body = owned.clone();
        let payload_body = owned.clone();
        let app = axum::Router::new()
            .route(
                "/cdn-cgi/trace",
                get(move || {
                    let body = trace_body.clone();
                    async move { (*body).clone() }
                }),
            )
            .route(
                "/payload",
                get(move || {
                    let body = payload_body.clone();
                    async move { (*body).clone() }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        port
    }

    #[test]
    fn trace_parsing_extracts_the_identity_fields() {
        let fields =
            parse_trace("fl=1f4\nh=www.cloudflare.com\nip=203.0.113.10\nloc=US\ncolo=LAX\n");
        assert_eq!(fields.get("ip").map(String::as_str), Some("203.0.113.10"));
        assert_eq!(fields.get("loc").map(String::as_str), Some("US"));
        assert_eq!(fields.get("colo").map(String::as_str), Some("LAX"));
    }

    #[test]
    fn trace_parsing_tolerates_garbage_lines() {
        let fields = parse_trace("noequals\n=orphan\nip=198.51.100.7\n");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields.get("ip").map(String::as_str), Some("198.51.100.7"));
    }

    #[tokio::test]
    async fn egress_reads_the_exit_identity_through_the_lane() {
        let port = spawn_mock_exit("ip=203.0.113.10\nloc=US\ncolo=LAX\n", 1).await;
        let identity = egress(
            port,
            "http://www.cloudflare.com/cdn-cgi/trace",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(identity.ip.as_deref(), Some("203.0.113.10"));
        assert_eq!(identity.country.as_deref(), Some("US"));
        assert_eq!(identity.colo.as_deref(), Some("LAX"));
    }

    #[tokio::test]
    async fn an_empty_trace_body_is_a_failure_not_a_zero_identity() {
        let port = spawn_mock_exit("", 1).await;
        let err = egress(
            port,
            "http://www.cloudflare.com/cdn-cgi/trace",
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(err.contains("empty trace"), "{err}");
    }

    #[tokio::test]
    async fn fetch_counts_bytes_and_stops_at_the_cap() {
        // 1 MiB body behind a 64 KiB cap: the read must stop early and still
        // report a completed HTTP 200.
        let port = spawn_mock_exit("x", 1024 * 1024).await;
        let (status, nbytes) = fetch(port, "http://mock/payload", Duration::from_secs(10), 65_536)
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert!(nbytes >= 65_536, "cap not reached: {nbytes}");
        assert!(nbytes < 1024 * 1024, "cap ignored: {nbytes}");
    }

    #[tokio::test]
    async fn a_dial_failure_is_a_bounded_error() {
        // No proxy listens on port 1: the fetch must fail with a bounded
        // reason, never panic.
        let err = fetch(1, "http://mock/payload", Duration::from_secs(2), 1024)
            .await
            .unwrap_err();
        assert!(err.len() <= 160);
        assert!(!err.is_empty());
    }
}
