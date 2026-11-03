//! The single-flight rules of 《指导》§7, proven where they are enforced: the
//! database.

use super::*;
use crate::sources::INPUT_COLLECTION;

fn create_storage() -> Storage {
    Storage::open_in_memory().unwrap()
}

/// A synced source, ready to be claimed.
fn source(storage: &Storage, key: &str) -> String {
    storage
        .sync_source(key, key, INPUT_COLLECTION, key, true)
        .unwrap()
}

/// A real round row, because the spacing anchor reads `rounds`.
fn round(storage: &Storage) -> i64 {
    storage.start_round("api-manual", None).unwrap()
}

/// Two triggers for one source produce one run: the second is told to watch
/// the first, and the ledger holds exactly one open slot.
#[test]
fn one_source_holds_exactly_one_open_run() {
    let storage = create_storage();
    let source_id = source(&storage, "air");
    let first = storage
        .claim_source_run(&source_id, "api-manual", None, Duration::ZERO)
        .unwrap();
    let ClaimOutcome::Claimed { run_id } = first else {
        panic!("the first trigger owns the slot: {first:?}");
    };
    let second = storage
        .claim_source_run(&source_id, "api-manual", None, Duration::ZERO)
        .unwrap();
    assert_eq!(
        second,
        ClaimOutcome::Deduped {
            run_id,
            round_id: None,
            open: true,
        },
        "a second trigger duplicates the run in flight"
    );
    // And the schema would have refused a second open row anyway.
    let refused = storage.conn.execute(
        "INSERT INTO source_runs(source_id, round_id, trigger, status, idempotency_key, claimed_at, updated_at)
         VALUES(?1, NULL, 'api-manual', 'claimed', NULL, '2026-11-03T19:00:00', '2026-11-03T19:00:00')",
        params![source_id],
    );
    assert!(refused.is_err(), "the partial unique index is the mutex");
}

/// The same `Idempotency-Key` always resolves to the same run, even after that
/// run closed -- so a client that retries a timed-out request sees the
/// original run, not a duplicate measurement.
#[test]
fn one_idempotency_key_resolves_to_one_run() {
    let storage = create_storage();
    let source_id = source(&storage, "air");
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "api-manual", Some("key-1"), Duration::ZERO)
        .unwrap()
    else {
        panic!();
    };
    storage.attach_source_run(run_id, round(&storage)).unwrap();

    // In flight: the same key dedupes to the open slot.
    let again = storage
        .claim_source_run(&source_id, "api-manual", Some("key-1"), Duration::ZERO)
        .unwrap();
    assert!(matches!(
        again,
        ClaimOutcome::Deduped { run_id: id, open: true, .. } if id == run_id
    ));

    // Closed: still the same answer, and nothing new started.
    storage.close_source_run(run_id, STATUS_COMPLETED).unwrap();
    let after = storage
        .claim_source_run(&source_id, "api-manual", Some("key-1"), Duration::ZERO)
        .unwrap();
    assert!(matches!(
        after,
        ClaimOutcome::Deduped { run_id: id, open: false, .. } if id == run_id
    ));
    assert_eq!(storage.list_source_runs(&source_id, 50).unwrap().len(), 1);
    // A fresh key now gets a fresh slot.
    assert!(storage
        .claim_source_run(&source_id, "api-manual", Some("key-2"), Duration::ZERO)
        .unwrap()
        .is_claimed());
}

/// Reusing one key across two sources is a client bug, and the answer names
/// the other source's run rather than starting anything.
#[test]
fn a_key_cannot_span_two_sources() {
    let storage = create_storage();
    let air = source(&storage, "air");
    let backup = source(&storage, "backup");
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&air, "api-manual", Some("shared"), Duration::ZERO)
        .unwrap()
    else {
        panic!();
    };
    let cross = storage
        .claim_source_run(&backup, "api-manual", Some("shared"), Duration::ZERO)
        .unwrap();
    assert_eq!(cross, ClaimOutcome::KeyConflict { run_id });
    assert!(storage.open_source_run(&backup).unwrap().is_none());
}

/// A release frees both the slot and the key: the retry after a transient
/// busy state must be able to start a run, not be pinned to one that vanished.
#[test]
fn releasing_a_claim_frees_the_slot_and_the_key() {
    let storage = create_storage();
    let source_id = source(&storage, "air");
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "api-manual", Some("k"), Duration::ZERO)
        .unwrap()
    else {
        panic!();
    };
    storage.release_source_run(run_id).unwrap();
    assert!(storage.open_source_run(&source_id).unwrap().is_none());
    let retry = storage
        .claim_source_run(&source_id, "api-manual", Some("k"), Duration::ZERO)
        .unwrap();
    assert!(retry.is_claimed(), "the retry starts a real run: {retry:?}");
}

/// The spacing window answers a second trigger with the run that just happened.
#[test]
fn a_fast_second_trigger_is_coalesced_into_the_last_run() {
    let storage = create_storage();
    let source_id = source(&storage, "air");
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "api-manual", None, Duration::ZERO)
        .unwrap()
    else {
        panic!();
    };
    let round_id = round(&storage);
    storage.attach_source_run(run_id, round_id).unwrap();
    storage.close_source_run(run_id, STATUS_COMPLETED).unwrap();

    let soon = storage
        .claim_source_run(&source_id, "api-manual", None, Duration::from_secs(900))
        .unwrap();
    assert_eq!(
        soon,
        ClaimOutcome::Coalesced {
            run_id,
            round_id: Some(round_id)
        },
        "the source ran seconds ago"
    );
    // With no window the same trigger is accepted.
    assert!(storage
        .claim_source_run(&source_id, "api-manual", None, Duration::ZERO)
        .unwrap()
        .is_claimed());
}

/// A round that started an hour ago is outside the window, so the spacing rule
/// throttles rather than blocks.
#[test]
fn the_spacing_window_measures_from_the_round_start() {
    let storage = create_storage();
    let source_id = source(&storage, "air");
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "api-manual", None, Duration::ZERO)
        .unwrap()
    else {
        panic!();
    };
    let round_id = round(&storage);
    storage.attach_source_run(run_id, round_id).unwrap();
    storage
        .conn
        .execute(
            "UPDATE rounds SET started_at = '2026-11-03T09:00:00' WHERE id = ?1",
            params![round_id],
        )
        .unwrap();
    storage.close_source_run(run_id, STATUS_COMPLETED).unwrap();

    assert!(storage
        .claim_source_run(&source_id, "api-manual", None, Duration::from_secs(900))
        .unwrap()
        .is_claimed());
}

/// Crash recovery: a slot whose round is gone is abandoned, and only after the
/// budget, so a slot the live process just opened is never touched.
#[test]
fn a_stale_slot_is_reaped_a_fresh_one_is_not() {
    let storage = create_storage();
    let source_id = source(&storage, "air");
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "schedule", None, Duration::ZERO)
        .unwrap()
    else {
        panic!();
    };
    // Young: nothing is reaped while a live claim could still be attached.
    assert_eq!(
        storage
            .reap_orphan_source_runs(Duration::from_secs(600))
            .unwrap(),
        0
    );
    // Aged out with no round: abandoned, and the slot is free again.
    storage
        .conn
        .execute(
            "UPDATE source_runs SET claimed_at = '2026-11-03T09:00:00' WHERE id = ?1",
            params![run_id],
        )
        .unwrap();
    assert_eq!(
        storage
            .reap_orphan_source_runs(Duration::from_secs(600))
            .unwrap(),
        1
    );
    assert!(storage.open_source_run(&source_id).unwrap().is_none());
    assert_eq!(
        storage.source_run(run_id).unwrap().unwrap().status,
        STATUS_ABANDONED
    );
    // A finished round's slot is orphaned too.
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "schedule", None, Duration::ZERO)
        .unwrap()
    else {
        panic!();
    };
    let round_id = round(&storage);
    storage.attach_source_run(run_id, round_id).unwrap();
    storage.finish_round(round_id, Some("done")).unwrap();
    storage
        .conn
        .execute(
            "UPDATE source_runs SET claimed_at = '2026-11-03T09:00:00' WHERE id = ?1",
            params![run_id],
        )
        .unwrap();
    assert_eq!(
        storage
            .reap_orphan_source_runs(Duration::from_secs(600))
            .unwrap(),
        1
    );
}

/// Closing is idempotent and refuses an open status: a guard racing an
/// explicit close must not resurrect or corrupt the slot.
#[test]
fn closing_a_slot_is_idempotent_and_type_checked() {
    let storage = create_storage();
    let source_id = source(&storage, "air");
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "api-manual", None, Duration::ZERO)
        .unwrap()
    else {
        panic!();
    };
    storage.close_source_run(run_id, STATUS_FAILED).unwrap();
    storage.close_source_run(run_id, STATUS_COMPLETED).unwrap();
    assert_eq!(
        storage.source_run(run_id).unwrap().unwrap().status,
        STATUS_FAILED,
        "the first close wins"
    );
    assert!(storage.close_source_run(run_id, STATUS_ACTIVE).is_err());
    assert!(
        storage.release_source_run(run_id).is_err(),
        "not open any more"
    );
    assert!(storage.attach_source_run(999_999, 1).is_err());
}

/// A CLI one-shot records history through the same table without ever
/// disturbing an open slot.
#[test]
fn a_recorded_run_never_disturbs_an_open_slot() {
    let storage = create_storage();
    let source_id = source(&storage, "air");
    let ClaimOutcome::Claimed { run_id } = storage
        .claim_source_run(&source_id, "api-manual", None, Duration::ZERO)
        .unwrap()
    else {
        panic!();
    };
    let recorded = storage
        .record_source_run(&source_id, round(&storage), "cli-round", STATUS_COMPLETED)
        .unwrap();
    assert_eq!(recorded, None, "the open slot is the truth");
    storage.release_source_run(run_id).unwrap();
    let now = storage
        .record_source_run(&source_id, round(&storage), "cli-round", STATUS_COMPLETED)
        .unwrap();
    assert!(now.is_some());
    let rows = storage.list_source_runs(&source_id, 50).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, STATUS_COMPLETED);
    assert!(!rows[0].has_idempotency_key);
}

/// Hostile ids and keys are refused before they reach a statement.
#[test]
fn hostile_claim_inputs_are_refused() {
    let storage = create_storage();
    let source_id = source(&storage, "air");
    assert!(storage
        .claim_source_run(
            "'; DROP TABLE source_runs; --",
            "api-manual",
            None,
            Duration::ZERO
        )
        .is_err());
    assert!(storage
        .claim_source_run(&source_id, "", None, Duration::ZERO)
        .is_err());
    assert!(storage
        .claim_source_run(&source_id, "api-manual", Some(""), Duration::ZERO)
        .is_err());
    assert!(storage
        .claim_source_run(&source_id, "api-manual", Some("line\nfeed"), Duration::ZERO)
        .is_err());
    assert!(storage
        .claim_source_run(&source_id, &"t".repeat(40), None, Duration::ZERO)
        .is_err());
    // A well-formed key of the maximum length is fine.
    assert!(storage
        .claim_source_run(
            &source_id,
            "api-manual",
            Some(&"k".repeat(128)),
            Duration::ZERO
        )
        .unwrap()
        .is_claimed());
}
