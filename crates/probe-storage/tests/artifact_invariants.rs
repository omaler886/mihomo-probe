//! Artifact invariants under the full per-source loop (《指导》§11.3): publish
//! and run bookkeeping interleaved, the way the CLI/API actually drives a
//! round -- claim a run, attach the round, publish, record the publish
//! status, close the run -- repeated past the point where a second artifact
//! or a second open slot could sneak in.
//!
//! `sources/tests.rs` pins the pure artifact loop (100 publications, rename,
//! digest rules) and `runs/tests.rs` pins the slot rules; this file pins the
//! interleaving of the two, which is where a caller-order regression would
//! land.

use std::time::Duration;

use probe_storage::publication::{export_hash, PublicationInput};
use probe_storage::runs::{
    ClaimOutcome, PUBLISH_FAILED, PUBLISH_NO_CHANGE, PUBLISH_PUBLISHED, STATUS_COMPLETED,
};
use probe_storage::{sources as ledger_sources, Storage};

const CONTENT: &str = "{\"proxies\": [{\"name\": \"a\", \"type\": \"ss\"}]}\n";

/// The per-source round loop, one iteration: claim -> attach -> publish ->
/// record the publish status -> close. Returns the run row.
fn one_round(storage: &Storage, source_id: &str, digest: &str, status: &'static str) {
    let round_id = storage.start_round("test", None).unwrap();
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(source_id, "test", None, Duration::ZERO)
        .unwrap()
    else {
        panic!("the previous round's slot closed; this claim must own a fresh one");
    };
    storage.attach_source_run(run_id, round_id).unwrap();
    let artifact = storage.artifact_for_source(source_id).unwrap().unwrap();
    // CAS as the engine does: name the active snapshot the publication
    // replaces (None only when nothing is active yet).
    let expected_active = storage
        .current_export("air")
        .unwrap()
        .map(|snapshot| snapshot.summary.id);
    storage
        .publish_export(PublicationInput {
            key: "air",
            content: CONTENT,
            sha256: &export_hash(CONTENT),
            node_count: 1,
            actor: "test",
            forced: false,
            expected_active,
        })
        .unwrap();
    if status == PUBLISH_PUBLISHED {
        storage
            .mark_artifact_published(&artifact.artifact_id, digest, round_id, None, None)
            .unwrap();
    }
    storage
        .record_source_publish_status(source_id, round_id, status, None)
        .unwrap();
    storage.close_source_run(run_id, STATUS_COMPLETED).unwrap();
}

/// Publishes interleaved with run claims and closes still mint exactly one
/// artifact, one run row per round, and a digest that only ever advanced on
/// the rounds that really published.
#[test]
fn runs_and_publishes_interleaved_stay_one_artifact_one_row_per_round() {
    let storage = Storage::open_in_memory().unwrap();
    let source_id = storage
        .sync_source("air", "Air", ledger_sources::INPUT_COLLECTION, "air", true)
        .unwrap();
    storage.ensure_artifact(&source_id, "air.yaml").unwrap();
    let digest = export_hash(CONTENT);

    // Ten rounds: the first really publishes, the rest report NoChange (the
    // digest must not move on those), and the fifth alternates a failure in
    // -- the failure must not move the digest either.
    for round in 1..=10 {
        let status = match round {
            1 => PUBLISH_PUBLISHED,
            5 => PUBLISH_FAILED,
            _ => PUBLISH_NO_CHANGE,
        };
        one_round(&storage, &source_id, &digest, status);
    }

    assert_eq!(storage.list_artifacts().unwrap().len(), 1);
    let rows = storage.list_source_runs(&source_id, 50).unwrap();
    assert_eq!(rows.len(), 10, "one run row per round");
    // The history reads newest first; walk it in round order.
    let mut per_round: Vec<(i64, Option<&str>)> = rows
        .iter()
        .map(|row| (row.round_id.unwrap(), row.publish_status.as_deref()))
        .collect();
    per_round.sort_by_key(|(round_id, _)| *round_id);
    let statuses: Vec<Option<&str>> = per_round.into_iter().map(|(_, status)| status).collect();
    assert_eq!(
        statuses,
        vec![
            Some(PUBLISH_PUBLISHED),
            Some(PUBLISH_NO_CHANGE),
            Some(PUBLISH_NO_CHANGE),
            Some(PUBLISH_NO_CHANGE),
            Some(PUBLISH_FAILED),
            Some(PUBLISH_NO_CHANGE),
            Some(PUBLISH_NO_CHANGE),
            Some(PUBLISH_NO_CHANGE),
            Some(PUBLISH_NO_CHANGE),
            Some(PUBLISH_NO_CHANGE),
        ]
    );
    let artifact = storage.artifact_for_source(&source_id).unwrap().unwrap();
    assert_eq!(
        artifact.last_content_sha256.as_deref(),
        Some(digest.as_str()),
        "the digest names the one content the destination accepted"
    );
    assert_eq!(
        artifact.last_success_run_id,
        Some(1),
        "only round 1 published"
    );
    // Every slot closed: nothing is pinned open by the bookkeeping.
    let open: Vec<_> = rows.iter().filter(|row| is_open(row)).collect();
    assert!(open.is_empty(), "no run slot may stay open: {open:?}");
}

/// A row whose slot still owns the source (the single-flight mutex).
fn is_open(row: &probe_storage::runs::SourceRunRow) -> bool {
    row.status == probe_storage::runs::STATUS_CLAIMED
        || row.status == probe_storage::runs::STATUS_ACTIVE
}

/// A claim that never closes (a crashed round) does not fork the artifact:
/// the next round's publish still lands on the same row, and the abandoned
/// slot is history, not an open blocker after reaping.
#[test]
fn an_interrupted_run_does_not_fork_the_artifact() {
    let storage = Storage::open_in_memory().unwrap();
    let source_id = storage
        .sync_source("air", "Air", ledger_sources::INPUT_COLLECTION, "air", true)
        .unwrap();
    storage.ensure_artifact(&source_id, "air.yaml").unwrap();
    let digest = export_hash(CONTENT);

    // Round 1 claims, publishes, and dies before closing.
    let round_id = storage.start_round("test", None).unwrap();
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "test", None, Duration::ZERO)
        .unwrap()
    else {
        panic!();
    };
    storage.attach_source_run(run_id, round_id).unwrap();
    storage
        .publish_export(PublicationInput {
            key: "air",
            content: CONTENT,
            sha256: &digest,
            node_count: 1,
            actor: "test",
            forced: false,
            expected_active: None,
        })
        .unwrap();
    storage
        .mark_artifact_published(
            &storage
                .artifact_for_source(&source_id)
                .unwrap()
                .unwrap()
                .artifact_id,
            &digest,
            round_id,
            None,
            None,
        )
        .unwrap();
    // (no close_source_run -- the crash)

    // The crash also left the round row open. A live slot whose round is
    // still open is NOT an orphan -- the round may legitimately be running --
    // so the reaper must leave it alone until the round closes.
    let guarded = storage.reap_orphan_source_runs(Duration::ZERO).unwrap();
    assert_eq!(guarded, 0, "an open round protects its run slot");
    // The watchdog closes the dead round; only then is the slot an orphan.
    storage.finish_round(round_id, Some("orphan")).unwrap();
    let reaped = storage.reap_orphan_source_runs(Duration::ZERO).unwrap();
    assert_eq!(reaped, 1, "the crashed slot is the orphan");
    one_round(&storage, &source_id, &digest, PUBLISH_NO_CHANGE);

    assert_eq!(
        storage.list_artifacts().unwrap().len(),
        1,
        "a crashed run must not mint a second artifact"
    );
    let rows = storage.list_source_runs(&source_id, 50).unwrap();
    assert_eq!(rows.len(), 2, "the abandoned slot is history too");
    let abandoned: Vec<_> = rows
        .iter()
        .filter(|row| row.status == probe_storage::runs::STATUS_ABANDONED)
        .collect();
    assert_eq!(abandoned.len(), 1, "the crashed slot is marked abandoned");
    let open: Vec<_> = rows.iter().filter(|row| is_open(row)).collect();
    assert!(open.is_empty(), "reaping frees the slot for the retry");
}
