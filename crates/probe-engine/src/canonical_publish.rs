//! Canonical 四文件发布（G-2，HK3 §8 / workstreams/25）。
//!
//! 把 [`crate::canonical::CanonicalArtifact`]（G-1 模型）装配成固定四文件
//! 集合并经 [`FilesPublisher`] 发布：幂等门禁钉在 canonical 文件上——内容
//! 未变零 PATCH 零 revision，status/history/README 是随行文件（裁决与行为
//! 与 Python `gist_store.publish` 一致，见 workstreams/24 §3）。
//!
//! 账本边界：[`CanonicalPublishOutcome::digest`] 是**本次尝试**发布的内容
//! digest；调用方只在 `status == PUBLISH_PUBLISHED` 时推进账本/存储的
//! digest——发布失败保留上一成功 digest（§3.4 (5)/(6)，19 号验收 §11.1-6）。
use probe_publish::{FilesOutcome, FilesPublisher, PublishError};
use probe_storage::runs::{PUBLISH_FAILED, PUBLISH_NO_CHANGE, PUBLISH_PUBLISHED};

use crate::canonical::CanonicalArtifact;

/// 唯一可订阅的 canonical 节点列表。
pub const CANONICAL_FILE: &str = "mihomo-alive.yaml";
/// 当前轮 + 探测点健康概况（无节点凭据）。
pub const STATUS_FILE: &str = "probe-status.json";
/// 最近 100 轮摘要（调用方负责滚动截断），无凭据。
pub const HISTORY_FILE: &str = "probe-history.jsonl";
/// 更新时间、格式说明、secret 非私密警告、手动推送方式。
pub const README_FILE: &str = "README.md";

/// 一次 round 的四文件集合。
///
/// `from_round(None, ..)` = 本轮无可发布产物（空轮/护栏拦截）：不构造、不
/// 发请求，上一稳定四文件原样站住（§11.2-4）。
pub struct CanonicalFiles {
    files: Vec<(String, String)>,
    digest: String,
}

impl CanonicalFiles {
    /// 装配四文件；`status_payload` 以 Python 兼容的 pretty JSON（2 空格缩进
    /// + 换行）序列化，保证同输入字节一致（幂等依赖字节稳定）。
    pub fn from_round(
        artifact: Option<&CanonicalArtifact>,
        status_payload: &serde_json::Value,
        history_text: &str,
        readme_text: &str,
    ) -> Option<Self> {
        let artifact = artifact?;
        let status_text = match serde_json::to_string_pretty(status_payload) {
            Ok(mut text) => {
                text.push('\n');
                text
            }
            Err(_) => return None,
        };
        let files = vec![
            (
                CANONICAL_FILE.to_string(),
                artifact.rendered.content().to_string(),
            ),
            (STATUS_FILE.to_string(), status_text),
            (HISTORY_FILE.to_string(), history_text.to_string()),
            (README_FILE.to_string(), readme_text.to_string()),
        ];
        Some(Self {
            files,
            digest: artifact.meta.digest.clone(),
        })
    }

    pub fn files(&self) -> &[(String, String)] {
        &self.files
    }

    /// 本次尝试发布的连接字段 digest（改名不移动）。
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// 发布结果：`status` 复用 probe-storage 的账本口径
/// （published / no_change / publish_failed），`digest` 永远携带本次尝试的
/// 内容 digest——推进与否由调用方按 `status` 决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPublishOutcome {
    pub status: &'static str,
    pub digest: String,
    pub changed: Vec<String>,
    pub error_code: Option<String>,
    pub retryable: bool,
    pub message: String,
}

/// 把四文件集合发到 `gist_id`；远端错误（终态/可重试）如实映射，不在本层
/// 重试之外再加重试——`GistPublisher` 的有界退避已经跑在调用里。
pub async fn publish_canonical(
    publisher: &impl FilesPublisher,
    gist_id: &str,
    files: &CanonicalFiles,
) -> CanonicalPublishOutcome {
    match publisher
        .publish_files(gist_id, files.files(), CANONICAL_FILE)
        .await
    {
        Ok(FilesOutcome::NoChange) => CanonicalPublishOutcome {
            status: PUBLISH_NO_CHANGE,
            digest: files.digest().to_string(),
            changed: Vec::new(),
            error_code: None,
            retryable: false,
            message: String::new(),
        },
        Ok(FilesOutcome::Published { changed, revision }) => CanonicalPublishOutcome {
            status: PUBLISH_PUBLISHED,
            digest: files.digest().to_string(),
            changed,
            error_code: None,
            retryable: false,
            message: revision.unwrap_or_default(),
        },
        Err(error) => CanonicalPublishOutcome {
            status: PUBLISH_FAILED,
            digest: files.digest().to_string(),
            changed: Vec::new(),
            error_code: Some(error.code.as_str().to_string()),
            retryable: error.retryable,
            message: error.message.clone(),
        },
    }
}

/// 供调用方在账本侧执行「digest 只进不退」的判定（19 号验收 §11.1-6）：
/// 只有 published 才允许把存储的 digest 推进到本次内容。
pub fn digest_advances(status: &str) -> bool {
    status == PUBLISH_PUBLISHED
}

/// 错误类型再导出，测试与调用方不必直接依赖 probe-publish 的路径。
pub type CanonicalPublishError = PublishError;
