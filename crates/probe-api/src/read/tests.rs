//! HTTP tests for the v1 read/control surface: the node and round readers
//! (whitelist, keyset cursors, 404s) and round cancel.

use std::sync::Arc;

use axum::http::StatusCode;
use probe_config::PublishSection;
use probe_engine::{RoundCtx, RoundSettings};
use probe_mihomo::Controller;
use probe_storage::{ConvergeNode, RoundGuardSummary, Storage};
use serde_json::Value;

use super::MAX_NOTE_CHARS;
use crate::{AppState, RunningRoundGuard, ServeConfig, SharedState};

const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PUBLISH: &str = "pppppppppppppppppppppppppppppppp";
const ROUND: i64 = 7;

/// The exact node DTO contract; anything outside it is a leak.
const NODE_FIELDS: [&str; 16] = [
    "source",
    "fingerprint",
    "display",
    "proto",
    "server",
    "country",
    "status",
    "consec_fail",
    "total_ok",
    "total_fail",
    "last_delay_ms",
    "last_reason",
    "ip_alive",
    "ip_total",
    "exit_ip",
    "exit_asn",
];

/// The exact round DTO contract.
const ROUND_FIELDS: [&str; 13] = [
    "round_id",
    "started_at",
    "finished_at",
    "trigger",
    "total",
    "ok",
    "failed",
    "dropped",
    "restored",
    "suspect",
    "inconclusive",
    "duration_s",
    "note",
];

fn test_state() -> SharedState {
    let cfg = probe_config::Config::load(std::path::Path::new("/nonexistent/config.json"))
        .expect("default config");
    Arc::new(
        AppState::new(
            Storage::open_in_memory().unwrap(),
            Some(ADMIN.into()),
            Controller::new("http://127.0.0.1:1", None),
            ServeConfig {
                kernel_config_path: "/test/config.yaml".into(),
                round: RoundSettings::from_config(&cfg),
                core: cfg.core,
                sources: cfg.sources,
                backend: "http://127.0.0.1:1".into(),
                root: std::path::PathBuf::from("/test/root"),
                substore: None,
                chain: cfg.chain,
                publish_prefix: cfg.publish_prefix,
                publish: cfg.publish,
                schedule: cfg.schedule,
                policy: cfg.policy,
            },
        )
        .with_publish(PublishSection {
            token: Some(PUBLISH.into()),
            ..PublishSection::default()
        }),
    )
}

async fn spawn(state: SharedState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = crate::build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

async fn get(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    query: &[(&str, &str)],
    token: Option<&str>,
) -> reqwest::Response {
    let mut request = client.get(format!("{base}{path}")).query(query);
    if let Some(token) = token {
        request = request.header("X-Auth-Token", token);
    }
    request.send().await.unwrap()
}

fn error_code(body: &Value) -> Option<&str> {
    body["error"]["code"].as_str()
}

fn keys_of(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

// --- ledger fixtures -----------------------------------------------------

fn policy() -> probe_domain::Policy {
    probe_config::PolicySection::default().into()
}

fn live<'a>(source: &'a str, fingerprint: &'a str, delay: i64) -> ConvergeNode<'a> {
    ConvergeNode {
        source,
        fingerprint,
        display: "node-name",
        category: Some("direct"),
        ok: true,
        delay_ms: Some(delay),
        reason: None,
        is_excluded: false,
        proto: Some("trojan"),
        server: Some("origin.example"),
        ip_alive: Some(2),
        ip_total: Some(2),
        country: Some("US"),
        exit_ip: Some("192.0.2.9"),
        exit_asn: Some("AS64500"),
    }
}

fn dead<'a>(source: &'a str, fingerprint: &'a str) -> ConvergeNode<'a> {
    ConvergeNode {
        ok: false,
        delay_ms: None,
        reason: Some("timeout"),
        ..live(source, fingerprint, 0)
    }
}

fn summary(total: i64, ok: i64) -> RoundGuardSummary {
    RoundGuardSummary {
        total,
        ok,
        failed: total - ok,
        dropped: 0,
        restored: 0,
        suspect: false,
        inconclusive: false,
    }
}

/// Two finished rounds. Nodes, in (source, fingerprint) order:
/// air/a1 (alive then failing), air/a2 (first seen alive in round two),
/// mobile/m1 (alive in round one only).
fn seed(state: &SharedState) -> (i64, i64) {
    let storage = state.storage.lock().unwrap();
    let first = storage.start_round("manual", None).unwrap();
    storage
        .converge_nodes(
            first,
            &[live("air", "a1", 42), live("mobile", "m1", 43)],
            &policy(),
        )
        .unwrap();
    storage
        .finish_round_full(first, Some("first round"), &summary(2, 2))
        .unwrap();
    let second = storage.start_round("schedule", Some("chain")).unwrap();
    storage
        .converge_nodes(
            second,
            &[dead("air", "a1"), live("air", "a2", 55)],
            &policy(),
        )
        .unwrap();
    storage
        .finish_round_full(second, Some("second round"), &summary(2, 1))
        .unwrap();
    (first, second)
}

// --- read surface --------------------------------------------------------

/// Build one stored result row for a node.
fn result(
    source: &str,
    fingerprint: &str,
    verdict: &str,
    delay_ms: Option<i64>,
    exit_ip: Option<&str>,
) -> probe_storage::ResultRow {
    probe_storage::ResultRow {
        source: source.into(),
        fingerprint: fingerprint.into(),
        verdict: Some(verdict.into()),
        delay_ms,
        exit_ip: exit_ip.map(str::to_string),
        ..Default::default()
    }
}

/// The event log is listed newest-first and refuses a bad limit.
#[tokio::test]
async fn events_are_listed_newest_first() {
    let state = test_state();
    {
        let storage = state.storage.lock().unwrap();
        storage.record_event("info", "round 1 started").unwrap();
        storage.record_event("warn", "source saki failed").unwrap();
    }
    let base = spawn(state).await;
    let client = reqwest::Client::new();
    let response = get(&client, &base, "/api/v1/events", &[], Some(ADMIN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["message"].as_str(), Some("source saki failed"));
    assert_eq!(events[0]["level"].as_str(), Some("warn"));
    assert_eq!(events[1]["message"].as_str(), Some("round 1 started"));
    // Outside 1..=MAX is a 400, never a silent clamp.
    let bad = get(
        &client,
        &base,
        "/api/v1/events",
        &[("limit", "0")],
        Some(ADMIN),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}

/// Measured throughput lands in the same quality block the panel reads.
#[tokio::test]
async fn node_detail_reports_measured_throughput() {
    let state = test_state();
    let (first, _second) = seed(&state);
    {
        let storage = state.storage.lock().unwrap();
        storage
            .record_quality_samples(
                first,
                &[probe_storage::QualitySampleRow {
                    source: "air".into(),
                    fingerprint: "a1".into(),
                    ts: String::new(),
                    // 1 MB in 100 ms is 80 decimal megabits per second.
                    bytes: 1_000_000,
                    elapsed_ms: 100.0,
                }],
            )
            .unwrap();
    }
    let base = spawn(state).await;
    let response = get(
        &reqwest::Client::new(),
        &base,
        "/api/v1/nodes/a1",
        &[],
        Some(ADMIN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let quality = &body["quality"];
    assert_eq!(quality["transfers"].as_u64(), Some(1));
    assert_eq!(
        quality["throughput_mbps"].as_f64(),
        Some(80.0),
        "1 MB / 100 ms is 80 Mbps"
    );
}

/// Node detail carries a quality summary over the node's newest observations,
/// computed oldest-first so jitter follows real time.
#[tokio::test]
async fn node_detail_summarizes_recent_quality() {
    let state = test_state();
    let (first, second) = seed(&state);
    {
        let storage = state.storage.lock().unwrap();
        storage
            .record_results(
                first,
                &[result("air", "a1", "ok", Some(42), Some("192.0.2.9"))],
            )
            .unwrap();
        storage
            .record_results(
                second,
                &[result("air", "a1", "ok", Some(100), Some("192.0.2.9"))],
            )
            .unwrap();
    }
    let base = spawn(state).await;
    let client = reqwest::Client::new();
    let response = get(&client, &base, "/api/v1/nodes/a1", &[], Some(ADMIN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let quality = &body["quality"];
    assert!(quality.is_object(), "a quality block is expected: {body}");
    assert_eq!(quality["window"].as_u64(), Some(2), "only this node's rows");
    assert_eq!(quality["samples"].as_u64(), Some(2));
    assert_eq!(quality["successes"].as_u64(), Some(2));
    assert_eq!(quality["success_rate"].as_f64(), Some(1.0));
    assert_eq!(quality["median_ms"].as_f64(), Some(71.0));
    assert_eq!(quality["min_ms"].as_f64(), Some(42.0));
    assert_eq!(quality["max_ms"].as_f64(), Some(100.0));
    assert_eq!(
        quality["jitter_ms"].as_f64(),
        Some(58.0),
        "jitter runs over chronological order"
    );
    assert_eq!(quality["exits"].as_u64(), Some(1));
}

/// The node list is keyset-paged, filterable, and carries exactly the
/// whitelisted metadata.
#[tokio::test]
async fn nodes_list_is_keyset_paged_and_whitelisted() {
    let state = test_state();
    let (_first, _second) = seed(&state);
    let base = spawn(state).await;
    let client = reqwest::Client::new();

    let response = get(&client, &base, "/api/v1/nodes", &[], Some(ADMIN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    for forbidden in ["password", "uuid", "secret", "category", "last_ok"] {
        assert!(
            !text.contains(forbidden),
            "node list must not expose {forbidden}: {text}"
        );
    }
    let body: Value = serde_json::from_str(&text).unwrap();
    let nodes = body["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[0]["source"].as_str(), Some("air"));
    assert_eq!(nodes[0]["fingerprint"].as_str(), Some("a1"));
    assert_eq!(nodes[1]["fingerprint"].as_str(), Some("a2"));
    assert_eq!(nodes[2]["source"].as_str(), Some("mobile"));
    assert_eq!(keys_of(&nodes[0]), {
        let mut expected = NODE_FIELDS.to_vec();
        expected.sort_unstable();
        expected
    });
    assert_eq!(nodes[0]["server"].as_str(), Some("origin.example"));
    assert_eq!(body["next_after"], Value::Null);

    // One row per page, advancing with the returned cursor.
    let first_page: Value = serde_json::from_str(
        &get(
            &client,
            &base,
            "/api/v1/nodes",
            &[("limit", "1")],
            Some(ADMIN),
        )
        .await
        .text()
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(first_page["nodes"].as_array().unwrap().len(), 1);
    let cursor = first_page["next_after"].as_str().unwrap().to_string();
    let second_page: Value = serde_json::from_str(
        &get(
            &client,
            &base,
            "/api/v1/nodes",
            &[("limit", "2"), ("after", cursor.as_str())],
            Some(ADMIN),
        )
        .await
        .text()
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(second_page["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(second_page["nodes"][0]["fingerprint"].as_str(), Some("a2"));
    assert_eq!(second_page["nodes"][1]["fingerprint"].as_str(), Some("m1"));

    // Filters run before paging.
    let filtered: Value = serde_json::from_str(
        &get(
            &client,
            &base,
            "/api/v1/nodes",
            &[("status", "alive"), ("source", "air")],
            Some(ADMIN),
        )
        .await
        .text()
        .await
        .unwrap(),
    )
    .unwrap();
    let filtered = filtered["nodes"].as_array().unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["fingerprint"].as_str(), Some("a2"));
    assert_eq!(filtered[0]["status"].as_str(), Some("alive"));
    assert_eq!(filtered[0]["exit_ip"].as_str(), Some("192.0.2.9"));

    // Bad page parameters are client errors.
    for query in [
        vec![("limit", "0")],
        vec![("limit", "101")],
        vec![("after", "not-a-cursor")],
    ] {
        let response = get(&client, &base, "/api/v1/nodes", &query, Some(ADMIN)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query:?}");
        let body: Value = response.json().await.unwrap();
        assert!(error_code(&body).is_some());
    }
}

/// Node detail returns the whitelisted row plus newest-first status moves.
#[tokio::test]
async fn node_detail_returns_recent_history_or_404() {
    let state = test_state();
    seed(&state);
    let base = spawn(state).await;
    let client = reqwest::Client::new();

    let body: Value = serde_json::from_str(
        &get(&client, &base, "/api/v1/nodes/a1", &[], Some(ADMIN))
            .await
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["node"]["fingerprint"].as_str(), Some("a1"));
    assert_eq!(body["node"]["source"].as_str(), Some("air"));
    assert_eq!(keys_of(&body["node"]), {
        let mut expected = NODE_FIELDS.to_vec();
        expected.sort_unstable();
        expected
    });
    let history = body["history"].as_array().unwrap();
    assert!(!history.is_empty(), "the seeded node moved at least once");
    assert_eq!(
        keys_of(&history[0]),
        ["from_status", "id", "reason", "round_id", "to_status", "ts"]
    );
    let ids: Vec<i64> = history
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .collect();
    let mut descending = ids.clone();
    descending.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(ids, descending, "history is newest first");

    // The history page size is bounded the same way.
    let limited: Value = serde_json::from_str(
        &get(
            &client,
            &base,
            "/api/v1/nodes/a1",
            &[("history_limit", "1")],
            Some(ADMIN),
        )
        .await
        .text()
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(limited["history"].as_array().unwrap().len(), 1);
    assert_eq!(
        get(
            &client,
            &base,
            "/api/v1/nodes/a1",
            &[("history_limit", "0")],
            Some(ADMIN)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );

    let missing = get(
        &client,
        &base,
        "/api/v1/nodes/0f0f0f0f0f0f0f0f",
        &[],
        Some(ADMIN),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let body: Value = missing.json().await.unwrap();
    assert_eq!(error_code(&body), Some("not_found"));
}

/// Rounds page newest first with an id cursor, and expose the whitelisted DTO.
#[tokio::test]
async fn rounds_list_is_id_paged_and_whitelisted() {
    let state = test_state();
    let (first, second) = seed(&state);
    let base = spawn(state).await;
    let client = reqwest::Client::new();

    let body: Value = serde_json::from_str(
        &get(&client, &base, "/api/v1/rounds", &[], Some(ADMIN))
            .await
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    let rounds = body["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 2);
    assert_eq!(rounds[0]["round_id"].as_i64(), Some(second));
    assert_eq!(rounds[1]["round_id"].as_i64(), Some(first));
    assert_eq!(keys_of(&rounds[0]), {
        let mut expected = ROUND_FIELDS.to_vec();
        expected.sort_unstable();
        expected
    });
    assert_eq!(rounds[0]["trigger"].as_str(), Some("schedule"));
    assert_eq!(rounds[0]["total"].as_i64(), Some(2));
    assert_eq!(rounds[0]["ok"].as_i64(), Some(1));
    assert_eq!(rounds[0]["failed"].as_i64(), Some(1));
    assert_eq!(rounds[0]["note"].as_str(), Some("second round"));
    assert_eq!(body["next_before_id"], Value::Null);

    // One row per page, walking backwards with the cursor.
    let page: Value = serde_json::from_str(
        &get(
            &client,
            &base,
            "/api/v1/rounds",
            &[("limit", "1")],
            Some(ADMIN),
        )
        .await
        .text()
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(page["rounds"][0]["round_id"].as_i64(), Some(second));
    assert_eq!(page["next_before_id"].as_i64(), Some(second));
    let before = second.to_string();
    let older: Value = serde_json::from_str(
        &get(
            &client,
            &base,
            "/api/v1/rounds",
            &[("limit", "1"), ("before_id", before.as_str())],
            Some(ADMIN),
        )
        .await
        .text()
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(older["rounds"][0]["round_id"].as_i64(), Some(first));

    for query in [vec![("limit", "0")], vec![("before_id", "abc")]] {
        assert_eq!(
            get(&client, &base, "/api/v1/rounds", &query, Some(ADMIN))
                .await
                .status(),
            StatusCode::BAD_REQUEST,
            "{query:?}"
        );
    }
}

/// One round by id; a long ledger note is truncated for the wire.
#[tokio::test]
async fn round_detail_returns_one_round_and_truncates_notes() {
    let state = test_state();
    let long_note = "n".repeat(MAX_NOTE_CHARS + 100);
    let round_id = {
        let storage = state.storage.lock().unwrap();
        let id = storage.start_round("manual", None).unwrap();
        storage.finish_round(id, Some(&long_note)).unwrap();
        id
    };
    let base = spawn(state).await;
    let client = reqwest::Client::new();
    let path = format!("/api/v1/rounds/{round_id}");

    let body: Value = serde_json::from_str(
        &get(&client, &base, &path, &[], Some(ADMIN))
            .await
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["round_id"].as_i64(), Some(round_id));
    let note = body["note"].as_str().unwrap();
    assert_eq!(note.chars().count(), MAX_NOTE_CHARS + 1);
    assert!(note.ends_with('…'));

    for path in [
        "/api/v1/rounds/999999".to_string(),
        "/api/v1/rounds/not-a-number".to_string(),
    ] {
        let response = get(&client, &base, &path, &[], Some(ADMIN)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(error_code(&body), Some("not_found"));
    }
}

/// The whole read surface is admin-only: a publish credential gets a 401.
#[tokio::test]
async fn read_endpoints_reject_the_publish_token() {
    let state = test_state();
    let (first, _) = seed(&state);
    let base = spawn(state).await;
    let client = reqwest::Client::new();
    let paths = [
        "/api/v1/nodes".to_string(),
        "/api/v1/nodes/a1".to_string(),
        "/api/v1/rounds".to_string(),
        format!("/api/v1/rounds/{first}"),
    ];
    for path in &paths {
        assert_eq!(
            get(&client, &base, path, &[], Some(PUBLISH)).await.status(),
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
        assert_eq!(
            get(&client, &base, path, &[], None).await.status(),
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
        assert_eq!(
            get(&client, &base, path, &[], Some(ADMIN)).await.status(),
            StatusCode::OK,
            "{path}"
        );
    }
}

// --- round cancel --------------------------------------------------------

/// Register a simulated in-flight round; the guard clears it when dropped.
fn register(state: &SharedState, round_id: i64) -> (RoundCtx, RunningRoundGuard) {
    let ctx = RoundCtx::new(state.round.limits);
    let guard = state.register_running(round_id, ctx.clone());
    (ctx, guard)
}

async fn cancel(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    token: Option<&str>,
) -> reqwest::Response {
    let mut request = client.post(format!("{base}/api/v1/rounds/{id}/cancel"));
    if let Some(token) = token {
        request = request.header("X-Auth-Token", token);
    }
    request.send().await.unwrap()
}

/// Nothing in flight is a 409 with the standard error body, and the control
/// endpoint stays behind the admin guard.
#[tokio::test]
async fn cancel_with_nothing_in_flight_is_409_with_the_standard_error_body() {
    let state = test_state();
    let base = spawn(state).await;
    let client = reqwest::Client::new();
    let response = cancel(&client, &base, "7", Some(ADMIN)).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value = response.json().await.unwrap();
    assert_eq!(error_code(&body), Some("no_round"));
    let denied = cancel(&client, &base, "7", Some(PUBLISH)).await;
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        cancel(&client, &base, "7", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

/// Only the round that actually holds the slot can be cancelled; the shared
/// token fires and the slot clears when the task guard drops.
#[tokio::test]
async fn cancel_fires_only_for_the_in_flight_round() {
    let state = test_state();
    let base = spawn(state.clone()).await;
    let client = reqwest::Client::new();
    let (ctx, guard) = register(&state, ROUND);

    let mismatch = cancel(&client, &base, "8", Some(ADMIN)).await;
    assert_eq!(mismatch.status(), StatusCode::NOT_FOUND);
    let body: Value = mismatch.json().await.unwrap();
    assert_eq!(error_code(&body), Some("not_found"));
    for id in ["abc", "0", "-3"] {
        assert_eq!(
            cancel(&client, &base, id, Some(ADMIN)).await.status(),
            StatusCode::NOT_FOUND,
            "{id}"
        );
    }
    assert!(!ctx.is_cancelled(), "a mismatched id must not cancel");

    let response = cancel(&client, &base, "7", Some(ADMIN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["round_id"].as_i64(), Some(ROUND));
    assert_eq!(body["cancelling"].as_bool(), Some(true));
    assert!(ctx.is_cancelled());

    drop(guard);
    assert_eq!(
        cancel(&client, &base, "7", Some(ADMIN)).await.status(),
        StatusCode::CONFLICT
    );
}
