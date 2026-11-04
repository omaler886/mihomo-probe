//! Process-level tests for the `migrate` subcommands (workstreams/18 §B5).
//!
//! The Sub-Store backend is a loopback stub; the binary is pointed at it via
//! the same `SUBSTORE_BACKEND` override `serve` honours, so no request ever
//! leaves 127.0.0.1. What the tests pin:
//!
//! * an unknown `migrate` subcommand is a usage error;
//! * `migrate scan --report` writes the plan file, with upstream URLs only
//!   in their redacted form;
//! * `migrate apply` WITHOUT `--execute` is a dry run -- the stub observes
//!   zero write requests;
//! * `migrate apply --execute` really applies, update-only.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

/// A tokenized upstream URL: the outputs must show it only redacted.
const TOKENED_URL: &str = "https://up.example/s?token=tok123";
const TOKEN: &str = "tok123";
const CLASH: &str = "proxies:\n  - {name: n1, type: ss, server: 1.1.1.1, port: 443}\n";

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_probe-cli")
}

fn run_with_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut command = Command::new(binary());
    command.arg("--root").arg(root).args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().expect("spawn probe-cli");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

struct StubSubs {
    records: Mutex<BTreeMap<String, Value>>,
    patches: AtomicUsize,
}

fn stub_state() -> Arc<StubSubs> {
    Arc::new(StubSubs {
        records: Mutex::new(BTreeMap::from([
            (
                "probe-air".to_string(),
                json!({"name": "probe-air", "url": TOKENED_URL, "source": "remote"}),
            ),
            (
                "probe-air-dup".to_string(),
                json!({"name": "probe-air-dup", "url": TOKENED_URL, "source": "remote"}),
            ),
            (
                "other-sub".to_string(),
                json!({"name": "other-sub", "url": "https://up.example/other"}),
            ),
        ])),
        patches: AtomicUsize::new(0),
    })
}

/// The same read/write surface `probe_source::migrate::SubStoreHttpStore`
/// uses: listing, the per-object record, the download route for the digest
/// tier, and the update-only PATCH.
///
/// Served from its own OS thread with its own runtime: the tests below block
/// on the child process, and a blocked executor could not serve the stub.
fn spawn_stub(stub: Arc<StubSubs>) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("stub runtime");
        runtime.block_on(serve_stub(stub, tx));
    });
    rx.recv().unwrap()
}

async fn serve_stub(stub: Arc<StubSubs>, tx: std::sync::mpsc::Sender<String>) {
    let list = AxumState(stub.clone());
    let record_get = AxumState(stub.clone());
    let record_patch = AxumState(stub.clone());
    let app = Router::new()
        .route(
            "/api/subs",
            get(move || async move {
                let records = list.records.lock().unwrap();
                let subs: Vec<Value> = records
                    .values()
                    .map(|record| {
                        json!({
                            "name": record["name"],
                            "url": record["url"],
                            "source": "remote",
                        })
                    })
                    .collect();
                Json(subs)
            }),
        )
        .route(
            "/api/sub/{name}",
            get(move |AxumPath(name): AxumPath<String>| async move {
                let records = record_get.records.lock().unwrap();
                match records.get(&name) {
                    Some(record) => Json(record.clone()).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            })
            .patch(
                move |AxumPath(name): AxumPath<String>, Json(payload): Json<Value>| async move {
                    let mut records = record_patch.records.lock().unwrap();
                    if !records.contains_key(&name) {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    records.insert(name, payload);
                    record_patch.patches.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"ok": true})).into_response()
                },
            ),
        )
        .route(
            "/download/{name}",
            get(move |AxumPath(name): AxumPath<String>| async move {
                if name.starts_with("probe-") {
                    (StatusCode::OK, CLASH.to_string()).into_response()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }),
        )
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tx.send(base).unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// An unknown `migrate` subcommand is a usage error, like `db`'s and `gist`'s.
#[test]
fn an_unknown_migrate_subcommand_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_with_env(tmp.path(), &["migrate", "frobnicate"], &[]);
    assert_eq!(code, 2);
    assert!(err.contains("expected scan | apply | rollback"), "{err}");
}

/// `migrate scan --report FILE` writes the pretty plan file (and prints the
/// plan); the tokenized upstream URL appears only in its redacted form, and
/// a scan never writes to the backend.
#[test]
fn scan_writes_a_redacted_plan_report() {
    let stub = stub_state();
    let backend = spawn_stub(stub.clone());
    let tmp = tempfile::tempdir().unwrap();
    let report = tmp.path().join("plan.json");
    let (code, out, err) = run_with_env(
        tmp.path(),
        &["migrate", "scan", "--report", &report.to_string_lossy()],
        &[("SUBSTORE_BACKEND", &backend)],
    );
    assert_eq!(code, 0, "{out}{err}");
    let text = std::fs::read_to_string(&report).unwrap();
    assert!(text.contains("\"schema\": 1"), "{text}");
    assert!(text.contains("probe-air"), "{text}");
    assert!(
        !text.contains(TOKEN),
        "no raw token in the written plan: {text}"
    );
    assert!(text.contains("https://up.example/s?<redacted>"), "{text}");
    assert!(
        out.contains("\"schema\": 1"),
        "the plan is also printed: {out}"
    );
    assert_eq!(
        stub.patches.load(Ordering::SeqCst),
        0,
        "a scan never writes"
    );
}

/// `migrate apply` WITHOUT `--execute` is a dry run: the output says so in
/// plain words, the report says `dry_run: true`, and the stub observed zero
/// write requests.
#[test]
fn apply_without_execute_is_a_dry_run() {
    let stub = stub_state();
    let backend = spawn_stub(stub.clone());
    let tmp = tempfile::tempdir().unwrap();
    let plan = tmp.path().join("plan.json");
    let (code, _out, err) = run_with_env(
        tmp.path(),
        &["migrate", "scan", "--report", &plan.to_string_lossy()],
        &[("SUBSTORE_BACKEND", &backend)],
    );
    assert_eq!(code, 0, "the scan that produces the fixture failed: {err}");

    let (code, out, err) = run_with_env(
        tmp.path(),
        &["migrate", "apply", "--plan", &plan.to_string_lossy()],
        &[("SUBSTORE_BACKEND", &backend)],
    );
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.contains("dry run"),
        "the dry-run fact is printed: {out}"
    );
    assert!(out.contains("\"dry_run\": true"), "{out}");
    assert!(out.contains("\"applied\": 0"), "{out}");
    assert_eq!(
        stub.patches.load(Ordering::SeqCst),
        0,
        "a dry run never writes"
    );
}

/// `migrate apply --execute` really applies: exactly one PATCH (the
/// retarget), the canonical's URL is the stable artifact URL, and a failed
/// action turns into a non-zero exit.
#[test]
fn apply_with_execute_retargets_on_the_stub() {
    let stub = stub_state();
    let backend = spawn_stub(stub.clone());
    let tmp = tempfile::tempdir().unwrap();
    let plan = tmp.path().join("plan.json");
    let (code, _out, err) = run_with_env(
        tmp.path(),
        &["migrate", "scan", "--report", &plan.to_string_lossy()],
        &[("SUBSTORE_BACKEND", &backend)],
    );
    assert_eq!(code, 0, "the scan that produces the fixture failed: {err}");

    let (code, out, err) = run_with_env(
        tmp.path(),
        &[
            "migrate",
            "apply",
            "--plan",
            &plan.to_string_lossy(),
            "--execute",
        ],
        &[("SUBSTORE_BACKEND", &backend)],
    );
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.contains("REAL APPLY"),
        "the real apply is announced: {out}"
    );
    assert!(out.contains("\"applied\": 1"), "{out}");
    assert_eq!(stub.patches.load(Ordering::SeqCst), 1, "one retarget PATCH");
    let records = stub.records.lock().unwrap();
    // The ledger on this root is empty, so the key ("air") is the fallback id.
    assert_eq!(
        records["probe-air"]["url"], "https://probe.example.com/artifacts/air/subscription",
        "{records:?}"
    );
}
