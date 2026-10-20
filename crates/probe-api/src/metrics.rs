//! Prometheus text exposition (G16).
//!
//! Deliberately derived from the ledger at scrape time: the numbers a scrape
//! reports are the same numbers `/api/v1/status` shows, and there is no second
//! counter to keep in sync (or to lose on restart).
//!
//! Labels stay a bounded set: node status (at most five values) and nothing
//! else. Keys, fingerprints, hostnames, ports and credentials never become
//! labels, because a scrape endpoint is the easiest place to leak the fleet
//! inventory -- and high-cardinality labels are how a metrics store falls over.

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use probe_storage::{MetricsSnapshot, RoundRow};

use crate::{exports, SharedState};

/// The Prometheus text format this endpoint speaks.
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

pub(crate) fn routes() -> Router<SharedState> {
    Router::new().route("/metrics", get(metrics))
}

/// `GET /metrics` -- one scrape, rendered from the ledger.
async fn metrics(State(state): State<SharedState>) -> Response {
    let (snapshot, last) = {
        let storage = match state.storage.lock() {
            Ok(storage) => storage,
            Err(_) => return storage_error("ledger lock poisoned"),
        };
        // The last-round gauges describe the newest CLOSED round: an in-flight
        // row carries NULL counts, and reading it here would report "all nodes
        // dead" on every scrape for the duration of every round.
        match (storage.metrics_snapshot(), storage.last_finished_round()) {
            (Ok(snapshot), Ok(last)) => (snapshot, last),
            (Err(err), _) | (_, Err(err)) => return storage_error(&err.to_string()),
        }
    };
    let body = render(
        &snapshot,
        last.as_ref(),
        state.schedule.enabled,
        state.schedule.interval_minutes,
    );
    let mut response = body.into_response();
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn storage_error(message: &str) -> Response {
    exports::error_response(StatusCode::INTERNAL_SERVER_ERROR, "storage", message)
}

/// Render the exposition text. Pure so the shape is unit-testable.
fn render(
    snapshot: &MetricsSnapshot,
    last: Option<&RoundRow>,
    schedule_enabled: bool,
    interval_minutes: i64,
) -> String {
    let mut out = String::new();
    counter(
        &mut out,
        "probe_rounds_finished_total",
        "Rounds closed with a verdict, all time.",
        snapshot.rounds_finished,
    );
    counter(
        &mut out,
        "probe_rounds_suspect_total",
        "Finished rounds the alive-floor guard flagged.",
        snapshot.rounds_suspect,
    );
    counter(
        &mut out,
        "probe_rounds_inconclusive_total",
        "Finished rounds marked inconclusive (nothing measured or committed).",
        snapshot.rounds_inconclusive,
    );
    counter(
        &mut out,
        "probe_rounds_with_duration_total",
        "Finished rounds that recorded a wall-clock duration.",
        snapshot.rounds_with_duration,
    );
    out.push_str("# HELP probe_nodes Nodes by ledger status.\n");
    out.push_str("# TYPE probe_nodes gauge\n");
    for (status, rows) in &snapshot.nodes_by_status {
        out.push_str(&format!(
            "probe_nodes{{status=\"{}\"}} {}\n",
            escape_label(status),
            rows
        ));
    }
    gauge(
        &mut out,
        "probe_exports_active",
        "Export keys with an active snapshot.",
        snapshot.active_exports as f64,
    );
    gauge(
        &mut out,
        "probe_scheduler_enabled",
        "1 when the in-process round timer is enabled.",
        if schedule_enabled { 1.0 } else { 0.0 },
    );
    gauge(
        &mut out,
        "probe_scheduler_interval_minutes",
        "Configured minutes between scheduled rounds.",
        interval_minutes as f64,
    );
    if let Some(last) = last {
        gauge(
            &mut out,
            "probe_round_last_total",
            "Nodes measured in the newest round.",
            last.total as f64,
        );
        gauge(
            &mut out,
            "probe_round_last_ok",
            "Nodes that passed in the newest round.",
            last.ok as f64,
        );
        gauge(
            &mut out,
            "probe_round_last_failed",
            "Nodes that failed in the newest round.",
            last.failed as f64,
        );
        if let Some(duration) = last.duration_s {
            gauge(
                &mut out,
                "probe_round_last_duration_seconds",
                "Wall-clock seconds of the newest round.",
                duration,
            );
        }
    }
    out
}

fn counter(out: &mut String, name: &str, help: &str, value: i64) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
    out.push_str(&format!("{name} {value}\n"));
}

fn gauge(out: &mut String, name: &str, help: &str, value: f64) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
    out.push_str(&format!("{name} {value}\n"));
}

/// Escape a label value per the text exposition format: backslash, quote and
/// newline. Statuses come from the ledger, which a Python-era row may have
/// written, so this is a correctness rule rather than a formality.
fn escape_label(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped
}
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use probe_config::PublishSection;
    use probe_engine::RoundSettings;
    use probe_mihomo::Controller;
    use probe_storage::{MetricsSnapshot, RoundRow, Storage};

    use super::*;
    use crate::{AppState, ServeConfig, SharedState};

    const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn snapshot() -> MetricsSnapshot {
        MetricsSnapshot {
            rounds_finished: 27,
            rounds_suspect: 1,
            rounds_inconclusive: 20,
            rounds_with_duration: 4,
            nodes_by_status: vec![("alive".into(), 63), ("dead".into(), 242)],
            active_exports: 4,
        }
    }

    fn round() -> RoundRow {
        RoundRow {
            round_id: 27,
            started_at: "2026-10-20T10:05:29".into(),
            finished_at: Some("2026-10-20T10:07:32".into()),
            trigger: Some("schedule".into()),
            mode: None,
            note: None,
            total: 141,
            ok: 106,
            failed: 33,
            dropped: 0,
            restored: 0,
            suspect: false,
            inconclusive: true,
            duration_s: Some(111.6),
        }
    }

    #[test]
    fn the_scrape_carries_every_documented_family() {
        let body = render(&snapshot(), Some(&round()), true, 60);
        for needle in [
            "# TYPE probe_rounds_finished_total counter",
            "probe_rounds_finished_total 27",
            "probe_rounds_suspect_total 1",
            "probe_rounds_inconclusive_total 20",
            "probe_rounds_with_duration_total 4",
            "probe_nodes{status=\"alive\"} 63",
            "probe_nodes{status=\"dead\"} 242",
            "probe_exports_active 4",
            "probe_scheduler_enabled 1",
            "probe_scheduler_interval_minutes 60",
            "probe_round_last_total 141",
            "probe_round_last_ok 106",
            "probe_round_last_failed 33",
            "probe_round_last_duration_seconds 111.6",
        ] {
            assert!(body.contains(needle), "missing {needle} in:\n{body}");
        }
    }

    #[test]
    fn a_disabled_schedule_and_an_empty_ledger_still_scrape() {
        let body = render(&MetricsSnapshot::default(), None, false, 30);
        assert!(body.contains("probe_scheduler_enabled 0"));
        assert!(body.contains("probe_rounds_finished_total 0"));
        assert!(
            !body.contains("probe_round_last_total"),
            "no round means no last-round gauge"
        );
        assert!(!body.contains("probe_round_last_duration_seconds"));
    }

    /// Statuses come from the ledger; a quote or newline must never break the
    /// exposition (or inject a metric line).
    #[test]
    fn a_hostile_status_is_escaped_not_injected() {
        let mut hostile = snapshot();
        hostile.nodes_by_status = vec![("alive\"} 1\nprobe_injected 1".into(), 3)];
        let body = render(&hostile, None, true, 60);
        assert!(
            body.contains("probe_nodes{status=\"alive\\\"} 1\\nprobe_injected 1\"} 3"),
            "the label must be escaped, got:\n{body}"
        );
        assert!(
            !body.lines().any(|line| line.starts_with("probe_injected")),
            "a label must never become its own metric line"
        );
    }

    fn test_state() -> SharedState {
        let cfg = probe_config::Config::load(std::path::Path::new("/nonexistent/config.json"))
            .expect("default config");
        Arc::new(
            AppState::new(
                Storage::open_in_memory().unwrap(),
                Some(ADMIN.into()),
                Controller::new("http://127.0.0.1:1", None),
                ServeConfig {
                    kernel_config_path: "/test/config.yaml".into(),
                    round: RoundSettings::from_config(&cfg),
                    core: cfg.core,
                    sources: cfg.sources,
                    backend: "http://127.0.0.1:1".into(),
                    root: std::path::PathBuf::from("/test/root"),
                    substore: None,
                    chain: cfg.chain,
                    publish_prefix: cfg.publish_prefix,
                    publish: cfg.publish,
                    schedule: cfg.schedule,
                    policy: cfg.policy,
                },
            )
            .with_publish(PublishSection::default()),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metrics_requires_the_admin_token() {
        let state = test_state();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::build_router(state);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();

        let anonymous = client.get(format!("{base}/metrics")).send().await.unwrap();
        assert_eq!(anonymous.status(), 401);

        let authorized = client
            .get(format!("{base}/metrics"))
            .header("X-Auth-Token", ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(authorized.status(), 200);
        assert_eq!(
            authorized.headers()["content-type"],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        assert_eq!(authorized.headers()["cache-control"], "no-store");
        let body = authorized.text().await.unwrap();
        assert!(body.contains("probe_rounds_finished_total 0"));
        assert!(body.contains("probe_scheduler_interval_minutes"));
    }
}
