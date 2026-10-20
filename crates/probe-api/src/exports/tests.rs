//! HTTP-level tests for the publication read surface.
//!
//! Every test drives the real router over a loopback listener: the contract
//! under test (status codes, headers, exact bytes, credential separation) is
//! the wire contract Sub-Store and the operator actually consume.

use std::sync::Arc;

use axum::http::{header, StatusCode};
use probe_config::PublishSection;
use probe_engine::RoundSettings;
use probe_mihomo::Controller;
use probe_storage::publication::{export_hash, PublicationInput};
use probe_storage::Storage;
use serde_json::{json, Value};

use super::*;
use crate::ServeConfig;

const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PUBLISH: &str = "pppppppppppppppppppppppppppppppp";
/// Payload text that must never appear in a metadata reply.
const SECRET_MARKER: &str = "export-content-must-not-leak";

fn state_with(publish: PublishSection) -> SharedState {
    let cfg = probe_config::Config::load(std::path::Path::new("/nonexistent/config.json"))
        .expect("default config");
    let state = AppState::new(
        Storage::open_in_memory().unwrap(),
        Some(ADMIN.into()),
        Controller::new("http://127.0.0.1:1", None),
        ServeConfig {
            kernel_config_path: "/test/config.yaml".into(),
            round: RoundSettings::from_config(&cfg),
            core: cfg.core,
            sources: cfg.sources,
            backend: "http://127.0.0.1:1".into(),
            // No test here starts a round; the root is only used by that path.
            root: std::path::PathBuf::from("/test/root"),
            substore: None,
            chain: cfg.chain,
            publish_prefix: cfg.publish_prefix,
            // `with_publish` below replaces this; the constructor still needs
            // a value, and the deny-by-default one is the safe placeholder.
            publish: PublishSection::default(),
            schedule: probe_config::ScheduleSection::default(),
            policy: cfg.policy,
        },
    );
    Arc::new(state.with_publish(publish))
}

/// Same as [`state_with`], but with a real writable root: the rollback path
/// routes through the engine's materialize-then-CAS, which writes the
/// snapshot's immutable file under `<root>/exports` before activating it.
/// The caller owns the tempdir and must keep it alive for the test.
fn writable_root_state(publish: PublishSection) -> (SharedState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = probe_config::Config::load(std::path::Path::new("/nonexistent/config.json"))
        .expect("default config");
    let state = AppState::new(
        Storage::open_in_memory().unwrap(),
        Some(ADMIN.into()),
        Controller::new("http://127.0.0.1:1", None),
        ServeConfig {
            kernel_config_path: "/test/config.yaml".into(),
            round: RoundSettings::from_config(&cfg),
            core: cfg.core,
            sources: cfg.sources,
            backend: "http://127.0.0.1:1".into(),
            root: tmp.path().to_path_buf(),
            substore: None,
            chain: cfg.chain,
            publish_prefix: cfg.publish_prefix,
            publish: PublishSection::default(),
            schedule: probe_config::ScheduleSection::default(),
            policy: cfg.policy,
        },
    );
    (Arc::new(state.with_publish(publish)), tmp)
}

fn publish_token_state() -> SharedState {
    state_with(PublishSection {
        token: Some(PUBLISH.into()),
        ..PublishSection::default()
    })
}

/// Synthetic export content: a JSON document carrying a secret marker.
fn content(name: &str) -> String {
    json!({"proxies": [{
        "name": name,
        "type": "trojan",
        "server": "origin.invalid",
        "port": 443,
        "password": SECRET_MARKER,
    }]})
    .to_string()
}

/// Publish one snapshot and return its id.
fn publish_key(state: &SharedState, key: &str, body: &str) -> i64 {
    let storage = state.storage.lock().unwrap();
    let expected = storage
        .current_export(key)
        .unwrap()
        .map(|snapshot| snapshot.summary.id);
    storage
        .publish_export(PublicationInput {
            key,
            content: body,
            sha256: &export_hash(body),
            node_count: 1,
            actor: "api-test",
            forced: false,
            expected_active: expected,
        })
        .unwrap()
        .snapshot
        .id
}

async fn spawn(state: SharedState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = crate::build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// The publish credential reads exports and nothing else; the admin token
/// keeps both powers.
#[tokio::test]
async fn publish_token_reads_exports_and_cannot_touch_the_admin_surface() {
    let state = publish_token_state();
    let body = content("exported-node");
    publish_key(&state, "air", &body);
    let base = spawn(state.clone()).await;
    let client = reqwest::Client::new();

    // ?token= and both header styles are accepted, byte for byte.
    for request in [
        client
            .get(format!("{base}/api/export/air.yaml"))
            .query(&[("token", PUBLISH)]),
        client
            .get(format!("{base}/api/export/air.yaml"))
            .header("X-Auth-Token", PUBLISH),
        client
            .get(format!("{base}/api/export/air.yaml"))
            .header("Authorization", format!("Bearer {PUBLISH}")),
        // A blank query token must not shadow a valid header credential.
        client
            .get(format!("{base}/api/export/air.yaml"))
            .query(&[("token", "")])
            .header("X-Auth-Token", PUBLISH),
    ] {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/yaml");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.text().await.unwrap(), body);
    }

    // The admin surface is off limits to the publish credential.
    let status = client
        .get(format!("{base}/api/v1/status"))
        .header("X-Auth-Token", PUBLISH)
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::UNAUTHORIZED);
    let round = client
        .post(format!("{base}/api/v1/rounds"))
        .header("X-Auth-Token", PUBLISH)
        .send()
        .await
        .unwrap();
    assert_eq!(round.status(), StatusCode::UNAUTHORIZED);
    assert!(
        state
            .storage
            .lock()
            .unwrap()
            .last_round()
            .unwrap()
            .is_none(),
        "a publish token must not open a round row"
    );
    for path in ["/api/v1/exports", "/api/v1/exports/air/history"] {
        let denied = client
            .get(format!("{base}{path}"))
            .header("X-Auth-Token", PUBLISH)
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    let rollback = client
        .post(format!("{base}/api/v1/exports/air/rollback"))
        .header("X-Auth-Token", PUBLISH)
        .json(&json!({"target_id": 1, "expected_active": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(rollback.status(), StatusCode::UNAUTHORIZED);

    // The admin token may read both surfaces.
    assert_eq!(
        client
            .get(format!("{base}/api/v1/status"))
            .header("X-Auth-Token", ADMIN)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let admin_download = client
        .get(format!("{base}/api/export/air.yaml"))
        .query(&[("token", ADMIN)])
        .send()
        .await
        .unwrap();
    assert_eq!(admin_download.status(), StatusCode::OK);
    assert_eq!(admin_download.text().await.unwrap(), body);

    // Credentials gate the route before key lookup: missing/wrong is 401 and
    // an authenticated call for an unpublished key is 404.
    for (presented, expected) in [
        ("wrong", StatusCode::UNAUTHORIZED),
        ("", StatusCode::UNAUTHORIZED),
        (ADMIN, StatusCode::NOT_FOUND),
        (PUBLISH, StatusCode::NOT_FOUND),
    ] {
        let response = client
            .get(format!("{base}/api/export/missing.yaml"))
            .query(&[("token", presented)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{presented:?}");
    }
    let no_token = client
        .get(format!("{base}/api/export/air.yaml"))
        .send()
        .await
        .unwrap();
    assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(no_token.headers()[header::CACHE_CONTROL], "no-store");
}

/// An absent or blank publish token denies downloads (never "auth off").
#[tokio::test]
async fn absent_or_blank_publish_token_denies_downloads() {
    let body = content("exported-node");
    for publish in [
        PublishSection::default(),
        PublishSection {
            token: Some("   ".into()),
            ..PublishSection::default()
        },
    ] {
        let state = state_with(publish);
        publish_key(&state, "air", &body);
        let base = spawn(state).await;
        let client = reqwest::Client::new();
        for presented in ["", "   ", PUBLISH, "anything"] {
            let response = client
                .get(format!("{base}/api/export/air.yaml"))
                .query(&[("token", presented)])
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "token {presented:?} must be denied when publish.token is absent"
            );
        }
        // The admin credential is independent of the publish switch.
        let admin = client
            .get(format!("{base}/api/export/air.yaml"))
            .query(&[("token", ADMIN)])
            .send()
            .await
            .unwrap();
        assert_eq!(admin.status(), StatusCode::OK);
    }
}

/// Invalid, unpublished, and unknown keys are all a plain 404.
#[tokio::test]
async fn export_download_404s_for_unknown_unsafe_or_unpublished_keys() {
    let state = publish_token_state();
    publish_key(&state, "air", &content("exported-node"));
    let base = spawn(state).await;
    let client = reqwest::Client::new();
    let get = |path: &str| {
        client
            .get(format!("{base}{path}"))
            .query(&[("token", PUBLISH)])
    };
    for path in [
        "/api/export/missing.yaml", // valid shape, never published
        "/api/export/.hidden.yaml", // rejected by the storage key policy
        "/api/export/air",          // missing the .yaml download suffix
        "/api/export/air.yaml.tmp", // same
    ] {
        let response = get(path).send().await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{path} must be a plain 404"
        );
    }
    assert_eq!(
        get("/api/export/air.yaml").send().await.unwrap().status(),
        StatusCode::OK
    );
}

/// The list is paged metadata only: no content, no proxy names.
#[tokio::test]
async fn exports_list_is_paged_metadata_only() {
    let state = publish_token_state();
    publish_key(&state, "air", &content("air-node"));
    publish_key(&state, "mobile", &content("mobile-node"));
    let base = spawn(state.clone()).await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/api/v1/exports"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(
        !text.contains(SECRET_MARKER) && !text.contains("air-node") && !text.contains("proxies"),
        "the list must carry metadata only: {text}"
    );
    let body: Value = serde_json::from_str(&text).unwrap();
    let exports = body["exports"].as_array().unwrap();
    assert_eq!(exports.len(), 2);
    assert_eq!(exports[0]["key"].as_str(), Some("air"));
    assert_eq!(exports[1]["key"].as_str(), Some("mobile"));
    for export in exports {
        assert!(export.get("content").is_none());
        assert_eq!(export["is_active"].as_bool(), Some(true));
        assert_eq!(export["node_count"].as_i64(), Some(1));
        assert_eq!(export["sha256"].as_str().unwrap().len(), 64);
    }

    // Cursor paging: one row per page, cursor is the last returned key.
    let first: Value = client
        .get(format!("{base}/api/v1/exports"))
        .query(&[("limit", "1")])
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["exports"].as_array().unwrap().len(), 1);
    assert_eq!(first["exports"][0]["key"].as_str(), Some("air"));
    assert_eq!(first["next_after_key"].as_str(), Some("air"));
    let second: Value = client
        .get(format!("{base}/api/v1/exports"))
        .query(&[("limit", "1"), ("after_key", "air")])
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["exports"][0]["key"].as_str(), Some("mobile"));
    let empty: Value = client
        .get(format!("{base}/api/v1/exports"))
        .query(&[("after_key", "mobile")])
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty["exports"].as_array().unwrap().is_empty());

    // Bad page parameters are client errors, not silent full dumps.
    for query in [
        vec![("limit", "0")],
        vec![("limit", "101")],
        vec![("limit", "abc")],
        vec![("after_key", ".bad")],
    ] {
        let response = client
            .get(format!("{base}/api/v1/exports"))
            .query(&query)
            .header("X-Auth-Token", ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query:?}");
    }
}

/// History is paged metadata only, newest first, with a working cursor.
#[tokio::test]
async fn export_history_is_paged_metadata_only() {
    let state = publish_token_state();
    let first_body = content("first-node");
    let first_id = publish_key(&state, "air", &first_body);
    let second_body = content("second-node");
    let second_id = publish_key(&state, "air", &second_body);
    assert!(second_id > first_id);
    let base = spawn(state).await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/api/v1/exports/air/history"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(
        !text.contains(SECRET_MARKER) && !text.contains("proxies"),
        "history must carry metadata only: {text}"
    );
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["key"].as_str(), Some("air"));
    let history = body["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["id"].as_i64(), Some(second_id));
    assert_eq!(history[0]["is_active"].as_bool(), Some(true));
    assert_eq!(history[1]["id"].as_i64(), Some(first_id));
    assert_eq!(history[1]["is_active"].as_bool(), Some(false));
    assert!(body["next_before_id"].is_null());

    // Cursor paging walks newest -> oldest without repeating a row.
    let page: Value = client
        .get(format!("{base}/api/v1/exports/air/history"))
        .query(&[("limit", "1")])
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["history"][0]["id"].as_i64(), Some(second_id));
    assert_eq!(page["next_before_id"].as_i64(), Some(second_id));
    let before = second_id.to_string();
    let older: Value = client
        .get(format!("{base}/api/v1/exports/air/history"))
        .query(&[("limit", "1"), ("before_id", before.as_str())])
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(older["history"][0]["id"].as_i64(), Some(first_id));
    assert_eq!(older["history"][0]["is_active"].as_bool(), Some(false));

    // An unpublished valid key is an empty page, not an error.
    let empty: Value = client
        .get(format!("{base}/api/v1/exports/mobile/history"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty["history"].as_array().unwrap().is_empty());

    // Invalid keys and cursors are client errors.
    for query in [vec![("before_id", "0")], vec![("before_id", "abc")]] {
        let response = client
            .get(format!("{base}/api/v1/exports/air/history"))
            .query(&query)
            .header("X-Auth-Token", ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query:?}");
    }
    let invalid_key = client
        .get(format!("{base}/api/v1/exports/.hidden/history"))
        .header("X-Auth-Token", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_key.status(), StatusCode::BAD_REQUEST);
}

/// Rollback is CAS-guarded, returns metadata only, and flips the download.
#[tokio::test]
async fn rollback_is_cas_guarded_and_publish_token_cannot_call_it() {
    // A real root: the engine's rollback path materializes the target's
    // immutable file before the CAS, so a fake path would fail here.
    let (state, _root) = writable_root_state(PublishSection {
        token: Some(PUBLISH.into()),
        ..PublishSection::default()
    });
    let first_body = content("first-node");
    let first_id = publish_key(&state, "air", &first_body);
    let second_body = content("second-node");
    let second_id = publish_key(&state, "air", &second_body);
    let base = spawn(state.clone()).await;
    let client = reqwest::Client::new();
    let rollback = |body: Value, token: &str| {
        client
            .post(format!("{base}/api/v1/exports/air/rollback"))
            .header("X-Auth-Token", token)
            .json(&body)
    };

    // Stale baseline: the active pointer is second_id, not first_id.
    let conflict = rollback(
        json!({"target_id": first_id, "expected_active": first_id}),
        ADMIN,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let conflict_body: Value = conflict.json().await.unwrap();
    assert_eq!(conflict_body["error"]["code"].as_str(), Some("conflict"));
    assert_eq!(conflict_body["active_id"].as_i64(), Some(second_id));

    // Omitting expected_active means "I believe no export is active".
    assert_eq!(
        rollback(json!({"target_id": first_id}), ADMIN)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    // Unknown targets and invalid bodies are distinguishable client errors.
    assert_eq!(
        rollback(
            json!({"target_id": 999_999, "expected_active": second_id}),
            ADMIN
        )
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        rollback(json!({"target_id": 0, "expected_active": second_id}), ADMIN)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let malformed = client
        .post(format!("{base}/api/v1/exports/air/rollback"))
        .header("X-Auth-Token", ADMIN)
        .header(header::CONTENT_TYPE, "application/json")
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    // The publish credential is read-only.
    assert_eq!(
        rollback(
            json!({"target_id": first_id, "expected_active": second_id}),
            PUBLISH
        )
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::UNAUTHORIZED
    );

    // A matching CAS activates the historical snapshot...
    let response = rollback(
        json!({"target_id": first_id, "expected_active": second_id}),
        ADMIN,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(
        !text.contains(SECRET_MARKER) && !text.contains("proxies"),
        "rollback must not carry content: {text}"
    );
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["rolled_back"].as_bool(), Some(true));
    assert_eq!(body["export"]["id"].as_i64(), Some(first_id));
    assert_eq!(body["export"]["is_active"].as_bool(), Some(true));
    assert_eq!(body["previous_active"].as_i64(), Some(second_id));

    // ...and the ledger plus the download agree immediately.
    let active = state
        .storage
        .lock()
        .unwrap()
        .current_export("air")
        .unwrap()
        .unwrap();
    assert_eq!(active.summary.id, first_id);
    assert_eq!(active.content, first_body);
    let download = client
        .get(format!("{base}/api/export/air.yaml"))
        .query(&[("token", PUBLISH)])
        .send()
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(download.text().await.unwrap(), first_body);

    // History keeps both snapshots after the flip, newest first.
    let ids: Vec<i64> = state
        .storage
        .lock()
        .unwrap()
        .export_history("air", 10, None)
        .unwrap()
        .iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(ids, vec![second_id, first_id]);
}
