//! The v1 source and run surface (workstreams/18; 《指导》§7/§8).
//!
//! Every source is addressed by its permanent `source_id` here, never by the
//! configured key: the key is an attribute an operator may rename, and a URL
//! built from it would fork the same logical subscription into a second
//! artifact the first time somebody tidied the config.
//!
//! Two contracts this module owns:
//!
//! * **Per-source single flight.** `POST /api/v1/sources/{id}/runs` claims a
//!   slot in `source_runs` before it starts anything. The claim carries the
//!   `Idempotency-Key` the caller sent (so a retry sees the same run) and a
//!   minimum spacing (so a double-click coalesces into the run that just
//!   happened). Only after the claim succeeds does the round start -- and if
//!   the global round lock refuses it, the claim is released so the retry that
//!   follows a 409 is not pinned to a run that never existed.
//! * **The stable subscription URL.** `GET /artifacts/{id}/subscription`
//!   answers the exact bytes of the last successful snapshot, with an ETag
//!   derived from its content digest, so Sub-Store (or any client) can point
//!   one immutable URL at a subscription that keeps updating underneath it.
//!
//! The `security_headers` middleware forces `Cache-Control: no-store` on every
//! response. That is right for the panel and wrong for the subscription: a
//! 304 plus a five-minute TTL is what keeps a 500-node list off the export
//! path every time a client refreshes. This module therefore carries its own
//! cache policy and [`build_router`] re-applies it after the middleware runs.

use std::collections::HashMap;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use probe_domain::DomainResult;
use probe_storage::runs::{ClaimOutcome, SourceRunRow};
use probe_storage::sources::{ArtifactRow, SourceRow};
use serde_json::json;

use crate::exports::{bad_request, error_response, internal, storage_failure};
use crate::{presented_token, spawn_round, token_matches, AppState, RoundStart, SharedState};

/// The maximum number of runs a history page returns when the caller is silent.
const DEFAULT_RUN_PAGE: usize = 20;
/// The reaper's grace before a claim whose round never appeared is abandoned.
/// Sized against the watchdog budget: a live round cannot outlive it.
const ORPHAN_CLAIM_BUDGET: Duration = Duration::from_secs(20 * 60);

/// The cache policy the stable subscription is served with.
const SUBSCRIPTION_CACHE: &str = "public, max-age=300, stale-if-error=86400";

/// The admin routes this module adds to the guarded group.
pub(crate) fn admin_routes() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/sources", get(list_sources))
        .route("/api/v1/sources/{source_id}", get(source_detail))
        .route(
            "/api/v1/sources/{source_id}/runs",
            post(start_source_run).get(list_source_runs),
        )
        .route("/api/v1/runs/{run_id}", get(run_detail))
}

/// The unauthenticated (but credential-checked) subscription route.
///
/// Kept out of the guarded group on purpose: a Sub-Store backend holds the
/// read-only publish credential, not the admin one, exactly like
/// `/api/export/<key>.yaml`.
pub(crate) fn public_routes() -> Router<SharedState> {
    Router::new().route(
        "/artifacts/{source_id}/subscription",
        get(subscription_yaml),
    )
}

/// The middleware that restores the subscription's cache policy.
///
/// `security_headers` overwrites `Cache-Control` with `no-store` on everything
/// (it runs last), so the subscription endpoint needs its headers re-applied
/// afterwards rather than set before. Only the cache headers this route owns
/// are touched, so the rest of the policy still applies.
pub(crate) async fn subscription_cache_headers(req: Request, next: Next) -> Response {
    let is_subscription = req.uri().path().starts_with("/artifacts/");
    let mut response = next.run(req).await;
    if is_subscription {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(SUBSCRIPTION_CACHE),
        );
    }
    response
}

// --- reads ----------------------------------------------------------------

/// `GET /api/v1/sources` -- one card per configured source, with its artifact.
async fn list_sources(State(state): State<SharedState>) -> Response {
    let rows = match state.storage.lock() {
        Ok(storage) => storage.list_sources(),
        Err(_) => return internal("ledger lock poisoned"),
    };
    match rows {
        Ok(sources) => {
            let mut out = Vec::with_capacity(sources.len());
            for source in &sources {
                out.push(source_dto(&state, source));
            }
            Json(json!({"sources": out})).into_response()
        }
        Err(err) => storage_failure(err),
    }
}

/// `GET /api/v1/sources/{source_id}` -- the same card, plus its artifact.
async fn source_detail(
    State(state): State<SharedState>,
    AxumPath(source_id): AxumPath<String>,
) -> Response {
    let source = match state.storage.lock() {
        Ok(storage) => storage.source_by_id(&source_id),
        Err(_) => return internal("ledger lock poisoned"),
    };
    match source {
        Ok(Some(source)) => Json(source_dto(&state, &source)).into_response(),
        Ok(None) => not_found("source does not exist"),
        Err(err) => storage_failure(err),
    }
}

/// `GET /api/v1/sources/{source_id}/runs?limit=N` -- the run history the
/// source detail page renders, newest first.
async fn list_source_runs(
    State(state): State<SharedState>,
    AxumPath(source_id): AxumPath<String>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let limit = match params.get("limit").filter(|raw| !raw.is_empty()) {
        None => DEFAULT_RUN_PAGE as i64,
        Some(raw) => match raw.parse::<i64>() {
            Ok(limit) if (1..=200).contains(&limit) => limit,
            _ => {
                return bad_request(
                    "invalid_limit",
                    "limit must be an integer between 1 and 200",
                )
            }
        },
    };
    let rows = match state.storage.lock() {
        Ok(storage) => storage.list_source_runs(&source_id, limit),
        Err(_) => return internal("ledger lock poisoned"),
    };
    match rows {
        Ok(runs) => {
            Json(json!({"runs": runs.iter().map(run_dto).collect::<Vec<_>>()})).into_response()
        }
        Err(err) => storage_failure(err),
    }
}

/// `GET /api/v1/runs/{run_id}` -- one slot, whichever source it belongs to.
async fn run_detail(State(state): State<SharedState>, AxumPath(run_id): AxumPath<i64>) -> Response {
    let row = match state.storage.lock() {
        Ok(storage) => storage.source_run(run_id),
        Err(_) => return internal("ledger lock poisoned"),
    };
    match row {
        Ok(Some(run)) => Json(run_dto(&run)).into_response(),
        Ok(None) => not_found("run does not exist"),
        Err(err) => storage_failure(err),
    }
}

/// `GET /artifacts/{source_id}/subscription` -- the stable subscription URL.
///
/// The credential is the read-only publish token (or the admin one), exactly
/// like the key-based export route. The body is the last successful snapshot
/// byte for byte, so a failed round leaves the previous good list in place
/// instead of handing a client an empty subscription.
///
/// `ETag` is the snapshot's content digest, quoted per RFC 9110, and
/// `If-None-Match` answers 304 with no body -- a client that polls this URL
/// every five minutes costs one indexed read when nothing changed.
async fn subscription_yaml(
    State(state): State<SharedState>,
    AxumPath(source_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let presented = presented_token(&headers);
    if !subscription_authorized(&state, presented.as_deref()) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a read credential is required (Authorization: Bearer or X-Auth-Token)",
        );
    }
    // Authentication first: an unauthenticated caller learns nothing about
    // which source ids exist.
    let source = match state.storage.lock() {
        Ok(storage) => storage.source_by_id(&source_id),
        Err(_) => return internal("ledger lock poisoned"),
    };
    let Ok(Some(source)) = source else {
        return not_found("no published artifact for this source");
    };
    // The artifact's file name is what the ledger's export key was created
    // from, and the active export under it is what a download would serve.
    let key = artifact_export_key(&state, &source).await;
    let Some(key) = key else {
        return not_found("no published artifact for this source");
    };
    let snapshot = match state.storage.lock() {
        Ok(storage) => storage.current_export(&key),
        Err(_) => return internal("ledger lock poisoned"),
    };
    match snapshot {
        Ok(Some(snapshot)) => {
            let etag = format!("\"{}\"", snapshot.summary.sha256);
            if if_none_match(&headers, &etag) {
                // 304 carries the validator and nothing else: no body, no
                // content type, but the cache headers still apply.
                return (
                    StatusCode::NOT_MODIFIED,
                    [(header::ETAG, etag)],
                    Body::empty(),
                )
                    .into_response();
            }
            let mut response = snapshot.content.into_response();
            *response.status_mut() = StatusCode::OK;
            let headers_out = response.headers_mut();
            headers_out.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/yaml"),
            );
            headers_out.insert(
                header::ETAG,
                HeaderValue::from_str(&etag).unwrap_or_else(|_| HeaderValue::from_static("\"\"")),
            );
            response
        }
        Ok(None) => not_found("no published artifact for this source"),
        Err(err) => storage_failure(err),
    }
}

// --- writes ---------------------------------------------------------------

/// `POST /api/v1/sources/{source_id}/runs` -- start this source's run.
///
/// The round is global (one round walks every enabled source), so what this
/// endpoint adds is the per-source *accounting*: a claim that deduplicates a
/// double-click, an `Idempotency-Key` that survives a retry, and a cooldown
/// that coalesces a second trigger into the run that just happened.
async fn start_source_run(
    State(state): State<SharedState>,
    AxumPath(source_id): AxumPath<String>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    // Body is optional; a present body is read for the optional key only, so
    // a malformed one never blocks a legitimate trigger.
    let idempotency_key = headers
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .or_else(|| {
            body.as_ref().and_then(|Json(value)| {
                value
                    .get("idempotency_key")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .map(str::to_string)
            })
        });

    let source = match state.storage.lock() {
        Ok(storage) => storage.source_by_id(&source_id),
        Err(_) => return internal("ledger lock poisoned"),
    };
    let Ok(Some(source)) = source else {
        return not_found("source does not exist");
    };
    let cooldown = Duration::from_secs(state.min_run_interval_seconds.max(0) as u64);
    let claim = match state.storage.lock() {
        Ok(storage) => storage.claim_source_run(
            &source.source_id,
            "api-manual",
            idempotency_key.as_deref(),
            cooldown,
        ),
        Err(_) => return internal("ledger lock poisoned"),
    };
    let claim = match claim {
        Ok(claim) => claim,
        Err(err) => return storage_failure(err),
    };
    match claim {
        ClaimOutcome::Claimed { run_id } => {
            match start_round_for_claim(&state, run_id) {
                Ok(round_id) => Json(json!({
                    "run_id": run_id,
                    "round_id": round_id,
                    "status": "started",
                }))
                .into_response(),
                Err(response) => {
                    // The claim could not become a round: release it so a retry
                    // (including one with the same Idempotency-Key) can succeed.
                    if let Ok(storage) = state.storage.lock() {
                        let _ = storage.release_source_run(run_id);
                    }
                    *response
                }
            }
        }
        ClaimOutcome::Deduped {
            run_id,
            round_id,
            open,
        } => Json(json!({
            "run_id": run_id,
            "round_id": round_id,
            "status": if open { "running" } else { "completed" },
            "deduplicated": true,
        }))
        .into_response(),
        ClaimOutcome::Coalesced { run_id, round_id } => Json(json!({
            "run_id": run_id,
            "round_id": round_id,
            "status": "coalesced",
            "deduplicated": true,
        }))
        .into_response(),
        ClaimOutcome::KeyConflict { run_id } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": {"code": "idempotency_key_conflict",
                          "message": "this Idempotency-Key already belongs to another source",
                          "run_id": run_id},
            })),
        )
            .into_response(),
    }
}

/// Start a round for a claimed slot and attach the slot to it.
///
/// The round start is the API's own path, so the global single-flight rules
/// (in-process flag plus the cross-process round lock) apply unchanged. The
/// only thing added here is binding the claim to the round row that opens.
///
/// The error is boxed: an `axum::Response` is ~200 bytes, and clippy refuses a
/// `Result` whose error arm dwarfs the success arm.
fn start_round_for_claim(state: &SharedState, run_id: i64) -> Result<i64, Box<Response>> {
    // `spawn_round` takes a `'static str` trigger because it is spawned; the
    // per-source trigger is recorded on the slot itself, which is where the
    // history reads it from.
    match spawn_round(state, "api-manual", None) {
        RoundStart::Started(round_id) => {
            let attached = state.storage.lock().map(|storage| {
                storage
                    .attach_source_run(run_id, round_id)
                    .map(|()| round_id)
            });
            match attached {
                Ok(Ok(round_id)) => Ok(round_id),
                Ok(Err(err)) => Err(Box::new(storage_failure(err))),
                Err(_) => Err(Box::new(internal("ledger lock poisoned"))),
            }
        }
        RoundStart::Busy => Err(Box::new(
            (
                StatusCode::CONFLICT,
                Json(json!({"error": {"code": "busy",
                "message": "a round is already running; the claim is released, retry"}})),
            )
                .into_response(),
        )),
        RoundStart::Storage(err) => Err(Box::new(storage_failure(err))),
    }
}

/// Close the slots a crashed process left open. Called by the scheduler tick
/// beside the round reaper: a claim without a round belongs to a process that
/// died between the claim and the round start, and the source must accept
/// triggers again once the budget has passed.
pub(crate) fn reap_orphan_claims(storage: &probe_storage::Storage) -> DomainResult<usize> {
    storage.reap_orphan_source_runs(ORPHAN_CLAIM_BUDGET)
}

// --- DTOs and helpers -----------------------------------------------------

/// The publish/admin credential check for the subscription route.
///
/// Kept separate from `exports::export_authorized` because the two must agree
/// in *effect* (either credential reads either surface) while differing in
/// source: this one takes the presented token directly rather than a query
/// parameter, matching the route's documented header-only contract.
fn subscription_authorized(state: &AppState, presented: Option<&str>) -> bool {
    let Some(presented) = presented else {
        return false;
    };
    let publish_ok = state
        .publish
        .token
        .as_deref()
        .filter(|token| !token.trim().is_empty())
        .is_some_and(|expected| token_matches(presented, expected));
    let admin_ok = state
        .auth_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .is_some_and(|expected| token_matches(presented, expected));
    publish_ok | admin_ok
}

/// One source card: the configured attributes, the artifact, and the stable
/// URL a client should point at.
fn source_dto(state: &AppState, source: &SourceRow) -> serde_json::Value {
    let artifact = state.storage.lock().ok().and_then(|storage| {
        storage
            .artifact_for_source(&source.source_id)
            .ok()
            .flatten()
    });
    json!({
        "source_id": source.source_id,
        "key": source.key,
        "display_name": source.display_name,
        "input_type": source.input_type,
        "input_ref": source.input_ref,
        "enabled": source.enabled,
        "artifact": artifact.as_ref().map(artifact_dto),
    })
}

/// The artifact card: the stable URL, the Gist mapping, and the last published
/// digest. `gist_id` is echoed (it is an identifier, not a credential) but the
/// token is never anywhere near this crate.
fn artifact_dto(artifact: &ArtifactRow) -> serde_json::Value {
    json!({
        "artifact_id": artifact.artifact_id,
        "gist_file": artifact.gist_file,
        "gist_id": artifact.gist_id,
        "stable_url": artifact.stable_url,
        "last_content_sha256": artifact.last_content_sha256,
        "last_success_run_id": artifact.last_success_run_id,
        "last_published_at": artifact.last_published_at,
    })
}

/// The export key a source's artifact publishes under.
///
/// The artifact row stores the Gist file name (`<key>.yaml`); the ledger's
/// export key is the same stem. Resolving it through the storage layer rather
/// than stripping the suffix keeps the two namespaces from drifting.
async fn artifact_export_key(state: &AppState, source: &SourceRow) -> Option<String> {
    let artifact = state.storage.lock().ok().and_then(|storage| {
        storage
            .artifact_for_source(&source.source_id)
            .ok()
            .flatten()
    })?;
    artifact
        .gist_file
        .strip_suffix(".yaml")
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .or(Some(source.key.clone()))
}

/// The run slot DTO. The idempotency key is a boolean, never the key itself:
/// it belongs to the caller, and echoing it into an admin response would put
/// a client's retry handle into a log line. The publish columns are the
/// remote leg's masked bookkeeping (workstreams/18 B2): a status word and a
/// frozen error code, never a message body.
fn run_dto(run: &SourceRunRow) -> serde_json::Value {
    json!({
        "run_id": run.id,
        "source_id": run.source_id,
        "round_id": run.round_id,
        "trigger": run.trigger,
        "status": run.status,
        "has_idempotency_key": run.has_idempotency_key,
        "claimed_at": run.claimed_at,
        "updated_at": run.updated_at,
        "publish_status": run.publish_status,
        "error_code": run.error_code,
    })
}

/// The RFC 9110 `If-None-Match` comparison for one entity-tag.
///
/// A single quoted tag is compared literally (after trimming the surrounding
/// quotes), because that is what a well-behaved client sends; `*` matches any
/// current representation and is the one list form worth honouring.
fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    let Some(header_value) = headers.get(header::IF_NONE_MATCH) else {
        return false;
    };
    let Ok(raw) = header_value.to_str() else {
        return false;
    };
    raw.split(',')
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
        .any(|candidate| candidate == "*" || candidate.trim_matches('"') == etag.trim_matches('"'))
}

fn not_found(message: &'static str) -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found", message)
}
