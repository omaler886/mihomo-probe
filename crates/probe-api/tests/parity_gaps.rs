//! 21_RUST_PARITY G-7 / G-10 的 HTTP 级收口（19_ACCEPTANCE_MAP §11.1-8 /
//! §11.2-5 的缺口行）。
//!
//! * G-7：`POST /api/v1/sources/{id}/runs` 的 `Idempotency-Key` 重放——同一
//!   key 两次请求必须拿到同一 run_id（claim 语义此前只有 DB 级测试钉住）。
//! * G-10：双进程并发触发——两个 AppState 各持一条独立 SQLite 连接（同一
//!   账本文件，等于两个进程），并发 `POST /api/v1/rounds`，恰好一个拿到
//!   round、另一个 409（跨进程互斥的真实路径；进程内 in-flight 旗标管不到
//!   另一个进程）。

use std::path::Path;
use std::sync::Arc;

use axum::http::StatusCode;
use probe_config::SourceSpec;
use probe_engine::RoundSettings;
use probe_mihomo::Controller;
use probe_storage::Storage;
use serde_json::{json, Value};

use probe_api::{build_router, AppState, ServeConfig};

const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn serve_config(root: &Path) -> ServeConfig {
    let cfg =
        probe_config::Config::load(Path::new("/nonexistent/config.json")).expect("default config");
    ServeConfig {
        kernel_config_path: "/test/config.yaml".into(),
        round: RoundSettings::from_config(&cfg),
        core: cfg.core.clone(),
        sources: Vec::<SourceSpec>::new(),
        backend: "http://127.0.0.1:1".into(),
        root: root.to_path_buf(),
        substore: None,
        substore_target_name: probe_config::DEFAULT_SUBSTORE_TARGET_NAME.into(),
        aggregation: probe_config::AggregationSection::default(),
        probe_config_digest: String::new(),
        chain: cfg.chain.clone(),
        publish_prefix: cfg.publish_prefix.clone(),
        publish: cfg.publish.clone(),
        schedule: cfg.schedule.clone(),
        policy: cfg.policy.clone(),
        min_run_interval_seconds: 0,
        substore_allow_delete: false,
        probes: Vec::new(),
    }
}

fn state_with(storage: Storage, root: &Path) -> Arc<AppState> {
    Arc::new(AppState::new(
        storage,
        Some(ADMIN.into()),
        Controller::new("http://127.0.0.1:1", None),
        serve_config(root),
    ))
}

async fn spawn(state: Arc<AppState>) -> String {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// G-7：同一 Idempotency-Key 重放返回同一 run_id（HTTP 级）。
#[tokio::test]
async fn the_idempotency_key_replay_returns_the_same_run_id_over_http() {
    let tmp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&tmp.path().join("ledger.db")).unwrap();
    let source_id = storage
        .sync_source("air", "air", "sub", "https://sub.example.com/air", true)
        .unwrap();
    let state = state_with(storage, tmp.path());
    let base = spawn(state).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/api/v1/sources/{source_id}/runs");

    let first = client
        .post(&url)
        .header("X-Auth-Token", ADMIN)
        .header("Idempotency-Key", "replay-g7")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        first.status(),
        StatusCode::OK,
        "{}",
        first.text().await.unwrap_or_default()
    );
    let first_body: Value = first.json().await.unwrap();
    assert_eq!(first_body["status"], "started");
    let run_id = first_body["run_id"].as_i64().expect("run_id");

    // 重放：同 key → 同 run_id（deduplicated，此刻槽还开着）。
    let replay = client
        .post(&url)
        .header("X-Auth-Token", ADMIN)
        .header("Idempotency-Key", "replay-g7")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    let replay_body: Value = replay.json().await.unwrap();
    assert_eq!(replay_body["deduplicated"], true, "{replay_body}");
    assert_eq!(replay_body["run_id"].as_i64(), Some(run_id), "same slot");

    // 换 key 但同 source：不同的 run_id（新 claim）或 cooldown 合并——都不
    // 是冲突；真正被钉的是跨 source 的 KeyConflict 由 DB 测试覆盖。
    let other_key = client
        .post(&url)
        .header("X-Auth-Token", ADMIN)
        .header("Idempotency-Key", "replay-g7-other")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let other_body: Value = other_key.json().await.unwrap();
    let other_run = other_body["run_id"].as_i64();
    assert_ne!(other_run, Some(0));
}

/// G-10：两个 AppState（两条独立 SQLite 连接 = 两个进程）并发触发，
/// 恰好一个 202 一个 409，账本里恰好一条 round 行。
#[tokio::test]
async fn two_processes_triggering_concurrently_start_exactly_one_round() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("ledger.db");
    // 两个「进程」：各开各的连接（SQLite 跨连接互斥是真实机制）。
    let storage_a = Storage::open(&db_path).unwrap();
    let storage_b = Storage::open(&db_path).unwrap();
    let state_a = state_with(storage_a, tmp.path());
    let state_b = state_with(storage_b, tmp.path());
    let base_a = spawn(state_a.clone()).await;
    let base_b = spawn(state_b.clone()).await;

    let client = reqwest::Client::new();
    let (first, second) = tokio::join!(
        client
            .post(format!("{base_a}/api/v1/rounds"))
            .header("X-Auth-Token", ADMIN)
            .send(),
        client
            .post(format!("{base_b}/api/v1/rounds"))
            .header("X-Auth-Token", ADMIN)
            .send(),
    );
    let statuses = [first.unwrap().status(), second.unwrap().status()];
    let started = statuses
        .iter()
        .filter(|s| **s == StatusCode::ACCEPTED)
        .count();
    let refused = statuses
        .iter()
        .filter(|s| **s == StatusCode::CONFLICT)
        .count();
    assert_eq!(started, 1, "exactly one round starts: {statuses:?}");
    assert_eq!(refused, 1, "the other process is refused: {statuses:?}");

    // 账本里恰好一条 round 行（另一条进程没有写入第二轮）。
    let storage = Storage::open(&db_path).unwrap();
    let rounds = storage.list_rounds(10, None).unwrap();
    assert_eq!(rounds.len(), 1, "one round row in the shared ledger");
}
