//! HTTP API surface for the Rust slice (axum), contract-first per
//! workstreams/10: `/healthz`, `/readyz`, `/api/v1/status`, `POST
//! /api/v1/rounds`, plus the R8 publication read surface
//! (`GET /api/export/<key>.yaml` and the admin `/api/v1/exports` routes).
//!
//! Security rules carried over from the Python service and required by the
//! master document §13: an absent/empty admin token denies (never "auth
//! off"), status payloads carry zero secrets, duplicate rounds are refused at
//! the edge, and the server binds loopback unless explicitly told otherwise.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Html;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use probe_config::SourceSpec;
use probe_engine::{run_round_with_ctx, Ledger, RoundCtx, RoundDeadline, RoundPlan, RoundSettings};
use probe_mihomo::Controller;
use probe_storage::Storage;

mod config_check;
mod exports;
mod metrics;
mod read;

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
    /// The `publish` section snapshot (R8). Its `token` is the read-only
    /// export credential; it is deliberately separate from `auth_token`.
    /// The default carries no token, so an unwired deployment denies every
    /// download instead of silently disabling authentication.
    pub publish: probe_config::PublishSection,
    /// The `policy` section snapshot: convergence thresholds and the guard.
    pub policy: probe_config::PolicySection,
    /// The `schedule` section snapshot: the in-process round timer.
    pub schedule: probe_config::ScheduleSection,
    /// The timer's last computed deadline, as `HH:MM:SS` UTC; `None` = off.
    schedule_next_run: Arc<Mutex<Option<String>>>,
    /// Process memory of what the manual-front sub was last written from.
    pub manual: probe_engine::collect::ManualFrontCache,
    /// G03 kernel validator used by the preflight step; production uses
    /// [`probe_engine::MihomoValidator`], tests inject a scripted one.
    pub validator: Arc<dyn probe_engine::KernelValidator + Send + Sync>,
    in_flight: Arc<AtomicBool>,
    /// The round currently holding the in-flight slot: its id and the shared
    /// context whose cancellation token ends it. Registered before the task
    /// starts so `POST /api/v1/rounds/{id}/cancel` can reach it immediately.
    running: Arc<Mutex<Option<RunningRound>>>,
    /// The config file this process was started from, when the caller
    /// provided one. Empty means the self-check endpoint cannot read a file.
    config_path: std::path::PathBuf,
}

impl AppState {
    pub fn new(
        storage: Storage,
        auth_token: Option<String>,
        controller: Controller,
        serve: ServeConfig,
    ) -> Self {
        // The config file is conventionally `<root>/data/config.json` -- the
        // same convention `db_path` follows -- so the self-check needs no
        // extra wiring at each call site.
        let config_path = serve.root.join("data").join("config.json");
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
            publish: serve.publish,
            policy: serve.policy,
            schedule: serve.schedule,
            schedule_next_run: Arc::new(Mutex::new(None)),
            manual: probe_engine::collect::ManualFrontCache::default(),
            validator: Arc::new(probe_engine::MihomoValidator),
            in_flight: Arc::new(AtomicBool::new(false)),
            running: Arc::new(Mutex::new(None)),
            config_path,
        }
    }

    /// Record the config file this process was started from, so the config
    /// self-check can parse exactly what an operator is about to rely on.
    pub fn with_config_path(mut self, path: std::path::PathBuf) -> Self {
        self.config_path = path;
        self
    }

    /// Attach the deployment's `publish` section snapshot.
    ///
    /// Kept out of `new` so a caller that has not wired publication yet gets
    /// the deny-by-default value instead of accidentally serving exports
    /// unauthenticated (an absent token must never mean "auth off").
    pub fn with_publish(mut self, publish: probe_config::PublishSection) -> Self {
        self.publish = publish;
        self
    }

    /// Replace the preflight validator. Tests use a scripted one so a kernel
    /// rejection can be observed without a kernel binary.
    pub fn with_validator(
        mut self,
        validator: Arc<dyn probe_engine::KernelValidator + Send + Sync>,
    ) -> Self {
        self.validator = validator;
        self
    }

    /// Register the round that just opened its row. The returned guard clears
    /// the slot when the round task ends, however it ends.
    pub(crate) fn register_running(
        &self,
        round_id: probe_domain::RoundId,
        ctx: RoundCtx,
    ) -> RunningRoundGuard {
        if let Ok(mut slot) = self.running.lock() {
            *slot = Some(RunningRound { round_id, ctx });
        }
        RunningRoundGuard {
            slot: Arc::clone(&self.running),
            round_id,
        }
    }

    /// The id of the round currently holding the in-flight slot, if any.
    pub(crate) fn running_round_id(&self) -> Option<probe_domain::RoundId> {
        self.running
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|running| running.round_id))
    }

    /// Record the timer's next deadline; `None` means the timer is off.
    pub(crate) fn set_next_run(&self, next_run: Option<String>) {
        if let Ok(mut slot) = self.schedule_next_run.lock() {
            *slot = next_run;
        }
    }

    /// The timer's last computed deadline, `HH:MM:SS` UTC.
    pub(crate) fn next_run(&self) -> Option<String> {
        self.schedule_next_run
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
    }

    /// Cancel `round_id` only when it is the round currently in flight.
    pub(crate) fn cancel_round(&self, round_id: probe_domain::RoundId) -> CancelOutcome {
        let slot = match self.running.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        match slot.as_ref() {
            None => CancelOutcome::NotRunning,
            Some(running) if running.round_id != round_id => CancelOutcome::Mismatch,
            Some(running) => {
                running.ctx.cancel();
                CancelOutcome::Cancelled
            }
        }
    }
}

/// The round currently holding the in-flight slot.
struct RunningRound {
    round_id: probe_domain::RoundId,
    ctx: RoundCtx,
}

/// Clears the in-flight registration when the round task ends.
pub(crate) struct RunningRoundGuard {
    slot: Arc<Mutex<Option<RunningRound>>>,
    round_id: probe_domain::RoundId,
}

impl Drop for RunningRoundGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.slot.lock() {
            if slot
                .as_ref()
                .is_some_and(|running| running.round_id == self.round_id)
            {
                *slot = None;
            }
        }
    }
}

/// What `POST /api/v1/rounds/{id}/cancel` found when it looked at the slot.
pub(crate) enum CancelOutcome {
    /// The in-flight round matched and its cancellation token fired.
    Cancelled,
    /// Nothing is in flight; the caller's id was not compared at all.
    NotRunning,
    /// A different round is in flight.
    Mismatch,
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
    /// The `publish` section (R8): the round's export publication and the
    /// read-only download credential.
    pub publish: probe_config::PublishSection,
    /// The `schedule` section: the in-process round timer.
    pub schedule: probe_config::ScheduleSection,
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

/// Apply private-response headers to all routes, including authentication failures.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    for (name, value) in [
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
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
    // The full round DTO, so the panel's cards (ok/failed/total) get real
    // numbers; field names match `GET /api/v1/rounds` on purpose.
    let last = state
        .storage
        .lock()
        .ok()
        .and_then(|storage| storage.list_rounds(1, None).ok())
        .and_then(|mut rows| (!rows.is_empty()).then(|| rows.remove(0)))
        .map(|row| read::round_dto(&row));
    let running = state.in_flight.load(Ordering::Acquire);
    // Per-source counts and the export ledger size: three indexed reads that
    // let the panel show which upstream is carrying the round, and how much
    // publication history retention is keeping.
    let (sources, active_exports, snapshots) = match state.storage.lock() {
        Ok(storage) => (
            storage.source_summary().unwrap_or_default(),
            storage.active_export_count().unwrap_or(0),
            storage.snapshot_count().unwrap_or(0),
        ),
        Err(_) => (Vec::new(), 0, 0),
    };
    let sources: Vec<serde_json::Value> = sources
        .iter()
        .map(|row| {
            serde_json::json!({
                "source": row.source,
                "total": row.total,
                "alive": row.alive,
                "dead": row.dead,
            })
        })
        .collect();
    Json(serde_json::json!({
        "service": "mihomo-probe-rs",
        "rounds": {
            "running": running,
            "running_round_id": state.running_round_id(),
            "last": last,
        },
        "kernel": {"api_host_port_only": kernel_host_port(&state)},
        "schedule": {
            "enabled": state.schedule.enabled,
            "interval_minutes": state.schedule.interval().as_secs() / 60,
            "next_run": state.next_run(),
        },
        "sources": sources,
        "exports": {"active": active_exports, "snapshots": snapshots},
    }))
    .into_response()
}

/// Return a credential-free controller address for the status DTO.
fn kernel_host_port(state: &AppState) -> String {
    state.controller.host_port().unwrap_or_default()
}

/// Run G03 preflight: let the kernel validate the config it is about to load,
/// pruning a node it refuses and blocking the round if it still refuses.
///
/// `make_testable` owns the config write, so this replaces the old write-config
/// step and the caller must not write it again. `None` means the accepted
/// config is on disk and the round may run; `Some(reason)` blocks the round.
async fn preflight_round(
    state: &AppState,
    collected: &mut probe_engine::Collected,
    round_id: probe_domain::RoundId,
    ctx: &RoundCtx,
) -> Option<String> {
    let secret = match probe_config::Config::core_secret(&state.root.join("data")) {
        Ok(secret) => secret,
        Err(err) => {
            // Without a secret the config would be unauthenticated; never write
            // one. The round still opens and closes a row, and says why.
            tracing::error!(round_id, %err, "cannot read the core secret; round blocked");
            return Some(err.to_string());
        }
    };
    if ctx.is_cancelled() {
        // Already over: spending up to six `mihomo -t` attempts on a round
        // whose ctx is cancelled would only delay the close. `run_round`
        // sees the same token right after this and aborts cleanly.
        return None;
    }
    // `make_testable` is synchronous by design -- one `mihomo -t` subprocess
    // per attempt, up to MAX_PRUNE+1 of them, each with its own 120s timeout
    // -- so it runs on a blocking thread. Inline on the async worker, every
    // attempt blocked the whole runtime: every endpoint, including this
    // round's own cancel route and the deadline timer that arms the
    // watchdog, froze behind a wedged validator.
    let core_dir = state.root.join("core");
    let core = state.core.clone();
    let validator = state.validator.clone();
    let proxies = std::mem::take(&mut collected.proxies);
    let joined = tokio::task::spawn_blocking(move || {
        let mut preflight_log = Vec::new();
        let (proxies, verdict) = probe_engine::make_testable(
            &core_dir,
            &core,
            &secret,
            proxies,
            &*validator,
            &mut preflight_log,
        );
        (proxies, verdict, preflight_log)
    })
    .await;
    let (proxies, verdict, preflight_log) = match joined {
        Ok(triple) => triple,
        Err(join) => {
            tracing::error!(round_id, %join, "preflight task panicked; round blocked");
            return Some(format!("preflight task panicked: {join}"));
        }
    };
    for line in &preflight_log {
        tracing::info!(round_id, line = %line, "preflight");
    }
    match verdict {
        probe_engine::PreflightVerdict::Accepted => {}
        probe_engine::PreflightVerdict::Skipped { reason } => {
            tracing::info!(
                round_id,
                reason = %reason,
                "config validation skipped; round runs unchecked"
            );
        }
        probe_engine::PreflightVerdict::Pruned { dropped } => {
            tracing::info!(
                round_id,
                pruned = dropped.len(),
                "pruned nodes the kernel refused"
            );
            // A pruned node has no kernel proxy left, so its jobs go too. The
            // ledger identity is what makes this removal unambiguous across the
            // kernel-facing renames.
            let pruned: std::collections::HashSet<&str> =
                dropped.iter().map(String::as_str).collect();
            let before = collected.jobs.len();
            collected
                .jobs
                .retain(|job| !pruned.contains(job.fingerprint.as_str()));
            if before > collected.jobs.len() {
                tracing::info!(
                    round_id,
                    jobs = before - collected.jobs.len(),
                    "dropped jobs whose node the kernel pruned"
                );
            }
        }
        probe_engine::PreflightVerdict::Rejected { detail } => {
            tracing::warn!(
                round_id,
                detail = %detail,
                "kernel rejects the generated config; round blocked"
            );
            collected.proxies = proxies;
            return Some(format!("kernel rejected the generated config: {detail}"));
        }
    }
    collected.proxies = proxies;
    None
}

async fn start_round(State(state): State<SharedState>) -> Response {
    match spawn_round(&state, "api", None) {
        RoundStart::Started(round_id) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"round_id": round_id, "started": true})),
        )
            .into_response(),
        RoundStart::Busy => {
            exports::error_response(StatusCode::CONFLICT, "busy", "a round is already running")
        }
        RoundStart::Storage(err) => exports::error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage",
            err.to_string(),
        ),
    }
}

/// What claiming the single in-flight slot produced.
enum RoundStart {
    Started(probe_domain::RoundId),
    Busy,
    Storage(probe_domain::DomainError),
}

/// Extra age a round lock may carry past the watchdog budget before the
/// scheduler will steal it: a deadline-cancelled round keeps the lock while
/// it drains in-flight probes (each bounded by its own timeout) and closes
/// its row. Two minutes covers that drain with room to spare and changes
/// nothing for genuinely dead holders -- they are still stolen one budget
/// plus two minutes later.
const ROUND_LOCK_DRAIN_SLACK: std::time::Duration = std::time::Duration::from_secs(120);

/// Claim the in-flight slot, open the round row, and spawn the round task.
///
/// The one start path: `POST /api/v1/rounds` calls it with "api", the
/// scheduler with "schedule". Everything through the spawn is synchronous, so
/// two ticks -- or a tick and a POST -- can never race past the busy check.
fn spawn_round(
    state: &SharedState,
    trigger: &'static str,
    mode: Option<&'static str>,
) -> RoundStart {
    // One round at a time, refused at the edge (GLM_5.3_Flash §13).
    if state
        .in_flight
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return RoundStart::Busy;
    }
    // The guard releases the flag on every return path below and at the end of
    // the round task, including unwinding: a storage failure must not wedge the
    // endpoint at 409 for the life of the process.
    let busy = InFlight::acquire(Arc::clone(&state.in_flight));
    // The in-process flag cannot see a `probe-cli round` started by hand, and
    // both would rewrite the same kernel config. The lock file can.
    // The staleness window is the watchdog budget PLUS drain slack: a round
    // the deadline cancelled at exactly the budget is still closing -- its
    // probes unwind on their own timeouts and the close is unconditional --
    // and stealing the lock from a round that is still draining would let two
    // processes rewrite the same kernel config and reload the same kernel.
    let stale_after = std::time::Duration::from_secs(
        state
            .round
            .watchdog
            .round_timeout_minutes
            .saturating_mul(60),
    ) + ROUND_LOCK_DRAIN_SLACK;
    let round_lock = match probe_engine::RoundLock::acquire(&state.root, stale_after) {
        Ok(lock) => lock,
        Err(reason) => {
            tracing::warn!(%reason, "round refused: another process holds the round lock");
            return RoundStart::Busy;
        }
    };
    // The row is opened here rather than inside the spawned round: the HTTP
    // caller has to carry the id, and a storage failure has to surface as a
    // 500 instead of a 202 that never becomes a round.
    let round_id = match state.storage.lock() {
        Ok(storage) => match storage.start_round(trigger, mode) {
            Ok(round_id) => round_id,
            Err(err) => return RoundStart::Storage(err),
        },
        Err(_) => {
            return RoundStart::Storage(probe_domain::DomainError::Storage(
                "ledger lock poisoned".into(),
            ))
        }
    };
    // The context is created here, not inside the task: the cancel route needs
    // the same token, and a cancel racing the response must find this round.
    let ctx = RoundCtx::new(state.round.limits);
    let running = state.register_running(round_id, ctx.clone());
    let task_state = state.clone();
    tokio::spawn(round_task(
        task_state,
        round_id,
        trigger,
        mode,
        ctx,
        RoundGuards {
            _busy: busy,
            _running: running,
            _lock: round_lock,
        },
    ));
    RoundStart::Started(round_id)
}

/// Append one operational event, best effort.
///
/// The ledger event log is the operator's history (Python `db.log`); a
/// failure to write one must never fail the round it describes.
fn record_event(state: &crate::AppState, level: &str, message: &str) {
    let storage = state.storage.clone();
    let level = level.to_string();
    let message = message.to_string();
    // Off the async path: rusqlite is blocking and the round task must not
    // wait on a log write.
    std::thread::spawn(move || {
        if let Ok(storage) = storage.lock() {
            if let Err(err) = storage.record_event(&level, &message) {
                tracing::warn!(%err, "event not recorded");
            }
        }
    });
}

/// What a running round owns until it ends: the in-process busy flag, the
/// cancel slot, and the cross-process lock file.
struct RoundGuards {
    _busy: InFlight,
    _running: RunningRoundGuard,
    _lock: probe_engine::RoundLock,
}

/// The round body shared by `POST /api/v1/rounds` and the scheduler.
async fn round_task(
    task_state: SharedState,
    round_id: probe_domain::RoundId,
    trigger: &'static str,
    mode: Option<&'static str>,
    ctx: RoundCtx,
    guards: RoundGuards,
) {
    // Moved in, not read: every guard is released when this task ends,
    // including when it unwinds.
    let _guards = guards;
    let _deadline = RoundDeadline::arm(
        &ctx,
        std::time::Duration::from_secs(
            task_state
                .round
                .watchdog
                .round_timeout_minutes
                .saturating_mul(60),
        ),
    );
    record_event(
        &task_state,
        "info",
        &format!(
            "round {round_id} started (trigger={trigger}{})",
            mode.map(|mode| format!(", mode={mode}"))
                .unwrap_or_default()
        ),
    );
    let mut round_settings = task_state.round.clone();
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
    let services = probe_engine::network::NetworkServices::new(
        task_state.storage.clone(),
        &round_settings.verify,
    );
    let mut collected = match probe_engine::collect::collect_and_resolve(
        &fetcher,
        &task_state.sources,
        &round_settings,
        chain,
        &services,
        &ctx,
    )
    .await
    {
        Ok(collected) => collected,
        Err(err) => {
            tracing::error!(%err, round_id, "round preparation failed");
            round_settings.blocked = Some(err.to_string());
            probe_engine::Collected::default()
        }
    };
    for err in &collected.errors {
        tracing::warn!(%err, "source fetch failed; continuing with the rest");
        record_event(&task_state, "warn", &format!("source fetch failed: {err}"));
    }
    if round_settings.blocked.is_none() {
        // G03: the kernel validates the config it is about to load. `make_testable`
        // owns the write (pruning + retrying); a config it still refuses blocks
        // the round instead of testing against something that never loaded.
        if let Some(reason) = preflight_round(&task_state, &mut collected, round_id, &ctx).await {
            round_settings.blocked = Some(reason);
        }
    }
    // `take` keeps `collected` whole: the publication step below needs its
    // originals, and `RoundPlan` owns the job list.
    let plan = RoundPlan {
        trigger: trigger.into(),
        mode: mode.map(str::to_string),
        jobs: std::mem::take(&mut collected.jobs),
        policy: task_state.policy.clone().into(),
        domain_pass: round_settings.verify.domain_pass,
        preserved_sources: std::mem::take(&mut collected.preserved_sources),
    };
    let kernel = round_settings.kernel_prep(
        task_state.controller.clone(),
        &task_state.kernel_config_path,
    );
    let tester = round_settings.verified_tester(
        task_state.controller.clone(),
        &task_state.core,
        &collected.proxies,
    );
    let ledger: Arc<dyn Ledger> = task_state.storage.clone();

    match run_round_with_ctx(round_id, plan, ctx, kernel, tester, ledger).await {
        Ok(outcome) => {
            tracing::info!(
                round_id = outcome.round_id,
                counts.total = outcome.counts.total,
                cancelled = outcome.cancelled,
                note = %outcome.note,
                "round closed"
            );
            record_event(
                &task_state,
                if outcome.suspect || outcome.cancelled {
                    "warn"
                } else {
                    "info"
                },
                &format!(
                    "round {} closed: total {} ok {} failed {} inconclusive {} dropped {}",
                    outcome.round_id,
                    outcome.counts.total,
                    outcome.counts.ok,
                    outcome.counts.failed,
                    outcome.counts.inconclusive,
                    outcome.counts.dropped,
                ),
            );
            // R8: publish after the row closed, while this task still holds
            // the in-flight flag -- a client that sees the flag clear can
            // trust that this round's exports are durable.
            publish_after_round(&task_state, collected, outcome).await;
        }
        Err(err) => {
            // The row may be open: `finish_round` failed. `db.open_rounds()`
            // and the reaper exist for exactly this, so the honest thing is
            // to say so loudly rather than pretend the round closed.
            tracing::error!(%err, round_id, "round failed; check for an open round row");
            record_event(
                &task_state,
                "error",
                &format!("round {round_id} failed: {err}"),
            );
        }
    }
}

/// Publish this round's per-source exports (R8) before the round task ends.
///
/// The round guard owns the decision: `publish_round` skips everything when the
/// `publish` section is disabled or the round did not apply convergence, and
/// then leaves the previous immutable files and ledger pointers untouched. One
/// source failing never stops the others, so publication failures are logged
/// and never turn a completed round into a failed response.
///
/// Takes the round's data by value: `publish_round` is synchronous and writes
/// files (hash, fsync, a directory scan per source) under the ledger mutex, so
/// it runs on a blocking thread -- running it inline on the async worker
/// froze every endpoint (including this round's own cancel route and deadline
/// timer) behind one round's fsyncs.
async fn publish_after_round(
    state: &AppState,
    collected: probe_engine::Collected,
    outcome: probe_engine::RoundOutcome,
) {
    let export_root = state.publish.export_root(&state.root);
    // `publish_round` never creates the root: an existing exclusive directory
    // is its contract. A fresh deployment has no `exports/` yet, so create it
    // here (owner-only on Unix, which its root check requires) rather than
    // failing every source on the first round.
    if state.publish.enabled {
        if let Err(err) = ensure_export_root(&export_root) {
            tracing::error!(
                round_id = outcome.round_id,
                %err,
                "cannot prepare the export root; publication may fail"
            );
        }
    }
    let round_id = outcome.round_id;
    let guard = outcome.guard;
    let measured = !outcome.cancelled && outcome.results_written > 0;
    let publish = state.publish.clone();
    let sources = state.sources.clone();
    let storage = state.storage.clone();
    let joined = tokio::task::spawn_blocking(move || {
        // Lock discipline inside the blocking task: the mutex is held only for
        // the synchronous `publish_round` call and released when it returns --
        // the same scope the inline version had, minus the frozen worker.
        let storage = match storage.lock() {
            Ok(storage) => storage,
            Err(poisoned) => poisoned.into_inner(),
        };
        probe_engine::publish_round(
            &storage,
            &export_root,
            probe_engine::PublishRoundRequest {
                publish: &publish,
                sources: &sources,
                collected: &collected,
                verdicts: &outcome.verdicts,
                guard,
                // A cancelled round, or one that committed nothing, has no
                // baseline to publish from; partial coverage still publishes
                // (Python parity).
                measured,
                actor: "api-round",
            },
        )
    })
    .await;
    let published = match joined {
        Ok(published) => published,
        Err(join) => {
            tracing::error!(
                round_id,
                %join,
                "publication task panicked; previous exports stay active"
            );
            return;
        }
    };
    if let Some(reason) = published.skipped {
        // Disabled section, or a round that did not apply convergence: the
        // previous exports stay active by design.
        tracing::info!(
            round_id = round_id,
            ?reason,
            "publication skipped for the whole round"
        );
    }
    for source in &published.sources {
        match source {
            probe_engine::SourcePublication::Published {
                key, node_count, ..
            } => {
                tracing::info!(
                    round_id = round_id,
                    %key,
                    node_count,
                    "export published"
                );
            }
            probe_engine::SourcePublication::Skipped { key, reason } => {
                tracing::info!(round_id = round_id, %key, ?reason, "export skipped");
            }
            probe_engine::SourcePublication::Failed { key, reason } => {
                tracing::warn!(
                    round_id = round_id,
                    %key,
                    %reason,
                    "export publication failed"
                );
            }
        }
    }
    if !published.dropped.is_empty() {
        tracing::info!(
            round_id = round_id,
            dropped = published.dropped.len(),
            "nodes left out of their export body"
        );
    }
    if let Some(pruned) = &published.pruned {
        if pruned.snapshots_deleted > 0 || pruned.files_deleted > 0 {
            tracing::info!(
                round_id = round_id,
                snapshots = pruned.snapshots_deleted,
                files = pruned.files_deleted,
                keys = pruned.keys,
                "export retention pruned superseded snapshots"
            );
        }
    }
    if !state.publish.push_to_substore {
        return;
    }
    // G13 push mode: mirror the just-published exports into Sub-Store as
    // `<prefix>-<key>-local`. `push_round_exports` borrows `&Storage` across
    // awaits, and rusqlite's Connection is !Sync, so that future is !Send -- it
    // cannot be awaited from a `tokio::spawn` round task. Drive it on a blocking
    // thread with its own current-thread runtime instead.
    let backend = state.backend.clone();
    let publish = state.publish.clone();
    let sources = state.sources.clone();
    let storage = state.storage.clone();
    let joined = tokio::task::spawn_blocking(move || -> Vec<probe_engine::PushRecord> {
        let admin = probe_source::SubStoreClient::new(&backend);
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!(%err, "cannot start the Sub-Store push runtime");
                return Vec::new();
            }
        };
        let storage = match storage.lock() {
            Ok(storage) => storage,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut published = published;
        runtime.block_on(probe_engine::push_round_exports(
            &admin,
            &storage,
            &publish,
            &sources,
            &mut published,
        ));
        published.push
    })
    .await;
    match joined {
        Ok(records) => {
            for record in records {
                if record.ok {
                    tracing::info!(
                        round_id = round_id,
                        key = %record.key,
                        name = %record.name,
                        node_count = record.node_count,
                        "export pushed into Sub-Store"
                    );
                } else {
                    tracing::warn!(
                        round_id = round_id,
                        key = %record.key,
                        name = %record.name,
                        detail = %record.detail,
                        "Sub-Store push failed"
                    );
                }
            }
        }
        Err(err) => tracing::warn!(%err, "Sub-Store push task failed"),
    }
}

/// Create the export root when a round has to publish into it.
///
/// `probe_engine::publish` requires the root to exist, be a real directory and,
/// on Unix, be owner-only (`0700`); it deliberately does not create it itself.
pub(crate) fn ensure_export_root(root: &std::path::Path) -> std::io::Result<()> {
    let existed = root.exists();
    std::fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !existed {
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    let _ = existed;
    Ok(())
}

/// The in-process round timer's poll interval (Python `scheduler_loop`: 20s).
pub const SCHEDULER_TICK: std::time::Duration = std::time::Duration::from_secs(20);

/// Start the in-process round timer.
///
/// It replaces the old systemd timer: every tick reaps rows a dead process left
/// open, then starts a round when `schedule` says one is due. Manual and API
/// rounds keep working while the timer is disabled.
pub fn spawn_scheduler(state: SharedState) {
    spawn_scheduler_every(state, SCHEDULER_TICK);
}

/// The same loop with an injectable tick, so tests do not wait 20 seconds.
pub fn spawn_scheduler_every(state: SharedState, tick: std::time::Duration) {
    tokio::spawn(async move {
        let process_started = probe_storage::utc_now();
        loop {
            scheduler_tick(&state, &process_started).await;
            tokio::time::sleep(tick).await;
        }
    });
}

/// One scheduler pass: reap orphans, decide, publish `next_run`, start if due.
///
/// `process_started` is the stored-format UTC text of the serve start: the
/// empty-ledger deadline is anchored there (Python's loop-local `first_due`).
pub(crate) async fn scheduler_tick(state: &SharedState, process_started: &str) {
    let Some(now) = probe_engine::parse_stored_instant(&probe_storage::utc_now()) else {
        return;
    };
    let budget = std::time::Duration::from_secs(
        state
            .round
            .watchdog
            .round_timeout_minutes
            .saturating_mul(60),
    );
    let (busy, last_started) = {
        let storage = match state.storage.lock() {
            Ok(storage) => storage,
            Err(_) => return,
        };
        // Reap first: a row from a previous process must close before this
        // tick's decision reads "the last round". The round this process
        // still has in flight is exempt: a deadline-cancelled round stays
        // open past the budget while it drains, and it closes its own row.
        match probe_engine::reap_orphan_rounds(&*storage, now, budget, state.running_round_id()) {
            Ok(closed) => {
                for round_id in closed {
                    tracing::info!(round_id, "reclaimed a round this process did not finish");
                }
            }
            Err(err) => tracing::warn!(%err, "orphan round reaping failed"),
        }
        let last_started = storage
            .last_round()
            .ok()
            .flatten()
            .and_then(|round| probe_engine::parse_stored_instant(&round.started_at));
        (state.in_flight.load(Ordering::Acquire), last_started)
    };
    let process_started = probe_engine::parse_stored_instant(process_started).unwrap_or(now);
    let (decision, next_run) =
        probe_engine::decide(&state.schedule, last_started, process_started, now, busy);
    // `next_run` is UTC (the ledger's clock) so the panel's clock cannot drift
    // with the host's zone between ticks.
    state.set_next_run(next_run.map(|instant| {
        format!(
            "{:02}:{:02}:{:02}",
            instant.hour(),
            instant.minute(),
            instant.second()
        )
    }));
    match decision {
        probe_engine::SchedulerDecision::Start => match spawn_round(state, "schedule", None) {
            RoundStart::Started(round_id) => {
                tracing::info!(round_id, "scheduler started a round")
            }
            RoundStart::Busy => {
                tracing::debug!("scheduler: another round claimed the slot first")
            }
            RoundStart::Storage(err) => {
                tracing::error!(%err, "scheduler could not open a round row")
            }
        },
        probe_engine::SchedulerDecision::Disabled
        | probe_engine::SchedulerDecision::NotDue
        | probe_engine::SchedulerDecision::Busy => {}
    }
}

pub fn build_router(state: SharedState) -> Router {
    let guarded = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/rounds", post(start_round))
        .merge(exports::admin_routes())
        .merge(read::admin_routes())
        .merge(metrics::routes())
        .merge(config_check::routes())
        .route_layer(middleware::from_fn_with_state(state.clone(), guard));
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/", get(panel_index))
        .route("/panel", get(panel_index))
        .merge(exports::public_routes())
        .merge(guarded)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// A minimal read-only dashboard, embedded so a deployment stays a single
/// binary. It is deliberately not the Python panel: that one edits config and
/// therefore needs the full guard surface. This one shows the last round and
/// the ledger's headline numbers, which is what an operator checks first.
async fn panel_index() -> Html<&'static str> {
    Html(PANEL_HTML)
}

const PANEL_HTML: &str = r#"<!doctype html>
<html lang="zh"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>probe</title>
<style>
:root{color-scheme:dark}
body{font:14px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;background:#111;color:#ddd;margin:0;padding:20px}
h1{font-size:15px;font-weight:600;margin:0 0 2px;color:#fff}
.sub{color:#777;margin-bottom:16px}
.row{display:flex;gap:10px;flex-wrap:wrap;align-items:center;margin-bottom:14px}
.card{background:#1a1a1a;border:1px solid #2a2a2a;border-radius:8px;padding:10px 14px;min-width:96px}
.card .k{color:#777;font-size:11px;text-transform:uppercase;letter-spacing:.5px}
.card .v{font-size:20px;font-weight:600;color:#fff;margin-top:2px}
.ok{color:#4ade80}.warn{color:#fbbf24}.bad{color:#f87171}.muted{color:#666}
table{width:100%;border-collapse:collapse;font-size:12px;margin-bottom:6px}
th,td{text-align:left;padding:4px 8px;border-bottom:1px solid #1f1f1f;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:320px}
th{color:#777;font-weight:500}
section{margin-bottom:22px}
h2{font-size:13px;color:#aaa;margin:0 0 8px;font-weight:600}
input,select,button{background:#1a1a1a;border:1px solid #2a2a2a;color:#ddd;padding:5px 9px;border-radius:6px;font:inherit}
button{cursor:pointer}button:hover{background:#262626}
</style></head><body>
<h1>probe</h1>
<div class="sub">mihomo probe &middot; 只读面板（admin token）</div>
<div class="row">
  <input id="tok" type="password" placeholder="admin token" autocomplete="off" style="width:300px">
  <button id="load">加载</button>
  <span id="status-line" class="muted"></span>
</div>
<div class="row" id="cards"></div>
<section>
  <h2>节点 <span class="muted" id="nodes-count"></span></h2>
  <div class="row">
    <select id="f-status">
      <option value="">全部状态</option><option>alive</option><option>dead</option>
      <option>pending</option><option>unknown</option><option>excluded</option>
    </select>
    <input id="f-source" placeholder="来源 source" style="width:150px">
    <button id="f-apply">筛选</button>
    <button id="nodes-more">加载更多</button>
  </div>
  <table><thead><tr>
    <th>来源</th><th>节点</th><th>状态</th><th>国别</th><th>出口</th><th>延迟</th><th>连败</th><th>ok/fail</th><th>最后原因</th>
  </tr></thead><tbody id="nodes-body"></tbody></table>
</section>
<section id="detail-section" hidden>
  <h2>节点详情 <span class="muted" id="detail-title"></span></h2>
  <div class="row" id="detail-quality"></div>
  <table><thead><tr><th>时间</th><th>变化</th><th>原因</th><th>轮次</th></tr></thead>
  <tbody id="detail-history"></tbody></table>
</section>
<section>
  <h2>来源</h2>
  <table><thead><tr><th>来源</th><th>节点</th><th>存活</th><th>失败</th><th>存活率</th></tr></thead>
  <tbody id="sources-body"></tbody></table>
</section>
<section>
  <h2>轮次</h2>
  <table><thead><tr>
    <th>#</th><th>开始 (UTC)</th><th>耗时</th><th>总数</th><th>存活</th><th>失败</th><th>触发</th><th>护栏</th><th>备注</th>
  </tr></thead><tbody id="rounds-body"></tbody></table>
</section>
<section>
  <h2>事件 <span class="muted" id="events-count"></span></h2>
  <table><thead><tr><th>时间 (UTC)</th><th>级别</th><th>内容</th></tr></thead>
  <tbody id="events-body"></tbody></table>
</section>
<section>
  <h2>导出 <span class="muted" id="exports-count"></span></h2>
  <table><thead><tr>
    <th>key</th><th>节点数</th><th>激活 (UTC)</th><th>sha256</th><th></th>
  </tr></thead><tbody id="exports-body"></tbody></table>
</section>
<script>
(function(){
  'use strict';
  var state = { nodesAfter: null, hasMore: false };

  function $(selector){ return document.querySelector(selector); }
  function token(){ return ($('#tok').value || '').trim(); }
  function say(text, cls){
    var line = $('#status-line');
    line.textContent = text || '';
    line.className = cls || 'muted';
  }
  function clear(node){ while(node.firstChild){ node.removeChild(node.firstChild); } }
  function cell(row, text, cls){
    var td = document.createElement('td');
    if(cls){ td.className = cls; }
    td.textContent = (text === null || text === undefined) ? '' : String(text);
    row.appendChild(td);
    return td;
  }
  function row(body){
    var tr = document.createElement('tr');
    body.appendChild(tr);
    return tr;
  }
  function get(path){
    return fetch(path, { headers: { 'X-Auth-Token': token() }, cache: 'no-store' })
      .then(function(response){
        return response.json().catch(function(){ return {}; })
          .then(function(body){ return { status: response.status, body: body }; });
      });
  }
  function card(label, value, cls){
    var box = document.createElement('div'); box.className = 'card';
    var key = document.createElement('div'); key.className = 'k'; key.textContent = label;
    var val = document.createElement('div'); val.className = 'v' + (cls ? ' ' + cls : ''); val.textContent = String(value);
    box.appendChild(key); box.appendChild(val);
    $('#cards').appendChild(box);
  }
  function renderCards(status){
    clear($('#cards'));
    var last = (status.rounds && status.rounds.last) || {};
    var schedule = status.schedule || {};
    var running = !!(status.rounds && status.rounds.running);
    card('状态', running ? '运行中' : '空闲', running ? 'warn' : 'ok');
    card('轮次', last.round_id === undefined ? '-' : last.round_id);
    card('存活', last.ok === undefined ? '-' : last.ok, (last.ok || 0) > 0 ? 'ok' : 'bad');
    card('失败', last.failed === undefined ? '-' : last.failed);
    card('总数', last.total === undefined ? '-' : last.total);
    card('耗时', last.duration_s === null || last.duration_s === undefined ? '-' : last.duration_s + 's');
    card('下次轮次', schedule.next_run || (schedule.enabled ? '待定' : '已关闭'));
    var exports = status.exports || {};
    card('导出 key', exports.active === undefined ? '-' : exports.active);
    card('快照', exports.snapshots === undefined ? '-' : exports.snapshots);
  }
  function renderNodes(nodes, append){
    var body = $('#nodes-body');
    if(!append){ clear(body); }
    nodes.forEach(function(node){
      var tr = row(body);
      cell(tr, node.source);
      cell(tr, node.display);
      cell(tr, node.status, node.status === 'alive' ? 'ok' : (node.status === 'dead' ? 'bad' : 'warn'));
      cell(tr, node.country);
      cell(tr, node.exit_ip ? node.exit_ip + (node.exit_asn ? ' ' + node.exit_asn : '') : '');
      cell(tr, node.last_delay_ms === null || node.last_delay_ms === undefined ? '' : node.last_delay_ms + 'ms');
      cell(tr, node.consec_fail);
      cell(tr, (node.total_ok || 0) + '/' + (node.total_fail || 0));
      cell(tr, node.last_reason);
      tr.style.cursor = 'pointer';
      tr.addEventListener('click', function(){ showDetail(node); });
    });
    $('#nodes-count').textContent = '共 ' + body.children.length + ' 行';
  }
  function loadNodes(reset){
    var params = ['limit=50'];
    var status = $('#f-status').value;
    var source = ($('#f-source').value || '').trim();
    if(status){ params.push('status=' + encodeURIComponent(status)); }
    if(source){ params.push('source=' + encodeURIComponent(source)); }
    if(!reset && state.nodesAfter){ params.push('after=' + encodeURIComponent(state.nodesAfter)); }
    return get('/api/v1/nodes?' + params.join('&')).then(function(result){
      if(result.status !== 200){ throw new Error('nodes HTTP ' + result.status); }
      var page = result.body.nodes || [];
      renderNodes(page, !reset);
      state.nodesAfter = result.body.next_after || null;
      state.hasMore = !!state.nodesAfter;
      $('#nodes-more').disabled = !state.hasMore;
    });
  }
  // 配置自检：JSON 坏掉时 Config::load 会静默退回默认值，只有这里能看出来。
  function renderConfig(check){
    var box = document.createElement('div'); box.className = 'card';
    var key = document.createElement('div'); key.className = 'k'; key.textContent = '配置';
    var val = document.createElement('div');
    var text = '正常', cls = 'ok';
    if(!check.json_valid){ text = 'JSON 损坏'; cls = 'bad'; }
    else if(!check.matches_running){ text = '未重启'; cls = 'warn'; }
    val.className = 'v ' + cls; val.textContent = text;
    box.appendChild(key); box.appendChild(val);
    var differences = check.differences || [];
    if(differences.length){
      box.style.cursor = 'pointer';
      box.title = differences.join('\n');
      box.addEventListener('click', function(){ say(differences.join(' · '), 'warn'); });
    }
    $('#cards').appendChild(box);
  }
  function renderEvents(rows){
    var body = $('#events-body');
    clear(body);
    (rows || []).forEach(function(item){
      var tr = row(body);
      cell(tr, item.ts);
      cell(tr, item.level, item.level === 'warn' ? 'warn' : (item.level === 'error' ? 'bad' : ''));
      cell(tr, item.message);
    });
    $('#events-count').textContent = (rows || []).length ? (rows.length + ' 条') : '';
  }
  function renderSources(rows){
    var body = $('#sources-body');
    clear(body);
    (rows || []).forEach(function(item){
      var tr = row(body);
      cell(tr, item.source);
      cell(tr, item.total);
      cell(tr, item.alive, item.alive > 0 ? 'ok' : '');
      cell(tr, item.dead, item.dead > 0 ? 'bad' : '');
      var rate = item.total > 0 ? Math.round((item.alive / item.total) * 100) : 0;
      cell(tr, rate + '%', rate >= 50 ? 'ok' : 'warn');
    });
  }
  function renderRounds(result){
    var body = $('#rounds-body');
    clear(body);
    (result.rounds || []).forEach(function(round){
      var tr = row(body);
      cell(tr, round.round_id);
      cell(tr, round.started_at);
      cell(tr, round.duration_s === null || round.duration_s === undefined ? '' : round.duration_s + 's');
      cell(tr, round.total);
      cell(tr, round.ok, 'ok');
      cell(tr, round.failed, round.failed > 0 ? 'bad' : '');
      cell(tr, round.trigger);
      var flags = [];
      if(round.suspect){ flags.push('suspect'); }
      if(round.inconclusive){ flags.push('inconclusive'); }
      if((round.dropped || 0) > 0){ flags.push('dropped ' + round.dropped); }
      if((round.restored || 0) > 0){ flags.push('restored ' + round.restored); }
      cell(tr, flags.join(' + '), round.suspect || round.inconclusive ? 'warn' : '');
      cell(tr, round.note);
    });
  }
  function renderExports(result){
    var body = $('#exports-body');
    clear(body);
    var rows = result.exports || [];
    rows.forEach(function(item){
      var tr = row(body);
      cell(tr, item.key);
      cell(tr, item.node_count);
      cell(tr, item.created_at);
      cell(tr, (item.sha256 || '').slice(0, 16));
      var td = document.createElement('td');
      var button = document.createElement('button');
      button.textContent = '下载';
      button.addEventListener('click', function(){ download(item.key); });
      td.appendChild(button);
      tr.appendChild(td);
    });
    $('#exports-count').textContent = rows.length ? rows.length + ' 个 key' : '';
  }
  function download(key){
    fetch('/api/export/' + encodeURIComponent(key) + '.yaml', { headers: { 'X-Auth-Token': token() } })
      .then(function(response){
        if(!response.ok){ throw new Error('HTTP ' + response.status); }
        return response.blob();
      })
      .then(function(blob){
        var url = URL.createObjectURL(blob);
        var link = document.createElement('a');
        link.href = url; link.download = key + '.yaml';
        document.body.appendChild(link); link.click(); link.remove();
        URL.revokeObjectURL(url);
      })
      .catch(function(error){ say('下载失败: ' + error.message, 'bad'); });
  }
  function detailCard(label, value, cls){
    var box = document.createElement('div'); box.className = 'card';
    var key = document.createElement('div'); key.className = 'k'; key.textContent = label;
    var val = document.createElement('div'); val.className = 'v' + (cls ? ' ' + cls : ''); val.textContent = String(value);
    box.appendChild(key); box.appendChild(val);
    $('#detail-quality').appendChild(box);
  }
  function ratio(value){
    return (value === null || value === undefined) ? '-' : Math.round(value * 100) + '%';
  }
  function millis(value){
    return (value === null || value === undefined) ? '-' : Math.round(value) + 'ms';
  }
  function showDetail(node){
    var section = $('#detail-section');
    section.hidden = false;
    $('#detail-title').textContent = node.source + ' / ' + node.fingerprint + (node.display ? ' · ' + node.display : '');
    clear($('#detail-quality'));
    clear($('#detail-history'));
    get('/api/v1/nodes/' + encodeURIComponent(node.fingerprint)).then(function(result){
      if(result.status !== 200){ throw new Error('HTTP ' + result.status); }
      var quality = result.body.quality;
      if(quality){
        detailCard('窗口', quality.window);
        detailCard('成功', (quality.successes || 0) + '/' + (quality.samples || 0));
        detailCard('成功率', ratio(quality.success_rate));
        detailCard('中位延迟', millis(quality.median_ms));
        detailCard('p95', millis(quality.p95_ms));
        detailCard('抖动', millis(quality.jitter_ms));
        detailCard('出口数', quality.exits === null || quality.exits === undefined ? '-' : quality.exits);
        detailCard('出口稳定度', ratio(quality.exit_stability));
      } else {
        var none = document.createElement('div');
        none.className = 'muted';
        none.textContent = '暂无历史样本';
        $('#detail-quality').appendChild(none);
      }
      (result.body.history || []).forEach(function(move){
        var tr = row($('#detail-history'));
        cell(tr, move.ts);
        cell(tr, (move.from_status || '?') + ' -> ' + move.to_status);
        cell(tr, move.reason);
        cell(tr, move.round_id);
      });
    }).catch(function(error){ say('详情加载失败: ' + error.message, 'bad'); });
  }

  function load(){
    if(!token()){ say('先填 admin token', 'bad'); return; }
    localStorage.setItem('probe_token', token());
    say('加载中…', 'muted');
    state.nodesAfter = null;
    Promise.all([get('/api/v1/status'), get('/api/v1/rounds?limit=15'), get('/api/v1/exports?limit=50'), get('/api/v1/config/validate'), get('/api/v1/events?limit=20')])
      .then(function(results){
        if(results[0].status === 401 || results[0].status === 403){
          say('token 不对（' + results[0].status + '）', 'bad');
          return null;
        }
        if(results[0].status !== 200){ throw new Error('status HTTP ' + results[0].status); }
        renderCards(results[0].body);
        renderSources(results[0].body.sources);
        if(results[1].status === 200){ renderRounds(results[1].body); }
        if(results[2].status === 200){ renderExports(results[2].body); }
        if(results[3].status === 200){ renderConfig(results[3].body); }
        if(results[4] && results[4].status === 200){ renderEvents(results[4].body.events); }
        return loadNodes(true);
      })
      .then(function(done){
        if(done !== null){ say('已更新 ' + new Date().toLocaleTimeString(), 'ok'); }
      })
      .catch(function(error){ say('加载失败: ' + error.message, 'bad'); });
  }

  $('#load').addEventListener('click', load);
  $('#f-apply').addEventListener('click', function(){ state.nodesAfter = null; loadNodes(true).catch(function(error){ say('筛选失败: ' + error.message, 'bad'); }); });
  $('#nodes-more').addEventListener('click', function(){ if(state.hasMore){ loadNodes(false).catch(function(error){ say('翻页失败: ' + error.message, 'bad'); }); } });

  var saved = localStorage.getItem('probe_token');
  if(saved){ $('#tok').value = saved; load(); }
})();
</script>
</body></html>"#;

/// Bind and serve; the caller decides the bind address (loopback by default).
pub async fn serve(state: SharedState, host: &str, port: u16) -> std::io::Result<()> {
    // The in-process round timer replaces the old systemd unit: Python's
    // `scheduler_loop` runs inside the service too.
    spawn_scheduler(state.clone());
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
mod tests_pipeline;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> probe_config::Config {
        probe_config::Config::load(std::path::Path::new("/nonexistent/config.json")).unwrap()
    }

    fn test_state(controller: Controller, root: &std::path::Path) -> SharedState {
        test_state_with(controller, root, test_config().sources.clone())
    }

    /// A preflight validator driven by a scripted list of kernel verdicts.
    struct ScriptedValidator {
        verdicts:
            std::sync::Mutex<std::collections::VecDeque<probe_mihomo::config_check::ConfigCheck>>,
    }

    impl ScriptedValidator {
        fn scripted(
            verdicts: Vec<probe_mihomo::config_check::ConfigCheck>,
        ) -> Arc<dyn probe_engine::KernelValidator + Send + Sync> {
            Arc::new(Self {
                verdicts: std::sync::Mutex::new(verdicts.into()),
            })
        }
    }

    impl probe_engine::KernelValidator for ScriptedValidator {
        fn validate(&self, _core_dir: &std::path::Path) -> probe_mihomo::config_check::ConfigCheck {
            self.verdicts.lock().unwrap().pop_front().unwrap_or(
                probe_mihomo::config_check::ConfigCheck::Passed(String::new()),
            )
        }
    }

    /// Build a state whose round path never dials a real port: the caller picks
    /// the sources (an empty list means collection has nothing to fetch).
    fn test_state_with(
        controller: Controller,
        root: &std::path::Path,
        sources: Vec<SourceSpec>,
    ) -> SharedState {
        Arc::new(test_app(controller, root, sources))
    }

    fn test_app(
        controller: Controller,
        root: &std::path::Path,
        sources: Vec<SourceSpec>,
    ) -> AppState {
        let cfg = test_config();
        AppState::new(
            Storage::open_in_memory().unwrap(),
            Some("a".repeat(32)),
            controller,
            ServeConfig {
                kernel_config_path: "/root/.config/mihomo/config.yaml".into(),
                round: RoundSettings::from_config(&cfg),
                core: cfg.core.clone(),
                sources,
                backend: "http://127.0.0.1:1".into(),
                root: root.to_path_buf(),
                substore: None,
                chain: cfg.chain.clone(),
                publish_prefix: cfg.publish_prefix.clone(),
                publish: cfg.publish.clone(),
                schedule: cfg.schedule.clone(),
                policy: cfg.policy.clone(),
            },
        )
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

    /// Display only parsed host/port even when a configured URL contains credentials.
    #[test]
    fn kernel_status_never_echoes_url_credentials_or_paths() {
        let tmp = tempfile::tempdir().unwrap();
        for (url, expected) in [
            (
                "http://operator:dummy-password@127.0.0.1:19190/private?key=dummy#fragment",
                "127.0.0.1:19190",
            ),
            (
                "http://operator:dummy-password@[::1]:19190/private",
                "[::1]:19190",
            ),
            ("http://kernel.example:19190/", "kernel.example:19190"),
            ("not-a-url", ""),
            ("file://host/private", ""),
        ] {
            let state = test_state(Controller::new(url, None), tmp.path());
            assert_eq!(kernel_host_port(&state), expected);
        }
    }

    /// A failed secret read must block the round before any config write: never
    /// replace a working config with one rendered without authentication.
    #[tokio::test]
    async fn a_missing_core_secret_never_writes_an_unauthenticated_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("data/core.secret")).unwrap();
        std::fs::create_dir_all(tmp.path().join("core")).unwrap();
        let path = tmp.path().join("core/config.yaml");
        std::fs::write(&path, "previous configuration").unwrap();
        let state = test_state(Controller::new("http://127.0.0.1:1", None), tmp.path());
        let mut collected = probe_engine::Collected::default();
        let blocked = preflight_round(
            &state,
            &mut collected,
            1,
            &probe_engine::RoundCtx::new(probe_engine::Limits::from_concurrency(1)),
        )
        .await;
        assert!(
            blocked.is_some(),
            "a missing core secret must block the round"
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "previous configuration"
        );
    }

    /// A kernel that blames one proxy: preflight prunes it, the job with the
    /// same ledger identity is dropped, and the round stays runnable.
    #[tokio::test]
    async fn preflight_prunes_a_rejected_identity_and_removes_its_job() {
        use probe_mihomo::config_check::ConfigCheck;
        let tmp = tempfile::tempdir().unwrap();
        let name = "0123456789abcdef";
        let culprit = serde_json::json!({
            "name": name,
            "type": "trojan",
            "server": "origin.example",
            "port": 443,
            "password": "synthetic-private-value",
        });
        // The verdict reports the ledger identity, not the kernel-facing name:
        // the job below has to carry that hash, or the prune cannot match it.
        let identity = probe_source::fingerprint_proxy(&culprit);
        assert_ne!(identity, name, "the identity is a hash, not the name");
        let mut collected = probe_engine::Collected {
            proxies: vec![
                culprit,
                // A second, innocent node: pruning one must not mean "prune all".
                serde_json::json!({
                    "name": "node-ok",
                    "type": "trojan",
                    "server": "healthy.example",
                    "port": 443,
                    "password": "synthetic-private-value",
                }),
            ],
            jobs: vec![probe_engine::Job::new(
                "air",
                identity.clone(),
                name,
                "direct",
                "origin.example",
            )],
            ..Default::default()
        };
        let validator = ScriptedValidator::scripted(vec![
            ConfigCheck::Failed(format!("parse config error: proxy \"{name}\" unknown")),
            ConfigCheck::Passed(String::new()),
        ]);
        let state = Arc::new(
            test_app(
                Controller::new("http://127.0.0.1:1", None),
                tmp.path(),
                Vec::new(),
            )
            .with_validator(validator),
        );

        let blocked = preflight_round(
            &state,
            &mut collected,
            1,
            &probe_engine::RoundCtx::new(probe_engine::Limits::from_concurrency(1)),
        )
        .await;
        assert!(
            blocked.is_none(),
            "a pruned config must still run the round: {blocked:?}"
        );
        assert_eq!(
            collected.proxies.len(),
            1,
            "only the refused identity is gone from the config"
        );
        assert_eq!(
            collected.proxies[0]["name"].as_str(),
            Some("node-ok"),
            "the innocent node stays"
        );
        assert!(
            collected.jobs.is_empty(),
            "the job whose fingerprint is the pruned identity is gone"
        );
    }

    /// A real HTTP round: the sheduled task opens the row, closes it even with
    /// no kernel behind the URL, and a second request can start afterwards.
    #[tokio::test]
    async fn a_round_starts_once_and_closes_with_an_honest_note() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty sources + an unreachable controller: collection has nothing to
        // fetch, so the round exercises the close path without dialling.
        let state = test_state_with(
            Controller::new("http://127.0.0.1:1", None),
            tmp.path(),
            Vec::new(),
        );
        let port = spawn_on_port(state.clone()).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/v1/rounds"))
            .header("X-Auth-Token", "a".repeat(32))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        // Polled, not a fixed sleep: the close is the invariant under test.
        let mut last = None;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let row = state.storage.lock().unwrap().last_round().unwrap().unwrap();
            // Wait for the row to close AND the in-flight flag to clear: the
            // flag is released just after `finish_round`, so polling only for
            // `finished_at` leaves a window where the next POST answers 409.
            if row.finished_at.is_some() && !state.in_flight.load(Ordering::Acquire) {
                last = Some(row);
                break;
            }
        }
        let last = last.expect("round row must not stay open");
        assert!(
            !last.note.unwrap_or_default().trim().is_empty(),
            "a closed round always says what happened"
        );
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

    /// Write `<root>/data/config.json` the way the service would find it.
    fn write_config(root: &std::path::Path, body: &str) {
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data").join("config.json"), body).unwrap();
    }

    /// A round held by another process is refused: two processes must never
    /// rewrite the same kernel config.
    #[tokio::test]
    async fn a_round_held_by_another_process_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_with(
            Controller::new("http://127.0.0.1:1", None),
            tmp.path(),
            Vec::new(),
        );
        // Another process holds the lock; its file is fresh, so it is not
        // stolen and this round must be refused rather than started.
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(
            tmp.path().join("data").join("round.lock"),
            "pid=424242 started=0\n",
        )
        .unwrap();
        let port = spawn_on_port(state).await;
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/v1/rounds"))
            .header("X-Auth-Token", "a".repeat(32))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"].as_str(), Some("busy"));
    }

    /// The self-check parses the file the process was started from, reports the
    /// effective values, and never echoes a secret.
    #[tokio::test]
    async fn config_self_check_reports_effective_values_without_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"publish":{"token":"secret-value","hostname":"probe.example.net"}}"#,
        );
        let port = spawn_on_port(test_state(
            Controller::new("http://127.0.0.1:1", None),
            tmp.path(),
        ))
        .await;
        let response = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/v1/config/validate"))
            .header("X-Auth-Token", "a".repeat(32))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(
            !text.contains("secret-value"),
            "the publish token value must never be echoed: {text}"
        );
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["json_valid"], true);
        assert_eq!(
            body["matches_running"], true,
            "nothing was edited after start"
        );
        assert_eq!(body["summary"]["publish"]["token_configured"], true);
        assert_eq!(
            body["summary"]["publish"]["hostname"].as_str(),
            Some("probe.example.net")
        );
    }

    /// A file edited after start is reported as drift, not as a match.
    #[tokio::test]
    async fn config_self_check_reports_drift_from_the_running_values() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"schedule":{"interval_minutes":45},"publish":{"keep_snapshots":20}}"#,
        );
        let port = spawn_on_port(test_state(
            Controller::new("http://127.0.0.1:1", None),
            tmp.path(),
        ))
        .await;
        let body: serde_json::Value = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/v1/config/validate"))
            .header("X-Auth-Token", "a".repeat(32))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["matches_running"], false);
        let differences = body["differences"].as_array().unwrap();
        assert_eq!(differences.len(), 2, "{differences:?}");
        assert!(differences.iter().any(|line| line
            .as_str()
            .unwrap_or_default()
            .contains("schedule.interval_minutes")));
        assert!(differences
            .iter()
            .any(|line| line.as_str().unwrap_or_default().contains("keep_snapshots")));
    }

    /// A file that is not JSON at all is exactly the case `Config::load` hides:
    /// the service runs the defaults, and only this endpoint says so.
    #[tokio::test]
    async fn config_self_check_names_a_broken_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{\"publish\": {");
        let port = spawn_on_port(test_state(
            Controller::new("http://127.0.0.1:1", None),
            tmp.path(),
        ))
        .await;
        let body: serde_json::Value = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/v1/config/validate"))
            .header("X-Auth-Token", "a".repeat(32))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["json_valid"], false);
        let (defaults, _) = probe_config::Config::parse("{}").unwrap();
        assert_eq!(
            body["summary"]["sources"]["total"].as_u64(),
            Some(defaults.sources.len() as u64),
            "a broken file really does mean the defaults, not an empty config"
        );
    }

    /// Status carries the per-source counts and the export ledger size, so the
    /// panel can show which upstream carries the round without a second call.
    #[tokio::test]
    async fn status_carries_source_and_export_aggregates() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(Controller::new("http://127.0.0.1:1", None), tmp.path());
        {
            let storage = state.storage.lock().unwrap();
            let round = storage.start_round("manual", None).unwrap();
            storage
                .converge_nodes(
                    round,
                    &[probe_storage::ConvergeNode {
                        source: "air",
                        fingerprint: "a1",
                        display: "node-a",
                        category: Some("direct"),
                        ok: true,
                        delay_ms: Some(20),
                        reason: None,
                        is_excluded: false,
                        proto: Some("trojan"),
                        server: Some("origin.example"),
                        ip_alive: Some(1),
                        ip_total: Some(1),
                        country: Some("US"),
                        exit_ip: Some("192.0.2.5"),
                        exit_asn: Some("AS64500"),
                    }],
                    &probe_config::PolicySection::default().into(),
                )
                .unwrap();
        }
        let port = spawn_on_port(state).await;
        let response = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/v1/status"))
            .header("X-Auth-Token", "a".repeat(32))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        let sources = body["sources"].as_array().expect("a sources array");
        assert_eq!(sources.len(), 1, "one row per source that owns a node");
        assert_eq!(sources[0]["source"].as_str(), Some("air"));
        assert_eq!(sources[0]["total"].as_i64(), Some(1));
        assert_eq!(sources[0]["alive"].as_i64(), Some(1));
        assert_eq!(body["exports"]["active"].as_i64(), Some(0));
        assert_eq!(body["exports"]["snapshots"].as_i64(), Some(0));
    }

    /// Browsers and proxies must not cache an authenticated payload or an auth
    /// failure: both carry the same hardened headers.
    #[tokio::test]
    async fn security_headers_cover_success_and_unauthorized_responses() {
        let tmp = tempfile::tempdir().unwrap();
        let port = spawn_on_port(test_state(
            Controller::new("http://127.0.0.1:1", None),
            tmp.path(),
        ))
        .await;
        let client = reqwest::Client::new();
        for token in ["a".repeat(32), "wrong".into()] {
            let response = client
                .get(format!("http://127.0.0.1:{port}/api/v1/status"))
                .header("X-Auth-Token", &token)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), if token == "wrong" { 401 } else { 200 });
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
        }
    }

    #[test]
    fn token_comparison_is_constant_time_shaped() {
        assert!(token_matches("abcdef", "abcdef"));
        assert!(!token_matches("abcdef", "abcdeg"));
        assert!(!token_matches("abc", "abcdef"));
        assert!(!token_matches("", "abcdef"));
        assert!(!token_matches("abcdef", ""));
    }

    /// A config the kernel keeps refusing blocks the round instead of reloading.
    #[tokio::test]
    async fn preflight_blocks_the_round_when_the_kernel_keeps_refusing() {
        use probe_mihomo::config_check::ConfigCheck;
        let tmp = tempfile::tempdir().unwrap();
        let mut collected = probe_engine::Collected {
            proxies: vec![serde_json::json!({
                "name": "node-a",
                "type": "trojan",
                "server": "origin.example",
                "port": 443,
                "password": "synthetic-private-value",
            })],
            ..Default::default()
        };
        // Text that names no proxy: culprit localization gives up at once.
        let validator = ScriptedValidator::scripted(vec![
            ConfigCheck::Failed("parse config error: something else".into()),
            ConfigCheck::Failed("parse config error: something else".into()),
        ]);
        let state = Arc::new(
            test_app(
                Controller::new("http://127.0.0.1:1", None),
                tmp.path(),
                Vec::new(),
            )
            .with_validator(validator),
        );
        let blocked = preflight_round(
            &state,
            &mut collected,
            1,
            &probe_engine::RoundCtx::new(probe_engine::Limits::from_concurrency(1)),
        )
        .await;
        let reason = blocked.expect("a refused config must block the round");
        assert!(
            reason.contains("kernel rejected"),
            "the reason names the kernel, got: {reason}"
        );
    }

    /// The panel is a static asset: it renders every value with `textContent`
    /// (a node name is operator-supplied data, not markup) and it only ever
    /// reads -- the write surface needs the guard, not a dashboard button.
    #[test]
    fn the_panel_is_read_only_and_never_injects_markup() {
        for needle in [
            "id=\"nodes-body\"",
            "id=\"rounds-body\"",
            "id=\"exports-body\"",
            "id=\"f-status\"",
            "id=\"nodes-more\"",
            "id=\"detail-section\"",
            "id=\"detail-quality\"",
            "id=\"sources-body\"",
            "id=\"events-body\"",
            "renderSources",
            "renderEvents",
            "showDetail",
            "exit_ip",
            "/api/v1/nodes",
            "/api/v1/rounds",
            "/api/v1/exports",
            "/api/export/",
            "localStorage",
        ] {
            assert!(PANEL_HTML.contains(needle), "the panel lost {needle}");
        }
        assert!(
            !PANEL_HTML.contains("innerHTML"),
            "panel data must be rendered with textContent, never as markup"
        );
        assert!(
            !PANEL_HTML.contains("method: 'POST'"),
            "the panel stays read-only: no write verb anywhere"
        );
        // The one config endpoint it may touch is the read-only self-check.
        for (index, _) in PANEL_HTML.match_indices("/api/v1/config") {
            let tail = &PANEL_HTML[index..(index + 40).min(PANEL_HTML.len())];
            assert!(
                tail.starts_with("/api/v1/config/validate"),
                "only the read-only self-check may be called, got {tail}"
            );
        }
    }

    /// No validator available (no kernel binary, no docker): skip the check and
    /// run the round rather than failing it.
    #[tokio::test]
    async fn preflight_degrades_to_a_running_round_when_validation_is_unavailable() {
        use probe_mihomo::config_check::ConfigCheck;
        let tmp = tempfile::tempdir().unwrap();
        let proxy = serde_json::json!({
            "name": "node-a",
            "type": "trojan",
            "server": "origin.example",
            "port": 443,
            "password": "synthetic-private-value",
        });
        let mut collected = probe_engine::Collected {
            proxies: vec![proxy.clone()],
            ..Default::default()
        };
        let validator = ScriptedValidator::scripted(vec![ConfigCheck::Degraded(
            "no available native or Docker validator; no verdict".into(),
        )]);
        let state = Arc::new(
            test_app(
                Controller::new("http://127.0.0.1:1", None),
                tmp.path(),
                Vec::new(),
            )
            .with_validator(validator),
        );
        let blocked = preflight_round(
            &state,
            &mut collected,
            1,
            &probe_engine::RoundCtx::new(probe_engine::Limits::from_concurrency(1)),
        )
        .await;
        assert!(
            blocked.is_none(),
            "a missing validator must not block the round: {blocked:?}"
        );
        assert_eq!(collected.proxies.len(), 1, "the proxies survive a skip");
    }

    /// A cancelled round never enters preflight: the cancel route and the
    /// deadline timer must not have to wait out up to six 120-second
    /// `mihomo -t` attempts for a round that is already over.
    #[tokio::test]
    async fn a_cancelled_round_never_starts_preflight() {
        use probe_mihomo::config_check::ConfigCheck;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingValidator {
            calls: AtomicUsize,
        }
        impl probe_engine::KernelValidator for CountingValidator {
            fn validate(&self, _core_dir: &std::path::Path) -> ConfigCheck {
                self.calls.fetch_add(1, Ordering::SeqCst);
                ConfigCheck::Passed(String::new())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("core")).unwrap();
        // A valid secret so the cancelled check is the only way out.
        let validator = std::sync::Arc::new(CountingValidator {
            calls: AtomicUsize::new(0),
        });
        let counted = validator.clone();
        let mut state = test_app(
            Controller::new("http://127.0.0.1:1", None),
            tmp.path(),
            Vec::new(),
        );
        state.validator = validator;
        let state = std::sync::Arc::new(state);
        let mut collected = probe_engine::Collected {
            proxies: vec![serde_json::json!({
                "name": "node-a",
                "type": "trojan",
                "server": "origin.example",
                "port": 443,
                "password": "synthetic-private-value",
            })],
            ..Default::default()
        };
        let ctx = probe_engine::RoundCtx::new(probe_engine::Limits::from_concurrency(1));
        ctx.cancel();
        let blocked = preflight_round(&state, &mut collected, 1, &ctx).await;
        assert!(
            blocked.is_none(),
            "the cancelled round ends via run_round, not via a blocked note"
        );
        assert_eq!(
            counted.calls.load(Ordering::SeqCst),
            0,
            "a cancelled round must not spend a single validator attempt"
        );
    }
}
