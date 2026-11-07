//! 人工 Sub-Store 推送三件套的 API 面（G-4，HK3 §9.4/§13.5；Python
//! `_v1_substore_{preview,confirm,push}` 的 Rust 镜像）。
//!
//! P5 之后 Sub-Store 的**唯一**写路径：预览（只读 diff）→ confirm-token
//! （60s 一次性，绑定 target+digest+节点数）→ push（校验并消费 token →
//! update-only 更新目标对象）→ publish_audit。全部 admin token 门禁、全部
//! 落审计。update-only：目标对象不存在一律报错，绝不新建。
//! canonical 产物来自本地落盘（`<export_root>/mihomo-alive.yaml` + meta，
//! workstreams/27 的 parity 文件）——尚无产物即 `NO_CANONICAL_ARTIFACT`。

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use probe_engine::manual_publish::{self, ConfirmTokens};
use probe_source::SubStoreClient;
use serde_json::{json, Value};
use std::sync::OnceLock;

use crate::{AppState, SharedState};

/// 进程内 confirm-token 注册表（重启丢失只让操作员重按一次预览）。
fn tokens() -> &'static ConfirmTokens {
    static TOKENS: OnceLock<ConfirmTokens> = OnceLock::new();
    TOKENS.get_or_init(ConfirmTokens::default)
}

/// The admin routes this module adds to the guarded group.
pub(crate) fn admin_routes() -> Router<crate::SharedState> {
    Router::new()
        .route("/api/v1/substore/preview", post(preview))
        .route("/api/v1/substore/confirm-token", post(confirm_token))
        .route("/api/v1/substore/push", post(push))
}

/// 一次动作的审计行（publish_audit 表；成功与拒绝都落，§13.5）。
fn audit(
    state: &crate::AppState,
    audit_id: &str,
    target: &str,
    digest: &str,
    kind: &str,
    summary: Value,
) {
    if let Err(err) = state
        .storage
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .record_publish_audit(
            audit_id,
            "substore_push",
            target,
            None,
            digest,
            "api",
            kind,
            Some(&summary.to_string()),
        )
    {
        tracing::warn!(%err, "publish_audit write failed");
    }
}

fn deny(code: &str, message: &str, audit_id: &str) -> (StatusCode, Json<Value>) {
    let status = match code {
        "UNKNOWN_PROBE" => StatusCode::NOT_FOUND,
        "NO_CANONICAL_ARTIFACT"
        | "CONFIRM_TOKEN_EXPIRED"
        | "CONFIRM_TOKEN_MISMATCH"
        | "CONFIRM_TOKEN_REUSED"
        | "PUSH_CONFIRM_REQUIRED"
        | "SUBSTORE_TARGET_MISSING" => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(json!({"error": code, "message": message, "audit_id": audit_id})),
    )
}

/// 本地落盘的 canonical 产物：`Some((正文, digest, count))`；meta 侧车缺失
/// 或正文缺失 = 尚无产物。
fn canonical_artifact(state: &AppState) -> Option<(String, String, usize)> {
    let export_root = state.publish.export_root(&state.root);
    let content = std::fs::read_to_string(export_root.join(manual_publish_local_file())).ok()?;
    let meta = std::fs::read_to_string(export_root.join(manual_publish_meta_file())).ok()?;
    let meta: Value = serde_json::from_str(&meta).ok()?;
    let digest = meta.get("digest").and_then(Value::as_str)?.to_string();
    let count = meta.get("count").and_then(Value::as_u64)? as usize;
    Some((content, digest, count))
}

fn manual_publish_local_file() -> &'static str {
    probe_engine::canonical_round::LOCAL_CANONICAL_FILE
}

fn manual_publish_meta_file() -> &'static str {
    probe_engine::canonical_round::LOCAL_META_FILE
}

/// `POST /api/v1/substore/preview` — canonical 与目标对象的差异（只读）。
/// 目标对象不存在不是错误——如实显示「全部为新增」，推送腿才以
/// SUBSTORE_TARGET_MISSING 拒绝（update-only，绝不新建）。
async fn preview(State(state): State<SharedState>) -> (StatusCode, Json<Value>) {
    let audit_id = audit_id();
    let target = state.substore_target_name.clone();
    let Some((content, digest, count)) = canonical_artifact(&state) else {
        audit(
            &state,
            &audit_id,
            &target,
            "",
            "no_artifact",
            json!({"action": "preview"}),
        );
        return deny(
            "NO_CANONICAL_ARTIFACT",
            "尚无 canonical 导出，先完成一轮测活",
            &audit_id,
        );
    };
    let canonical = match manual_publish::parse_proxies(&content) {
        Ok(proxies) => proxies,
        Err(error) => {
            audit(
                &state,
                &audit_id,
                &target,
                "",
                &format!("rejected_{}", error.code),
                json!({"action": "preview"}),
            );
            return deny(error.code, &error.message, &audit_id);
        }
    };
    let client = SubStoreClient::new(&state.backend);
    let (current, missing) = match client.sub_record(&target).await {
        Ok(Some(record)) => {
            let text = record
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (
                manual_publish::parse_proxies(&text).unwrap_or_default(),
                false,
            )
        }
        Ok(None) => (Vec::new(), true),
        Err(error) => {
            audit(
                &state,
                &audit_id,
                &target,
                &digest,
                "substore_error",
                json!({"action": "preview"}),
            );
            return deny(
                "SUBSTORE_ERROR",
                &format!("Sub-Store 读取失败: {error}"),
                &audit_id,
            );
        }
    };
    let diff = manual_publish::diff_preview(&current, &canonical);
    audit(
        &state,
        &audit_id,
        &target,
        &digest,
        "preview",
        json!({"action": "preview", "missing_target": missing,
               "added": diff.added, "removed": diff.removed, "renamed": diff.renamed}),
    );
    (
        StatusCode::OK,
        Json(json!({
            "ok": true, "target": target, "digest": digest,
            "node_count": count, "missing_target": missing,
            "preview": diff, "audit_id": audit_id,
        })),
    )
}

/// `POST /api/v1/substore/confirm-token` — 签发 60s 一次性确认单，绑定
/// target+digest+节点数。
async fn confirm_token(State(state): State<SharedState>) -> (StatusCode, Json<Value>) {
    let audit_id = audit_id();
    let target = state.substore_target_name.clone();
    let Some((_, digest, count)) = canonical_artifact(&state) else {
        audit(
            &state,
            &audit_id,
            &target,
            "",
            "no_artifact",
            json!({"action": "confirm-token"}),
        );
        return deny(
            "NO_CANONICAL_ARTIFACT",
            "尚无 canonical 导出，先完成一轮测活",
            &audit_id,
        );
    };
    let issued = tokens().issue(
        &target,
        &digest,
        count,
        std::time::Duration::from_secs(manual_publish::TOKEN_TTL_S),
    );
    audit(
        &state,
        &audit_id,
        &target,
        &digest,
        "confirm_issued",
        json!({"action": "confirm-token", "node_count": count}),
    );
    (
        StatusCode::OK,
        Json(json!({
            "ok": true, "confirm_token": issued.token,
            "expires_in": issued.expires_in, "target": target,
            "digest": digest, "node_count": count, "audit_id": audit_id,
        })),
    )
}

/// `POST /api/v1/substore/push` — 消费 confirm token 后 update-only 推送。
async fn push(
    State(state): State<SharedState>,
    body: Option<Json<Value>>,
) -> (StatusCode, Json<Value>) {
    let audit_id = audit_id();
    let body = body.map(|Json(value)| value).unwrap_or(Value::Null);
    let target = state.substore_target_name.clone();
    let Some((content, digest, count)) = canonical_artifact(&state) else {
        audit(
            &state,
            &audit_id,
            &target,
            "",
            "no_artifact",
            json!({"action": "push"}),
        );
        return deny(
            "NO_CANONICAL_ARTIFACT",
            "尚无 canonical 导出，先完成一轮测活",
            &audit_id,
        );
    };
    let confirm_token = body.get("confirm_token").and_then(Value::as_str);
    if let Err(error) = tokens().consume(confirm_token, &target, &digest, count) {
        audit(
            &state,
            &audit_id,
            &target,
            &digest,
            &format!("rejected_{}", error.code),
            json!({"action": "push"}),
        );
        return deny(error.code, &error.message, &audit_id);
    }
    if let Err(error) = manual_publish::parse_proxies(&content) {
        audit(
            &state,
            &audit_id,
            &target,
            &digest,
            "publish_failed",
            json!({"action": "push", "error_code": error.code}),
        );
        return deny(error.code, &error.message, &audit_id);
    }
    let client = SubStoreClient::new(&state.backend);
    match manual_publish::push_canonical(&client, &target, &content, count).await {
        Ok(pushed) => {
            audit(
                &state,
                &audit_id,
                &target,
                &digest,
                "pushed",
                json!({"action": "push", "count": count, "action_taken": pushed.action}),
            );
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true, "status": "pushed", "target": target,
                    "count": count, "digest": digest,
                    "substore_action": pushed.action, "audit_id": audit_id,
                })),
            )
        }
        Err(error) => {
            audit(
                &state,
                &audit_id,
                &target,
                &digest,
                "publish_failed",
                json!({"action": "push", "error_code": error.code}),
            );
            deny(error.code, &error.message, &audit_id)
        }
    }
}

/// 一次动作的审计 id（uuid 形状，非安全用途）。
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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use axum::http::StatusCode;
    use probe_config::SourceSpec;
    use probe_engine::RoundSettings;
    use probe_mihomo::Controller;
    use probe_storage::Storage;
    use serde_json::{json, Value};

    use super::*;
    use crate::{build_router, AppState, ServeConfig};

    const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// 服务一个带本地 canonical 产物的 state（export_root 里预写
    /// mihomo-alive.yaml + meta）。
    async fn spawn_with_artifact(root: &Path) -> (String, Arc<AppState>) {
        let cfg = probe_config::Config::load(Path::new("/nonexistent/config.json"))
            .expect("default config");
        let state = Arc::new(AppState::new(
            Storage::open_in_memory().unwrap(),
            Some(ADMIN.into()),
            Controller::new("http://127.0.0.1:1", None),
            ServeConfig {
                kernel_config_path: "/test/config.yaml".into(),
                round: RoundSettings::from_config(&cfg),
                core: cfg.core.clone(),
                sources: Vec::<SourceSpec>::new(),
                backend: "http://127.0.0.1:1".into(),
                root: root.to_path_buf(),
                substore: None,
                chain: cfg.chain.clone(),
                publish_prefix: cfg.publish_prefix.clone(),
                publish: cfg.publish.clone(),
                schedule: cfg.schedule.clone(),
                policy: cfg.policy.clone(),
                min_run_interval_seconds: 0,
                substore_allow_delete: false,
                probes: Vec::new(),
                substore_target_name: probe_config::DEFAULT_SUBSTORE_TARGET_NAME.into(),
                aggregation: probe_config::AggregationSection::default(),
                probe_config_digest: String::new(),
            },
        ));
        let export_root = state.publish.export_root(&state.root);
        std::fs::create_dir_all(&export_root).unwrap();
        let body = r#"{"proxies": [{"name": "Tokyo 01", "type": "ss", "server": "s.example.com", "port": 443, "cipher": "aes-128-gcm", "password": "pw"}]}"#;
        std::fs::write(export_root.join(manual_publish_local_file()), body).unwrap();
        let digest = probe_source::content_digest(body);
        std::fs::write(
            export_root.join(manual_publish_meta_file()),
            json!({"count": 1, "digest": digest}).to_string(),
        )
        .unwrap();
        let app = build_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, state)
    }

    async fn post(base: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}{path}"))
            .header("X-Auth-Token", ADMIN)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (
            status,
            response.json::<Value>().await.unwrap_or(Value::Null),
        )
    }

    /// 无产物：三个端点都以 409 NO_CANONICAL_ARTIFACT 拒绝并落审计。
    #[tokio::test]
    async fn without_a_canonical_artifact_every_step_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = probe_config::Config::load(Path::new("/nonexistent/config.json")).unwrap();
        let state = Arc::new(AppState::new(
            Storage::open_in_memory().unwrap(),
            Some(ADMIN.into()),
            Controller::new("http://127.0.0.1:1", None),
            ServeConfig {
                kernel_config_path: "/test/config.yaml".into(),
                round: RoundSettings::from_config(&cfg),
                core: cfg.core.clone(),
                sources: Vec::<SourceSpec>::new(),
                backend: "http://127.0.0.1:1".into(),
                root: tmp.path().to_path_buf(),
                substore: None,
                chain: cfg.chain.clone(),
                publish_prefix: cfg.publish_prefix.clone(),
                publish: cfg.publish.clone(),
                schedule: cfg.schedule.clone(),
                policy: cfg.policy.clone(),
                min_run_interval_seconds: 0,
                substore_allow_delete: false,
                probes: Vec::new(),
                substore_target_name: probe_config::DEFAULT_SUBSTORE_TARGET_NAME.into(),
                aggregation: probe_config::AggregationSection::default(),
                probe_config_digest: String::new(),
            },
        ));
        let app = build_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for path in [
            "/api/v1/substore/preview",
            "/api/v1/substore/confirm-token",
            "/api/v1/substore/push",
        ] {
            let (status, body) = post(&base, path, json!({})).await;
            assert_eq!(status, StatusCode::CONFLICT, "{path}: {body}");
            assert_eq!(body["error"], "NO_CANONICAL_ARTIFACT", "{path}");
        }
        let audits = state
            .storage
            .lock()
            .unwrap()
            .list_publish_audit(10)
            .unwrap();
        assert_eq!(audits.len(), 3, "every refusal is audited");
    }

    /// 三步流程：preview（目标缺失如实呈现）→ confirm-token → push；
    /// token 一次性：复放被 REUSED 拒绝；一切落审计。
    #[tokio::test]
    async fn the_three_step_flow_issues_consumes_and_audits() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, state) = spawn_with_artifact(tmp.path()).await;
        // 1) preview：backend 不可达 → SUBSTORE_ERROR（如实报错，不是 panic）。
        let (status, body) = post(&base, "/api/v1/substore/preview", json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "SUBSTORE_ERROR");
        // 2) confirm-token：签发。
        let (status, body) = post(&base, "/api/v1/substore/confirm-token", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["node_count"], 1);
        assert_eq!(body["expires_in"], 60);
        let token = body["confirm_token"].as_str().unwrap().to_string();
        // 3) push：backend 不可达 → SUBSTORE_ERROR（token 已被正确消费——
        //    重放必须被 REUSED 拒绝，这正是防重放的可观测面）。
        let (status, body) = post(
            &base,
            "/api/v1/substore/push",
            json!({"confirm_token": token}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "SUBSTORE_ERROR");
        // 重放同一 token：SUBSTORE_ERROR 不是 token 错误——但 token 已被
        // 上一次消费……此处直接验证注册表状态：再签一张，推一次，复放。
        let (_, body) = post(&base, "/api/v1/substore/confirm-token", json!({})).await;
        let token2 = body["confirm_token"].as_str().unwrap().to_string();
        let _ = post(
            &base,
            "/api/v1/substore/push",
            json!({"confirm_token": token2.clone()}),
        )
        .await;
        // 第二次 push 用同一张 token：在 consume 处就被拒绝（REUSED），
        // 不再触达 backend——因此错误码是 CONFIRM_TOKEN_REUSED。
        let (status, body) = post(
            &base,
            "/api/v1/substore/push",
            json!({"confirm_token": token2}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "CONFIRM_TOKEN_REUSED");
        let audits = state
            .storage
            .lock()
            .unwrap()
            .list_publish_audit(20)
            .unwrap();
        assert!(
            audits.len() >= 5,
            "preview/confirm/push/reuse all audited: {}",
            audits.len()
        );
    }
}
