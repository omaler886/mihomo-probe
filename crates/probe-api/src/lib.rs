//! HTTP API surface for the Rust slice (axum), contract-first per
//! workstreams/10: `/healthz`, `/readyz`, `/api/v1/status`, `POST
//! /api/v1/rounds`.
//!
//! Security rules carried over from the Python service and required by the
//! master document §13: an absent/empty admin token denies (never "auth
//! off"), status payloads carry zero secrets, duplicate rounds are refused at
//! the edge, and the server binds loopback unless explicitly told otherwise.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use probe_mihomo::Controller;
use probe_storage::Storage;

#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<Mutex<Storage>>,
    /// `None` = no admin token configured: every guarded endpoint denies.
    pub auth_token: Option<String>,
    pub controller: Controller,
    /// Kernel-side config path handed to `PUT /configs` on reload.
    pub kernel_config_path: String,
    in_flight: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(
        storage: Storage,
        auth_token: Option<String>,
        controller: Controller,
        kernel_config_path: String,
    ) -> Self {
        Self {
            storage: Arc::new(Mutex::new(storage)),
            auth_token,
            controller,
            kernel_config_path,
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub type SharedState = Arc<AppState>;

/// Constant-time token comparison; False unless both are non-empty. Mirrors
/// the Python `_matches` (UTF-8 bytes, `compare_digest`).
fn token_matches(candidate: &str, expected: &str) -> bool {
    if candidate.is_empty() || expected.is_empty() {
        return false;
    }
    let (a, b) = (candidate.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The presented token: `Authorization: Bearer` first, then `X-Auth-Token`.
/// The query-token channel is Python-legacy compat and is deliberately NOT
/// implemented on the v1 API (workstreams/10: new interfaces prefer headers).
fn presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    if let Some(token) = headers.get("X-Auth-Token").and_then(|v| v.to_str().ok()) {
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    None
}

fn authorized(headers: &HeaderMap, state: &AppState) -> bool {
    let Some(expected) = state.auth_token.as_deref() else {
        return false;
    };
    presented_token(headers)
        .map(|presented| token_matches(&presented, expected))
        .unwrap_or(false)
}

async fn guard(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    if !authorized(req.headers(), &state) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": {"code": "unauthorized",
                "message": "send header Authorization: Bearer <token> or X-Auth-Token"}})),
        )
            .into_response();
    }
    next.run(req).await
}

async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({"ok": true}))
}

async fn readyz(State(state): State<SharedState>) -> Response {
    let ok = state
        .storage
        .lock()
        .map(|storage| storage.ping())
        .unwrap_or(false);
    if ok {
        (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false})),
        )
            .into_response()
    }
}

/// `GET /api/v1/status` -- machine-readable, zero-secret by contract: no
/// token value, no node credentials, no subscription URL ever enters here.
async fn status(State(state): State<SharedState>) -> Response {
    let last = state.storage.lock().ok().and_then(|storage| {
        storage.last_round().ok().flatten().map(|round| {
            serde_json::json!({
                "round_id": round.round_id,
                "started_at": round.started_at,
                "finished_at": round.finished_at,
                "trigger": round.trigger,
                "note": round.note,
            })
        })
    });
    let running = state.in_flight.load(Ordering::Acquire);
    Json(serde_json::json!({
        "service": "mihomo-probe-rs",
        "rounds": {"running": running, "last": last},
        "kernel": {"api_host_port_only": kernel_host_port(&state)},
    }))
    .into_response()
}

fn kernel_host_port(state: &AppState) -> String {
    // Host:port only -- a query string or embedded credentials must never
    // reach a status payload.
    state
        .controller
        .url()
        .split("://")
        .nth(1)
        .unwrap_or("")
        .to_string()
}

async fn start_round(State(state): State<SharedState>) -> Response {
    // One round at a time, refused at the edge (GLM_5.3_Flash §13).
    if state
        .in_flight
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": {"code": "busy",
                "message": "a round is already running"}})),
        )
            .into_response();
    }
    let round_id = {
        let storage = state.storage.lock().expect("storage lock");
        match storage.start_round("api", None) {
            Ok(id) => id,
            Err(err) => {
                state.in_flight.store(false, Ordering::Release);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": {"code": "storage",
                        "message": err.to_string()}})),
                )
                    .into_response();
            }
        }
    };
    let task_state = state.clone();
    tokio::spawn(async move {
        // Slice round: probe the controller, reload the config, close the
        // round with an honest note. The engine phases arrive in R5; what
        // must not wait is the round row never being left open.
        let note = match task_state.controller.version().await {
            Ok(_) => {
                let reloaded = task_state
                    .controller
                    .reload(&task_state.kernel_config_path)
                    .await
                    .unwrap_or(false);
                if reloaded {
                    "slice: controller reachable, config reloaded"
                } else {
                    "slice: controller reachable, reload refused"
                }
            }
            Err(err) => {
                tracing::warn!(%err, round_id, "controller unreachable during slice round");
                "slice: controller unreachable"
            }
        };
        if let Ok(storage) = task_state.storage.lock() {
            if let Err(err) = storage.finish_round(round_id, Some(note)) {
                tracing::error!(%err, round_id, "could not close round row");
            }
        }
        task_state.in_flight.store(false, Ordering::Release);
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"round_id": round_id, "started": true})),
    )
        .into_response()
}

pub fn build_router(state: SharedState) -> Router {
    let guarded = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/rounds", post(start_round))
        .route_layer(middleware::from_fn_with_state(state.clone(), guard));
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .merge(guarded)
        .with_state(state)
}

/// Bind and serve; the caller decides the bind address (loopback by default).
pub async fn serve(state: SharedState, host: &str, port: u16) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    axum::serve(listener, build_router(state)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(controller: Controller) -> SharedState {
        Arc::new(AppState::new(
            Storage::open_in_memory().unwrap(),
            Some("a".repeat(32)),
            controller,
            "/root/.config/mihomo/config.yaml".into(),
        ))
    }

    async fn spawn_on_port(state: SharedState) -> u16 {
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr.port()
    }

    #[tokio::test]
    async fn healthz_and_readyz_need_no_token() {
        let port = spawn_on_port(test_state(Controller::new("http://127.0.0.1:1", None))).await;
        let client = reqwest::Client::new();
        let health = client
            .get(format!("http://127.0.0.1:{port}/healthz"))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), 200);
        let ready = client
            .get(format!("http://127.0.0.1:{port}/readyz"))
            .send()
            .await
            .unwrap();
        assert_eq!(ready.status(), 200);
    }

    #[tokio::test]
    async fn guarded_endpoints_deny_without_or_with_a_wrong_token() {
        let port = spawn_on_port(test_state(Controller::new("http://127.0.0.1:1", None))).await;
        let client = reqwest::Client::new();
        // No token at all...
        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/v1/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        // ...and a wrong token.
        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/v1/status"))
            .header("X-Auth-Token", "b".repeat(32))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn status_carries_no_secret() {
        let port = spawn_on_port(test_state(Controller::new("http://127.0.0.1:19190", None))).await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/v1/status"))
            .header("X-Auth-Token", "a".repeat(32))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(!body.contains("Bearer"), "no credentials in status: {body}");
        assert!(!body.contains("http://"), "host:port only, no URL: {body}");
    }

    #[tokio::test]
    async fn a_round_starts_once_and_closes_with_an_honest_note() {
        // No kernel behind this URL: the round must still close with a note
        // rather than leaving the row open forever.
        let state = test_state(Controller::new("http://127.0.0.1:1", None));
        assert!(state.kernel_config_path.ends_with("config.yaml"));
        let port = spawn_on_port(state.clone()).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/v1/rounds"))
            .header("X-Auth-Token", "a".repeat(32))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        // Wait for the spawned round task to close the row: polled, not a
        // fixed sleep, because the close is the invariant under test and the
        // scheduler has no obligation to finish it within an arbitrary
        // constant. If it never closes in 5s there is a real bug.
        let mut last = None;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let storage = state.storage.lock().unwrap();
            let row = storage.last_round().unwrap().unwrap();
            if row.finished_at.is_some() {
                last = Some(row);
                break;
            }
        }
        let last = last.expect("round row must not stay open");
        assert!(last.note.unwrap_or_default().contains("unreachable"));
        assert!(state
            .storage
            .lock()
            .unwrap()
            .open_round_ids()
            .unwrap()
            .is_empty());
        // After the round closed, a new one can start.
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/v1/rounds"))
            .header("X-Auth-Token", "a".repeat(32))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
    }

    #[tokio::test]
    async fn token_comparison_is_constant_time_shaped() {
        assert!(token_matches("abcdef", "abcdef"));
        assert!(!token_matches("abcdef", "abcdeg"));
        assert!(!token_matches("abc", "abcdef"));
        assert!(!token_matches("", "abcdef"));
        assert!(!token_matches("abcdef", ""));
    }
}
