//! Canonical round 接线场景（workstreams/27，19_ACCEPTANCE_MAP 写法）。
//!
//! 组合测试：轮次状态（verdicts + collected + guard）→ `assemble` →
//! 本地 parity 落盘 → `publish_round_canonical`（假目的地）。钉住的行为：
//! 护栏空轮零请求零落盘、正常轮四文件 + 本地 mihomo-alive.yaml/meta、
//! 同内容重跑零写（digest 稳定）、无远程通道时本地照常落盘、发布失败
//! 不影响报告外的任何轮次状态。

use std::collections::BTreeMap;
use std::sync::Mutex;

use probe_config::{PublishSection, SourceSpec};
use probe_domain::ExitIdentity;
use probe_engine::canonical::{CanonicalArtifact, CanonicalMeta};
use probe_engine::canonical_publish::{CANONICAL_FILE, HISTORY_FILE, README_FILE, STATUS_FILE};
use probe_engine::canonical_round::{
    publish_round_canonical, CanonicalRoundRequest, LOCAL_CANONICAL_FILE, LOCAL_META_FILE, SKIPPED,
};
use probe_engine::collect::Collected;
use probe_engine::publish::select_source_exports;
use probe_engine::round::{GuardDecision, NodeVerdict, RoundCounts};
use probe_engine::Job;
use probe_publish::{FilesOutcome, FilesPublisher, PublishError, PublishErrorCode};
use probe_storage::Storage;
use serde_json::{json, Value};

/// 假目的地：与真 GistPublisher 同一契约（gate sha 判等、PATCH 计数、可注入
/// 失败）。与 canonical_publish_scenarios 的 FakeFiles 同构——两份是因为
/// 集成测试 crate 不共享代码；各自钉自己批次的场景。
struct FakeFiles {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    files: BTreeMap<String, String>,
    patches: usize,
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
}

impl FilesPublisher for FakeFiles {
    async fn publish_files(
        &self,
        _target: &str,
        files: &[(String, String)],
        gate_file: &str,
    ) -> Result<FilesOutcome, PublishError> {
        let mut state = self.state.lock().unwrap();
        if let Some(error) = state.fail_next.take() {
            return Err(error);
        }
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
        for (name, content) in changed {
            state.files.insert(name, content);
        }
        Ok(FilesOutcome::Published {
            changed: vec![gate_file.to_string()],
            revision: Some("rev012345678".to_string()),
        })
    }
}

fn proxy(name: &str, port: i64) -> Value {
    json!({"name": name, "type": "vless", "server": "s.example.com",
           "port": port, "uuid": "u-1", "tls": true})
}

fn source() -> SourceSpec {
    SourceSpec {
        key: "air".to_string(),
        kind: "sub".to_string(),
        name: "air".to_string(),
        label: "air".to_string(),
        enabled: true,
        relay: false,
        direct: true,
        chain: false,
        export: true,
    }
}

/// 一个活节点的 verdict：source_id=air, fingerprint=fp-1, 显示名 Tokyo 01。
fn alive_verdict(name: &str, fingerprint: &str, port: i64, country: &str) -> NodeVerdict {
    let mut verdict = NodeVerdict::alive(
        Job::new("air", fingerprint, name, "direct", "203.0.113.9"),
        120,
    );
    verdict.exit = Some(ExitIdentity {
        ip: Some("203.0.113.7".to_string()),
        country: Some(country.to_string()),
        colo: None,
        asn: None,
    });
    let _ = port;
    verdict
}

/// 一轮状态：一个活节点（originals 里有 domain-form 代理）。
fn fixture() -> (Collected, Vec<NodeVerdict>) {
    let mut collected = Collected::default();
    collected.originals.insert(
        ("air".to_string(), "fp-1".to_string()),
        proxy("Tokyo 01", 443),
    );
    let verdicts = vec![alive_verdict("Tokyo 01", "fp-1", 443, "JP")];
    (collected, verdicts)
}

fn request<'a>(
    publish: &'a PublishSection,
    sources: &'a [SourceSpec],
    collected: &'a Collected,
    verdicts: &'a [NodeVerdict],
    guard: GuardDecision,
) -> CanonicalRoundRequest<'a> {
    CanonicalRoundRequest {
        publish,
        sources,
        collected,
        verdicts,
        guard,
        measured: true,
        round_id: 7,
        counts: RoundCounts {
            total: 1,
            ok: 1,
            ..RoundCounts::default()
        },
        suspect: false,
    }
}

/// selection 与逐源产物同口径：同样的输入在 select_source_exports 下
/// 产出一个非空 body（这个断言钉住「canonical 不另立 selection 口径」）。
#[test]
fn selection_feeds_the_canonical_from_the_same_call() {
    let (collected, verdicts) = fixture();
    let publish = PublishSection::default();
    let sources = vec![source()];
    let selections = select_source_exports(&publish, &sources, &collected, &verdicts);
    assert_eq!(selections.len(), 1);
    assert!(selections[0].skipped.is_none());
    assert_eq!(selections[0].proxies.len(), 1);
}

/// 护栏空轮：零请求、零落盘（§11.2-4 的 round 级镜像）。
#[tokio::test]
async fn a_guarded_round_builds_no_files_and_sends_no_requests() {
    let (collected, verdicts) = fixture();
    let publish = PublishSection::default();
    let sources = vec![source()];
    let storage = Storage::open_in_memory().unwrap();
    let root = tempfile::tempdir().unwrap();
    let fake = FakeFiles::new();
    let report = publish_round_canonical(
        Some(&fake),
        "gist0001",
        &storage,
        root.path(),
        &request(
            &publish,
            &sources,
            &collected,
            &verdicts,
            GuardDecision::PreservePreviousState,
        ),
    )
    .await;
    assert_eq!(report.status, SKIPPED);
    assert!(!report.attempted);
    assert_eq!(fake.patches(), 0);
    assert!(!root.path().join(LOCAL_CANONICAL_FILE).exists());

    // publish 关闭同判。
    let disabled = PublishSection {
        enabled: false,
        ..PublishSection::default()
    };
    let report = publish_round_canonical(
        Some(&fake),
        "gist0001",
        &storage,
        root.path(),
        &request(
            &disabled,
            &sources,
            &collected,
            &verdicts,
            GuardDecision::ApplyConvergence,
        ),
    )
    .await;
    assert_eq!(report.status, SKIPPED);
    assert_eq!(fake.patches(), 0);
}

/// 正常轮：四文件远端发布 + 本地 parity 落盘 + 报告携带 digest 与 revision。
#[tokio::test]
async fn a_normal_round_publishes_four_files_and_materializes_locally() {
    let (collected, verdicts) = fixture();
    let publish = PublishSection::default();
    let sources = vec![source()];
    let storage = Storage::open_in_memory().unwrap();
    let root = tempfile::tempdir().unwrap();
    let fake = FakeFiles::new();
    let report = publish_round_canonical(
        Some(&fake),
        "gist0001",
        &storage,
        root.path(),
        &request(
            &publish,
            &sources,
            &collected,
            &verdicts,
            GuardDecision::ApplyConvergence,
        ),
    )
    .await;
    assert_eq!(report.status, probe_storage::runs::PUBLISH_PUBLISHED);
    assert!(report.attempted);
    assert!(
        report.digest.as_deref().is_some_and(|d| d.len() == 32),
        "{report:?}"
    );
    assert_eq!(report.revision.as_deref(), Some("rev012345678"));
    // 四文件一个不缺。
    for name in [CANONICAL_FILE, STATUS_FILE, HISTORY_FILE, README_FILE] {
        assert!(fake.stored(name).is_some(), "{name} missing");
    }
    // 本地 parity：canonical + meta 侧车（Rust 产物是 pretty JSON 文档）。
    let local = std::fs::read_to_string(root.path().join(LOCAL_CANONICAL_FILE)).unwrap();
    assert!(local.contains("\"proxies\""), "{local}");
    assert!(local.contains("JP | Tokyo 01"), "rename + exit tag present");
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(root.path().join(LOCAL_META_FILE)).unwrap())
            .unwrap();
    assert_eq!(meta["count"], 1);
    assert_eq!(meta["digest"], report.digest.clone().unwrap());
    // status 随行文件带轮次与 digest。
    let status: Value = serde_json::from_str(&fake.stored(STATUS_FILE).unwrap()).unwrap();
    assert_eq!(status["round_id"], 7);
    assert_eq!(status["digest"], report.digest.clone().unwrap());
    assert_eq!(status["alive"], 1);
}

/// 同内容重跑：canonical 字节稳定 → gate 未变 → 零 PATCH（幂等门禁钉在
/// canonical 文件字节上）。注意 updated_at 变化发生在 status 文件里，而
/// status 是随行文件——gate 没动就不发（workstreams/25 裁决 1）。
#[tokio::test]
async fn a_rerun_of_the_same_round_writes_nothing() {
    let (collected, verdicts) = fixture();
    let publish = PublishSection::default();
    let sources = vec![source()];
    let storage = Storage::open_in_memory().unwrap();
    let root = tempfile::tempdir().unwrap();
    let fake = FakeFiles::new();
    let first = publish_round_canonical(
        Some(&fake),
        "gist0001",
        &storage,
        root.path(),
        &request(
            &publish,
            &sources,
            &collected,
            &verdicts,
            GuardDecision::ApplyConvergence,
        ),
    )
    .await;
    assert_eq!(first.status, probe_storage::runs::PUBLISH_PUBLISHED);
    let second = publish_round_canonical(
        Some(&fake),
        "gist0001",
        &storage,
        root.path(),
        &request(
            &publish,
            &sources,
            &collected,
            &verdicts,
            GuardDecision::ApplyConvergence,
        ),
    )
    .await;
    assert_eq!(
        second.status,
        probe_storage::runs::PUBLISH_NO_CHANGE,
        "the rerun is a byte-identical no-op"
    );
    assert_eq!(second.digest, first.digest, "digest does not move");
    assert_eq!(fake.patches(), 1);
}

/// 无远程通道（provider=local）：本地 parity 照常落盘，attempted=false。
#[tokio::test]
async fn without_a_remote_channel_the_local_materialization_still_happens() {
    let (collected, verdicts) = fixture();
    let publish = PublishSection::default();
    let sources = vec![source()];
    let storage = Storage::open_in_memory().unwrap();
    let root = tempfile::tempdir().unwrap();
    let report = publish_round_canonical::<FakeFiles>(
        None,
        "",
        &storage,
        root.path(),
        &request(
            &publish,
            &sources,
            &collected,
            &verdicts,
            GuardDecision::ApplyConvergence,
        ),
    )
    .await;
    assert_eq!(report.status, SKIPPED);
    assert!(!report.attempted);
    assert!(root.path().join(LOCAL_CANONICAL_FILE).exists());
    assert!(root.path().join(LOCAL_META_FILE).exists());
}

/// 发布失败：publish_failed + attempted digest，本地落盘照常，上一稳定内容
/// 由目的地契约站住（假目的地的 fail_next 抛错即未写入）。
#[tokio::test]
async fn a_failed_publish_reports_the_attempted_digest_and_keeps_local_files() {
    let (collected, verdicts) = fixture();
    let publish = PublishSection::default();
    let sources = vec![source()];
    let storage = Storage::open_in_memory().unwrap();
    let root = tempfile::tempdir().unwrap();
    let fake = FakeFiles::new();
    fake.fail_next(PublishError::terminal(
        PublishErrorCode::WebdavAuth,
        "rejected",
    ));
    let report = publish_round_canonical(
        Some(&fake),
        "",
        &storage,
        root.path(),
        &request(
            &publish,
            &sources,
            &collected,
            &verdicts,
            GuardDecision::ApplyConvergence,
        ),
    )
    .await;
    assert_eq!(report.status, probe_storage::runs::PUBLISH_FAILED);
    assert_eq!(report.error_code.as_deref(), Some("webdav_auth"));
    assert!(report.digest.is_some());
    assert!(report.revision.is_none());
    // 本地落盘不因远端失败而回退。
    assert!(root.path().join(LOCAL_CANONICAL_FILE).exists());
    assert_eq!(fake.patches(), 0);
}

/// history 由账本生成：一轮完成后，canonical 的 history 文件携带该轮。
#[tokio::test]
async fn the_history_file_describes_ledger_rounds() {
    let storage = Storage::open_in_memory().unwrap();
    let round_id = storage.start_round("manual", Some("direct")).unwrap();
    storage.finish_round(round_id, Some("done")).unwrap();
    let history = probe_engine::canonical_round::build_history(&storage);
    assert!(history.contains("\"round_id\":"), "{history}");
    assert!(history.contains("\"publish\":\"ok\""), "{history}");
}

/// digest「只进不退」的调用方判定复用 canonical_publish 的口径。
#[test]
fn the_digest_only_advances_through_published() {
    use probe_engine::canonical_publish::digest_advances;
    assert!(digest_advances(probe_storage::runs::PUBLISH_PUBLISHED));
    assert!(!digest_advances(probe_storage::runs::PUBLISH_NO_CHANGE));
    assert!(!digest_advances(probe_storage::runs::PUBLISH_FAILED));
}

/// 本地 parity 落盘是原子替换：已存在的旧文件被完整替换，不留 .tmp 残骸。
#[tokio::test]
async fn local_materialization_replaces_previous_files_atomically() {
    let root = tempfile::tempdir().unwrap();
    let artifact = CanonicalArtifact {
        rendered: probe_engine::publish::render_clash_meta(&[proxy("Tokyo 01", 443)], &[]).unwrap(),
        meta: CanonicalMeta {
            count: 1,
            digest: "d".repeat(32),
            content_sha256: "c".repeat(64),
            renames: Vec::new(),
            source_aliases: BTreeMap::new(),
        },
    };
    probe_engine::canonical_round::write_local_canonical(root.path(), &artifact).unwrap();
    let first = std::fs::read_to_string(root.path().join(LOCAL_CANONICAL_FILE)).unwrap();
    assert!(first.contains("Tokyo 01"));
    // 重写同一内容：文件仍在、无 .tmp 残留。
    probe_engine::canonical_round::write_local_canonical(root.path(), &artifact).unwrap();
    let entries: Vec<String> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().all(|name| !name.ends_with(".tmp")),
        "no tmp leftovers: {entries:?}"
    );
}
