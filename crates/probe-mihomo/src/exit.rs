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
use std::net::IpAddr;
use std::time::{Duration, Instant};

pub use probe_domain::ExitIdentity;

const MAX_TRACE_BYTES: usize = 16 * 1024;
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

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

#[derive(Debug, Clone, PartialEq)]
pub struct PayloadMeasurement {
    pub status: u16,
    /// Application payload retained/consumed, excluding protocol overhead and bytes in flight.
    pub bytes: u64,
    pub elapsed_ms: f64,
    pub complete: bool,
}

fn client_via(port: u16, timeout: Duration) -> reqwest::Client {
    // `Proxy::all` (http and https) through the lane's loopback inbound --
    // the same proxy shape urllib's ProxyHandler builds on the Python side.
    let proxy =
        reqwest::Proxy::all(format!("http://127.0.0.1:{port}")).expect("static proxy config");
    reqwest::Client::builder()
        .no_proxy()
        .proxy(proxy)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .expect("static client config")
}

/// Validate a configured target without allowing credentials or fragments into errors.
fn target_url(target: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(target).map_err(|_| "invalid verification URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("invalid verification URL".into());
    }
    Ok(url)
}

/// Return a stable error class, never reqwest's URL-bearing display text.
fn transport_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "verification timed out"
    } else if error.is_connect() {
        "verification connection failed"
    } else {
        "verification transport failed"
    }
    .into()
}

/// Check the configured payload target directly before blaming a failed proxy path.
pub async fn target_available(url: &str, timeout: Duration) -> bool {
    let Ok(url) = target_url(url) else {
        return false;
    };
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
    else {
        return false;
    };
    client
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .send()
        .await
        .is_ok()
}

/// Pull the trace URL through one lane; Ok(identity) on any completed HTTP
/// response, Err(bounded reason) on dial errors / timeouts / resets.
pub async fn egress(port: u16, trace_url: &str, timeout: Duration) -> Result<ExitIdentity, String> {
    let client = client_via(port, timeout);
    let mut resp = client
        .get(target_url(trace_url)?)
        .send()
        .await
        .map_err(transport_error)?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("trace HTTP {}", resp.status().as_u16()));
    }
    if resp
        .content_length()
        .is_some_and(|length| length > MAX_TRACE_BYTES as u64)
    {
        return Err("trace body too large".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(transport_error)? {
        if chunk.len() > MAX_TRACE_BYTES - body.len() {
            return Err("trace body too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&body).map_err(|_| "invalid trace encoding".to_string())?;
    let fields = parse_trace(text);
    if fields.is_empty() {
        return Err("empty trace response".into());
    }
    for key in ["ip", "loc", "asn"] {
        if text
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(field, _)| field.trim() == key)
            .count()
            > 1
        {
            return Err("ambiguous trace identity".into());
        }
    }
    let ip: IpAddr = fields
        .get("ip")
        .ok_or("trace IP missing")?
        .parse()
        .map_err(|_| "invalid trace IP".to_string())?;
    if ip.is_unspecified() || ip.is_multicast() {
        return Err("invalid trace IP".into());
    }
    let country = fields
        .get("loc")
        .map(|value| value.to_ascii_uppercase())
        .filter(|country| probe_domain::geo::is_country_code(country));
    Ok(ExitIdentity {
        ip: Some(ip.to_string()),
        country,
        colo: fields.get("colo").cloned(),
        asn: fields
            .get("asn")
            .and_then(|value| probe_domain::geo::normalize_asn(value)),
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
    let result = fetch_measurement(port, url, timeout, cap).await?;
    Ok((result.status, result.bytes))
}

/// Read a bounded payload prefix through a lane, requesting an HTTP range where supported.
pub async fn fetch_measurement(
    port: u16,
    url: &str,
    timeout: Duration,
    cap: usize,
) -> Result<PayloadMeasurement, String> {
    if cap == 0 || cap > MAX_PAYLOAD_BYTES {
        return Err("invalid payload byte budget".into());
    }
    let started = Instant::now();
    let client = client_via(port, timeout);
    let mut resp = client
        .get(target_url(url)?)
        .header(reqwest::header::RANGE, format!("bytes=0-{}", cap - 1))
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(transport_error)?;
    let status = resp.status().as_u16();
    let mut nbytes = 0u64;
    let mut complete = false;
    while nbytes < cap as u64 {
        match resp.chunk().await {
            Ok(Some(chunk)) => nbytes += (chunk.len() as u64).min(cap as u64 - nbytes),
            Ok(None) => {
                complete = true;
                break;
            }
            Err(err) => return Err(transport_error(err)),
        }
    }
    Ok(PayloadMeasurement {
        status,
        bytes: nbytes,
        complete,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
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
        assert_eq!(
            nbytes, 65_536,
            "payload accounting must not overshoot the cap"
        );
        assert!(nbytes < 1024 * 1024, "cap ignored: {nbytes}");
    }

    /// Hostile trace bodies must not panic and must not invent fields.
    ///
    /// Deterministic (fixed seed, fixed mutation schedule) so a failure is
    /// reproducible; the libFuzzer variant is documented in
    /// TEST_REPORT_RUST.md.
    #[test]
    fn trace_parsing_survives_random_input() {
        const SEEDS: [&str; 4] = [
            "ip=203.0.113.7\nloc=US\ncolo=FRA\n",
            "warp=off\ngateway=on",
            "=empty-key\n\n=\nno-equals-at-all",
            "ip=\u{feff}203.0.113.7\r\nloc=CN\r\n",
        ];
        let mut state = 0x0bad_c0de_1234_5678u64;
        let mut next = |bound: usize| -> usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % (bound.max(1) as u64)) as usize
        };
        for iteration in 0..1_000 {
            let seed = SEEDS[next(SEEDS.len())];
            let mut bytes = seed.as_bytes().to_vec();
            match next(4) {
                0 => {
                    if !bytes.is_empty() {
                        let at = next(bytes.len());
                        bytes[at] = next(256) as u8;
                    }
                }
                1 => {
                    let cut = next(bytes.len() + 1);
                    bytes.truncate(cut);
                }
                2 => {
                    let at = next(bytes.len() + 1);
                    bytes.insert(at, b'=');
                }
                _ => bytes.extend_from_slice(&[0x00, 0xff, b'\n', b'=', b' ']),
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let fields = parse_trace(&text);
            for (key, value) in &fields {
                assert!(
                    !key.is_empty() && !key.contains('=') && key.trim() == key,
                    "iteration {iteration}: bad key {key:?} from {text:?}"
                );
                // A value cannot contain a newline (lines are split on it) and
                // must already be trimmed. An embedded `\r` is allowed on
                // purpose: Python's `partition("=").strip()` keeps it too, and
                // changing that here would be a silent divergence.
                assert!(
                    !value.contains('\n') && value.trim() == value,
                    "iteration {iteration}: bad value {value:?} from {text:?}"
                );
            }
        }
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
