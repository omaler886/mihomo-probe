//! Batch country lookup for the entry-IP filter (Python `engine.py`'s
//! `lookup_countries` + `_fetch_country_batch`, ported to the transport only
//! -- the cache and the batch size live with the caller/ledger).

/// `BATCH_URL`: the endpoint and the fields the round needs.
pub const BATCH_URL: &str = "http://ip-api.com/batch?fields=query,countryCode,isp";

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

/// One resolved row of the batch endpoint (Python's `normalised` shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeoRow {
    /// The endpoint echoes the queried address in the `query` field.
    pub ip: String,
    /// ISO country code, may be empty.
    pub country: Option<String>,
    pub isp: Option<String>,
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
                .build()
                .expect("static client config"),
            url: url.to_string(),
        }
    }

    /// One batch call, one retry on failure. Rows missing from the answer stay
    /// missing; the caller treats "fewer rows than asked" as partial.
    pub async fn fetch_batch(&self, ips: &[String]) -> Result<Vec<GeoRow>, String> {
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
        let response = self
            .http
            .post(&self.url)
            .timeout(std::time::Duration::from_secs(BATCH_TIMEOUT_S))
            .json(ips)
            .send()
            .await
            .map_err(|e| bounded(&e.to_string()))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        // Deserialising straight to a `Vec` means a *well-formed but non-array*
        // body also lands here as an error and gets retried. Python's
        // `json.load` would accept such a body and only fail later, in
        // `lookup_countries`'s `row.get("query")` on a `str`. Retrying is the
        // deliberate choice -- the stricter of the two, and the endpoint never
        // sends a non-array on a 200.
        let rows: Vec<serde_json::Value> =
            response.json().await.map_err(|e| bounded(&e.to_string()))?;
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
                    isp: row.get("isp").and_then(|v| v.as_str()).map(str::to_string),
                })
            })
            .collect())
    }
}

fn bounded(text: &str) -> String {
    text.chars().take(ERROR_TEXT_CAP).collect()
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
}
