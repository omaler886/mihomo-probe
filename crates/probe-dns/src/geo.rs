//! Batch country lookup for the entry-IP filter (Python `engine.py`'s
//! `lookup_countries` + `_fetch_country_batch`, ported to the transport only
//! -- the cache and the batch size live with the caller/ledger).

/// `BATCH_URL`: the endpoint and the fields the round needs.
pub const BATCH_URL: &str = "http://ip-api.com/batch?fields=query,countryCode,isp,as";

/// One retry, short pause. A 429 is a rate-limit answer rather than a
/// permanent failure, so it is worth asking once more -- but only once: the
/// caller has already stopped filtering this round either way, and hammering
/// the endpoint is what turns a transient limit into a recurring one.
pub const BATCH_RETRY_PAUSE_S: u64 = 2;

/// Python `_fetch_country_batch` calls `urlopen(req, timeout=45)`. Without a
/// timeout a hung endpoint stalls the whole round: the caller has stopped
/// filtering, but it is still awaiting this future.
pub const BATCH_TIMEOUT_S: u64 = 45;

/// Python `str(last)[:80]` -- the error text handed to the caller is bounded.
const ERROR_TEXT_CAP: usize = 80;
const MAX_BATCH_BODY_BYTES: usize = 512 * 1024;

/// One resolved row of the batch endpoint (Python's `normalised` shape).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeoRow {
    /// The endpoint echoes the queried address in the `query` field.
    pub ip: String,
    /// ISO country code, may be empty.
    pub country: Option<String>,
    pub isp: Option<String>,
    pub asn: Option<String>,
}

/// The HTTP half of the batch lookup: POST the addresses, read the rows.
/// Chunking (90 per call) and the cache belong to the caller.
pub struct GeoClient {
    http: reqwest::Client,
    /// Overridable endpoint (tests point it at a local axum stub); the
    /// constructor is the only way in, so the field stays private.
    url: String,
}

impl Default for GeoClient {
    fn default() -> Self {
        Self::with_url(BATCH_URL)
    }
}

impl GeoClient {
    pub fn with_url(url: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("static client config"),
            url: url.to_string(),
        }
    }

    /// One batch call, one retry on failure. Rows missing from the answer stay
    /// missing; the caller treats "fewer rows than asked" as partial.
    pub async fn fetch_batch(&self, ips: &[String]) -> Result<Vec<GeoRow>, String> {
        if ips.is_empty() {
            return Ok(Vec::new());
        }
        if ips.len() > 90 || ips.iter().any(|ip| ip.parse::<std::net::IpAddr>().is_err()) {
            return Err("invalid geo IP batch".into());
        }
        let mut last = String::new();
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(BATCH_RETRY_PAUSE_S)).await;
            }
            // One attempt. A transport error, a non-2xx status and a body that
            // does not parse all take the same path: keep the text and retry.
            // Python's `json.load(resp)` sits inside the same `try` as the
            // request, so a malformed body is retried there too -- a `?` here
            // would make the two disagree on exactly the case a flaky endpoint
            // produces.
            match self.attempt(ips).await {
                Ok(rows) => return Ok(rows),
                Err(err) => last = err,
            }
        }
        Err(last)
    }

    /// A single POST, `Result<rows, error text>`.
    async fn attempt(&self, ips: &[String]) -> Result<Vec<GeoRow>, String> {
        let mut response = self
            .http
            .post(&self.url)
            .timeout(std::time::Duration::from_secs(BATCH_TIMEOUT_S))
            .json(ips)
            .send()
            .await
            .map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        // Deserialising straight to a `Vec` means a *well-formed but non-array*
        // body also lands here as an error and gets retried. Python's
        // `json.load` would accept such a body and only fail later, in
        // `lookup_countries`'s `row.get("query")` on a `str`. Retrying is the
        // deliberate choice -- the stricter of the two, and the endpoint never
        // sends a non-array on a 200.
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BATCH_BODY_BYTES as u64)
        {
            return Err("geo response too large".into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if chunk.len() > MAX_BATCH_BODY_BYTES - body.len() {
                return Err("geo response too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        let rows: Vec<serde_json::Value> =
            serde_json::from_slice(&body).map_err(|_| "invalid geo JSON response".to_string())?;
        if rows.len() > 100 {
            return Err("too many geo response rows".into());
        }
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                // Python is `if row.get("query")` -- a truthiness test, so an
                // empty string is dropped exactly like a missing key.
                let ip = row.get("query")?.as_str()?;
                if ip.is_empty() {
                    return None;
                }
                Some(GeoRow {
                    ip: ip.to_string(),
                    country: row
                        .get("countryCode")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    isp: row
                        .get("isp")
                        .and_then(|v| v.as_str())
                        .map(|value| value.chars().take(256).collect()),
                    asn: row
                        .get("as")
                        .and_then(|value| value.as_str())
                        .map(|value| value.chars().take(128).collect()),
                })
            })
            .collect())
    }
}

/// Classify transport failures without copying an endpoint URL or credentials.
fn transport_error(error: reqwest::Error) -> String {
    let message = if error.is_timeout() {
        "geo request timed out"
    } else if error.is_connect() {
        "geo connection failed"
    } else {
        "geo transport failed"
    };
    message.chars().take(ERROR_TEXT_CAP).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Batch endpoint semantics: the request is a bare JSON array; the
    /// response echoes each address under `query`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_batch_call_posts_a_json_array_and_reads_echoed_addresses() {
        let app = axum::Router::new().fallback(|body: String| async move {
            let ips: Vec<String> = serde_json::from_str(&body).unwrap();
            let rows: Vec<serde_json::Value> = ips
                .into_iter()
                .map(|ip| serde_json::json!({"query": ip, "countryCode": "JP", "isp": "x"}))
                .collect();
            (axum::http::StatusCode::OK, axum::Json(rows))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = GeoClient::with_url(&format!("http://{addr}/batch"));
        let rows = client
            .fetch_batch(&["1.2.3.4".to_string(), "5.6.7.8".to_string()])
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ip, "1.2.3.4");
        assert_eq!(rows[0].country.as_deref(), Some("JP"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_429_is_an_error_after_one_retry_not_a_panic() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let app = axum::Router::new().fallback(move || {
            let hits = hits_clone.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (axum::http::StatusCode::TOO_MANY_REQUESTS, "slow down")
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = GeoClient::with_url(&format!("http://{addr}/batch"));
        let err = client
            .fetch_batch(&["1.2.3.4".to_string()])
            .await
            .unwrap_err();
        assert_eq!(hits.load(Ordering::SeqCst), 2, "exactly one retry");
        assert!(err.contains("429"), "{err}");
    }

    /// A body that is not JSON is retried, not surfaced immediately: Python's
    /// `json.load(resp)` sits inside the same `except` as the request, so a
    /// flaky endpoint that returns an error page once still succeeds on the
    /// retry. A `?` on the parse would have diverged here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_malformed_body_is_retried_like_python() {
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let app = axum::Router::new().fallback(move || {
            let hits = hits_clone.clone();
            async move {
                if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    (axum::http::StatusCode::OK, "not json at all").into_response()
                } else {
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(vec![
                            serde_json::json!({"query": "1.2.3.4", "countryCode": "JP"}),
                        ]),
                    )
                        .into_response()
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = GeoClient::with_url(&format!("http://{addr}/batch"));
        let rows = client.fetch_batch(&["1.2.3.4".to_string()]).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2, "the bad body was retried");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].country.as_deref(), Some("JP"));
    }

    /// Oversized replies and redirects fail without following another endpoint.
    #[tokio::test]
    async fn large_responses_and_redirects_are_rejected() {
        use axum::response::IntoResponse;
        let sink_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = sink_hits.clone();
        let app = axum::Router::new()
            .route(
                "/large",
                axum::routing::post(|| async { vec![b' '; MAX_BATCH_BODY_BYTES + 1] }),
            )
            .route(
                "/redirect",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::TEMPORARY_REDIRECT,
                        [(axum::http::header::LOCATION, "/sink")],
                    )
                        .into_response()
                }),
            )
            .route(
                "/sink",
                axum::routing::post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        axum::Json(Vec::<serde_json::Value>::new())
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for route in ["large", "redirect"] {
            let client = GeoClient::with_url(&format!("http://{address}/{route}"));
            assert!(client.fetch_batch(&["192.0.2.1".into()]).await.is_err());
        }
        assert_eq!(sink_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// Empty input does no IO and malformed/oversized batches fail before connecting.
    #[tokio::test]
    async fn geo_request_inputs_are_bounded() {
        let client = GeoClient::with_url("http://127.0.0.1:1/dummy-private-path");
        assert!(client.fetch_batch(&[]).await.unwrap().is_empty());
        assert!(client.fetch_batch(&["not-an-ip".into()]).await.is_err());
        assert!(client
            .fetch_batch(&vec!["192.0.2.1".into(); 91])
            .await
            .is_err());
        let error = client.fetch_batch(&["192.0.2.1".into()]).await.unwrap_err();
        assert!(!error.contains("dummy-private-path"));
    }
}
