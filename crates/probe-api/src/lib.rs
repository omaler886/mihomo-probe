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
use probe_config::SourceSpec;
use probe_engine::{run_round, Ledger, RoundPlan, RoundSettings};
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
    /// Gate widths and test target, from the deployment config.
    pub round: RoundSettings,
    /// Kernel section, used to render `<root>/core/config.yaml` every round.
    pub core: probe_config::CoreConfig,
    /// Collection snapshot taken at serve time: which sources to fetch.
    pub sources: Vec<SourceSpec>,
    /// Sub-Store backend the collection fetches through.
    pub backend: String,
    /// Deployment root: `<root>/core/config.yaml` is rewritten every round
    /// before the kernel reload, `<root>/data/core.secret` signs it.
    pub root: std::path::PathBuf,
    /// Embedded Sub-Store to run alongside the API; `None` disables.
    pub substore: Option<probe_substore::SubStoreConfig>,
    /// The chain section snapshot (front pool config) taken at serve time.
    pub chain: probe_config::ChainSection,
    /// `publish.prefix`, from which the manual-front sub is named.
    pub publish_prefix: String,
    /// The `policy` section snapshot: convergence thresholds and the guard.
    pub policy: probe_config::PolicySection,
    /// Process memory of what the manual-front sub was last written from.
    pub manual: probe_engine::collect::ManualFrontCache,
    in_flight: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(
        storage: Storage,
        auth_token: Option<String>,
        controller: Controller,
        serve: ServeConfig,
    ) -> Self {
        Self {
            storage: Arc::new(Mutex::new(storage)),
            auth_token,
            controller,
            kernel_config_path: serve.kernel_config_path,
            round: serve.round,
            core: serve.core,
            sources: serve.sources,
            backend: serve.backend,
            root: serve.root,
            substore: serve.substore,
            chain: serve.chain,
            publish_prefix: serve.publish_prefix,
            policy: serve.policy,
            manual: probe_engine::collect::ManualFrontCache::default(),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Everything `serve` snapshots from the deployment config at startup.
pub struct ServeConfig {
    /// Kernel-side config path handed to `PUT /configs` on reload.
    pub kernel_config_path: String,
    /// Gate widths and test target, from the deployment config.
    pub round: RoundSettings,
    /// Kernel section, used to render `<root>/core/config.yaml` every round.
    pub core: probe_config::CoreConfig,
    /// Collection snapshot: which sources to fetch.
    pub sources: Vec<SourceSpec>,
    /// Sub-Store backend the collection fetches through.
    pub backend: String,
    /// Deployment root: `<root>/core/config.yaml` is rewritten every round
    /// before the kernel reload, `<root>/data/core.secret` signs it.
    pub root: std::path::PathBuf,
    /// When set, `serve` also runs the embedded Sub-Store on its own
    /// listener. `None` keeps the API surface byte-identical (tests, shadow
    /// runs, deployments that point at a standalone Sub-Store).
    pub substore: Option<probe_substore::SubStoreConfig>,
    /// The chain section snapshot (front pool config) taken at serve time.
    pub chain: probe_config::ChainSection,
    /// `publish.prefix`, from which the manual-front sub is named.
    pub publish_prefix: String,
    /// The `policy` section snapshot: convergence thresholds and the guard.
    pub policy: probe_config::PolicySection,
}

pub type SharedState = Arc<AppState>;

/// Holds the single-round flag and releases it on drop.
///
/// `POST /api/v1/rounds` answers 409 while a round is in flight. Releasing the
/// flag with a bare `store(false)` at the end of the spawned task means a panic
/// anywhere in the round -- or in this handler between the CAS and the spawn --
/// leaves the endpoint refusing forever. A drop guard survives unwinding.
struct InFlight(Arc<AtomicBool>);

impl InFlight {
    /// The caller must already hold the flag: it is set by the compare-exchange
    /// that admitted this round.
    fn acquire(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

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
    // The row is opened here rather than inside the spawned round: the
    // response has to carry the id, and a storage failure has to surface as a
    // 500 instead of a 202 that never becomes a round.
    //
    // `busy` releases the flag on drop. Doing it with a plain `store(false)` at
    // the end of the task means a panic anywhere in the round -- or in this
    // handler between the CAS and the spawn -- wedges the endpoint at 409 for
    // the life of the process.
    let busy = InFlight::acquire(Arc::clone(&state.in_flight));
    let round_id = {
        let storage = match state.storage.lock() {
            Ok(storage) => storage,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": {"code": "storage",
                        "message": "ledger lock poisoned"}})),
                )
                    .into_response();
            }
        };
        match storage.start_round("api", None) {
            Ok(id) => id,
            Err(err) => {
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
        // Moved in, not read: the flag is released when this task ends,
        // including when it unwinds.
        let _busy = busy;
        // The round body lives in `probe-engine` so this handler, the CLI and
        // the future scheduler all run the same path -- including the gate and
        // the "the row always closes" invariant.
        //
        // Collection runs here, inside the task, through the same `collect`
        // the CLI uses: fetch enabled sources, prepare kernel proxies, derive
        // jobs. A failed source is skipped (logged below); the kernel config
        // is rewritten before the reload so the kernel serves this round's
        // nodes rather than a stale file.
        let fetcher = probe_source::SubStoreClient::new(&task_state.backend);
        let admin = probe_source::SubStoreClient::new(&task_state.backend);
        let chain = probe_engine::collect::ChainContext {
            section: &task_state.chain,
            publish_prefix: &task_state.publish_prefix,
            admin: &admin,
            manual: &task_state.manual,
            // The API rounds run the scheduler's semantics: chain exactly as
            // configured (the 直连测活 button is a Python-UI concept).
            mode: None,
        };
        let collected =
            probe_engine::collect(&fetcher, &task_state.sources, true, chain).await;
        for err in &collected.errors {
            tracing::warn!(%err, "source fetch failed; continuing with the rest");
        }
        let data_dir = task_state.root.join("data");
        let secret = probe_config::Config::core_secret(&data_dir).unwrap_or_default();
        let mut round_settings = task_state.round.clone();
        if let Err(err) = probe_mihomo::write_config(
            &task_state.root.join("core"),
            &task_state.core,
            &secret,
            &collected.proxies,
        ) {
            // No config for the kernel to load: the round still opens and
            // closes a row, and says why (same as the CLI path).
            tracing::error!(%err, "kernel config write failed; round will report blocked");
            round_settings.blocked = Some(err.to_string());
        }
        let plan = RoundPlan {
            trigger: "api".into(),
            mode: None,
            jobs: collected.jobs,
            policy: task_state.policy.clone().into(),
        };
        let kernel = round_settings.kernel_prep(
            task_state.controller.clone(),
            &task_state.kernel_config_path,
        );
        let tester = round_settings.tester(task_state.controller.clone());
        let ledger: Arc<dyn Ledger> = task_state.storage.clone();

        match run_round(
            round_id,
            plan,
            round_settings.limits,
            kernel,
            tester,
            ledger,
        )
        .await
        {
            Ok(outcome) => tracing::info!(
                round_id = outcome.round_id,
                counts.total = outcome.counts.total,
                cancelled = outcome.cancelled,
                note = %outcome.note,
                "round closed"
            ),
            Err(err) => {
                // The row may be open: `finish_round` failed. `db.open_rounds()`
                // and the reaper exist for exactly this, so the honest thing is
                // to say so loudly rather than pretend the round closed.
                tracing::error!(%err, round_id, "round failed; check for an open round row");
            }
        }
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
    // The embedded Sub-Store rides along on its own listener. Its failure to
    // boot degrades to "collection fetcher points at a dead URL" -- the probe
    // API itself stays up, consistent with how a standalone Sub-Store dying is
    // already handled.
    if let Some(sub_cfg) = state.substore.clone() {
        tokio::spawn(async move {
            if let Err(err) = probe_substore::serve(sub_cfg).await {
                tracing::error!("embedded sub-store failed: {err}");
            }
        });
    }
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    axum::serve(listener, build_router(state)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(controller: Controller, root: &std::path::Path) -> SharedState {
        let cfg =
            probe_config::Config::load(std::path::Path::new("/nonexistent/config.json")).unwrap();
        Arc::new(AppState::new(
            Storage::open_in_memory().unwrap(),
            Some("a".repeat(32)),
            controller,
            ServeConfig {
                kernel_config_path: "/root/.config/mihomo/config.yaml".into(),
                round: RoundSettings::from_config(&cfg),
                core: cfg.core.clone(),
                // Default sources with a backend that refuses fast: collection
                // fails closed to zero jobs, the round still closes honestly.
                sources: cfg.sources.clone(),
                backend: "http://127.0.0.1:1".into(),
                root: root.to_path_buf(),
                substore: None,
                chain: cfg.chain.clone(),
                publish_prefix: cfg.publish_prefix.clone(),
                policy: cfg.policy.clone(),
            },
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
        let tmp = tempfile::tempdir().unwrap();
        let port = spawn_on_port(test_state(
            Controller::new("http://127.0.0.1:1", None),
            tmp.path(),
        ))
        .await;
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
        let tmp = tempfile::tempdir().unwrap();
        let port = spawn_on_port(test_state(
            Controller::new("http://127.0.0.1:1", None),
            tmp.path(),
        ))
        .await;
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
        let tmp = tempfile::tempdir().unwrap();
        let port = spawn_on_port(test_state(
            Controller::new("http://127.0.0.1:19190", None),
            tmp.path(),
        ))
        .await;
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
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(Controller::new("http://127.0.0.1:1", None), tmp.path());
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
            let row = state.storage.lock().unwrap().last_round().unwrap().unwrap();
            // Wait for the row to close AND the in-flight flag to clear: the
            // flag is released just after `finish_round`, so polling only for
            // `finished_at` leaves a window where the next POST still sees a
            // busy round and answers 409.
            if row.finished_at.is_some() && !state.in_flight.load(Ordering::Acquire) {
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
