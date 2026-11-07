//! 人工 Sub-Store 推送三件套的核心（G-4，HK3 §0.6/§9.4/§13.5；
//! `mihomo_test/manual_publish.py` 的 Rust 镜像）。
//!
//! P5 之后 Sub-Store 的**唯一**写路径是：
//!
//! ```text
//! 预览（只读 diff）→ confirm-token（60s 一次性，绑定 target+digest+节点数）
//! → push（校验并消费 token → update-only 更新目标对象）→ publish_audit
//! ```
//!
//! 全部人工触发、全部落审计。自动回灌已从引擎删除——本模块是仅存的写
//! 入口，且 update-only：目标对象不存在一律报错，绝不新建。稳定错误码
//! （§10 的扩展，HTTP 映射在 API 层）。token 注册表在进程内存中：60s TTL
//! 下重启丢 token 只是让操作员重按一次预览，不构成安全问题；审计在 DB，
//! 跨重启可查。样例输出只含 name/type——server/uuid/password 等 §13.7
//! 敏感字段不进预览、不进日志。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// 一次性确认 token 的有效期（秒）。
pub const TOKEN_TTL_S: u64 = 60;

/// 预览样例的上限（added/removed/renamed 各取前 20 条，§9.4）。
pub const SAMPLE_LIMIT: usize = 20;

/// 稳定错误码（§10 扩展；HTTP 映射在 API 层照抄 Python `V1_ERROR_STATUS`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualPublishError {
    pub code: &'static str,
    pub message: String,
}

impl ManualPublishError {
    fn of(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

// ------------------------------------------------- confirm token（防重放）

/// 一张已签发的确认单。
#[derive(Debug, Clone)]
struct TokenEntry {
    target: String,
    digest: String,
    node_count: usize,
    expires_at: Instant,
    used: bool,
}

/// 进程内 confirm-token 注册表：一次性、绑定 (target, digest, node_count)、
/// 防重放。API 层持有一份全局实例。
#[derive(Default)]
pub struct ConfirmTokens {
    entries: Mutex<HashMap<String, TokenEntry>>,
}

/// 一次签发的结果（响应体形状与 Python `issue_confirm_token` 一致）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct IssuedToken {
    pub token: String,
    pub expires_in: u64,
    pub target: String,
    pub digest: String,
    pub node_count: usize,
}

impl ConfirmTokens {
    /// 签发一张 60s 一次性确认单，绑定 (target, digest, node_count)。同
    /// target 的一切旧 token 立刻作废：操作员重新预览即重新签发，不允许
    /// 两张并行有效的确认单指向同一目标。
    pub fn issue(
        &self,
        target: &str,
        digest: &str,
        node_count: usize,
        ttl: Duration,
    ) -> IssuedToken {
        let token = random_token();
        let mut entries = self.entries.lock().expect("token registry poisoned");
        for entry in entries.values_mut() {
            if entry.target == target {
                entry.used = true;
            }
        }
        entries.insert(
            token.clone(),
            TokenEntry {
                target: target.to_string(),
                digest: digest.to_string(),
                node_count,
                expires_at: Instant::now() + ttl,
                used: false,
            },
        );
        IssuedToken {
            token,
            expires_in: ttl.as_secs(),
            target: target.to_string(),
            digest: digest.to_string(),
            node_count,
        }
    }

    /// 消费一张 token，或给出稳定错误码。四类拒绝各归其位（§9.4）。
    pub fn consume(
        &self,
        token: Option<&str>,
        target: &str,
        digest: &str,
        node_count: usize,
    ) -> Result<(), ManualPublishError> {
        let mut entries = self.entries.lock().expect("token registry poisoned");
        let key = token.unwrap_or("").trim().to_string();
        let Some(entry) = entries.get_mut(&key) else {
            return Err(ManualPublishError::of(
                "PUSH_CONFIRM_REQUIRED",
                "缺少有效的确认 token，请先 POST /api/v1/substore/confirm-token",
            ));
        };
        if entry.used {
            return Err(ManualPublishError::of(
                "CONFIRM_TOKEN_REUSED",
                "确认 token 已被使用（防重放）",
            ));
        }
        if Instant::now() >= entry.expires_at {
            entry.used = true;
            return Err(ManualPublishError::of(
                "CONFIRM_TOKEN_EXPIRED",
                "确认 token 已过期（60s），请重新预览签发",
            ));
        }
        if entry.target != target || entry.digest != digest || entry.node_count != node_count {
            return Err(ManualPublishError::of(
                "CONFIRM_TOKEN_MISMATCH",
                "目标/digest/节点数与签发时不一致，canonical 已变化，请重新预览",
            ));
        }
        entry.used = true;
        Ok(())
    }

    /// 测试钩子：清空全部未决 token。
    pub fn reset(&self) {
        self.entries
            .lock()
            .expect("token registry poisoned")
            .clear();
    }
}

/// URL-safe 随机 token。Python 用 `secrets.token_urlsafe(32)`；Rust 侧取
/// 32 字节熵并 hex 化（长度相当，用途相同——一次性确认单，非长期凭据）。
fn random_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut entropy = [0u8; 16];
    let _ = getrandom::fill(&mut entropy);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hex = String::with_capacity(64);
    for byte in entropy
        .iter()
        .chain(nanos.to_be_bytes().iter())
        .chain(counter.to_be_bytes().iter())
    {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

// ------------------------------------------------------------- preview

/// ClashMeta YAML（或 JSON）→ proxy 数组；其余形状一律 BAD_CONTENT。
pub fn parse_proxies(content_text: &str) -> Result<Vec<Value>, ManualPublishError> {
    let data: Value = serde_norway::from_str(content_text)
        .or_else(|_| serde_json::from_str(content_text))
        .map_err(|error| ManualPublishError::of("BAD_CONTENT", format!("解析失败: {error}")))?;
    let proxies = data.get("proxies").and_then(Value::as_array);
    match proxies {
        Some(items) => Ok(items
            .iter()
            .filter(|item| item.is_object())
            .cloned()
            .collect()),
        None => Err(ManualPublishError::of(
            "BAD_CONTENT",
            "内容缺少 proxies 数组",
        )),
    }
}

/// 样例只带展示字段——§13.7 的连接凭据永不进预览/日志。
fn sample(proxy: &Value) -> Value {
    json!({
        "name": proxy.get("name").and_then(Value::as_str),
        "type": proxy.get("type").and_then(Value::as_str),
    })
}

/// What the push would change, identity = 连接参数指纹。改名绝不能呈现成
/// 删除+新增对（同指纹不同显示名 → renamed）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiffPreview {
    pub added: usize,
    pub removed: usize,
    pub renamed: usize,
    pub unchanged: usize,
    pub current_count: usize,
    pub canonical_count: usize,
    pub samples: DiffSamples,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiffSamples {
    pub added: Vec<Value>,
    pub removed: Vec<Value>,
    pub renamed: Vec<Value>,
}

pub fn diff_preview(current: &[Value], canonical: &[Value]) -> DiffPreview {
    let by_fingerprint = |proxies: &[Value]| -> std::collections::BTreeMap<String, Value> {
        let mut map = std::collections::BTreeMap::new();
        for proxy in proxies {
            map.entry(probe_source::identity::fingerprint_proxy(proxy))
                .or_insert_with(|| proxy.clone());
        }
        map
    };
    let current_by_fp = by_fingerprint(current);
    let canonical_by_fp = by_fingerprint(canonical);
    let added: Vec<&String> = canonical_by_fp
        .keys()
        .filter(|fp| !current_by_fp.contains_key(*fp))
        .collect();
    let removed: Vec<&String> = current_by_fp
        .keys()
        .filter(|fp| !canonical_by_fp.contains_key(*fp))
        .collect();
    let renamed: Vec<Value> = canonical_by_fp
        .iter()
        .filter(|(fp, canonical)| {
            current_by_fp
                .get(*fp)
                .is_some_and(|current| current.get("name") != canonical.get("name"))
        })
        .map(|(fp, canonical)| {
            json!({
                "from": current_by_fp.get(fp.as_str()).and_then(|p| p.get("name")).and_then(Value::as_str),
                "to": canonical.get("name").and_then(Value::as_str),
            })
        })
        .collect();
    DiffPreview {
        added: added.len(),
        removed: removed.len(),
        renamed: renamed.len(),
        unchanged: canonical_by_fp.len() - added.len() - renamed.len(),
        current_count: current_by_fp.len(),
        canonical_count: canonical_by_fp.len(),
        samples: DiffSamples {
            added: added
                .iter()
                .take(SAMPLE_LIMIT)
                .map(|fp| sample(&canonical_by_fp[*fp]))
                .collect(),
            removed: removed
                .iter()
                .take(SAMPLE_LIMIT)
                .map(|fp| sample(&current_by_fp[*fp]))
                .collect(),
            renamed: renamed.into_iter().take(SAMPLE_LIMIT).collect(),
        },
    }
}

// ---------------------------------------------------------------- push

/// 一次成功推送的结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Pushed {
    pub status: &'static str,
    pub target: String,
    pub count: usize,
    /// Sub-Store admin 的动作拼写（"updated"）。
    pub action: String,
}

/// update-only 把 canonical 内容写进 `target`；绝不新建对象。目标不存在
/// → SUBSTORE_TARGET_MISSING（调用方落审计后原样拒绝）。
pub async fn push_canonical<A: probe_source::SubAdmin>(
    admin: &A,
    target: &str,
    content_text: &str,
    count: usize,
) -> Result<Pushed, ManualPublishError> {
    let payload = json!({
        "name": target,
        "displayName": format!("mihomo-probe alive ({count})"),
        "source": "local",
        "url": "",
        "content": content_text,
        "mergeSources": "",
        "ignoreFailedRemoteSub": "quiet",
        "passThroughUA": false,
        "process": [],
    });
    match admin.update_sub(target, &payload).await {
        Ok(action) => Ok(Pushed {
            status: "pushed",
            target: target.to_string(),
            count,
            action,
        }),
        Err(error)
            if error.contains("not found")
                || error.contains("404")
                || error.contains("does not exist") =>
        {
            Err(ManualPublishError::of(
                "SUBSTORE_TARGET_MISSING",
                format!("Sub-Store 对象 {target} 不存在；本服务绝不新建，请先在 Sub-Store 绑定"),
            ))
        }
        Err(error) => Err(ManualPublishError::of(
            "SUBSTORE_ERROR",
            format!("Sub-Store 更新失败: {error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proxy(name: &str, server: &str, port: i64) -> Value {
        json!({"name": name, "type": "ss", "server": server, "port": port,
               "cipher": "aes-128-gcm", "password": "pw"})
    }

    /// token 的一次性 + 绑定 + 同 target 作废旧单。
    #[test]
    fn the_confirm_token_is_one_shot_and_bound() {
        let registry = ConfirmTokens::default();
        let issued = registry.issue("t1", "digest-1", 5, Duration::from_secs(60));
        // 绑定不匹配 → MISMATCH。
        let error = registry
            .consume(Some(&issued.token), "t1", "digest-2", 5)
            .unwrap_err();
        assert_eq!(error.code, "CONFIRM_TOKEN_MISMATCH");
        // 正确消费一次。
        registry
            .consume(Some(&issued.token), "t1", "digest-1", 5)
            .unwrap();
        // 重放 → REUSED。
        let error = registry
            .consume(Some(&issued.token), "t1", "digest-1", 5)
            .unwrap_err();
        assert_eq!(error.code, "CONFIRM_TOKEN_REUSED");
        // 同 target 重签：旧单作废（哪怕未消费）。
        let first = registry.issue("t2", "d", 1, Duration::from_secs(60));
        let _second = registry.issue("t2", "d", 1, Duration::from_secs(60));
        let error = registry
            .consume(Some(&first.token), "t2", "d", 1)
            .unwrap_err();
        assert_eq!(error.code, "CONFIRM_TOKEN_REUSED");
        // 未知 token → REQUIRED。
        let error = registry.consume(Some("nope"), "t2", "d", 1).unwrap_err();
        assert_eq!(error.code, "PUSH_CONFIRM_REQUIRED");
        registry.reset();
    }

    /// 过期 → EXPIRED（消费掉，防重放窗口不留活口）。
    #[test]
    fn an_expired_token_is_consumed_and_rejected() {
        let registry = ConfirmTokens::default();
        let issued = registry.issue("t", "d", 1, Duration::from_millis(0));
        std::thread::sleep(Duration::from_millis(2));
        let error = registry
            .consume(Some(&issued.token), "t", "d", 1)
            .unwrap_err();
        assert_eq!(error.code, "CONFIRM_TOKEN_EXPIRED");
        let error = registry
            .consume(Some(&issued.token), "t", "d", 1)
            .unwrap_err();
        assert_eq!(error.code, "CONFIRM_TOKEN_REUSED");
        registry.reset();
    }

    /// BAD_CONTENT：非 YAML/JSON、缺 proxies、proxies 非数组。
    #[test]
    fn content_without_a_proxies_array_is_bad_content() {
        for hostile in ["", "not: [valid", "[1, 2, 3]", "{\"items\": []}"] {
            let error = parse_proxies(hostile).unwrap_err();
            assert_eq!(error.code, "BAD_CONTENT", "{hostile}");
        }
        // 合法 YAML 与合法 JSON 同权。
        assert_eq!(
            parse_proxies("proxies:\n  - name: a\n    type: ss\n")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            parse_proxies(r#"{"proxies": [{"name": "a", "type": "ss"}]}"#)
                .unwrap()
                .len(),
            1
        );
    }

    /// diff_preview：added/removed/renamed/unchanged 的指纹口径，改名
    /// 绝不呈现成删除+新增对；样例只有 name/type。
    #[test]
    fn the_preview_folds_renames_into_the_renamed_bucket() {
        let current = vec![
            proxy("Tokyo 01", "s1.example.com", 443),
            proxy("Dead", "s2.example.com", 444),
        ];
        let canonical = vec![
            proxy("Tokyo 01 · 改名", "s1.example.com", 443),
            proxy("New", "s3.example.com", 445),
        ];
        let preview = diff_preview(&current, &canonical);
        assert_eq!(preview.added, 1);
        assert_eq!(preview.removed, 1);
        assert_eq!(preview.renamed, 1);
        assert_eq!(preview.unchanged, 0);
        assert_eq!(preview.canonical_count, 2);
        assert_eq!(preview.samples.renamed[0]["from"], "Tokyo 01");
        assert_eq!(preview.samples.renamed[0]["to"], "Tokyo 01 · 改名");
        // 样例无凭据字段。
        for sample_value in preview
            .samples
            .added
            .iter()
            .chain(preview.samples.removed.iter())
        {
            assert!(sample_value.get("password").is_none());
            assert!(sample_value.get("server").is_none());
        }
    }

    /// push_canonical 的 update-only 语义：目标缺失拒绝、绝不新建。
    #[tokio::test]
    async fn the_push_is_update_only_and_never_creates() {
        struct RecordingAdmin {
            calls: Mutex<Vec<(String, bool)>>,
        }
        impl probe_source::SubAdmin for RecordingAdmin {
            fn upsert_sub<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a Value,
            ) -> probe_source::BoxFuture<'a, Result<String, String>> {
                Box::pin(async { Err("upsert must never be called".to_string()) })
            }
            fn delete_sub<'a>(
                &'a self,
                _name: &'a str,
            ) -> probe_source::BoxFuture<'a, Result<bool, String>> {
                Box::pin(async { Err("delete must never be called".to_string()) })
            }
            fn update_sub<'a>(
                &'a self,
                name: &'a str,
                _payload: &'a Value,
            ) -> probe_source::BoxFuture<'a, Result<String, String>> {
                Box::pin(async move {
                    self.calls.lock().unwrap().push((name.to_string(), true));
                    Ok("updated".to_string())
                })
            }
        }
        let admin = RecordingAdmin {
            calls: Mutex::new(Vec::new()),
        };
        let pushed = push_canonical(&admin, "mihomo-probe-alive", "proxies: []", 0)
            .await
            .unwrap();
        assert_eq!(pushed.status, "pushed");
        assert_eq!(pushed.action, "updated");
        assert_eq!(pushed.target, "mihomo-probe-alive");
        // upsert 从未被调用：唯一写路径就是 update-only。
        assert_eq!(admin.calls.lock().unwrap().len(), 1);

        struct MissingAdmin;
        impl probe_source::SubAdmin for MissingAdmin {
            fn upsert_sub<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a Value,
            ) -> probe_source::BoxFuture<'a, Result<String, String>> {
                Box::pin(async { Err("no".to_string()) })
            }
            fn delete_sub<'a>(
                &'a self,
                _name: &'a str,
            ) -> probe_source::BoxFuture<'a, Result<bool, String>> {
                Box::pin(async { Ok(false) })
            }
            fn update_sub<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a Value,
            ) -> probe_source::BoxFuture<'a, Result<String, String>> {
                Box::pin(async { Err("sub not found (404)".to_string()) })
            }
        }
        let error = push_canonical(&MissingAdmin, "t", "x", 1)
            .await
            .unwrap_err();
        assert_eq!(error.code, "SUBSTORE_TARGET_MISSING");
        assert!(error.message.contains("绝不新建"));

        struct BrokenAdmin;
        impl probe_source::SubAdmin for BrokenAdmin {
            fn upsert_sub<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a Value,
            ) -> probe_source::BoxFuture<'a, Result<String, String>> {
                Box::pin(async { Err("no".to_string()) })
            }
            fn delete_sub<'a>(
                &'a self,
                _name: &'a str,
            ) -> probe_source::BoxFuture<'a, Result<bool, String>> {
                Box::pin(async { Ok(false) })
            }
            fn update_sub<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a Value,
            ) -> probe_source::BoxFuture<'a, Result<String, String>> {
                Box::pin(async { Err("backend exploded".to_string()) })
            }
        }
        let error = push_canonical(&BrokenAdmin, "t", "x", 1).await.unwrap_err();
        assert_eq!(error.code, "SUBSTORE_ERROR");
    }
}
