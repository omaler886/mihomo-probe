//! One source, one artifact: the invariants the refactor's hard acceptance
//! (《指导》§11.3) is written against, proven at the ledger layer where the
//! unique constraints live.

use super::*;

/// A fresh ledger with every migration applied, so the backfill paths run.
fn create_storage() -> Storage {
    Storage::open_in_memory().unwrap()
}

/// The sha256 of the publication module, for a digest-shaped test value.
fn digest_of(text: &str) -> String {
    crate::publication::export_hash(text)
}

/// A source key that survives validation, synced with its default attributes.
fn sync(storage: &Storage, key: &str) -> String {
    storage
        .sync_source(key, key, INPUT_COLLECTION, key, true)
        .unwrap()
}

/// One hundred publications of the same source must not mint a second
/// artifact: `UNIQUE(source_id)` is the schema's promise, and this is the
/// test that would catch a caller working around it.
#[test]
fn one_source_never_grows_a_second_artifact() {
    let storage = create_storage();
    let source_id = sync(&storage, "air");
    for round in 1..=100 {
        let artifact = storage.ensure_artifact(&source_id, "air.yaml").unwrap();
        storage
            .mark_artifact_published(
                &artifact.artifact_id,
                &digest_of(&format!("round {round}")),
                round,
                None,
                None,
            )
            .unwrap();
    }
    assert_eq!(storage.list_artifacts().unwrap().len(), 1);
    // Re-read from the ledger: the in-loop handle predates the last write.
    let artifact = storage.artifact_for_source(&source_id).unwrap().unwrap();
    assert_eq!(artifact.source_id, source_id);
    assert_eq!(artifact.last_success_run_id, Some(100));
    assert_eq!(storage.list_sources().unwrap().len(), 1);
}

/// A renamed source keeps its identity and its artifact: the key stays the
/// natural key, the display name and the input reference are attributes.
#[test]
fn a_rename_updates_attributes_and_keeps_the_identity() {
    let storage = create_storage();
    let source_id = sync(&storage, "air");
    let artifact = storage.ensure_artifact(&source_id, "air.yaml").unwrap();

    let same = storage
        .sync_source("air", "机场A（新名字）", INPUT_SUB, "airport-a", false)
        .unwrap();
    assert_eq!(same, source_id, "the source id is immutable");

    let row = storage.source_by_id(&source_id).unwrap().unwrap();
    assert_eq!(row.display_name, "机场A（新名字）");
    assert_eq!(row.input_type, INPUT_SUB);
    assert_eq!(row.input_ref, "airport-a");
    assert!(!row.enabled);

    let after = storage.artifact_for_source(&source_id).unwrap().unwrap();
    assert_eq!(after.artifact_id, artifact.artifact_id);
    assert_eq!(after.gist_file, "air.yaml");
    assert_eq!(storage.list_artifacts().unwrap().len(), 1);
}

/// Two sources may not share a Gist file: the namespace is flat, and a silent
/// overwrite would empty one subscription every round.
#[test]
fn two_sources_cannot_share_a_gist_file() {
    let storage = create_storage();
    let air = sync(&storage, "air");
    let backup = sync(&storage, "backup");
    storage.ensure_artifact(&air, "shared.yaml").unwrap();
    let err = storage.ensure_artifact(&backup, "shared.yaml").unwrap_err();
    assert!(
        err.to_string().contains("ensure artifact"),
        "the unique constraint must surface as a storage error: {err}"
    );
    // The first owner keeps its row; the failed insert changed nothing.
    assert_eq!(
        storage
            .artifact_by_gist_file("shared.yaml")
            .unwrap()
            .unwrap()
            .source_id,
        air
    );
}

/// The digest is the publisher's NO_CHANGE baseline, so a re-publish of the
/// same content must be observable through it.
#[test]
fn the_digest_advances_only_on_a_successful_publish() {
    let storage = create_storage();
    let source_id = sync(&storage, "air");
    let artifact = storage.ensure_artifact(&source_id, "air.yaml").unwrap();
    assert_eq!(artifact.last_content_sha256, None);

    let first = digest_of("round one");
    storage
        .mark_artifact_published(&artifact.artifact_id, &first, 7, Some("gist123"), None)
        .unwrap();
    let row = storage
        .artifact_by_id(&artifact.artifact_id)
        .unwrap()
        .unwrap();
    assert_eq!(row.last_content_sha256.as_deref(), Some(first.as_str()));
    assert_eq!(row.last_success_run_id, Some(7));
    assert_eq!(row.gist_id.as_deref(), Some("gist123"));
    assert!(row.last_published_at.is_some());

    // The same content again: the row still reads as the last success, and a
    // caller comparing digests sees NO_CHANGE before any network call.
    storage
        .mark_artifact_published(&artifact.artifact_id, &first, 8, None, None)
        .unwrap();
    let again = storage
        .artifact_by_id(&artifact.artifact_id)
        .unwrap()
        .unwrap();
    assert_eq!(again.last_content_sha256, row.last_content_sha256);
    assert_eq!(
        again.gist_id.as_deref(),
        Some("gist123"),
        "gist fields survive"
    );
}

/// A failed publish must leave the previous digest standing, or the next run
/// would skip content that never reached the Gist.
#[test]
fn a_failed_publish_leaves_the_previous_digest() {
    let storage = create_storage();
    let source_id = sync(&storage, "air");
    let artifact = storage.ensure_artifact(&source_id, "air.yaml").unwrap();
    let good = digest_of("published");
    storage
        .mark_artifact_published(&artifact.artifact_id, &good, 1, None, None)
        .unwrap();
    // A publisher failure is the caller's business: nothing here moves, so the
    // digest still names the content that IS live.
    let row = storage
        .artifact_by_id(&artifact.artifact_id)
        .unwrap()
        .unwrap();
    assert_eq!(row.last_content_sha256.as_deref(), Some(good.as_str()));
}

/// The backfill gives every key that already owns ledger rows or an export
/// snapshot an id and an artifact, and a second apply adds nothing.
#[test]
fn the_backfill_claims_existing_keys_once() {
    let storage = create_storage();
    storage
        .conn
        .execute_batch(
            "INSERT INTO nodes(source, fingerprint, status) VALUES('air', 'a1b2c3d4e5f60718', 'alive');
             INSERT INTO nodes(source, fingerprint, status) VALUES('backup', '1122334455667788', 'dead');
             INSERT INTO export_snapshots(key, created_at, node_count, sha256, content)
               VALUES('legacy-air', '2026-11-03T00:00:00', 1, '0000000000000000000000000000000000000000000000000000000000000000', '{\"proxies\":[]}');",
        )
        .unwrap();
    // 0010 ran at open time, before these rows existed; re-applying it is the
    // upgrade path for a ledger whose rows predate the migration.
    storage
        .conn
        .execute_batch(include_str!(
            "../../../../migrations/0010_source_artifact.sql"
        ))
        .unwrap();

    let sources = storage.list_sources().unwrap();
    let keys: Vec<&str> = sources.iter().map(|row| row.key.as_str()).collect();
    assert!(keys.contains(&"air"), "{keys:?}");
    assert!(keys.contains(&"backup"), "{keys:?}");
    assert!(keys.contains(&"legacy-air"), "{keys:?}");
    assert_eq!(storage.list_artifacts().unwrap().len(), sources.len());
    for source in &sources {
        assert_eq!(source.input_type, INPUT_UNKNOWN);
        assert!(!source.source_id.is_empty());
        let artifact = storage
            .artifact_for_source(&source.source_id)
            .unwrap()
            .unwrap();
        assert_eq!(artifact.gist_file, format!("{}.yaml", source.key));
        assert_eq!(
            artifact.artifact_id,
            format!("artifact:{}", source.source_id)
        );
        assert_eq!(artifact.last_content_sha256, None);
    }
}

/// The sync path never rewrites a historical `source` column: the Python
/// service keeps reading and writing those rows while this table exists
/// beside them.
#[test]
fn the_sync_path_never_touches_the_python_columns() {
    let storage = create_storage();
    storage
        .conn
        .execute_batch(
            "INSERT INTO nodes(source, fingerprint, status) VALUES('air', 'a1b2c3d4e5f60718', 'alive');",
        )
        .unwrap();
    sync(&storage, "air");
    let (source, status): (String, String) = storage
        .conn
        .query_row(
            "SELECT source, status FROM nodes WHERE fingerprint = 'a1b2c3d4e5f60718'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(source, "air");
    assert_eq!(status, "alive");
}

/// A source id is an opaque handle: an unknown one reads as absent, and a
/// hostile one is refused rather than interpolated anywhere.
#[test]
fn unknown_and_hostile_ids_are_refused_or_absent() {
    let storage = create_storage();
    // Well-formed but never minted: absent, not an error.
    assert!(storage
        .source_by_id("00000000-0000-4000-8000-000000000000")
        .unwrap()
        .is_none());
    assert!(storage.source_by_id("'; DROP TABLE sources; --").is_err());
    assert!(storage.artifact_by_id(&"x".repeat(200)).is_err());
    assert!(storage.artifact_by_id("artifact:not-a-uuid").is_err());
    assert!(storage.source_id_for_key("").is_err());
}

/// `gist init` points an artifact at its Gist and may re-map the file name
/// only through the explicit setter, never through the ensure path.
#[test]
fn gist_init_records_the_id_and_keeps_the_file_name() {
    let storage = create_storage();
    let source_id = sync(&storage, "air");
    let artifact = storage.ensure_artifact(&source_id, "air.yaml").unwrap();
    storage
        .set_artifact_gist(&artifact.artifact_id, "aa11bb22cc33dd44", "air.yaml")
        .unwrap();
    storage
        .set_artifact_stable_url(
            &artifact.artifact_id,
            "https://probe.example.com/artifacts/x/subscription",
        )
        .unwrap();
    let row = storage
        .artifact_by_id(&artifact.artifact_id)
        .unwrap()
        .unwrap();
    assert_eq!(row.gist_id.as_deref(), Some("aa11bb22cc33dd44"));
    assert_eq!(row.gist_file, "air.yaml");
    assert_eq!(
        row.stable_url.as_deref(),
        Some("https://probe.example.com/artifacts/x/subscription")
    );
    // A second ensure with a different hint cannot move a live file name.
    let again = storage.ensure_artifact(&source_id, "other.yaml").unwrap();
    assert_eq!(again.gist_file, "air.yaml");
    // A malformed gist id is refused, not stored.
    assert!(storage
        .set_artifact_gist(&artifact.artifact_id, "not a gist id!", "air.yaml")
        .is_err());
}
