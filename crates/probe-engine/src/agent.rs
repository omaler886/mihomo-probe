//! Probe-agent 协议与执行（G-3 收口，HK3 §4.1/§4.2/§13.1/§13.2；
//! `__main__.py` 的 agent 与 `probe_dispatch.dispatch_round` 的 Rust 镜像）。
//!
//! 两侧共用同一线格式（§4.1）：
//!
//! ```json
//! {"round_id": "7", "probe_id": "hk3", "config_digest": "sha256:…",
//!  "deadline": "ISO", "chain_enabled": false,
//!  "nodes": [{"fingerprint": "fp", "mihomo_name": "n1", "proxy": {…}}]}
//! ```
//!
//! * 派发侧（Controller）：`build_jobs` 造每个启用探测点一份 Job →
//!   `dispatch_job` 按 `sign_request` 签名 POST `{endpoint}/jobs`
//!   （Bearer + X-Probe-Timestamp + X-Probe-Signature）。一个探测点不可达
//!   只是「本轮无观测」，绝不让轮次失败（§4.3 观测缺失语义）。
//! * 接收侧（agent）：`verify_job` 复算签名 + 时间窗 + Bearer 常量时间
//!   比较 + JSON 形状校验——顺序与 Python 一致（token 先于 body 字节）。
//! * 回传侧：`post_result` 把 ProbeResult 用 Bearer POST 回
//!   `/api/v1/probes/{id}/jobs/{round}/result`（回传不签名，与 Python 同）。
//! * 执行侧：`JobRunner` 抽象测量；生产实现 `KernelRunner` 用
//!   `probe_mihomo::build_config` + `Controller::reload` + `delay`，per-job
//!   配置写在 `<config_dir>` 下（凭据不进长寿命文件——用完即删由调用方负责，
//!   与 Python 的 tempdir 同义）。`egress_country` 恒 None：出口验证是
//!   Controller 的一次性职责，不是每个探测点各做一遍（§4.1）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use probe_config::ProbeSpec;
use probe_mihomo::Controller;
use serde_json::{json, Value};

use crate::probe_dispatch::{
    sign_request, verify_signature, SIGNATURE_HEADER, SIGNATURE_WINDOW_S, TIMESTAMP_HEADER,
};

/// 一个待测节点（§4.1：探测点重测所需的最小集——账本指纹、内核名、代理本体）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobNode {
    pub fingerprint: Option<String>,
    pub mihomo_name: String,
    pub proxy: Value,
}

/// 一次派发的 Job（线格式）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobWire {
    pub round_id: String,
    pub probe_id: String,
    pub config_digest: String,
    pub deadline: Option<String>,
    pub chain_enabled: bool,
    pub nodes: Vec<JobNode>,
}

impl JobWire {
    pub fn to_json(&self) -> Value {
        json!({
            "round_id": self.round_id,
            "probe_id": self.probe_id,
            "config_digest": self.config_digest,
            "deadline": self.deadline,
            "chain_enabled": self.chain_enabled,
            "nodes": self.nodes.iter().map(|node| json!({
                "fingerprint": node.fingerprint,
                "mihomo_name": node.mihomo_name,
                "proxy": node.proxy,
            })).collect::<Vec<_>>(),
        })
    }

    /// 从原始 JSON 解析并校验一个 Job；形状不合即 BAD_PAYLOAD。
    pub fn from_json(payload: &Value) -> Result<Self, &'static str> {
        let map = payload.as_object().ok_or("Job 不是对象")?;
        let round_id = map
            .get("round_id")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .ok_or("Job 缺少 round_id")?;
        let nodes = map
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or("Job 缺少 nodes 数组")?;
        let mut parsed = Vec::with_capacity(nodes.len());
        for node in nodes {
            let map = node.as_object().ok_or("nodes 成员不是对象")?;
            let mihomo_name = map
                .get("mihomo_name")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .ok_or("节点缺少 mihomo_name")?;
            let proxy = map.get("proxy").cloned().unwrap_or(Value::Null);
            if !proxy.is_object() {
                return Err("节点缺少 proxy 对象");
            }
            parsed.push(JobNode {
                fingerprint: map
                    .get("fingerprint")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                mihomo_name: mihomo_name.to_string(),
                proxy,
            });
        }
        Ok(Self {
            round_id: round_id.to_string(),
            probe_id: map
                .get("probe_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            config_digest: map
                .get("config_digest")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            deadline: map
                .get("deadline")
                .and_then(Value::as_str)
                .map(str::to_string),
            chain_enabled: map
                .get("chain_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            nodes: parsed,
        })
    }
}

/// 一条测量结果（`egress_country` 恒 None——出口验证归 Controller）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProbeResultItem {
    pub fingerprint: String,
    pub alive: bool,
    pub delay_ms: Option<i64>,
    pub reason: Option<String>,
    pub egress_country: Option<String>,
}

/// 回传 payload（§4.2 形状）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProbeResult {
    pub round_id: String,
    pub probe_id: String,
    pub config_digest: String,
    pub started_at: String,
    pub finished_at: String,
    pub results: Vec<ProbeResultItem>,
}

impl ProbeResult {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

// ------------------------------------------------------- 接收侧（agent）

/// agent 对一次 POST /jobs 的拒绝类（HTTP 映射照抄 Python）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentReject {
    /// 401 — Bearer 不对。
    Unauthorized,
    /// 403 — 时间戳非法或窗口外。
    SignatureStale(&'static str),
    /// 403 — 签名不匹配。
    SignatureInvalid,
    /// 400 — JSON 或形状不合法。
    BadPayload(&'static str),
}

impl AgentReject {
    pub fn status(&self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::SignatureStale(_) | Self::SignatureInvalid => 403,
            Self::BadPayload(_) => 400,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::SignatureStale(_) => "SIGNATURE_STALE",
            Self::SignatureInvalid => "SIGNATURE_INVALID",
            Self::BadPayload(_) => "BAD_PAYLOAD",
        }
    }
}

/// agent 的 /jobs 接收门禁，顺序与 Python 一致：Bearer 常量时间比较 →
/// 原始字节签名复核（时间戳在签名材料里）→ JSON 形状。未通过鉴权的调用方
/// 一个字节都不该被读到——调用方（HTTP 层）负责这个顺序，本函数只做纯判定。
pub fn verify_job(
    bearer: Option<&str>,
    ts: Option<&str>,
    signature: Option<&str>,
    body: &[u8],
    secret: &str,
    now_epoch: i64,
) -> Result<JobWire, AgentReject> {
    let Some(bearer) = bearer.map(str::trim).filter(|token| !token.is_empty()) else {
        return Err(AgentReject::Unauthorized);
    };
    // 常量时间形状：比较 sha256 摘要而不是裸 memcmp（与 token_matches 同理，
    // 这里直接用等长 hex 比较——长度差泄露的只有长度）。
    if !constant_time_eq(bearer.as_bytes(), secret.as_bytes()) {
        return Err(AgentReject::Unauthorized);
    }
    let Some(ts) = ts.map(str::trim).filter(|text| !text.is_empty()) else {
        return Err(AgentReject::SignatureStale("缺少合法时间戳"));
    };
    let Ok(ts_value) = ts.parse::<i64>() else {
        return Err(AgentReject::SignatureStale("缺少合法时间戳"));
    };
    if (now_epoch - ts_value).abs() > SIGNATURE_WINDOW_S {
        return Err(AgentReject::SignatureStale("签名时间窗已过（5 分钟）"));
    }
    if !verify_signature(secret, body, ts, signature, SIGNATURE_WINDOW_S, now_epoch) {
        return Err(AgentReject::SignatureInvalid);
    }
    let payload: Value =
        serde_json::from_slice(body).map_err(|_| AgentReject::BadPayload("invalid JSON body"))?;
    let job = JobWire::from_json(&payload).map_err(AgentReject::BadPayload)?;
    Ok(job)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

// ------------------------------------------------------- 派发侧（Controller）

/// 为每个启用的探测点造一份 Job（§4.1：一探测点一份轻量任务）。
pub fn build_jobs(
    probes: &[ProbeSpec],
    round_id: i64,
    config_digest: &str,
    deadline: Option<&str>,
    chain_enabled: bool,
    nodes: Vec<JobNode>,
) -> Vec<JobWire> {
    probes
        .iter()
        .filter(|probe| probe.enabled)
        .map(|probe| JobWire {
            round_id: round_id.to_string(),
            probe_id: probe.id.clone(),
            config_digest: config_digest.to_string(),
            deadline: deadline.map(str::to_string),
            chain_enabled,
            nodes: nodes.clone(),
        })
        .collect()
}

/// 把一个 Job 签名后 POST 到探测点的 `{endpoint}/jobs`。
/// 返回 Err(原因) = 该探测点本轮无观测（观测缺失语义，调用方只记日志）。
pub async fn dispatch_job(job: &JobWire, endpoint: &str, secret: &str) -> Result<(), String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        return Err("探测点未配置 endpoint".to_string());
    }
    let url = format!("{endpoint}/jobs");
    let body = serde_json::to_vec(&job.to_json()).unwrap_or_default();
    let ts = now_epoch();
    let signature = sign_request(secret, &body, ts);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| format!("http client: {error}"))?;
    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {secret}"))
        .header(TIMESTAMP_HEADER, ts.to_string())
        .header(SIGNATURE_HEADER, signature)
        .body(body)
        .send()
        .await
        .map_err(|error| format!("无响应（{error}）"))?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("HTTP {status}"))
    }
}

/// 把 ProbeResult 回传给 Controller（§4.2 回传路由，Bearer，不签名）。
pub async fn post_result(
    controller: &str,
    probe_id: &str,
    token: &str,
    result: &ProbeResult,
) -> Result<u16, String> {
    let url = format!(
        "{}/api/v1/probes/{probe_id}/jobs/{}/result",
        controller.trim().trim_end_matches('/'),
        result.round_id
    );
    let body = serde_json::to_vec(&result.to_json()).unwrap_or_default();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("http client: {error}"))?;
    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .body(body)
        .send()
        .await
        .map_err(|error| format!("回传失败（{error}）"))?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        Ok(status)
    } else {
        let detail = response.text().await.unwrap_or_default();
        Err(format!(
            "HTTP {status}: {}",
            detail.chars().take(200).collect::<String>()
        ))
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ------------------------------------------------------- 执行侧（测量）

/// 一次 Job 的测量器。生产实现走内核；测试注入假实现。装箱 future 让
/// `Arc<dyn JobRunner>` 可用（agent 服务进程持有任意实现）。
pub trait JobRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        job: &'a JobWire,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ProbeResultItem>> + Send + 'a>>;
}

/// 生产测量器：prepare → build_config → reload → delay×N。
///
/// 与 Python 的差异（有意）：Python 每个 Job 起停一个内核容器；这里复用
/// agent 常驻的单内核（external-controller + secret 指向它），每 Job
/// `PUT /configs` 重载——busy 门禁保证同一时刻只有一个 Job，重载语义等价，
/// 省掉每轮容器起停。`kernel` 与 agent 必须同机（reload 传的是内核侧路径）。
pub struct KernelRunner {
    pub controller: Arc<Controller>,
    pub core: probe_config::CoreConfig,
    pub secret: String,
    /// 每轮 Job 的内核配置落盘目录（agent 与内核共享的路径视角）。
    pub config_dir: PathBuf,
    pub test_url: String,
    pub timeout_ms: u64,
    pub concurrency: usize,
}

impl KernelRunner {
    /// 把 Job 的节点写成内核配置并重载；返回 (按内核名的准备结果, 失败项)。
    async fn prepare_kernel(&self, job: &JobWire) -> Result<Vec<(String, Option<String>)>, String> {
        let entries: Vec<serde_json::Value> = job
            .nodes
            .iter()
            .map(|node| {
                let mut proxy = node.proxy.clone();
                proxy["name"] = Value::String(node.mihomo_name.clone());
                json!({
                    "name": node.mihomo_name,
                    "fingerprint": node.fingerprint,
                    "proxy": proxy,
                })
            })
            .collect();
        // 内核配置用 Controller 指定的名字（§4.1：Job 是本轮命名的唯一来源）。
        let proxies: Vec<Value> = entries.iter().map(|entry| entry["proxy"].clone()).collect();
        if proxies.is_empty() {
            return Err("Job 无节点".to_string());
        }
        let config = probe_mihomo::build_config(&self.core, &self.secret, &proxies);
        std::fs::create_dir_all(&self.config_dir)
            .map_err(|error| format!("config dir: {error}"))?;
        let path = self
            .config_dir
            .join(format!("agent-job-{}.yaml", job.round_id));
        std::fs::write(&path, config).map_err(|error| format!("config write: {error}"))?;
        let reloaded = self
            .controller
            .reload(&path.to_string_lossy())
            .await
            .map_err(|error| format!("kernel reload: {error}"))?;
        if !reloaded {
            return Err("kernel refused the job config".to_string());
        }
        Ok(job
            .nodes
            .iter()
            .map(|node| (node.mihomo_name.clone(), node.fingerprint.clone()))
            .collect())
    }
}

impl JobRunner for KernelRunner {
    fn run<'a>(
        &'a self,
        job: &'a JobWire,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ProbeResultItem>> + Send + 'a>>
    {
        Box::pin(self.run_inner(job))
    }
}

impl KernelRunner {
    async fn run_inner(&self, job: &JobWire) -> Vec<ProbeResultItem> {
        let by_name = match self.prepare_kernel(job).await {
            Ok(by_name) => by_name,
            Err(reason) => {
                // 整轮测量无法开始：每个节点一条失败结果（不是静默丢）。
                return job
                    .nodes
                    .iter()
                    .map(|node| ProbeResultItem {
                        fingerprint: node
                            .fingerprint
                            .clone()
                            .unwrap_or_else(|| node.mihomo_name.clone()),
                        alive: false,
                        delay_ms: None,
                        reason: Some(reason.clone()),
                        egress_country: None,
                    })
                    .collect();
            }
        };
        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.concurrency.max(1)));
        let mut tasks = Vec::with_capacity(by_name.len());
        for (mihomo_name, fingerprint) in by_name {
            let controller = Arc::clone(&self.controller);
            let semaphore = Arc::clone(&semaphore);
            let test_url = self.test_url.clone();
            let timeout_ms = self.timeout_ms;
            tasks.push(tokio::spawn(async move {
                let _permit = semaphore.acquire().await;
                let outcome = controller
                    .delay(&mihomo_name, &test_url, timeout_ms, "")
                    .await;
                let item = match outcome {
                    Ok(probe_mihomo::DelayOutcome::Ok(delay_ms)) => ProbeResultItem {
                        fingerprint: fingerprint.unwrap_or_else(|| mihomo_name.clone()),
                        alive: true,
                        delay_ms: Some(i64::from(delay_ms)),
                        reason: None,
                        egress_country: None,
                    },
                    // 内核答「测不过」与传输错误同归失败：reason 带内文，
                    // delay 不落（Python delay 的 (None, reason) 口径）。
                    Ok(probe_mihomo::DelayOutcome::Failed { kind, message, .. }) => {
                        ProbeResultItem {
                            fingerprint: fingerprint.unwrap_or_else(|| mihomo_name.clone()),
                            alive: false,
                            delay_ms: None,
                            reason: Some(if message.is_empty() { kind } else { message }),
                            egress_country: None,
                        }
                    }
                    Err(error) => ProbeResultItem {
                        fingerprint: fingerprint.unwrap_or_else(|| mihomo_name.clone()),
                        alive: false,
                        delay_ms: None,
                        reason: Some(error.to_string()),
                        egress_country: None,
                    },
                };
                (mihomo_name, item)
            }));
        }
        let mut items: Vec<(String, ProbeResultItem)> = Vec::new();
        for task in tasks {
            if let Ok(pair) = task.await {
                items.push(pair);
            }
        }
        // 稳定顺序：按内核名（mihomo_name 在 Job 内唯一）。
        items.sort_by(|left, right| left.0.cmp(&right.0));
        items.into_iter().map(|(_, item)| item).collect()
    }
}

/// 跑完一个 Job 并组回传 payload（Python `_execute_job` 的组装部分）。
pub async fn execute_job(
    runner: &dyn JobRunner,
    job: &JobWire,
    started_at: String,
    finished_at: String,
) -> ProbeResult {
    let results = runner.run(job).await;
    ProbeResult {
        round_id: job.round_id.clone(),
        probe_id: job.probe_id.clone(),
        config_digest: job.config_digest.clone(),
        started_at,
        finished_at,
        results,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::probe_dispatch::sign_request;

    fn job_json() -> Value {
        json!({
            "round_id": "7",
            "probe_id": "hk3",
            "config_digest": "sha256:abc",
            "deadline": "2026-11-08T00:00:00",
            "chain_enabled": false,
            "nodes": [
                {"fingerprint": "fp-1", "mihomo_name": "n1", "proxy": {"name": "x", "type": "ss", "server": "s", "port": 443}},
                {"fingerprint": null, "mihomo_name": "n2", "proxy": {"name": "y", "type": "ss", "server": "s2", "port": 444}}
            ],
        })
    }

    /// 接收门禁的顺序与拒绝类：未鉴权 → 签名 → 形状。
    #[test]
    fn the_job_gate_orders_auth_signature_shape() {
        let body = serde_json::to_vec(&job_json()).unwrap();
        let secret = "probe-secret";
        let ts = now_epoch();
        let signature = sign_request(secret, &body, ts);

        // 全对 → Job。
        let job = verify_job(
            Some(secret),
            Some(&ts.to_string()),
            Some(&signature),
            &body,
            secret,
            ts,
        )
        .unwrap();
        assert_eq!(job.round_id, "7");
        assert_eq!(job.nodes.len(), 2);
        assert_eq!(job.nodes[0].mihomo_name, "n1");

        // 无 Bearer / 错 Bearer → 401（错 token 与长 token 同类）。
        assert_eq!(
            verify_job(
                None,
                Some(&ts.to_string()),
                Some(&signature),
                &body,
                secret,
                ts
            )
            .unwrap_err()
            .status(),
            401
        );
        assert_eq!(
            verify_job(
                Some("wrong"),
                Some(&ts.to_string()),
                Some(&signature),
                &body,
                secret,
                ts
            )
            .unwrap_err()
            .status(),
            401
        );

        // 缺时间戳 / 非数字 / 窗口外 → 403 SIGNATURE_STALE。
        assert_eq!(
            verify_job(Some(secret), None, Some(&signature), &body, secret, ts)
                .unwrap_err()
                .status(),
            403
        );
        assert_eq!(
            verify_job(
                Some(secret),
                Some("abc"),
                Some(&signature),
                &body,
                secret,
                ts
            )
            .unwrap_err()
            .status(),
            403
        );
        assert_eq!(
            verify_job(
                Some(secret),
                Some(&(ts - SIGNATURE_WINDOW_S - 1).to_string()),
                Some(&signature),
                &body,
                secret,
                ts
            )
            .unwrap_err()
            .status(),
            403
        );

        // 签名不匹配（body 被篡改）→ 403 SIGNATURE_INVALID。
        let tampered: Vec<u8> = body
            .iter()
            .map(|byte| if *byte == b'7' { b'8' } else { *byte })
            .collect();
        assert_eq!(
            verify_job(
                Some(secret),
                Some(&ts.to_string()),
                Some(&signature),
                &tampered,
                secret,
                ts
            )
            .unwrap_err()
            .status(),
            403
        );

        // 签名对但形状坏 → 400 BAD_PAYLOAD。
        let broken = br#"{"round_id": "7"}"#;
        let signature_broken = sign_request(secret, broken, ts);
        assert_eq!(
            verify_job(
                Some(secret),
                Some(&ts.to_string()),
                Some(&signature_broken),
                broken,
                secret,
                ts
            )
            .unwrap_err()
            .status(),
            400
        );
    }

    /// build_jobs：每个启用探测点一份，禁用的没有；节点克隆互不共享。
    #[test]
    fn build_jobs_covers_only_enabled_probes() {
        let probes = vec![
            ProbeSpec {
                id: "p1".into(),
                enabled: true,
                ..ProbeSpec::default()
            },
            ProbeSpec {
                id: "p2".into(),
                enabled: false,
                ..ProbeSpec::default()
            },
        ];
        let nodes = vec![JobNode {
            fingerprint: Some("fp".into()),
            mihomo_name: "n1".into(),
            proxy: json!({"name": "n1"}),
        }];
        let jobs = build_jobs(&probes, 7, "sha256:x", Some("d"), false, nodes);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].probe_id, "p1");
        assert_eq!(jobs[0].round_id, "7");
        assert_eq!(jobs[0].nodes.len(), 1);
    }

    /// KernelRunner：delay 结果映射（alive/delay/reason），semaphore 限并发。
    /// 这里用假 Controller 不行——Controller 是具体类型；改用 JobRunner 的
    /// 假实现测 execute_job 的组装，KernelRunner 的内核路径由 stub HTTP 钉。
    struct FakeRunner(Vec<ProbeResultItem>);

    impl JobRunner for FakeRunner {
        fn run<'a>(
            &'a self,
            _job: &'a JobWire,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ProbeResultItem>> + Send + 'a>>
        {
            Box::pin(async { self.0.clone() })
        }
    }

    #[tokio::test]
    async fn execute_job_assembles_the_result_payload() {
        let job = JobWire::from_json(&job_json()).unwrap();
        let runner = FakeRunner(vec![ProbeResultItem {
            fingerprint: "fp-1".into(),
            alive: true,
            delay_ms: Some(120),
            reason: None,
            egress_country: None,
        }]);
        let result = execute_job(&runner, &job, "s".into(), "f".into()).await;
        assert_eq!(result.round_id, "7");
        assert_eq!(result.probe_id, "hk3");
        assert_eq!(result.config_digest, "sha256:abc");
        assert_eq!(result.results.len(), 1);
        assert_eq!(
            result.results[0].egress_country, None,
            "出口验证归 Controller"
        );
        let payload = result.to_json();
        assert_eq!(payload["results"][0]["alive"], true);
    }

    /// dispatch_job 对不可达端点只报观测缺失（不 panic、不重试到死）。
    #[tokio::test]
    async fn dispatch_to_a_dead_endpoint_reports_missing_observation() {
        let job = JobWire::from_json(&job_json()).unwrap();
        let error = dispatch_job(&job, "http://127.0.0.1:1", "secret")
            .await
            .unwrap_err();
        assert!(
            error.contains("无响应") || error.contains("HTTP"),
            "{error}"
        );
    }

    /// post_result 对不可达 Controller 同样只是错误返回。
    #[tokio::test]
    async fn posting_a_result_to_a_dead_controller_fails_softly() {
        let result = ProbeResult {
            round_id: "7".into(),
            probe_id: "hk3".into(),
            config_digest: "d".into(),
            started_at: "s".into(),
            finished_at: "f".into(),
            results: Vec::new(),
        };
        assert!(post_result("http://127.0.0.1:1", "hk3", "t", &result)
            .await
            .is_err());
    }
}
