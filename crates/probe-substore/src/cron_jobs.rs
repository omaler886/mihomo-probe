//! Host-side cron for the embedded Sub-Store: the port of subs-check-pro's
//! `StartSubStoreCronJobs` (loon_server.go).
//!
//! The Loon script-engine bundle has no scheduler — Loon runs scripts on
//! demand — so the *host* owns the two periodic jobs the Node backend would
//! have handled itself:
//!
//! 1. **Gist sync** (`sync_cron`): `GET <api_base>/api/sync/artifacts`, the
//!    bundle's own endpoint, which pushes subscriptions/files to a private
//!    Gist.
//! 2. **Produce** (`produce_cron`): periodically GET each configured
//!    subscription/collection through `/download/...` so the bundle's script
//!    cache is warm and real client downloads (and the probe's collection
//!    fetches) are served without waiting on slow upstreams.
//!
//! Two behaviours are ported verbatim from the Go blueprint because they are
//! load-bearing, not incidental:
//!
//! * **SkipIfStillRunning**: a slow run must not overlap with the next tick,
//!   or jobs would pile up behind the one-request-at-a-time dispatch mutex.
//!   A still-running job's tick is skipped, never queued.
//! * **A redirect-refusing, proxy-less HTTP client**: the backend answers an
//!   unmatched GET with a 302 to the official frontend (upstream behaviour —
//!   fine for browsers), and reqwest would otherwise happily follow it or
//!   route the self-call through a system proxy. Cron calls must stay on
//!   `127.0.0.1` hitting the bundle API only.
//!
//! Jobs are detached tasks that live for the process lifetime, exactly like
//! the server task; they are only ever spawned *after* the listener is up,
//! so a failed boot never leaves a self-calling zombie behind.

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::Local;
use cron::Schedule;

/// One `<cron>,<sub|col>,<names...>` entry of `produce_cron`, expanded to one
/// task per name (the Go loop calls `AddFunc` per name as well).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProduceTask {
    pub cron: String,
    /// `true` for `col`: fetch through `/download/collection/<name>` instead
    /// of `/download/<name>`.
    pub collection: bool,
    pub name: String,
}

/// Parse the `produce_cron` spec: tasks separated by `;`; each task is
/// `<cron>,<sub|col>,<name>[,<name>...]`. The `sub`/`col` marker is located
/// by reverse scan so a cron expression that itself contains commas
/// (`0 0 1,15 * *`) survives the naive comma split — both halves are
/// re-joined around the marker.
///
/// Malformed entries are logged and skipped, matching the Go behaviour of
/// never letting one bad line kill the other jobs.
pub fn parse_produce_tasks(spec: &str) -> Vec<ProduceTask> {
    let mut out = Vec::new();
    for task_str in spec.split(';') {
        let task_str = task_str.trim();
        if task_str.is_empty() {
            continue;
        }
        let parts: Vec<&str> = task_str.split(',').collect();
        if parts.len() < 3 {
            tracing::error!(task = %task_str, "produce cron 配置格式错误(参数不足)");
            continue;
        }
        let mut parsed: Option<(String, bool, String)> = None;
        for i in 1..parts.len() - 1 {
            let marker = parts[i].trim();
            if marker == "sub" || marker == "col" {
                parsed = Some((
                    parts[..i].join(","),
                    marker == "col",
                    parts[i + 1..].join(","),
                ));
                break;
            }
        }
        let Some((cron, collection, names)) = parsed else {
            tracing::error!(task = %task_str, "produce cron 配置格式错误(未找到 sub 或 col 类型标识)");
            continue;
        };
        for name in names.split(',') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            out.push(ProduceTask {
                cron: cron.trim().to_string(),
                collection,
                name: name.to_string(),
            });
        }
    }
    out
}

/// Spawn every configured job. `listen` is the server's bind address; the
/// self-call always targets the loopback form of it (a `0.0.0.0`/`::` bind
/// still answers on `127.0.0.1`). Returns whether anything was spawned (the
/// caller only logs the startup line then).
pub fn spawn_jobs(
    listen: &str,
    backend_path: &str,
    sync_cron: Option<&str>,
    produce_cron: Option<&str>,
) -> bool {
    let api_base = format!("http://{}{backend_path}", loopback_host(listen));
    let mut spawned = false;
    if let Some(expr) = sync_cron.map(str::trim).filter(|s| !s.is_empty()) {
        match parse_schedule(expr) {
            Ok(schedule) => {
                spawn_job(
                    schedule,
                    "Gist同步".to_string(),
                    format!("{api_base}/api/sync/artifacts"),
                );
                spawned = true;
            }
            Err(err) => tracing::error!(expr, %err, "sync cron 表达式解析失败"),
        }
    }
    if let Some(spec) = produce_cron.map(str::trim).filter(|s| !s.is_empty()) {
        for task in parse_produce_tasks(spec) {
            let target = if task.collection {
                format!("{api_base}/download/collection/{}", path_escape(&task.name))
            } else {
                format!("{api_base}/download/{}", path_escape(&task.name))
            };
            match parse_schedule(&task.cron) {
                Ok(schedule) => {
                    spawn_job(schedule, format!("缓存订阅「{}」", task.name), target);
                    spawned = true;
                }
                Err(err) => {
                    tracing::error!(expr = %task.cron, name = %task.name, %err, "produce cron 表达式解析失败")
                }
            }
        }
    }
    if spawned {
        tracing::info!(?sync_cron, ?produce_cron, "sub-store cron jobs started");
    }
    spawned
}

/// Parse one cron expression with the Go blueprint's semantics. The `cron`
/// crate's longhand is 6-7 fields with **seconds first**; robfig/cron v3's
/// default parser — what the config expressions are written for — is 5 fields
/// (min hour dom month dow). A 5-field expression therefore gets a `0`
/// seconds field prepended; 6-7 field expressions (or `@` shorthands, which
/// the crate handles itself) pass through unchanged.
fn parse_schedule(expr: &str) -> Result<Schedule, cron::error::Error> {
    let field_count = expr.split_whitespace().count();
    if field_count == 5 {
        Schedule::from_str(&format!("0 {expr}"))
    } else {
        Schedule::from_str(expr)
    }
}

/// One schedule entry: sleep until the next fire, run unless a previous run
/// is still going, repeat. `SkipIfStillRunning` in robfig/cron terms.
fn spawn_job(schedule: Schedule, task: String, target: String) {
    tokio::spawn(async move {
        let running = Arc::new(AtomicBool::new(false));
        let wakeups = schedule.after_owned(Local::now());
        for next in wakeups {
            let now = Local::now();
            let wait = next
                .signed_duration_since(now)
                .to_std()
                .unwrap_or(Duration::ZERO);
            tokio::time::sleep(wait).await;
            if running.swap(true, Ordering::AcqRel) {
                tracing::warn!(task = %task, "cron job still running; skipped this tick");
                continue;
            }
            call_local_sub_store(&task, &target).await;
            running.store(false, Ordering::Release);
        }
    });
}

fn cron_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // Unmatched GETs bounce to the official frontend; a cron self-call
            // must never follow that bounce out to the internet.
            .redirect(reqwest::redirect::Policy::none())
            // The target is always 127.0.0.1; HTTP(S)_PROXY env vars must not
            // hijack it.
            .no_proxy()
            .timeout(Duration::from_secs(10 * 60))
            .build()
            .expect("cron http client")
    })
}

/// GET one local endpoint and judge success by status code, logging the
/// bundle's error JSON on failure (the Go `callLocalSubStore` shape: the
/// details field is what actually tells an operator what broke).
pub(crate) async fn call_local_sub_store(task: &str, target: &str) {
    let request = cron_client()
        .get(target)
        // Browser-ish UA: the bundle's internal request checks reject
        // non-browser agents with a 500, and /download/ coalescing keys on
        // the UA (same choice as the Go client).
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) MihomoProbe-Cron/1.0",
        )
        .header("Accept", "application/json, text/plain, */*");
    let resp = match request.send().await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::error!(task, %target, %err, "定时任务网络请求失败");
            return;
        }
    };
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.as_u16() >= 300 {
        let details = error_details(&body);
        tracing::error!(
            task,
            details,
            status = status.as_u16(),
            "Sub-Store 定时任务执行失败"
        );
        return;
    }
    tracing::debug!(task, "定时任务执行成功");
}

/// Extract `error.details` from the bundle's error JSON, stripping the
/// `Reason:` prefix and the surrounding `[ ]` the Go version strips.
fn error_details(body: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let raw = parsed
        .pointer("/error/details")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .strip_prefix("Reason:")
        .unwrap_or_default()
        .trim()
        .trim_matches(|c| c == '[' || c == ']' || c == ' ')
        .to_string();
    if raw.is_empty() {
        // Not the expected shape (or truncated): surface the head of the body
        // so the log line is never empty.
        let head: String = body.chars().take(200).collect();
        head.trim().to_string()
    } else {
        raw
    }
}

/// `url.PathEscape` equivalent for one path segment.
fn path_escape(name: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    // Go's PathEscape keeps `-_.~` and alphanumerics unescaped. NON_ALPHANUMERIC
    // escapes a superset (including `-_.~`), which every URL decoder accepts —
    // over-escaping is safe where under-escaping would break the route.
    utf8_percent_encode(name, NON_ALPHANUMERIC).to_string()
}

/// The host part of `listen` in loopback form: a wildcard bind answers on
/// `127.0.0.1`, and the cron self-call must use an address that dials
/// ourselves rather than relying on external routing.
fn loopback_host(listen: &str) -> String {
    let (host, port) = match listen.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => return listen.to_string(),
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let host = match bare {
        "" | "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    format!("{host}:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_with_commas_in_the_cron_expression_survives_the_split() {
        let tasks = parse_produce_tasks("0 0 1,15 * *,sub,air");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].cron, "0 0 1,15 * *");
        assert!(!tasks[0].collection);
        assert_eq!(tasks[0].name, "air");
    }

    #[test]
    fn col_type_and_multiple_names_expand_to_one_task_per_name() {
        let tasks = parse_produce_tasks("*/30 * * * *,col,合集 A,合集B");
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| t.collection));
        assert_eq!(tasks[0].name, "合集 A");
        assert_eq!(tasks[1].name, "合集B");
        assert_eq!(tasks[0].cron, "*/30 * * * *");
    }

    #[test]
    fn multiple_tasks_are_semicolon_separated_and_blank_ones_skipped() {
        let tasks = parse_produce_tasks(" ;; 0 9 * * *,sub,air ;; 5 5 * * *,col,x ");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, "air");
        assert!(!tasks[0].collection);
        assert_eq!(tasks[1].name, "x");
        assert!(tasks[1].collection);
    }

    #[test]
    fn malformed_entries_are_skipped_not_fatal() {
        assert!(parse_produce_tasks("only-two-parts,sub").is_empty());
        assert!(parse_produce_tasks("a,b,c").is_empty(), "no sub/col marker");
        // An empty cron half still parses into a task; validation happens at
        // spawn time (AddFunc in the Go original).
        let tasks = parse_produce_tasks(",sub,air");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].cron, "");
        // Empty names between commas produce nothing.
        assert!(parse_produce_tasks("0 9 * * *,sub,,,").is_empty());
    }

    #[test]
    fn every_expression_shape_the_go_version_accepts_parses() {
        // 5-field (robfig default): normalized to the crate's seconds-first form.
        for expr in ["*/30 * * * *", "0 9 * * *", "0 0 1,15 * *", "5 4 * * sun"] {
            parse_schedule(expr).unwrap_or_else(|e| panic!("{expr}: {e}"));
        }
        // 6-field (seconds first) passes through, and @-shorthands work.
        parse_schedule("*/10 0 9 * * *").unwrap();
        parse_schedule("@daily").unwrap();
        assert!(parse_schedule("not a cron").is_err());
        assert!(parse_schedule("").is_err());
    }

    /// A real local server + one real fire of `call_local_sub_store`: proves
    /// the UA/Accept headers land, that a 302 (the backend's frontend bounce)
    /// is *not* followed, and that the error-JSON path reads `error.details`.
    #[tokio::test]
    async fn cron_calls_stay_local_and_read_the_error_details() {
        use axum::http::HeaderMap;
        use std::sync::atomic::AtomicUsize;

        let followed = Arc::new(AtomicUsize::new(0));
        let ua = Arc::new(std::sync::Mutex::new(String::new()));

        let app = {
            let followed = followed.clone();
            let ua = ua.clone();
            axum::Router::new()
                .route(
                    "/api/sync/artifacts",
                    axum::routing::get(move |headers: HeaderMap| async move {
                        *ua.lock().unwrap() = headers
                            .get("user-agent")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        (axum::http::StatusCode::OK, "ok")
                    }),
                )
                .route(
                    // The backend's unmatched-GET bounce: 302 to the frontend.
                    // The cron client must take the 302 as a failure verdict,
                    // never follow it.
                    "/bounce-source",
                    axum::routing::get(|| async {
                        (
                            axum::http::StatusCode::FOUND,
                            [("Location", "/bounced")],
                            "",
                        )
                    }),
                )
                .route(
                    "/bounced",
                    axum::routing::get(move || async move {
                        followed.fetch_add(1, Ordering::SeqCst);
                        "should not be reached"
                    }),
                )
                .route(
                    "/download/broken",
                    axum::routing::get(|| async {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            r#"{"status":"error","error":{"code":500,"type":"Internal","message":"x","details":"Reason:[ upstream timeout ]"}}"#,
                        )
                    }),
                )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        call_local_sub_store(
            "Gist同步",
            &format!("http://127.0.0.1:{port}/api/sync/artifacts"),
        )
        .await;
        assert!(ua.lock().unwrap().contains("MihomoProbe-Cron"));

        call_local_sub_store("重定向", &format!("http://127.0.0.1:{port}/bounce-source")).await;
        assert_eq!(
            followed.load(Ordering::SeqCst),
            0,
            "302 must not be followed"
        );

        call_local_sub_store(
            "缓存订阅「broken」",
            &format!("http://127.0.0.1:{port}/download/broken"),
        )
        .await;
        // Failure path is logged, not propagated; reaching here is the assert.
    }

    #[test]
    fn error_details_extraction_strips_the_go_shaped_noise() {
        let body = r#"{"status":"error","error":{"details":"Reason:[ upstream timeout ]"}}"#;
        assert_eq!(error_details(body), "upstream timeout");
        assert_eq!(error_details("not json at all"), "not json at all");
    }

    #[test]
    fn wildcard_binds_normalize_to_loopback_for_self_calls() {
        assert_eq!(loopback_host("127.0.0.1:18321"), "127.0.0.1:18321");
        assert_eq!(loopback_host("0.0.0.0:8299"), "127.0.0.1:8299");
        assert_eq!(loopback_host("0.0.0.0:8299"), "127.0.0.1:8299");
        assert_eq!(loopback_host("[::]:8299"), "127.0.0.1:8299");
        assert_eq!(loopback_host(":8299"), "127.0.0.1:8299");
    }
}
