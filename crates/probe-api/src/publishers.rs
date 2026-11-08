//! The admin publish diagnostics surface (workstreams/18 B2, workstreams/26).
//!
//! Two endpoints so far, one per remote destination: `POST
//! /api/v1/publishers/gist/test` and `POST /api/v1/publishers/webdav/test`,
//! the GUI's 「测试连接」 buttons. Each constructs the publisher (the
//! credential file is read there, and ONLY there) and runs one healthcheck
//! -- nothing is published, nothing is created.
//!
//! The response is a mask, not a report: `configured`, `ok` and, on failure,
//! the frozen `error_code`. Never the password, never a gist id, never a
//! message body (a message can carry a filesystem path; the code cannot
//! carry anything but its frozen spelling). An unconfigured deployment
//! answers `configured: false` with `error_code: "config"` -- the exact
//! condition the SPA renders as 「未启用」.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use probe_config::{GistSection, WebdavSection};
use probe_publish::{GistPublisher, PublishErrorCode, Publisher, WebDavPublisher};
use serde_json::{json, Value};

use crate::SharedState;

/// The admin routes this module adds to the guarded group.
pub(crate) fn admin_routes() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/publishers/gist/test", post(gist_test))
        .route("/api/v1/publishers/webdav/test", post(webdav_test))
}

/// `POST /api/v1/publishers/gist/test` -- probe the configured Gist.
async fn gist_test(State(state): State<SharedState>) -> Json<Value> {
    Json(gist_test_result(&state.publish.gist, probe_publish::GITHUB_API_BASE).await)
}

/// `POST /api/v1/publishers/webdav/test` -- probe the configured WebDAV
/// collection (workstreams/26).
async fn webdav_test(State(state): State<SharedState>) -> Json<Value> {
    Json(webdav_test_result(&state.publish.webdav).await)
}

/// The masked probe result for one WebDAV section. `base_url` is injectable
/// (the `None` here means "use the configured URL") so the tests can aim the
/// publisher at a local stub instead of a real server.
async fn webdav_test_result(section: &WebdavSection) -> Value {
    let publisher = match WebDavPublisher::from_config(section) {
        Ok(publisher) => publisher,
        // Construction failure = unconfigured (no URL, no readable password
        // file): the reason is always the config code, because construction
        // only fails on config.
        Err(_) => return json!({"configured": false, "ok": false, "error_code": "config"}),
    };
    match publisher.healthcheck("").await {
        Ok(()) => json!({"configured": true, "ok": true}),
        Err(error) => json!({
            // A config-shaped failure means the destination cannot even be
            // addressed; everything else is a live destination answering
            // badly.
            "configured": !matches!(error.code, PublishErrorCode::Config),
            "ok": false,
            "error_code": error.code.as_str(),
        }),
    }
}

/// The masked probe result for one gist section. `api_base` is injectable so
/// the tests aim it at a local stub instead of GitHub; production passes
/// [`probe_publish::GITHUB_API_BASE`].
async fn gist_test_result(section: &GistSection, api_base: &str) -> Value {
    let publisher = match GistPublisher::with_api_base(section, api_base) {
        Ok(publisher) => publisher,
        // Construction failure = unconfigured (no token file): the reason is
        // always the config code, because construction only fails on config.
        Err(_) => return json!({"configured": false, "ok": false, "error_code": "config"}),
    };
    // An empty probe id makes the publisher answer with the config code too
    // (nothing to probe), which reads as "half configured".
    let gist_id = section.gist_id.as_deref().unwrap_or("");
    match publisher.healthcheck(gist_id).await {
        Ok(()) => json!({"configured": true, "ok": true}),
        Err(error) => json!({
            // A config-shaped failure means the destination cannot even be
            // addressed (no gist id); everything else is a live destination
            // answering badly.
            "configured": !matches!(error.code, PublishErrorCode::Config),
            "ok": false,
            "error_code": error.code.as_str(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use axum::http::StatusCode;
    use probe_config::SourceSpec;
    use probe_engine::RoundSettings;
    use probe_mihomo::Controller;
    use probe_storage::Storage;
    use serde_json::Value;

    use super::*;
    use crate::{build_router, AppState, ServeConfig};

    const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// The same loopback state the GUI tests use: a default config (whose
    /// publish.gist carries no token file), no sources, no reachable ports.
    fn state(root: &Path) -> Arc<AppState> {
        let cfg = probe_config::Config::load(Path::new("/nonexistent/config.json"))
            .expect("default config");
        Arc::new(AppState::new(
            Storage::open_in_memory().unwrap(),
            Some(ADMIN.into()),
            Controller::new("http://127.0.0.1:1", None),
            ServeConfig {
                kernel_config_path: "/test/config.yaml".into(),
                round: RoundSettings::from_config(&cfg),
                core: cfg.core.clone(),
                sources: Vec::<SourceSpec>::new(),
                backend: "http://127.0.0.1:1".into(),
                root: root.to_path_buf(),
                substore: None,
                chain: cfg.chain.clone(),
                publish_prefix: cfg.publish_prefix.clone(),
                publish: cfg.publish.clone(),
                schedule: cfg.schedule.clone(),
                policy: cfg.policy.clone(),
                min_run_interval_seconds: 0,
                substore_allow_delete: false,
                probes: Vec::new(),
                substore_target_name: probe_config::DEFAULT_SUBSTORE_TARGET_NAME.into(),
                aggregation: probe_config::AggregationSection::default(),
                probe_config_digest: String::new(),
                alert: probe_config::AlertSection::default(),
            },
        ))
    }

    async fn spawn(state: Arc<AppState>) -> String {
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    /// The endpoint lives in the guarded group: without the admin token it is
    /// the same 401 every guarded endpoint answers with.
    #[tokio::test]
    async fn gist_test_requires_the_admin_token() {
        let tmp = tempfile::tempdir().unwrap();
        let base = spawn(state(tmp.path())).await;
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/api/v1/publishers/gist/test"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// Admin token, unconfigured deployment: `configured=false` with the
    /// config reason code -- the response the SPA degrades on -- and no
    /// credential material anywhere in the body.
    #[tokio::test]
    async fn an_unconfigured_deployment_answers_configured_false() {
        let tmp = tempfile::tempdir().unwrap();
        let base = spawn(state(tmp.path())).await;
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/api/v1/publishers/gist/test"))
            .header("X-Auth-Token", ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["configured"], false);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error_code"], "config");
        let text = body.to_string();
        assert!(!text.contains(ADMIN), "no admin token echo: {text}");
    }

    /// A token file that does not exist is the same mask, at the unit level.
    #[tokio::test]
    async fn a_missing_token_file_is_configured_false() {
        let section = GistSection {
            token_file: Some(std::env::temp_dir().join("definitely-not-here-90210.token")),
            ..GistSection::default()
        };
        let body = gist_test_result(&section, "http://127.0.0.1:1").await;
        assert_eq!(body["configured"], false);
        assert_eq!(body["error_code"], "config");
    }

    /// workstreams/26: the WebDAV probe shares the mask contract. An
    /// unconfigured deployment answers `configured=false, error_code=config`
    /// and no credential material ever appears in the body.
    #[tokio::test]
    async fn webdav_test_shares_the_mask_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let base = spawn(state(tmp.path())).await;
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/api/v1/publishers/webdav/test"))
            .header("X-Auth-Token", ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["configured"], false);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error_code"], "config");
        let text = body.to_string();
        assert!(!text.contains(ADMIN), "no admin token echo: {text}");
    }

    /// workstreams/26: a configured, healthy WebDAV stub answers
    /// `configured=true, ok=true`; a 404 answers the frozen `webdav_missing`
    /// code with `configured=true`. No request ever leaves 127.0.0.1.
    #[tokio::test]
    async fn the_webdav_probe_maps_destination_answers() {
        for status in [200u16, 404] {
            let app = axum::Router::new()
                .fallback(move |method: axum::http::Method| async move {
                    let ok = method == axum::http::Method::from_bytes(b"PROPFIND").unwrap();
                    if ok {
                        (StatusCode::from_u16(status).unwrap(), String::new())
                    } else {
                        (StatusCode::METHOD_NOT_ALLOWED, String::new())
                    }
                })
                .with_state(());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

            let section = WebdavSection {
                url: Some(base),
                username: "probe".into(),
                password_file: None,
                directory: "mihomo-probe".into(),
                timeout_seconds: 5,
                max_retries: 1,
            };
            let body = webdav_test_result(&section).await;
            if status == 200 {
                assert_eq!(body["configured"], true);
                assert_eq!(body["ok"], true, "{body}");
            } else {
                assert_eq!(body["configured"], true);
                assert_eq!(body["ok"], false);
                assert_eq!(body["error_code"], "webdav_missing", "{body}");
            }
        }
    }

    /// A local stub plays api.github.com: a healthy destination answers
    /// `configured=true, ok=true`; a 404 answers the frozen `gist_missing`
    /// code with `configured=true` (the destination IS configured, it just
    /// answered badly). No request ever leaves 127.0.0.1.
    #[tokio::test]
    async fn the_probe_maps_destination_answers() {
        for status in [200u16, 404] {
            let app = axum::Router::new()
                .route(
                    "/gists/{id}",
                    axum::routing::get(move || async move {
                        (StatusCode::from_u16(status).unwrap(), "{}")
                    }),
                )
                .with_state(());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

            let dir = tempfile::tempdir().unwrap();
            let token_file = dir.path().join("gist.token");
            std::fs::write(&token_file, "test-token-not-real").unwrap();
            let section = GistSection {
                gist_id: Some("gist0001abcd".into()),
                token_file: Some(token_file),
                ..GistSection::default()
            };
            let body = gist_test_result(&section, &base).await;
            if status == 200 {
                assert_eq!(body["configured"], true);
                assert_eq!(body["ok"], true, "{body}");
            } else {
                assert_eq!(body["configured"], true);
                assert_eq!(body["ok"], false);
                assert_eq!(body["error_code"], "gist_missing", "{body}");
            }
            let text = body.to_string();
            assert!(!text.contains("test-token-not-real"));
        }
    }
}
