//! Fixed-file-set publication (HK3 §8 四文件布局，workstreams/25 G-2)。
//!
//! 与逐 artifact 的 [`crate::Publisher`] 互补：canonical 订阅是一个 Gist 里
//! 的**固定文件集合**（canonical/status/history/README），幂等门禁钉在
//! gate 文件（canonical）的远端 sha 上——gate 未变 → 零 PATCH、零 revision
//! （随行文件等待，最新轮状态由 dashboard 实时给出）；gate 变了 → **一次**
//! PATCH 携带全部变化的文件。一个 Gist revision = 一次真实节点集变化。
use std::future::Future;

use crate::types::PublishError;

/// 一次固定文件集发布的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesOutcome {
    /// gate 文件远端 sha 与本次内容一致：零请求写、零 revision。
    NoChange,
    /// gate 变化，一次 PATCH 提交了全部变化文件（永不含 content:null——
    /// 那是 GitHub 的删除语义）。
    Published {
        /// 本次 PATCH 实际携带的文件名（排序后）。
        changed: Vec<String>,
        /// 内容 revision：gate 文件 sha256 首 12 hex（与 manifest revision
        /// 同约定）。真实 GitHub revision 以 history_url 为准，这里给的是
        /// 可在本地复算的内容标识。
        revision: Option<String>,
    },
}

/// 固定文件集发布的最小接缝：`GistPublisher` 提供真实现，引擎级场景测试
/// 用假目的地实现同一契约（与 `Publisher`/`ShaGist` 的分工方式一致）。
pub trait FilesPublisher: Send + Sync {
    fn publish_files(
        &self,
        gist_id: &str,
        files: &[(String, String)],
        gate_file: &str,
    ) -> impl Future<Output = Result<FilesOutcome, PublishError>> + Send;
}
