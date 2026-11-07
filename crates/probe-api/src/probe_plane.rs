//! Probe-agent API surface (G-3, HK3 §4.2/§13.1; Python `_v1_result` /
//! heartbeat 路由的 Rust 镜像)。
//!
//! 两套凭证刻意分离（§13.1）：admin token 能驱动宿主 docker，绝不能认证
//! probe 路由；probe token 也绝不能到达 admin 路由。probe token 只从
//! `probes[].token_env` 命名的环境变量读——请求、配置文件、响应里都不存在。
//!
//! 每一种拒绝都只是审计口径（稳定错误码），绝不产生观测行、绝不改死亡
//! 计数（§4.2）。错误码与 HTTP 状态映射照抄 Python `V1_ERROR_STATUS`。
//! 签名（X-Probe-Signature）属于 dispatch 方向（controller → agent）；
//! agent → controller 的回传走 Bearer 探测点凭证，与 Python 端点同协议，
//! Python agent 与 Rust agent 可互换。

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use probe_config::ProbeSpec;
use probe_engine::probe_dispatch::{self, RoundState, VerifyReject};
use serde_json::{json, Value};

use crate::{token_matches, AppState, SharedState};

/// Python `V1_ERROR_STATUS` 的相关行（G-3 面；未列出的码按 Python 的
/// 默认 400 走）。
fn error_status(code: &str) -> StatusCode {
    match code {
        "UNKNOWN_PROBE" => StatusCode::NOT_FOUND,
        "ROUND_STALE" | "DIGEST_MISMATCH" | "RESULT_LATE" => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    }
}

/// The routes this module adds. Probe routes authenticate with the PROBE
/// token (Bearer), not the admin token, so they live OUTSIDE the guarded
/// group and do their own auth.
pub(crate) fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/probes/{probe_id}/heartbeat", post(heartbeat))
        .route(
            "/api/v1/probes/{probe_id}/jobs/{round_id}/result",
            post(result),
        )
}

/// `probes[].token_env` 里存的是环境变量名——token 本体只从这里读。
fn probe_token(entry: &ProbeSpec) -> Option<String> {
    if entry.token_env.is_empty() {
        return None;
    }
    std::env::var(&entry.token_env)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// The Bearer token a request presented.
fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// Probe-token auth: the entry must be registered (live config, or the
/// ledger's heartbeat table for a probe that was since removed from the
/// config — audit-only visibility), and the presented Bearer must match the
/// env-var token. `PROBE_TOKEN_UNSET` is an operator mistake and must not
/// read as 401 (Python probe_auth 契约).
fn probe_auth(
    state: &AppState,
    probe_id: &str,
    headers: &HeaderMap,
) -> Result<ProbeSpec, (StatusCode, &'static str)> {
    let entry = state
        .probes
        .iter()
        .find(|probe| probe.id == probe_id)
        .cloned();
    let Some(entry) = entry else {
        let known_in_ledger = state
            .storage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .list_probes()
            .map(|probes| probes.iter().any(|probe| probe.probe_id == probe_id))
            .unwrap_or(false);
        if known_in_ledger {
            // 账本里心跳过：注册过但已从配置摘除——维持 UNKNOWN_PROBE 语义
            // 但审计可见（Python _probe_known 的宽口径）。
            return Err((StatusCode::NOT_FOUND, "UNKNOWN_PROBE"));
        }
        return Err((StatusCode::NOT_FOUND, "UNKNOWN_PROBE"));
    };
    let Some(token) = probe_token(&entry) else {
        return Err((StatusCode::CONFLICT, "PROBE_TOKEN_UNSET"));
    };
    let presented = bearer(headers).unwrap_or_default();
    if !token_matches(&presented, &token) {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    Ok(entry)
}

fn deny(status: StatusCode, code: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": code })))
}

/// `POST /api/v1/probes/{probe_id}/heartbeat` — stamp last_seen_at and mark
/// the probe online. 0 rows updated = not registered: audit only, never a
/// node failure (§4.2).
async fn heartbeat(
    State(state): State<SharedState>,
    Path(probe_id): Path<String>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    let entry = match probe_auth(&state, &probe_id, &headers) {
        Ok(entry) => entry,
        Err((status, code)) => return deny(status, code),
    };
    let touched = state
        .storage
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .touch_probe_seen(&probe_id, None, Some("online"));
    match touched {
        Ok(rows) if rows > 0 => (
            StatusCode::OK,
            Json(json!({"ok": true, "probe_id": entry.id})),
        ),
        Ok(_) => deny(StatusCode::NOT_FOUND, "UNKNOWN_PROBE"),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "storage", "detail": err.to_string()})),
        ),
    }
}

/// `POST /api/v1/probes/{probe_id}/jobs/{round_id}/result` — validate one
/// ProbeResult and record its observations idempotently. The raw body is
/// taken as `Bytes` FIRST so a future signature layer can hash the exact
/// bytes (the dispatch direction signs; this endpoint authenticates with
/// the Bearer probe token, Python 同协议).
async fn result(
    State(state): State<SharedState>,
    Path((probe_id, path_round_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    if let Err((status, code)) = probe_auth(&state, &probe_id, &headers) {
        return deny(status, code);
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return deny(StatusCode::BAD_REQUEST, "BAD_PAYLOAD");
    };
    let audit_id = audit_id();
    // 路径点名了 Controller 派发的轮次；正文不一致是客户端 bug——按正文的
    // 轮次记账会让审计行说谎（Python ROUND_MISMATCH 契约）。
    let body_round = payload.get("round_id").and_then(Value::as_i64);
    match body_round {
        Some(value) if value.to_string() == path_round_id => {}
        _ => {
            return deny(StatusCode::BAD_REQUEST, "ROUND_MISMATCH");
        }
    }
    let Ok(path_round) = path_round_id.parse::<i64>() else {
        return deny(StatusCode::BAD_REQUEST, "ROUND_MISMATCH");
    };

    // 轮次状态从账本查：存在、未结算、仍是最新（旧轮次只入审计，§4.2）。
    let storage = state
        .storage
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let round = match storage.probe_round_state(path_round) {
        Ok((exists, finished, newest)) => RoundState {
            exists,
            finished,
            is_newest: newest == path_round,
        },
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "storage", "detail": err.to_string()})),
            )
        }
    };
    // digest 期望值：dispatch 侧签发 Job 时写入本轮行的 config_digest；
    // 尚未落列的部署传空——空期望值让 digest 校验退化为「必须非空且等于
    // 自身」之外的不可能（Python expected_digest=None → 用 config_digest(cfg)
    // 重算的口径在 Rust 侧由 dispatch 落列后启用）。
    let expected_digest = storage
        .round_config_digest(path_round)
        .unwrap_or(None)
        .unwrap_or_default();
    // 列空 = 尚未启用 dispatch 的部署：任何非空 digest 都接受（Python
    // expected_digest=None 的重算口径在 dispatch 落列后生效）。
    // dispatch 登记的截止时间（probe_deadlines 表，migration 0014）：
    // None = 未启用 dispatch 的部署，RESULT_LATE 由 round-match 门禁兜底
    // （Python _deadline_for 的 None 分支）。
    let deadline_epoch = storage
        .probe_deadline(&probe_id, &path_round_id)
        .ok()
        .flatten()
        .and_then(|stamp| parse_epoch(&stamp));
    let finished_at_epoch = payload
        .get("finished_at")
        .and_then(Value::as_str)
        .and_then(parse_epoch);
    let probe_known = state.probes.iter().any(|probe| probe.id == probe_id)
        || storage
            .list_probes()
            .map(|probes| probes.iter().any(|probe| probe.probe_id == probe_id))
            .unwrap_or(false);

    match probe_dispatch::verify_result(
        &payload,
        round,
        probe_known,
        &expected_digest,
        deadline_epoch,
        finished_at_epoch,
        now_epoch(),
    ) {
        Ok((items, payload_digest)) => {
            let mut recorded = 0usize;
            for item in &items {
                if storage
                    .record_observation(
                        &path_round_id,
                        &probe_id,
                        &item.fingerprint,
                        Some(item.alive),
                        item.delay_ms,
                        item.reason.as_deref(),
                        item.egress_country.as_deref(),
                        &payload_digest,
                    )
                    .is_ok()
                {
                    recorded += 1;
                }
            }
            let _ = storage.touch_probe_seen(&probe_id, None, Some("online"));
            (
                StatusCode::OK,
                Json(json!({"ok": true, "recorded": recorded, "audit_id": audit_id})),
            )
        }
        Err(rejection) => {
            let code = rejection.as_str();
            (
                error_status(code),
                Json(json!({
                    "error": code,
                    "audit_id": audit_id,
                    "detail": rejection_detail(&rejection),
                })),
            )
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SS`（UTC，秒精度，无时区后缀）→ Unix epoch；
/// 解析失败 = None。存储值恒为 UTC（`utc_now()`），按 naive 解析后假设 UTC。
fn parse_epoch(stamp: &str) -> Option<i64> {
    let format = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    time::PrimitiveDateTime::parse(stamp, &format)
        .ok()
        .map(|dt| dt.assume_utc().unix_timestamp())
}

fn rejection_detail(rejection: &VerifyReject) -> String {
    match rejection {
        VerifyReject::BadPayload(detail) => (*detail).to_string(),
        VerifyReject::RoundStale(detail) => (*detail).to_string(),
        VerifyReject::UnknownProbe => "探测点未注册".to_string(),
        VerifyReject::DigestMismatch => "config_digest 与当前配置不一致".to_string(),
        VerifyReject::ResultLate(detail) => (*detail).to_string(),
    }
}

/// 一次接收的审计 id（uuid4 形状；非安全用途，熵来自进程内计数器 + 时间）。
fn audit_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:016x}{counter:016x}")
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, OnceLock};

    use axum::http::StatusCode;
    use probe_config::{ProbeSpec, SourceSpec};
    use probe_engine::RoundSettings;
    use probe_mihomo::Controller;
    use probe_storage::Storage;
    use serde_json::{json, Value};

    use crate::{build_router, AppState, ServeConfig};

    /// 一个测试专用的探测点 token：环境变量进程级设置一次（并行测试共享）。
    fn probe_token_value() -> &'static str {
        static TOKEN: OnceLock<String> = OnceLock::new();
        TOKEN.get_or_init(|| {
            std::env::set_var("PROBE_TOKEN_TEST_HK3", "probe-secret-9137");
            "probe-secret-9137".to_string()
        })
    }

    fn probe_entry() -> ProbeSpec {
        ProbeSpec {
            id: "hk3".into(),
            name: "hk3".into(),
            enabled: true,
            endpoint: "https://p.example.com".into(),
            token_env: "PROBE_TOKEN_TEST_HK3".into(),
            weight: 1,
            tags: vec![],
        }
    }

    fn state(root: &std::path::Path, probes: Vec<ProbeSpec>) -> Arc<AppState> {
        let cfg = probe_config::Config::load(Path::new("/nonexistent/config.json"))
            .expect("default config");
        Arc::new(AppState::new(
            Storage::open_in_memory().unwrap(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            Controller::new("http://127.0.0.1:1", None),
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
                probes,
            },
        ))
    }

    async fn spawn(state: Arc<AppState>) -> String {
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    async fn post_json(
        base: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let client = reqwest::Client::new();
        let mut request = client.post(format!("{base}{path}"));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.json(&body).send().await.unwrap();
        let status = response.status();
        let payload = response.json::<Value>().await.unwrap_or(Value::Null);
        (status, payload)
    }

    /// 未注册探测点：heartbeat 与 result 都是 404 UNKNOWN_PROBE——审计口径，
    /// 绝不产生观测行（§4.2）。
    #[tokio::test]
    async fn an_unregistered_probe_is_unknown_on_both_routes() {
        probe_token_value();
        let tmp = tempfile::tempdir().unwrap();
        let base = spawn(state(tmp.path(), vec![])).await;
        // 两套凭证分离：probe 路由对一切非 probe 凭证（含 admin token）按
        // 未注册/未授权掩面；未注册探测点的冻结码是 UNKNOWN_PROBE → 404。
        let (status, body) = post_json(
            &base,
            "/api/v1/probes/ghost/heartbeat",
            Some("x"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"], "UNKNOWN_PROBE");
        let (status, body) = post_json(
            &base,
            "/api/v1/probes/ghost/jobs/1/result",
            Some("x"),
            json!({"round_id": 1, "probe_id": "ghost", "config_digest": "d", "results": []}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"], "UNKNOWN_PROBE");
    }

    /// 已注册但 token_env 未设置：409 PROBE_TOKEN_UNSET——运维失误不是
    /// 认证失败（Python probe_auth 契约）。
    #[tokio::test]
    async fn an_unset_token_env_is_not_an_auth_failure() {
        probe_token_value();
        let tmp = tempfile::tempdir().unwrap();
        let entry = ProbeSpec {
            token_env: "PROBE_TOKEN_DEFINITELY_UNSET_9137".into(),
            ..probe_entry()
        };
        let base = spawn(state(tmp.path(), vec![entry])).await;
        let (status, body) =
            post_json(&base, "/api/v1/probes/hk3/heartbeat", Some("x"), json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "PROBE_TOKEN_UNSET");
    }

    /// 错 token → 401；对 token → 200 且心跳写入账本（state=online）。
    #[tokio::test]
    async fn a_heartbeat_stamps_the_ledger_row() {
        let token = probe_token_value();
        let tmp = tempfile::tempdir().unwrap();
        let base = spawn(state(tmp.path(), vec![probe_entry()])).await;
        let (status, _) = post_json(
            &base,
            "/api/v1/probes/hk3/heartbeat",
            Some("wrong"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = post_json(
            &base,
            "/api/v1/probes/hk3/heartbeat",
            Some(token),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        // admin token 不能认证 probe 路由（两套凭证刻意分离，§13.1）。
        let (status, _) = post_json(
            &base,
            "/api/v1/probes/hk3/heartbeat",
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// 结果回传的冻结类：ROUND_MISMATCH / ROUND_STALE / BAD_PAYLOAD；
    /// 未启用 dispatch（digest 列空）时合法结果被接受并落观测（幂等覆盖）。
    #[tokio::test]
    async fn the_result_route_maps_the_frozen_classes() {
        let token = probe_token_value();
        let tmp = tempfile::tempdir().unwrap();
        let app_state = state(tmp.path(), vec![probe_entry()]);
        {
            let storage = app_state.storage.lock().unwrap();
            storage
                .upsert_probe("hk3", "hk3", true, None, None, None)
                .unwrap();
            storage.start_round("manual", Some("direct")).unwrap();
        }
        let round_id = app_state
            .storage
            .lock()
            .unwrap()
            .list_rounds(1, None)
            .unwrap()[0]
            .round_id;
        let base = spawn(app_state.clone()).await;

        // 正文轮次与路径不一致 → 400 ROUND_MISMATCH。
        let (status, body) = post_json(
            &base,
            "/api/v1/probes/hk3/jobs/7/result",
            Some(token),
            json!({"round_id": 8, "probe_id": "hk3", "config_digest": "d", "results": []}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "ROUND_MISMATCH");

        // 轮次不存在 → 409 ROUND_STALE（audit-only，绝不落观测）。
        let (status, body) = post_json(
            &base,
            "/api/v1/probes/hk3/jobs/99999/result",
            Some(token),
            json!({"round_id": 99999, "probe_id": "hk3", "config_digest": "d", "results": []}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "ROUND_STALE");

        // 轮次存在、未结算、最新：结果合法 → 接受 + 落观测。
        let (status, body) = post_json(
            &base,
            &format!("/api/v1/probes/hk3/jobs/{round_id}/result"),
            Some(token),
            json!({
                "round_id": round_id,
                "probe_id": "hk3",
                "config_digest": "d",
                "results": [
                    {"fingerprint": "fp-1", "alive": true, "delay_ms": 120, "egress_country": "JP"},
                    {"fingerprint": "fp-2", "alive": false, "reason": "timeout"}
                ],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["recorded"], 2);

        // 幂等覆盖：重复上报不翻倍（PK round+probe+fingerprint）。
        let (status, body) = post_json(
            &base,
            &format!("/api/v1/probes/hk3/jobs/{round_id}/result"),
            Some(token),
            json!({
                "round_id": round_id,
                "probe_id": "hk3",
                "config_digest": "d",
                "results": [
                    {"fingerprint": "fp-1", "alive": true, "delay_ms": 120, "egress_country": "JP"}
                ],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let observations = app_state
            .storage
            .lock()
            .unwrap()
            .get_observations(&round_id.to_string())
            .unwrap();
        assert_eq!(
            observations.len(),
            2,
            "PK overwrite, never duplicate: {body}"
        );
        assert_eq!(observations[0].alive, Some(true));
        assert_eq!(observations[1].alive, Some(false));
    }
}
