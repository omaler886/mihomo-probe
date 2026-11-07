//! 21_RUST_PARITY G-8 / G-9 的引擎级收口（19_ACCEPTANCE_MAP §11.2-1 /
//! §11.2-3 的缺口行）。
//!
//! * G-8（§11.2-1）：100 节点首次发布——1 source / 1 artifact / 1 快照 /
//!   正文 100 节点；同输入重跑 = 零写（幂等门禁在规模下仍成立）。
//! * G-9（§11.2-3）：一个节点死亡的三轮以上跨轮集成——把状态机
//!   （`converge_nodes`：streak 未到阈值 = pending，账本仍 alive，绝不发布
//!   死亡）与发布腿（`publish_round`：导出集 = 本轮通过者；同一 artifact 行
//!   只进不退）串在一条真实账本上：r1 两节点活 → 导出 2 节点；r2 victim
//!   失败（streak 1，pending）→ 导出 1 节点，artifact 仍 1 行；r3 victim
//!   再败（streak 2，pending）→ 同上；r4 第三败（streak 3 ≥ 阈值）→ dead
//!   （drop），导出 1 节点，artifact 仍 1 行。死亡在账本侧只发生一次
//!   （第四轮），发布侧从 r2 起就不再携带 victim——两腿各自的口径都如实钉住。

use std::path::PathBuf;

use probe_config::{PublishSection, SourceSpec};
use probe_engine::publish::SkipReason;
use probe_engine::round::GuardDecision;
use probe_engine::{
    publish_round, Collected, Job, NodeVerdict, PublishRoundOutcome, PublishRoundRequest,
    SourcePublication,
};
use probe_storage::runs::{ClaimOutcome, STATUS_COMPLETED};
use probe_storage::{sources as ledger_sources, ConvergeNode, Storage};

use std::path::PathBuf as _PathBufAlias;

const POLICY: probe_domain::Policy = probe_domain::Policy {
    drop_after_consecutive_fails: 3,
    suspect_floor_ratio: 0.5,
    suspect_floor_absolute: 3,
};

fn specs() -> Vec<SourceSpec> {
    vec![SourceSpec {
        key: "air".to_string(),
        kind: "collection".to_string(),
        name: "air".to_string(),
        label: "air".to_string(),
        enabled: true,
        relay: false,
        direct: true,
        chain: false,
        export: true,
    }]
}

/// 每节点独立 server——B8 的 `dedup_nodes`（默认开）按
/// type+server+port+凭据折叠，同端点的测试夹具会被并成 1 个节点。
fn node(fingerprint: &str, display: &str, server: &str) -> (Job, serde_json::Value) {
    let job = Job::new("air", fingerprint, display, "direct", server).with_origin(
        display.to_string(),
        Some(server.to_string()),
        Some("ss".to_string()),
    );
    let original = serde_json::json!({"name": display, "type": "ss",
        "server": server, "port": 443, "password": fingerprint});
    (job, original)
}

/// 一轮：账本 claim + 本地发布腿（与 CLI/API 的调用序列同构）。
/// `alive` 是本轮通过者的指纹/显示名集合——决定导出内容。
fn run_publish_leg(
    storage: &Storage,
    export_root: &std::path::Path,
    alive: &[(&str, &str)],
) -> PublishRoundOutcome {
    let specs = specs();
    let mut collected = Collected::default();
    let mut verdicts = Vec::new();
    for (fingerprint, display) in alive {
        let (job, original) = node(
            fingerprint,
            display,
            &format!("s-{fingerprint}.example.invalid"),
        );
        collected.originals.insert(
            ("air".to_string(), job.fingerprint.clone()),
            original.clone(),
        );
        verdicts.push(NodeVerdict::alive(job, 120));
    }
    let publish = PublishSection::default();
    let round_id = storage.start_round("test", None).unwrap();
    let source_id = storage.source_id_for_key("air").unwrap().unwrap();
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "test", None, std::time::Duration::ZERO)
        .unwrap()
    else {
        panic!("each round in this test starts from a free slot");
    };
    storage.attach_source_run(run_id, round_id).unwrap();
    let outcome = publish_round(
        storage,
        export_root,
        PublishRoundRequest {
            publish: &publish,
            sources: &specs,
            collected: &collected,
            verdicts: &verdicts,
            guard: GuardDecision::ApplyConvergence,
            measured: true,
            actor: "test",
        },
    );
    storage.close_source_run(run_id, STATUS_COMPLETED).unwrap();
    let _ = round_id;
    outcome
}

/// G-8：100 节点首次发布 + 同输入重跑。
#[tokio::test]
async fn one_hundred_nodes_publish_one_artifact_and_the_rerun_writes_nothing() {
    let storage = Storage::open_in_memory().unwrap();
    let export_root = tempfile::tempdir().unwrap();
    let source_id = storage
        .sync_source("air", "Air", ledger_sources::INPUT_COLLECTION, "air", true)
        .unwrap();
    storage.ensure_artifact(&source_id, "air.yaml").unwrap();

    let nodes: Vec<(String, String)> = (0..100)
        .map(|i| (format!("fp-{i:03}"), format!("Node {i:03}")))
        .collect();
    let _ = &nodes;
    let refs: Vec<(&str, &str)> = nodes
        .iter()
        .map(|(fp, display)| (fp.as_str(), display.as_str()))
        .collect();

    let outcome = run_publish_leg(&storage, export_root.path(), &refs);
    assert_eq!(outcome.skipped, None);
    assert_eq!(outcome.sources.len(), 1);
    let SourcePublication::Published { node_count, .. } = &outcome.sources[0] else {
        panic!("expected a publication, got {:?}", outcome.sources[0]);
    };
    assert_eq!(*node_count, 100, "all 100 alive nodes publish");
    // 1 source / 1 artifact / 1 活跃快照，正文 100 节点。
    assert_eq!(storage.list_artifacts().unwrap().len(), 1);
    let snapshot = storage
        .current_export("air")
        .unwrap()
        .expect("an active snapshot");
    assert_eq!(snapshot.summary.node_count, 100);
    assert!(
        snapshot.content.matches("\"name\"").count() >= 100,
        "100 proxies in the body"
    );

    // 同输入重跑：零写（NoChange/复用），artifact 仍 1。
    let rerun = run_publish_leg(&storage, export_root.path(), &refs);
    match &rerun.sources[0] {
        SourcePublication::Published { node_count, .. } => {
            assert_eq!(*node_count, 100);
            // 内容字节相同 → 快照复用（幂等门禁）。
        }
        other => panic!("the rerun still publishes from the ledger, got {other:?}"),
    }
    assert_eq!(
        storage.list_artifacts().unwrap().len(),
        1,
        "never a second artifact"
    );
    let same_snapshot = storage.current_export("air").unwrap().unwrap();
    assert_eq!(
        same_snapshot.summary.id, snapshot.summary.id,
        "byte-identical rerun reuses the snapshot"
    );
}

/// G-9：victim 三败跨轮——pending 段账本不判死，发布腿逐轮收敛。
#[tokio::test]
async fn a_dying_node_stays_pending_in_the_ledger_while_the_export_converges() {
    let storage = Storage::open_in_memory().unwrap();
    let export_root = tempfile::tempdir().unwrap();
    let source_id = storage
        .sync_source("air", "Air", ledger_sources::INPUT_COLLECTION, "air", true)
        .unwrap();
    storage.ensure_artifact(&source_id, "air.yaml").unwrap();

    // r1：victim + survivor 都过（账本与发布）。
    let pass = |fp: &'static str, delay: i64| ConvergeNode {
        source: "air",
        fingerprint: fp,
        display: "n",
        category: Some("direct"),
        ok: true,
        delay_ms: Some(delay),
        reason: None,
        ..Default::default()
    };
    let fail = |fp: &'static str, reason: &'static str| ConvergeNode {
        source: "air",
        fingerprint: fp,
        display: "n",
        category: None,
        ok: false,
        delay_ms: None,
        reason: Some(reason),
        ..Default::default()
    };

    let round1 = storage.start_round("test", None).unwrap();
    let summary = storage
        .converge_nodes(
            round1,
            &[pass("fp-victim", 100), pass("fp-survivor", 200)],
            &POLICY,
        )
        .unwrap();
    assert_eq!(summary.dropped, 0);
    assert_eq!(
        storage
            .get_node("air", "fp-victim")
            .unwrap()
            .unwrap()
            .status,
        "alive"
    );
    let outcome1 = run_publish_leg(
        &storage,
        export_root.path(),
        &[("fp-victim", "Victim"), ("fp-survivor", "Survivor")],
    );
    let content1 = storage
        .current_export("air")
        .unwrap()
        .unwrap()
        .content
        .clone();
    assert!(
        content1.contains("Victim") && content1.contains("Survivor"),
        "{content1}"
    );

    // r2：victim 本轮败（streak 1 < 3）——账本仍 alive（pending，绝不发布
    // 死亡）；发布腿的导出集 = 本轮通过者 → 内容收敛为 1 节点。
    let round2 = storage.start_round("test", None).unwrap();
    let summary = storage
        .converge_nodes(round2, &[fail("fp-victim", "timeout")], &POLICY)
        .unwrap();
    assert_eq!(summary.dropped, 0, "streak 1 of 3: pending, never dead");
    assert_eq!(
        storage
            .get_node("air", "fp-victim")
            .unwrap()
            .unwrap()
            .status,
        "pending",
        "streak 1: the ledger marks pending, never dead"
    );
    assert_eq!(
        storage
            .get_node("air", "fp-victim")
            .unwrap()
            .unwrap()
            .consec_fail,
        1
    );
    let outcome2 = run_publish_leg(&storage, export_root.path(), &[("fp-survivor", "Survivor")]);
    assert_eq!(
        storage.list_artifacts().unwrap().len(),
        1,
        "the artifact row never forks across the convergence"
    );
    let content2 = storage
        .current_export("air")
        .unwrap()
        .unwrap()
        .content
        .clone();
    assert!(
        !content2.contains("Victim"),
        "the export speaks this round's passers: {content2}"
    );
    assert!(content2.contains("Survivor"));

    // r3：再败（streak 2）——仍 pending。
    let round3 = storage.start_round("test", None).unwrap();
    let summary = storage
        .converge_nodes(round3, &[fail("fp-victim", "timeout")], &POLICY)
        .unwrap();
    assert_eq!(summary.dropped, 0, "streak 2 of 3");
    assert_eq!(
        storage
            .get_node("air", "fp-victim")
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
    let _ = run_publish_leg(&storage, export_root.path(), &[("fp-survivor", "Survivor")]);
    assert_eq!(storage.list_artifacts().unwrap().len(), 1);

    // r4：第三败（streak 3 ≥ 3）——死亡在账本侧只发生这一次。
    let round4 = storage.start_round("test", None).unwrap();
    let summary = storage
        .converge_nodes(round4, &[fail("fp-victim", "timeout")], &POLICY)
        .unwrap();
    assert_eq!(summary.dropped, 1, "the streak crossed the threshold");
    assert_eq!(
        storage
            .get_node("air", "fp-victim")
            .unwrap()
            .unwrap()
            .status,
        "dead"
    );
    let _ = run_publish_leg(&storage, export_root.path(), &[("fp-survivor", "Survivor")]);
    assert_eq!(
        storage.list_artifacts().unwrap().len(),
        1,
        "one artifact across the whole lifecycle"
    );
    // 稳定产物指针始终有效（稳定 URL 的 continuity 契约）。
    assert!(storage.current_export("air").unwrap().is_some());

    let _ = (outcome1, outcome2, SkipReason::NoAliveNodes);
    let _: _PathBufAlias = PathBuf::new();
}
