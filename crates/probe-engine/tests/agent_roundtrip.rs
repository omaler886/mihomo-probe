//! G-3 dispatch→agent→result 全链路（workstreams/31，HK3 §4.1/§4.2/§13.1）。
//!
//! 组合测试：真协议函数（dispatch_job 签名发送 / verify_job 门禁 /
//! post_result 回传）+ 假测量器（JobRunner），在 127.0.0.1 上跑一个内联
//! agent（与 probe-cli agent 同一 verify→busy→execute→post 顺序）与一个
//! 记录型 Controller。钉住的行为：签名 Job 被接受、测量结果按 §4.2 形状
//! 回传、错签名 403、BUSY 409、payload digest 在两侧一致。

use std::sync::Arc;
use std::time::Duration;

use probe_config::ProbeSpec;
use probe_engine::agent::{
    self, dispatch_job, execute_job, post_result, verify_job, JobNode, JobRunner, JobWire,
};
use probe_engine::probe_dispatch::{sign_request, SIGNATURE_HEADER, TIMESTAMP_HEADER};
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// 记录型 Controller：收 `/api/v1/probes/{id}/jobs/{round}/result`。
async fn spawn_stub_controller() -> (String, Arc<Mutex<Vec<Value>>>) {
    let recorded: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&recorded);
    let app = axum::Router::new().route(
        "/api/v1/probes/{probe_id}/jobs/{round_id}/result",
        axum::routing::post(
            move |axum::extract::Path((probe_id, round_id)): axum::extract::Path<(
                String,
                String,
            )>,
                  body: String| {
                let sink = Arc::clone(&sink);
                async move {
                    let payload: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    sink.lock().await.push(json!({
                        "probe_id": probe_id,
                        "round_id": round_id,
                        "payload": payload,
                    }));
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({"ok": true, "recorded": 1})),
                    )
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, recorded)
}

/// 内联 agent：POST /jobs 用与 probe-cli agent 完全相同的
/// verify_job → busy → execute_job → post_result 顺序。
struct InlineAgent {
    base: String,
    secret: String,
    busy: Arc<std::sync::atomic::AtomicBool>,
    received: Arc<Mutex<Vec<JobWire>>>,
}

impl InlineAgent {
    async fn spawn(runner: Arc<dyn JobRunner + Send + Sync>, controller: &str) -> Self {
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let received: Arc<Mutex<Vec<JobWire>>> = Arc::default();
        let agent_busy = Arc::clone(&busy);
        let agent_received = Arc::clone(&received);
        let secret = "probe-secret-3131".to_string();
        let agent_secret = secret.clone();
        let controller = controller.to_string();
        let app = axum::Router::new().route(
            "/jobs",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let secret = agent_secret.clone();
                    let busy = Arc::clone(&agent_busy);
                    let received = Arc::clone(&agent_received);
                    let runner = Arc::clone(&runner);
                    let controller = controller.clone();
                    async move {
                        let bearer = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("Bearer "))
                            .map(str::trim);
                        let ts = headers.get(TIMESTAMP_HEADER).and_then(|v| v.to_str().ok());
                        let signature = headers.get(SIGNATURE_HEADER).and_then(|v| v.to_str().ok());
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        let job = match verify_job(bearer, ts, signature, &body, &secret, now) {
                            Ok(job) => job,
                            Err(reject) => {
                                return (
                                    axum::http::StatusCode::from_u16(reject.status()).unwrap(),
                                    axum::Json(json!({"error": reject.code()})),
                                );
                            }
                        };
                        use std::sync::atomic::Ordering;
                        if busy
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_err()
                        {
                            return (
                                axum::http::StatusCode::CONFLICT,
                                axum::Json(json!({"error": "BUSY"})),
                            );
                        }
                        received.lock().await.push(job.clone());
                        tokio::spawn(async move {
                            let result =
                                execute_job(runner.as_ref(), &job, "s".into(), "f".into()).await;
                            let _ = post_result(&controller, &job.probe_id, &secret, &result).await;
                            busy.store(false, Ordering::SeqCst);
                        });
                        (
                            axum::http::StatusCode::ACCEPTED,
                            axum::Json(json!({"accepted": true})),
                        )
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            secret,
            busy,
            received,
        }
    }
}

struct FakeRunner;

impl JobRunner for FakeRunner {
    fn run<'a>(
        &'a self,
        job: &'a JobWire,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Vec<probe_engine::agent::ProbeResultItem>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            job.nodes
                .iter()
                .map(|node| probe_engine::agent::ProbeResultItem {
                    fingerprint: node
                        .fingerprint
                        .clone()
                        .unwrap_or_else(|| node.mihomo_name.clone()),
                    alive: node.mihomo_name != "dead-node",
                    delay_ms: if node.mihomo_name == "dead-node" {
                        None
                    } else {
                        Some(120)
                    },
                    reason: (node.mihomo_name == "dead-node").then(|| "timeout".to_string()),
                    egress_country: None,
                })
                .collect()
        })
    }
}

fn job_for(probe_id: &str, round_id: i64) -> JobWire {
    JobWire {
        round_id: round_id.to_string(),
        probe_id: probe_id.to_string(),
        config_digest: "sha256:digest-31".to_string(),
        deadline: None,
        chain_enabled: false,
        nodes: vec![
            JobNode {
                fingerprint: Some("fp-alive".into()),
                mihomo_name: "alive-node".into(),
                proxy: json!({"name": "alive-node", "type": "ss", "server": "s", "port": 443}),
            },
            JobNode {
                fingerprint: Some("fp-dead".into()),
                mihomo_name: "dead-node".into(),
                proxy: json!({"name": "dead-node", "type": "ss", "server": "s2", "port": 444}),
            },
        ],
    }
}

#[tokio::test]
async fn a_signed_job_round_trips_through_the_agent_to_the_controller() {
    let (controller_base, recorded) = spawn_stub_controller().await;
    let agent_server = InlineAgent::spawn(Arc::new(FakeRunner), &controller_base).await;

    // 派发侧：签名 Job → agent。
    let job = job_for("hk3", 31);
    dispatch_job(&job, &agent_server.base, &agent_server.secret)
        .await
        .expect("signed dispatch must be accepted");

    // agent 收到的 Job 与派发的一致。
    let received = agent_server.received.lock().await.clone();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0], job);

    // 回传落到 Controller（等 agent 的 spawn 完成回传）。
    for _ in 0..50 {
        if !recorded.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let rows = recorded.lock().await;
    assert_eq!(rows.len(), 1, "one ProbeResult lands: {rows:?}");
    let payload = &rows[0]["payload"];
    assert_eq!(rows[0]["probe_id"], "hk3");
    assert_eq!(rows[0]["round_id"], "31");
    assert_eq!(payload["config_digest"], "sha256:digest-31");
    assert_eq!(payload["results"].as_array().unwrap().len(), 2);
    assert_eq!(payload["results"][0]["fingerprint"], "fp-alive");
    assert_eq!(payload["results"][0]["alive"], true);
    assert_eq!(payload["results"][1]["alive"], false);
    assert_eq!(payload["results"][1]["reason"], "timeout");
    assert_eq!(payload["results"][0]["egress_country"], Value::Null);
}

#[tokio::test]
async fn a_forged_request_is_rejected_and_nothing_executes() {
    let (controller_base, recorded) = spawn_stub_controller().await;
    let agent_server = InlineAgent::spawn(Arc::new(FakeRunner), &controller_base).await;
    let client = reqwest::Client::new();

    // 错 Bearer（含错 secret 的整包伪造）→ 401：token 先于 body 字节。
    let job = job_for("hk3", 32);
    let outcome = dispatch_job(&job, &agent_server.base, "attacker-secret").await;
    assert!(
        outcome.is_err(),
        "the agent answers 401 — an HTTP error the dispatch surfaces as 观测缺失"
    );

    // 对的 Bearer 但错签名（body 被篡改）→ 403 SIGNATURE_INVALID。
    let body = serde_json::to_vec(&job.to_json()).unwrap();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let forged = sign_request("attacker-secret", &body, ts as i64);
    let response = client
        .post(format!("{}/jobs", agent_server.base))
        .header("Authorization", format!("Bearer {}", agent_server.secret))
        .header(TIMESTAMP_HEADER, ts.to_string())
        .header(SIGNATURE_HEADER, forged)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::FORBIDDEN,
        "forged signature"
    );

    assert!(
        agent_server.received.lock().await.is_empty(),
        "nothing executed"
    );
    assert!(recorded.lock().await.is_empty(), "nothing posted back");
    assert!(!agent_server.busy.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn a_second_job_while_busy_answers_conflict() {
    let (controller_base, _recorded) = spawn_stub_controller().await;
    // 慢 runner 占住 busy 位。
    struct SlowRunner;
    impl JobRunner for SlowRunner {
        fn run<'a>(
            &'a self,
            _job: &'a JobWire,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Vec<probe_engine::agent::ProbeResultItem>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Vec::new()
            })
        }
    }
    let agent_server = InlineAgent::spawn(Arc::new(SlowRunner), &controller_base).await;

    let client = reqwest::Client::new();
    async fn send(
        client: &reqwest::Client,
        base: &str,
        secret: &str,
        job: &JobWire,
    ) -> reqwest::Response {
        let body = serde_json::to_vec(&job.to_json()).unwrap();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let signature = sign_request(secret, &body, ts as i64);
        client
            .post(format!("{base}/jobs"))
            .header("Authorization", format!("Bearer {secret}"))
            .header(TIMESTAMP_HEADER, ts.to_string())
            .header(SIGNATURE_HEADER, signature)
            .body(body)
            .send()
            .await
            .unwrap()
    }

    let first = send(
        &client,
        &agent_server.base,
        &agent_server.secret,
        &job_for("hk3", 33),
    )
    .await;
    assert_eq!(first.status(), axum::http::StatusCode::ACCEPTED);
    // busy 仍被持有（runner 500ms）：第二个 Job 409。
    let second = send(
        &client,
        &agent_server.base,
        &agent_server.secret,
        &job_for("hk3", 34),
    )
    .await;
    assert_eq!(
        second.status(),
        axum::http::StatusCode::CONFLICT,
        "{:?}",
        second.text().await
    );
    // 第一个完成后 busy 释放：第三个 Job 再次被接受。
    tokio::time::sleep(Duration::from_millis(900)).await;
    let third = send(
        &client,
        &agent_server.base,
        &agent_server.secret,
        &job_for("hk3", 35),
    )
    .await;
    assert_eq!(third.status(), axum::http::StatusCode::ACCEPTED);
}

/// build_jobs 的注册表面：只有启用的探测点拿到 Job（禁用的不派发）。
#[tokio::test]
async fn dispatch_covers_only_enabled_probes() {
    let probes = vec![
        ProbeSpec {
            id: "p1".into(),
            enabled: true,
            endpoint: "http://127.0.0.1:1".into(),
            ..ProbeSpec::default()
        },
        ProbeSpec {
            id: "p2".into(),
            enabled: false,
            endpoint: "http://127.0.0.1:1".into(),
            ..ProbeSpec::default()
        },
    ];
    let jobs = agent::build_jobs(&probes, 36, "sha256:d", None, false, Vec::new());
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].probe_id, "p1");
}
