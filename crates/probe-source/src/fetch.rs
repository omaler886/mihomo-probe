//! Fetching a source's proxies.
//!
//! Port of the part of `store.py` a round actually uses: the `download` route,
//! rendered for `ClashMeta`. Python deliberately does not read a resource's
//! stored `content` -- a remote sub keeps its nodes behind `url` and has an
//! empty `content` field -- and it does not parse share-link dialects itself,
//! because Sub-Store already does and its parser is the one that keeps up with
//! the dialects.
//!
//! The fetch sits behind [`Fetcher`] so a round can be tested without a
//! Sub-Store instance: the collection path is the one piece of this crate that
//! cannot be unit-tested against a fixture.

use std::future::Future;
use std::pin::Pin;

use probe_config::SourceSpec;
use serde_json::Value;

use crate::subscription::parse_proxies;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Error text is bounded before it reaches a log or a ledger: an unbounded
/// response body can carry a URL with credentials in it.
const ERROR_LIMIT: usize = 200;

/// Sub-Store's default render target for this project.
pub const DEFAULT_TARGET: &str = "ClashMeta";

/// `store.BROWSER_UA`. Not cosmetic: a Cloudflare-fronted Sub-Store answers 403
/// with an error code to a `Python-urllib` UA, which reads as "the service is
/// down".
pub const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0 Safari/537.36";

/// Fetches one source's proxies.
///
/// A trait object with a boxed future rather than an `async fn`: RPITIT methods
/// are not dyn-compatible, and a round holds this behind `Arc<dyn Fetcher>` so
/// tests can substitute one.
pub trait Fetcher: Send + Sync {
    /// `Err` is a human-readable reason. It must not contain the backend URL:
    /// a Sub-Store backend may embed a secret path.
    fn fetch<'a>(&'a self, source: &'a SourceSpec) -> BoxFuture<'a, Result<Vec<Value>, String>>;
}

/// The Sub-Store HTTP client.
pub struct SubStoreClient {
    backend: String,
    target: String,
    http: reqwest::Client,
}

impl SubStoreClient {
    pub fn new(backend: &str) -> Self {
        Self::with_target(backend, DEFAULT_TARGET)
    }

    pub fn with_target(backend: &str, target: &str) -> Self {
        Self {
            backend: backend.trim_end_matches('/').to_string(),
            target: target.to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("static client config"),
        }
    }

    /// The route for one source. Python: `collection` downloads under
    /// `/download/collection/<name>`, a single sub under `/download/<name>`.
    fn route(source: &SourceSpec) -> String {
        if source.kind == "sub" {
            format!("/download/{}", encode(&source.name))
        } else {
            format!("/download/collection/{}", encode(&source.name))
        }
    }
}

impl Fetcher for SubStoreClient {
    fn fetch<'a>(&'a self, source: &'a SourceSpec) -> BoxFuture<'a, Result<Vec<Value>, String>> {
        Box::pin(async move {
            let route = Self::route(source);
            let url = format!("{}{route}?target={}", self.backend, encode(&self.target));
            let response = self
                .http
                .get(&url)
                .header("User-Agent", BROWSER_UA)
                .header("Accept", "application/json, text/yaml, text/plain, */*")
                .send()
                .await
                .map_err(|err| bounded(&format!("{} {route}: {err}", kind_of(&err))))?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                // The route, not the URL: the backend may embed a secret path.
                return Err(bounded(&format!(
                    "HTTP {} {route}: {text}",
                    status.as_u16()
                )));
            }
            parse_proxies(&text).map_err(|err| bounded(&err.to_string()))
        })
    }
}

/// Sub-Store's sub-management half (Python `store.Client.upsert`/`delete`),
/// used by the manual front pool: the pasted front text is upserted as a
/// local sub and read back rendered, because Sub-Store already parses every
/// share-link dialect and ours would be the one that rots.
///
/// `Err` carries a bounded reason that never names the backend (same rule as
/// [`Fetcher::fetch`]).
pub trait SubAdmin: Send + Sync {
    /// Create or replace a sub. Returns `"created"`, `"updated"` or
    /// `"patched"` -- the PATCH-after-failed-POST fallback is what makes it
    /// `"patched"`, and that wording is what the log shows.
    fn upsert_sub<'a>(
        &'a self,
        name: &'a str,
        payload: &'a Value,
    ) -> BoxFuture<'a, Result<String, String>>;
    /// Remove a sub. `Ok(false)` means it was not there: a caller cleaning up
    /// something it believes it created needs "removed" to be distinguishable
    /// from "was never there", and both are fine outcomes here.
    fn delete_sub<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<bool, String>>;
}

impl SubStoreClient {
    /// `GET /api/sub/{name}`: `Ok(Some(data))` when present, `Ok(None)` when
    /// Sub-Store answers "missing" (a 404, or one of the 500 shapes that mean
    /// the same -- see [`is_missing`]).
    async fn sub_exists(&self, name: &str) -> Result<bool, String> {
        let path = format!("/api/sub/{}", encode(name));
        let response = self
            .http
            .get(format!("{}{path}", self.backend))
            .header("User-Agent", BROWSER_UA)
            .send()
            .await
            .map_err(|err| bounded(&format!("{} {path}: {err}", kind_of(&err))))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status.is_success() {
            return Ok(true);
        }
        if status.as_u16() == 404 || is_missing(status.as_u16(), &text) {
            return Ok(false);
        }
        Err(bounded(&format!(
            "HTTP {} {path}: {}",
            status.as_u16(),
            summarize(&text)
        )))
    }

    async fn send_json(
        &self,
        method: reqwest::Method,
        path: &str,
        payload: &Value,
    ) -> Result<(), JsonError> {
        let response = self
            .http
            .request(method, format!("{}{path}", self.backend))
            .header("User-Agent", BROWSER_UA)
            .header("Content-Type", "application/json")
            .json(payload)
            .send()
            .await
            .map_err(|err| JsonError {
                status: 0,
                body: bounded(&format!("{} {path}: {err}", kind_of(&err))),
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        Err(JsonError {
            status: status.as_u16(),
            body: text,
        })
    }
}

/// A failed Sub-Store admin call: the status, plus the raw body so the
/// `500-that-means-404` shapes (`is_missing`) stay decidable.
struct JsonError {
    status: u16,
    body: String,
}

impl JsonError {
    /// The bounded, backend-free message a log or an error path may carry.
    fn message(&self) -> String {
        bounded(&format!("HTTP {} : {}", self.status, summarize(&self.body)))
    }
}

impl SubAdmin for SubStoreClient {
    fn upsert_sub<'a>(
        &'a self,
        name: &'a str,
        payload: &'a Value,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let path = format!("/api/sub/{}", encode(name));
            if self.sub_exists(name).await? {
                self.send_json(reqwest::Method::PATCH, &path, payload)
                    .await
                    .map_err(|err| err.message())?;
                return Ok("updated".to_string());
            }
            match self
                .send_json(reqwest::Method::POST, "/api/subs", payload)
                .await
            {
                Ok(()) => Ok("created".to_string()),
                // A concurrent create or a partially-registered name: fall
                // back to PATCH, exactly as Python does.
                Err(_) => {
                    self.send_json(reqwest::Method::PATCH, &path, payload)
                        .await
                        .map_err(|err| err.message())?;
                    Ok("patched".to_string())
                }
            }
        })
    }

    fn delete_sub<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let path = format!("/api/sub/{}", encode(name));
            match self
                .send_json(reqwest::Method::DELETE, &path, &Value::Null)
                .await
            {
                Ok(()) => Ok(true),
                Err(err) if err.status == 404 || is_missing(err.status, &err.body) => Ok(false),
                Err(err) => Err(err.message()),
            }
        })
    }
}

/// `store.Client._is_missing`: Sub-Store answers HTTP 500 for a resource that
/// does not exist (collections come back as `SUBSCRIPTION_NOT_FOUND`, single
/// subs hit an unhandled TypeError). Both mean "absent", not "broken" --
/// treating them as failures is what silently killed the earlier pipelines.
fn is_missing(status: u16, text: &str) -> bool {
    if status != 500 {
        return false;
    }
    if text.contains("SUBSCRIPTION_NOT_FOUND")
        || text.contains("RESOURCE_NOT_FOUND")
        || text.contains("Cannot convert undefined or null to object")
    {
        return true;
    }
    match serde_json::from_str::<Value>(text) {
        Ok(payload) => payload
            .get("error")
            .and_then(|error| error.as_object())
            .map(|error| {
                error
                    .get("code")
                    .and_then(|code| code.as_str())
                    .map(|code| code.contains("NOT_FOUND"))
                    .unwrap_or(false)
                    || error.get("details") == Some(&Value::from(404))
            })
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// The first line of a response body, for error text: a full body can be a
/// wall of HTML, and only the shape of the rejection is interesting.
fn summarize(text: &str) -> String {
    let head = text.trim_start().chars().take(160).collect::<String>();
    head.lines().next().unwrap_or("").to_string()
}

fn kind_of(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else {
        "request"
    }
}

fn bounded(text: &str) -> String {
    text.chars().take(ERROR_LIMIT).collect()
}

/// `urllib.parse.quote(value, safe="")`.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The digest a manual-front cache keys on (`engine._MANUAL_FRONT_SYNCED`
/// stores `sha256(text)`): an unchanged paste costs no Sub-Store write, and
/// losing the cache only costs one idempotent upsert.
pub fn content_digest(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let bytes = hasher.finalize();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn spec(kind: &str, name: &str) -> SourceSpec {
        SourceSpec {
            key: "air".into(),
            kind: kind.into(),
            name: name.into(),
            label: name.into(),
            enabled: true,
            relay: false,
            direct: true,
            chain: true,
        }
    }

    #[test]
    fn a_collection_and_a_sub_use_different_routes() {
        assert_eq!(
            SubStoreClient::route(&spec("collection", "air")),
            "/download/collection/air"
        );
        assert_eq!(SubStoreClient::route(&spec("sub", "air")), "/download/air");
    }

    #[test]
    fn a_name_with_a_slash_is_encoded_not_path_traversed() {
        // A resource name is user input; letting it reach the path unencoded
        // would let it address a different route entirely.
        assert_eq!(
            SubStoreClient::route(&spec("sub", "a/b c")),
            "/download/a%2Fb%20c"
        );
        assert_eq!(
            SubStoreClient::route(&spec("collection", "..")),
            "/download/collection/.."
        );
    }

    #[test]
    fn the_user_agent_is_the_browser_one() {
        // A Cloudflare-fronted Sub-Store answers 403 to a Python-urllib UA.
        assert!(BROWSER_UA.starts_with("Mozilla/5.0"));
    }

    #[test]
    fn error_text_is_bounded_and_never_carries_the_backend() {
        let client = SubStoreClient::new("https://secret.example/Ef7HmpuSjyNOVU2dkBo54a");
        assert_eq!(
            client.backend,
            "https://secret.example/Ef7HmpuSjyNOVU2dkBo54a"
        );
        // The route is what an error names; the backend is only ever used to
        // build the request URL.
        assert!(!SubStoreClient::route(&spec("sub", "air")).contains("secret.example"));
        assert_eq!(bounded(&"x".repeat(500)).chars().count(), ERROR_LIMIT);
    }

    #[test]
    fn the_500_shapes_that_mean_missing_are_recognised() {
        assert!(is_missing(
            500,
            r#"{"code":"SUBSCRIPTION_NOT_FOUND","details":404}"#
        ));
        assert!(is_missing(
            500,
            "Cannot convert undefined or null to object"
        ));
        assert!(is_missing(
            500,
            r#"{"status":"failed","error":{"code":"RESOURCE_NOT_FOUND"}}"#
        ));
        assert!(is_missing(
            500,
            r#"{"status":"failed","error":{"code":"whatever","details":404}}"#
        ));
        assert!(!is_missing(500, r#"{"code":"OTHER"}"#));
        assert!(!is_missing(
            404,
            "plain 404 is handled by its status, not here"
        ));
        assert!(!is_missing(200, "anything"));
    }

    /// A scripted HTTP stub, built on axum like the `probe-substore` tests:
    /// one canned response per request, in order, with the `METHOD path`
    /// lines recorded. This is the whole surface `SubAdmin` uses -- status
    /// codes decide everything, bodies only matter through `is_missing`.
    async fn stub_server(responses: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        use std::collections::VecDeque;

        use axum::extract::State;
        use axum::http::{Method, StatusCode, Uri};
        use axum::response::{IntoResponse, Response};

        #[derive(Clone)]
        struct Stub {
            script: Arc<Mutex<VecDeque<(u16, &'static str)>>>,
            log: Arc<Mutex<Vec<String>>>,
        }

        async fn handle(State(stub): State<Stub>, method: Method, uri: Uri) -> Response {
            stub.log
                .lock()
                .unwrap()
                .push(format!("{method} {}", uri.path()));
            let (status, body) = stub
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or((500, "script exhausted"));
            (StatusCode::from_u16(status).unwrap(), body.to_string()).into_response()
        }

        let stub = Stub {
            script: Arc::new(Mutex::new(responses.into())),
            log: Arc::new(Mutex::new(Vec::new())),
        };
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(stub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), stub.log)
    }

    fn manual_payload() -> Value {
        json!({"name": "probe-front-manual", "source": "local", "content": "vless://x"})
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_absent_sub_is_created_via_post() {
        let (backend, requests) = stub_server(vec![
            (500, r#"{"code":"SUBSCRIPTION_NOT_FOUND","details":404}"#),
            (200, "{}"),
        ])
        .await;
        let client = SubStoreClient::new(&backend);
        let action = client
            .upsert_sub("probe-front-manual", &manual_payload())
            .await;
        assert_eq!(action.unwrap(), "created");
        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                "GET /api/sub/probe-front-manual".to_string(),
                "POST /api/subs".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn an_existing_sub_is_patched_not_recreated() {
        let (backend, requests) = stub_server(vec![(200, "{}"), (200, "{}")]).await;
        let client = SubStoreClient::new(&backend);
        let action = client
            .upsert_sub("probe-front-manual", &manual_payload())
            .await;
        assert_eq!(action.unwrap(), "updated");
        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                "GET /api/sub/probe-front-manual".to_string(),
                "PATCH /api/sub/probe-front-manual".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_post_falls_back_to_patch() {
        // A concurrent create between the existence check and the POST lands
        // here; Python records it as "patched" and so does this port.
        let (backend, requests) = stub_server(vec![
            (500, r#"{"code":"SUBSCRIPTION_NOT_FOUND","details":404}"#),
            (500, "conflict"),
            (200, "{}"),
        ])
        .await;
        let client = SubStoreClient::new(&backend);
        let action = client
            .upsert_sub("probe-front-manual", &manual_payload())
            .await;
        assert_eq!(action.unwrap(), "patched");
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0], "GET /api/sub/probe-front-manual");
        assert_eq!(requests[1], "POST /api/subs");
        assert_eq!(requests[2], "PATCH /api/sub/probe-front-manual");
    }

    #[tokio::test]
    async fn a_delete_404_reads_as_never_there() {
        let (backend, _) = stub_server(vec![(404, "nope")]).await;
        let client = SubStoreClient::new(&backend);
        assert!(!client.delete_sub("probe-front-manual").await.unwrap());
    }

    #[tokio::test]
    async fn a_delete_500_that_means_missing_also_reads_as_never_there() {
        let (backend, _) = stub_server(vec![(
            500,
            r#"{"code":"SUBSCRIPTION_NOT_FOUND","details":404}"#,
        )])
        .await;
        let client = SubStoreClient::new(&backend);
        assert!(!client.delete_sub("probe-front-manual").await.unwrap());
    }

    #[tokio::test]
    async fn a_successful_delete_reports_removed() {
        let (backend, _) = stub_server(vec![(200, "{}")]).await;
        let client = SubStoreClient::new(&backend);
        assert!(client.delete_sub("probe-front-manual").await.unwrap());
    }

    #[tokio::test]
    async fn a_real_error_still_errors_and_stays_backend_free() {
        // A 403 from a Cloudflare-fronted backend is not "missing": swallowing
        // it would leave a manual sub the operator believes was cleaned up.
        let (backend, _) = stub_server(vec![(403, "blocked")]).await;
        let client = SubStoreClient::new(&backend);
        let err = client.delete_sub("probe-front-manual").await.unwrap_err();
        assert!(err.contains("HTTP 403"), "{err}");
        assert!(!err.contains(&backend), "no backend in the error: {err}");
    }
}
