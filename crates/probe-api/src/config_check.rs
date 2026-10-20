//! `GET /api/v1/config/validate` -- the config self-check.
//!
//! [`probe_config::Config::load`] treats an unreadable or non-JSON file as an
//! empty object (Python `stored = {}`), which is the right *runtime* behaviour
//! -- the service must still start -- but it means a typo in `config.json`
//! silently degrades the deployment to defaults: no sources, no publish
//! credential, a fresh subscription. Nothing in the logs says so.
//!
//! This endpoint parses the same file the process was started from and reports
//! whether it was JSON at all, what the effective values are, and which of them
//! differ from the values this process is actually running with (the usual
//! cause: the file was edited and the service was not restarted). Every secret
//! is reduced to a `*_configured` boolean; no token, URL or credential is ever
//! echoed.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::SharedState;

pub fn routes() -> Router<SharedState> {
    Router::new().route("/api/v1/config/validate", get(validate))
}

/// The effective values an operator needs to confirm, never a secret.
fn summary(config: &probe_config::Config) -> serde_json::Value {
    let payload = &config.verify.chain_payload;
    json!({
        "sources": {
            "total": config.sources.len(),
            "enabled": config.sources.iter().filter(|source| source.enabled).count(),
            "exporting": config.sources.iter().filter(|source| source.export).count(),
        },
        "publish": {
            "enabled": config.publish.enabled,
            "push_to_substore": config.publish.push_to_substore,
            "prefix": config.publish.prefix,
            "hostname": config.publish.hostname,
            "add_region_tag": config.publish.add_region_tag,
            "dir": config.publish.dir,
            "keep_snapshots": config.publish.keep_snapshots,
            "token_configured": config.publish.token.is_some(),
        },
        "schedule": {
            "enabled": config.schedule.enabled,
            "interval_minutes": config.schedule.interval_minutes,
        },
        "verify": {
            "enabled": config.verify.enabled,
            // URLs are free-form operator strings and may carry credentials
            // (query keys, userinfo); like `geo_url`, they are reduced to a
            // boolean instead of echoed -- the endpoint's own contract.
            "trace_url_configured": !config.verify.trace_url.trim().is_empty(),
            "exclude_countries": config.verify.exclude_countries,
            "payload_enabled": payload.enabled,
            "payload_scope": match payload.scope {
                probe_config::PayloadScope::All => "all",
                probe_config::PayloadScope::Chain => "chain",
            },
            "payload_url_configured": !payload.url.trim().is_empty(),
            "payload_max_bytes": payload.max_bytes,
        },
        "core": {"lanes": config.core.lanes, "base_port": config.core.base_port},
        "chain": {"enabled": config.chain.enabled},
        "auth_token_configured": config.auth_token.is_some(),
        "substore": {"embedded": config.substore.embedded},
    })
}

/// Values where the file and the running process disagree.
///
/// Only scalars an operator changes by hand are compared: a full diff would be
/// noise, and the question this answers is "did my edit take effect?".
fn differences(config: &probe_config::Config, state: &crate::AppState) -> Vec<String> {
    let mut out = Vec::new();
    let running = &state;
    if config.schedule.enabled != running.schedule.enabled {
        out.push(format!(
            "schedule.enabled: file {} != running {}",
            config.schedule.enabled, running.schedule.enabled
        ));
    }
    if config.schedule.interval_minutes != running.schedule.interval_minutes {
        out.push(format!(
            "schedule.interval_minutes: file {} != running {}",
            config.schedule.interval_minutes, running.schedule.interval_minutes
        ));
    }
    if config.publish.enabled != running.publish.enabled {
        out.push(format!(
            "publish.enabled: file {} != running {}",
            config.publish.enabled, running.publish.enabled
        ));
    }
    if config.publish.keep_snapshots != running.publish.keep_snapshots {
        out.push(format!(
            "publish.keep_snapshots: file {:?} != running {:?}",
            config.publish.keep_snapshots, running.publish.keep_snapshots
        ));
    }
    if config.publish.dir != running.publish.dir {
        out.push(format!(
            "publish.dir: file {:?} != running {:?}",
            config.publish.dir, running.publish.dir
        ));
    }
    if config.sources.len() != running.sources.len() {
        out.push(format!(
            "sources: file {} != running {}",
            config.sources.len(),
            running.sources.len()
        ));
    }
    if config.verify.chain_payload.enabled != running.round.verify.chain_payload.enabled
        || config.verify.chain_payload.scope != running.round.verify.chain_payload.scope
    {
        out.push(format!(
            "verify.chain_payload: file {:?}/{} != running {:?}/{}",
            config.verify.chain_payload.enabled,
            if config.verify.chain_payload.scope == probe_config::PayloadScope::All {
                "all"
            } else {
                "chain"
            },
            running.round.verify.chain_payload.enabled,
            if running.round.verify.chain_payload.scope == probe_config::PayloadScope::All {
                "all"
            } else {
                "chain"
            }
        ));
    }
    if config.core.lanes != running.core.lanes || config.core.base_port != running.core.base_port {
        out.push(format!(
            "core: file {} lanes/{} != running {}/{}",
            config.core.lanes, config.core.base_port, running.core.lanes, running.core.base_port
        ));
    }
    if config.chain.enabled != running.chain.enabled {
        out.push(format!(
            "chain.enabled: file {} != running {}",
            config.chain.enabled, running.chain.enabled
        ));
    }
    out
}

async fn validate(State(state): State<SharedState>) -> Response {
    let path = state.config_path.clone();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": {"code": "config_unreadable",
                    "message": format!("cannot read {}: {}", path.display(), err.kind())}})),
            )
                .into_response();
        }
    };
    let (config, json_valid) = match probe_config::Config::parse(&text) {
        Ok(parsed) => parsed,
        Err(err) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": {"code": "invalid_config",
                    "message": err.to_string()}})),
            )
                .into_response();
        }
    };
    let differences = differences(&config, &state);
    (
        StatusCode::OK,
        Json(json!({
            "path": path.display().to_string(),
            "json_valid": json_valid,
            "matches_running": differences.is_empty(),
            "differences": differences,
            "summary": summary(&config),
        })),
    )
        .into_response()
}
