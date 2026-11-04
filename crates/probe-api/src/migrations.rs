//! The admin migration surface (workstreams/18 §B5): the frozen B5 core
//! (`probe_source::migrate`) wired to HTTP, following the same
//! admin-guarded POST pattern as `publishers.rs`.
//!
//! Three endpoints, all inside the guarded group:
//!
//! * `POST /api/v1/migrations/substore/scan` -- the read-only duplicate
//!   scan. It takes an optional `{prefix}` body and never writes to the
//!   backend by construction (`scan` only GETs).
//! * `POST /api/v1/migrations/substore/apply` -- retarget group canonicals
//!   onto the stable artifact URL and quarantine duplicates. The body is
//!   `{plan, dry_run}` and **`dry_run` defaults to true when absent**: the
//!   core's `apply()` takes the flag as a bare parameter, so the safe
//!   default has to live in this layer. A request that omits it can only
//!   ever preview.
//! * `POST /api/v1/migrations/substore/rollback` -- restore from a
//!   client-supplied `ApplyReport` (the exact document a previous apply
//!   returned). Per the core's documented limitation, URLs in a serialized
//!   report are redacted, so a rollback from JSON restores the redacted
//!   form; the in-memory report remains the exact-restore path.
//!
//! The stable URL a canonical is retargeted to is the same route
//! `sources.rs` serves, `…/artifacts/{source_id}/subscription` under the
//! configured publish hostname; when a plan group carries no saved
//! `source_id`, the key→source_id mapping from the ledger resolves it, and
//! the bare key is the last-resort fallback.
//!
//! Responses carry no credentials: plans and reports serialize redacted
//! URLs (`?<redacted>`), and the error bodies reuse the core's bounded,
//! backend-free messages under the standard error envelope.

use std::collections::HashMap;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use probe_source::migrate::{
    apply as core_apply, rollback as core_rollback, scan as core_scan, ApplyReport, ArtifactRef,
    MigrationError, MigrationPlan, RollbackReport, ScanCaps, SubStoreHttpStore,
};
use serde::Deserialize;

use crate::exports::{bad_request, error_response, internal};
use crate::gui::AUDIT_LEVEL;
use crate::{AppState, SharedState};

/// The admin routes this module adds to the guarded group.
pub(crate) fn admin_routes() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/migrations/substore/scan", post(scan_substore))
        .route("/api/v1/migrations/substore/apply", post(apply_substore))
        .route(
            "/api/v1/migrations/substore/rollback",
            post(rollback_substore),
        )
}

#[derive(Debug, Default, Deserialize)]
struct ScanBody {
    prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApplyBody {
    plan: MigrationPlan,
    /// Absent means dry run. `Option` (not `#[serde(default)]`) so an
    /// explicit `null` is also read as the safe default instead of failing
    /// the request.
    dry_run: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RollbackBody {
    report: ApplyReport,
}

/// `POST /api/v1/migrations/substore/scan` -- the read-only duplicate scan.
async fn scan_substore(State(state): State<SharedState>, body: Option<Json<ScanBody>>) -> Response {
    let prefix = body
        .and_then(|Json(body)| body.prefix)
        .map(|prefix| prefix.trim().to_string())
        .filter(|prefix| !prefix.is_empty())
        .unwrap_or_else(default_prefix(&state));
    let key_to_source = match source_id_map(&state) {
        Ok(map) => map,
        Err(err) => return internal(err),
    };
    // No config section for the caps exists yet, so the core's defaults
    // (50 fetches, a 24h quarantine window) stand; the plan records them.
    let store = SubStoreHttpStore::new(&state.backend);
    match core_scan(&store, &prefix, Some(&key_to_source), ScanCaps::default()).await {
        Ok(plan) => Json(plan).into_response(),
        Err(err) => migration_error(err),
    }
}

/// `POST /api/v1/migrations/substore/apply` -- retarget and quarantine from
/// a client-supplied plan. Dry run unless the body says otherwise.
async fn apply_substore(
    State(state): State<SharedState>,
    request: Result<Json<ApplyBody>, JsonRejection>,
) -> Response {
    let Json(body) = match request {
        Ok(json) => json,
        Err(rejection) => {
            return bad_request("invalid_body", format!("invalid apply body: {rejection}"))
        }
    };
    // THE default: a body that stays silent on dry_run can only preview.
    let dry_run = body.dry_run.unwrap_or(true);
    let key_to_source = match source_id_map(&state) {
        Ok(map) => map,
        Err(err) => return internal(err),
    };
    let hostname = state.publish.hostname.trim().to_string();
    let store = SubStoreHttpStore::new(&state.backend);
    let report = core_apply(
        &body.plan,
        &store,
        |artifact| stable_artifact_url(&hostname, artifact, &key_to_source),
        dry_run,
    )
    .await;
    if !dry_run {
        // One audit line per real apply, through the same writer gui.rs
        // uses; a dry run writes nothing and says nothing.
        record_migration_audit(
            &state,
            "migrations.apply",
            &format!(
                "dry_run=false applied={} failed={} skipped={}",
                report.counts.applied, report.counts.failed, report.counts.skipped
            ),
        );
    }
    Json(report).into_response()
}

/// `POST /api/v1/migrations/substore/rollback` -- restore from a prior
/// apply's report (the exact document that apply returned).
async fn rollback_substore(
    State(state): State<SharedState>,
    request: Result<Json<RollbackBody>, JsonRejection>,
) -> Response {
    let Json(body) = match request {
        Ok(json) => json,
        Err(rejection) => {
            return bad_request(
                "invalid_body",
                format!("invalid rollback body: {rejection}"),
            )
        }
    };
    let store = SubStoreHttpStore::new(&state.backend);
    let report: RollbackReport = core_rollback(&body.report, &store).await;
    Json(report).into_response()
}

/// The scan prefix when the caller is silent: the configured publish prefix
/// plus the dash every sub name is built with (`<prefix>-<key>`), falling
/// back to the core's own default when the config carries an empty prefix.
fn default_prefix(state: &AppState) -> impl Fn() -> String + '_ {
    move || {
        if state.publish_prefix.is_empty() {
            probe_source::migrate::DEFAULT_PREFIX.to_string()
        } else {
            format!("{}-", state.publish_prefix)
        }
    }
}

/// The ledger's key→source_id registry. The engine syncs it at serve time
/// (workstreams/18), so a configured key resolves even before the first
/// round. The error is a message, not a built `Response`: the envelope type
/// is too large for a `Result` error variant (clippy `result_large_err`).
fn source_id_map(state: &AppState) -> Result<HashMap<String, String>, String> {
    let rows = match state.storage.lock() {
        Ok(storage) => storage.list_sources(),
        Err(_) => return Err("ledger lock poisoned".to_string()),
    };
    match rows {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|row| (row.key, row.source_id))
            .collect()),
        Err(err) => Err(err.to_string()),
    }
}

/// The stable artifact URL a canonical is retargeted to: the route
/// `sources.rs` serves (`/artifacts/{source_id}/subscription`) under the
/// publish hostname -- the same host the legacy export URLs were built on.
/// A group without a saved source_id resolves through the ledger's
/// key→source_id mapping; the bare key is the documented last resort.
///
/// Public because `probe-cli migrate` must build the exact same URLs as the
/// API endpoint when it applies a plan.
pub fn stable_artifact_url(
    hostname: &str,
    artifact: &ArtifactRef,
    key_to_source: &HashMap<String, String>,
) -> String {
    let source_id = artifact
        .source_id
        .clone()
        .or_else(|| key_to_source.get(&artifact.key).cloned())
        .unwrap_or_else(|| artifact.key.clone());
    format!("https://{hostname}/artifacts/{source_id}/subscription")
}

/// Map the core's typed errors onto the standard envelope. `Unsupported` is
/// a bad request (an empty prefix, a rollback with nothing to restore);
/// every backend-facing failure is a bad gateway whose message the core
/// already bounded and stripped of the backend URL (the URL is itself the
/// credential).
fn migration_error(err: MigrationError) -> Response {
    match err {
        MigrationError::Unsupported(message) => bad_request("unsupported", message),
        other => error_response(StatusCode::BAD_GATEWAY, "backend", other.to_string()),
    }
}

/// Append the migration's audit row through the same public writer gui.rs
/// uses (`Storage::record_event`, level [`AUDIT_LEVEL`]). Best effort: an
/// audit failure is logged and never fails the apply it describes.
fn record_migration_audit(state: &AppState, action: &str, detail: &str) {
    let message = format!("{action} {detail}");
    let outcome = match state.storage.lock() {
        Ok(storage) => storage.record_event(AUDIT_LEVEL, &message),
        Err(poisoned) => poisoned.into_inner().record_event(AUDIT_LEVEL, &message),
    };
    if let Err(err) = outcome {
        tracing::warn!(%err, "migration audit row not recorded");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path as AxumPath, State as AxumState};
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::{Json, Router};
    use probe_config::SourceSpec;
    use probe_engine::RoundSettings;
    use probe_mihomo::Controller;
    use probe_storage::Storage;
    use serde_json::{json, Value};

    use super::*;
    use crate::{build_router, AppState, ServeConfig};

    const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    /// A publish-prefix sub's URL with a token in it: the plan must show it
    /// only redacted.
    const TOKENED_URL: &str = "https://up.example/s?token=tok123";
    const TOKEN: &str = "tok123";

    /// The Clash body the stub's download route serves; parsed by the core's
    /// digest tier.
    const CLASH: &str = "proxies:\n  - {name: n1, type: ss, server: 1.1.1.1, port: 443}\n";

    /// Loopback state whose Sub-Store backend is `backend`. `sources` feeds
    /// the ledger sync, so a test can pin the key→source_id resolution.
    fn state(root: &std::path::Path, backend: &str, sources: Vec<SourceSpec>) -> SharedState {
        let cfg = probe_config::Config::load(std::path::Path::new("/nonexistent/config.json"))
            .expect("default config");
        Arc::new(AppState::new(
            Storage::open_in_memory().unwrap(),
            Some(ADMIN.into()),
            Controller::new("http://127.0.0.1:1", None),
            ServeConfig {
                kernel_config_path: "/test/config.yaml".into(),
                round: RoundSettings::from_config(&cfg),
                core: cfg.core.clone(),
                sources,
                backend: backend.to_string(),
                root: root.to_path_buf(),
                substore: None,
                chain: cfg.chain.clone(),
                publish_prefix: cfg.publish_prefix.clone(),
                publish: cfg.publish.clone(),
                schedule: cfg.schedule.clone(),
                policy: cfg.policy.clone(),
                min_run_interval_seconds: 0,
            },
        ))
    }

    async fn spawn(state: SharedState) -> String {
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    /// An in-memory Sub-Store stub: two same-URL publish-prefix subs (a
    /// tier-2 duplicate pair), one unprefixed bystander. Counts PATCHes so a
    /// test can prove zero writes. `patch_ok=false` answers 500 on writes,
    /// for the failing-apply path.
    struct StubSubs {
        records: Mutex<BTreeMap<String, Value>>,
        patches: AtomicUsize,
        patch_ok: bool,
    }

    impl StubSubs {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                records: Mutex::new(BTreeMap::from([
                    (
                        "probe-air".to_string(),
                        json!({"name": "probe-air", "url": TOKENED_URL, "source": "remote"}),
                    ),
                    (
                        "probe-air-dup".to_string(),
                        json!({"name": "probe-air-dup", "url": TOKENED_URL, "source": "remote"}),
                    ),
                    (
                        "other-sub".to_string(),
                        json!({"name": "other-sub", "url": "https://up.example/other"}),
                    ),
                ])),
                patches: AtomicUsize::new(0),
                patch_ok: true,
            })
        }

        fn patch_count(&self) -> usize {
            self.patches.load(Ordering::SeqCst)
        }
    }

    async fn spawn_stub(stub: Arc<StubSubs>) -> String {
        let list = AxumState(stub.clone());
        let record_get = AxumState(stub.clone());
        let record_patch = AxumState(stub.clone());
        let app = Router::new()
            .route(
                "/api/subs",
                get(move || async move {
                    let records = list.records.lock().unwrap();
                    let subs: Vec<Value> = records
                        .values()
                        .map(|record| {
                            json!({
                                "name": record["name"],
                                "url": record["url"],
                                "source": "remote",
                            })
                        })
                        .collect();
                    Json(subs)
                }),
            )
            .route(
                "/api/sub/{name}",
                get(move |AxumPath(name): AxumPath<String>| async move {
                    let records = record_get.records.lock().unwrap();
                    match records.get(&name) {
                        Some(record) => Json(record.clone()).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                })
                .patch(
                    move |AxumPath(name): AxumPath<String>, Json(payload): Json<Value>| async move {
                        if !record_patch.patch_ok {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"message": "stub refuses writes"})),
                            )
                                .into_response();
                        }
                        let mut records = record_patch.records.lock().unwrap();
                        if !records.contains_key(&name) {
                            return StatusCode::NOT_FOUND.into_response();
                        }
                        records.insert(name, payload);
                        record_patch.patches.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"ok": true})).into_response()
                    },
                ),
            )
            .route(
                "/download/{name}",
                get(move |AxumPath(name): AxumPath<String>| async move {
                    if name.starts_with("probe-") {
                        (StatusCode::OK, CLASH.to_string()).into_response()
                    } else {
                        StatusCode::NOT_FOUND.into_response()
                    }
                }),
            )
            .with_state(());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    async fn scan_plan(client: &reqwest::Client, api: &str) -> Value {
        let response = client
            .post(format!("{api}/api/v1/migrations/substore/scan"))
            .header("X-Auth-Token", ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "scan must answer a plan");
        response.json().await.unwrap()
    }

    /// The migration endpoints live in the guarded group: without the admin
    /// token they are the same 401 every guarded endpoint answers with.
    #[tokio::test]
    async fn migration_endpoints_require_the_admin_token() {
        let tmp = tempfile::tempdir().unwrap();
        let base = spawn(state(tmp.path(), "http://127.0.0.1:1", Vec::new())).await;
        let client = reqwest::Client::new();
        for path in [
            "/api/v1/migrations/substore/scan",
            "/api/v1/migrations/substore/apply",
            "/api/v1/migrations/substore/rollback",
        ] {
            let response = client.post(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(response.status(), 401, "{path} is guarded");
        }
    }

    /// An unreachable backend maps to the error envelope: a bad gateway with
    /// the core's bounded, backend-free message -- never a success-shaped
    /// body, and never the backend URL (it is itself the credential).
    #[tokio::test]
    async fn an_unreachable_backend_answers_the_error_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let base = spawn(state(tmp.path(), "http://127.0.0.1:1", Vec::new())).await;
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/api/v1/migrations/substore/scan"))
            .header("X-Auth-Token", ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "backend", "{body}");
        assert!(
            !body.to_string().contains("127.0.0.1:1"),
            "the backend URL must not leak: {body}"
        );
    }

    /// A scan against the stub answers the versioned plan: schema 1, one
    /// tier-2 group (same URL) with its duplicate, honest counts, and the
    /// tokenized upstream URL shown only redacted.
    #[tokio::test]
    async fn scan_against_a_stub_answers_the_plan_shape() {
        let stub = StubSubs::new();
        let backend = spawn_stub(stub.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let base = spawn(state(tmp.path(), &backend, Vec::new())).await;
        let client = reqwest::Client::new();
        let plan = scan_plan(&client, &base).await;

        assert_eq!(plan["schema"], 1, "{plan}");
        assert_eq!(plan["prefix"], "probe-", "{plan}");
        assert_eq!(plan["counts"]["keep"], 1, "{plan}");
        assert_eq!(plan["counts"]["duplicate"], 1, "{plan}");
        assert_eq!(plan["counts"]["untouched"], 1, "{plan}");
        let groups = plan["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{plan}");
        assert_eq!(groups[0]["canonical"]["name"], "probe-air", "{plan}");
        assert_eq!(groups[0]["canonical"]["class"], "keep", "{plan}");
        assert_eq!(
            groups[0]["duplicates"][0]["name"], "probe-air-dup",
            "{plan}"
        );
        assert_eq!(groups[0]["duplicates"][0]["class"], "duplicate", "{plan}");
        // The tokenized upstream URL is redacted everywhere in the document.
        let text = plan.to_string();
        assert!(!text.contains(TOKEN), "no token in the plan: {text}");
        assert!(text.contains("https://up.example/s?<redacted>"), "{text}");
        // The bystander is reported, never a candidate.
        let untouched = plan["untouched"].as_array().unwrap();
        assert_eq!(untouched[0]["name"], "other-sub", "{plan}");
        // Exactly one content fetch: the dup joined on the URL tier.
        assert_eq!(plan["fetches_used"], 1, "{plan}");
        assert_eq!(stub.patch_count(), 0, "a scan never writes");
    }

    /// An apply whose body omits `dry_run` is a dry run: the report says so
    /// and the stub observed zero write requests.
    #[tokio::test]
    async fn apply_without_dry_run_is_a_dry_run_with_zero_writes() {
        let stub = StubSubs::new();
        let backend = spawn_stub(stub.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let base = spawn(state(tmp.path(), &backend, Vec::new())).await;
        let client = reqwest::Client::new();
        let plan = scan_plan(&client, &base).await;

        let response = client
            .post(format!("{base}/api/v1/migrations/substore/apply"))
            .header("X-Auth-Token", ADMIN)
            .json(&json!({"plan": plan}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let report: Value = response.json().await.unwrap();
        assert_eq!(report["dry_run"], true, "{report}");
        assert!(
            report["counts"]["would_do"].as_u64().unwrap() >= 1,
            "{report}"
        );
        assert_eq!(report["counts"]["applied"], 0, "{report}");
        assert_eq!(stub.patch_count(), 0, "a dry run never writes");
        // The preview names the stable URL the real apply would use.
        let text = report.to_string();
        assert!(
            text.contains("/artifacts/air/subscription"),
            "the preview carries the stable URL: {text}"
        );
    }

    /// `dry_run=false` really applies: the canonical is retargeted onto the
    /// stable artifact URL on the stub (one PATCH), the duplicate is left
    /// for the rename-unsupported skip, and one `security` audit row lands.
    #[tokio::test]
    async fn apply_with_dry_run_false_retargets_on_the_stub() {
        let stub = StubSubs::new();
        let backend = spawn_stub(stub.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let app_state = state(tmp.path(), &backend, Vec::new());
        let base = spawn(app_state.clone()).await;
        let client = reqwest::Client::new();
        let plan = scan_plan(&client, &base).await;

        let response = client
            .post(format!("{base}/api/v1/migrations/substore/apply"))
            .header("X-Auth-Token", ADMIN)
            .json(&json!({"plan": plan, "dry_run": false}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let report: Value = response.json().await.unwrap();
        assert_eq!(report["dry_run"], false, "{report}");
        assert_eq!(report["counts"]["applied"], 1, "{report}");
        // The canonical now points at the stable artifact URL (key fallback:
        // the ledger has no `air` source here, so the key is the id).
        let records = stub.records.lock().unwrap();
        assert_eq!(
            records["probe-air"]["url"], "https://probe.example.com/artifacts/air/subscription",
            "{records:?}"
        );
        drop(records);
        assert_eq!(stub.patch_count(), 1, "exactly the retarget PATCH");
        // One audit line for the real apply.
        let events = app_state.storage.lock().unwrap().recent_events(10).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.message.contains("migrations.apply dry_run=false")),
            "the real apply is audited: {events:?}"
        );
    }

    /// A configured source resolves the group's key to its ledger
    /// source_id, so the stable URL addresses the permanent artifact route
    /// rather than the (renamable) key.
    #[tokio::test]
    async fn apply_uses_the_ledger_source_id_for_the_stable_url() {
        let stub = StubSubs::new();
        let backend = spawn_stub(stub.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let app_state = state(
            tmp.path(),
            &backend,
            vec![SourceSpec {
                key: "air".into(),
                kind: "collection".into(),
                name: "air".into(),
                label: "air".into(),
                enabled: true,
                relay: false,
                direct: true,
                chain: false,
                export: true,
            }],
        );
        let base = spawn(app_state.clone()).await;
        let source_id = app_state
            .storage
            .lock()
            .unwrap()
            .list_sources()
            .unwrap()
            .remove(0)
            .source_id;
        let client = reqwest::Client::new();
        let plan = scan_plan(&client, &base).await;

        let response = client
            .post(format!("{base}/api/v1/migrations/substore/apply"))
            .header("X-Auth-Token", ADMIN)
            .json(&json!({"plan": plan, "dry_run": false}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let records = stub.records.lock().unwrap();
        assert_eq!(
            records["probe-air"]["url"],
            format!("https://probe.example.com/artifacts/{source_id}/subscription"),
            "{records:?}"
        );
    }

    /// A rollback from the returned report restores the canonical's URL and
    /// is idempotent: the second run finds the original already in place.
    /// Per the core's documented limitation, the URL in a serialized report
    /// is redacted, so that redacted form is what a JSON round-trip restores.
    #[tokio::test]
    async fn rollback_from_the_report_restores_and_is_idempotent() {
        let stub = StubSubs::new();
        let backend = spawn_stub(stub.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let base = spawn(state(tmp.path(), &backend, Vec::new())).await;
        let client = reqwest::Client::new();
        let plan = scan_plan(&client, &base).await;

        let response = client
            .post(format!("{base}/api/v1/migrations/substore/apply"))
            .header("X-Auth-Token", ADMIN)
            .json(&json!({"plan": plan, "dry_run": false}))
            .send()
            .await
            .unwrap();
        let report: Value = response.json().await.unwrap();

        let response = client
            .post(format!("{base}/api/v1/migrations/substore/rollback"))
            .header("X-Auth-Token", ADMIN)
            .json(&json!({"report": report}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let rollback: Value = response.json().await.unwrap();
        assert_eq!(rollback["schema"], 1, "{rollback}");
        assert_eq!(rollback["counts"]["restored"], 1, "{rollback}");
        // Scoped so the guard cannot reach an await point.
        let restored_to = {
            let records = stub.records.lock().unwrap();
            records["probe-air"]["url"].clone()
        };
        assert_eq!(
            restored_to, "https://up.example/s?<redacted>",
            "the redacted original is what a JSON report restores"
        );

        // Idempotent: restoring again writes nothing.
        let response = client
            .post(format!("{base}/api/v1/migrations/substore/rollback"))
            .header("X-Auth-Token", ADMIN)
            .json(&json!({"report": report}))
            .send()
            .await
            .unwrap();
        let again: Value = response.json().await.unwrap();
        assert_eq!(again["counts"]["already_restored"], 1, "{again}");
        assert_eq!(stub.patch_count(), 2, "one retarget + one restore PATCH");
    }
}
