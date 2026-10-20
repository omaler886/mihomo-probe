//! Local HTTP integration: Sub-Store -> DoH -> controller -> endpoint ledger.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{json, Value};

use super::{AppState, ServeConfig, SharedState};
use probe_engine::RoundSettings;
use probe_mihomo::Controller;
use probe_storage::{RoundGuardSummary, Storage};

const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[derive(Default)]
struct MockState {
    dns_queries: AtomicUsize,
    delays: Mutex<Vec<String>>,
    /// Names the push mode created through `POST /api/subs`.
    upserts: Mutex<Vec<String>>,
}

/// Reply with matching DNS metadata and two A endpoints; AAAA is known NODATA.
async fn dns_reply(
    State(state): State<Arc<MockState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    state.dns_queries.fetch_add(1, Ordering::SeqCst);
    let Some(encoded) = query.get("dns") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(request) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let name = probe_dns::wire::encode_name("origin.example").unwrap();
    let question_end = 12 + name.len() + 4;
    if request.len() < question_end {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let qtype = u16::from_be_bytes([request[question_end - 4], request[question_end - 3]]);
    let mut response = request[..question_end].to_vec();
    response[2..4].copy_from_slice(&0x8180_u16.to_be_bytes());
    response[6..8].copy_from_slice(&(if qtype == 1 { 2_u16 } else { 0 }).to_be_bytes());
    response[8..12].fill(0);
    if qtype == 1 {
        for endpoint in [[192, 0, 2, 1], [192, 0, 2, 2]] {
            response.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1]);
            response.extend_from_slice(&60_u32.to_be_bytes());
            response.extend_from_slice(&4_u16.to_be_bytes());
            response.extend_from_slice(&endpoint);
        }
    }
    (
        [(header::CONTENT_TYPE, "application/dns-message")],
        response,
    )
        .into_response()
}

/// Record which temporary alias was actually measured through the controller API.
async fn delay_reply(
    State(state): State<Arc<MockState>>,
    AxumPath(name): AxumPath<String>,
) -> Json<Value> {
    state.delays.lock().unwrap().push(name);
    Json(json!({"delay": 42}))
}

/// Record one Sub-Store upsert (push mode) and answer like the real backend.
async fn upsert_sub(
    State(state): State<Arc<MockState>>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    if let Some(name) = payload.get("name").and_then(Value::as_str) {
        state.upserts.lock().unwrap().push(name.to_string());
    }
    Json(json!({"ok": true}))
}

/// List the subs this mock holds; the push mode's prune reads it.
async fn list_subs(State(state): State<Arc<MockState>>) -> Json<Value> {
    let names = state.upserts.lock().unwrap().clone();
    Json(Value::Array(
        names
            .into_iter()
            .map(|name| json!({"name": name, "source": "local"}))
            .collect(),
    ))
}

/// Serve all external contracts on one isolated loopback listener.
async fn mock_services() -> (String, Arc<MockState>) {
    mock_services_with(std::time::Duration::ZERO).await
}

/// The same contracts, with kernel preparation (`PUT /configs`) held open so a
/// test can observe a round while it is genuinely in flight.
async fn mock_services_with(kernel_delay: std::time::Duration) -> (String, Arc<MockState>) {
    let state = Arc::new(MockState::default());
    let app = Router::new()
        .route("/download/collection/{name}", get(|| async {
            "proxies:\n  - name: upstream-node\n    type: trojan\n    server: origin.example\n    port: 443\n    password: dummy-password\n"
        }))
        .route("/dns-query", get(dns_reply))
        .route("/version", get(|| async { Json(json!({"version": "test-kernel"})) }))
        .route("/configs", put(move || async move {
            tokio::time::sleep(kernel_delay).await;
            StatusCode::NO_CONTENT
        }))
        .route("/proxies/{name}/delay", get(delay_reply))
        // Sub-Store's admin half: a sub never exists yet, an upsert is recorded,
        // and listing returns what was recorded (the push mode's prune reads it).
        .route(
            "/api/sub/{name}",
            get(|| async { StatusCode::NOT_FOUND })
                .patch(|| async { Json(json!({"ok": true})) })
                .delete(|| async { Json(json!({"ok": true})) }),
        )
        .route("/api/subs", get(list_subs).post(upsert_sub))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, state)
}

/// Use a real ledger and the normal API task with every outbound URL locally controlled.
fn pipeline_state(root: &std::path::Path, services: &str) -> SharedState {
    pipeline_state_tuned(root, services, |_, _| {})
}

/// The same state with a chance to adjust the config/round settings: the
/// scheduler tests turn the timer off, shrink the orphan budget and so on.
fn pipeline_state_tuned(
    root: &std::path::Path,
    services: &str,
    tune: impl FnOnce(&mut probe_config::Config, &mut RoundSettings),
) -> SharedState {
    let mut cfg = probe_config::Config::load(&root.join("missing-config.json")).unwrap();
    let mut round = RoundSettings::from_config(&cfg);
    round.dns.views = BTreeMap::from([(
        "local".into(),
        probe_config::DnsView {
            resolver: format!("{services}/dns-query"),
            ecs: None,
            ecs_prefix: 24,
        },
    )]);
    round.verify.entry_check = false;
    round.verify.enabled = false;
    round.verify.chain_payload.enabled = false;
    tune(&mut cfg, &mut round);
    Arc::new(AppState::new(
        Storage::open_in_memory().unwrap(),
        Some(ADMIN.into()),
        Controller::new(services, None),
        ServeConfig {
            kernel_config_path: "/test/config.yaml".into(),
            round,
            core: cfg.core,
            sources: cfg.sources,
            backend: services.into(),
            root: root.into(),
            substore: None,
            chain: cfg.chain,
            publish_prefix: cfg.publish_prefix,
            publish: cfg.publish,
            schedule: cfg.schedule,
            policy: cfg.policy,
        },
    ))
}

/// Confirm actual HTTP DNS expansion feeds the controller and preserves durable metadata.
#[tokio::test]
async fn api_round_runs_the_dns_pipeline_before_persisting_endpoint_observations() {
    let root = tempfile::tempdir().unwrap();
    let (services, external) = mock_services().await;
    let state = pipeline_state(root.path(), &services);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/v1/rounds", listener.local_addr().unwrap());
    let app = super::build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = reqwest::Client::new()
        .post(endpoint)
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let response: Value = response.json().await.unwrap();
    let round_id = response["round_id"].as_i64().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state.in_flight.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("local pipeline must finish");
    assert_eq!(external.dns_queries.load(Ordering::SeqCst), 2);
    let aliases = external.delays.lock().unwrap();
    assert_eq!(aliases.len(), 2);
    assert!(aliases.iter().any(|name| name.ends_with("@192.0.2.1")));
    assert!(aliases.iter().any(|name| name.ends_with("@192.0.2.2")));
    let storage = state.storage.lock().unwrap();
    let rows = storage.results_for_round(round_id).unwrap();
    assert_eq!(rows.len(), 2);
    let node = storage
        .get_node("air", &rows[0].fingerprint)
        .unwrap()
        .unwrap();
    assert_eq!(node.status, "alive");
    assert_eq!(node.display.as_deref(), Some("upstream-node"));
    assert_eq!(node.server.as_deref(), Some("origin.example"));
    assert_eq!((node.ip_alive, node.ip_total), (Some(2), Some(2)));
    assert_eq!(storage.last_round().unwrap().unwrap().total, 1);
    assert!(rows
        .iter()
        .all(|row| row.endpoint_ip.is_some()
            && row.original_server.as_deref() == Some("origin.example")));
    let config: Value = serde_norway::from_str(
        &std::fs::read_to_string(root.path().join("core/config.yaml")).unwrap(),
    )
    .unwrap();
    let proxies = config["proxies"].as_array().unwrap();
    assert_eq!(proxies.len(), 2);
    assert!(proxies
        .iter()
        .all(|proxy| proxy["server"] != "origin.example" && proxy["sni"] == "origin.example"));
}

/// The API round path publishes what the round measured: an active export and
/// its immutable file are observable before the in-flight flag clears.
#[tokio::test]
async fn api_round_publishes_the_source_exports() {
    let root = tempfile::tempdir().unwrap();
    let (services, _external) = mock_services().await;
    let state = pipeline_state(root.path(), &services);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/v1/rounds", listener.local_addr().unwrap());
    let app = super::build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = reqwest::Client::new()
        .post(endpoint)
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state.in_flight.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("local pipeline must finish");

    // The round ended with an applied guard, so the API published the source
    // body before releasing the round: the active row and the immutable file
    // are both observable here.
    let storage = state.storage.lock().unwrap();
    let active = storage
        .current_export("air")
        .unwrap()
        .expect("an API round end must activate the source export");
    assert_eq!(active.summary.node_count, 1);
    assert!(active.content.contains("upstream-node"));
    assert_eq!(
        active.summary.sha256,
        probe_storage::publication::export_hash(&active.content)
    );
    let listed = storage.list_exports(10, None).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, "air");
    drop(storage);

    let entries = std::fs::read_dir(root.path().join("exports"))
        .unwrap()
        .count();
    assert_eq!(
        entries, 1,
        "one immutable export file, no staging leftovers"
    );
}

/// A cancel request aimed at the round that is still preparing ends it: the row
/// closes with a cancelled note, nothing is published, and the id is no longer
/// cancellable once the slot clears.
#[tokio::test]
async fn cancel_stops_the_in_flight_round_before_it_measures_anything() {
    let root = tempfile::tempdir().unwrap();
    // Kernel preparation sleeps long enough that the cancel request certainly
    // arrives while this round holds the in-flight slot.
    let (services, _external) = mock_services_with(std::time::Duration::from_secs(2)).await;
    let state = pipeline_state(root.path(), &services);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = super::build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();

    let started: Value = client
        .post(format!("{base}/api/v1/rounds"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let round_id = started["round_id"].as_i64().unwrap();
    let cancelled = client
        .post(format!("{base}/api/v1/rounds/{round_id}/cancel"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(cancelled.status(), StatusCode::OK);
    let body: Value = cancelled.json().await.unwrap();
    assert_eq!(body["round_id"].as_i64(), Some(round_id));
    assert_eq!(body["cancelling"].as_bool(), Some(true));

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while state.in_flight.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a cancelled round must still close its row");
    let row = state.storage.lock().unwrap().last_round().unwrap().unwrap();
    assert_eq!(row.round_id, round_id);
    assert!(
        row.finished_at.is_some(),
        "the cancelled round must be closed"
    );
    assert!(
        row.note.unwrap_or_default().contains("cancelled"),
        "the note must say the round was cancelled"
    );
    // It measured nothing, so the round must not publish an export.
    assert!(state
        .storage
        .lock()
        .unwrap()
        .current_export("air")
        .unwrap()
        .is_none());
    // Nothing is in flight any more: the same id is a 409, not a 200.
    let again = client
        .post(format!("{base}/api/v1/rounds/{round_id}/cancel"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::CONFLICT);
}

// --- scheduler -----------------------------------------------------------

/// A disabled timer starts nothing and reports no next run.
#[tokio::test]
async fn a_disabled_schedule_never_starts_a_round_and_shows_no_next_run() {
    let root = tempfile::tempdir().unwrap();
    let (services, _external) = mock_services().await;
    let state = pipeline_state_tuned(root.path(), &services, |cfg, _round| {
        cfg.schedule.enabled = false;
    });
    let expected_minutes = state.schedule.interval().as_secs() / 60;
    // The real loop, with a tiny tick: 120ms is several polls.
    crate::spawn_scheduler_every(state.clone(), std::time::Duration::from_millis(10));
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    assert!(
        state
            .storage
            .lock()
            .unwrap()
            .last_round()
            .unwrap()
            .is_none(),
        "a disabled timer must not open a round row"
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = super::build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let body: Value = reqwest::Client::new()
        .get(format!("{base}/api/v1/status"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["schedule"]["enabled"].as_bool(), Some(false));
    assert_eq!(
        body["schedule"]["interval_minutes"].as_u64(),
        Some(expected_minutes)
    );
    assert!(body["schedule"]["next_run"].is_null());
    assert!(body["rounds"]["last"].is_null());
}

/// A due tick opens exactly one round; the next tick sees it in flight.
#[tokio::test]
async fn a_due_tick_starts_exactly_one_round_and_a_second_tick_waits() {
    let root = tempfile::tempdir().unwrap();
    // A slow kernel keeps the first round in flight across the second tick.
    let (services, _external) = mock_services_with(std::time::Duration::from_secs(2)).await;
    let state = pipeline_state_tuned(root.path(), &services, |_, _| {});
    // A process "started" in the past puts the empty ledger past its grace.
    crate::scheduler_tick(&state, "2020-01-01T00:00:00").await;
    let opened = state
        .storage
        .lock()
        .unwrap()
        .last_round()
        .unwrap()
        .expect("a due tick must open a round row");
    assert_eq!(opened.trigger, "schedule");
    assert!(
        state.in_flight.load(Ordering::Acquire),
        "the scheduled round must still be in flight"
    );
    assert_eq!(state.next_run().map(|next| next.len()), Some(8));

    crate::scheduler_tick(&state, "2020-01-01T00:00:00").await;
    assert_eq!(
        state
            .storage
            .lock()
            .unwrap()
            .list_rounds(10, None)
            .unwrap()
            .len(),
        1,
        "one tick, one round: the busy slot must refuse the second start"
    );

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while state.in_flight.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the scheduled round must finish");
}

/// A tick reaps a round abandoned past the watchdog budget before deciding.
#[tokio::test]
async fn a_tick_reaps_a_round_abandoned_past_the_watchdog_budget() {
    let root = tempfile::tempdir().unwrap();
    let (services, _external) = mock_services().await;
    let state = pipeline_state_tuned(root.path(), &services, |_, round| {
        // A zero budget makes anything started a second ago already overdue.
        round.watchdog.round_timeout_minutes = 0;
    });
    let abandoned = {
        let storage = state.storage.lock().unwrap();
        storage.start_round("manual", None).unwrap()
    };
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    crate::scheduler_tick(&state, &probe_storage::utc_now()).await;
    let closed = state
        .storage
        .lock()
        .unwrap()
        .round_by_id(abandoned)
        .unwrap()
        .expect("the abandoned row still exists");
    assert!(
        closed.finished_at.is_some(),
        "the tick must close the abandoned round"
    );
    assert_eq!(closed.note.as_deref(), Some(probe_engine::ORPHAN_NOTE));
}

/// Status carries the last round's full counters plus the schedule block.
#[tokio::test]
async fn status_reports_last_round_counts_and_the_schedule_block() {
    let root = tempfile::tempdir().unwrap();
    let (services, _external) = mock_services().await;
    let state = pipeline_state(root.path(), &services);
    let expected_minutes = state.schedule.interval().as_secs() / 60;
    let round_id = {
        let storage = state.storage.lock().unwrap();
        let id = storage.start_round("manual", None).unwrap();
        storage
            .finish_round_full(
                id,
                Some("counts"),
                &RoundGuardSummary {
                    total: 5,
                    ok: 3,
                    failed: 2,
                    dropped: 1,
                    restored: 0,
                    suspect: false,
                    inconclusive: false,
                },
            )
            .unwrap();
        id
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = super::build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = reqwest::Client::new()
        .get(format!("{base}/api/v1/status"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(
        !text.contains("Bearer") && !text.contains("http://"),
        "{text}"
    );
    let body: Value = serde_json::from_str(&text).unwrap();
    let last = &body["rounds"]["last"];
    assert_eq!(last["round_id"].as_i64(), Some(round_id));
    assert_eq!(last["total"].as_i64(), Some(5));
    assert_eq!(last["ok"].as_i64(), Some(3));
    assert_eq!(last["failed"].as_i64(), Some(2));
    assert_eq!(last["dropped"].as_i64(), Some(1));
    assert_eq!(last["inconclusive"].as_bool(), Some(false));
    assert_eq!(last["suspect"].as_bool(), Some(false));
    assert!(last["duration_s"].is_null());
    assert_eq!(body["schedule"]["enabled"].as_bool(), Some(true));
    assert_eq!(
        body["schedule"]["interval_minutes"].as_u64(),
        Some(expected_minutes)
    );
    // No timer loop runs in this test, so next_run stays null until a tick.
    assert!(body["schedule"]["next_run"].is_null());
}

/// Push mode on: the round end upserts the published export into Sub-Store,
/// and the in-flight flag only clears after the push finished.
#[tokio::test]
async fn push_mode_upserts_the_published_export_at_round_end() {
    let root = tempfile::tempdir().unwrap();
    let (services, external) = mock_services().await;
    let state = pipeline_state_tuned(root.path(), &services, |cfg, _round| {
        cfg.publish.push_to_substore = true;
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = super::build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = reqwest::Client::new()
        .post(format!("{base}/api/v1/rounds"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while state.in_flight.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the round (including its push) must finish");

    // The push runs at round end, before the in-flight flag clears, so the
    // upsert is already recorded here.
    let upserts = external.upserts.lock().unwrap().clone();
    assert_eq!(
        upserts.len(),
        1,
        "one published source, one upsert: {upserts:?}"
    );
    assert!(
        upserts[0].ends_with("-air-local"),
        "the push sub name must be <prefix>-<key>-local: {upserts:?}"
    );
    // The export it pushed is the active one.
    let storage = state.storage.lock().unwrap();
    let active = storage
        .current_export("air")
        .unwrap()
        .expect("the round must have published the export");
    assert_eq!(active.summary.node_count, 1);
}
