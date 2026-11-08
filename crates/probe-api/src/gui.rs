//! The standalone GUI (workstreams/18 §B4; 《指导》§5, F-050..F-060): a single
//! embedded SPA served at `GET /gui`, talking only to `/api/v1/*` and
//! `/artifacts/*`. The old read-only panel at `/` is untouched.
//!
//! # Routes this module owns
//!
//! * `GET /gui` -- the SPA (static HTML, inline CSS/JS, vanilla JS, no build
//!   tooling). The HTML is a compile-time constant: no JSON value is ever
//!   interpolated into it, which is the server half of the XSS story.
//! * `GET /api/v1/session` -- exchanges the existing admin token (the same
//!   `crate::authorized` mechanism every guarded endpoint uses; no new token
//!   source) for a CSRF pair: a random token in the response body and the
//!   same value in an `HttpOnly; SameSite=Strict` cookie (double submit).
//!   The response carries no secret material: the CSRF token is a session
//!   capability, not a credential, and the admin token is never echoed.
//! * `POST /api/v1/gui/runs` -- the GUI's 立即测活 write. It reuses the
//!   existing trigger-round path exactly (`crate::spawn_round`, the same
//!   function `POST /api/v1/rounds` calls) behind the admin-token check AND
//!   the CSRF double-submit check, and appends an audit row.
//!
//! The publisher (`POST /api/v1/publishers/gist/{test,init}`) and migration
//! (`POST /api/v1/migrations/substore/{scan,apply,rollback}`) contracts land
//! in later batches (B2/B5). The SPA already calls those paths and renders a
//! 404 as 「未启用」 -- contract-first, nothing is invented here.
//!
//! # Security model
//!
//! * Auth: `/api/v1/session` and `/api/v1/gui/runs` both require the admin
//!   token through [`crate::authorized`], exactly like the guarded group.
//!   `/gui` itself is static and unauthenticated (no secrets in the bytes).
//! * CSRF: a GUI write must present the session's token BOTH as the
//!   `X-CSRF-Token` header and as the `probe_csrf` cookie, the two must
//!   match (constant time), and the pair must still be live in the
//!   process-local session table. Missing or wrong -> `403 csrf`.
//!   The cookie is `HttpOnly` (the SPA sends the header from the JSON body,
//!   it never reads the cookie) and `SameSite=Strict`.
//! * Audit: every GUI write that passes the checks records an audit row
//!   through the existing public writer `Storage::record_event` with level
//!   `security` (plus one row per issued session, an authentication event).
//!   The `security_audit` table's only writer, `activate_and_audit` in
//!   probe-storage `publication.rs`, is private to that module and reachable
//!   only via `publish_export`/`rollback_export`; exposing it means editing
//!   probe-storage, which this batch may not do. [`record_gui_audit`] is the
//!   single function to re-point when probe-storage grows a public
//!   `security_audit` writer.
//! * XSS: the SPA renders every dynamic value with `textContent` (the old
//!   panel's discipline); it contains no `innerHTML`,
//!   `insertAdjacentHTML` or `document.write` at all, and the tests pin
//!   that. A `display_name` of `<script>alert(1)</script>` therefore never
//!   becomes markup.
//! * No tokens, credentials or absolute filesystem paths in any GUI/session
//!   response: config values surface as `*_configured` booleans, Gist ids as
//!   masked strings, the SPA keeps the admin token in JS memory only.
//! * A Content-Security-Policy `meta` blocks any injected remote resource.
//!
//! # Mounting (the coordinator's two lib.rs lines -- nothing else changes)
//!
//! ```ignore
//! pub mod gui;                 // beside `mod sources;`
//!
//! // in build_router, on the unguarded router, immediately BEFORE
//! // `.merge(guarded)` so the trailing `security_headers` layer still
//! // wraps these routes:
//! .merge(gui::gui_routes())
//! ```
//!
//! `gui_routes()` carries its own [`crate::security_headers`] layer, so the
//! merge position is forgiving: an earlier or later merge on the unguarded
//! router yields the same headers (the middleware inserts, it does not
//! append). Merging it into the *guarded* group would put `/gui` behind the
//! admin token and break the SPA's 401-driven token prompt -- don't.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use serde_json::json;

use crate::exports::error_response;
use crate::{authorized, spawn_round, token_matches, RoundStart, SharedState};

/// The double-submit cookie name. `probe_` prefix: the same namespace the
/// service's other cookies would live in (there are none today).
const CSRF_COOKIE: &str = "probe_csrf";

/// The header a GUI write must carry the CSRF token in.
const CSRF_HEADER: &str = "X-CSRF-Token";

/// How long an issued CSRF pair stays valid. The SPA re-enters through the
/// token prompt after that (it treats any 403 as "session expired").
const SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);

/// Upper bound on live sessions. Evicting the soonest-to-expire entry keeps a
/// leaked token loop from growing the table without bound.
const MAX_SESSIONS: usize = 256;

/// The audit level [`record_gui_audit`] writes through `record_event`.
pub(crate) const AUDIT_LEVEL: &str = "security";

/// The routes this module adds. See the module docs for the merge position.
pub fn gui_routes() -> Router<SharedState> {
    Router::new()
        .route("/gui", get(gui_index))
        .route("/api/v1/session", get(session))
        .route("/api/v1/gui/runs", post(gui_trigger_run))
        .layer(middleware::from_fn(crate::security_headers))
}

/// `GET /gui` -- the embedded SPA. Static bytes: nothing user-controlled is
/// ever rendered into this response server-side.
async fn gui_index() -> Html<&'static str> {
    Html(GUI_HTML)
}

/// `GET /api/v1/session` -- admin-token in, CSRF pair out.
///
/// The response body holds the token the SPA echoes back as `X-CSRF-Token`;
/// the same value rides in an `HttpOnly` cookie so the server can compare
/// the two (double submit). Nothing else about the deployment is disclosed.
async fn session(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state) {
        return unauthorized();
    }
    let token = issue_csrf_token();
    record_gui_audit(&state, "session_issued", "actor=admin-token");
    let cookie = format!(
        "{CSRF_COOKIE}={token}; Path=/; Max-Age={max_age}; SameSite=Strict; HttpOnly",
        max_age = SESSION_TTL.as_secs(),
    );
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({"csrf_token": token, "expires_in": SESSION_TTL.as_secs()})),
    )
        .into_response()
}

/// `POST /api/v1/gui/runs` -- the SPA's 立即测活.
///
/// Admin token first (`401` like every guarded endpoint), then the CSRF
/// double submit (`403`), then the exact trigger path `POST /api/v1/rounds`
/// uses. The audit row records the outcome either way: a refused trigger is
/// as much an operator-relevant fact as a started one.
async fn gui_trigger_run(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state) {
        return unauthorized();
    }
    if let Some(rejection) = csrf_rejection(&headers) {
        return rejection;
    }
    match spawn_round(&state, "api", None) {
        RoundStart::Started(round_id) => {
            record_gui_audit(
                &state,
                "trigger_run",
                &format!("round_id={round_id} outcome=started"),
            );
            (
                StatusCode::ACCEPTED,
                Json(json!({"round_id": round_id, "started": true})),
            )
                .into_response()
        }
        RoundStart::Busy => {
            record_gui_audit(&state, "trigger_run", "outcome=busy");
            error_response(StatusCode::CONFLICT, "busy", "a round is already running")
        }
        RoundStart::Storage(err) => {
            record_gui_audit(&state, "trigger_run", "outcome=storage_error");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage",
                err.to_string(),
            )
        }
    }
}

// --- session store ---------------------------------------------------------

/// Live CSRF pairs: token -> the `Instant` it expires. Process-local by
/// design -- a restart invalidates the SPA's pair, which is the honest
/// behaviour for a panel that re-authenticates in one click.
fn sessions() -> &'static Mutex<HashMap<String, Instant>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Issue a fresh CSRF token and register its deadline.
fn issue_csrf_token() -> String {
    let token = random_csrf_token();
    let mut sessions = match sessions().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let now = Instant::now();
    sessions.retain(|_, deadline| *deadline > now);
    while sessions.len() >= MAX_SESSIONS {
        let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, deadline)| **deadline)
            .map(|(token, _)| token.clone())
        else {
            break;
        };
        sessions.remove(&oldest);
    }
    let deadline = now.checked_add(SESSION_TTL).unwrap_or(now);
    sessions.insert(token.clone(), deadline);
    token
}

/// Is this token a live session? An expired hit is dropped on sight.
fn session_live(token: &str) -> bool {
    let mut sessions = match sessions().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match sessions.get(token) {
        Some(deadline) if *deadline > Instant::now() => true,
        Some(_) => {
            sessions.remove(token);
            false
        }
        None => false,
    }
}

/// The CSRF check for GUI writes: `None` admits the request, `Some` is the
/// 403 to return. Header and cookie must both be present, equal (constant
/// time), and known to the session table -- a forged cookie pair without a
/// session row is refused, closing the classic double-submit injection hole.
fn csrf_rejection(headers: &HeaderMap) -> Option<Response> {
    let presented = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let cookie = cookie_value(headers, CSRF_COOKIE).unwrap_or_default();
    if presented.is_empty() || cookie.is_empty() || !token_matches(presented, &cookie) {
        return Some(csrf_forbidden(
            "X-CSRF-Token header and probe_csrf cookie must carry the session token",
        ));
    }
    if !session_live(presented) {
        return Some(csrf_forbidden("csrf session expired; reload the GUI"));
    }
    None
}

fn csrf_forbidden(message: &'static str) -> Response {
    error_response(StatusCode::FORBIDDEN, "csrf", message)
}

/// The same denial shape the admin guard answers with, so one client code
/// path handles both.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": {"code": "unauthorized",
            "message": "send header Authorization: Bearer <token> or X-Auth-Token"}})),
    )
        .into_response()
}

/// First `name=value` pair of the request cookies, if any. Deliberately
/// minimal: one cookie is defined, so a parser is all `std` needs.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for cookie_header in headers.get_all(header::COOKIE) {
        let Ok(raw) = cookie_header.to_str() else {
            continue;
        };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some(rest) = pair.strip_prefix(name) {
                if let Some(value) = rest.strip_prefix('=') {
                    return Some(value.trim().to_string());
                }
            }
        }
    }
    None
}

// --- randomness ------------------------------------------------------------

/// The splitmix64 generator state, seeded lazily.
static PRNG: OnceLock<Mutex<u64>> = OnceLock::new();

/// A 128-bit CSRF token as 32 lowercase hex chars.
///
/// probe-api has no RNG dependency and this batch may not edit `Cargo.toml`,
/// so the seed comes from the standard library's OS-seeded `RandomState`
/// (the HashMap key material) mixed with wall-clock nanoseconds, then
/// advanced by splitmix64. That is not a CSPRNG; it is, however,
/// unpredictable to anyone who cannot read process memory -- the threat
/// model of a double-submit CSRF token on a loopback-bound admin panel --
/// and the module tests pin the token to the documented shape.
fn random_csrf_token() -> String {
    let mutex = PRNG.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        let hashed = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos() as u64)
            .unwrap_or(0);
        Mutex::new(hashed ^ nanos.rotate_left(17))
    });
    let mut state = match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let a = splitmix64(&mut state);
    let b = splitmix64(&mut state);
    format!("{a:016x}{b:016x}")
}

/// One splitmix64 step (Steele et al. 2014): cheap, well-distributed, and
/// deterministic given the seed -- which is what the tests rely on.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// --- audit -----------------------------------------------------------------

/// Append the GUI's audit row through the existing public audit writer.
///
/// `Storage::record_event` appends to the `events` ledger (level truncated
/// to 16 chars, message to 500): the row reads
/// `security | gui.<action> <detail>`. The `security_audit` table's writer
/// is private to probe-storage (see module docs); re-point this one function
/// there when a public writer lands. Best effort: an audit failure is logged
/// and never fails the write it describes, matching [`record_event`]'s own
/// "never fail the round" contract.
fn record_gui_audit(state: &crate::AppState, action: &str, detail: &str) {
    let message = format!("gui.{action} {detail}");
    let outcome = match state.storage.lock() {
        Ok(storage) => storage.record_event(AUDIT_LEVEL, &message),
        Err(poisoned) => poisoned.into_inner().record_event(AUDIT_LEVEL, &message),
    };
    if let Err(err) = outcome {
        tracing::warn!(%err, "gui audit row not recorded");
    }
}

// --- the SPA ---------------------------------------------------------------

/// The embedded GUI. Every dynamic value reaches the DOM through
/// `textContent`; the script contains no `innerHTML`, `insertAdjacentHTML`
/// or `document.write`, and `gui_tests.rs` pins both that absence and the
/// escape discipline with a hostile `display_name`.
const GUI_HTML: &str = r#"<!doctype html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'">
<title>probe 控制台</title>
<style>
:root{
  color-scheme: light dark;
  --bg:#101319; --panel:#181c25; --panel2:#1f2430; --line:#2b3140;
  --text:#e4e7ee; --muted:#8b94a7; --code:#0d1016;
  --ok:#4ade80; --bad:#f87171; --warn:#fbbf24; --accent:#5b8def;
}
@media (prefers-color-scheme: light){
  :root{
    --bg:#f4f6fa; --panel:#ffffff; --panel2:#eef1f7; --line:#d7dce6;
    --text:#1d2530; --muted:#5d6a7e; --code:#eef1f7;
    --ok:#15803d; --bad:#b91c1c; --warn:#92610a; --accent:#2456c4;
  }
}
*{box-sizing:border-box}
body{margin:0;padding:16px;background:var(--bg);color:var(--text);
  font:14px/1.55 system-ui,-apple-system,'Segoe UI',Roboto,'PingFang SC','Microsoft YaHei',sans-serif}
h1{font-size:17px;margin:0 0 2px}
h2{font-size:14px;margin:20px 0 8px;color:var(--muted)}
h3{font-size:14px;margin:0}
a{color:var(--accent)}
.muted{color:var(--muted)}
.mono{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:12px}
code.url{background:var(--code);border:1px solid var(--line);border-radius:6px;
  padding:2px 6px;word-break:break-all}
header{margin-bottom:12px}
.gate{background:var(--panel);border:1px solid var(--line);border-radius:10px;
  padding:16px;max-width:420px;margin:24px auto}
.gate label{display:block;margin-bottom:6px}
.gate input{width:100%;margin-bottom:10px}
.hint{color:var(--muted);font-size:12px;margin:8px 0 0}
nav[role=tablist]{display:flex;gap:6px;flex-wrap:wrap;margin:8px 0 14px;
  border-bottom:1px solid var(--line);padding-bottom:8px}
nav[role=tablist] button{background:var(--panel);border:1px solid var(--line);
  color:var(--text);border-radius:8px;padding:6px 12px;cursor:pointer;font:inherit}
nav[role=tablist] button[aria-selected=true]{background:var(--panel2);
  border-color:var(--accent);font-weight:600}
button{font:inherit}
.btn,.gate button{background:var(--panel2);border:1px solid var(--line);color:var(--text);
  border-radius:8px;padding:6px 12px;cursor:pointer}
.btn:hover,nav[role=tablist] button:hover{border-color:var(--accent)}
.btn.primary{background:var(--accent);border-color:var(--accent);color:#fff}
.btn.small{padding:3px 8px;font-size:12px}
.btn[aria-disabled=true]{opacity:.55;cursor:not-allowed}
button:focus-visible,input:focus-visible,select:focus-visible,[tabindex]:focus-visible{
  outline:2px solid var(--accent);outline-offset:2px}
.live{min-height:22px;margin:0 0 10px;padding:4px 10px;border-radius:8px;
  background:var(--panel);border:1px dashed var(--line);color:var(--muted)}
.row{display:flex;gap:10px;flex-wrap:wrap;align-items:center;margin-bottom:12px}
.stat{background:var(--panel);border:1px solid var(--line);border-radius:10px;
  padding:8px 14px;min-width:120px}
.stat .k{color:var(--muted);font-size:11px;text-transform:uppercase;letter-spacing:.4px}
.stat .v{font-size:15px;font-weight:600;margin-top:2px}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(290px,1fr));gap:12px}
.card{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:12px}
.card-head{display:flex;justify-content:space-between;gap:8px;align-items:baseline;margin-bottom:6px}
.link{background:none;border:none;padding:0;color:var(--accent);cursor:pointer;font:inherit;font-weight:600}
.line{display:flex;gap:6px;align-items:center;flex-wrap:wrap;margin:3px 0;font-size:13px}
.line .k{color:var(--muted);flex:none}
.url-line{gap:8px}
.actions{margin-top:8px;display:flex;gap:8px}
.badge{display:inline-block;border-radius:999px;padding:1px 9px;font-size:12px;
  border:1px solid var(--line);background:var(--panel2);white-space:nowrap}
.badge.ok{border-color:var(--ok);color:var(--ok)}
.badge.bad{border-color:var(--bad);color:var(--bad)}
.badge.warn{border-color:var(--warn);color:var(--warn)}
.badge.muted{color:var(--muted)}
.tscroll{overflow-x:auto;background:var(--panel);border:1px solid var(--line);
  border-radius:10px;margin-bottom:14px}
table{width:100%;border-collapse:collapse;font-size:12.5px;min-width:520px}
th,td{text-align:left;padding:5px 9px;border-bottom:1px solid var(--line);
  white-space:nowrap;max-width:260px;overflow:hidden;text-overflow:ellipsis}
th{color:var(--muted);font-weight:500;position:sticky;top:0;background:var(--panel)}
tbody tr:last-child td{border-bottom:none}
select,input{background:var(--panel2);border:1px solid var(--line);color:var(--text);
  border-radius:8px;padding:6px 9px;font:inherit}
select{max-width:100%}
pre{background:var(--code);border:1px solid var(--line);border-radius:8px;
  padding:10px;overflow-x:auto;font-size:12px}
.panel-note{background:var(--panel);border:1px solid var(--line);border-radius:10px;
  padding:10px 12px;margin-bottom:12px}
.kv{margin-bottom:10px}
.kv > div{display:flex;gap:10px;font-size:13px;margin:2px 0}
.kv .k{color:var(--muted);min-width:90px;flex:none}
footer{margin-top:22px;color:var(--muted);font-size:12px}
@media (max-width:400px){
  body{padding:10px}
  .grid{grid-template-columns:1fr}
  .stat{min-width:0;flex:1 1 40%}
}
</style>
</head>
<body>
<header>
  <h1>probe 控制台</h1>
  <div class="muted">独立 GUI · 数据来自 /api/v1 与 /artifacts · 旧只读面板位于 /</div>
</header>

<noscript><div class="panel-note">本控制台需要启用 JavaScript。</div></noscript>

<div id="gate" class="gate">
  <label for="tok">管理令牌</label>
  <input id="tok" type="password" autocomplete="off" placeholder="admin token">
  <button id="enter" type="button" class="btn primary">进入</button>
  <p class="hint">令牌只保存在本页内存中，用于调用受保护接口；不会写入存储或日志。</p>
</div>

<div id="app" hidden>
  <nav id="tabs" role="tablist" aria-label="页面">
    <button type="button" role="tab" id="tab-dash" data-tab="dash"
      aria-controls="page-dash" aria-selected="true">总览</button>
    <button type="button" role="tab" id="tab-detail" data-tab="detail"
      aria-controls="page-detail" aria-selected="false" tabindex="-1">Source 详情</button>
    <button type="button" role="tab" id="tab-pub" data-tab="pub"
      aria-controls="page-pub" aria-selected="false" tabindex="-1">发布设置</button>
    <button type="button" role="tab" id="tab-clean" data-tab="clean"
      aria-controls="page-clean" aria-selected="false" tabindex="-1">清理中心</button>
  </nav>
  <div id="live" aria-live="polite"></div>

  <section id="page-dash" role="tabpanel" aria-labelledby="tab-dash">
    <div class="row" id="dash-cards"></div>
    <h2>Sources</h2>
    <div id="source-cards" class="grid"></div>
  </section>

  <section id="page-detail" role="tabpanel" aria-labelledby="tab-detail" hidden>
    <div class="row">
      <label for="detail-source">Source</label>
      <select id="detail-source"></select>
      <button type="button" id="detail-refresh" class="btn">刷新</button>
    </div>
    <div class="kv" id="detail-meta"></div>
    <h2>状态分布</h2>
    <div class="row" id="dist-list"></div>
    <h2>近 20 次运行</h2>
    <div class="tscroll"><table>
      <thead><tr><th>#</th><th>状态</th><th>触发</th><th>轮次</th><th>领取 (UTC)</th><th>更新 (UTC)</th></tr></thead>
      <tbody id="runs-body"></tbody>
    </table></div>
    <h2>失败聚合</h2>
    <div class="tscroll"><table>
      <thead><tr><th>原因</th><th>节点数</th></tr></thead>
      <tbody id="fail-agg"></tbody>
    </table></div>
    <h2>节点指纹（只读）</h2>
    <div class="tscroll"><table>
      <thead><tr><th>指纹</th><th>名称</th><th>状态</th><th>国别</th><th>出口</th>
        <th>延迟 ms</th><th>连败</th><th>最后原因</th></tr></thead>
      <tbody id="nodes-body"></tbody>
    </table></div>
    <div class="row">
      <button type="button" class="btn" aria-disabled="true" data-placeholder="产物对比"
        title="产物对比将在后续批次提供">产物对比</button>
      <button type="button" class="btn" aria-disabled="true" data-placeholder="回滚"
        title="回滚将在后续批次提供">回滚</button>
    </div>
  </section>

  <section id="page-pub" role="tabpanel" aria-labelledby="tab-pub" hidden>
    <h2>发布器</h2>
    <div class="kv" id="pub-config"></div>
    <div class="row" id="pub-badges"></div>
    <div class="row">
      <button type="button" id="pub-test" class="btn">测试连接 (Gist)</button>
      <button type="button" id="pub-test-webdav" class="btn">测试连接 (WebDAV)</button>
      <button type="button" id="pub-init" class="btn">初始化 Gist</button>
    </div>
    <div class="panel-note" id="pub-result">尚未执行发布器操作。</div>
    <h2>人工推送到 Sub-Store</h2>
    <div class="panel-note" id="pub-push-note">三步流程：预览差异 → 签发一次性确认单（60s）→ 推送（update-only，绝不新建）。目标：<code id="pub-push-target">—</code></div>
    <div class="row">
      <button type="button" id="pub-push-preview" class="btn">预览差异</button>
      <button type="button" id="pub-push-confirm" class="btn">签发确认单</button>
      <button type="button" id="pub-push" class="btn">推送到 Sub-Store</button>
    </div>
    <div class="panel-note" id="pub-push-result">尚未执行人工推送操作。</div>
    <h2>文件名映射</h2>
    <div class="tscroll"><table>
      <thead><tr><th>Source</th><th>文件名映射</th><th>Gist ID</th><th>最近发布 (UTC)</th><th>产物 sha</th></tr></thead>
      <tbody id="pub-mapping"></tbody>
    </table></div>
    <h2>配置预检查</h2>
    <div class="panel-note" id="pub-precheck">尚未读取配置。</div>
  </section>

  <section id="page-clean" role="tabpanel" aria-labelledby="tab-clean" hidden>
    <div class="panel-note">
      清理中心扫描旧 Sub-Store 测活产物（&lt;prefix&gt;-*-local、&lt;prefix&gt;-front-manual、
      ipmap 实测映射订阅）并分类 keep / duplicate / uncertain。
      「扫描」是 dry-run；「删除」针对 duplicate 类，需要两次确认，且部署侧必须
      打开 substore.allow_delete（或 ALLOW_SUBSTORE_DELETE=1）才真正执行；
      删除前每个对象先备份到本地，「回滚清理」用最近一次删除的备份还原。
    </div>
    <div class="row">
      <button type="button" id="clean-scan" class="btn primary">扫描（dry-run）</button>
      <button type="button" id="clean-apply" class="btn">删除测活产物…</button>
      <button type="button" id="clean-rollback" class="btn">回滚清理…</button>
    </div>
    <div class="panel-note" id="clean-result">尚未扫描。</div>
  </section>

  <footer>probe 独立 GUI · 写操作均要求会话 CSRF 令牌并写入审计</footer>
</div>

<script>
(function(){
'use strict';

var tok = '';
var csrf = '';
// 最近一次清理删除写入的备份文件名（应答里的 backup_file）；回滚清理用。
var lastCleanupBackup = null;
var sourcesCache = [];
var detailSeq = 0;
var TABS = ['dash', 'detail', 'pub', 'clean'];
var ICONS = { ok: '✓ ', bad: '✕ ', warn: '▲ ', muted: '○ ' };

function $(sel){ return document.querySelector(sel); }

function el(tag, cls, text){
  var node = document.createElement(tag);
  if (cls) { node.className = cls; }
  if (text !== undefined && text !== null) { node.textContent = String(text); }
  return node;
}

function clear(node){
  while (node.firstChild) { node.removeChild(node.firstChild); }
}

function say(message){
  var live = $('#live');
  clear(live);
  live.appendChild(el('span', null, message));
}

function badge(text, kind){
  return el('span', 'badge ' + kind, (ICONS[kind] || '') + text);
}

function short8(value){
  return value ? String(value).slice(0, 8) : '—';
}

function when(value){
  return value ? String(value).replace('T', ' ') : '—';
}

function maskId(value){
  var id = String(value || '');
  if (!id) { return '—'; }
  if (id.length <= 8) { return '····' + id.slice(-2); }
  return id.slice(0, 4) + '…' + id.slice(-4);
}

function api(method, path, payload, write){
  var headers = { 'Accept': 'application/json', 'X-Auth-Token': tok };
  if (write) { headers['X-CSRF-Token'] = csrf; }
  var init = { method: method, headers: headers, cache: 'no-store' };
  if (payload !== undefined && payload !== null) {
    headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(payload);
  }
  return fetch(path, init).then(function(response){
    return response.text().then(function(raw){
      var body = null;
      if (raw) { try { body = JSON.parse(raw); } catch (err) { body = null; } }
      return { status: response.status, ok: response.ok, body: body };
    });
  });
}

function errorNote(res, label){
  var code = res.body && res.body.error && res.body.error.code ? res.body.error.code : '';
  return label + '失败（' + res.status + (code ? ' ' + code : '') + '）。';
}

// --- session gate ----------------------------------------------------------

function enter(){
  var input = $('#tok');
  tok = (input.value || '').trim();
  if (!tok) { say('请先输入管理令牌。'); input.focus(); return; }
  api('GET', '/api/v1/session').then(function(res){
    if (res.status === 200 && res.body && res.body.csrf_token) {
      csrf = String(res.body.csrf_token);
      $('#gate').hidden = true;
      $('#app').hidden = false;
      say('会话已建立，数据加载中…');
      loadDashboard();
    } else if (res.status === 401) {
      say('管理令牌无效（401）。');
    } else {
      say('会话建立失败（' + res.status + '）。');
    }
  }).catch(function(){ say('网络错误：服务不可达。'); });
}

// --- tabs ------------------------------------------------------------------

function selectTab(name, focus){
  TABS.forEach(function(t){
    var tab = $('#tab-' + t);
    var page = $('#page-' + t);
    var active = (t === name);
    tab.setAttribute('aria-selected', active ? 'true' : 'false');
    tab.tabIndex = active ? 0 : -1;
    page.hidden = !active;
  });
  if (focus) { $('#tab-' + name).focus(); }
  if (name === 'dash') { loadDashboard(); }
  if (name === 'detail') { loadDetailPage(null); }
  if (name === 'pub') { loadPublish(); }
}

function initTabs(){
  var nav = $('#tabs');
  nav.addEventListener('click', function(ev){
    var tab = ev.target && ev.target.closest ? ev.target.closest('[role=tab]') : null;
    if (tab && tab.getAttribute('data-tab')) { selectTab(tab.getAttribute('data-tab'), false); }
  });
  nav.addEventListener('keydown', function(ev){
    var active = document.activeElement;
    var current = active && active.getAttribute ? TABS.indexOf(active.getAttribute('data-tab')) : -1;
    if (current < 0) { return; }
    var next = null;
    if (ev.key === 'ArrowRight') { next = (current + 1) % TABS.length; }
    else if (ev.key === 'ArrowLeft') { next = (current + TABS.length - 1) % TABS.length; }
    else if (ev.key === 'Home') { next = 0; }
    else if (ev.key === 'End') { next = TABS.length - 1; }
    if (next !== null) { ev.preventDefault(); selectTab(TABS[next], true); }
  });
}

// --- 总览 dashboard ---------------------------------------------------------

function statusBySource(statusBody){
  var map = {};
  var list = (statusBody && statusBody.sources) || [];
  list.forEach(function(row){
    if (row && row.source) { map[row.source] = row; }
  });
  return map;
}

function dashCard(label, value){
  var card = el('div', 'stat');
  card.appendChild(el('div', 'k', label));
  card.appendChild(el('div', 'v', value));
  return card;
}

function renderDashCards(status){
  var host = $('#dash-cards');
  clear(host);
  var rounds = (status && status.rounds) || {};
  var schedule = (status && status.schedule) || {};
  var last = rounds.last || {};
  host.appendChild(dashCard('运行状态', rounds.running ? '● 运行中' : '○ 空闲'));
  host.appendChild(dashCard('上次轮次', (last.round_id === undefined || last.round_id === null) ? '—' : '#' + last.round_id));
  host.appendChild(dashCard('上次结果', last.total !== undefined ? ('存活 ' + last.ok + ' / 失败 ' + last.failed + ' / 共 ' + last.total) : '—'));
  host.appendChild(dashCard('下次轮次', schedule.next_run || (schedule.enabled ? '待定' : '已关闭')));
  // G-3（workstreams/34 R3）：探测点面板——最近一轮聚合摘要的诚实口径。
  var probes = (status && status.probes) || null;
  if (probes) {
    var completed = (probes.completed_probes || []).join('、') || '—';
    var missing = (probes.missing_probes || []);
    var offline = (probes.offline_probes || []);
    var text = '已回传 ' + completed
      + '（缺 ' + missing.length + '、离线 ' + offline.length + '）'
      + '，最少完成 ' + (probes.minimum_completed_probes === undefined ? '—' : probes.minimum_completed_probes);
    host.appendChild(dashCard('探测点', text));
    if (missing.length || offline.length) {
      host.lastChild.classList.add('warn');
    }
  }
}

function runBadge(status){
  var map = {
    'completed': ['已完成', 'ok'],
    'active': ['运行中', 'warn'],
    'claimed': ['已领取', 'warn'],
    'failed': ['失败', 'bad'],
    'coalesced': ['已合并', 'muted'],
    'released': ['已释放', 'muted'],
    'abandoned': ['已中止', 'muted']
  };
  var hit = map[status] || [String(status || '未知'), 'muted'];
  return badge(hit[0], hit[1]);
}

function sourceCard(source, summary){
  var card = el('div', 'card');
  var head = el('div', 'card-head');
  var nameBtn = el('button', 'link', source.display_name || source.key);
  nameBtn.type = 'button';
  nameBtn.title = '查看 Source 详情';
  nameBtn.addEventListener('click', function(){
    selectTab('detail', false);
    loadDetailPage(source.source_id);
  });
  head.appendChild(nameBtn);
  var sid = el('span', 'mono muted', short8(source.source_id));
  sid.title = 'source_id: ' + source.source_id;
  head.appendChild(sid);
  card.appendChild(head);

  var artifact = source.artifact || null;
  var counts = summary
    ? ('存活 ' + summary.alive + ' / 失败 ' + summary.dead + ' / 共 ' + summary.total)
    : '暂无轮次统计';
  var nodesLine = el('div', 'line');
  nodesLine.appendChild(el('span', 'k', '节点'));
  nodesLine.appendChild(el('span', null, counts));
  card.appendChild(nodesLine);

  var runLine = el('div', 'line');
  runLine.appendChild(el('span', 'k', '上次测活'));
  var runSlot = el('span', 'muted', '读取中…');
  runLine.appendChild(runSlot);
  card.appendChild(runLine);

  var pubLine = el('div', 'line');
  pubLine.appendChild(el('span', 'k', '发布'));
  pubLine.appendChild(artifact && artifact.gist_id ? badge('Gist 已配置', 'ok') : badge('Gist 未配置', 'muted'));
  pubLine.appendChild(el('span', 'muted', when(artifact && artifact.last_published_at)));
  card.appendChild(pubLine);

  var shaLine = el('div', 'line');
  shaLine.appendChild(el('span', 'k', '产物 sha'));
  shaLine.appendChild(el('span', 'mono', artifact && artifact.last_content_sha256 ? short8(artifact.last_content_sha256) : '—'));
  card.appendChild(shaLine);

  var url = (artifact && artifact.stable_url)
    ? String(artifact.stable_url)
    : ('/artifacts/' + encodeURIComponent(source.source_id) + '/subscription');
  var urlLine = el('div', 'line url-line');
  urlLine.appendChild(el('span', 'k', '稳定订阅'));
  urlLine.appendChild(el('code', 'url mono', url));
  var copyBtn = el('button', 'btn small', '复制');
  copyBtn.type = 'button';
  copyBtn.setAttribute('data-url', url);
  copyBtn.addEventListener('click', copyUrl);
  urlLine.appendChild(copyBtn);
  card.appendChild(urlLine);

  var actions = el('div', 'actions');
  var runBtn = el('button', 'btn primary', '立即测活');
  runBtn.type = 'button';
  runBtn.addEventListener('click', function(){ triggerRun(runBtn); });
  actions.appendChild(runBtn);
  card.appendChild(actions);

  api('GET', '/api/v1/sources/' + encodeURIComponent(source.source_id) + '/runs?limit=1').then(function(res){
    clear(runSlot);
    var runs = (res.body && res.body.runs) || [];
    if (res.status !== 200) {
      runSlot.appendChild(el('span', 'muted', '读取失败（' + res.status + '）'));
      return;
    }
    if (!runs.length) {
      runSlot.appendChild(el('span', 'muted', '尚无记录'));
      return;
    }
    runSlot.appendChild(runBadge(runs[0].status));
    runSlot.appendChild(el('span', 'muted', ' ' + when(runs[0].updated_at)));
  }).catch(function(){
    clear(runSlot);
    runSlot.appendChild(el('span', 'muted', '读取失败：网络错误'));
  });
  return card;
}

function renderSourceCards(sources, bySource){
  var host = $('#source-cards');
  clear(host);
  if (!sources.length) {
    host.appendChild(el('p', 'muted', '当前部署没有配置任何 Source。'));
    return;
  }
  sources.forEach(function(source){
    host.appendChild(sourceCard(source, bySource[source.key] || null));
  });
}

function loadDashboard(){
  Promise.all([api('GET', '/api/v1/sources'), api('GET', '/api/v1/status')]).then(function(pair){
    if (pair[0].status === 401 || pair[1].status === 401) {
      say('管理令牌无效（401），请刷新页面重新进入。');
      return;
    }
    renderDashCards(pair[1].body);
    renderSourceCards((pair[0].body && pair[0].body.sources) || [], statusBySource(pair[1].body));
    say('总览已刷新。');
  }).catch(function(){ say('加载总览失败：网络错误。'); });
}

function triggerRun(btn){
  if (!window.confirm('立即测活会触发一轮全量测活（覆盖所有启用中的 Source）。继续吗？')) {
    say('已取消测活触发。');
    return;
  }
  btn.disabled = true;
  api('POST', '/api/v1/gui/runs', {}, true).then(function(res){
    btn.disabled = false;
    if (res.status === 202) {
      say('测活轮次已启动（round ' + ((res.body && res.body.round_id) !== undefined ? res.body.round_id : '—') + '）。');
    } else if (res.status === 409) {
      say('已有轮次在运行（409），本次未重复触发。');
    } else if (res.status === 401) {
      say('管理令牌无效（401）。');
    } else if (res.status === 403) {
      say('CSRF 校验失败（403）：会话可能已过期，请刷新页面重新进入。');
    } else {
      say(errorNote(res, '触发'));
    }
  }).catch(function(){
    btn.disabled = false;
    say('网络错误：触发请求未送达。');
  });
}

function copyUrl(ev){
  var btn = ev.currentTarget;
  var url = btn.getAttribute('data-url') || '';
  var fallback = function(){
    window.prompt('手动复制稳定订阅 URL：', url);
    say('已弹出 URL，请手动复制。');
  };
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(url).then(function(){ say('稳定订阅 URL 已复制。'); }, fallback);
  } else {
    fallback();
  }
}

// --- Source 详情 ------------------------------------------------------------

function findSource(id){
  var hit = null;
  (sourcesCache || []).forEach(function(s){ if (s.source_id === id) { hit = s; } });
  return hit;
}

function loadDetailPage(presetId){
  var seq = ++detailSeq;
  api('GET', '/api/v1/sources').then(function(res){
    if (seq !== detailSeq) { return; }
    if (res.status !== 200) {
      say('读取 Source 列表失败（' + res.status + '）。');
      return;
    }
    sourcesCache = (res.body && res.body.sources) || [];
    var sel = $('#detail-source');
    clear(sel);
    if (!sourcesCache.length) {
      sel.appendChild(el('option', null, '（无 Source）'));
    }
    sourcesCache.forEach(function(s){
      var opt = el('option', null, (s.display_name || s.key) + '（' + short8(s.source_id) + '）');
      opt.value = s.source_id;
      sel.appendChild(opt);
    });
    if (presetId && sourcesCache.some(function(s){ return s.source_id === presetId; })) {
      sel.value = presetId;
    }
    if (sel.value) { loadDetail(sel.value); }
    else { say('当前部署没有可查看的 Source。'); }
  }).catch(function(){ say('加载详情失败：网络错误。'); });
}

function loadDetail(sourceId){
  var id = sourceId || ($('#detail-source') ? $('#detail-source').value : '');
  if (!id) { say('请选择一个 Source。'); return; }
  var source = findSource(id);
  if (!source) { say('Source 不存在或已被移除。'); return; }
  var meta = $('#detail-meta');
  clear(meta);
  var nameRow = el('span', null, source.display_name || source.key);
  var title = el('div', null, null);
  title.appendChild(el('span', 'k', 'Source'));
  title.appendChild(nameRow);
  meta.appendChild(title);
  var idRow = el('div', null, null);
  idRow.appendChild(el('span', 'k', 'source_id'));
  idRow.appendChild(el('span', 'mono', source.source_id));
  meta.appendChild(idRow);
  var keyRow = el('div', null, null);
  keyRow.appendChild(el('span', 'k', 'key'));
  keyRow.appendChild(el('span', 'mono', source.key));
  meta.appendChild(keyRow);
  var stateRow = el('div', null, null);
  stateRow.appendChild(el('span', 'k', '输入'));
  stateRow.appendChild(el('span', null, (source.input_type || '—') + ' · ' + (source.input_ref || '—')));
  meta.appendChild(stateRow);
  loadDetailNodes(source);
  loadDetailRuns(id);
}

function loadDetailNodes(source){
  clearRows($('#nodes-body'));
  clearRows($('#fail-agg'));
  clear($('#dist-list'));
  api('GET', '/api/v1/nodes?source=' + encodeURIComponent(source.key) + '&limit=200').then(function(res){
    if (res.status !== 200) {
      say('读取节点失败（' + res.status + '）。');
      return;
    }
    var nodes = (res.body && res.body.nodes) || [];
    renderDistribution(nodes);
    renderFailureAggregation(nodes);
    renderNodeTable(nodes);
  }).catch(function(){ say('读取节点失败：网络错误。'); });
}

var STATE_LABELS = {
  alive: ['存活', 'ok'],
  dead: ['失败', 'bad'],
  pending: ['等待', 'warn'],
  unknown: ['未知', 'muted'],
  excluded: ['排除', 'muted']
};

function renderDistribution(nodes){
  var host = $('#dist-list');
  clear(host);
  var counts = {};
  nodes.forEach(function(node){
    var status = String(node.status || 'unknown');
    counts[status] = (counts[status] || 0) + 1;
  });
  if (!nodes.length) {
    host.appendChild(el('span', 'muted', '该 Source 暂无节点记录。'));
    return;
  }
  Object.keys(STATE_LABELS).forEach(function(status){
    if (counts[status]) {
      var hit = STATE_LABELS[status];
      host.appendChild(badge(hit[0] + ' ' + counts[status], hit[1]));
    }
  });
  Object.keys(counts).forEach(function(status){
    if (!STATE_LABELS[status]) {
      host.appendChild(badge(status + ' ' + counts[status], 'muted'));
    }
  });
}

function renderFailureAggregation(nodes){
  var body = $('#fail-agg');
  clearRows(body);
  var reasons = {};
  nodes.forEach(function(node){
    var reason = node.last_reason ? String(node.last_reason) : '（无原因记录）';
    reasons[reason] = (reasons[reason] || 0) + 1;
  });
  var keys = Object.keys(reasons).sort(function(a, b){ return reasons[b] - reasons[a]; });
  if (!keys.length) {
    var none = el('tr');
    none.appendChild(el('td', null, '暂无数据'));
    body.appendChild(none);
    return;
  }
  keys.forEach(function(reason){
    var tr = el('tr');
    tr.appendChild(el('td', null, reason));
    tr.appendChild(el('td', null, reasons[reason]));
    body.appendChild(tr);
  });
}

function renderNodeTable(nodes){
  var body = $('#nodes-body');
  clearRows(body);
  nodes.forEach(function(node){
    var tr = el('tr');
    var fp = el('td', 'mono', short8(node.fingerprint));
    if (node.fingerprint) { fp.title = node.fingerprint; }
    tr.appendChild(fp);
    tr.appendChild(el('td', null, node.display || '—'));
    var statusHit = STATE_LABELS[node.status] || [String(node.status || '未知'), 'muted'];
    var statusCell = el('td');
    statusCell.appendChild(badge(statusHit[0], statusHit[1]));
    tr.appendChild(statusCell);
    tr.appendChild(el('td', null, node.country || '—'));
    tr.appendChild(el('td', 'mono', node.exit_ip || '—'));
    tr.appendChild(el('td', null, (node.last_delay_ms === undefined || node.last_delay_ms === null) ? '—' : node.last_delay_ms));
    tr.appendChild(el('td', null, node.consec_fail === undefined ? '—' : node.consec_fail));
    tr.appendChild(el('td', null, node.last_reason || '—'));
    body.appendChild(tr);
  });
  if (!nodes.length) {
    var tr = el('tr');
    tr.appendChild(el('td', null, '暂无数据'));
    body.appendChild(tr);
  }
}

function loadDetailRuns(sourceId){
  clearRows($('#runs-body'));
  api('GET', '/api/v1/sources/' + encodeURIComponent(sourceId) + '/runs?limit=20').then(function(res){
    if (res.status !== 200) {
      say('读取运行历史失败（' + res.status + '）。');
      return;
    }
    var body = $('#runs-body');
    var runs = (res.body && res.body.runs) || [];
    runs.forEach(function(run, index){
      var tr = el('tr');
      tr.appendChild(el('td', null, index + 1));
      var statusCell = el('td');
      statusCell.appendChild(runBadge(run.status));
      tr.appendChild(statusCell);
      tr.appendChild(el('td', null, run.trigger || '—'));
      tr.appendChild(el('td', 'mono', (run.round_id === undefined || run.round_id === null) ? '—' : '#' + run.round_id));
      tr.appendChild(el('td', null, when(run.claimed_at)));
      tr.appendChild(el('td', null, when(run.updated_at)));
      body.appendChild(tr);
    });
    if (!runs.length) {
      var tr = el('tr');
      tr.appendChild(el('td', null, '暂无数据'));
      body.appendChild(tr);
    }
  }).catch(function(){ say('读取运行历史失败：网络错误。'); });
}

function clearRows(body){
  clear(body);
  var tr = el('tr');
  tr.appendChild(el('td', null, '读取中…'));
  body.appendChild(tr);
}

// --- 发布设置 ---------------------------------------------------------------

function setPanelText(node, message){
  clear(node);
  node.appendChild(el('span', null, message));
}

function renderPublishConfig(res){
  var host = $('#pub-config');
  clear(host);
  var badges = $('#pub-badges');
  clear(badges);
  if (res.status !== 200) {
    setPanelText($('#pub-precheck'), '配置预检查读取失败（' + res.status + '）。');
    host.appendChild(el('span', 'muted', '配置读取失败，无法显示发布器状态。'));
    return;
  }
  // 端点把发布面放在 summary.publish（顶层是 path/differences 等）；兼容
  // 两种形状，避免整页 mask 落到空对象的默认值上。
  var cfg = (res.body && res.body.publish)
    || ((res.body && res.body.summary && res.body.summary.publish) || {});
  host.appendChild(kvRow('发布功能', cfg.enabled ? '已启用' : '未启用'));
  // 只显示配置布尔，从不回显凭据本身。
  host.appendChild(kvRow('发布凭据', cfg.token_configured ? '已配置' : '未配置'));
  host.appendChild(kvRow('订阅前缀', cfg.prefix || '—'));
  host.appendChild(kvRow('主机名', cfg.hostname || '—'));
  host.appendChild(kvRow('发布通道', cfg.provider || 'local'));
  badges.appendChild(badge(cfg.enabled ? '发布已启用' : '发布未启用', cfg.enabled ? 'ok' : 'muted'));
  badges.appendChild(badge('通道 ' + (cfg.provider || 'local'), 'muted'));
  badges.appendChild(cfg.gist_ready ? badge('Gist 就绪', 'ok') : badge('Gist 未就绪', 'muted'));
  badges.appendChild(cfg.webdav_ready ? badge('WebDAV 就绪', 'ok') : badge('WebDAV 未就绪', 'muted'));
  var pushTarget = $('#pub-push-target');
  if (pushTarget && cfg.substore_target_name) { pushTarget.textContent = cfg.substore_target_name; }
  renderPrecheck(res.body);
}

function kvRow(label, value){
  var row = el('div', null, null);
  row.appendChild(el('span', 'k', label));
  row.appendChild(el('span', null, value));
  return row;
}

function renderPrecheck(body){
  var panel = $('#pub-precheck');
  clear(panel);
  var diffs = (body && body.differences) || [];
  var schedule = (body && body.schedule) || {};
  var sources = (body && body.sources) || {};
  var summary = 'Sources：共 ' + (sources.total === undefined ? '—' : sources.total)
    + '，启用 ' + (sources.enabled === undefined ? '—' : sources.enabled)
    + '；调度：' + (schedule.enabled ? '每 ' + (schedule.interval_minutes === undefined ? '—' : schedule.interval_minutes) + ' 分钟' : '已关闭')
    + '（建议 ≥ 5 分钟）。';
  panel.appendChild(el('span', null, summary));
  if (diffs.length) {
    panel.appendChild(el('div', 'muted', '配置文件与运行中的进程存在差异：'));
    var list = el('ul', null, null);
    diffs.forEach(function(diff){ list.appendChild(el('li', null, diff)); });
    panel.appendChild(list);
  } else {
    panel.appendChild(el('div', 'muted', '配置文件与运行中的进程一致。'));
  }
}

function renderPublishMapping(sources){
  var body = $('#pub-mapping');
  clearRows(body);
  var gistConfigured = false;
  sources.forEach(function(source){
    var artifact = source.artifact || null;
    if (artifact && artifact.gist_id) { gistConfigured = true; }
    var tr = el('tr');
    tr.appendChild(el('td', null, source.display_name || source.key));
    tr.appendChild(el('td', 'mono', (artifact && artifact.gist_file) || '—'));
    tr.appendChild(el('td', 'mono', artifact && artifact.gist_id ? maskId(artifact.gist_id) : '—'));
    tr.appendChild(el('td', null, when(artifact && artifact.last_published_at)));
    tr.appendChild(el('td', 'mono', artifact && artifact.last_content_sha256 ? short8(artifact.last_content_sha256) : '—'));
    body.appendChild(tr);
  });
  if (!sources.length) {
    var tr = el('tr');
    tr.appendChild(el('td', null, '暂无 Source'));
    body.appendChild(tr);
  }
  var badges = $('#pub-badges');
  badges.appendChild(gistConfigured ? badge('Gist 已配置', 'ok') : badge('Gist 未配置', 'muted'));
}

function loadPublish(){
  Promise.all([api('GET', '/api/v1/config/validate'), api('GET', '/api/v1/sources')]).then(function(pair){
    renderPublishConfig(pair[0]);
    renderPublishMapping((pair[1].body && pair[1].body.sources) || []);
  }).catch(function(){ say('加载发布设置失败：网络错误。'); });
}

// 后续批次（B2/B5）的契约端点；404 表示尚未部署，前端如实显示「未启用」。
function publisherAction(path, label){
  var panel = $('#pub-result');
  setPanelText(panel, label + '：请求中…');
  api('POST', path, {}, true).then(function(res){
    if (res.status === 404) {
      setPanelText(panel, label + '未启用（404）：发布器端点将在后续批次提供。');
      return;
    }
    if (res.status === 401 || res.status === 403) {
      setPanelText(panel, label + '失败（' + res.status + '）：鉴权或 CSRF 未通过。');
      return;
    }
    if (!res.ok) {
      setPanelText(panel, errorNote(res, label));
      return;
    }
    // 探针端点恒答 200，真实结论在 body 的 mask 里（configured/ok/error_code）。
    var body = res.body || {};
    if (body.ok === true) {
      setPanelText(panel, label + '：目的地可达（ok）。');
    } else if (body.configured === false) {
      setPanelText(panel, label + '：未启用（未配置凭据或 URL，error_code ' + (body.error_code || 'config') + '）。');
    } else {
      setPanelText(panel, label + '：目的地应答异常（error_code ' + (body.error_code || '—') + '）。');
    }
  }).catch(function(){ setPanelText(panel, label + '：网络错误。'); });
}

// --- 人工推送三件套（G-4，workstreams/29） ----------------------------------

function pushPanel(){ return $('#pub-push-result'); }

function pushSay(text){ setPanelText(pushPanel(), text); }

function pushErrorNote(res, label){
  var code = (res.body && res.body.error) || '';
  var detail = (res.body && (res.body.message || res.body.detail)) || '';
  var suffix = code ? ('（' + code + (detail ? '：' + detail : '') + '）') : '';
  return label + '失败（' + res.status + '）' + suffix + '。';
}

function pushPreview(){
  pushSay('预览差异：请求中…');
  api('POST', '/api/v1/substore/preview', {}, true).then(function(res){
    if (!res.ok) { pushSay(pushErrorNote(res, '预览差异')); return; }
    var preview = (res.body && res.body.preview) || {};
    var missing = res.body && res.body.missing_target;
    pushSay('预览完成：目标 ' + res.body.target
      + '，digest ' + String(res.body.digest || '—').slice(0, 12)
      + (missing ? '（目标对象不存在——推送将被拒绝，绝不新建）' : '')
      + '。新增 ' + preview.added + '，移除 ' + preview.removed
      + '，改名 ' + preview.renamed + '，不变 ' + preview.unchanged + '。');
  }).catch(function(){ pushSay('预览差异：网络错误。'); });
}

function pushConfirm(){
  pushSay('签发确认单：请求中…');
  api('POST', '/api/v1/substore/confirm-token', {}, true).then(function(res){
    if (!res.ok) { pushSay(pushErrorNote(res, '签发确认单')); return; }
    pushSay('确认单已签发（' + res.body.expires_in + 's 内有效，绑定 digest '
      + String(res.body.digest || '—').slice(0, 12) + '）。点击「推送到 Sub-Store」完成。');
  }).catch(function(){ pushSay('签发确认单：网络错误。'); });
}

function pushExecute(){
  if (!window.confirm('即将把 canonical 节点列表 update-only 写入 Sub-Store 目标对象（绝不新建）。继续吗？')) {
    pushSay('已取消推送。');
    return;
  }
  pushSay('推送：请求中…');
  // 两段式：先签新确认单（绑定当前 digest），再立即消费——一次点击完成
  // 「确认单 → 推送」，但仍受 60s/一次性/绑定三重门禁约束。
  api('POST', '/api/v1/substore/confirm-token', {}, true).then(function(tokenRes){
    if (!tokenRes.ok) { pushSay(pushErrorNote(tokenRes, '签发确认单')); return; }
    return api('POST', '/api/v1/substore/push',
               { confirm_token: tokenRes.body.confirm_token }, true);
  }).then(function(res){
    if (!res) { return; }
    if (!res.ok) { pushSay(pushErrorNote(res, '推送')); return; }
    pushSay('推送成功：' + res.body.target + '，' + res.body.count
      + ' 节点（' + res.body.substore_action + '），digest '
      + String(res.body.digest || '—').slice(0, 12) + '。');
  }).catch(function(){ pushSay('推送：网络错误。'); });
}

// --- 清理中心 ---------------------------------------------------------------

function runMigration(path, label, body){
  var panel = $('#clean-result');
  setPanelText(panel, label + '：请求中…');
  api('POST', path, body === undefined ? {} : body, true).then(function(res){
    if (res.status === 404) {
      setPanelText(panel, label + '未启用（404）：迁移端点将在后续批次提供，本次未做任何更改。');
      return;
    }
    if (res.status === 401 || res.status === 403) {
      setPanelText(panel, label + '失败（' + res.status + '）：鉴权、CSRF 或删除门禁未通过，未做任何更改。');
      return;
    }
    if (!res.ok) {
      setPanelText(panel, errorNote(res, label) + '未做任何更改。');
      return;
    }
    // 删除成功的应答带 backup_file：记住它，回滚清理就用这一份。
    if (res.body && res.body.backup_file) { lastCleanupBackup = String(res.body.backup_file); }
    renderMigrationReport(res.body || {}, label);
  }).catch(function(){ setPanelText(panel, label + '：网络错误，未做任何更改。'); });
}

function deepCount(value, key){
  if (!value || typeof value !== 'object') { return null; }
  if (Object.prototype.hasOwnProperty.call(value, key) && typeof value[key] === 'number') {
    return value[key];
  }
  var keys = Object.keys(value);
  for (var i = 0; i < keys.length; i++) {
    var found = deepCount(value[keys[i]], key);
    if (found !== null) { return found; }
  }
  return null;
}

function renderMigrationReport(body, label){
  var panel = $('#clean-result');
  clear(panel);
  panel.appendChild(el('p', null, label + '结果：'));
  var row = el('div', 'row');
  [['keep', 'ok'], ['duplicate', 'warn'], ['uncertain', 'muted']].forEach(function(pair){
    var count = deepCount(body, pair[0]);
    row.appendChild(badge(pair[0] + ' ' + (count === null ? '—' : count), pair[1]));
  });
  panel.appendChild(row);
  panel.appendChild(el('pre', 'mono', JSON.stringify(body, null, 2)));
}

// --- init ------------------------------------------------------------------

function init(){
  initTabs();
  $('#enter').addEventListener('click', enter);
  $('#tok').addEventListener('keydown', function(ev){ if (ev.key === 'Enter') { enter(); } });
  $('#detail-source').addEventListener('change', function(){ loadDetail(null); });
  $('#detail-refresh').addEventListener('click', function(){ loadDetail(null); });
  $('#pub-test').addEventListener('click', function(){ publisherAction('/api/v1/publishers/gist/test', '测试连接 (Gist)'); });
  $('#pub-test-webdav').addEventListener('click', function(){ publisherAction('/api/v1/publishers/webdav/test', '测试连接 (WebDAV)'); });
  $('#pub-init').addEventListener('click', function(){
    if (!window.confirm('初始化 Gist 将创建并绑定发布对象（调用后续批次的契约端点）。继续吗？')) {
      say('已取消初始化 Gist。');
      return;
    }
    publisherAction('/api/v1/publishers/gist/init', '初始化 Gist');
  });
  $('#pub-push-preview').addEventListener('click', pushPreview);
  $('#pub-push-confirm').addEventListener('click', pushConfirm);
  $('#pub-push').addEventListener('click', pushExecute);
  $('#clean-scan').addEventListener('click', function(){ lastCleanupBackup = null; runMigration('/api/v1/migrations/substore/cleanup-plan', '扫描清理'); });
  $('#clean-apply').addEventListener('click', function(){
    if (!window.confirm('第一步确认：即将对旧 Sub-Store 测活产物（<prefix>-*-local、front-manual、ipmap 映射）执行删除。继续吗？')) {
      say('已取消删除（第一步确认）。');
      return;
    }
    if (!window.confirm('第二步确认：删除前会备份到本地，但删除本身需要部署侧打开 substore.allow_delete。确定要执行吗？')) {
      say('已取消删除（第二步确认）。');
      return;
    }
    runMigration('/api/v1/migrations/substore/cleanup-apply', '删除测活产物', { execute: true });
  });
  $('#clean-rollback').addEventListener('click', function(){
    if (!lastCleanupBackup) {
      say('还没有可回滚的备份：先执行一次「删除测活产物」。');
      return;
    }
    if (!window.confirm('回滚将按最近一次删除的备份（' + lastCleanupBackup + '）还原这些对象。继续吗？')) {
      say('已取消回滚。');
      return;
    }
    runMigration('/api/v1/migrations/substore/cleanup-rollback', '回滚清理', { backup: lastCleanupBackup });
  });
  Array.prototype.forEach.call(document.querySelectorAll('[data-placeholder]'), function(btn){
    btn.addEventListener('click', function(){
      say(btn.getAttribute('data-placeholder') + '功能将在后续批次提供，当前不可用。');
    });
  });
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', init);
} else {
  init();
}
})();
</script>
</body>
</html>"#;

#[cfg(test)]
#[path = "gui_tests.rs"]
mod gui_tests;
