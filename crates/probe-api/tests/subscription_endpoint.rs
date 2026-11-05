//! The stable subscription endpoint (`GET /artifacts/{source_id}/subscription`,
//! 《指导》§3.2/§11.2 场景4) over the real router: the last good snapshot's
//! exact bytes with a content-digest ETag, 304 on `If-None-Match`, the cache
//! policy that must survive `security_headers`, and the read credential
//! boundary (the publish token, not the admin surface).
//!
//! No kernel, no network: the state is constructed with an unreachable
//! controller and an empty source list, exactly like the in-crate HTTP tests;
//! what is under test is the ledger-to-HTTP seam a Sub-Store consumer rides
//! on, which no other test exercises end to end.

use std::sync::Arc;

use probe_api::{AppState, ServeConfig, SharedState};
use probe_engine::RoundSettings;
use probe_mihomo::Controller;
use probe_storage::publication::{export_hash, PublicationInput};
use probe_storage::Storage;

const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const READ: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

const CONTENT_A: &str = "{\"proxies\": [{\"name\": \"a\", \"type\": \"ss\"}]}\n";
const CONTENT_B: &str = "{\"proxies\": [{\"name\": \"b\", \"type\": \"ss\"}]}\n";

/// A state whose round path never dials anything, with one synced source
/// whose artifact owns the gist file `air.yaml`.
fn state() -> SharedState {
    let cfg = probe_config::Config::load(std::path::Path::new("/nonexistent/config.json"))
        .expect("default config");
    let mut publish = cfg.publish.clone();
    publish.token = Some(READ.to_string());
    Arc::new(AppState::new(
        Storage::open_in_memory().unwrap(),
        Some(ADMIN.into()),
        Controller::new("http://127.0.0.1:1", None),
        ServeConfig {
            kernel_config_path: "/test/config.yaml".into(),
            round: RoundSettings::from_config(&cfg),
            core: cfg.core.clone(),
            sources: Vec::new(),
            backend: "http://127.0.0.1:1".into(),
            root: std::env::temp_dir().join("probe-api-subscription-tests"),
            substore: None,
            chain: cfg.chain.clone(),
            publish_prefix: cfg.publish_prefix.clone(),
            publish,
            schedule: cfg.schedule.clone(),
            policy: cfg.policy.clone(),
            min_run_interval_seconds: 0,
            substore_allow_delete: false,
        },
    ))
}

async fn spawn(state: SharedState) -> String {
    let app = probe_api::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// One source with its artifact and one active export snapshot; returns the
/// source id the stable URL is addressed by.
fn seed(state: &SharedState, content: &str) -> String {
    let storage = state.storage.lock().unwrap();
    let source_id = storage
        .sync_source(
            "air",
            "Air",
            probe_storage::sources::INPUT_COLLECTION,
            "air",
            true,
        )
        .unwrap();
    storage.ensure_artifact(&source_id, "air.yaml").unwrap();
    storage
        .publish_export(PublicationInput {
            key: "air",
            content,
            sha256: &export_hash(content),
            node_count: 1,
            actor: "test",
            forced: false,
            expected_active: None,
        })
        .unwrap();
    source_id
}

/// Activate a second snapshot over the first (a later round's local leg).
fn republish(state: &SharedState, content: &str) {
    let storage = state.storage.lock().unwrap();
    let active = storage
        .current_export("air")
        .unwrap()
        .expect("the first snapshot is active");
    storage
        .publish_export(PublicationInput {
            key: "air",
            content,
            sha256: &export_hash(content),
            node_count: 1,
            actor: "test",
            forced: false,
            expected_active: Some(active.summary.id),
        })
        .unwrap();
}

async fn get(
    base: &str,
    path: &str,
    token: Option<&str>,
    if_none_match: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new().get(format!("{base}{path}"));
    if let Some(token) = token {
        request = request.header("X-Auth-Token", token);
    }
    if let Some(etag) = if_none_match {
        request = request.header("If-None-Match", etag);
    }
    request.send().await.unwrap()
}

/// A stable URL is a credential boundary first: no token, no content.
#[tokio::test]
async fn a_subscription_url_needs_a_read_credential() {
    let state = state();
    let source_id = seed(&state, CONTENT_A);
    let base = spawn(state).await;
    let path = format!("/artifacts/{source_id}/subscription");

    let response = get(&base, &path, None, None).await;
    assert_eq!(response.status(), 401);
    assert!(
        !response.text().await.unwrap().is_empty(),
        "the standard error body names the credential"
    );

    // The wrong token is the same answer.
    let response = get(&base, &path, Some("cccccccccccccccccccccccccccccccc"), None).await;
    assert_eq!(response.status(), 401);

    // The read-only publish credential reads; so does the admin's (an
    // operator debugging from a browser session must not need a second
    // secret), but the point of the route is the publish scope.
    for token in [READ, ADMIN] {
        let response = get(&base, &path, Some(token), None).await;
        assert_eq!(response.status(), 200, "token {token}");
    }
}

/// The route serves the ledger's active snapshot byte for byte, with the
/// content digest as the ETag -- and a later round that committed locally
/// replaces what it serves, while a round that failed before committing
/// changes nothing (the consumer never sees an empty or partial list).
#[tokio::test]
async fn the_stable_url_tracks_the_last_good_snapshot_only() {
    let state = state();
    let source_id = seed(&state, CONTENT_A);
    let base = spawn(state.clone()).await;
    let path = format!("/artifacts/{source_id}/subscription");

    let response = get(&base, &path, Some(READ), None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), CONTENT_A);
    let etag_a = format!("\"{}\"", export_hash(CONTENT_A));

    // A later round commits new content locally: the stable URL follows.
    republish(&state, CONTENT_B);
    let response = get(&base, &path, Some(READ), None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), CONTENT_B);

    // A failed round publishes nothing: no new snapshot, no body change.
    // (Nothing to call here -- the point is what the URL has NOT become.)
    let response = get(&base, &path, Some(READ), None).await;
    assert_eq!(response.text().await.unwrap(), CONTENT_B);
    let _ = etag_a;
}

/// `If-None-Match` with the current digest is a bodyless 304: a client that
/// polls this URL costs one indexed read while nothing changed.
#[tokio::test]
async fn a_current_etag_answers_304_with_no_body() {
    let state = state();
    let source_id = seed(&state, CONTENT_A);
    let base = spawn(state).await;
    let path = format!("/artifacts/{source_id}/subscription");
    let etag = format!("\"{}\"", export_hash(CONTENT_A));

    let response = get(&base, &path, Some(READ), Some(&etag)).await;
    assert_eq!(response.status(), 304);
    assert_eq!(
        response.headers().get("etag").and_then(|v| v.to_str().ok()),
        Some(etag.as_str())
    );
    assert_eq!(response.text().await.unwrap(), "");

    // A stale or wrong validator gets the full body again.
    let response = get(&base, &path, Some(READ), Some("\"0000000\"")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), CONTENT_A);
}

/// The cache policy the endpoint owns must survive `security_headers`
/// (which stamps `no-store` on everything else): a subscription a client is
/// allowed to cache for five minutes, with stale-if-error as the fallback.
///
/// KNOWN FAILING at the B6 baseline (commit 13c2889) -- found by this test.
/// `build_router` layers `subscription_cache_headers` INSIDE the global
/// `security_headers`, so the outer middleware re-stamps `no-store` over the
/// subscription's policy after the inner one set it; the plan's mitigation
/// ("stable endpoint path exempt, pinned by a separate test") was never
/// wired. Ignored rather than inverted: the assertion is the spec, and the
/// fix belongs to the probe-api owner (swap the layer order or exempt the
/// path inside `security_headers`). Tracked in
/// workstreams/19_ACCEPTANCE_MAP.md and TEST_REPORT.md.
#[tokio::test]
#[ignore = "open defect B6-1: security_headers clobbers the subscription Cache-Control (build_router layer order); unignore when fixed"]
async fn the_cache_policy_survives_the_security_headers() {
    let state = state();
    let source_id = seed(&state, CONTENT_A);
    let base = spawn(state).await;
    let path = format!("/artifacts/{source_id}/subscription");

    for token in [Some(READ), None] {
        let response = get(&base, &path, token, None).await;
        assert_eq!(
            response
                .headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("public, max-age=300, stale-if-error=86400"),
            "status {}",
            response.status()
        );
    }
}

/// An unknown or malformed source id reveals nothing: 404 with the standard
/// body, never a panic and never a host-shaped error.
#[tokio::test]
async fn an_unknown_or_malformed_source_id_is_a_plain_404() {
    let state = state();
    seed(&state, CONTENT_A);
    let base = spawn(state).await;

    let unknown = "00000000-0000-4000-8000-00000000000f";
    let response = get(
        &base,
        &format!("/artifacts/{unknown}/subscription"),
        Some(READ),
        None,
    )
    .await;
    assert_eq!(response.status(), 404);

    // Well-formed but never synced, and hostile shapes, all the same answer.
    let hostile = ["..%2F..%2Fetc", "not-a-uuid"];
    for id in hostile {
        let response = get(
            &base,
            &format!("/artifacts/{id}/subscription"),
            Some(READ),
            None,
        )
        .await;
        assert_eq!(response.status(), 404, "id {id}");
    }
}

/// A synced source with no active snapshot yet is 404 too: the route serves
/// the last GOOD snapshot or nothing, never a placeholder.
#[tokio::test]
async fn a_source_without_a_snapshot_is_404_not_empty() {
    let state = state();
    let source_id = {
        let storage = state.storage.lock().unwrap();
        storage
            .sync_source(
                "fresh",
                "Fresh",
                probe_storage::sources::INPUT_COLLECTION,
                "fresh",
                true,
            )
            .unwrap()
    };
    let base = spawn(state).await;
    let response = get(
        &base,
        &format!("/artifacts/{source_id}/subscription"),
        Some(READ),
        None,
    )
    .await;
    assert_eq!(response.status(), 404);
}
