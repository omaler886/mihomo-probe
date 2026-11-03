//! Contract tests for the publishers against a local axum stub that plays
//! api.github.com. Every request stays on 127.0.0.1; the stub serves a
//! scripted sequence per method, records the request bodies, and measures
//! how many requests were in flight at once (the same-gist lock assertion).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use probe_config::GistSection;
use probe_publish::{
    sha256_hex, ArtifactRef, CompositePublisher, GistPublisher, LocalPublisher, PublishOutcome,
    Publisher, MANIFEST_FILE,
};
use serde_json::{json, Value};

/// A fake token. The tests assert it NEVER shows up in an error message;
/// it must never appear anywhere else either.
const TOKEN: &str = "ghp_test_TOKEN_NOT_REAL_1234567890";
const GIST_ID: &str = "gist0001deadbeef";
const SOURCE: &str = "aaaaaaaa";
const FILE: &str = "air-a1.yaml";
const FOREIGN: &str = "cccccccc";

// ---------------------------------------------------------------------------
// The stub
// ---------------------------------------------------------------------------

struct Scripted {
    status: u16,
    retry_after: Option<u64>,
    body: String,
}

fn script(status: u16, body: impl Into<String>) -> Scripted {
    Scripted {
        status,
        retry_after: None,
        body: body.into(),
    }
}

fn script_with_retry_after(status: u16, seconds: u64, body: impl Into<String>) -> Scripted {
    Scripted {
        status,
        retry_after: Some(seconds),
        body: body.into(),
    }
}

/// A canned `GET /gists/{id}` answer with the given `files` map.
fn gist(files: Value) -> Scripted {
    script(200, json!({ "files": files }).to_string())
}

#[derive(Clone)]
struct Stub {
    gets: Arc<Mutex<VecDeque<Scripted>>>,
    patches: Arc<Mutex<VecDeque<Scripted>>>,
    /// Scripted POST answers (the `gist init` create), in order.
    posts: Arc<Mutex<VecDeque<Scripted>>>,
    /// Parsed PATCH bodies, in arrival order.
    patch_bodies: Arc<Mutex<Vec<Value>>>,
    /// Parsed POST bodies, in arrival order.
    post_bodies: Arc<Mutex<Vec<Value>>>,
    /// `Authorization` header values seen (proves the token file content is
    /// what actually authenticates; also the pool of strings leak checks
    /// must NOT be about -- the header itself legitimately carries it).
    auth_seen: Arc<Mutex<Vec<String>>>,
    /// Every raw request line, for the token-leak sweep over server-side
    /// observations.
    raw_log: Arc<Mutex<Vec<String>>>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    /// Per-request stall, to make interleaving visible to `max_in_flight`.
    delay: Duration,
}

impl Stub {
    fn patch_count(&self) -> usize {
        self.patch_bodies.lock().unwrap().len()
    }

    fn patches(&self) -> Vec<Value> {
        self.patch_bodies.lock().unwrap().clone()
    }

    fn post_count(&self) -> usize {
        self.post_bodies.lock().unwrap().len()
    }

    fn posts(&self) -> Vec<Value> {
        self.post_bodies.lock().unwrap().clone()
    }

    fn request_count(&self) -> usize {
        self.raw_log.lock().unwrap().len()
    }

    fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }

    /// The token must not appear in any error message, any stored body
    /// snippet, or anything else a log line could be built from.
    fn assert_no_token_leak(&self, texts: &[String]) {
        let haystack = self
            .raw_log
            .lock()
            .unwrap()
            .iter()
            .chain(texts.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !haystack.contains(TOKEN),
            "the fake token leaked into an error text or log"
        );
    }
}

async fn handle(
    State(stub): State<Stub>,
    method: Method,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) {
        stub.auth_seen
            .lock()
            .unwrap()
            .push(auth.to_str().unwrap_or("<binary>").to_string());
    }
    let parsed = serde_json::from_str::<Value>(&body).ok();
    stub.raw_log
        .lock()
        .unwrap()
        .push(format!("{method} {body}"));
    let scripted = match method {
        Method::PATCH => {
            if let Some(value) = parsed {
                stub.patch_bodies.lock().unwrap().push(value);
            }
            stub.patches.lock().unwrap().pop_front()
        }
        Method::POST => {
            if let Some(value) = parsed {
                stub.post_bodies.lock().unwrap().push(value);
            }
            stub.posts.lock().unwrap().pop_front()
        }
        _ => stub.gets.lock().unwrap().pop_front(),
    };
    // Track concurrency around the stall: an unserialized pair of publishes
    // would show two requests in the air at once.
    let current = stub.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    stub.max_in_flight.fetch_max(current, Ordering::SeqCst);
    tokio::time::sleep(stub.delay).await;
    stub.in_flight.fetch_sub(1, Ordering::SeqCst);
    let scripted = scripted.unwrap_or(Scripted {
        status: 500,
        retry_after: None,
        body: "script exhausted".to_string(),
    });
    let mut response = (
        StatusCode::from_u16(scripted.status).unwrap(),
        scripted.body,
    )
        .into_response();
    if let Some(seconds) = scripted.retry_after {
        if let Ok(value) = seconds.to_string().parse() {
            response.headers_mut().insert("Retry-After", value);
        }
    }
    response
}

async fn start_stub(
    gets: Vec<Scripted>,
    patches: Vec<Scripted>,
    delay: Duration,
) -> (String, Stub) {
    start_stub_full(gets, patches, Vec::new(), delay).await
}

/// The same stub with a scripted POST queue, for the `gist init` create.
async fn start_stub_full(
    gets: Vec<Scripted>,
    patches: Vec<Scripted>,
    posts: Vec<Scripted>,
    delay: Duration,
) -> (String, Stub) {
    let stub = Stub {
        gets: Arc::new(Mutex::new(gets.into())),
        patches: Arc::new(Mutex::new(patches.into())),
        posts: Arc::new(Mutex::new(posts.into())),
        patch_bodies: Arc::new(Mutex::new(Vec::new())),
        post_bodies: Arc::new(Mutex::new(Vec::new())),
        auth_seen: Arc::new(Mutex::new(Vec::new())),
        raw_log: Arc::new(Mutex::new(Vec::new())),
        in_flight: Arc::new(AtomicUsize::new(0)),
        max_in_flight: Arc::new(AtomicUsize::new(0)),
        delay,
    };
    let app = axum::Router::new()
        .fallback(handle)
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), stub)
}

// ---------------------------------------------------------------------------
// Construction helpers
// ---------------------------------------------------------------------------

/// A config section whose token file lives in `dir` (0600 content on Unix is
/// out of scope for tests: the mode check is Unix-only and the file is a
/// test fixture).
fn gist_section(dir: &Path, gist_id: Option<&str>) -> GistSection {
    let token_file = dir.join("gist.token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    GistSection {
        gist_id: gist_id.map(str::to_string),
        token_file: Some(token_file),
        ..GistSection::default()
    }
}

fn publisher_in(base: &str, dir: &Path) -> GistPublisher {
    GistPublisher::with_api_base(&gist_section(dir, Some(GIST_ID)), base)
        .unwrap()
        .with_backoff_base(Duration::from_millis(2))
        .with_max_retry_after(Duration::from_millis(20))
}

/// The manifest content the stub serves: a foreign source's record plus,
/// optionally, our own (stale) one.
fn stub_manifest(own_sha: Option<&str>) -> String {
    let mut manifest = serde_json::Map::new();
    manifest.insert(
        FOREIGN.to_string(),
        json!({"source_id": FOREIGN, "file": "ceta-c3.yaml", "sha256": "03",
               "published_at": "2026-01-01T00:00:00Z", "revision": "03", "status": "active"}),
    );
    if let Some(sha) = own_sha {
        manifest.insert(
            SOURCE.to_string(),
            json!({"source_id": SOURCE, "file": FILE, "sha256": sha,
                   "published_at": "2026-01-01T00:00:00Z", "revision": &sha[..12],
                   "status": "active"}),
        );
    }
    Value::Object(manifest).to_string()
}

fn artifact(previous: Option<String>) -> ArtifactRef {
    ArtifactRef {
        previous_sha256: previous,
        ..ArtifactRef::new(SOURCE, FILE)
    }
}

const OLD_CONTENT: &str = "old content\n";
const NEW_CONTENT: &str = "new content\n";

// ---------------------------------------------------------------------------
// Idempotence (F-032)
// ---------------------------------------------------------------------------

/// A publish whose content hash matches the artifact row is NoChange with
/// ZERO HTTP requests -- not even the GET.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_change_makes_zero_requests() {
    let (base, stub) = start_stub(vec![], vec![], Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let outcome = publisher
        .publish(
            &artifact(Some(sha256_hex(NEW_CONTENT.as_bytes()))),
            NEW_CONTENT,
        )
        .await;
    assert_eq!(outcome, PublishOutcome::NoChange);
    assert_eq!(stub.request_count(), 0, "a no-change publish must not talk");
    assert_eq!(stub.patch_count(), 0);
    stub.assert_no_token_leak(&[]);
}

/// Gate 2: the row's baseline is unknown, but the Gist already holds exactly
/// this content with a current manifest entry -- still NoChange, still zero
/// PATCHes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_remote_content_with_a_current_manifest_is_no_change() {
    let sha = sha256_hex(NEW_CONTENT.as_bytes());
    let files = json!({
        (FILE): {"content": NEW_CONTENT},
        "other.yaml": {"content": "keep me"},
        (MANIFEST_FILE): {"content": stub_manifest(Some(&sha))},
    });
    let (base, stub) = start_stub(vec![gist(files)], vec![], Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let outcome = publisher.publish(&artifact(None), NEW_CONTENT).await;
    assert_eq!(outcome, PublishOutcome::NoChange);
    assert_eq!(
        stub.patch_count(),
        0,
        "identical remote content is not re-SENT"
    );
    assert_eq!(stub.request_count(), 1, "exactly the pre-write GET");
}

// ---------------------------------------------------------------------------
// The PATCH contract (F-030/F-031)
// ---------------------------------------------------------------------------

/// A changed content GETs the gist, then PATCHes exactly two file keys --
/// the target and manifest.json -- and the manifest merge preserves the
/// foreign source's entry. No other file is named (a null would DELETE it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_content_patches_only_the_target_and_the_manifest() {
    let files = json!({
        (FILE): {"content": OLD_CONTENT},
        "other.yaml": {"content": "keep me"},
        (MANIFEST_FILE): {"content": stub_manifest(Some(&sha256_hex(OLD_CONTENT.as_bytes())))},
    });
    let (base, stub) = start_stub(vec![gist(files)], vec![script(200, "{}")], Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let outcome = publisher
        .publish(
            &artifact(Some(sha256_hex(OLD_CONTENT.as_bytes()))),
            NEW_CONTENT,
        )
        .await;
    let PublishOutcome::Published {
        revision,
        gist_failed,
    } = outcome
    else {
        panic!("expected Published, got {outcome:?}");
    };
    assert!(gist_failed.is_none());
    let sha = sha256_hex(NEW_CONTENT.as_bytes());
    assert_eq!(revision.as_deref(), Some(&sha[..12]));

    let patches = stub.patches();
    assert_eq!(patches.len(), 1, "one PATCH, no more");
    let files = patches[0]["files"].as_object().unwrap();
    let mut keys: Vec<&str> = files.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [FILE, "manifest.json"],
        "only the target and the manifest"
    );
    assert_eq!(files[FILE]["content"], NEW_CONTENT);
    assert!(
        !files.get("other.yaml").is_some_and(|v| v.is_null()),
        "foreign files are never nulled"
    );
    assert!(!patches[0]["files"]
        .as_object()
        .unwrap()
        .contains_key("other.yaml"));

    // The manifest keeps the foreign entry and refreshes our own.
    let manifest: Value =
        serde_json::from_str(files["manifest.json"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        manifest[FOREIGN]["file"], "ceta-c3.yaml",
        "the foreign source's entry survives the merge"
    );
    assert_eq!(manifest[FOREIGN]["sha256"], "03");
    let own = &manifest[SOURCE];
    assert_eq!(own["file"], FILE);
    assert_eq!(own["sha256"], sha);
    assert_eq!(own["status"], "active");
    assert!(!own["published_at"].as_str().unwrap().is_empty());
    assert_eq!(own["revision"], &sha[..12]);
}

/// The target file is already live on the Gist but the manifest drifted:
/// only the manifest is written back, the content file is left untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_content_with_a_drifted_manifest_repairs_the_manifest_only() {
    let sha = sha256_hex(NEW_CONTENT.as_bytes());
    // The remote manifest carries only the foreign source -- our entry is
    // missing, though the content itself is already correct.
    let files = json!({
        (FILE): {"content": NEW_CONTENT},
        (MANIFEST_FILE): {"content": stub_manifest(None)},
    });
    let (base, stub) = start_stub(vec![gist(files)], vec![script(200, "{}")], Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let outcome = publisher.publish(&artifact(None), NEW_CONTENT).await;
    assert!(
        matches!(outcome, PublishOutcome::Published { .. }),
        "{outcome:?}"
    );

    let patches = stub.patches();
    assert_eq!(patches.len(), 1);
    let files = patches[0]["files"].as_object().unwrap();
    assert_eq!(
        files.keys().map(String::as_str).collect::<Vec<_>>(),
        ["manifest.json"],
        "the content file is already correct and must not be re-SENT"
    );
    let manifest: Value =
        serde_json::from_str(files["manifest.json"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(manifest[SOURCE]["sha256"], sha);
    assert_eq!(manifest[FOREIGN]["file"], "ceta-c3.yaml");
}

// ---------------------------------------------------------------------------
// Error and retry semantics (F-037)
// ---------------------------------------------------------------------------

/// 409: the remote state moved under us. The publisher re-reads and merges
/// ONCE (it does not blindly resend), then succeeds. A second 409 is a
/// terminal failure, not a loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conflict_regets_and_merges_once() {
    let files = json!({
        (FILE): {"content": OLD_CONTENT},
        (MANIFEST_FILE): {"content": stub_manifest(Some(&sha256_hex(OLD_CONTENT.as_bytes())))},
    });
    let (base, stub) = start_stub(
        vec![gist(files.clone()), gist(files)],
        vec![script(409, "conflict"), script(200, "{}")],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let outcome = publisher
        .publish(
            &artifact(Some(sha256_hex(OLD_CONTENT.as_bytes()))),
            NEW_CONTENT,
        )
        .await;
    assert!(
        matches!(outcome, PublishOutcome::Published { .. }),
        "the merge retry must land the publish: {outcome:?}"
    );
    assert_eq!(stub.patch_count(), 2, "one conflict, one retry -- no more");
    assert_eq!(stub.request_count(), 4, "each pass is its own GET+PATCH");

    // A 409 twice in a row is a failure, never a merge loop.
    let (base, stub) = start_stub(
        vec![gist(json!({})), gist(json!({}))],
        vec![script(409, "conflict"), script(409, "conflict")],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let outcome = publisher.publish(&artifact(None), NEW_CONTENT).await;
    assert!(
        matches!(
            outcome,
            PublishOutcome::Failed {
                retryable: false,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(stub.patch_count(), 2);
}

/// 401: exactly one PATCH attempt, a terminal non-retryable `gist_auth`, and
/// the token appears in no error text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthorized_is_never_retried_and_never_leaks_the_token() {
    let (base, stub) = start_stub(
        vec![gist(json!({}))],
        vec![script(401, r#"{"message":"Bad credentials"}"#)],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let outcome = publisher
        .publish(
            &artifact(Some(sha256_hex(OLD_CONTENT.as_bytes()))),
            NEW_CONTENT,
        )
        .await;
    let PublishOutcome::Failed {
        code,
        retryable,
        message,
    } = outcome
    else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(code.as_str(), "gist_auth");
    assert!(!retryable, "a 401 must not be retried");
    assert_eq!(stub.patch_count(), 1, "one attempt, no retries");
    stub.assert_no_token_leak(&[message]);

    // And the credential actually used IS the token file's content, trimmed.
    let auth = stub.auth_seen.lock().unwrap().clone();
    assert!(auth.iter().all(|value| *value == format!("Bearer {TOKEN}")));
}

/// 404 on the pre-write GET: terminal `gist_missing`, no PATCH at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_gist_is_terminal() {
    let (base, stub) = start_stub(
        vec![script(404, r#"{"message":"Not Found"}"#)],
        vec![],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let outcome = publisher.publish(&artifact(None), NEW_CONTENT).await;
    assert_eq!(
        outcome,
        PublishOutcome::Failed {
            code: probe_publish::PublishErrorCode::GistMissing,
            retryable: false,
            message: outcome_error_message(&outcome),
        }
    );
    assert_eq!(stub.patch_count(), 0);
    stub.assert_no_token_leak(&[outcome_error_message(&outcome)]);
}

/// 429 with a `Retry-After` of an hour: honored but bounded -- the publish
/// retries after the clamped millisecond cap and succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_honors_retry_after_within_the_bound() {
    let (base, stub) = start_stub(
        vec![gist(json!({}))],
        vec![
            script_with_retry_after(429, 3600, r#"{"message":"Too many requests"}"#),
            script(200, "{}"),
        ],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path()); // max_retry_after = 20ms
    let started = std::time::Instant::now();
    let outcome = publisher.publish(&artifact(None), NEW_CONTENT).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the clamp must bound the wait"
    );
    assert!(
        matches!(outcome, PublishOutcome::Published { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        stub.patch_count(),
        2,
        "the 429 was retried once and succeeded"
    );
}

/// 5xx: exactly `max_retries` attempts (first try included), then a
/// retryable `remote` failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_errors_are_attempted_exactly_max_retries_times() {
    let (base, stub) = start_stub(
        vec![gist(json!({}))],
        vec![
            script(500, "boom"),
            script(500, "boom"),
            script(500, "boom"),
        ],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path()); // max_retries = 3
    let outcome = publisher.publish(&artifact(None), NEW_CONTENT).await;
    let PublishOutcome::Failed {
        code,
        retryable,
        message,
    } = outcome
    else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(code.as_str(), "remote");
    assert!(retryable, "a 5xx exhaustion stays retryable");
    assert_eq!(stub.patch_count(), 3, "exactly max_retries attempts");
    stub.assert_no_token_leak(&[message]);

    // Same budget, but GitHub recovers on the third try: published.
    let (base, stub) = start_stub(
        vec![gist(json!({}))],
        vec![script(500, "boom"), script(500, "boom"), script(200, "{}")],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let outcome = publisher.publish(&artifact(None), NEW_CONTENT).await;
    assert!(
        matches!(outcome, PublishOutcome::Published { .. }),
        "{outcome:?}"
    );
    assert_eq!(stub.patch_count(), 3);
}

/// A missing or empty token file is a CONSTRUCTION error -- before any
/// request could be built, let alone sent.
#[test]
fn a_broken_token_file_fails_at_construction() {
    for section in [
        // Not configured at all.
        GistSection::default(),
        // Path that does not exist.
        GistSection {
            token_file: Some(std::env::temp_dir().join("definitely-not-here-90210.token")),
            ..GistSection::default()
        },
    ] {
        let error = GistPublisher::with_api_base(&section, "http://127.0.0.1:1").unwrap_err();
        assert_eq!(
            error.code,
            probe_publish::PublishErrorCode::Config,
            "{error}"
        );
        assert!(!error.message.contains(TOKEN));
    }

    // An empty file is not a token either.
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("empty.token");
    std::fs::write(&token_file, "   \n").unwrap();
    let section = GistSection {
        token_file: Some(token_file),
        ..GistSection::default()
    };
    let error = GistPublisher::with_api_base(&section, "http://127.0.0.1:1").unwrap_err();
    assert!(error.message.contains("empty"), "{error}");
    assert!(!error.message.contains(TOKEN));
}

/// healthcheck maps 200 -> ok, 404 -> gist_missing, 401 -> gist_auth, all in
/// one probe per status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthcheck_maps_statuses() {
    let (base, _stub) = start_stub(
        vec![
            gist(json!({})),
            script(404, r#"{"message":"Not Found"}"#),
            script(401, r#"{"message":"Bad credentials"}"#),
        ],
        vec![],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());

    publisher.healthcheck(GIST_ID).await.unwrap();

    let error = publisher.healthcheck(GIST_ID).await.unwrap_err();
    assert_eq!(error.code, probe_publish::PublishErrorCode::GistMissing);

    let error = publisher.healthcheck(GIST_ID).await.unwrap_err();
    assert_eq!(error.code, probe_publish::PublishErrorCode::GistAuth);
    assert!(!error.to_string().contains(TOKEN));

    // An empty probe id falls back to the configured gist.
    let (base2, _stub2) = start_stub(vec![gist(json!({}))], vec![], Duration::ZERO).await;
    let dir2 = tempfile::tempdir().unwrap();
    let publisher2 = publisher_in(&base2, dir2.path());
    publisher2.healthcheck("").await.unwrap();
}

// ---------------------------------------------------------------------------
// The same-gist write lock (F-033)
// ---------------------------------------------------------------------------

/// Two concurrent publishes to the SAME gist (distinct publishers, as two
/// sources would have) serialize end to end: the stub never sees two
/// requests in the air, so the read-merge-write of one publish can never
/// interleave with the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_publishes_to_one_gist_serialize() {
    let (base, stub) = start_stub(
        vec![gist(json!({})), gist(json!({}))],
        vec![script(200, "{}"), script(200, "{}")],
        Duration::from_millis(100),
    )
    .await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let publisher_a = publisher_in(&base, dir_a.path());
    let publisher_b = publisher_in(&base, dir_b.path());

    let artifact_a = artifact(None);
    let artifact_b = artifact(None);
    let (outcome_a, outcome_b) = tokio::join!(
        publisher_a.publish(&artifact_a, "content A\n"),
        publisher_b.publish(&artifact_b, "content B\n"),
    );
    assert!(
        matches!(outcome_a, PublishOutcome::Published { .. }),
        "{outcome_a:?}"
    );
    assert!(
        matches!(outcome_b, PublishOutcome::Published { .. }),
        "{outcome_b:?}"
    );
    assert_eq!(stub.patch_count(), 2);
    assert_eq!(
        stub.max_in_flight(),
        1,
        "the per-gist lock must keep one publish's GET+PATCH from overlapping the other's"
    );
}

// ---------------------------------------------------------------------------
// The composite publisher
// ---------------------------------------------------------------------------

/// Local first, gist second: a healthy pair publishes both, and a Gist
/// failure after a local success is reported (`gist_failed`) without
/// failing the publish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn composite_writes_local_then_gist_and_survives_a_gist_failure() {
    let root = tempfile::tempdir().unwrap();
    let local = LocalPublisher::new(root.path());

    // Healthy pair.
    let (base, stub) = start_stub(
        vec![gist(json!({}))],
        vec![script(200, "{}")],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let composite = CompositePublisher::new(local.clone(), publisher_in(&base, dir.path()));
    let outcome = composite.publish(&artifact(None), NEW_CONTENT).await;
    let PublishOutcome::Published {
        revision,
        gist_failed,
    } = outcome
    else {
        panic!("expected Published, got {outcome:?}");
    };
    assert!(gist_failed.is_none());
    assert!(revision.is_some());
    assert_eq!(
        std::fs::read_to_string(root.path().join(FILE)).unwrap(),
        NEW_CONTENT,
        "the local artifact is in place"
    );
    assert_eq!(stub.patch_count(), 1);

    // Gist gone (404 on the GET), content changed again: local wins, the
    // Gist failure rides along in `gist_failed`.
    let (base, stub) = start_stub(
        vec![script(404, r#"{"message":"Not Found"}"#)],
        vec![],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let composite = CompositePublisher::new(local.clone(), publisher_in(&base, dir.path()));
    let outcome = composite.publish(&artifact(None), "content v3\n").await;
    let PublishOutcome::Published { gist_failed, .. } = outcome else {
        panic!("expected Published with a gist failure, got {outcome:?}");
    };
    assert!(
        gist_failed
            .as_deref()
            .is_some_and(|reason| reason.contains("gist_missing")),
        "the gist failure must be reported: {gist_failed:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join(FILE)).unwrap(),
        "content v3\n",
        "the local artifact was updated even though the gist failed"
    );
    assert_eq!(stub.patch_count(), 0);
    stub.assert_no_token_leak(&[]);
}

/// NoChange needs BOTH legs to agree; a failed local leg fails the whole
/// publish and never touches the Gist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn composite_no_change_and_local_failure_rules() {
    let root = tempfile::tempdir().unwrap();
    let local = LocalPublisher::new(root.path());

    // Both legs no-change (gate 1 fires in both): zero requests.
    let (base, stub) = start_stub(vec![], vec![], Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let composite = CompositePublisher::new(local.clone(), publisher_in(&base, dir.path()));
    let outcome = composite
        .publish(
            &artifact(Some(sha256_hex(NEW_CONTENT.as_bytes()))),
            NEW_CONTENT,
        )
        .await;
    assert_eq!(outcome, PublishOutcome::NoChange);
    assert_eq!(
        stub.request_count(),
        0,
        "a no-change composite must not talk"
    );

    // Local floor broken: Failed, and the gist leg never runs.
    let blocked_root = root.path().join("not-a-dir");
    std::fs::write(&blocked_root, "a file, not a directory").unwrap();
    let (base, stub) = start_stub(
        vec![gist(json!({}))],
        vec![script(200, "{}")],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let composite = CompositePublisher::new(
        LocalPublisher::new(blocked_root.join("sub")),
        publisher_in(&base, dir.path()),
    );
    let outcome = composite.publish(&artifact(None), "content v4\n").await;
    assert!(
        matches!(
            &outcome,
            PublishOutcome::Failed {
                retryable: true,
                ..
            }
        ),
        "a local IO failure fails the publish: {outcome:?}"
    );
    assert_eq!(
        stub.request_count(),
        0,
        "a broken local floor must not reach the gist"
    );
}

fn outcome_error_message(outcome: &PublishOutcome) -> String {
    match outcome {
        PublishOutcome::Failed { message, .. } => message.clone(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// create_gist -- the one-time `gist init` step (workstreams/18 B2)
// ---------------------------------------------------------------------------

/// A 201 create answers with the new gist id, is secret by default, seeds
/// exactly the `manifest.json` placeholder, authenticates with the token
/// file's content, and is sent EXACTLY ONCE (no retry can exist: a retried
/// POST that lands twice is how duplicate Gists are born).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_gist_posts_once_secret_by_default() {
    let (base, stub) = start_stub_full(
        vec![],
        vec![],
        vec![script(
            201,
            json!({"id": GIST_ID, "html_url": "https://gist.github.com/x", "public": false})
                .to_string(),
        )],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let gist_id = publisher.create_gist("mihomo-probe exports").await.unwrap();
    assert_eq!(gist_id, GIST_ID);

    assert_eq!(stub.post_count(), 1, "one POST, never a retry");
    let body = &stub.posts()[0];
    assert_eq!(body["public"], false, "secret by default");
    assert_eq!(body["description"], "mihomo-probe exports");
    let files = body["files"].as_object().unwrap();
    assert_eq!(
        files.keys().map(String::as_str).collect::<Vec<_>>(),
        [MANIFEST_FILE],
        "the create seeds only the manifest placeholder"
    );
    let manifest: Value =
        serde_json::from_str(files[MANIFEST_FILE]["content"].as_str().unwrap()).unwrap();
    assert!(
        manifest.is_object(),
        "the placeholder is a JSON object the first publish can merge into"
    );
    let auth = stub.auth_seen.lock().unwrap().clone();
    assert!(
        auth.iter().all(|value| *value == format!("Bearer {TOKEN}")),
        "the create authenticates with the token file's content"
    );
    stub.assert_no_token_leak(&[]);
}

/// `GistSection::public = true` is the only way a public Gist is created.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_gist_honors_the_public_flag() {
    let (base, stub) = start_stub_full(
        vec![],
        vec![],
        vec![script(201, json!({"id": GIST_ID}).to_string())],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let section = GistSection {
        public: true,
        ..gist_section(dir.path(), None)
    };
    let publisher = GistPublisher::with_api_base(&section, &base).unwrap();
    publisher.create_gist("x").await.unwrap();
    assert_eq!(stub.posts()[0]["public"], true);
}

/// A refused create maps the status and stays a single attempt (the 422 here
/// is the "validate before create" case a naive retry would double).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_create_maps_the_status_without_retrying() {
    let (base, stub) = start_stub_full(
        vec![],
        vec![],
        vec![script(422, r#"{"message":"Validation Failed"}"#)],
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let publisher = publisher_in(&base, dir.path());
    let error = publisher.create_gist("x").await.unwrap_err();
    assert_eq!(error.code, probe_publish::PublishErrorCode::Remote);
    assert!(!error.retryable, "a failed create is never auto-retried");
    assert_eq!(stub.post_count(), 1, "no retries on a create");
    stub.assert_no_token_leak(std::slice::from_ref(&error.message));
}

/// A create whose response never arrives (dead port) is a `network` failure
/// and -- again -- exactly one attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_create_that_never_completes_is_reported_once() {
    let dir = tempfile::tempdir().unwrap();
    let publisher =
        GistPublisher::with_api_base(&gist_section(dir.path(), None), "http://127.0.0.1:1")
            .unwrap()
            .with_backoff_base(Duration::from_millis(2))
            .with_max_retry_after(Duration::from_millis(20));
    let error = publisher.create_gist("x").await.unwrap_err();
    assert_eq!(error.code, probe_publish::PublishErrorCode::Network);
    assert!(
        !error.retryable,
        "a lost create response must not be retried"
    );
    assert!(!error.message.contains(TOKEN));
}
