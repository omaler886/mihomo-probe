//! Mihomo external-controller client.
//!
//! Failure semantics ported from Python `core.Core`: a controller-level
//! failure (`KernelError::Controller`) says nothing about any node and must
//! never advance a node's failure streak -- that distinction is why
//! `controller_error` is absent from the terminal reasons (engine.py).

use probe_domain::DomainError;
use serde_json::Value;

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
    secret: Option<String>,
    http: reqwest::Client,
}

impl Controller {
    pub fn new(api: &str, secret: Option<&str>) -> Self {
        Self {
            api: api.trim_end_matches('/').to_string(),
            secret: secret.map(str::to_string),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("static client config"),
        }
    }

    /// The configured controller base URL (scheme + host + port). Callers
    /// that must display it may only show host:port -- never a URL that could
    /// embed credentials.
    pub fn url(&self) -> &str {
        &self.api
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.secret {
            Some(secret) => req.bearer_auth(secret),
            None => req,
        }
    }

    /// GET /version -- the kernel's readiness probe (Python `wait_ready`).
    pub async fn version(&self) -> Result<Value, KernelError> {
        let url = format!("{}/version", self.api);
        let req = self.auth(self.http.get(&url));
        self.send(req).await
    }

    /// PUT /configs?force=true -- ask the kernel to re-read the config file.
    ///
    /// Returns Ok(true) when the kernel reloaded, Ok(false) when it refused
    /// (the caller recreates the kernel, as Python `start_and_load` does), and
    /// Err(KernelError::Controller) when the controller itself is gone. Every
    /// controller-level failure is soft on purpose: the 2026-09-29 incident
    /// (a busy event loop outliving a 20s read) must degrade to a recreate,
    /// never abort the round.
    pub async fn reload(&self, config_path: &str) -> Result<bool, KernelError> {
        let url = format!("{}/configs?force=true", self.api);
        let payload = serde_json::json!({ "path": config_path });
        let req = self
            .auth(self.http.put(&url))
            .json(&payload)
            .timeout(std::time::Duration::from_secs(40));
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 200 || status == 204 {
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(err) => Err(KernelError::Controller(err.to_string())),
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
        let quoted = urlencode_component(proxy);
        let full = format!(
            "{}/proxies/{quoted}/delay?timeout={timeout_ms}&url={}&expected={expected}",
            self.api,
            urlencode_component(url),
        );
        // The kernel answers within the delay budget; the client window adds
        // 8s of slack exactly as core.delay does.
        let req = self
            .auth(self.http.get(&full))
            .timeout(std::time::Duration::from_millis(timeout_ms + 8_000));
        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(err) => return Err(KernelError::Controller(err.to_string())),
        };
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status == 200 {
            let parsed: Value = serde_json::from_str(&body).map_err(|_| KernelError::Node {
                kind: "bad_response".into(),
                message: body.chars().take(120).collect(),
            })?;
            let delay = parsed
                .get("delay")
                .and_then(|d| d.as_u64())
                .ok_or_else(|| KernelError::Node {
                    kind: "bad_delay".into(),
                    message: body.chars().take(120).collect(),
                })?;
            let delay = u32::try_from(delay).map_err(|_| KernelError::Node {
                kind: "bad_delay".into(),
                message: body.chars().take(120).collect(),
            })?;
            return Ok(DelayOutcome::Ok(delay));
        }
        let failure = super::classify_failure(status, &body);
        Err(KernelError::Node {
            kind: failure.kind,
            message: failure.message,
        })
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value, KernelError> {
        let resp = req
            .send()
            .await
            .map_err(|err| KernelError::Controller(err.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(KernelError::Controller(format!(
                "HTTP {} {body}",
                status.as_u16()
            )));
        }
        serde_json::from_str(&body)
            .map_err(|e| KernelError::Controller(format!("non-JSON reply: {e}")))
    }
}

fn urlencode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
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
}
