//! Mihomo external-controller client.
//!
//! Failure semantics ported from Python `core.Core`: a controller-level
//! failure (`KernelError::Controller`) says nothing about any node and must
//! never advance a node's failure streak -- that distinction is why
//! `controller_error` is absent from the terminal reasons (engine.py).

use probe_domain::DomainError;
use serde_json::Value;
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_NODE_TIMEOUT_MS: u64 = 120_000;
const CLIENT_SLACK_MS: u64 = 8_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const RELOAD_TIMEOUT: Duration = Duration::from_secs(40);

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    /// The controller itself is unreachable or misbehaved. Transient by
    /// definition; retryable, and never a node verdict.
    #[error("controller error: {0}")]
    Controller(String),
    /// The controller answered; the *node* failed with a classified reason.
    #[error("node failed: {kind}")]
    Node { kind: String, message: String },
}

impl From<KernelError> for DomainError {
    fn from(value: KernelError) -> Self {
        DomainError::Kernel(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelayOutcome {
    /// Delay in milliseconds as the kernel measured it.
    Ok(u32),
    Failed {
        kind: String,
        message: String,
    },
}

#[derive(Clone)]
pub struct Controller {
    api: String,
    base: Option<reqwest::Url>,
    secret: Option<String>,
    http: reqwest::Client,
}

impl Controller {
    /// Build a client from a base URL and optional bearer secret. Invalid URLs
    /// remain inert: every operation returns a redacted controller error.
    pub fn new(api: &str, secret: Option<&str>) -> Self {
        Self {
            api: api.trim_end_matches('/').to_string(),
            base: parse_http_url(api, false).ok(),
            secret: secret.map(str::to_string),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(Duration::from_secs(5))
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("static client config"),
        }
    }

    /// Validate a base URL eagerly; returns an inert-error-free client or a
    /// redacted controller error. The secret is never put into the URL.
    pub fn try_new(api: &str, secret: Option<&str>) -> Result<Self, KernelError> {
        parse_http_url(api, false)?;
        Ok(Self::new(api, secret))
    }

    /// The full configured controller base URL, which may contain credentials.
    /// Use [`Controller::host_port`] for status payloads or display.
    pub fn url(&self) -> &str {
        &self.api
    }

    /// Return only the HTTP(S) host/port for display, or None for an invalid URL.
    ///
    /// Display is deliberately independent of parse_http_url: a configured URL
    /// that carries credentials, a query or a fragment is rejected for
    /// *requests* (so those parts can never be used or leaked), but the status
    /// payload must still name the kernel it points at -- and must name it
    /// without echoing any of them. The strict parse stays the only source of
    /// request URLs.
    pub fn host_port(&self) -> Option<String> {
        display_host_port(&self.api)
    }

    /// Append encoded path segments to the configured base, preserving its path.
    fn endpoint(&self, segments: &[&str]) -> Result<reqwest::Url, KernelError> {
        if segments.iter().any(|part| matches!(*part, "" | "." | "..")) {
            return Err(controller_error("invalid controller path segment"));
        }
        let mut url = self
            .base
            .clone()
            .ok_or_else(|| controller_error("invalid controller base URL"))?;
        url.path_segments_mut()
            .map_err(|_| controller_error("invalid controller base URL"))?
            .pop_if_empty()
            .extend(segments.iter().copied());
        Ok(url)
    }

    /// Attach the configured bearer secret to a request without exposing it.
    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.secret {
            Some(secret) => req.bearer_auth(secret),
            None => req,
        }
    }

    /// GET /version -- the kernel's readiness probe (Python `wait_ready`).
    pub async fn version(&self) -> Result<Value, KernelError> {
        let req = self.auth(self.http.get(self.endpoint(&["version"])?));
        self.send(req).await
    }

    /// PUT /configs?force=true -- ask the kernel to re-read the config file.
    ///
    /// Returns Ok(true) on 200/204, Ok(false) on config refusal (400/422), and a
    /// controller error for other statuses or transport failures. Neither proves
    /// this round's aliases were installed: callers must confirm recovery or
    /// mark the round inconclusive, never probe the previous configuration.
    pub async fn reload(&self, config_path: &str) -> Result<bool, KernelError> {
        let mut url = self.endpoint(&["configs"])?;
        url.query_pairs_mut().append_pair("force", "true");
        let payload = serde_json::json!({ "path": config_path });
        let req = self
            .auth(self.http.put(url))
            .json(&payload)
            .timeout(RELOAD_TIMEOUT);
        let (status, _) = self.exchange(req).await?;
        match status {
            200 | 204 => Ok(true),
            400 | 422 => Ok(false),
            _ => Err(http_error(status)),
        }
    }

    /// PUT /proxies/{group} -- pin one proxy onto a selector group. This is
    /// how a verification lane holds a node while traffic flows through it
    /// (Python `core.Core.select`).
    pub async fn select(&self, group: &str, name: &str) -> Result<(), KernelError> {
        let url = self.endpoint(&["proxies", group])?;
        let payload = serde_json::json!({ "name": name });
        let req = self
            .auth(self.http.put(url))
            .json(&payload)
            .timeout(REQUEST_TIMEOUT);
        let (status, _) = self.exchange(req).await?;
        match status {
            200 | 204 => Ok(()),
            _ => Err(http_error(status)),
        }
    }

    /// GET /proxies/{name}/delay -- one real traffic test through the kernel.
    pub async fn delay(
        &self,
        proxy: &str,
        url: &str,
        timeout_ms: u64,
        expected: &str,
    ) -> Result<DelayOutcome, KernelError> {
        let target = parse_http_url(url, true)?;
        let timeout_ms = timeout_ms.clamp(1, MAX_NODE_TIMEOUT_MS);
        let mut full = self.endpoint(&["proxies", proxy, "delay"])?;
        full.query_pairs_mut()
            .append_pair("timeout", &timeout_ms.to_string())
            .append_pair("url", target.as_str())
            .append_pair("expected", expected);
        let req = self
            .auth(self.http.get(full))
            .timeout(Duration::from_millis(timeout_ms + CLIENT_SLACK_MS));
        let (status, body) = self.exchange(req).await?;
        if status == 200 {
            let parsed = parse_json(&body)?;
            let delay = parsed
                .get("delay")
                .and_then(|d| d.as_u64())
                .and_then(|delay| u32::try_from(delay).ok())
                .ok_or_else(|| controller_error("invalid controller delay response"))?;
            return Ok(DelayOutcome::Ok(delay));
        }
        Err(delay_failure(status, &body))
    }

    /// Execute a request and retain at most MAX_RESPONSE_BYTES, including errors.
    async fn exchange(&self, req: reqwest::RequestBuilder) -> Result<(u16, Vec<u8>), KernelError> {
        let mut response = req
            .send()
            .await
            .map_err(|_| controller_error("controller request failed"))?;
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(controller_error("controller response exceeded size limit"));
        }
        let status = response.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| controller_error("controller response read failed"))?
        {
            if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
                return Err(controller_error("controller response exceeded size limit"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    }

    /// Execute a JSON operation, returning only bounded, parsed success data.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value, KernelError> {
        let (status, body) = self.exchange(req).await?;
        if !(200..300).contains(&status) {
            return Err(http_error(status));
        }
        parse_json(&body)
    }
}

/// Extract a display-only host[:port] from a configured URL.
///
/// Tolerates the parts parse_http_url rejects (userinfo, path, query,
/// fragment) because status must stay honest about what was configured, but it
/// never echoes them: only the authority's host and port survive. Returns None
/// for anything that is not an http(s) URL with a usable host.
fn display_host_port(text: &str) -> Option<String> {
    // Same hostile-input guards as the request parser: a display must never
    // echo control characters or Windows path separators back to a panel.
    if text.trim() != text || text.chars().any(|c| c.is_control() || c == '\\') {
        return None;
    }
    let (scheme, rest) = text.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    // user:secret@host:port displays as host:port.
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, tail)| tail);
    if host_port.is_empty() {
        return None;
    }
    if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, tail) = bracketed.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        return match tail {
            "" => Some(format!("[{host}]")),
            _ => tail
                .strip_prefix(':')
                .filter(|port| is_port(port))
                .map(|port| format!("[{host}]:{port}")),
        };
    }
    match host_port.rsplit_once(':') {
        None => Some(host_port.to_string()),
        Some((host, port)) if !host.is_empty() && is_port(port) => Some(format!("{host}:{port}")),
        Some(_) => None,
    }
}

/// A non-empty, purely numeric port with no sign or whitespace.
fn is_port(port: &str) -> bool {
    !port.is_empty() && port.chars().all(|character| character.is_ascii_digit())
}

/// Parse an HTTP(S) URL; base queries, all fragments and userinfo are forbidden.
fn parse_http_url(text: &str, allow_query: bool) -> Result<reqwest::Url, KernelError> {
    // The URL parser normalizes away empty userinfo; reject that syntax too.
    let authority = text
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default());
    if authority.is_none_or(|authority| authority.is_empty() || authority.contains('@'))
        || text
            .chars()
            .any(|character| character.is_control() || character == '\\')
        || text.trim() != text
    {
        return Err(controller_error("invalid controller or test URL"));
    }
    let url = reqwest::Url::parse(text)
        .map_err(|_| controller_error("invalid controller or test URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || (!allow_query && url.query().is_some())
    {
        return Err(controller_error("invalid controller or test URL"));
    }
    Ok(url)
}

/// Make a fixed diagnostic; callers must not supply URLs, secrets or body text.
fn controller_error(message: &'static str) -> KernelError {
    KernelError::Controller(message.into())
}

/// Report an HTTP status without retaining any untrusted response text.
fn http_error(status: u16) -> KernelError {
    KernelError::Controller(format!("controller HTTP {status}"))
}

/// Parse bounded JSON and suppress parser diagnostics that could quote input.
fn parse_json(body: &[u8]) -> Result<Value, KernelError> {
    serde_json::from_slice(body).map_err(|_| controller_error("invalid controller JSON response"))
}

/// Only known Mihomo delay-error shapes prove a node test ran. Unknown proxies,
/// auth failures, generic gateways and malformed replies remain infrastructure.
fn delay_failure(status: u16, body: &[u8]) -> KernelError {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let message = parsed
        .as_ref()
        .filter(|value| value.as_object().is_some_and(|object| object.len() == 1))
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str);
    let (kind, message) = match (status, message) {
        (504, Some(message)) if message.eq_ignore_ascii_case("Timeout") => ("timeout", "Timeout"),
        (503, Some("An error occurred in the delay test")) => ("kernel_error", "Delay test failed"),
        _ => return http_error(status),
    };
    KernelError::Node {
        kind: kind.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Request;
    use axum::middleware::Next;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, put};
    use axum::{Json, Router};

    async fn require_bearer(req: Request, next: Next) -> Response {
        // Every controller call must carry the Bearer secret.
        let ok = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            == Some("Bearer s3cret");
        if ok {
            next.run(req).await
        } else {
            axum::http::StatusCode::UNAUTHORIZED.into_response()
        }
    }

    async fn delay_handler(
        axum::extract::Path(_name): axum::extract::Path<String>,
        axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    ) -> Response {
        if q.get("url").map(|u| u.contains("good")) == Some(true) {
            Json(serde_json::json!({ "delay": 233 })).into_response()
        } else {
            // The kernel's real timeout shape: HTTP 504 + {"message": "Timeout"}.
            (
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({ "message": "Timeout" })),
            )
                .into_response()
        }
    }

    async fn spawn_mock_kernel() -> String {
        let app = Router::new()
            .route(
                "/version",
                get(|| async { Json(serde_json::json!({ "version": "v1.19.29" })) }),
            )
            .route(
                "/configs",
                put(|| async { axum::http::StatusCode::NO_CONTENT }),
            )
            .route("/proxies/{name}/delay", get(delay_handler))
            .layer(axum::middleware::from_fn(require_bearer));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn version_requires_and_sends_the_bearer_secret() {
        let api = spawn_mock_kernel().await;
        let controller = Controller::new(&api, Some("s3cret"));
        let version = controller.version().await.unwrap();
        assert_eq!(version["version"], "v1.19.29");
        // Wrong secret must be refused by the mock's auth middleware.
        let bad = Controller::new(&api, Some("wrong"));
        assert!(bad.version().await.is_err());
    }

    #[tokio::test]
    async fn reload_accepts_204_and_reports_refusals() {
        let api = spawn_mock_kernel().await;
        let controller = Controller::new(&api, Some("s3cret"));
        assert!(controller
            .reload("/root/.config/mihomo/config.yaml")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn delay_distinguishes_success_from_timeout() {
        let api = spawn_mock_kernel().await;
        let controller = Controller::new(&api, Some("s3cret"));
        let ok = controller
            .delay("n1", "https://good/generate_204", 5000, "204")
            .await
            .unwrap();
        assert_eq!(ok, DelayOutcome::Ok(233));
        let err = controller
            .delay("n1", "https://bad/generate_204", 5000, "204")
            .await
            .unwrap_err();
        match err {
            KernelError::Node { kind, message } => {
                assert_eq!(kind, "timeout");
                assert_eq!(message, "Timeout");
            }
            other => panic!("expected a node failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unreachable_controller_is_a_controller_error() {
        // Port 1 is reserved and never listening; this must classify as
        // controller-level, not as a node verdict.
        let controller = Controller::new("http://127.0.0.1:1", Some("s3cret"));
        let err = controller.version().await.unwrap_err();
        assert!(matches!(err, KernelError::Controller(_)), "{err:?}");
    }

    /// Transport errors must not copy URL credentials into a verdict or API log.
    #[tokio::test]
    async fn transport_errors_omit_the_request_url() {
        let controller = Controller::new(
            "http://operator:dummy-password@127.0.0.1:1/dummy-private-path",
            None,
        );
        let error = controller.version().await.unwrap_err().to_string();
        assert!(!error.contains("dummy"));
        assert!(!error.contains("operator"));
        assert!(!error.contains("http://"));
    }

    struct MockServer {
        api: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockServer {
        /// Stop only the test-owned mock when an assertion or request finishes.
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// Serve a local router on an ephemeral port and return its owned lifetime.
    async fn serve(app: Router) -> MockServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MockServer { api, task }
    }

    /// Respond identically to every controller operation using synthetic data.
    async fn serve_reply(status: u16, body: &str) -> MockServer {
        let body = body.to_string();
        serve(Router::new().fallback(move || {
            let body = body.clone();
            async move { (axum::http::StatusCode::from_u16(status).unwrap(), body) }
        }))
        .await
    }

    /// Assert infrastructure classification and redaction on both Display/Debug.
    fn assert_infrastructure(error: KernelError) {
        assert!(matches!(error, KernelError::Controller(_)), "{error:?}");
        for text in [error.to_string(), format!("{error:?}")] {
            for private in [
                "private-marker",
                "private-node",
                "private-secret",
                "http://",
                "https://",
            ] {
                assert!(!text.contains(private), "{text}");
            }
        }
    }

    /// Exercise all operations against one failing mock, including write calls.
    async fn assert_all_infrastructure(controller: &Controller) {
        assert_infrastructure(controller.version().await.unwrap_err());
        assert_infrastructure(
            controller
                .reload("/private-marker/config.yaml")
                .await
                .unwrap_err(),
        );
        assert_infrastructure(
            controller
                .select("private-marker", "private-node")
                .await
                .unwrap_err(),
        );
        assert_infrastructure(
            controller
                .delay(
                    "private-node",
                    "https://example.test/private-marker",
                    5,
                    "204",
                )
                .await
                .unwrap_err(),
        );
    }

    #[tokio::test]
    async fn invalid_base_and_target_urls_are_rejected_without_network_io() {
        for api in [
            "file:///private-marker",
            "ftp://example.test",
            "http://operator:private-secret@example.test/private-marker",
            "http://@example.test",
            "http://example.test/#private-marker",
            "http://example.test/?private-marker=1",
            "http:example.test",
            "http://example.test\\private-marker",
            "http://example.test/\nprivate-marker",
        ] {
            assert!(Controller::try_new(api, None).is_err());
            let controller = Controller::new(api, Some("private-secret"));
            // Display may name a parseable host, but it must never echo the
            // parts that made the URL unusable as a base (display_host_port).
            if let Some(shown) = controller.host_port() {
                assert!(
                    !shown.contains("private-marker") && !shown.contains("operator"),
                    "display leaked rejected URL parts: {shown}"
                );
            }
            assert_all_infrastructure(&controller).await;
        }
        // No usable host, or hostile bytes: the display stays blank.
        for api in [
            "file:///private-marker",
            "ftp://example.test",
            "http:example.test",
            "http://example.test\\private-marker",
            "http://example.test/\nprivate-marker",
        ] {
            assert!(Controller::new(api, None).host_port().is_none(), "{api}");
        }

        let controller = Controller::new("http://127.0.0.1:1", None);
        for target in [
            "file:///private-marker",
            "https://user:private-secret@example.test",
            "https://example.test/#private-marker",
        ] {
            assert_infrastructure(
                controller
                    .delay("node", target, 1, "204")
                    .await
                    .unwrap_err(),
            );
        }
        for name in ["", ".", ".."] {
            assert_infrastructure(controller.select(name, "private-node").await.unwrap_err());
        }
    }

    #[tokio::test]
    async fn base_paths_and_all_user_parameters_are_encoded() {
        use axum::extract::{Path, Query};
        use std::collections::HashMap;
        const NAME: &str = "node /%?# &\u{4e1c}\u{4eac}";
        const EXPECTED: &str = "204,200 &extra=1#fragment?+";
        let app = Router::new()
            .route("/controller/base/version", get(|| async {
                Json(serde_json::json!({"version": "synthetic"}))
            }))
            .route("/controller/base/configs", put(
                |Query(query): Query<HashMap<String, String>>, Json(body): Json<Value>| async move {
                    assert_eq!(query.len(), 1);
                    assert_eq!(query["force"], "true");
                    assert_eq!(body["path"], "/synthetic config.yaml");
                    axum::http::StatusCode::NO_CONTENT
                },
            ))
            .route("/controller/base/proxies/{name}", put(
                |Path(name): Path<String>, Json(body): Json<Value>| async move {
                    assert_eq!(name, NAME);
                    assert_eq!(body["name"], NAME);
                    axum::http::StatusCode::NO_CONTENT
                },
            ))
            .route("/controller/base/proxies/{name}/delay", get(
                |Path(name): Path<String>, Query(query): Query<HashMap<String, String>>| async move {
                    assert_eq!(name, NAME);
                    assert_eq!(query.len(), 3);
                    assert_eq!(query["expected"], EXPECTED);
                    assert_eq!(query["url"], "https://example.test/a?x=1&y=%23");
                    assert!(matches!(query["timeout"].as_str(), "1" | "120000"));
                    Json(serde_json::json!({"delay": 0}))
                },
            ))
            .layer(axum::middleware::from_fn(require_bearer));
        let server = serve(app).await;
        for suffix in ["/controller/base", "/controller/base/"] {
            let controller =
                Controller::try_new(&format!("{}{suffix}", server.api), Some("s3cret")).unwrap();
            assert_eq!(controller.version().await.unwrap()["version"], "synthetic");
            assert!(controller.reload("/synthetic config.yaml").await.unwrap());
            controller.select(NAME, NAME).await.unwrap();
            for timeout in [0, u64::MAX] {
                assert_eq!(
                    controller
                        .delay(NAME, "https://example.test/a?x=1&y=%23", timeout, EXPECTED)
                        .await
                        .unwrap(),
                    DelayOutcome::Ok(0)
                );
            }
        }
    }

    #[tokio::test]
    async fn redirects_are_never_followed_for_any_operation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let destination = serve(Router::new().fallback(move || {
            observed.fetch_add(1, Ordering::Relaxed);
            async { Json(serde_json::json!({"delay": 1})) }
        }))
        .await;
        for status in [301, 302, 303, 307, 308] {
            let location = destination.api.clone();
            let source = serve(Router::new().fallback(move || {
                let location = location.clone();
                async move {
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        [(axum::http::header::LOCATION, location)],
                        "private-marker",
                    )
                }
            }))
            .await;
            let controller = Controller::new(&source.api, Some("private-secret"));
            assert_all_infrastructure(&controller).await;
        }
        assert_eq!(requests.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn auth_missing_alias_and_unknown_failures_are_infrastructure() {
        for status in [401, 403, 404, 408, 429, 500, 502] {
            let server = serve_reply(status, r#"{"message":"private-marker Timeout"}"#).await;
            assert_all_infrastructure(&Controller::new(&server.api, Some("private-secret"))).await;
        }
        for (status, body) in [
            (503, r#"{"message":"Proxy not found: private-node"}"#),
            (504, "private-marker gateway timeout"),
            (504, r#"{"message":"proxy private-node does not exist"}"#),
            (
                504,
                r#"{"message":"Timeout","error":"proxy private-node not found"}"#,
            ),
            (400, r#"{"message":"private-marker"}"#),
        ] {
            let server = serve_reply(status, body).await;
            let controller = Controller::new(&server.api, None);
            assert_infrastructure(
                controller
                    .delay("private-node", "https://example.test", 1, "204")
                    .await
                    .unwrap_err(),
            );
        }
    }

    #[tokio::test]
    async fn reload_distinguishes_config_refusal_from_infrastructure_failure() {
        for status in [400, 422] {
            let server = serve_reply(status, "private-marker").await;
            assert!(!Controller::new(&server.api, None)
                .reload("/config.yaml")
                .await
                .unwrap());
        }
        let server = serve_reply(503, "private-marker").await;
        assert_infrastructure(
            Controller::new(&server.api, None)
                .reload("/config.yaml")
                .await
                .unwrap_err(),
        );
    }

    #[tokio::test]
    async fn malformed_json_and_invalid_delays_never_become_node_failures() {
        for body in [
            "private-marker malformed JSON",
            r#"{"delay":"private-marker"}"#,
            r#"{"delay":-1}"#,
            r#"{"delay":4294967296}"#,
            r#"{"delay":1.5}"#,
            r#"{"message":"private-marker"}"#,
            "null",
            "[]",
        ] {
            let server = serve_reply(200, body).await;
            let controller = Controller::new(&server.api, None);
            assert_infrastructure(
                controller
                    .delay("private-node", "https://example.test", 1, "204")
                    .await
                    .unwrap_err(),
            );
            if body.starts_with("private-marker") {
                assert_infrastructure(controller.version().await.unwrap_err());
            }
        }
    }

    #[tokio::test]
    async fn every_operation_bounds_advertised_response_bodies() {
        for status in [200, 400, 503] {
            let server = serve_reply(status, &"x".repeat(MAX_RESPONSE_BYTES + 1)).await;
            assert_all_infrastructure(&Controller::new(&server.api, None)).await;
        }
    }

    /// Serve one raw response, optionally keeping the socket open for timeout
    /// tests; the task is owned and cancelled even if a request rejects early.
    async fn serve_raw(response: Vec<u8>, stall: bool) -> MockServer {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0 && request.len() < 16 * 1024);
                request.extend_from_slice(&buffer[..count]);
            }
            // Early client rejection is expected for oversized responses.
            if stream.write_all(&response).await.is_err() {
                return;
            }
            if stall {
                std::future::pending::<()>().await;
            }
        });
        MockServer { api, task }
    }

    #[tokio::test]
    async fn chunked_oversized_and_truncated_responses_are_infrastructure() {
        let chunk = "x".repeat(MAX_RESPONSE_BYTES / 2 + 1);
        let oversized = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
            chunk.len(), chunk.len()
        );
        for response in [
            oversized.into_bytes(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nprivate-marker".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n\xff".to_vec(),
        ] {
            let server = serve_raw(response, false).await;
            assert_infrastructure(
                Controller::new(&server.api, None)
                    .version()
                    .await
                    .unwrap_err(),
            );
        }
    }

    #[tokio::test]
    async fn stalled_response_body_obeys_the_client_deadline() {
        let server = serve_raw(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{".to_vec(),
            true,
        )
        .await;
        let mut controller = Controller::new(&server.api, None);
        controller.http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), controller.version()).await;
        assert_infrastructure(
            result
                .expect("body read must finish within its deadline")
                .unwrap_err(),
        );
    }

    #[test]
    fn host_port_never_displays_a_base_path_or_secret() {
        let controller =
            Controller::new("http://[::1]:19190/private-marker/", Some("private-secret"));
        assert_eq!(controller.host_port().as_deref(), Some("[::1]:19190"));
        assert_eq!(controller.url(), "http://[::1]:19190/private-marker");
    }

    /// A configured URL the request path rejects must still display its kernel
    /// host -- and only its kernel host.
    #[test]
    fn host_port_displays_only_host_and_port_even_for_rejected_urls() {
        for (configured, expected) in [
            (
                "http://operator:secret@127.0.0.1:19190/private?key=dummy#fragment",
                Some("127.0.0.1:19190"),
            ),
            (
                "http://operator:secret@[::1]:19190/private",
                Some("[::1]:19190"),
            ),
            (
                "https://kernel.example:19190/",
                Some("kernel.example:19190"),
            ),
            ("http://kernel.example/", Some("kernel.example")),
            ("HTTP://KERNEL.EXAMPLE:19190", Some("KERNEL.EXAMPLE:19190")),
            ("not-a-url", None),
            ("file://host/private", None),
            ("http://", None),
            ("http://host:notaport/", None),
            ("http://[::1/", None),
        ] {
            let controller = Controller::new(configured, None);
            assert_eq!(
                controller.host_port().as_deref(),
                expected,
                "configured {configured}"
            );
            assert!(
                !controller
                    .host_port()
                    .unwrap_or_default()
                    .contains("secret"),
                "credentials must never be echoed: {configured}"
            );
        }
    }

    #[tokio::test]
    async fn ambient_proxy_settings_do_not_redirect_requests() {
        const CHILD_BASE: &str = "PROBE_MIHOMO_TEST_CHILD_BASE";
        if let Ok(base) = std::env::var(CHILD_BASE) {
            let version = Controller::new(&base, None).version().await.unwrap();
            assert_eq!(version["version"], "direct-local-mock");
            return;
        }
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let proxy = serve(Router::new().fallback(move || {
            observed.fetch_add(1, Ordering::Relaxed);
            async { axum::http::StatusCode::BAD_GATEWAY }
        }))
        .await;
        let destination = serve_reply(200, r#"{"version":"direct-local-mock"}"#).await;
        // Environment overrides belong only to this child, never the test runner.
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "controller::tests::ambient_proxy_settings_do_not_redirect_requests",
        ]);
        command.env(CHILD_BASE, &destination.api);
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env(name, &proxy.api);
        }
        for name in ["NO_PROXY", "no_proxy"] {
            command.env(name, "");
        }
        let result = tokio::task::spawn_blocking(move || {
            crate::config_check::run_test_command(command, Duration::from_secs(10))
        })
        .await
        .unwrap();
        let (status, output) = result.expect("isolated controller test must complete");
        assert!(
            status.success(),
            "isolated controller fixture failed: {output}"
        );
        assert_eq!(requests.load(Ordering::Relaxed), 0);
    }
}
