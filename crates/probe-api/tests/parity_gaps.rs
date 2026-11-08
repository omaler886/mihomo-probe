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
        alert: probe_config::AlertSection::default(),
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

/// workstreams/33：API 轮的 dispatch 发送与 CLI 同构——
/// POST /api/v1/rounds 触发的轮次把签名 Job 派发给注册探测点、登记 digest
/// 与 deadline；轮次本体随后的内核失败不影响这些簿记。
#[tokio::test]
async fn the_api_round_dispatches_signed_jobs_and_registers_the_bookkeeping() {
    use std::sync::Mutex as StdMutex;
    use std::sync::OnceLock;

    // 探测点 token（进程级设置一次；命名带 stub 标记过 scan_secrets）。
    static TOKEN: OnceLock<String> = OnceLock::new();
    let token = TOKEN
        .get_or_init(|| {
            std::env::set_var("PROBE_TOKEN_STUB_API33", "stub-probe-secret-3131");
            "stub-probe-secret-3131".to_string()
        })
        .clone();

    // 假 agent：收 /jobs（签名复核 + 记录），同 agent_roundtrip 的顺序。
    let received: Arc<StdMutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&received);
    let agent_app = axum::Router::new().route(
        "/jobs",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let sink = Arc::clone(&sink);
                async move {
                    let ts = headers
                        .get("X-Probe-Timestamp")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let signature = headers
                        .get("X-Probe-Signature")
                        .and_then(|v| v.to_str().ok());
                    let bearer = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .map(str::trim);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let job = probe_engine::agent::verify_job(
                        bearer,
                        Some(ts),
                        signature,
                        &body,
                        token.as_str(),
                        now,
                    );
                    match job {
                        Ok(job) => {
                            sink.lock()
                                .unwrap()
                                .push(serde_json::to_value(job.to_json()).unwrap());
                            (
                                axum::http::StatusCode::ACCEPTED,
                                axum::Json(json!({"accepted": true})),
                            )
                        }
                        Err(reject) => (
                            axum::http::StatusCode::from_u16(reject.status()).unwrap(),
                            axum::Json(json!({"error": reject.code()})),
                        ),
                    }
                }
            },
        ),
    );
    let agent_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let agent_base = format!("http://{}", agent_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(agent_listener, agent_app).await.unwrap() });

    // 带 probes 注册表的 AppState（真实 API 配置路径：cfg.probes → state）。
    let tmp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&tmp.path().join("ledger.db")).unwrap();
    let mut state = state_with(storage, tmp.path());
    // 经 ServeConfig 注入注册表：直接改 AppState（测试专用入口）。
    let probes = vec![probe_config::ProbeSpec {
        id: "stub1".into(),
        name: "stub".into(),
        enabled: true,
        endpoint: agent_base.clone(),
        token_env: "PROBE_TOKEN_STUB_API33".into(),
        weight: 1,
        tags: vec![],
    }];
    {
        let serve_probes = probes.clone();
        let state_mut = Arc::get_mut(&mut state).expect("single owner");
        state_mut.probes = serve_probes;
        state_mut.probe_config_digest = "sha256:stub-digest-33".into();
    }
    let base = spawn(state.clone()).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/api/v1/rounds"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "{}",
        response.text().await.unwrap_or_default()
    );

    // 假 agent 收到签名 Job（轮次收集无 sources → 0 节点 Job，协议有效）。
    for _ in 0..50 {
        if !received.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let jobs = received.lock().unwrap();
    assert_eq!(jobs.len(), 1, "one signed job dispatched: {jobs:?}");
    assert_eq!(jobs[0]["probe_id"], "stub1");
    let round_id = jobs[0]["round_id"].as_str().unwrap().to_string();
    assert!(!round_id.is_empty());

    // 簿记落库：digest 进 rounds 行、deadline 进 probe_deadlines。
    let storage = Storage::open(&tmp.path().join("ledger.db")).unwrap();
    assert_eq!(
        storage
            .round_config_digest(round_id.parse::<i64>().unwrap())
            .unwrap()
            .as_deref(),
        Some("sha256:stub-digest-33")
    );
    let deadline = storage
        .probe_deadline("stub1", &round_id)
        .unwrap()
        .expect("the deadline was registered");
    assert!(!deadline.is_empty());
}
