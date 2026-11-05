//! Publish pipeline scenarios (《指导》§11.2) at the seams a round actually
//! crosses: `publish_round` over a real ledger and export root, then
//! `publish_round_remote_with` against a destination that mirrors what the
//! Gist publisher promises (sha-identical -> NoChange, unavailable -> Failed).
//!
//! These are deliberately composition tests, not unit tests: the unit layer
//! (probe-publish stub server, the `publish_remote` fixture) pins each branch
//! in isolation; here the branches must still hold when the local pipeline
//! materialises files, reuses snapshots and writes run bookkeeping. What they
//! cannot cover -- a real GitHub, a real Sub-Store -- is marked NOT_RUN in
//! workstreams/19_ACCEPTANCE_MAP.md rather than faked here.

use std::path::PathBuf;
use std::sync::Mutex;

use probe_config::{PublishSection, SourceSpec};
use probe_engine::publish::SkipReason;
use probe_engine::publish_remote::{
    publish_round_remote_with, RemotePublishReport, RemotePublishRequest,
};
use probe_engine::round::GuardDecision;
use probe_engine::{
    publish_round, Collected, Job, NodeVerdict, PublishRoundOutcome, PublishRoundRequest,
    SourcePublication,
};
use probe_publish::{ArtifactRef, PublishError, PublishErrorCode, PublishOutcome, Publisher};
use probe_storage::publication::export_hash;
use probe_storage::runs::{
    ClaimOutcome, PUBLISH_FAILED, PUBLISH_NO_CHANGE, PUBLISH_PUBLISHED, STATUS_COMPLETED,
};
use probe_storage::{sources as ledger_sources, Storage};

/// The configured source list: one enabled, exporting source.
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

/// A node in the round: its ledger fingerprint and upstream display name.
/// Changing either changes what the rendered export carries.
fn node(fingerprint: &str, display: &str) -> (Job, serde_json::Value) {
    let job = Job::new(
        "air",
        fingerprint,
        display,
        "direct",
        "entry.example.invalid",
    )
    .with_origin(
        display.to_string(),
        Some("entry.example.invalid".to_string()),
        Some("ss".to_string()),
    );
    let original = serde_json::json!({"name": display, "type": "ss",
        "server": "entry.example.invalid", "port": 443});
    (job, original)
}

/// Drive one round's legs, exactly as the CLI/API does: `publish_round` over
/// the ledger and export root, then the remote leg for every locally
/// published source against the caller-supplied destination. `nodes` is the
/// round's alive node set -- the input that decides the rendered content.
async fn one_round(
    storage: &Storage,
    export_root: &std::path::Path,
    nodes: &[(&str, &str)],
    guard: GuardDecision,
    measured: bool,
    publisher: &ShaGist,
) -> (i64, PublishRoundOutcome, RemotePublishReport) {
    let specs = specs();
    let mut collected = Collected::default();
    let mut verdicts = Vec::new();
    for (fingerprint, display) in nodes {
        let (job, original) = node(fingerprint, display);
        collected.originals.insert(
            ("air".to_string(), job.fingerprint.clone()),
            original.clone(),
        );
        verdicts.push(NodeVerdict::alive(job, 120));
    }
    let publish = PublishSection::default();
    // The real sequencing: the round row opens, the per-source run slot is
    // claimed and attached, the local leg commits, the remote leg records the
    // publish status ON that slot, and the slot closes.
    let round_id = storage.start_round("test", None).unwrap();
    let source_id = storage.source_id_for_key("air").unwrap().unwrap();
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "test", None, std::time::Duration::ZERO)
        .unwrap()
    else {
        panic!("a fresh ledger has a free run slot");
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
            guard,
            measured,
            actor: "test",
        },
    );
    let report = publish_round_remote_with(
        storage,
        RemotePublishRequest {
            publish: &publish,
            export_root,
            outcome: &outcome,
            round_id,
            sources: &specs,
        },
        publisher,
    )
    .await;
    storage.close_source_run(run_id, STATUS_COMPLETED).unwrap();
    (round_id, outcome, report)
}

/// One recorded publish call: (file, previous digest or None, content,
/// outcome). The destination is contacted every round (the real publisher
/// reads the remote sha first); what must stay bounded is the WRITE outcome.
type RecordedCall = (String, Option<String>, String, &'static str);

/// A destination that behaves like the Gist publisher's contract without a
/// network: a publish whose content hash already matches the artifact's
/// previous digest is a NoChange (zero writes), anything else "succeeds".
/// Failures are one-shot: the first publish after arming fails.
struct ShaGist {
    calls: Mutex<Vec<RecordedCall>>,
    fail_next: Mutex<Option<PublishErrorCode>>,
}

impl ShaGist {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail_next: Mutex::new(None),
        }
    }

    fn fail_next(&self, code: PublishErrorCode) {
        *self.fail_next.lock().unwrap() = Some(code);
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Calls whose outcome changed the destination (or tried to).
    fn write_count(&self) -> usize {
        self.calls()
            .iter()
            .filter(|(.., outcome)| *outcome != "no_change")
            .count()
    }
}

impl Publisher for ShaGist {
    async fn publish(&self, artifact: &ArtifactRef, content: &str) -> PublishOutcome {
        let armed = self.fail_next.lock().unwrap().take();
        let mut calls = self.calls.lock().unwrap();
        if let Some(code) = armed {
            calls.push((
                artifact.gist_file.clone(),
                artifact.previous_sha256.clone(),
                content.to_string(),
                "failed",
            ));
            return PublishOutcome::Failed {
                code,
                retryable: false,
                message: format!("stubbed failure: {}", code.as_str()),
            };
        }
        let digest = export_hash(content);
        if artifact.previous_sha256.as_deref() == Some(digest.as_str()) {
            calls.push((
                artifact.gist_file.clone(),
                artifact.previous_sha256.clone(),
                content.to_string(),
                "no_change",
            ));
            PublishOutcome::NoChange
        } else {
            calls.push((
                artifact.gist_file.clone(),
                artifact.previous_sha256.clone(),
                content.to_string(),
                "published",
            ));
            PublishOutcome::Published {
                revision: Some(digest[..12].to_string()),
                gist_failed: None,
            }
        }
    }

    async fn healthcheck(&self, _gist_id: &str) -> Result<(), PublishError> {
        Ok(())
    }
}

/// The round's local outcome must be exactly one published source; its
/// content path and snapshot id are what the stable URL serves from.
fn published_of(outcome: &PublishRoundOutcome) -> (String, i64, PathBuf) {
    match outcome.sources.first() {
        Some(SourcePublication::Published {
            key,
            snapshot_id,
            content_path,
            ..
        }) => (key.clone(), *snapshot_id, content_path.clone()),
        other => panic!("expected a publication, got {other:?}"),
    }
}

/// The active snapshot's bytes, or a panic naming the key.
fn active_content(storage: &Storage, key: &str) -> String {
    storage
        .current_export(key)
        .unwrap()
        .expect("an active snapshot")
        .content
}

/// The run row a round's bookkeeping left behind.
fn run_row(storage: &Storage, source_id: &str, round_id: i64) -> probe_storage::runs::SourceRunRow {
    storage
        .list_source_runs(source_id, 50)
        .unwrap()
        .into_iter()
        .find(|row| row.round_id == Some(round_id))
        .unwrap_or_else(|| panic!("round {round_id}: a run row must exist"))
}

/// 场景 2（《指导》§11.2）: the same input rerun ten times publishes once and
/// then rides on NoChange -- ten run rows, one artifact, one destination
/// write, the digest advancing exactly once.
#[tokio::test]
async fn rerunning_the_same_input_writes_the_destination_once() {
    let storage = Storage::open_in_memory().unwrap();
    let source_id = storage
        .sync_source("air", "Air", ledger_sources::INPUT_COLLECTION, "air", true)
        .unwrap();
    storage.ensure_artifact(&source_id, "air.yaml").unwrap();
    let root = tempfile::tempdir().unwrap();
    let publisher = ShaGist::new();

    for round in 1..=10 {
        let (round_id, outcome, report) = one_round(
            &storage,
            root.path(),
            &[("fp-alive", "live")],
            GuardDecision::ApplyConvergence,
            true,
            &publisher,
        )
        .await;
        assert!(outcome.skipped.is_none(), "round {round} must publish");
        let (key, snapshot_id, _path) = published_of(&outcome);
        assert_eq!(key, "air");
        // The remote leg always ran (a fake destination here, a configured
        // Gist in production): one entry per locally published source.
        assert_eq!(report.entries.len(), 1, "round {round}");
        let expected_status = if round == 1 {
            PUBLISH_PUBLISHED
        } else {
            PUBLISH_NO_CHANGE
        };
        assert_eq!(report.entries[0].status, expected_status);
        // And the run slot the caller bookkeeps carries the same status.
        let row = run_row(&storage, &source_id, round_id);
        assert_eq!(row.publish_status.as_deref(), Some(expected_status));
        let snapshot = storage.current_export("air").unwrap().unwrap();
        assert_eq!(snapshot.summary.id, snapshot_id, "round {round}");
    }

    // The three invariants the scenario demands, re-read from the ledger.
    assert_eq!(
        storage.list_artifacts().unwrap().len(),
        1,
        "ten reruns mint one artifact"
    );
    assert_eq!(
        storage.list_source_runs(&source_id, 50).unwrap().len(),
        10,
        "ten run rows of history"
    );
    assert_eq!(
        publisher.write_count(),
        1,
        "the destination is written exactly once; rounds 2..10 are NoChange"
    );
    let (file, previous, content, outcome) = &publisher.calls()[0];
    assert_eq!(*outcome, "published");
    assert_eq!(file, "air.yaml");
    assert_eq!(
        previous.as_deref(),
        None,
        "the first publish has no baseline"
    );
    let artifact = storage.artifact_for_source(&source_id).unwrap().unwrap();
    assert_eq!(
        artifact.last_content_sha256.as_deref(),
        Some(export_hash(content).as_str()),
        "the digest advanced exactly once, to the published bytes"
    );
}

/// 场景 4（《指导》§11.2）: a whole-round fault makes the guard withhold
/// publication; the previous snapshot stays active and the destination is
/// never contacted. The stable endpoint leg (HTTP) is pinned in
/// `crates/probe-api/tests/subscription_endpoint.rs`; this pins the ledger
/// seam that endpoint serves from.
#[tokio::test]
async fn a_guarded_round_withholds_publication_and_keeps_the_previous_snapshot() {
    let storage = Storage::open_in_memory().unwrap();
    let source_id = storage
        .sync_source("air", "Air", ledger_sources::INPUT_COLLECTION, "air", true)
        .unwrap();
    storage.ensure_artifact(&source_id, "air.yaml").unwrap();
    let root = tempfile::tempdir().unwrap();
    let publisher = ShaGist::new();

    // A healthy round first: content A is live locally and remotely.
    let (_, published, _) = one_round(
        &storage,
        root.path(),
        &[("fp-alive", "live")],
        GuardDecision::ApplyConvergence,
        true,
        &publisher,
    )
    .await;
    let (key, snapshot_id, content_path) = published_of(&published);
    assert_eq!(key, "air");
    assert!(content_path.exists(), "the local file was materialised");
    let writes_after_first_round = publisher.write_count();
    let content_a = active_content(&storage, "air");

    // The whole round then fails (kernel unreachable -> nothing measured ->
    // the alive floor cannot be trusted). Both guard shapes must withhold:
    // the suspect floor (`PreservePreviousState`) and an inconclusive round
    // that measured nothing (`MarkRoundInconclusive` + `measured == false`).
    for (guard, measured) in [
        (GuardDecision::PreservePreviousState, true),
        (GuardDecision::MarkRoundInconclusive, false),
    ] {
        let (_, outcome, report) = one_round(
            &storage,
            root.path(),
            &[("fp-alive", "live")],
            guard,
            measured,
            &publisher,
        )
        .await;
        assert_eq!(
            outcome.skipped,
            Some(SkipReason::GuardNotApplied),
            "{guard:?} must withhold the round"
        );
        assert!(outcome
            .sources
            .iter()
            .all(|source| matches!(source, SourcePublication::Skipped { .. })));
        assert!(
            report.entries.is_empty(),
            "a withheld round has no locally published source to carry"
        );
    }

    // Nothing moved: same snapshot active, same bytes, destination silent.
    let snapshot = storage.current_export("air").unwrap().unwrap();
    assert_eq!(snapshot.summary.id, snapshot_id, "the snapshot stands");
    assert_eq!(active_content(&storage, "air"), content_a);
    assert_eq!(
        publisher.write_count(),
        writes_after_first_round,
        "a guarded round never reaches the destination"
    );
    let artifact = storage.artifact_for_source(&source_id).unwrap().unwrap();
    assert_eq!(
        artifact.last_content_sha256.as_deref(),
        Some(export_hash(&content_a).as_str())
    );
}

/// 场景 6（《指导》§11.2）: the destination becomes unavailable after the
/// local leg committed. The run records `publish_failed` with the frozen
/// error code, the local snapshot for the new content stays authoritative,
/// and the artifact's digest still names the content the destination last
/// accepted -- so the next healthy round re-offers the same content.
#[tokio::test]
async fn an_unavailable_destination_records_the_failure_and_keeps_the_local_snapshot() {
    let storage = Storage::open_in_memory().unwrap();
    let source_id = storage
        .sync_source("air", "Air", ledger_sources::INPUT_COLLECTION, "air", true)
        .unwrap();
    storage.ensure_artifact(&source_id, "air.yaml").unwrap();
    let root = tempfile::tempdir().unwrap();
    let publisher = ShaGist::new();

    // Round 1 succeeds: content A live here and there.
    let (_, _, report) = one_round(
        &storage,
        root.path(),
        &[("fp-alive", "live")],
        GuardDecision::ApplyConvergence,
        true,
        &publisher,
    )
    .await;
    assert_eq!(report.entries[0].status, PUBLISH_PUBLISHED);
    let content_a = active_content(&storage, "air");
    assert_eq!(
        storage
            .artifact_for_source(&source_id)
            .unwrap()
            .unwrap()
            .last_content_sha256
            .as_deref(),
        Some(export_hash(&content_a).as_str())
    );

    // Round 2 measures a changed node list; the local leg commits the new
    // snapshot; the destination then answers 5xx past its retry budget.
    publisher.fail_next(PublishErrorCode::Remote);
    let (round_id, outcome, report) = one_round(
        &storage,
        root.path(),
        &[("fp-moved", "moved")],
        GuardDecision::ApplyConvergence,
        true,
        &publisher,
    )
    .await;
    let (_, _, new_path) = published_of(&outcome);
    assert!(
        new_path.exists(),
        "the local leg still materialises the file"
    );
    assert_eq!(report.entries[0].status, PUBLISH_FAILED);
    let row = storage
        .list_source_runs(&source_id, 50)
        .unwrap()
        .into_iter()
        .find(|row| row.round_id == Some(round_id))
        .expect("the failed round still leaves a run row");
    assert_eq!(
        row.publish_status.as_deref(),
        Some(PUBLISH_FAILED),
        "the run is marked publish_failed"
    );
    assert_eq!(
        row.error_code.as_deref(),
        Some(PublishErrorCode::Remote.as_str()),
        "the frozen error code is persisted"
    );

    // Local is authoritative: the new snapshot is active, the old file's
    // bytes no longer served. The destination's world is untouched: the
    // digest still names the last accepted content, so a consumer there
    // keeps reading the previous (stale, non-empty) list.
    let content_b = active_content(&storage, "air");
    assert_ne!(content_b, content_a, "the local snapshot moved forward");
    let artifact = storage.artifact_for_source(&source_id).unwrap().unwrap();
    assert_eq!(
        artifact.last_content_sha256.as_deref(),
        Some(export_hash(&content_a).as_str()),
        "the digest advances only after the destination accepted the content"
    );

    // And the retry story holds: the next healthy round offers B once, with
    // the destination's digest as its baseline, and the digest follows.
    let (_, _, report) = one_round(
        &storage,
        root.path(),
        &[("fp-moved", "moved")],
        GuardDecision::ApplyConvergence,
        true,
        &publisher,
    )
    .await;
    assert_eq!(report.entries[0].status, PUBLISH_PUBLISHED);
    let calls = publisher.calls();
    let last = calls.last().expect("the retry reached the destination");
    assert_eq!(last.2, content_b, "the retry offers the local snapshot");
    assert_eq!(
        last.1.as_deref(),
        Some(export_hash(&content_a).as_str()),
        "the retry carries the destination's digest as its baseline"
    );
    let artifact = storage.artifact_for_source(&source_id).unwrap().unwrap();
    assert_eq!(
        artifact.last_content_sha256.as_deref(),
        Some(export_hash(&content_b).as_str())
    );
}
