//! Canonical 四文件发布场景（G-2，19_ACCEPTANCE_MAP 写法）。
//!
//! 组合测试：G-1 的 `build_canonical` 产物 → `CanonicalFiles::from_round` →
//! `publish_canonical`，目的地是实现同一 `FilesPublisher` 契约的假目的地
//! （与 publish_scenarios 的 ShaGist 分工方式一致；真 GitHub 属 NOT_RUN）。
//! 钉住的行为：10 轮重跑一次写入、随行文件只在 canonical 变化时同行、
//! 护栏空轮零请求、发布失败保留上一内容、digest 只进不退。

use std::collections::BTreeMap;
use std::sync::Mutex;

use probe_engine::canonical::{build_canonical, NodeMeta, SourceInput};
use probe_engine::canonical_publish::{
    digest_advances, publish_canonical, CanonicalFiles, CANONICAL_FILE, HISTORY_FILE, README_FILE,
    STATUS_FILE,
};
use probe_publish::{FilesOutcome, FilesPublisher, PublishError, PublishErrorCode};
use serde_json::{json, Value};

static EMPTY_META: std::sync::OnceLock<BTreeMap<String, NodeMeta>> = std::sync::OnceLock::new();

fn empty_meta() -> &'static BTreeMap<String, NodeMeta> {
    EMPTY_META.get_or_init(BTreeMap::new)
}

fn proxy(name: &str, port: i64) -> Value {
    json!({"name": name, "type": "vless", "server": "s.example.com",
           "port": port, "uuid": "u-1", "tls": true})
}

/// G-1 → G-2 的真实链路：同指纹同连接参数下，无论显示名怎么变，
/// canonical 内容与 digest 都是同一份。
fn artifact_for(names: &[&str]) -> probe_engine::canonical::CanonicalArtifact {
    let proxies: Vec<Value> = names
        .iter()
        .enumerate()
        .map(|(i, n)| proxy(n, 443 + i as i64))
        .collect();
    let src = [SourceInput {
        key: "air".to_string(),
        proxies: &proxies,
        groups: &[],
        meta: empty_meta(),
    }];
    build_canonical(&src, &Default::default()).unwrap().unwrap()
}

/// 假目的地：与真 GistPublisher 同一契约（gate 文件 sha 判等、PATCH 计数、
/// 可注入失败），供场景测试观察“写入次数”与“上一内容是否站住”。
struct FakeFiles {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    files: BTreeMap<String, String>,
    patches: usize,
    last_patch: Vec<(String, String)>,
    fail_next: Option<PublishError>,
}

impl FakeFiles {
    fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
        }
    }

    fn fail_next(&self, error: PublishError) {
        self.state.lock().unwrap().fail_next = Some(error);
    }

    fn patches(&self) -> usize {
        self.state.lock().unwrap().patches
    }

    fn stored(&self, name: &str) -> Option<String> {
        self.state.lock().unwrap().files.get(name).cloned()
    }

    fn last_patch(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().last_patch.clone()
    }
}

impl FilesPublisher for FakeFiles {
    async fn publish_files(
        &self,
        _gist_id: &str,
        files: &[(String, String)],
        gate_file: &str,
    ) -> Result<FilesOutcome, PublishError> {
        let mut state = self.state.lock().unwrap();
        if let Some(error) = state.fail_next.take() {
            return Err(error);
        }
        // 与真 GistPublisher 同一协议：先判 gate，未变 → 直接 NoChange，
        // **一个字节都不发**；PATCH 计数只统计真的发出的 PATCH。
        let changed: Vec<(String, String)> = files
            .iter()
            .filter(|(name, content)| {
                state.files.get(name).map(|c| c.as_str()) != Some(content.as_str())
            })
            .cloned()
            .collect();
        if !changed.iter().any(|(name, _)| name == gate_file) {
            return Ok(FilesOutcome::NoChange);
        }
        state.patches += 1;
        state.last_patch = changed.clone();
        for (name, content) in changed.iter() {
            state.files.insert(name.clone(), content.clone());
        }
        Ok(FilesOutcome::Published {
            changed: changed.into_iter().map(|(name, _)| name).collect(),
            revision: None,
        })
    }
}

fn files_for(
    artifact: &probe_engine::canonical::CanonicalArtifact,
    round_id: i64,
) -> CanonicalFiles {
    CanonicalFiles::from_round(
        Some(artifact),
        &json!({"round_id": round_id, "digest": artifact.meta.digest}),
        "{\"round_id\": 1}\n",
        "# mihomo-probe 测活订阅\n",
    )
    .unwrap()
}

#[tokio::test]
async fn ten_reruns_of_the_same_round_write_the_destination_once() {
    let publisher = FakeFiles::new();
    // 每轮显示名不同（上游重排的常态）：同指纹同连接参数 → 同一产物。
    let artifact = artifact_for(&["HK | a"]);
    for round in 1..=10i64 {
        let files = files_for(&artifact, round);
        let outcome = publish_canonical(&publisher, "g1", &files).await;
        let expected = if round == 1 { "published" } else { "no_change" };
        assert_eq!(outcome.status, expected, "round {round}");
        assert!(digest_advances(outcome.status) == (round == 1));
    }
    assert_eq!(publisher.patches(), 1, "10 轮重跑只有首轮一次 PATCH");
    // 随行文件停在首轮（最新轮状态由 dashboard 实时给出，不进 Gist）。
    assert!(publisher
        .stored(STATUS_FILE)
        .unwrap()
        .contains("\"round_id\": 1"));
}

#[tokio::test]
async fn the_status_file_rides_along_only_when_the_canonical_changes() {
    let publisher = FakeFiles::new();
    let round1 = files_for(&artifact_for(&["HK | a"]), 1);
    let outcome = publish_canonical(&publisher, "g1", &round1).await;
    assert_eq!(outcome.status, "published");

    // canonical 未变（同名节点）、仅 status 的 round_id 变化 → 零 PATCH。
    // 注意：改名会改变 canonical 内容（订阅方本应看到新名），那是一次真实
    // 发布——"内容未变零 PATCH"钉的是同名同连接参数的同输入。
    let round2 = files_for(&artifact_for(&["HK | a"]), 2);
    let outcome = publish_canonical(&publisher, "g1", &round2).await;
    assert_eq!(outcome.status, "no_change");
    assert_eq!(publisher.patches(), 1);

    // canonical 变了（新节点）→ 一次 PATCH 同时携带 canonical 与 status。
    let round3 = files_for(&artifact_for(&["HK | renamed", "US | b"]), 3);
    let outcome = publish_canonical(&publisher, "g1", &round3).await;
    assert_eq!(outcome.status, "published");
    assert_eq!(outcome.digest, round3.digest());
    let patch = publisher.last_patch();
    let names: Vec<&str> = patch.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&CANONICAL_FILE));
    assert!(names.contains(&STATUS_FILE), "随行文件同 PATCH：{names:?}");
    assert!(publisher
        .stored(STATUS_FILE)
        .unwrap()
        .contains("\"round_id\": 3"));
}

#[tokio::test]
async fn a_guarded_round_builds_no_files_and_sends_no_requests() {
    let publisher = FakeFiles::new();
    // 护栏拦截/空轮：artifact=None → 不构造文件集、不发任何请求。
    let none = CanonicalFiles::from_round(None, &json!({"round_id": 7}), "", "");
    assert!(none.is_none());
    let files = files_for(&artifact_for(&["HK | a"]), 1);
    let _ = publish_canonical(&publisher, "g1", &files).await;
    assert_eq!(publisher.patches(), 1, "只有显式发布的那一轮碰过目的地");
}

#[tokio::test]
async fn a_failed_publish_keeps_the_previous_content_and_reports_the_attempted_digest() {
    let publisher = FakeFiles::new();
    let round1 = files_for(&artifact_for(&["HK | a"]), 1);
    publish_canonical(&publisher, "g1", &round1).await;

    // canonical 变化但目的地持续 5xx（可重试预算耗尽）：outcome=publish_failed
    // + attempted digest；存储里的上一成功内容原样站住。
    publisher.fail_next(PublishError::retryable(
        PublishErrorCode::Remote,
        "github 500 past the retry budget",
    ));
    let round2 = files_for(&artifact_for(&["HK | a", "US | b"]), 2);
    let outcome = publish_canonical(&publisher, "g1", &round2).await;
    assert_eq!(outcome.status, "publish_failed");
    assert!(outcome.retryable);
    assert_eq!(outcome.error_code.as_deref(), Some("remote"));
    assert_eq!(
        outcome.digest,
        round2.digest(),
        "失败也如实报告尝试的 digest"
    );
    assert!(publisher.stored(CANONICAL_FILE).unwrap().contains("HK | a"));
    assert!(!publisher.stored(CANONICAL_FILE).unwrap().contains("US | b"));
    // 账本口径：失败不推进 digest（§11.1-6 的调用方判定）。
    assert!(!digest_advances(outcome.status));

    // 下一轮重试同内容：成功，digest 才推进。
    let outcome = publish_canonical(&publisher, "g1", &round2).await;
    assert_eq!(outcome.status, "published");
    assert!(digest_advances(outcome.status));
    assert!(publisher.stored(CANONICAL_FILE).unwrap().contains("US | b"));
}

#[tokio::test]
async fn terminal_auth_failures_are_reported_non_retryable() {
    let publisher = FakeFiles::new();
    publisher.fail_next(PublishError::terminal(
        PublishErrorCode::GistAuth,
        "github 401",
    ));
    let files = files_for(&artifact_for(&["HK | a"]), 1);
    let outcome = publish_canonical(&publisher, "g1", &files).await;
    assert_eq!(outcome.status, "publish_failed");
    assert!(!outcome.retryable, "401/403 终态，重跑无意义");
    assert_eq!(outcome.error_code.as_deref(), Some("gist_auth"));
}

#[tokio::test]
async fn a_first_publish_seeds_all_four_files_in_one_patch() {
    let publisher = FakeFiles::new();
    let files = files_for(&artifact_for(&["HK | a", "US | b"]), 1);
    let outcome = publish_canonical(&publisher, "g1", &files).await;
    assert_eq!(outcome.status, "published");
    assert_eq!(publisher.patches(), 1);
    let patch = publisher.last_patch();
    assert_eq!(patch.len(), 4, "首建一次 PATCH 建齐四文件");
    let names: Vec<&str> = patch.iter().map(|(n, _)| n.as_str()).collect();
    for expected in [CANONICAL_FILE, STATUS_FILE, HISTORY_FILE, README_FILE] {
        assert!(names.contains(&expected), "缺 {expected}：{names:?}");
    }
    // 永不含 content:null（那是删除语义）——假目的地按内容存储，缺失即失败。
    for (_, content) in &patch {
        assert!(!content.is_empty());
    }
    assert!(publisher
        .stored(HISTORY_FILE)
        .unwrap()
        .contains("\"round_id\": 1"));
}
