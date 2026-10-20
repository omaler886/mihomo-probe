use super::*;
use serde_json::json;
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use crate::limits::Job;
use probe_source::SubEntry;

/// Own an isolated private directory; remove only this newly created test tree.
struct TestRoot(PathBuf);

impl TestRoot {
    /// Create a unique root with owner-only Unix mode and return its canonical path.
    fn new() -> Self {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "probe-publication-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        // Owner-only on Unix; Windows inherits the temporary directory's rights.
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder
        };
        #[cfg(not(unix))]
        let builder = fs::DirBuilder::new();
        builder.create(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for TestRoot {
    /// Check containment again before cleaning only the test-created directory.
    fn drop(&mut self) {
        let base = std::env::temp_dir().canonicalize().unwrap();
        assert!(self.0.starts_with(&base) && self.0 != base);
        assert!(self
            .0
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("probe-publication-test-"));
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// Build a synthetic proxy with an original domain and distinct SNI.
fn proxy(name: &str) -> Value {
    json!({
        "name": name, "type": "vless", "server": "entry.example.invalid", "port": 443,
        "uuid": "00000000-0000-4000-8000-000000000001", "tls": true,
        "servername": "sni.example.invalid", "reality-opts": {
            "public-key": "synthetic-public-key", "short-id": "123456e2"
        }
    })
}

/// Return enabled, unguarded policy without a diff ceiling for synthetic tests.
fn policy() -> PublishPolicy {
    PublishPolicy {
        enabled: true,
        guarded: false,
        max_changed_identities: None,
        max_change_basis_points: None,
    }
}

/// Publish selected synthetic nodes; production callers must authorize the actor.
fn request<'a>(proxies: &'a [Value], policy: PublishPolicy, force: bool) -> PublishRequest<'a> {
    PublishRequest {
        key: "air",
        actor: "test-operator",
        proxies,
        groups: &[],
        policy,
        force,
    }
}

/// Quoted scalar strings survive both JSON and the workspace's real YAML parser.
#[test]
fn yaml_scalars_domains_sni_unicode_and_hashes_round_trip_unchanged() {
    let strings = [
        "123456e2",
        "000123",
        "0x12",
        "yes",
        "false",
        "null",
        "1:30",
        "#tag",
        "\u{9999}\u{6e2f}",
        "line\nbreak",
        "a: b",
        "deadbeef",
    ];
    for scalar in strings {
        let mut node = proxy("safe");
        node["password"] = json!(scalar);
        node["udp"] = json!(true);
        node["skip-cert-verify"] = json!(false);
        let output = render_clash_meta(&[node.clone()], &[]).unwrap();
        let json: Value = serde_json::from_str(output.content()).unwrap();
        assert_eq!(json["proxies"][0], node);
        let parsed_yaml = probe_source::parse_proxies(output.content()).unwrap();
        assert_eq!(parsed_yaml, vec![node]);
        assert_eq!(json["proxies"][0]["reality-opts"]["short-id"], "123456e2");
        assert_eq!(json["proxies"][0]["server"], "entry.example.invalid");
        assert_eq!(json["proxies"][0]["servername"], "sni.example.invalid");
        assert_eq!(output.sha256(), export_hash(output.content()));
    }
}

/// Serialization canonicalizes dictionary keys, not array ordering or field values.
#[test]
fn dictionary_order_is_idempotent_and_group_order_is_preserved() {
    let first: Value =
        serde_json::from_str(r#"{"name":"node","type":"ss","password":"secret"}"#).unwrap();
    let second: Value =
        serde_json::from_str(r#"{"password":"secret","type":"ss","name":"node"}"#).unwrap();
    assert_eq!(
        render_clash_meta(&[first], &[]).unwrap().content(),
        render_clash_meta(&[second], &[]).unwrap().content(),
    );
    let members = json!({"name": "hops", "type": "relay", "proxies": ["second", "first"]});
    let output = render_clash_meta(
        &[proxy("first"), proxy("second")],
        std::slice::from_ref(&members),
    )
    .unwrap();
    let document: Value = serde_json::from_str(output.content()).unwrap();
    assert_eq!(document["proxy-groups"][0], members);
}

/// Already-remapped single-hop and multi-front topologies are retained, not stripped.
#[test]
fn explicit_chain_and_group_closure_are_supported() {
    let mut landing = proxy("landing");
    landing["dialer-proxy"] = json!("front");
    let direct = render_clash_meta(&[landing.clone(), proxy("front")], &[]).unwrap();
    assert_eq!(
        parse_document(direct.content()).unwrap()["proxies"][0]["dialer-proxy"],
        "front"
    );
    landing["dialer-proxy"] = json!("front-pool");
    let group = json!({"name": "front-pool", "type": "fallback",
        "url": "https://example.invalid/204", "interval": 300, "proxies": ["front", "DIRECT"]});
    let output =
        render_clash_meta(&[landing, proxy("front")], std::slice::from_ref(&group)).unwrap();
    let document = parse_document(output.content()).unwrap();
    assert_eq!(document["proxy-groups"][0], group);
}

/// Duplicates, shadowed builtins, missing references, and every cycle fail closed.
#[test]
fn collisions_dangling_references_and_cycles_are_rejected() {
    assert!(render_clash_meta(&[proxy("same"), proxy("same")], &[]).is_err());
    assert!(render_clash_meta(&[proxy("DIRECT")], &[]).is_err());
    assert!(render_clash_meta(
        &[proxy("node")],
        &[json!({"name":"node","type":"select","proxies":["DIRECT"]})]
    )
    .is_err());
    let mut landing = proxy("landing");
    landing["dialer-proxy"] = json!("missing");
    assert!(render_clash_meta(&[landing.clone()], &[]).is_err());
    landing["dialer-proxy"] = json!("landing");
    assert!(render_clash_meta(&[landing.clone()], &[]).is_err());
    landing["dialer-proxy"] = json!("pool");
    assert!(render_clash_meta(
        &[landing],
        &[json!({"name":"pool","type":"select","proxies":["landing"]})]
    )
    .is_err());
    assert!(render_clash_meta(
        &[proxy("node")],
        &[json!({"name":"pool","type":"select","proxies":["missing"]})]
    )
    .is_err());
    let mut first = proxy("first");
    let mut second = proxy("second");
    first["dialer-proxy"] = json!("second");
    second["dialer-proxy"] = json!("first");
    assert!(render_clash_meta(&[first, second], &[]).is_err());
}

/// Provider-dependent groups and alias/coercion ambiguities require explicit caller repair.
#[test]
fn dynamic_membership_aliases_and_numeric_reality_ids_are_rejected() {
    for extra in [
        "use",
        "include-all",
        "include-all-providers",
        "include-all-proxies",
    ] {
        let mut group = json!({"name":"pool","type":"select","proxies":["node"]});
        group[extra] = json!(true);
        assert!(render_clash_meta(&[proxy("node")], &[group]).is_err());
    }
    let mut node = proxy("node");
    node["dialer_proxy"] = json!("DIRECT");
    assert!(render_clash_meta(&[node], &[]).is_err());
    let mut node = proxy("node");
    node["reality-opts"]["short-id"] = json!(123456);
    assert!(render_clash_meta(&[node], &[]).is_err());
    assert!(render_clash_meta(
        &[proxy("node")],
        &[json!({"name":"empty","type":"select","proxies":[]})]
    )
    .is_err());
}

/// The complete output and proxy count are bounded even when forcing would be requested.
#[test]
fn empty_oversized_or_excessive_output_is_rejected() {
    assert!(render_clash_meta(&[], &[]).is_err());
    let mut node = proxy("node");
    node["password"] = json!("x".repeat(MAX_EXPORT_BYTES));
    assert!(render_clash_meta(&[node], &[]).is_err());
    assert!(render_clash_meta(&vec![Value::Null; MAX_EXPORT_NODES + 1], &[]).is_err());
}

/// Force overrides only guard/diff policy, never disabled or zero-node publication.
#[test]
fn decisions_distinguish_disabled_empty_guarded_and_diff_limits() {
    assert_eq!(
        decide_publication(
            PublishPolicy {
                enabled: false,
                ..policy()
            },
            1,
            None,
            true
        )
        .unwrap(),
        PublishDecision::Disabled
    );
    assert_eq!(
        decide_publication(policy(), 0, None, true).unwrap(),
        PublishDecision::Empty
    );
    let guarded = PublishPolicy {
        guarded: true,
        ..policy()
    };
    assert_eq!(
        decide_publication(guarded, 1, None, false).unwrap(),
        PublishDecision::Guarded
    );
    assert_eq!(
        decide_publication(guarded, 1, None, true).unwrap(),
        PublishDecision::Publish { forced: true }
    );
    let diff = SnapshotDiff {
        changed: vec!["safe-id".into()],
        change_basis_points: 5000,
        ..Default::default()
    };
    let limited = PublishPolicy {
        max_changed_identities: Some(0),
        ..policy()
    };
    assert_eq!(
        decide_publication(limited, 1, Some(&diff), false).unwrap(),
        PublishDecision::DiffLimit
    );
    assert_eq!(
        decide_publication(limited, 1, Some(&diff), true).unwrap(),
        PublishDecision::Publish { forced: true }
    );
    let boundary = PublishPolicy {
        max_change_basis_points: Some(5000),
        ..policy()
    };
    assert_eq!(
        decide_publication(boundary, 1, Some(&diff), false).unwrap(),
        PublishDecision::Publish { forced: false }
    );
    assert!(decide_publication(
        PublishPolicy {
            max_change_basis_points: Some(10001),
            ..policy()
        },
        1,
        None,
        false
    )
    .is_err());
}

/// Credential-only changes are visible by opaque identity; no values/names leak in diffs.
#[test]
fn diffs_report_added_removed_changed_identities_without_credentials() {
    let mut before = proxy("name-with-secret-token");
    before["password"] = json!("old-password");
    let mut after = before.clone();
    after["password"] = json!("new-password");
    let previous = render_clash_meta(&[before, proxy("removed")], &[]).unwrap();
    let current = render_clash_meta(&[after, proxy("added")], &[]).unwrap();
    let diff = compare_snapshots(previous.content(), current.content()).unwrap();
    assert_eq!(
        (diff.added.len(), diff.removed.len(), diff.changed.len()),
        (1, 1, 1)
    );
    assert_eq!((diff.previous_count, diff.current_count), (2, 2));
    assert_eq!(diff.change_basis_points, 10_000);
    let serialized = serde_json::to_string(&diff).unwrap();
    for secret in [
        "old-password",
        "new-password",
        "name-with-secret-token",
        "example.invalid",
        "123456e2",
        "password",
    ] {
        assert!(!serialized.contains(secret));
    }
}

/// Disabled/empty/guarded requests write no file, even when a root is unavailable.
#[test]
fn skipped_publication_has_no_filesystem_or_snapshot_side_effects() {
    let storage = Storage::open_in_memory().unwrap();
    let root = TestRoot::new();
    let nodes = [proxy("node")];
    for input in [
        request(
            &nodes,
            PublishPolicy {
                enabled: false,
                ..policy()
            },
            true,
        ),
        request(&[], policy(), true),
        request(
            &nodes,
            PublishPolicy {
                guarded: true,
                ..policy()
            },
            false,
        ),
    ] {
        let result = publish_export(&storage, &root.0.join("missing"), input).unwrap();
        assert!(result.publication.is_none());
        assert!(result.content_path.is_none());
    }
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 0);
}

/// Content files have restrictive permissions, stable paths, and no mutable sidecar.
#[test]
fn immutable_materialization_is_idempotent_and_private() {
    let root = TestRoot::new();
    let rendered = render_clash_meta(&[proxy("node")], &[]).unwrap();
    let path = materialize_export(&root.0, "\u{9999}\u{6e2f}", &rendered).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), rendered.content());
    assert_eq!(
        materialize_export(&root.0, "\u{9999}\u{6e2f}", &rendered).unwrap(),
        path
    );
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
    assert!(path.starts_with(&root.0));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[cfg(windows)]
    windows_files::check_private(&path).unwrap();
}

/// Invalid paths, an unusable root, and a corrupted same-hash file never get repaired silently.
#[test]
fn path_containment_and_existing_file_integrity_are_enforced() {
    let root = TestRoot::new();
    let rendered = render_clash_meta(&[proxy("node")], &[]).unwrap();
    assert!(materialize_export(&root.0, "../escape", &rendered).is_err());
    assert!(materialize_export(Path::new("relative"), "air", &rendered).is_err());
    assert!(materialize_export(&root.0.join(".."), "air", &rendered).is_err());
    assert!(materialize_export(&root.0.join("missing"), "air", &rendered).is_err());
    let path = materialize_export(&root.0, "air", &rendered).unwrap();
    fs::write(&path, b"corrupt").unwrap();
    assert!(materialize_export(&root.0, "air", &rendered).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"corrupt");
}

/// The root gate refuses traversal, absolute names, prefix confusion, and shared roots.
#[test]
fn export_root_gate_rejects_traversal_absolute_names_and_shared_directories() {
    let root = TestRoot::new();
    let rendered = render_clash_meta(&[proxy("node")], &[]).unwrap();
    // A root that resolved to the shared parent is refused on every platform: on
    // Unix the literal `..` component is still visible, and on Windows the verbatim
    // join already popped it, so the parent is refused as a shared directory.
    assert!(checked_root(&root.0.join("..")).is_err());
    let checked = checked_root(&root.0).unwrap();
    for name in ["", ".", "..", "../escape", "/absolute.yaml", "a/b", "a\\b"] {
        assert!(checked.join_file(name).is_err(), "accepted name {name:?}");
    }
    // A contained name keeps the root as its exact parent, so a sibling directory
    // sharing a name prefix can never be addressed.
    let inside = checked.join_file("inside.yaml").unwrap();
    assert_eq!(inside.parent(), Some(checked.path()));
    assert!(inside.starts_with(&root.0));
    let key_hash = export_hash("air");
    assert!(is_export_artifact(OsStr::new(&format!(
        "{key_hash}-{}.yaml",
        rendered.sha256()
    ))));
    assert!(is_export_artifact(OsStr::new(".publication-1-2.tmp")));
    assert!(!is_export_artifact(OsStr::new("foreign.txt")));
    assert!(!is_export_artifact(OsStr::new(&format!("{key_hash}.yaml"))));
    // Entries this service did not create mean the root is not exclusively ours.
    fs::write(root.0.join("foreign.txt"), b"x").unwrap();
    assert!(materialize_export(&root.0, "air", &rendered).is_err());
    fs::remove_file(root.0.join("foreign.txt")).unwrap();
    assert!(materialize_export(&root.0, "air", &rendered).is_ok());
}

/// A failed install cannot advance the database or overwrite the last good file.
#[test]
fn failed_file_install_keeps_previous_export_and_never_calls_activation() {
    let root = TestRoot::new();
    let previous = render_clash_meta(&[proxy("old")], &[]).unwrap();
    let old_path = materialize_export(&root.0, "air", &previous).unwrap();
    let next = render_clash_meta(&[proxy("new")], &[]).unwrap();
    let blocked = root
        .0
        .join(format!("{}-{}.yaml", export_hash("air"), next.sha256()));
    fs::create_dir(&blocked).unwrap();
    let activated = Cell::new(false);
    let result = materialize_then(&root.0, "air", &next, || {
        activated.set(true);
        Ok(())
    });
    assert!(result.is_err());
    assert!(!activated.get());
    assert_eq!(fs::read_to_string(old_path).unwrap(), previous.content());
}

/// A transaction error after materialization leaves only a harmless immutable orphan.
#[test]
fn failed_activation_keeps_previous_file_and_authoritative_snapshot() {
    let storage = Storage::open_in_memory().unwrap();
    let root = TestRoot::new();
    let nodes = [proxy("old")];
    let first = publish_export(&storage, &root.0, request(&nodes, policy(), false)).unwrap();
    let previous_id = first.publication.unwrap().snapshot.id;
    let previous_path = first.content_path.unwrap();
    let previous_bytes = fs::read(&previous_path).unwrap();
    let next = render_clash_meta(&[proxy("new")], &[]).unwrap();
    let result: PublishResult<((), PathBuf)> = materialize_then(&root.0, "air", &next, || {
        Err(probe_domain::DomainError::Storage(
            "injected transaction failure".into(),
        ))
    });
    assert!(result.is_err());
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        previous_id
    );
    assert_eq!(fs::read(previous_path).unwrap(), previous_bytes);
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 2);
}

/// End-to-end publication, diff guard, forced override, reuse, and explicit rollback.
#[test]
fn publication_and_rollback_preserve_immutable_versions() {
    let storage = Storage::open_in_memory().unwrap();
    let root = TestRoot::new();
    let old = [proxy("old")];
    let first = publish_export(&storage, &root.0, request(&old, policy(), false)).unwrap();
    let first_id = first.publication.unwrap().snapshot.id;
    let empty = publish_export(&storage, &root.0, request(&[], policy(), true)).unwrap();
    assert_eq!(empty.decision, PublishDecision::Empty);
    let guarded_round = PublishPolicy {
        guarded: true,
        ..policy()
    };
    let guarded_result =
        publish_export(&storage, &root.0, request(&old, guarded_round, false)).unwrap();
    assert_eq!(guarded_result.decision, PublishDecision::Guarded);
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        first_id
    );
    let new = [proxy("new")];
    let guarded = PublishPolicy {
        max_changed_identities: Some(0),
        ..policy()
    };
    let skipped = publish_export(&storage, &root.0, request(&new, guarded, false)).unwrap();
    assert_eq!(skipped.decision, PublishDecision::DiffLimit);
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
    let second = publish_export(&storage, &root.0, request(&new, guarded, true)).unwrap();
    assert_eq!(second.decision, PublishDecision::Publish { forced: true });
    let second_id = second.publication.unwrap().snapshot.id;
    let repeated = publish_export(&storage, &root.0, request(&new, policy(), false)).unwrap();
    assert_eq!(repeated.publication.unwrap().snapshot.id, second_id);
    assert_eq!(storage.export_history("air", 10, None).unwrap().len(), 2);
    let (summary, path) = rollback_export(
        &storage,
        &root.0,
        "air",
        first_id,
        "operator",
        Some(second_id),
    )
    .unwrap();
    assert_eq!(summary.id, first_id);
    assert_eq!(Some(path), first.content_path);
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 2);
    assert!(rollback_export(&storage, &root.0, "other", first_id, "operator", None).is_err());
}

/// Retention keeps the newest snapshots (plus the active one), deletes the
/// files no surviving row references, and never touches a file it did not name.
#[test]
fn retention_prunes_superseded_snapshots_and_only_their_files() {
    let storage = Storage::open_in_memory().unwrap();
    let root = TestRoot::new();
    // Three distinct bodies -> three immutable files, each activated in turn.
    for name in ["one", "two", "three"] {
        let body = [proxy(name)];
        let result = publish_export(&storage, &root.0, request(&body, policy(), true)).unwrap();
        assert!(matches!(result.decision, PublishDecision::Publish { .. }));
    }
    assert_eq!(storage.export_history("air", 10, None).unwrap().len(), 3);
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 3);
    // A file the service never named must survive the sweep.
    fs::write(root.0.join("README.txt"), "not an export").unwrap();

    let report = prune_publications(&storage, &root.0, 1);
    assert_eq!(report.keys, 1);
    assert_eq!(
        report.snapshots_deleted, 2,
        "the active snapshot always stays"
    );
    assert_eq!(report.files_deleted, 2);
    assert_eq!(storage.export_history("air", 10, None).unwrap().len(), 1);
    assert_eq!(
        fs::read_dir(&root.0).unwrap().count(),
        2,
        "one export file plus the foreign file"
    );
    let current = storage.current_export("air").unwrap().unwrap();
    assert!(current.summary.is_active);
    assert!(
        current.summary.sha256.len() == 64,
        "the surviving snapshot is the one the pointer names"
    );

    // A second pass has nothing left to do.
    let again = prune_publications(&storage, &root.0, 1);
    assert_eq!((again.snapshots_deleted, again.files_deleted), (0, 0));
}

/// Root symlinks, symlink ancestors/targets, shared roots, and external hardlinks fail.
#[cfg(unix)]
#[test]
fn symlinks_junction_equivalents_and_shared_files_are_rejected() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = TestRoot::new();
    let outside = TestRoot::new();
    let rendered = render_clash_meta(&[proxy("node")], &[]).unwrap();
    let linked_root = root.0.join("linked");
    symlink(&outside.0, &linked_root).unwrap();
    assert!(materialize_export(&linked_root, "air", &rendered).is_err());
    let child = outside.0.join("child");
    fs::create_dir(&child).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(materialize_export(&linked_root.join("child"), "air", &rendered).is_err());
    // The checked root rejects every entry it did not create itself, so the
    // probe symlink has to leave before the clean-materialize assertions --
    // on Linux its presence made the next call fail with
    // "export root is shared with entries this service did not create"
    // (this test is #[cfg(unix)] and had never executed before CI run 3).
    fs::remove_file(&linked_root).unwrap();
    let target = root
        .0
        .join(format!("{}-{}.yaml", export_hash("air"), rendered.sha256()));
    symlink(outside.0.join("absent"), &target).unwrap();
    assert!(materialize_export(&root.0, "air", &rendered).is_err());
    assert!(!outside.0.join("absent").exists());
    fs::remove_file(&target).unwrap();
    let path = materialize_export(&root.0, "air", &rendered).unwrap();
    fs::hard_link(&path, outside.0.join("shared")).unwrap();
    assert!(materialize_export(&root.0, "air", &rendered).is_err());
    fs::remove_file(outside.0.join("shared")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(materialize_export(&root.0, "air", &rendered).is_err());
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(materialize_export(&root.0, "other", &rendered).is_err());
}

/// A normalized source entry; only the publish switches vary between tests.
fn source(key: &str, enabled: bool, export: bool) -> SourceSpec {
    SourceSpec {
        key: key.to_string(),
        kind: "collection".to_string(),
        name: key.to_string(),
        label: key.to_string(),
        enabled,
        relay: false,
        direct: true,
        chain: false,
        export,
    }
}

/// A proxy dictionary in the original domain form, with a publication marker.
fn exportable(name: &str, kind: &str) -> Value {
    json!({"name": name, "type": kind, "server": "entry.example.invalid", "port": 443,
        "uuid": "00000000-0000-4000-8000-000000000001", "origin": "domain"})
}

/// A job for one ledger identity in the configured (direct) role.
fn job(source: &str, fingerprint: &str, display: &str) -> Job {
    Job::new(
        source,
        fingerprint,
        display,
        "direct",
        "entry.example.invalid",
    )
    .with_origin(
        display.to_string(),
        Some("entry.example.invalid".to_string()),
        Some("vless".to_string()),
    )
}

/// A job for a chained variant of one identity.
fn chained_job(source: &str, fingerprint: &str, display: &str) -> Job {
    Job::new(
        source,
        fingerprint,
        display,
        "chain",
        "front.example.invalid",
    )
    .with_role(Role::Chain, Some("__FRONT0__".to_string()))
    .with_origin(
        display.to_string(),
        Some("entry.example.invalid".to_string()),
        Some("vless".to_string()),
    )
}

/// An alive verdict, optionally carrying the measured exit country.
fn alive(job: Job, country: Option<&str>) -> NodeVerdict {
    let mut verdict = NodeVerdict::alive(job, 120);
    verdict.exit = country.map(|country| probe_domain::ExitIdentity {
        country: Some(country.to_string()),
        ..probe_domain::ExitIdentity::default()
    });
    verdict
}

/// Collected originals for one source, keyed by ledger fingerprint.
fn originals(source: &str, entries: &[(&str, Value)]) -> Collected {
    let mut collected = Collected::default();
    for (fingerprint, proxy) in entries {
        collected.originals.insert(
            (source.to_string(), (*fingerprint).to_string()),
            proxy.clone(),
        );
    }
    collected
}

/// The published tuple of a one-source round outcome.
fn published_of(outcome: &PublishRoundOutcome) -> (&str, usize, i64, PathBuf) {
    match outcome.sources.first() {
        Some(SourcePublication::Published {
            key,
            node_count,
            snapshot_id,
            content_path,
        }) => (
            key.as_str(),
            *node_count,
            *snapshot_id,
            content_path.clone(),
        ),
        other => panic!("expected a publication, got {other:?}"),
    }
}

/// Only alive verdicts publish, and only as their original domain-form dictionary.
#[test]
fn select_source_exports_publishes_only_alive_original_dictionaries() {
    let sources = [source("air", true, true)];
    let collected = originals(
        "air",
        &[
            ("fp-alive", exportable("live", "vless")),
            ("fp-dead", exportable("gone", "vless")),
        ],
    );
    let exported = select_source_exports(
        &PublishSection::default(),
        &sources,
        &collected,
        &[
            NodeVerdict::dead(job("air", "fp-dead", "gone"), "unreachable", ""),
            alive(job("air", "fp-alive", "live"), Some("HK")),
        ],
    );
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0].key, "air");
    assert!(exported[0].skipped.is_none());
    assert!(exported[0].dropped.is_empty());
    assert_eq!(exported[0].proxies.len(), 1);
    // The original dictionary, not a kernel alias, and tagged from the measurement.
    assert_eq!(exported[0].proxies[0]["origin"], "domain");
    assert_eq!(exported[0].proxies[0]["name"], "[HK] live");
}

/// The measured exit country replaces any tag already in the name, idempotently.
#[test]
fn select_source_exports_rewrites_region_tags_idempotently() {
    let sources = [source("air", true, true)];
    let cases = [
        ("[HK] Foo", "[US] Foo"),
        ("\u{1F1ED}\u{1F1F0} [HK] Foo", "[US] Foo"),
        ("[HK][HK] Foo", "[US] Foo"),
        ("[US-CA] Foo", "[US] Foo"),
        ("Foo", "[US] Foo"),
    ];
    for (stored, expected) in cases {
        let collected = originals("air", &[("fp", exportable(stored, "ss"))]);
        let exported = select_source_exports(
            &PublishSection::default(),
            &sources,
            &collected,
            &[alive(job("air", "fp", stored), Some("us"))],
        );
        assert_eq!(
            exported[0].proxies[0]["name"], expected,
            "stored {stored:?}"
        );
        // Re-publishing the rewritten name is a no-op: the tag never stacks.
        let collected = originals("air", &[("fp", exportable(expected, "ss"))]);
        let rerun = select_source_exports(
            &PublishSection::default(),
            &sources,
            &collected,
            &[alive(job("air", "fp", expected), Some("us"))],
        );
        assert_eq!(rerun[0].proxies[0]["name"], expected, "rerun {stored:?}");
    }
    // Tagging off leaves the operator's name exactly as stored.
    let collected = originals("air", &[("fp", exportable("[HK] Foo", "ss"))]);
    let untagged = PublishSection {
        add_region_tag: false,
        ..PublishSection::default()
    };
    let exported = select_source_exports(
        &untagged,
        &sources,
        &collected,
        &[alive(job("air", "fp", "[HK] Foo"), Some("US"))],
    );
    assert_eq!(exported[0].proxies[0]["name"], "[HK] Foo");
}

/// The region re-tag can map two originals onto one published name
/// (`[SG] 01` measured through an HK exit and `[HK] 01` both become
/// `[HK] 01`). The loser must be superseded, not emitted twice -- a duplicate
/// name fails the whole source's topology validation and the export would
/// go stale every round. The sort-stable winner is the node whose own tag
/// was already right.
#[test]
fn a_region_tag_collision_supersedes_the_loser_instead_of_failing_the_source() {
    let sources = [source("air", true, true)];
    let collected = originals(
        "air",
        &[
            ("fp-hk", exportable("[HK] 01", "ss")),
            ("fp-sg", exportable("[SG] 01", "ss")),
        ],
    );
    let exported = select_source_exports(
        &PublishSection::default(),
        &sources,
        &collected,
        &[
            alive(job("air", "fp-hk", "[HK] 01"), Some("hk")),
            alive(job("air", "fp-sg", "[SG] 01"), Some("hk")),
        ],
    );
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0].proxies.len(), 1, "exactly one [HK] 01");
    assert_eq!(exported[0].proxies[0]["name"], "[HK] 01");
    let losers: Vec<&str> = exported[0]
        .dropped
        .iter()
        .map(|drop| drop.identity.as_str())
        .collect();
    assert_eq!(losers, ["fp-sg"], "the re-tagged duplicate is dropped");
    assert!(matches!(
        exported[0].dropped[0].reason,
        ExportDropReason::SupersededVariant
    ));
}

/// Disabled and muted sources are skipped, never published with a stale body.
#[test]
fn select_source_exports_skips_disabled_and_muted_sources() {
    let sources = [
        source("air", true, true),
        source("off", false, true),
        source("muted", true, false),
    ];
    let mut collected = Collected::default();
    let mut verdicts = Vec::new();
    for key in ["air", "off", "muted"] {
        collected
            .originals
            .insert((key.to_string(), "fp".to_string()), exportable(key, "ss"));
        verdicts.push(alive(job(key, "fp", key), None));
    }
    let exported =
        select_source_exports(&PublishSection::default(), &sources, &collected, &verdicts);
    let keys: Vec<&str> = exported.iter().map(|entry| entry.key.as_str()).collect();
    assert_eq!(keys, ["air", "off", "muted"]);
    assert!(exported[0].skipped.is_none());
    assert_eq!(exported[0].proxies.len(), 1);
    assert_eq!(exported[1].skipped, Some(SkipReason::SourceDisabled));
    assert!(exported[1].proxies.is_empty());
    assert_eq!(exported[2].skipped, Some(SkipReason::SourceMuted));
    assert!(exported[2].proxies.is_empty());
}

/// A dialer that cannot resolve inside the file is dropped, never published broken.
#[test]
fn select_source_exports_drops_unclosed_dialers_transitively() {
    let sources = [source("air", true, true)];
    let collected = originals(
        "air",
        &[
            (
                "fp-chained",
                json!({"name": "chained", "type": "vless", "server": "a.example.invalid",
                    "port": 443, "dialer-proxy": "__FRONT0__"}),
            ),
            (
                "fp-spoke",
                json!({"name": "spoke", "type": "vless", "server": "b.example.invalid",
                    "port": 443, "dialer-proxy": "__FRONT0__"}),
            ),
            (
                "fp-hub",
                json!({"name": "hub", "type": "vless", "server": "c.example.invalid",
                    "port": 443, "dialer-proxy": "spoke"}),
            ),
            (
                "fp-sibling",
                json!({"name": "sibling", "type": "vless", "server": "d.example.invalid",
                    "port": 443, "dialer-proxy": "landing"}),
            ),
            ("fp-landing", exportable("landing", "vless")),
        ],
    );
    let verdicts = [
        alive(chained_job("air", "fp-chained", "chained"), None),
        alive(job("air", "fp-spoke", "spoke"), None),
        alive(job("air", "fp-hub", "hub"), None),
        alive(job("air", "fp-sibling", "sibling"), None),
        alive(job("air", "fp-landing", "landing"), Some("HK")),
    ];
    let exported =
        select_source_exports(&PublishSection::default(), &sources, &collected, &verdicts);
    let names: Vec<&str> = exported[0]
        .proxies
        .iter()
        .filter_map(|proxy| proxy["name"].as_str())
        .collect();
    assert_eq!(names, ["[HK] landing", "sibling"]);
    // The sibling dials a published node by its pre-tag name, so the reference is
    // rewritten to the published name and the file really closes.
    assert_eq!(exported[0].proxies[1]["dialer-proxy"], "[HK] landing");
    let dropped: Vec<(&str, ExportDropReason)> = exported[0]
        .dropped
        .iter()
        .map(|drop| (drop.identity.as_str(), drop.reason))
        .collect();
    assert_eq!(dropped.len(), 3);
    for identity in ["fp-chained", "fp-spoke", "fp-hub"] {
        assert!(
            dropped.contains(&(identity, ExportDropReason::UnclosedDialer)),
            "missing {identity}"
        );
    }
}

/// Two variants of one node cannot both be published: the chain verdict wins.
#[test]
fn select_source_exports_collapses_variants_of_one_display_name() {
    let sources = [source("air", true, true)];
    let collected = originals(
        "air",
        &[
            ("fp-direct", exportable("node", "vless")),
            (
                "fp-chain",
                json!({"name": "node", "type": "vless", "server": "a.example.invalid",
                    "port": 443, "dialer-proxy": "front"}),
            ),
            ("fp-front", exportable("front", "vless")),
        ],
    );
    let verdicts = [
        alive(job("air", "fp-direct", "node"), None),
        alive(chained_job("air", "fp-chain", "node"), None),
        alive(job("air", "fp-front", "front"), None),
    ];
    let exported =
        select_source_exports(&PublishSection::default(), &sources, &collected, &verdicts);
    let names: Vec<&str> = exported[0]
        .proxies
        .iter()
        .filter_map(|proxy| proxy["name"].as_str())
        .collect();
    assert_eq!(names, ["front", "node"]);
    assert_eq!(exported[0].proxies[1]["dialer-proxy"], "front");
    assert!(exported[0]
        .dropped
        .iter()
        .any(|drop| drop.identity == "fp-direct"
            && drop.reason == ExportDropReason::SupersededVariant));
}

/// Bodies are ordered by (type, name) whatever order the verdicts arrive in.
#[test]
fn select_source_exports_order_is_stable_across_verdict_order() {
    let sources = [source("air", true, true)];
    let collected = originals(
        "air",
        &[
            ("fp-b", exportable("b", "ss")),
            ("fp-a", exportable("a", "ss")),
            ("fp-c", exportable("c", "vless")),
        ],
    );
    let forward = vec![
        alive(job("air", "fp-b", "b"), None),
        alive(job("air", "fp-a", "a"), None),
        alive(job("air", "fp-c", "c"), None),
    ];
    let reversed: Vec<NodeVerdict> = forward.iter().rev().cloned().collect();
    let first = select_source_exports(&PublishSection::default(), &sources, &collected, &forward);
    let second = select_source_exports(&PublishSection::default(), &sources, &collected, &reversed);
    assert_eq!(first[0].proxies, second[0].proxies);
    let names: Vec<&str> = first[0]
        .proxies
        .iter()
        .filter_map(|proxy| proxy["name"].as_str())
        .collect();
    assert_eq!(names, ["a", "b", "c"]);
}

/// A full round: first publication, snapshot reuse, and the guard's veto.
#[test]
fn publish_round_publishes_reuses_and_preserves_under_the_guard() {
    let storage = Storage::open_in_memory().unwrap();
    let root = TestRoot::new();
    let sources = [source("air", true, true), source("muted", true, false)];
    let collected = originals("air", &[("fp", exportable("live", "vless"))]);
    let verdicts = vec![alive(job("air", "fp", "live"), Some("HK"))];
    let publish = PublishSection::default();
    let round = |publish: &PublishSection, guard: GuardDecision| {
        publish_round(
            &storage,
            &root.0,
            PublishRoundRequest {
                publish,
                sources: &sources,
                collected: &collected,
                verdicts: &verdicts,
                guard,
                measured: true,
                actor: "test-operator",
            },
        )
    };

    // First round: no baseline snapshot exists yet.
    assert!(storage.current_export("air").unwrap().is_none());
    let first = round(&publish, GuardDecision::ApplyConvergence);
    assert!(first.skipped.is_none());
    let (key, node_count, snapshot_id, path) = published_of(&first);
    assert_eq!((key, node_count), ("air", 1));
    assert!(path.exists());
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        snapshot_id
    );
    let skipped: Vec<(&str, SkipReason)> = first
        .sources
        .iter()
        .filter_map(|source| match source {
            SourcePublication::Skipped { key, reason } => Some((key.as_str(), *reason)),
            _ => None,
        })
        .collect();
    assert_eq!(skipped, [("muted", SkipReason::SourceMuted)]);

    // Identical content reuses the same snapshot and the same immutable file.
    let again = round(&publish, GuardDecision::ApplyConvergence);
    let (_, _, reused_id, reused_path) = published_of(&again);
    assert_eq!(reused_id, snapshot_id);
    assert_eq!(reused_path, path);
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);

    // A guard that preserves the previous state publishes nothing at all.
    let guarded = round(&publish, GuardDecision::PreservePreviousState);
    assert_eq!(guarded.skipped, Some(SkipReason::GuardNotApplied));
    assert_eq!(guarded.sources.len(), 2);
    assert!(guarded
        .sources
        .iter()
        .all(|source| matches!(source, SourcePublication::Skipped { .. })));
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        snapshot_id
    );
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);

    // A disabled publish section writes nothing, even with a clean guard.
    let disabled = PublishSection {
        enabled: false,
        ..PublishSection::default()
    };
    let off = round(&disabled, GuardDecision::ApplyConvergence);
    assert_eq!(off.skipped, Some(SkipReason::PublishDisabled));
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        snapshot_id
    );
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
}

/// One unusable key fails alone; the other sources still publish.
#[test]
fn publish_round_reports_a_failed_source_without_stopping_the_others() {
    let storage = Storage::open_in_memory().unwrap();
    let root = TestRoot::new();
    let sources = [source("air", true, true), source("bad/key", true, true)];
    let mut collected = originals("air", &[("fp", exportable("live", "vless"))]);
    // The unusable key has a real body, so the failure comes from publishing it.
    collected.originals.insert(
        ("bad/key".to_string(), "fp".to_string()),
        exportable("live", "vless"),
    );
    let verdicts = vec![
        alive(job("air", "fp", "live"), None),
        alive(job("bad/key", "fp", "live"), None),
    ];
    let publish = PublishSection::default();
    let outcome = publish_round(
        &storage,
        &root.0,
        PublishRoundRequest {
            publish: &publish,
            sources: &sources,
            collected: &collected,
            verdicts: &verdicts,
            guard: GuardDecision::ApplyConvergence,
            measured: true,
            actor: "test-operator",
        },
    );
    assert!(matches!(
        outcome.sources[0],
        SourcePublication::Published { .. }
    ));
    match &outcome.sources[1] {
        SourcePublication::Failed { key, reason } => {
            assert_eq!(key, "bad/key");
            assert_eq!(reason, "invalid export key");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(storage.current_export("air").unwrap().is_some());
}

/// Only a disabled section, a breached alive floor, or an inconclusive round
/// that measured nothing withholds publication; partial coverage publishes.
#[test]
fn publish_round_publishes_partial_coverage_and_skips_only_the_guard_cases() {
    let storage = Storage::open_in_memory().unwrap();
    let root = TestRoot::new();
    let sources = [source("air", true, true)];
    let collected = originals(
        "air",
        &[
            ("fp-live", exportable("live", "vless")),
            ("fp-unverified", exportable("pending", "vless")),
        ],
    );
    // One node is alive, one never finished exit verification (the live round
    // 19/20 shape: partial coverage, not a broken round).
    let mut unverified = NodeVerdict::alive(job("air", "fp-unverified", "pending"), 90);
    unverified.verification_error = Some("exit verification incomplete".to_string());
    let verdicts = vec![alive(job("air", "fp-live", "live"), Some("HK")), unverified];
    let publish = PublishSection::default();
    let round = |guard: GuardDecision, measured: bool, publish: &PublishSection| {
        publish_round(
            &storage,
            &root.0,
            PublishRoundRequest {
                publish,
                sources: &sources,
                collected: &collected,
                verdicts: &verdicts,
                guard,
                measured,
                actor: "test-operator",
            },
        )
    };

    // Partial coverage publishes: the unverified node is not alive, so it never
    // enters the body -- only the measured node does.
    let partial = round(GuardDecision::MarkRoundInconclusive, true, &publish);
    assert!(partial.skipped.is_none());
    let (key, node_count, snapshot_id, path) = published_of(&partial);
    assert_eq!((key, node_count), ("air", 1));
    assert!(path.exists());
    let document: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let names: Vec<&str> = document["proxies"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|proxy| proxy["name"].as_str())
        .collect();
    assert_eq!(names, ["[HK] live"], "an unverified node must not publish");
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        snapshot_id
    );

    // An inconclusive round that measured nothing leaves file and pointer alone.
    let unmeasured = round(GuardDecision::MarkRoundInconclusive, false, &publish);
    assert_eq!(unmeasured.skipped, Some(SkipReason::GuardNotApplied));
    assert_eq!(
        unmeasured.sources,
        vec![SourcePublication::Skipped {
            key: "air".to_string(),
            reason: SkipReason::GuardNotApplied,
        }]
    );
    assert_eq!(
        storage.current_export("air").unwrap().unwrap().summary.id,
        snapshot_id
    );
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);

    // A breached alive floor skips the round whatever measured says.
    for measured in [true, false] {
        let floor = round(GuardDecision::PreservePreviousState, measured, &publish);
        assert_eq!(floor.skipped, Some(SkipReason::GuardNotApplied));
        assert_eq!(
            storage.current_export("air").unwrap().unwrap().summary.id,
            snapshot_id
        );
    }

    // A disabled section still writes nothing on an otherwise clean round.
    let disabled = PublishSection {
        enabled: false,
        ..PublishSection::default()
    };
    let off = round(GuardDecision::ApplyConvergence, true, &disabled);
    assert_eq!(off.skipped, Some(SkipReason::PublishDisabled));
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
}

/// A Sub-Store admin double: records every call, lists what the test seeded.
#[derive(Default)]
struct FakeAdmin {
    subs: Mutex<Vec<SubEntry>>,
    upserts: Mutex<Vec<(String, Value)>>,
    deletes: Mutex<Vec<String>>,
    lists: Mutex<usize>,
    fail_upserts: Mutex<bool>,
    fail_list: Mutex<bool>,
}

impl FakeAdmin {
    fn with_subs(subs: Vec<SubEntry>) -> Self {
        Self {
            subs: Mutex::new(subs),
            ..Self::default()
        }
    }

    fn upsert_names(&self) -> Vec<String> {
        self.upserts
            .lock()
            .unwrap()
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn upsert_payload(&self, name: &str) -> Value {
        self.upserts
            .lock()
            .unwrap()
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, payload)| payload.clone())
            .unwrap_or_else(|| panic!("no upsert was sent for {name}"))
    }

    fn deletes(&self) -> Vec<String> {
        self.deletes.lock().unwrap().clone()
    }
}

type AdminFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl SubAdmin for FakeAdmin {
    fn upsert_sub<'a>(
        &'a self,
        name: &'a str,
        payload: &'a Value,
    ) -> AdminFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.upserts
                .lock()
                .unwrap()
                .push((name.to_string(), payload.clone()));
            if *self.fail_upserts.lock().unwrap() {
                return Err("backend down".to_string());
            }
            Ok("created".to_string())
        })
    }

    fn delete_sub<'a>(&'a self, name: &'a str) -> AdminFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            self.deletes.lock().unwrap().push(name.to_string());
            Ok(true)
        })
    }

    fn list_subs<'a>(&'a self) -> AdminFuture<'a, Result<Vec<SubEntry>, String>> {
        Box::pin(async move {
            *self.lists.lock().unwrap() += 1;
            if *self.fail_list.lock().unwrap() {
                return Err("backend down".to_string());
            }
            Ok(self.subs.lock().unwrap().clone())
        })
    }
}

/// A round that published every key it is given, ready to push.
fn published_round(keys: &[&str]) -> (Storage, TestRoot, Vec<SourceSpec>, PublishRoundOutcome) {
    let storage = Storage::open_in_memory().unwrap();
    let root = TestRoot::new();
    let sources: Vec<SourceSpec> = keys.iter().map(|key| source(key, true, true)).collect();
    let mut collected = Collected::default();
    let mut verdicts = Vec::new();
    for key in keys {
        collected.originals.insert(
            (key.to_string(), "fp".to_string()),
            exportable("live", "vless"),
        );
        verdicts.push(alive(job(key, "fp", "live"), Some("HK")));
    }
    let publish = PublishSection::default();
    let outcome = publish_round(
        &storage,
        &root.0,
        PublishRoundRequest {
            publish: &publish,
            sources: &sources,
            collected: &collected,
            verdicts: &verdicts,
            guard: GuardDecision::ApplyConvergence,
            measured: true,
            actor: "test-operator",
        },
    );
    (storage, root, sources, outcome)
}

/// With the push mode off nothing is sent -- not even the listing.
#[tokio::test]
async fn push_mode_off_sends_nothing_to_substore() {
    let (storage, _root, sources, mut outcome) = published_round(&["air"]);
    let admin = FakeAdmin::default();
    let publish = PublishSection {
        push_to_substore: false,
        ..PublishSection::default()
    };
    push_round_exports(&admin, &storage, &publish, &sources, &mut outcome).await;

    assert!(outcome.push.is_empty());
    assert!(admin.upserts.lock().unwrap().is_empty());
    assert!(admin.deletes().is_empty());
    assert_eq!(
        *admin.lists.lock().unwrap(),
        0,
        "a disabled push makes no request at all"
    );
}

/// Every published source gets one local sub carrying its export body.
#[tokio::test]
async fn push_mode_upserts_one_local_sub_per_published_source() {
    let (storage, _root, sources, mut outcome) = published_round(&["air"]);
    let admin = FakeAdmin::default();
    let publish = PublishSection {
        push_to_substore: true,
        ..PublishSection::default()
    };
    push_round_exports(&admin, &storage, &publish, &sources, &mut outcome).await;

    assert_eq!(admin.upsert_names(), ["probe-air-local"]);
    let payload = admin.upsert_payload("probe-air-local");
    assert_eq!(payload["name"], "probe-air-local");
    assert_eq!(payload["displayName"], "air 测活 (1)");
    assert_eq!(payload["source"], "local");
    assert_eq!(payload["url"], "");
    assert_eq!(payload["mergeSources"], "");
    assert_eq!(payload["ignoreFailedRemoteSub"], "quiet");
    assert_eq!(payload["passThroughUA"], false);
    assert_eq!(payload["process"], json!([]));
    // The body is the active snapshot: what a download would serve, not a guess.
    let active = storage.current_export("air").unwrap().unwrap();
    assert_eq!(payload["content"], active.content);

    assert_eq!(outcome.push.len(), 1);
    assert_eq!(outcome.push[0].key, "air");
    assert_eq!(outcome.push[0].name, "probe-air-local");
    assert!(outcome.push[0].ok);
    assert_eq!(outcome.push[0].node_count, 1);
    assert_eq!(outcome.push[0].detail, "created");
}

/// One refused write is recorded; the next source is still pushed, and the
/// publication itself is never downgraded by a push failure.
#[tokio::test]
async fn a_failed_upsert_is_recorded_and_never_stops_the_other_sources() {
    let (storage, _root, sources, mut outcome) = published_round(&["air", "backup"]);
    let admin = FakeAdmin::default();
    *admin.fail_upserts.lock().unwrap() = true;
    let publish = PublishSection {
        push_to_substore: true,
        ..PublishSection::default()
    };
    push_round_exports(&admin, &storage, &publish, &sources, &mut outcome).await;

    assert_eq!(
        admin.upsert_names(),
        ["probe-air-local", "probe-backup-local"],
        "one failure must not stop the next source"
    );
    assert_eq!(outcome.push.len(), 2);
    assert!(outcome.push.iter().all(|record| !record.ok));
    assert!(outcome
        .push
        .iter()
        .all(|record| record.detail == "backend down"));
    assert!(
        outcome
            .sources
            .iter()
            .all(|source| matches!(source, SourcePublication::Published { .. })),
        "a push failure never downgrades a publication"
    );
}

/// The prune removes retired local subs and touches nothing else.
#[tokio::test]
async fn prune_removes_only_our_retired_local_subs() {
    let admin = FakeAdmin::with_subs(vec![
        SubEntry {
            name: "probe-air-local".to_string(),
            source: Some("local".to_string()),
        },
        SubEntry {
            name: "probe-retired-local".to_string(),
            source: Some("local".to_string()),
        },
        SubEntry {
            name: "probe-muted-local".to_string(),
            source: Some("local".to_string()),
        },
        SubEntry {
            name: "other-air-local".to_string(),
            source: Some("local".to_string()),
        },
        SubEntry {
            name: "probe-remote-local".to_string(),
            source: Some("remote".to_string()),
        },
        SubEntry {
            name: "probe-air".to_string(),
            source: Some("local".to_string()),
        },
    ]);
    let storage = Storage::open_in_memory().unwrap();
    let root = TestRoot::new();
    // "air" publishes; "muted" is enabled and marked for export but has nothing
    // alive, so it publishes nothing -- and its existing sub must survive.
    let sources = [source("air", true, true), source("muted", true, true)];
    let mut collected = Collected::default();
    collected.originals.insert(
        ("air".to_string(), "fp".to_string()),
        exportable("live", "vless"),
    );
    let verdicts = vec![alive(job("air", "fp", "live"), Some("HK"))];
    let publish = PublishSection {
        push_to_substore: true,
        ..PublishSection::default()
    };
    let mut outcome = publish_round(
        &storage,
        &root.0,
        PublishRoundRequest {
            publish: &publish,
            sources: &sources,
            collected: &collected,
            verdicts: &verdicts,
            guard: GuardDecision::ApplyConvergence,
            measured: true,
            actor: "test-operator",
        },
    );
    assert!(matches!(
        outcome.sources[1],
        SourcePublication::Skipped {
            reason: SkipReason::NoAliveNodes,
            ..
        }
    ));

    push_round_exports(&admin, &storage, &publish, &sources, &mut outcome).await;

    assert_eq!(admin.upsert_names(), ["probe-air-local"]);
    assert_eq!(admin.deletes(), ["probe-retired-local"]);
    let removed: Vec<&PushRecord> = outcome
        .push
        .iter()
        .filter(|record| record.detail == "removed")
        .collect();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].key, "retired");
    assert!(removed[0].ok);
}

/// A listing that failed deletes nothing: an unreadable list is not a licence
/// to remove subs.
#[tokio::test]
async fn a_failed_listing_never_deletes_anything() {
    let (storage, _root, sources, mut outcome) = published_round(&["air"]);
    let admin = FakeAdmin::with_subs(vec![SubEntry {
        name: "probe-retired-local".to_string(),
        source: Some("local".to_string()),
    }]);
    *admin.fail_list.lock().unwrap() = true;
    let publish = PublishSection {
        push_to_substore: true,
        ..PublishSection::default()
    };
    push_round_exports(&admin, &storage, &publish, &sources, &mut outcome).await;

    assert_eq!(
        admin.upsert_names(),
        ["probe-air-local"],
        "the upserts still happen"
    );
    assert!(admin.deletes().is_empty(), "no list, no deletes");
    assert_eq!(outcome.push.len(), 1, "only the upsert is recorded");
}
