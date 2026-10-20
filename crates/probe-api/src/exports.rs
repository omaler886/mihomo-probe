//! Read side of the publication surface (R8): the Sub-Store-facing export
//! download plus the admin metadata and rollback endpoints.
//!
//! The download contract is fixed by the Python panel this replaces: Sub-Store
//! stores `GET /api/export/<key>.yaml?token=<publish token>` once and then
//! consumes the response verbatim. The body is therefore
//! `current_export(key).content` byte for byte -- never re-serialised, never
//! redacted -- and the credential is the read-only publish token
//! (`publish.token`), or the admin token for an operator testing by hand.
//!
//! Security rules (master document §13): an absent or empty publish token
//! denies every download (never "auth off"), both credentials are compared in
//! constant time, a publish token reaches nothing outside this download, and
//! the admin DTOs are built from `SnapshotSummary`, which carries no content.

use std::collections::HashMap;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use probe_domain::DomainError;
use probe_storage::publication::{validate_export_key, SnapshotSummary, MAX_EXPORT_PAGE};
use serde::Deserialize;
use serde_json::json;

use crate::{presented_token, token_matches, AppState, SharedState};

/// Page size when the caller passes no `limit`.
const DEFAULT_PAGE: usize = 50;
/// Audit principal for API-driven rollbacks; never a token or a URL.
const API_ACTOR: &str = "api-admin";

/// The unauthenticated (but token-checked) download route.
pub(crate) fn public_routes() -> Router<SharedState> {
    Router::new().route("/api/export/{file}", get(export_yaml))
}

/// The admin routes; `build_router` mounts these behind the admin guard.
pub(crate) fn admin_routes() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/exports", get(list_exports))
        .route("/api/v1/exports/{key}/history", get(export_history))
        .route("/api/v1/exports/{key}/rollback", post(rollback_export))
}

/// One error envelope shape for every new endpoint, matching the v1 surface.
pub(crate) fn error_response(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message.into()}})),
    )
        .into_response()
}

fn unauthorized() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "send ?token=<publish token> or header X-Auth-Token",
    )
}

/// Deliberately constant: an invalid key and an unpublished key must be
/// indistinguishable, so neither the error body nor its timing becomes a key
/// oracle for a publish credential.
fn no_active_export() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "not_found",
        "no active export for this key",
    )
}

pub(crate) fn bad_request(code: &'static str, message: impl Into<String>) -> Response {
    error_response(StatusCode::BAD_REQUEST, code, message)
}

pub(crate) fn internal(message: impl Into<String>) -> Response {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "storage", message)
}

pub(crate) fn storage_failure(err: DomainError) -> Response {
    internal(err.to_string())
}

/// Parse the `limit` query parameter: default 50, strictly 1..=MAX_EXPORT_PAGE.
///
/// Returns `None` for a malformed or out-of-range request; the callers build
/// the error response, which keeps a large `Response` out of a `Result`'s
/// error variant (clippy's `result_large_err`).
fn page_limit(params: &HashMap<String, String>) -> Option<usize> {
    match params.get("limit") {
        None => Some(DEFAULT_PAGE),
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=MAX_EXPORT_PAGE).contains(limit)),
    }
}

fn invalid_limit() -> Response {
    bad_request(
        "invalid_limit",
        format!("limit must be an integer between 1 and {MAX_EXPORT_PAGE}"),
    )
}

/// Authorize the download credential: the publish token or the admin token.
/// Constant-time; an absent or blank configured token can never match.
fn export_authorized(state: &AppState, presented: Option<&str>) -> bool {
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
    // Bitwise OR: both comparisons always run, so the response time does not
    // reveal which credential matched (or that one is configured at all).
    publish_ok | admin_ok
}

/// `GET /api/export/<key>.yaml` -- the exact stored bytes for one active export.
async fn export_yaml(
    State(state): State<SharedState>,
    AxumPath(file): AxumPath<String>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let header_token = presented_token(&headers);
    let presented = params
        .get("token")
        .filter(|token| !token.is_empty())
        .map(String::as_str)
        .or(header_token.as_deref());
    // Authentication runs before key validation: an unauthenticated caller
    // learns nothing about which keys exist.
    if !export_authorized(&state, presented) {
        return unauthorized();
    }
    let Some(key) = file.strip_suffix(".yaml") else {
        return no_active_export();
    };
    if validate_export_key(key).is_err() {
        return no_active_export();
    }
    let current = match state.storage.lock() {
        Ok(storage) => storage.current_export(key),
        Err(_) => return internal("ledger lock poisoned"),
    };
    match current {
        Ok(Some(snapshot)) => {
            // The stored document is already the exact bytes Sub-Store needs
            // (valid ClashMeta YAML); never round-trip it through a parser.
            let mut response = snapshot.content.into_response();
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/yaml"),
            );
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Ok(None) => no_active_export(),
        Err(err) => {
            // The key was validated above, so a read error means the ledger is
            // corrupt -- say so instead of pretending the export is missing.
            storage_failure(err)
        }
    }
}

/// `GET /api/v1/exports` -- paged active-export metadata, never content.
async fn list_exports(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(limit) = page_limit(&params) else {
        return invalid_limit();
    };
    let after_key = params
        .get("after_key")
        .map(String::as_str)
        .filter(|key| !key.is_empty());
    if let Some(key) = after_key {
        if validate_export_key(key).is_err() {
            return bad_request("invalid_key", "after_key is not a valid export key");
        }
    }
    let listed = match state.storage.lock() {
        Ok(storage) => storage.list_exports(limit, after_key),
        Err(_) => return internal("ledger lock poisoned"),
    };
    let rows = match listed {
        Ok(rows) => rows,
        Err(err) => return storage_failure(err),
    };
    let next_after_key = next_after_key(&rows, limit);
    (
        StatusCode::OK,
        Json(json!({
            "exports": rows,
            "limit": limit,
            "next_after_key": next_after_key,
        })),
    )
        .into_response()
}

/// Cursor for the next page: the last key returned when the page was full.
fn next_after_key(rows: &[SnapshotSummary], limit: usize) -> Option<String> {
    if rows.len() == limit {
        rows.last().map(|row| row.key.clone())
    } else {
        None
    }
}

/// `GET /api/v1/exports/<key>/history` -- paged snapshot metadata, newest first.
async fn export_history(
    State(state): State<SharedState>,
    AxumPath(key): AxumPath<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if validate_export_key(&key).is_err() {
        return bad_request("invalid_key", "not a valid export key");
    }
    let Some(limit) = page_limit(&params) else {
        return invalid_limit();
    };
    let before_id = match params.get("before_id").filter(|raw| !raw.is_empty()) {
        None => None,
        Some(raw) => match raw.parse::<i64>() {
            Ok(id) if id > 0 => Some(id),
            _ => return bad_request("invalid_before_id", "before_id must be a positive integer"),
        },
    };
    let history = match state.storage.lock() {
        Ok(storage) => storage.export_history(&key, limit, before_id),
        Err(_) => return internal("ledger lock poisoned"),
    };
    let rows = match history {
        Ok(rows) => rows,
        Err(err) => return storage_failure(err),
    };
    let next_before_id = if rows.len() == limit {
        rows.last().map(|row| row.id)
    } else {
        None
    };
    (
        StatusCode::OK,
        Json(json!({
            "key": key,
            "history": rows,
            "limit": limit,
            "next_before_id": next_before_id,
        })),
    )
        .into_response()
}

/// Explicit rollback body: the snapshot to activate and the caller's view of
/// the current pointer (a compare-and-swap baseline, `null` = no active export).
#[derive(Debug, Deserialize)]
struct RollbackRequest {
    target_id: i64,
    #[serde(default)]
    expected_active: Option<i64>,
}

/// `POST /api/v1/exports/<key>/rollback` -- guarded re-activation with CAS.
async fn rollback_export(
    State(state): State<SharedState>,
    AxumPath(key): AxumPath<String>,
    request: Result<Json<RollbackRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(json) => json,
        Err(rejection) => {
            return bad_request(
                "invalid_body",
                format!("invalid rollback body: {rejection}"),
            )
        }
    };
    if validate_export_key(&key).is_err() {
        return bad_request("invalid_key", "not a valid export key");
    }
    if request.target_id <= 0 || request.expected_active.is_some_and(|id| id <= 0) {
        return bad_request(
            "invalid_target",
            "target_id and expected_active must be positive snapshot ids",
        );
    }
    let storage = match state.storage.lock() {
        Ok(storage) => storage,
        Err(_) => return internal("ledger lock poisoned"),
    };
    // The ledger mutex makes the read-check-write below atomic with respect to
    // every other handler; the engine's rollback path still re-checks the CAS
    // itself, which is what protects against another process sharing the file.
    //
    // Both pre-checks are metadata-only: the pointer and the target's
    // existence. The full body is loaded and validated exactly once, inside
    // the engine's materialize-then-CAS path -- loading and hashing three
    // ≤8 MiB bodies under this mutex stalled every other ledger endpoint
    // for nothing.
    let active_id = match storage.active_export_id(&key) {
        Ok(active_id) => active_id,
        Err(err) => return storage_failure(err),
    };
    if active_id != request.expected_active {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": {
                    "code": "conflict",
                    "message": "active export changed; retry with the current snapshot id",
                },
                "active_id": active_id,
                "expected_active": request.expected_active,
            })),
        )
            .into_response();
    }
    match storage.export_snapshot_exists(&key, request.target_id) {
        Ok(true) => {}
        Ok(false) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                "rollback target snapshot not found",
            )
        }
        Err(err) => return storage_failure(err),
    }
    // The engine's path, not the storage shortcut: it re-materializes and
    // re-validates the target's immutable file before the CAS, so the active
    // pointer can never reference a snapshot whose file is missing. The root
    // is provisioned the same way the round tail does before publishing.
    let export_root = state.publish.export_root(&state.root);
    if let Err(err) = crate::ensure_export_root(&export_root) {
        return storage_failure(probe_domain::DomainError::Storage(format!(
            "cannot prepare the export root: {err}"
        )));
    }
    match probe_engine::rollback_export(
        &storage,
        &export_root,
        &key,
        request.target_id,
        API_ACTOR,
        request.expected_active,
    ) {
        Ok((summary, _path)) => (
            StatusCode::OK,
            Json(json!({
                "key": key,
                "rolled_back": true,
                "previous_active": active_id,
                "export": summary,
            })),
        )
            .into_response(),
        Err(err) => rollback_failure(err),
    }
}

/// Map an unexpected rollback failure without pretending a CAS race was a
/// server bug (the pre-check above already covers the in-process case).
fn rollback_failure(err: probe_engine::publish::PublishError) -> Response {
    match &err {
        probe_engine::publish::PublishError::Invalid(
            "rollback snapshot not found for export key",
        ) => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "rollback target snapshot not found",
        ),
        probe_engine::publish::PublishError::Storage(probe_domain::DomainError::Storage(
            message,
        )) if message.contains("active export changed") => error_response(
            StatusCode::CONFLICT,
            "conflict",
            "active export changed; retry with the current snapshot id",
        ),
        probe_engine::publish::PublishError::Storage(probe_domain::DomainError::Storage(
            message,
        )) if message.contains("not found") => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "rollback target snapshot not found",
        ),
        probe_engine::publish::PublishError::Storage(err) => {
            storage_failure(probe_domain::DomainError::Storage(err.to_string()))
        }
        probe_engine::publish::PublishError::Invalid(reason) => {
            storage_failure(probe_domain::DomainError::Storage(reason.to_string()))
        }
        probe_engine::publish::PublishError::Io(kind) => storage_failure(
            probe_domain::DomainError::Storage(format!("export filesystem error: {kind:?}")),
        ),
    }
}

#[cfg(test)]
mod tests;
