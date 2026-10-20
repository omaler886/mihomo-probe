use super::*;
use serde_json::json;

/// Open a fresh ledger and apply 0006 directly until the parent registers it.
fn create_storage() -> Storage {
    let storage = Storage::open_in_memory().unwrap();
    storage
        .conn
        .execute_batch(include_str!("../../../../migrations/0006_publication.sql"))
        .unwrap();
    storage
}

/// Build synthetic secret-bearing content for one named test proxy.
fn content(name: &str) -> String {
    json!({"proxies": [{
        "name": name, "type": "ss", "server": "example.invalid", "port": 443,
        "cipher": "aes-128-gcm", "password": "synthetic-private-value"
    }]})
    .to_string()
}

/// Publish one fixture using the current authoritative pointer as the CAS baseline.
fn publish(storage: &Storage, key: &str, content: &str, forced: bool) -> PublicationResult {
    let expected = storage
        .current_export(key)
        .unwrap()
        .map(|snapshot| snapshot.summary.id);
    storage
        .publish_export(PublicationInput {
            key,
            content,
            sha256: &export_hash(content),
            node_count: 1,
            actor: "test-operator",
            forced,
            expected_active: expected,
        })
        .unwrap()
}

/// Return a synthetic table's row count for transaction assertions.
fn count_rows(storage: &Storage, table: &str) -> i64 {
    assert!(["export_snapshots", "active_exports", "security_audit"].contains(&table));
    storage
        .conn
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// New ledgers have no implicit publication; successful publication is readable by key/ID.
#[test]
fn publication_activates_validated_content_and_metadata_only_lists() {
    let storage = create_storage();
    assert!(storage.current_export("air").unwrap().is_none());
    let content = content("first");
    let result = publish(&storage, "air", &content, false);
    assert!(!result.reused);
    assert!(result.changed);
    assert!(result.snapshot.is_active);
    let current = storage.current_export("air").unwrap().unwrap();
    assert_eq!(current.content, content);
    assert_eq!(
        storage
            .export_snapshot("air", result.snapshot.id)
            .unwrap()
            .unwrap()
            .content,
        content
    );
    let summaries = storage.list_exports(10, None).unwrap();
    assert_eq!(summaries, vec![result.snapshot]);
    let serialized = serde_json::to_string(&summaries).unwrap();
    for forbidden in [
        "content",
        "password",
        "synthetic-private-value",
        "example.invalid",
    ] {
        assert!(!serialized.contains(forbidden));
    }
}

/// Exact repeated bytes reuse even historical snapshots, and explicit force always audits.
#[test]
fn identical_content_is_idempotent_across_rollback_and_force() {
    let storage = create_storage();
    let first = publish(&storage, "air", &content("first"), false);
    for _ in 0..8 {
        let repeated = publish(&storage, "air", &content("first"), false);
        assert_eq!(repeated.snapshot.id, first.snapshot.id);
        assert!(repeated.reused);
        assert!(!repeated.changed);
    }
    assert_eq!(count_rows(&storage, "export_snapshots"), 1);
    let second = publish(&storage, "air", &content("second"), false);
    storage
        .rollback_export(
            "air",
            first.snapshot.id,
            "operator",
            Some(second.snapshot.id),
        )
        .unwrap();
    let repeated = publish(&storage, "air", &content("second"), true);
    assert_eq!(repeated.snapshot.id, second.snapshot.id);
    assert!(repeated.reused);
    assert!(repeated.changed);
    publish(&storage, "air", &content("second"), true);
    let forced: i64 = storage
        .conn
        .query_row(
            "SELECT COUNT(*) FROM security_audit WHERE kind = 'export_force_publish'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(forced, 2);
    assert_eq!(count_rows(&storage, "export_snapshots"), 2);
}

/// Reused content is scoped by key; one source may not roll back to another's snapshot.
#[test]
fn wrong_key_rollback_and_lookup_do_not_expose_or_activate_other_content() {
    let storage = create_storage();
    let first = publish(&storage, "air", &content("same"), false);
    let other = publish(&storage, "other", &content("same"), false);
    assert_ne!(first.snapshot.id, other.snapshot.id);
    assert!(storage
        .export_snapshot("air", other.snapshot.id)
        .unwrap()
        .is_none());
    let audits = count_rows(&storage, "security_audit");
    assert!(storage
        .rollback_export(
            "air",
            other.snapshot.id,
            "operator",
            Some(first.snapshot.id)
        )
        .is_err());
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        first.snapshot.id
    );
    assert_eq!(count_rows(&storage, "security_audit"), audits);
}

/// A successful rollback changes authority, not history, and records safe metadata.
#[test]
fn rollback_keeps_history_and_audits_only_allowed_fields() {
    let storage = create_storage();
    let first = publish(&storage, "air", &content("old"), false);
    let second = publish(&storage, "air", &content("new"), false);
    let rolled_back = storage
        .rollback_export(
            "air",
            first.snapshot.id,
            "operator",
            Some(second.snapshot.id),
        )
        .unwrap();
    assert_eq!(rolled_back.id, first.snapshot.id);
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().content,
        content("old")
    );
    let history = storage.export_history("air", 10, None).unwrap();
    assert_eq!(history.len(), 2);
    assert!(!history[0].is_active);
    assert!(history[1].is_active);
    let detail: String = storage
        .conn
        .query_row(
            "SELECT detail FROM security_audit WHERE kind = 'export_rollback'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let detail: Value = serde_json::from_str(&detail).unwrap();
    let mut keys: Vec<_> = detail
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "actor",
            "key",
            "node_count",
            "previous_id",
            "sha256",
            "snapshot_id"
        ]
    );
    assert_eq!(detail["actor"], "operator");
    assert_eq!(detail["previous_id"], second.snapshot.id);
    assert!(!detail.to_string().contains("synthetic-private-value"));
}

/// All three write stages and the audit are one transaction, including initial publication.
#[test]
fn failed_publication_transactions_preserve_old_content_and_counts() {
    for stage in ["export_snapshots", "active_exports", "security_audit"] {
        for has_previous in [false, true] {
            let storage = create_storage();
            let previous = has_previous.then(|| publish(&storage, "air", &content("old"), false));
            let expected = previous.as_ref().map(|result| result.snapshot.id);
            let before = [
                count_rows(&storage, "export_snapshots"),
                count_rows(&storage, "security_audit"),
            ];
            storage
                .conn
                .execute_batch(&format!(
                    "CREATE TRIGGER fail_publication BEFORE INSERT ON {stage}
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
                ))
                .unwrap();
            let next = content("new");
            assert!(storage
                .publish_export(PublicationInput {
                    key: "air",
                    content: &next,
                    sha256: &export_hash(&next),
                    node_count: 1,
                    actor: "test",
                    forced: false,
                    expected_active: expected,
                })
                .is_err());
            assert_eq!(
                storage
                    .current_export("air")
                    .unwrap()
                    .map(|snapshot| snapshot.summary.id),
                expected
            );
            assert_eq!(count_rows(&storage, "export_snapshots"), before[0]);
            assert_eq!(count_rows(&storage, "security_audit"), before[1]);
        }
    }
}

/// A failed rollback audit also rolls back the pointer update.
#[test]
fn failed_rollback_transaction_preserves_authority() {
    let storage = create_storage();
    let first = publish(&storage, "air", &content("old"), false);
    let second = publish(&storage, "air", &content("new"), false);
    storage
        .conn
        .execute_batch(
            "CREATE TRIGGER fail_audit BEFORE INSERT ON security_audit
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    assert!(storage
        .rollback_export("air", first.snapshot.id, "test", Some(second.snapshot.id))
        .is_err());
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        second.snapshot.id
    );
    assert_eq!(count_rows(&storage, "security_audit"), 2);
}

/// A deferred foreign-key failure at COMMIT rolls back snapshot, pointer, and audit.
#[test]
fn failed_commit_does_not_leave_a_partial_publication() {
    let storage = create_storage();
    let first = publish(&storage, "air", &content("old"), false);
    storage
        .conn
        .execute_batch(
            "PRAGMA foreign_keys = ON;
         CREATE TABLE commit_parent(id INTEGER PRIMARY KEY);
         CREATE TABLE commit_child(parent_id INTEGER REFERENCES commit_parent(id)
           DEFERRABLE INITIALLY DEFERRED);
         CREATE TRIGGER fail_commit AFTER INSERT ON security_audit
           BEGIN INSERT INTO commit_child(parent_id) VALUES(999); END;",
        )
        .unwrap();
    let next = content("new");
    assert!(storage
        .publish_export(PublicationInput {
            key: "air",
            content: &next,
            sha256: &export_hash(&next),
            node_count: 1,
            actor: "test",
            forced: false,
            expected_active: Some(first.snapshot.id),
        })
        .is_err());
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        first.snapshot.id
    );
    assert_eq!(count_rows(&storage, "export_snapshots"), 1);
    assert_eq!(count_rows(&storage, "security_audit"), 1);
}

/// Separate connections cannot reuse a stale read to override a newer publication.
#[test]
fn independent_connections_enforce_the_same_cas_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("publication.sqlite");
    let first = Storage::open(&path).unwrap();
    first
        .conn
        .execute_batch(include_str!("../../../../migrations/0006_publication.sql"))
        .unwrap();
    let second = Storage::open(&path).unwrap();
    let original = publish(&first, "air", &content("original"), false);
    let baseline = second.current_export("air").unwrap().unwrap().summary.id;
    assert_eq!(baseline, original.snapshot.id);
    let newer = publish(&first, "air", &content("newer"), false);
    assert!(second
        .rollback_export("air", baseline, "operator", Some(baseline))
        .is_err());
    assert_eq!(
        second.current_export("air").unwrap().unwrap().summary.id,
        newer.snapshot.id
    );
}

/// Both writers refuse a stale baseline instead of overwriting a newer decision.
#[test]
fn stale_cas_rejects_publication_and_rollback() {
    let storage = create_storage();
    let first = publish(&storage, "air", &content("old"), false);
    let second = publish(&storage, "air", &content("new"), false);
    let next = content("next");
    assert!(storage
        .publish_export(PublicationInput {
            key: "air",
            content: &next,
            sha256: &export_hash(&next),
            node_count: 1,
            actor: "test",
            forced: true,
            expected_active: Some(first.snapshot.id),
        })
        .is_err());
    assert!(storage
        .rollback_export("air", first.snapshot.id, "test", Some(first.snapshot.id))
        .is_err());
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        second.snapshot.id
    );
    assert_eq!(count_rows(&storage, "export_snapshots"), 2);
}

/// Invalid key/hash/count/content/actor input never writes a snapshot or audit.
#[test]
fn invalid_publication_inputs_are_rejected_without_writes() {
    let storage = create_storage();
    let good = content("good");
    let oversized = " ".repeat(MAX_EXPORT_BYTES + 1);
    let invalid_cases = [
        ("../air", good.as_str(), export_hash(&good), 1, "test"),
        ("air", good.as_str(), "0".repeat(64), 1, "test"),
        ("air", good.as_str(), export_hash(&good), 0, "test"),
        ("air", good.as_str(), export_hash(&good), 2, "test"),
        (
            "air",
            good.as_str(),
            export_hash(&good),
            MAX_EXPORT_NODES + 1,
            "test",
        ),
        ("air", "{}", export_hash("{}"), 1, "test"),
        (
            "air",
            r#"{"proxies":[]}"#,
            export_hash(r#"{"proxies":[]}"#),
            1,
            "test",
        ),
        (
            "air",
            r#"{"proxies":[3]}"#,
            export_hash(r#"{"proxies":[3]}"#),
            1,
            "test",
        ),
        (
            "air",
            oversized.as_str(),
            export_hash(&oversized),
            1,
            "test",
        ),
        ("air", good.as_str(), export_hash(&good), 1, "bad\nactor"),
    ];
    for (key, content, hash, node_count, actor) in invalid_cases {
        assert!(storage
            .publish_export(PublicationInput {
                key,
                content,
                sha256: &hash,
                node_count,
                actor,
                forced: true,
                expected_active: None,
            })
            .is_err());
    }
    assert_eq!(count_rows(&storage, "export_snapshots"), 0);
    assert_eq!(count_rows(&storage, "security_audit"), 0);
}

/// Corruption of retained content/hash/count cannot be used for lookup or rollback.
#[test]
fn corrupt_snapshots_are_not_rollback_candidates() {
    for assignment in [
        "content = '{}'",
        "sha256 = 'bad'",
        "node_count = 0",
        "node_count = 2",
        "content = CAST(zeroblob(8388609) AS TEXT)",
    ] {
        let storage = create_storage();
        let first = publish(&storage, "air", &content("old"), false);
        let second = publish(&storage, "air", &content("new"), false);
        storage
            .conn
            .execute(
                &format!("UPDATE export_snapshots SET {assignment} WHERE id = ?1"),
                [first.snapshot.id],
            )
            .unwrap();
        assert!(storage.export_snapshot("air", first.snapshot.id).is_err());
        assert!(storage
            .rollback_export("air", first.snapshot.id, "test", Some(second.snapshot.id))
            .is_err());
        assert_eq!(
            storage.current_export("air").unwrap().unwrap().summary.id,
            second.snapshot.id
        );
    }
}

/// Metadata queries never read content, even for a retained corrupt/huge legacy payload.
#[test]
fn history_is_bounded_and_cursor_paged_without_loading_content() {
    let storage = create_storage();
    for index in 0..MAX_EXPORT_PAGE + 3 {
        publish(&storage, "air", &content(&format!("node-{index}")), false);
    }
    let first = storage
        .export_history("air", MAX_EXPORT_PAGE, None)
        .unwrap();
    assert_eq!(first.len(), MAX_EXPORT_PAGE);
    let second = storage
        .export_history("air", MAX_EXPORT_PAGE, Some(first.last().unwrap().id))
        .unwrap();
    assert_eq!(second.len(), 3);
    assert!(second.iter().all(|entry| !entry.is_active));
    assert!(storage.export_history("air", 0, None).is_err());
    assert!(storage
        .export_history("air", MAX_EXPORT_PAGE + 1, None)
        .is_err());
    assert!(storage.export_history("air", 2, Some(-1)).is_err());
    let oldest = second.last().unwrap().id;
    storage
        .conn
        .execute(
            "UPDATE export_snapshots SET content = 'broken' WHERE id = ?1",
            [oldest],
        )
        .unwrap();
    assert_eq!(
        storage
            .export_history("air", 3, Some(first.last().unwrap().id))
            .unwrap()
            .len(),
        3
    );
    assert!(storage.export_snapshot("air", oldest).is_err());
}

/// Active listing pagination is independent from snapshot history ordering.
#[test]
fn active_lists_are_bounded_and_key_paged() {
    let storage = create_storage();
    for key in ["first", "second", "third"] {
        publish(&storage, key, &content("node"), false);
    }
    let page = storage.list_exports(2, None).unwrap();
    assert_eq!(
        page.iter()
            .map(|summary| summary.key.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    let remaining = storage.list_exports(2, Some("second")).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].key, "third");
    assert!(storage.list_exports(0, None).is_err());
    assert!(storage.list_exports(MAX_EXPORT_PAGE + 1, None).is_err());
}

/// Migration preserves legacy snapshots and audit rows without guessing active state.
#[test]
fn additive_migration_preserves_legacy_data_and_can_be_reapplied() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("../../../../migrations/0002_probe_ledger.sql"))
        .unwrap();
    conn.execute(
        "INSERT INTO export_snapshots(key, created_at, node_count, sha256, content)
         VALUES('air', '2026-01-01T00:00:00', 1, ?1, 'proxies: []')",
        ["legacy-hash"],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO security_audit(ts, kind, detail) VALUES('old', 'legacy', 'retained')",
    )
    .unwrap();
    let migration = include_str!("../../../../migrations/0006_publication.sql");
    conn.execute_batch(migration).unwrap();
    conn.execute_batch(migration).unwrap();
    let retained: String = conn
        .query_row("SELECT content FROM export_snapshots", [], |row| row.get(0))
        .unwrap();
    assert_eq!(retained, "proxies: []");
    let active: i64 = conn
        .query_row("SELECT COUNT(*) FROM active_exports", [], |row| row.get(0))
        .unwrap();
    assert_eq!(active, 0);
    let audits: i64 = conn
        .query_row("SELECT COUNT(*) FROM security_audit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(audits, 1);
}

/// Ownership/immutability constraints work even if foreign_keys is disabled.
#[test]
fn migration_guards_active_ownership_update_and_delete() {
    let storage = create_storage();
    let first = publish(&storage, "air", &content("old"), false);
    storage
        .conn
        .execute_batch("PRAGMA foreign_keys = OFF")
        .unwrap();
    assert!(storage
        .conn
        .execute(
            "INSERT INTO active_exports(key, snapshot_id, activated_at) VALUES('wrong', ?1, 'now')",
            [first.snapshot.id],
        )
        .is_err());
    assert!(storage
        .conn
        .execute(
            "UPDATE active_exports SET key = 'wrong' WHERE key = 'air'",
            []
        )
        .is_err());
    assert!(storage
        .conn
        .execute(
            "DELETE FROM export_snapshots WHERE id = ?1",
            [first.snapshot.id]
        )
        .is_err());
    assert!(storage
        .conn
        .execute(
            "UPDATE export_snapshots SET content = '{}' WHERE id = ?1",
            [first.snapshot.id]
        )
        .is_err());
}

/// Paths and controls are rejected while ordinary Unicode source labels remain legal.
#[test]
fn export_keys_are_checked_without_echoing_unsafe_input() {
    for key in [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        "a\\b",
        "C:drive",
        "a\0b",
        " a",
        "a.",
        "a\nb",
    ] {
        assert!(validate_export_key(key).is_err());
    }
    assert!(validate_export_key(&"x".repeat(MAX_EXPORT_KEY_CHARS + 1)).is_err());
    assert!(validate_export_key("source.one-2").is_ok());
    assert!(validate_export_key("\u{9999}\u{6e2f}").is_ok());
}
