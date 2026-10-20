//! Process-level tests for the `db` subcommands (G19's verifiable half).
//!
//! These run the real binary: the whole point of `db check|migrate|backup|
//! rollback|verify` is that an operator can run them from a shell on a stopped
//! service, so the tests exercise argv, exit codes and the on-disk ledger the
//! same way an operator would.

use std::path::Path;
use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_probe-cli")
}

fn run(root: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(binary())
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("spawn probe-cli");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn db_path(root: &Path) -> std::path::PathBuf {
    root.join("data").join("state.db")
}

/// Drop the writer's sidecar files: the rollback refuses to run under a live
/// WAL, and an operator stops the service first. Removing them here is the
/// test's stand-in for "the service is down".
fn stop_writers(root: &Path) {
    for suffix in ["-wal", "-shm"] {
        let path = root.join("data").join(format!("state.db{suffix}"));
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn a_fresh_root_migrates_checks_backs_up_and_rolls_back() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let (code, out, err) = run(root, &["db", "migrate"]);
    assert_eq!(code, 0, "migrate failed: {out}{err}");
    assert!(
        out.contains("creating it"),
        "a missing ledger is created, not refused: {out}"
    );
    assert!(db_path(root).exists(), "migrate must create the file");

    let (code, out, err) = run(root, &["db", "check"]);
    assert_eq!(code, 0, "check failed: {out}{err}");
    assert!(
        out.contains("0 pending"),
        "a fresh ledger has every migration applied: {out}"
    );
    assert!(
        out.contains("integrity_check: ok"),
        "check must report integrity: {out}"
    );

    let backup = root.join("before-writes.db");
    let (code, out, err) = run(root, &["db", "backup", "--out", backup.to_str().unwrap()]);
    assert_eq!(code, 0, "backup failed: {out}{err}");
    assert!(backup.exists(), "backup file must exist: {out}");

    // Something the backup must be able to undo.
    {
        let storage = probe_storage::Storage::open(&db_path(root)).expect("open ledger");
        let round = storage.start_round("test", None).expect("open a round");
        storage
            .finish_round(round, Some("written after the backup"))
            .expect("close the round");
    }
    stop_writers(root);

    // Without --yes the command refuses and changes nothing.
    let (code, out, err) = run(
        root,
        &["db", "rollback", "--from", backup.to_str().unwrap()],
    );
    assert_eq!(code, 2, "an unconfirmed rollback exits 2: {out}{err}");
    assert!(
        err.contains("--yes"),
        "the refusal says how to confirm: {err}"
    );
    {
        let storage = probe_storage::Storage::open(&db_path(root)).unwrap();
        assert!(
            storage.last_round().unwrap().is_some(),
            "a refused rollback must not touch the ledger"
        );
    }
    stop_writers(root);

    let (code, out, err) = run(
        root,
        &[
            "db",
            "rollback",
            "--from",
            backup.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!(code, 0, "rollback failed: {out}{err}");
    assert!(out.contains("rolled back"), "{out}");
    assert!(
        out.contains("integrity_check: ok"),
        "rollback verifies what it restored: {out}"
    );
    {
        let storage = probe_storage::Storage::open(&db_path(root)).unwrap();
        assert!(
            storage.last_round().unwrap().is_none(),
            "the round written after the backup must be gone"
        );
    }
    stop_writers(root);

    let (code, out, err) = run(root, &["db", "verify"]);
    assert_eq!(code, 0, "verify failed: {out}{err}");
}

#[test]
fn check_refuses_a_missing_ledger_and_names_the_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, out, _) = run(tmp.path(), &["db", "check"]);
    assert_eq!(code, 1, "a missing ledger is not a success: {out}");
    assert!(out.contains("no database at"), "{out}");
    assert!(
        out.contains("state.db"),
        "the message names the file: {out}"
    );
}

#[test]
fn an_unknown_subcommand_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, _, err) = run(tmp.path(), &["db", "vacuum"]);
    assert_eq!(code, 2, "usage errors exit 2: {err}");
    assert!(err.contains("unknown db subcommand"), "{err}");
}

#[test]
fn rollback_without_a_source_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    run(tmp.path(), &["db", "migrate"]);
    let (code, _, err) = run(tmp.path(), &["db", "rollback", "--yes"]);
    assert_eq!(code, 2, "a missing --from is a usage error: {err}");
    assert!(err.contains("--from"), "{err}");
}
