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

/// `store.Client.retries`: the first attempt plus two retries.
const RETRIES: u32 = 2;

/// Redirects the client will follow before giving up.
///
/// `urllib` follows up to ten by default; a subscription route has no
/// business bouncing at all, so a small bound keeps a redirect loop from
/// turning one fetch into ten.
const MAX_REDIRECTS: usize = 5;

/// Largest subscription body this client will read.
///
/// A subscription is a node list, not a file transfer: the live collection is
/// tens of kilobytes, so eight mebibytes is generous -- and it stops a
/// misbehaving upstream from exhausting memory before any parser sees a byte.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// `store.Client._request`: `time.sleep(0.8 * (attempt + 1))` between attempts.
const RETRY_PAUSE_MS: u64 = 800;

/// Read a response body, refusing anything past [`MAX_BODY_BYTES`].
///
/// Streaming rather than `Response::text` on purpose: the limit has to apply
/// while the bytes arrive, not after they are already in memory.
async fn bounded_body(mut response: reqwest::Response) -> Result<String, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| bounded(&format!("{}: {err}", kind_of(&err))))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(format!("response body exceeds {MAX_BODY_BYTES} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Whether a failed attempt is worth repeating.
///
/// Python retries every 5xx and every transport error, and raises at once on
/// any other status: a 4xx (including Cloudflare's 403) is the server's
/// answer rather than a hiccup, and a 500 that actually reports a missing
/// resource is classified before the retry decision (`_is_missing`).
fn retryable_status(status: u16) -> bool {
    status == 0 || status >= 500
}

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
                .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
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
            let mut attempt = 0;
            loop {
                let outcome = self
                    .http
                    .get(&url)
                    .header("User-Agent", BROWSER_UA)
                    .header("Accept", "application/json, text/yaml, text/plain, */*")
                    .send()
                    .await;
                let failure = match outcome {
                    Ok(response) => {
                        let status = response.status();
                        let text = match bounded_body(response).await {
                            Ok(text) => text,
                            Err(err) => {
                                // A body we cannot finish reading (too large, or a
                                // broken stream) is a transport failure, not a
                                // verdict about the subscription.
                                if attempt >= RETRIES {
                                    return Err(err);
                                }
                                attempt += 1;
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    RETRY_PAUSE_MS * u64::from(attempt),
                                ))
                                .await;
                                continue;
                            }
                        };
                        if status.is_success() {
                            // A body the parser rejects is an answer, not a
                            // hiccup: Python parses outside the retry loop too.
                            return parse_proxies(&text).map_err(|err| bounded(&err.to_string()));
                        }
                        // The route, not the URL: the backend may embed a secret path.
                        let message = bounded(&format!("HTTP {} {route}: {text}", status.as_u16()));
                        if !retryable_status(status.as_u16()) {
                            return Err(message);
                        }
                        message
                    }
                    // Transport failures: classify only, never format reqwest's
                    // Display -- it embeds the full request URL, and the
                    // backend URL carries the secret path (the route, which
                    // is public, is already named). Python parity: fixed
                    // strings, like probe-dns's resolver.
                    Err(err) => bounded(&format!("{} {route}", kind_of(&err))),
                };
                if attempt >= RETRIES {
                    return Err(failure);
                }
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(
                    RETRY_PAUSE_MS * u64::from(attempt),
                ))
                .await;
            }
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
    /// Every sub the backend holds, as `(name, source)` -- Python
    /// `store.get_json("/api/subs")`. The push mode needs it to find the
    /// `-local` subs a retired source left behind.
    ///
    /// Defaults to "nothing listed", so an admin that cannot enumerate still
    /// compiles and its prune becomes a no-op: refusing to delete what cannot be
    /// listed is the only safe direction.
    fn list_subs<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SubEntry>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// One sub the backend holds, reduced to what the push mode reasons about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubEntry {
    pub name: String,
    /// Sub-Store's `source` field; `None` when the item does not carry one.
    pub source: Option<String>,
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
            .map_err(|err| bounded(&format!("{} {path}", kind_of(&err))))?;
        let status = response.status();
        // An unreadable or oversized body reads as empty here: the status code
        // already decided this call, and the text only classifies the failure.
        let text = bounded_body(response).await.unwrap_or_default();
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
        let mut attempt = 0;
        loop {
            let outcome = self
                .http
                .request(method.clone(), format!("{}{path}", self.backend))
                .header("User-Agent", BROWSER_UA)
                .header("Content-Type", "application/json")
                .json(payload)
                .send()
                .await;
            let failure = match outcome {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(());
                    }
                    let text = bounded_body(response).await.unwrap_or_default();
                    let error = JsonError {
                        status: status.as_u16(),
                        body: text,
                    };
                    // Python decides `_is_missing` before the retry branch, so a
                    // 500 that really reports "not found" is never repeated.
                    if !retryable_status(error.status) || is_missing(error.status, &error.body) {
                        return Err(error);
                    }
                    error
                }
                Err(err) => JsonError {
                    status: 0,
                    // Classify only: reqwest's Display embeds the request
                    // URL, and this backend's URL is the secret itself.
                    body: bounded(&format!("{} {path}", kind_of(&err))),
                },
            };
            if attempt >= RETRIES {
                return Err(failure);
            }
            attempt += 1;
            tokio::time::sleep(std::time::Duration::from_millis(
                RETRY_PAUSE_MS * u64::from(attempt),
            ))
            .await;
        }
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

    fn list_subs<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SubEntry>, String>> {
        Box::pin(async move {
            let response = self
                .http
                .get(format!("{}/api/subs", self.backend))
                .header("User-Agent", BROWSER_UA)
                .send()
                .await
                .map_err(|err| bounded(&format!("{} /api/subs", kind_of(&err))))?;
            let status = response.status();
            let text = bounded_body(response).await.unwrap_or_default();
            if !status.is_success() {
                return Err(bounded(&format!(
                    "HTTP {} /api/subs: {}",
                    status.as_u16(),
                    summarize(&text)
                )));
            }
            let items: Vec<Value> = serde_json::from_str(&text)
                .map_err(|_| "GET /api/subs returned unusable JSON".to_string())?;
            Ok(items
                .iter()
                .filter_map(|item| {
                    Some(SubEntry {
                        name: item.get("name").and_then(Value::as_str)?.to_string(),
                        source: item
                            .get("source")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect())
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
    } else if err.is_redirect() {
        "redirect"
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
            export: true,
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

    const CLASH_BODY: &str =
        r#"{"proxies":[{"name":"n1","type":"ss","server":"1.2.3.4","port":443}]}"#;

    /// `store.Client.retries = 2`: two 5xx answers then success is one source
    /// fetched, not a source lost for the round.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_transient_failure_is_retried_before_the_source_fails() {
        let (backend, requests) =
            stub_server(vec![(500, "boom"), (500, "boom"), (200, CLASH_BODY)]).await;
        let client = SubStoreClient::new(&backend);
        let proxies = client.fetch(&spec("sub", "air")).await.unwrap();
        assert_eq!(proxies.len(), 1, "the third attempt carries the source");
        assert_eq!(
            requests.lock().unwrap().len(),
            3,
            "Python retries twice and no more"
        );
    }

    /// A redirect loop must stop at the client's bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_redirect_loop_is_bounded() {
        use axum::http::{header, StatusCode};
        use axum::response::{IntoResponse, Response};

        async fn always_redirect() -> Response {
            (StatusCode::FOUND, [(header::LOCATION, "/download/air")], "").into_response()
        }

        let app = axum::Router::new().fallback(always_redirect);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = SubStoreClient::new(&format!("http://{addr}"));
        let error = client.fetch(&spec("sub", "air")).await.unwrap_err();
        assert!(
            error.to_lowercase().contains("redirect"),
            "a redirect loop must fail as a redirect, got: {error}"
        );
    }

    /// A body past [`MAX_BODY_BYTES`] is refused, not buffered whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_oversized_body_is_refused() {
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};

        async fn huge() -> Response {
            (StatusCode::OK, "x".repeat(MAX_BODY_BYTES + 1024)).into_response()
        }

        let app = axum::Router::new().fallback(huge);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = SubStoreClient::new(&format!("http://{addr}"));
        let error = client.fetch(&spec("sub", "air")).await.unwrap_err();
        assert!(
            error.contains("exceeds"),
            "an oversized body must be named as such, got: {error}"
        );
    }

    /// A 4xx is the answer, not a hiccup: retrying it would only delay the round.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_error_is_never_retried() {
        let (backend, requests) = stub_server(vec![(404, "gone"), (200, CLASH_BODY)]).await;
        let client = SubStoreClient::new(&backend);
        let error = client.fetch(&spec("sub", "air")).await.unwrap_err();
        assert!(error.contains("HTTP 404"), "{error}");
        assert_eq!(requests.lock().unwrap().len(), 1, "a 4xx is not retried");
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
        // here; Python records it as "patched" and so does this port. Every
        // attempt is scripted because `store.Client.retries` repeats a 5xx
        // POST twice before `upsert` gives up on it and patches instead.
        let (backend, requests) = stub_server(vec![
            (500, r#"{"code":"SUBSCRIPTION_NOT_FOUND","details":404}"#),
            (500, "conflict"),
            (500, "conflict"),
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
        assert_eq!(requests[2], "POST /api/subs", "a 5xx POST is retried");
        assert_eq!(requests[3], "POST /api/subs", "twice, like store.Client");
        assert_eq!(requests[4], "PATCH /api/sub/probe-front-manual");
    }

    /// The retry inside `_request` is what turns a one-off 5xx into a create
    /// instead of a needless PATCH fallback.
    #[tokio::test]
    async fn a_transient_post_failure_that_recovers_still_creates() {
        let (backend, requests) = stub_server(vec![
            (500, r#"{"code":"SUBSCRIPTION_NOT_FOUND","details":404}"#),
            (500, "hiccup"),
            (200, "{}"),
        ])
        .await;
        let client = SubStoreClient::new(&backend);
        let action = client
            .upsert_sub("probe-front-manual", &manual_payload())
            .await;
        assert_eq!(action.unwrap(), "created");
        assert_eq!(
            requests.lock().unwrap().len(),
            3,
            "GET, the failed POST, and the retried POST that succeeded"
        );
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

    /// reqwest's Display embeds the full request URL, and the backend URL is
    /// the credential (the secret path). A transport failure must classify
    /// the error without echoing it -- the round note and the events table
    /// both end up on the shared ledger the panel renders.
    #[tokio::test]
    async fn transport_failures_never_leak_the_backend_url() {
        // A port nobody listens on: connection refused, deterministically.
        let backend = "http://127.0.0.1:1/secret-backend-path".to_string();
        let client = SubStoreClient::new(&backend);
        let source = SourceSpec {
            key: "air".into(),
            kind: "sub".into(),
            name: "air".into(),
            label: "air".into(),
            enabled: true,
            relay: false,
            direct: true,
            chain: false,
            export: true,
        };
        let err = Fetcher::fetch(&client, &source).await.unwrap_err();
        assert!(
            err.contains("connect"),
            "classified as a connect error: {err}"
        );
        assert!(
            !err.contains("secret-backend-path") && !err.contains("127.0.0.1:1"),
            "no backend in the transport error: {err}"
        );
        assert!(!err.contains("http"), "no URL fragments at all: {err}");

        let err = client.delete_sub("probe-front-manual").await.unwrap_err();
        assert!(
            !err.contains("secret-backend-path"),
            "admin transport error stays backend-free: {err}"
        );
        let err = client
            .upsert_sub("probe-front-manual", &manual_payload())
            .await
            .unwrap_err();
        assert!(
            !err.contains("secret-backend-path"),
            "upsert transport error stays backend-free: {err}"
        );
    }
}
