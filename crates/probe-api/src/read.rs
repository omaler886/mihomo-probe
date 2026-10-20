//! The v1 read/control surface (workstreams/10 frozen contract).
//!
//! This module owns the endpoints that answer questions about the ledger --
//! nodes and rounds -- plus the one control endpoint, round cancel. They are
//! all admin-only: [`crate::build_router`] mounts them behind the admin guard,
//! so the read-only publish credential can never reach them.
//!
//! Every page is keyset-paged by the storage readers (never OFFSET), the node
//! DTO is a strict field whitelist (no credentials, and `server` never carries
//! a URL), and every error uses the one `{"error":{"code","message"}}` shape.

use std::collections::HashMap;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use probe_domain::quality::{summarize_samples, QualitySample};
use probe_storage::{NodeHistoryRow, NodeRow, ResultRow, RoundRow, MAX_READ_LIMIT};
use serde_json::{json, Value};

use crate::exports::{bad_request, error_response, internal, storage_failure};
use crate::{CancelOutcome, SharedState};

/// Page size when the caller passes no `limit`.
const DEFAULT_PAGE: usize = 50;
/// Status moves returned by the node-detail endpoint when no limit is given.
const DEFAULT_NODE_HISTORY: usize = 20;
/// Observations the node-detail quality summary is computed from.
const QUALITY_WINDOW: usize = 50;
/// Longest `note` a round DTO carries; a longer ledger note is cut at a
/// character boundary and marked with an ellipsis.
const MAX_NOTE_CHARS: usize = 500;

/// The admin routes this module adds to the guarded group.
pub(crate) fn admin_routes() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/nodes", get(list_nodes))
        .route("/api/v1/nodes/{fingerprint}", get(node_detail))
        .route("/api/v1/rounds", get(list_rounds))
        .route("/api/v1/rounds/{id}", get(round_detail))
        .route("/api/v1/rounds/{id}/cancel", post(cancel_round))
        .route("/api/v1/events", get(list_events))
}

// --- query parsing -------------------------------------------------------

/// A bounded page size: default when absent, `None` when malformed or outside
/// `1..=MAX_READ_LIMIT`. The storage layer refuses the same range; parsing it
/// here turns a bad request into a 400 instead of a 500.
fn page_limit(params: &HashMap<String, String>, key: &str, default: usize) -> Option<usize> {
    match params.get(key).filter(|raw| !raw.is_empty()) {
        None => Some(default),
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=MAX_READ_LIMIT).contains(limit)),
    }
}

fn invalid_limit() -> Response {
    bad_request(
        "invalid_limit",
        format!("page size must be an integer between 1 and {MAX_READ_LIMIT}"),
    )
}

/// `<source>/<fingerprint>`: a source key can never contain a slash and a
/// fingerprint is 16 hex characters, so the first slash is unambiguous.
fn parse_node_cursor(raw: &str) -> Option<(&str, &str)> {
    let (source, fingerprint) = raw.split_once('/')?;
    if source.is_empty() || fingerprint.is_empty() {
        return None;
    }
    Some((source, fingerprint))
}

fn not_found(message: &'static str) -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found", message)
}

// --- DTOs ----------------------------------------------------------------

/// The only fields a node ever exposes. Credentials, the raw kernel alias and
/// bookkeeping timestamps stay in the ledger.
fn node_dto(row: &NodeRow) -> Value {
    json!({
        "source": row.source,
        "fingerprint": row.fingerprint,
        "display": row.display,
        "proto": row.proto,
        "server": row.server.as_deref().map(host_only),
        "country": row.country,
        "status": row.status,
        "consec_fail": row.consec_fail,
        "total_ok": row.total_ok,
        "total_fail": row.total_fail,
        "last_delay_ms": row.last_delay_ms,
        "last_reason": row.last_reason,
        "ip_alive": row.ip_alive,
        "ip_total": row.ip_total,
        "exit_ip": row.exit_ip,
        "exit_asn": row.exit_asn,
    })
}

/// Reduce a stored server value to host/address form: no scheme, userinfo,
/// path, query or fragment can reach a response.
fn host_only(server: &str) -> String {
    let trimmed = server.trim();
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let after_user = after_scheme
        .rsplit_once('@')
        .map(|(_, rest)| rest)
        .unwrap_or(after_scheme);
    after_user
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Summarize a node's newest results into the quality block the panel shows.
///
/// `None` when the window is empty: a node with no history has no quality,
/// and zeros would read as "measured 0 ms". Failed observations still count
/// towards success rate but are excluded from the latency distribution by
/// [`summarize_samples`], which is exactly the split the panel needs.
fn quality_dto(rows: &[ResultRow], transfers: &[probe_storage::QualitySampleRow]) -> Option<Value> {
    if rows.is_empty() && transfers.is_empty() {
        return None;
    }
    // Jitter is defined over input order, so feed the window oldest-first
    // even though the query returns newest-first.
    let samples: Vec<QualitySample> = rows
        .iter()
        .rev()
        .map(|row| QualitySample {
            success: row.verdict.as_deref() == Some("ok"),
            latency_ms: row.delay_ms.filter(|delay| *delay >= 0).map(|d| d as f64),
            exit_identity: row.exit_ip.clone(),
            transfer: None,
        })
        .collect();
    let statistics = summarize_samples(&samples).ok()?;
    let exits = samples
        .iter()
        .filter_map(|sample| sample.exit_identity.as_deref())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    Some(json!({
        "window": rows.len(),
        "samples": statistics.sample_count,
        "successes": statistics.success_count,
        "success_rate": statistics.success_rate,
        "min_ms": statistics.latency.min_ms,
        "median_ms": statistics.latency.median_ms,
        "p90_ms": statistics.latency.p90_ms,
        "p95_ms": statistics.latency.p95_ms,
        "max_ms": statistics.latency.max_ms,
        "jitter_ms": statistics.latency.jitter_ms,
        "exit_stability": statistics.exit_stability,
        "exits": exits,
        // Measured throughput: aggregate payload over aggregate time, in the
        // same decimal megabits-per-second unit the quality module uses.
        "transfers": transfers.len(),
        "throughput_mbps": throughput_mbps(transfers),
    }))
}

/// Decimal megabits per second over every recorded transfer, or `None`.
fn throughput_mbps(transfers: &[probe_storage::QualitySampleRow]) -> Option<f64> {
    let bytes: i64 = transfers.iter().map(|sample| sample.bytes).sum();
    let elapsed: f64 = transfers.iter().map(|sample| sample.elapsed_ms).sum();
    if bytes <= 0 || elapsed <= 0.0 {
        return None;
    }
    Some((bytes as f64) * 0.008 / elapsed)
}

fn history_dto(row: &NodeHistoryRow) -> Value {
    json!({
        "id": row.id,
        "ts": row.ts,
        "from_status": row.from_status,
        "to_status": row.to_status,
        "reason": row.reason,
        "round_id": row.round_id,
    })
}

pub(crate) fn round_dto(row: &RoundRow) -> Value {
    json!({
        "round_id": row.round_id,
        "started_at": row.started_at,
        "finished_at": row.finished_at,
        "trigger": row.trigger,
        "total": row.total,
        "ok": row.ok,
        "failed": row.failed,
        "dropped": row.dropped,
        "restored": row.restored,
        "suspect": row.suspect,
        "inconclusive": row.inconclusive,
        "duration_s": row.duration_s,
        "note": row.note.as_deref().map(truncate_note),
    })
}

fn truncate_note(note: &str) -> String {
    if note.chars().count() <= MAX_NOTE_CHARS {
        return note.to_string();
    }
    let mut truncated: String = note.chars().take(MAX_NOTE_CHARS).collect();
    truncated.push('…');
    truncated
}

// --- handlers ------------------------------------------------------------

/// `GET /api/v1/nodes?limit=&after=&status=&source=` -- keyset-paged nodes.
async fn list_nodes(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(limit) = page_limit(&params, "limit", DEFAULT_PAGE) else {
        return invalid_limit();
    };
    let after = match params.get("after").filter(|raw| !raw.is_empty()) {
        None => None,
        Some(raw) => match parse_node_cursor(raw) {
            Some(cursor) => Some(cursor),
            None => return bad_request("invalid_cursor", "after must be <source>/<fingerprint>"),
        },
    };
    let status = params
        .get("status")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let source = params
        .get("source")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let listed = match state.storage.lock() {
        Ok(storage) => storage.list_nodes(limit, after, status, source),
        Err(_) => return internal("ledger lock poisoned"),
    };
    let rows = match listed {
        Ok(rows) => rows,
        Err(err) => return storage_failure(err),
    };
    let next_after = if rows.len() == limit {
        rows.last()
            .map(|row| format!("{}/{}", row.source, row.fingerprint))
    } else {
        None
    };
    let nodes: Vec<Value> = rows.iter().map(node_dto).collect();
    (
        StatusCode::OK,
        Json(json!({"nodes": nodes, "limit": limit, "next_after": next_after})),
    )
        .into_response()
}

/// `GET /api/v1/events?limit=` -- the operational event log, newest first.
///
/// Python's `db.recent_events(200)`, same shape: the ledger keeps the newest
/// 2000 rows and this reads a bounded page of them.
async fn list_events(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(limit) = page_limit(&params, "limit", DEFAULT_EVENT_PAGE) else {
        return invalid_limit();
    };
    let events = match state.storage.lock() {
        Ok(storage) => storage.recent_events(limit),
        Err(_) => return internal("ledger lock poisoned"),
    };
    match events {
        Ok(rows) => {
            let events: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "id": row.id,
                        "ts": row.ts,
                        "level": row.level,
                        "message": row.message,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({"events": events, "limit": limit})),
            )
                .into_response()
        }
        Err(err) => storage_failure(err),
    }
}

/// Events returned when the caller names no limit.
const DEFAULT_EVENT_PAGE: usize = 50;

/// `GET /api/v1/nodes/{fingerprint}` -- one node plus its newest status moves.
async fn node_detail(
    State(state): State<SharedState>,
    AxumPath(fingerprint): AxumPath<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(history_limit) = page_limit(&params, "history_limit", DEFAULT_NODE_HISTORY) else {
        return invalid_limit();
    };
    let found = match state.storage.lock() {
        Ok(storage) => match storage.find_node_by_fingerprint(&fingerprint) {
            Ok(Some(row)) => {
                let history =
                    storage.node_state_history(&row.source, &row.fingerprint, history_limit);
                let recent = storage.recent_results(&row.source, &row.fingerprint, QUALITY_WINDOW);
                let transfers =
                    storage.recent_quality(&row.source, &row.fingerprint, QUALITY_WINDOW);
                match (history, recent, transfers) {
                    (Ok(history), Ok(recent), Ok(transfers)) => {
                        Ok(Some((row, history, recent, transfers)))
                    }
                    (Err(err), _, _) | (_, Err(err), _) | (_, _, Err(err)) => Err(err),
                }
            }
            Ok(None) => Ok(None),
            Err(err) => Err(err),
        },
        Err(_) => return internal("ledger lock poisoned"),
    };
    match found {
        Ok(Some((row, history, recent, transfers))) => {
            let history: Vec<Value> = history.iter().map(history_dto).collect();
            (
                StatusCode::OK,
                Json(json!({
                    "node": node_dto(&row),
                    "history": history,
                    "history_limit": history_limit,
                    "quality": quality_dto(&recent, &transfers),
                })),
            )
                .into_response()
        }
        Ok(None) => not_found("no such node"),
        Err(err) => storage_failure(err),
    }
}

/// `GET /api/v1/rounds?limit=&before_id=` -- rounds newest first.
async fn list_rounds(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(limit) = page_limit(&params, "limit", DEFAULT_PAGE) else {
        return invalid_limit();
    };
    let before_id = match params.get("before_id").filter(|raw| !raw.is_empty()) {
        None => None,
        Some(raw) => match raw.parse::<i64>() {
            Ok(id) if id > 0 => Some(id),
            _ => return bad_request("invalid_cursor", "before_id must be a positive round id"),
        },
    };
    let listed = match state.storage.lock() {
        Ok(storage) => storage.list_rounds(limit, before_id),
        Err(_) => return internal("ledger lock poisoned"),
    };
    let rows = match listed {
        Ok(rows) => rows,
        Err(err) => return storage_failure(err),
    };
    let next_before_id = if rows.len() == limit {
        rows.last().map(|row| row.round_id)
    } else {
        None
    };
    let rounds: Vec<Value> = rows.iter().map(round_dto).collect();
    (
        StatusCode::OK,
        Json(json!({
            "rounds": rounds,
            "limit": limit,
            "next_before_id": next_before_id,
        })),
    )
        .into_response()
}

/// `GET /api/v1/rounds/{id}` -- one round, or 404.
async fn round_detail(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let round_id = match id.parse::<i64>() {
        Ok(round_id) if round_id > 0 => round_id,
        _ => return not_found("no such round"),
    };
    let found = match state.storage.lock() {
        Ok(storage) => storage.round_by_id(round_id),
        Err(_) => return internal("ledger lock poisoned"),
    };
    match found {
        Ok(Some(row)) => (StatusCode::OK, Json(round_dto(&row))).into_response(),
        Ok(None) => not_found("no such round"),
        Err(err) => storage_failure(err),
    }
}

/// `POST /api/v1/rounds/{id}/cancel` -- end the round that is currently in flight.
///
/// The id has to match the round that actually holds the in-flight slot:
/// nothing running is a 409 (the caller's id was never compared, so the answer
/// cannot claim that round exists), and a different round in flight is a 404.
async fn cancel_round(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let round_id = match id.parse::<i64>() {
        Ok(round_id) if round_id > 0 => round_id,
        _ => return not_found("no such round"),
    };
    match state.cancel_round(round_id) {
        CancelOutcome::Cancelled => (
            StatusCode::OK,
            Json(json!({"round_id": round_id, "cancelling": true})),
        )
            .into_response(),
        CancelOutcome::NotRunning => {
            error_response(StatusCode::CONFLICT, "no_round", "no round is running")
        }
        CancelOutcome::Mismatch => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "round is not the one in flight",
        ),
    }
}

#[cfg(test)]
mod tests;
