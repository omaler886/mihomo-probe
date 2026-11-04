//! HTTP tests for the GUI surface (workstreams/18 §B4): the embedded SPA, the
//! CSRF session flow, the audited write, the DTO whitelist and the batch
//! boundary the SPA degrades on. Same in-process style as the rest of the
//! crate: a real router on a loopback port driven by reqwest.

use std::sync::Arc;

use probe_config::SourceSpec;
use probe_engine::RoundSettings;
use probe_mihomo::Controller;
use probe_storage::Storage;
use serde_json::Value;

use super::AUDIT_LEVEL;
use crate::{AppState, ServeConfig, SharedState};

const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EVIL: &str = "<script>alert(1)</script>";

/// A state whose round path never dials a real port: an empty source list
/// means collection has nothing to fetch, and the missing core secret blocks
/// the round before any kernel interaction (the row just closes blocked).
fn gui_state(root: &std::path::Path, sources: Vec<SourceSpec>) -> SharedState {
    let cfg = probe_config::Config::load(std::path::Path::new("/nonexistent/config.json"))
        .expect("default config");
    Arc::new(AppState::new(
        Storage::open_in_memory().unwrap(),
        Some(ADMIN.into()),
        Controller::new("http://127.0.0.1:1", None),
        ServeConfig {
            kernel_config_path: "/test/config.yaml".into(),
            round: RoundSettings::from_config(&cfg),
            core: cfg.core.clone(),
            sources,
            backend: "http://127.0.0.1:1".into(),
            root: root.to_path_buf(),
            substore: None,
            chain: cfg.chain.clone(),
            publish_prefix: cfg.publish_prefix.clone(),
            publish: cfg.publish.clone(),
            schedule: cfg.schedule.clone(),
            policy: cfg.policy.clone(),
            min_run_interval_seconds: 0,
        },
    ))
}

/// The real router: `build_router` now carries the GUI merge itself, so the
/// tests exercise the exact production surface instead of re-merging the GUI
/// router (which would overlap-method-panic on the duplicated routes).
async fn spawn(state: SharedState) -> String {
    let app = crate::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

async fn get(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: Option<&str>,
) -> reqwest::Response {
    let mut request = client.get(format!("{base}{path}"));
    if let Some(token) = token {
        request = request.header("X-Auth-Token", token);
    }
    request.send().await.unwrap()
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: Option<&str>,
    csrf: Option<&str>,
    cookie: Option<&str>,
) -> reqwest::Response {
    let mut request = client.post(format!("{base}{path}"));
    if let Some(token) = token {
        request = request.header("X-Auth-Token", token);
    }
    if let Some(csrf) = csrf {
        request = request.header("X-CSRF-Token", csrf);
    }
    if let Some(cookie) = cookie {
        request = request.header("Cookie", cookie);
    }
    request.send().await.unwrap()
}

/// Exchange the admin token for the CSRF pair: the token the SPA echoes back
/// as `X-CSRF-Token`, and the `probe_csrf=...` cookie value to replay.
async fn establish_session(client: &reqwest::Client, base: &str) -> (String, String) {
    let response = client
        .get(format!("{base}/api/v1/session"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "the admin token must open a session"
    );
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .expect("session sets a cookie")
        .to_str()
        .unwrap()
        .to_string();
    let body: Value = response.json().await.unwrap();
    let csrf = body["csrf_token"]
        .as_str()
        .expect("session issues a csrf token")
        .to_string();
    assert!(
        set_cookie.starts_with("probe_csrf="),
        "cookie name: {set_cookie}"
    );
    assert!(
        set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"),
        "cookie hardening flags: {set_cookie}"
    );
    let cookie = set_cookie.split(';').next().unwrap().trim().to_string();
    (csrf, cookie)
}

fn sorted_keys(body: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = body
        .as_object()
        .expect("a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// `GET /gui` serves the SPA with the private-response headers.
#[tokio::test]
async fn gui_page_serves_with_security_headers() {
    let tmp = tempfile::tempdir().unwrap();
    let base = spawn(gui_state(tmp.path(), Vec::new())).await;
    let client = reqwest::Client::new();
    let response = get(&client, &base, "/gui", None).await;
    assert_eq!(response.status(), 200);
    let headers = response.headers();
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
    assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
    let content_type = headers.get("content-type").unwrap().to_str().unwrap();
    assert!(
        content_type.starts_with("text/html"),
        "type: {content_type}"
    );
    let html = response.text().await.unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("width=device-width"), "responsive viewport");
    // The escape discipline is structural: dynamic values reach the DOM via
    // textContent only, and no HTML-writing primitive exists at all.
    assert!(html.contains("textContent"));
    assert!(
        !html.contains("innerHTML"),
        "no innerHTML in the GUI script"
    );
    assert!(!html.contains("insertAdjacentHTML"));
    assert!(!html.contains("document.write"));
}

/// The session endpoint reuses the admin-token mechanism: an absent or wrong
/// token is the same 401 every guarded endpoint answers with.
#[tokio::test]
async fn session_requires_the_admin_token() {
    let tmp = tempfile::tempdir().unwrap();
    let base = spawn(gui_state(tmp.path(), Vec::new())).await;
    let client = reqwest::Client::new();
    for present in [None, Some("wrong-token-that-is-not-the-admin")] {
        let response = get(&client, &base, "/api/v1/session", present).await;
        assert_eq!(response.status(), 401);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "unauthorized");
    }
}

/// A valid admin token gets exactly the CSRF pair and nothing else: the
/// response is whitelisted to two fields, carries no credential material,
/// and the cookie mirrors the token with the hardening flags.
#[tokio::test]
async fn session_issues_csrf_pair_and_no_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let base = spawn(gui_state(tmp.path(), Vec::new())).await;
    let client = reqwest::Client::new();
    let response = get(&client, &base, "/api/v1/session", Some(ADMIN)).await;
    assert_eq!(response.status(), 200);
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let body: Value = response.json().await.unwrap();
    assert_eq!(sorted_keys(&body), vec!["csrf_token", "expires_in"]);
    let token = body["csrf_token"].as_str().unwrap();
    assert_eq!(token.len(), 32, "128 bits as hex: {token}");
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    let text = serde_json::to_string(&body).unwrap();
    assert!(!text.contains(ADMIN), "no admin token echo: {text}");
    assert!(
        set_cookie.contains(token),
        "double submit: cookie mirrors the token: {set_cookie}"
    );
    assert!(
        set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"),
        "cookie hardening: {set_cookie}"
    );
}

/// CSRF enforcement on the GUI write: missing header, missing cookie, wrong
/// token and a header/cookie mismatch are all 403 before anything runs.
#[tokio::test]
async fn gui_write_requires_admin_and_csrf() {
    let tmp = tempfile::tempdir().unwrap();
    let base = spawn(gui_state(tmp.path(), Vec::new())).await;
    let client = reqwest::Client::new();
    // No admin token: 401 before CSRF is even considered.
    let response = post(&client, &base, "/api/v1/gui/runs", None, None, None).await;
    assert_eq!(response.status(), 401);
    // Admin token but no CSRF pair at all: 403.
    let response = post(&client, &base, "/api/v1/gui/runs", Some(ADMIN), None, None).await;
    assert_eq!(response.status(), 403);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "csrf");
    // A full session, then every partial pair:
    let (csrf, cookie) = establish_session(&client, &base).await;
    let response = post(
        &client,
        &base,
        "/api/v1/gui/runs",
        Some(ADMIN),
        Some(&csrf),
        None,
    )
    .await;
    assert_eq!(response.status(), 403, "header without cookie");
    let response = post(
        &client,
        &base,
        "/api/v1/gui/runs",
        Some(ADMIN),
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), 403, "cookie without header");
    let response = post(
        &client,
        &base,
        "/api/v1/gui/runs",
        Some(ADMIN),
        Some(&"b".repeat(32)),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), 403, "wrong header token");
    let response = post(
        &client,
        &base,
        "/api/v1/gui/runs",
        Some(ADMIN),
        Some(&csrf),
        Some(&format!("probe_csrf={}", "c".repeat(32))),
    )
    .await;
    assert_eq!(response.status(), 403, "header/cookie mismatch");
}

/// The happy path: a valid pair starts a round through the same trigger path
/// `POST /api/v1/rounds` uses, and the write (plus the session issuance)
/// lands in the audit ledger.
#[tokio::test]
async fn gui_write_with_valid_pair_starts_round_and_audits() {
    let tmp = tempfile::tempdir().unwrap();
    let state = gui_state(tmp.path(), Vec::new());
    let base = spawn(state.clone()).await;
    let client = reqwest::Client::new();
    let (csrf, cookie) = establish_session(&client, &base).await;
    let response = post(
        &client,
        &base,
        "/api/v1/gui/runs",
        Some(ADMIN),
        Some(&csrf),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), 202);
    let text = response.text().await.unwrap();
    assert!(!text.contains(ADMIN), "no admin token echo: {text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(sorted_keys(&body), vec!["round_id", "started"]);
    assert_eq!(body["started"], true);
    // The audit rows: one for the write, one for the session that armed it.
    let events = state.storage.lock().unwrap().recent_events(100).unwrap();
    assert!(
        events
            .iter()
            .any(|row| row.level == AUDIT_LEVEL && row.message.contains("gui.trigger_run")),
        "audit row expected; events: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|row| row.level == AUDIT_LEVEL && row.message.contains("gui.session_issued")),
        "session audit row expected; events: {events:?}"
    );
}

/// No GUI/session response ever carries a credential or an absolute
/// filesystem path.
#[tokio::test]
async fn gui_responses_never_carry_secrets_or_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let base = spawn(gui_state(tmp.path(), Vec::new())).await;
    let client = reqwest::Client::new();
    let html = get(&client, &base, "/gui", None)
        .await
        .text()
        .await
        .unwrap();
    assert!(
        !html.contains(ADMIN),
        "the page never embeds the admin token"
    );
    for forbidden in ["C:\\", "D:\\", "/root/", "/home/"] {
        assert!(!html.contains(forbidden), "no absolute path: {forbidden}");
    }
    let session = get(&client, &base, "/api/v1/session", Some(ADMIN))
        .await
        .text()
        .await
        .unwrap();
    assert!(!session.contains(ADMIN), "no admin token echo: {session}");
    let (csrf, cookie) = establish_session(&client, &base).await;
    let write = post(
        &client,
        &base,
        "/api/v1/gui/runs",
        Some(ADMIN),
        Some(&csrf),
        Some(&cookie),
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(!write.contains(ADMIN), "no admin token echo: {write}");
}

/// The XSS case from the contract: a hostile `display_name` travels through
/// the JSON API (whose contract is to carry it) but never appears in the
/// served HTML or the session response.
#[tokio::test]
async fn xss_display_name_never_reaches_the_page() {
    let evil = SourceSpec {
        key: "evil".into(),
        kind: "collection".into(),
        name: "evil-collection".into(),
        label: EVIL.into(),
        enabled: true,
        relay: false,
        direct: true,
        chain: false,
        export: true,
    };
    let tmp = tempfile::tempdir().unwrap();
    let base = spawn(gui_state(tmp.path(), vec![evil])).await;
    let client = reqwest::Client::new();
    // The JSON API keeps the raw value: rendering, not storage, is the
    // defence -- and the SPA renders it as text.
    let sources = get(&client, &base, "/api/v1/sources", Some(ADMIN))
        .await
        .text()
        .await
        .unwrap();
    assert!(sources.contains(EVIL), "the API carries the raw value");
    let html = get(&client, &base, "/gui", None)
        .await
        .text()
        .await
        .unwrap();
    assert!(!html.contains(EVIL), "the page never embeds ledger data");
    let session = get(&client, &base, "/api/v1/session", Some(ADMIN))
        .await
        .text()
        .await
        .unwrap();
    assert!(
        !session.contains(EVIL),
        "the session carries no ledger data"
    );
}

/// Batch boundary (B5 landed): the publisher test endpoint AND the migration
/// endpoints now EXIST and are admin-gated -- 401 without the token, like
/// every guarded endpoint. With the token, an unreachable Sub-Store backend
/// fails honestly as a mapped error (the bad-gateway envelope), never as a
/// success-shaped body. The page itself is pinned to call those documented
/// paths, degrade on errors/`configured:false`, double-confirm deletion,
/// announce dynamic status and offer the light/dark theme.
#[tokio::test]
async fn migrations_and_gist_test_are_admin_gated() {
    let tmp = tempfile::tempdir().unwrap();
    let base = spawn(gui_state(tmp.path(), Vec::new())).await;
    let client = reqwest::Client::new();
    for path in [
        "/api/v1/migrations/substore/scan",
        "/api/v1/migrations/substore/apply",
        "/api/v1/migrations/substore/rollback",
    ] {
        let response = post(&client, &base, path, None, None, None).await;
        assert_eq!(response.status(), 401, "{path} is guarded");
    }
    // With the token, the unreachable stub backend (gui_state points at
    // 127.0.0.1:1) surfaces as the mapped bad gateway -- the scan is the one
    // endpoint whose backend failure is a hard answer; apply/rollback put
    // per-action failures inside their report, so a body-less call is a
    // malformed request, not a backend probe.
    let response = client
        .post(format!("{base}/api/v1/migrations/substore/scan"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502, "the backend error is mapped");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "backend", "{body}");
    // B2: the publisher test endpoint exists. Without the admin token it is
    // the guarded group's 401; with it, an unconfigured deployment answers
    // the masked `configured:false` the SPA renders as 「未启用」.
    let response = post(
        &client,
        &base,
        "/api/v1/publishers/gist/test",
        None,
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), 401);
    let response = post(
        &client,
        &base,
        "/api/v1/publishers/gist/test",
        Some(ADMIN),
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["configured"], false, "{body}");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error_code"], "config");
    let html = get(&client, &base, "/gui", None)
        .await
        .text()
        .await
        .unwrap();
    for path in [
        "/api/v1/publishers/gist/test",
        "/api/v1/publishers/gist/init",
        "/api/v1/migrations/substore/scan",
        "/api/v1/migrations/substore/apply",
        "/api/v1/migrations/substore/rollback",
    ] {
        assert!(html.contains(path), "the SPA calls {path}");
    }
    assert!(html.contains("未启用"), "degradation copy present");
    assert!(
        html.contains("第二步确认"),
        "deletion needs a second confirm"
    );
    assert!(html.contains("aria-live"), "dynamic status is announced");
    assert!(html.contains("prefers-color-scheme"), "light/dark theme");
}
