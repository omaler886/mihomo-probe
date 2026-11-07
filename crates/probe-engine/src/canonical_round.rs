//! Canonical 四文件的 round 流程接线（workstreams/27，G-2 集成步骤 (a)）。
//!
//! 收敛后的 round 在本地管线提交后装配 canonical：`select_source_exports`
//! 的入选节点（与逐源产物同一 selection，永不另立口径）→ `build_canonical`
//! （G-1 模型）→ 四文件 → 调用方给的 [`FilesPublisher`]（Gist 或 WebDAV）。
//!
//! 护栏语义与 `publish_round` 完全一致：publish 关闭 / `PreservePreviousState`
//! / 未测量过的 inconclusive 轮 → 不装配、不请求，上一稳定四文件站住
//! （§11.2-4）。发布是**尽力而为**：结果如实返回、记日志，但绝不改动本轮
//! 节点状态（Python `_publish_gist` 的 fire-and-forget 契约）。
//!
//! 本地 parity：canonical 正文与 meta 同步写到 `<export_root>/mihomo-alive.yaml`
//! 与 `mihomo-alive.meta.json`（Python P4 的 G-1 落盘形态），没有远程发布通道
//! 的部署也能从本地文件消费 canonical。
//!
//! digest「只进不退」由调用方执行（[`crate::canonical_publish::digest_advances`]）；
//! 幂等门禁本体钉在远端内容的 sha 上（[`FilesPublisher`]），不依赖本地 digest。

use std::collections::BTreeMap;
use std::path::Path;

use probe_config::PublishSection;
use probe_publish::FilesPublisher;
use serde_json::{json, Value};

use crate::canonical::{build_canonical, CanonicalArtifact, NodeMeta, RenameConfig, SourceInput};
use crate::canonical_publish::CanonicalFiles;
use crate::collect::Collected;
use crate::publish::select_source_exports;
use crate::round::{GuardDecision, NodeVerdict, RoundCounts, Verdict};

/// 本地 canonical 产物文件名（与 Python P4 的 `data/exports/` 形态同名）。
pub const LOCAL_CANONICAL_FILE: &str = "mihomo-alive.yaml";
/// 本地 canonical meta 侧车文件名。
pub const LOCAL_META_FILE: &str = "mihomo-alive.meta.json";

/// history 文件保留的轮数（Python `gist_store.HISTORY_LIMIT`）。
pub const HISTORY_LIMIT: usize = 100;

/// 「本轮没有可发布的 canonical」（护栏/关闭/空产物/无远程通道）。独立拼写：
/// 跳过不是失败，也不是 no_change。
pub const SKIPPED: &str = "skipped";

/// provider 决定的 canonical 远程目的地。Gist/Composite → Gist，
/// Webdav → WebDAV，Local → 无（`resolve` 返回 `None`）。一个枚举让调用方
/// 不必为每个目的地复制一遍调用样板。
#[derive(Debug, Clone)]
pub enum CanonicalPublisher {
    Gist(probe_publish::GistPublisher),
    Webdav(probe_publish::WebDavPublisher),
}

impl CanonicalPublisher {
    /// 从 publish 段解析目的地；`None` = 本地部署无远程通道。构造失败
    /// （token/密码文件不可用）以 `Err` 上抛——与逐源远程腿同一处理路径。
    /// 成功时同时给出发布目标（Gist 的 gist id；WebDAV 忽略目标，传空串）。
    pub fn resolve(
        publish: &PublishSection,
    ) -> Option<Result<(Self, String), probe_publish::PublishError>> {
        use probe_config::PublishProvider;
        match publish.provider {
            PublishProvider::Local => None,
            PublishProvider::Gist | PublishProvider::Composite => {
                let target = publish.gist.gist_id.clone().unwrap_or_default();
                Some(
                    probe_publish::GistPublisher::from_config(&publish.gist)
                        .map(|publisher| (Self::Gist(publisher), target)),
                )
            }
            PublishProvider::Webdav => Some(
                probe_publish::WebDavPublisher::from_config(&publish.webdav)
                    .map(|publisher| (Self::Webdav(publisher), String::new())),
            ),
        }
    }
}

impl FilesPublisher for CanonicalPublisher {
    async fn publish_files(
        &self,
        target: &str,
        files: &[(String, String)],
        gate_file: &str,
    ) -> Result<probe_publish::FilesOutcome, probe_publish::PublishError> {
        match self {
            Self::Gist(publisher) => publisher.publish_files(target, files, gate_file).await,
            Self::Webdav(publisher) => publisher.publish_files(target, files, gate_file).await,
        }
    }
}

/// 一次 canonical round 发布的请求。字段与
/// [`crate::publish::PublishRoundRequest`] 对齐：canonical 与逐源产物必须
/// 从同一轮状态出发，永不另立口径。
pub struct CanonicalRoundRequest<'a> {
    pub publish: &'a PublishSection,
    pub sources: &'a [probe_config::SourceSpec],
    pub collected: &'a Collected,
    pub verdicts: &'a [NodeVerdict],
    pub guard: GuardDecision,
    /// 本轮是否真的提交了测量（与逐源发布同一判定）。
    pub measured: bool,
    /// 本轮的 round 行 id（进 status 文件与 history）。
    pub round_id: i64,
    /// 本轮计数（`RoundOutcome::counts`）：status 文件与 history 的单一来源。
    pub counts: RoundCounts,
    /// 本轮护栏是否判了 suspect（`RoundOutcome::suspect`）。
    pub suspect: bool,
}

/// 一次 canonical round 发布的结果。`attempted=false` 时 `status` 恒为
/// [`SKIPPED`]；`digest` 携带本次（或未发布的）尝试 digest 以便对账。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRoundReport {
    pub attempted: bool,
    pub status: &'static str,
    /// 本次尝试的连接字段 digest（改名不移动）。
    pub digest: Option<String>,
    /// published 时的内容 revision（gate 文件 sha256 首 12 hex）。
    pub revision: Option<String>,
    pub error_code: Option<String>,
    /// publish_failed 时的脱敏消息（发布器的消息契约）。
    pub message: String,
}

impl CanonicalRoundReport {
    /// 成功路径（含「本轮没得发」）：调用方据此选择日志级别。
    pub fn is_success(&self) -> bool {
        self.status != probe_storage::runs::PUBLISH_FAILED
    }
}

/// 一次 round 的 canonical 装配结果：产物 + 随行的 status payload。
struct RoundCanonical {
    artifact: CanonicalArtifact,
    status: Value,
}

/// 装配 round 的 canonical 产物与 status payload；无可发布产物时返回
/// `None`（护栏判定与逐源发布的 skip 同构）。
fn assemble_round_canonical(request: &CanonicalRoundRequest<'_>) -> Option<RoundCanonical> {
    if !request.publish.enabled {
        return None;
    }
    match request.guard {
        GuardDecision::PreservePreviousState => return None,
        GuardDecision::MarkRoundInconclusive if !request.measured => return None,
        GuardDecision::ApplyConvergence | GuardDecision::MarkRoundInconclusive => {}
    }

    // 与逐源产物同一 selection（同一函数、同一轮状态）。
    let selections = select_source_exports(
        request.publish,
        request.sources,
        request.collected,
        request.verdicts,
    );
    // 先拥有数据（key/proxies/meta），再借用构造 SourceInput——借用结构
    // 不允许引用循环体内的临时 map。
    struct SelectionData {
        key: String,
        proxies: Vec<Value>,
        meta: BTreeMap<String, NodeMeta>,
    }
    let mut data: Vec<SelectionData> = Vec::new();
    for selection in &selections {
        if selection.skipped.is_some() || selection.proxies.is_empty() {
            continue;
        }
        // meta 以指纹为键（`lookup_meta` 显示名/指纹都能查到）；出口国别与
        // 延迟来自本轮 verdict，best_probe 留给 G-3 多探测点聚合。
        let mut meta: BTreeMap<String, NodeMeta> = BTreeMap::new();
        for verdict in request.verdicts.iter().filter(|v| v.is_alive()) {
            if verdict.job.source_id != selection.key {
                continue;
            }
            let delay = match verdict.verdict {
                Verdict::Alive { delay_ms } => Some(i64::from(delay_ms)),
                _ => None,
            };
            meta.insert(
                verdict.job.fingerprint.clone(),
                NodeMeta {
                    egress_country: verdict.exit.as_ref().and_then(|exit| exit.country.clone()),
                    delay,
                    best_probe: None,
                },
            );
        }
        data.push(SelectionData {
            key: selection.key.clone(),
            proxies: selection.proxies.clone(),
            meta,
        });
    }

    let inputs: Vec<SourceInput<'_>> = data
        .iter()
        .map(|entry| SourceInput {
            key: entry.key.clone(),
            proxies: &entry.proxies,
            // Rust 链式形态把 dialer 内嵌在节点里，没有独立 proxy-groups。
            groups: &[],
            meta: &entry.meta,
        })
        .collect();

    let artifact = match build_canonical(&inputs, &RenameConfig::default()) {
        Ok(Some(artifact)) => artifact,
        _ => return None,
    };
    let status = json!({
        "updated_at": probe_storage::utc_now(),
        "round_id": request.round_id,
        "total": request.counts.total,
        "alive": request.counts.ok,
        "dropped": request.counts.dropped,
        "suspect": request.suspect,
        // digest 显式随行：status 文件与幂等门禁都用它，不依赖发布器兜底。
        "digest": artifact.meta.digest,
    });
    Some(RoundCanonical { artifact, status })
}

/// 把 canonical 正文与 meta 侧车写到 `export_root`（Python P4 落盘形态）。
/// 原子替换：先写临时文件再 rename，读者永远看到完整文件。
pub fn write_local_canonical(
    export_root: &Path,
    artifact: &CanonicalArtifact,
) -> std::io::Result<()> {
    std::fs::create_dir_all(export_root)?;
    let content = artifact.rendered.content();
    let meta = serde_json::to_string_pretty(&artifact.meta).unwrap_or_else(|_| "{}".into());
    let target = export_root.join(LOCAL_CANONICAL_FILE);
    let meta_target = export_root.join(LOCAL_META_FILE);
    let tmp = export_root.join(format!("{LOCAL_CANONICAL_FILE}.tmp"));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &target)?;
    let meta_tmp = export_root.join(format!("{LOCAL_META_FILE}.tmp"));
    std::fs::write(&meta_tmp, meta)?;
    std::fs::rename(&meta_tmp, &meta_target)?;
    Ok(())
}

/// 最近 `HISTORY_LIMIT` 轮摘要，一行一 JSON；仅计数与发布状态，无凭据
/// （Python `gist_store.build_history` 的 Rust 镜像）。
pub fn build_history(storage: &probe_storage::Storage) -> String {
    let Ok(rows) = storage.list_rounds(HISTORY_LIMIT, None) else {
        return String::new();
    };
    let lines: Vec<String> = rows.iter().map(round_history_line).collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn round_history_line(row: &probe_storage::RoundRow) -> String {
    json!({
        "round_id": row.round_id,
        "started_at": row.started_at,
        "finished_at": row.finished_at,
        "trigger": row.trigger,
        "total": row.total,
        "alive": row.ok,
        "dead": row.failed,
        "dropped": row.dropped,
        "restored": row.restored,
        "suspect": row.suspect,
        "publish": if row.suspect { "skipped" } else { "ok" },
    })
    .to_string()
}

/// README.md 正文：更新时间、格式说明、secret 警告、消费方式
/// （Python `build_readme` 的 Rust 镜像，同样不含任何凭据）。
pub fn build_readme(status_payload: &Value, count: usize, digest: &str) -> String {
    let updated = status_payload
        .get("updated_at")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!(
        "# mihomo-probe 测活订阅\n\
\n\
- 更新时间（UTC）：{updated}\n\
- 节点数：{count}\n\
- digest：`{digest}`\n\
\n\
## 文件\n\
\n\
- `{LOCAL_CANONICAL_FILE}`：唯一可订阅的 canonical 节点列表（Clash/mihomo \
YAML）。每轮只更新这一个文件，节点顺序固定，不改连接参数。\n\
- `probe-status.json`：当前轮与各探测点健康概况。不含任何节点凭据。\n\
- `probe-history.jsonl`：最近 {HISTORY_LIMIT} 轮摘要（一行一 JSON，超出滚动\
截断）。不含节点凭据。\n\
\n\
## 安全提示\n\
\n\
secret 不等于私密：任何拿到 URL 的人都能读。订阅 YAML 含节点凭据（server / \
uuid / password / reality 等），请把 URL 当成凭据对待。若不能接受凭据经 URL \
暴露，请改用私有 Git 仓库或自托管对象存储（WebDAV）。\n\
\n\
## 消费方式\n\
\n\
在 Sub-Store 里添加 `{LOCAL_CANONICAL_FILE}` 的 raw URL 作为远程订阅，或用 \
Web 控制台「发布中心」手动推送。\n"
    )
}

/// 完整链路：装配 → 本地 parity 落盘 → （尽力而为）四文件发布。
/// `publisher: None` = 本地 parity 落盘后到此为止（provider 为 local 的部署）。
///
/// 返回值只描述「canonical 发布做了什么」，绝不携带本轮节点状态的变更
/// ——本函数不改任何节点状态（fire-and-forget 契约）。
pub async fn publish_round_canonical<P: FilesPublisher>(
    publisher: Option<&P>,
    target: &str,
    storage: &probe_storage::Storage,
    export_root: &Path,
    request: &CanonicalRoundRequest<'_>,
) -> CanonicalRoundReport {
    let skipped = || CanonicalRoundReport {
        attempted: false,
        status: SKIPPED,
        digest: None,
        revision: None,
        error_code: None,
        message: String::new(),
    };
    let Some(round_canonical) = assemble_round_canonical(request) else {
        return skipped();
    };
    let digest = round_canonical.artifact.meta.digest.clone();
    // 本地 parity 落盘：失败只记日志（导出根不可写是部署问题，不该把本轮
    // 变成失败），远端照常尝试。
    if let Err(error) = write_local_canonical(export_root, &round_canonical.artifact) {
        tracing::warn!(%error, "canonical local materialization failed");
    }

    let Some(publisher) = publisher else {
        return CanonicalRoundReport {
            attempted: false,
            status: SKIPPED,
            digest: Some(digest),
            revision: None,
            error_code: None,
            message: String::new(),
        };
    };

    let history = build_history(storage);
    let readme = build_readme(
        &round_canonical.status,
        round_canonical.artifact.meta.count,
        &digest,
    );
    let Some(files) = CanonicalFiles::from_round(
        Some(&round_canonical.artifact),
        &round_canonical.status,
        &history,
        &readme,
    ) else {
        return CanonicalRoundReport {
            attempted: false,
            status: SKIPPED,
            digest: Some(digest),
            revision: None,
            error_code: None,
            message: String::new(),
        };
    };
    let outcome = crate::canonical_publish::publish_canonical(publisher, target, &files).await;
    let revision = (outcome.status == probe_storage::runs::PUBLISH_PUBLISHED)
        .then(|| outcome.message.clone())
        .filter(|revision| !revision.is_empty());
    CanonicalRoundReport {
        attempted: true,
        status: outcome.status,
        digest: Some(digest),
        revision,
        error_code: outcome.error_code,
        message: outcome.message,
    }
}
