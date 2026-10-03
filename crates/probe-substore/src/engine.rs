//! The embedded Sub-Store JS engine.
//!
//! Architecture (ported from subs-check-pro's `loon_engine.go` + `init.js`,
//! restructured for rquickjs's async model):
//!
//! * One long-lived QuickJS runtime/context per [`Engine`]. The Go version
//!   builds a fresh runtime per request because quickjs-go has no safe way to
//!   re-enter a busy runtime; here rquickjs's `AsyncContext` serializes entry
//!   for us, and the bundle runs inside `__run_sub_store_script()` whose
//!   function scope resets the bundle's top-level state per call. Requests are
//!   additionally serialized by [`Engine::execute`] holding a dispatch mutex
//!   for the whole request lifetime, so two requests can never interleave
//!   bundle state. Trade-off: one request at a time; concurrent *downloads*
//!   inside a request still fan out through the HTTP bridge.
//! * The host supplies the Loon surface (`$httpClient`, `$persistentStore`,
//!   `$done`, ...) as Rust-backed globals. `$httpClient` calls spawn real
//!   reqwest tasks that re-enter the context to deliver responses through
//!   `__dispatch_http_response`; the current request's side-channel state
//!   (bodies registry, $done channel) rides in a `task_local` scope so stale
//!   callbacks from an earlier request can never cross into a later one.
//! * A runtime interrupt handler enforces a hard per-request deadline: a
//!   runaway synchronous loop is torn down instead of hanging the service
//!   forever (the Go version could only leak its goroutine there).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use rquickjs::prelude::Opt;
use rquickjs::CatchResultExt;
use rquickjs::{AsyncContext, AsyncRuntime, Function, Promise};
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::init_js;
use crate::kv::KvStore;

/// Hard deadline for one request's JS execution; the interrupt handler tears
/// the script down at this point (seconds).
const SCRIPT_DEADLINE_MS: i64 = 170_000;
/// Belt-and-braces outer timeout, slightly past the deadline (seconds).
const OUTER_TIMEOUT: Duration = Duration::from_secs(190);
const MAX_RESPONSE_BODY: usize = 64 << 20;

tokio::task_local! {
    static CURRENT: Arc<Scope>;
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("quickjs: {0}")]
    Js(#[from] rquickjs::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("script: {0}")]
    Script(String),
    #[error("script timed out")]
    Timeout,
    #[error("base64: {0}")]
    Base64(#[from] base64::DecodeError),
}

/// Incoming request, shaped exactly like subs-check-pro's `LoonHTTPRequest`.
#[derive(Debug, Clone)]
pub struct LoonRequest {
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// `$done` result, shaped like `LoonHTTPResponse`.
#[derive(Debug, Clone)]
pub struct LoonResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// Per-request side channel state, reachable from every binding via
/// [`CURRENT`]. Fresh per `execute` call.
struct Scope {
    bodies: Mutex<HashMap<String, String>>,
    body_counter: AtomicU64,
    done: Mutex<Option<oneshot::Sender<(String, String)>>>,
    closed: AtomicBool,
}

struct BridgeClients {
    follow: reqwest::Client,
    direct: reqwest::Client,
}

pub struct Engine {
    _rt: Arc<AsyncRuntime>,
    ctx: AsyncContext,
    dispatch: tokio::sync::Mutex<()>,
    deadline_ms: Arc<AtomicI64>,
    banner_printed: Arc<AtomicBool>,
    clients: Arc<BridgeClients>,
    /// When non-zero, replaces the default per-request deadline (test hook;
    /// always zero in production for now).
    deadline_override: AtomicI64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HttpOpts {
    url: String,
    headers: HashMap<String, String>,
    body: Option<String>,
    #[serde(alias = "bodyBase64")]
    body_base64: bool,
    timeout: Option<u64>,
    #[serde(alias = "redirection")]
    auto_redirect: Option<bool>,
    #[serde(rename = "binary-mode")]
    binary_mode: bool,
}

/// Extract the JS exception text (message + stack) from a QuickJS error so
/// bundle failures read like JS stack traces instead of a bare "Exception".
fn js_error(ctx: &rquickjs::Ctx<'_>, err: rquickjs::Error) -> EngineError {
    let caught = Err::<(), _>(err).catch(ctx);
    match caught {
        Err(rquickjs::CaughtError::Exception(e)) => EngineError::Script(format!(
            "{} | {}",
            e.message().unwrap_or_else(|| "?".into()),
            e.stack().unwrap_or_default()
        )),
        Err(other) => EngineError::Script(other.to_string()),
        Ok(()) => EngineError::Script("js error vanished".into()),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Engine {
    pub async fn new(
        bundle_src: &str,
        kv: Arc<dyn KvStore>,
        push_service: Option<String>,
    ) -> Result<Self, EngineError> {
        let rt = Arc::new(AsyncRuntime::new()?);
        rt.set_memory_limit(512 * 1024 * 1024).await;
        rt.set_max_stack_size(16 * 1024 * 1024).await;
        let deadline_ms = Arc::new(AtomicI64::new(i64::MAX));
        {
            let deadline_ms = deadline_ms.clone();
            rt.set_interrupt_handler(Some(Box::new(move || {
                now_ms() > deadline_ms.load(Ordering::Relaxed)
            })))
            .await;
        }
        let ctx = AsyncContext::full(&rt).await?;

        let clients = Arc::new(BridgeClients {
            follow: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(10))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| EngineError::Script(format!("http client: {e}")))?,
            direct: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| EngineError::Script(format!("http client: {e}")))?,
        });

        let engine = Self {
            _rt: rt,
            ctx: ctx.clone(),
            dispatch: tokio::sync::Mutex::new(()),
            deadline_ms,
            banner_printed: Arc::new(AtomicBool::new(false)),
            clients,
            deadline_override: AtomicI64::new(0),
        };

        let init = init_js::build_init_script(bundle_src);
        // Cloned up front so the async closure captures only these handles,
        // never `engine` itself (it is returned to the caller below).
        let kv = kv.clone();
        let push_service = push_service.clone();
        let banner_printed = engine.banner_printed.clone();
        let clients = engine.clients.clone();
        // The Ctx param is a borrowed handle scoped to this call; anything
        // escaping into spawned tasks must hold the owned AsyncContext.
        let ctx_for_http = engine.ctx.clone();
        let ctx_for_timer = engine.ctx.clone();
        ctx.async_with(async move |ctx| {
            let globals = ctx.globals();

            globals.set(
                "__done",
                Function::new(ctx.clone(), move |meta: String, body: String| {
                    let _ = CURRENT.try_with(|scope| {
                        if let Some(tx) = scope.done.lock().unwrap().take() {
                            let _ = tx.send((meta.clone(), body.clone()));
                        }
                    });
                }),
            )?;

            globals.set(
                "__ps_read",
                Function::new(ctx.clone(), {
                    let kv = kv.clone();
                    move |key: String| -> Option<String> { kv.read(&key) }
                }),
            )?;

            globals.set(
                "__ps_write",
                Function::new(ctx.clone(), {
                    let kv = kv.clone();
                    move |value: Opt<String>, key: String| {
                        kv.write(value.0, &key);
                    }
                }),
            )?;

            globals.set(
                "__notify_post",
                Function::new(ctx.clone(), {
                    let push_service = push_service.clone();
                    move |title: String, subtitle: String, content: String| {
                        tracing::info!(title, subtitle, content, "sub-store notification");
                        if let Some(cfg) = push_service.as_deref() {
                            if !cfg.is_empty() {
                                push_via_get(cfg, &title, &subtitle, &content);
                            }
                        }
                    }
                }),
            )?;

            globals.set(
                "__console_log",
                Function::new(ctx.clone(), {
                    let banner_printed = banner_printed.clone();
                    move |level: String, msg: String| {
                        if msg.contains("┅┅┅┅") || msg.contains("Sub-Store -- v") {
                            if banner_printed
                                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok()
                            {
                                tracing::info!("{msg}");
                            }
                            return;
                        }
                        if msg.contains("[CORS] allowed origins:") {
                            return;
                        }
                        match level.as_str() {
                            "info" => tracing::info!("{msg}"),
                            "warn" => tracing::warn!("{msg}"),
                            "error" => tracing::error!("{msg}"),
                            _ => tracing::debug!("{msg}"),
                        }
                    }
                }),
            )?;

            globals.set(
                "__go_http_request_async",
                Function::new(ctx.clone(), {
                    let clients = clients.clone();
                    let ctx = ctx_for_http.clone();
                    move |method: String, opts: String, js_req_id: String| {
                        let Ok(scope) = CURRENT.try_with(|s| s.clone()) else {
                            return;
                        };
                        let clients = clients.clone();
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            execute_http(scope, clients, ctx, method, opts, js_req_id).await;
                        });
                    }
                }),
            )?;

            globals.set(
                "__go_http_request_body",
                Function::new(ctx.clone(), move |req_id: String| -> Option<String> {
                    CURRENT
                        .try_with(|scope| scope.bodies.lock().unwrap().remove(&req_id))
                        .ok()
                        .unwrap_or(None)
                }),
            )?;

            globals.set(
                "__host_timer_set",
                Function::new(ctx.clone(), {
                    let ctx = ctx_for_timer.clone();
                    move |id: f64, ms: f64| {
                        let Ok(scope) = CURRENT.try_with(|s| s.clone()) else {
                            return;
                        };
                        let ctx = ctx.clone();
                        let ms = ms.clamp(0.0, 300_000.0) as u64;
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                            if scope.closed.load(Ordering::SeqCst) {
                                return;
                            }
                            let code = format!("__timer_fire({})", id as u64);
                            let _ = CURRENT
                                .scope(scope.clone(), async {
                                    ctx.async_with(async move |ctx| {
                                        let _: () = ctx.eval(code.as_str())?;
                                        Ok::<(), rquickjs::Error>(())
                                    })
                                    .await
                                })
                                .await;
                        });
                    }
                }),
            )?;

            globals.set(
                "__rand_bytes",
                Function::new(ctx.clone(), move |n: usize| -> String {
                    let n = n.min(65_536);
                    let mut buf = vec![0u8; n];
                    if getrandom::fill(&mut buf).is_err() {
                        return String::new();
                    }
                    base64::engine::general_purpose::STANDARD.encode(buf)
                }),
            )?;

            let _: () = ctx.eval(init.as_str())?;
            Ok::<(), EngineError>(())
        })
        .await?;

        Ok(engine)
    }

    /// Run one request through the bundle. Requests are serialized; see the
    /// module docs for why and what that costs.
    pub async fn execute(
        &self,
        req: &LoonRequest,
        argument: &str,
    ) -> Result<LoonResponse, EngineError> {
        let _guard = self.dispatch.lock().await;
        let (tx, rx) = oneshot::channel::<(String, String)>();
        let scope = Arc::new(Scope {
            bodies: Mutex::new(HashMap::new()),
            body_counter: AtomicU64::new(0),
            done: Mutex::new(Some(tx)),
            closed: AtomicBool::new(false),
        });
        let override_ms = self.deadline_override.load(Ordering::Relaxed);
        self.deadline_ms.store(
            if override_ms > 0 {
                override_ms
            } else {
                now_ms() + SCRIPT_DEADLINE_MS
            },
            Ordering::Relaxed,
        );

        let req_json = request_map(req).to_string();
        let argument = argument.to_string();
        let ctx = self.ctx.clone();

        let js_result = tokio::time::timeout(
            OUTER_TIMEOUT,
            CURRENT.scope(scope.clone(), async {
                ctx.async_with(async move |ctx| {
                    let globals = ctx.globals();
                    globals.set("__req_json", req_json)?;
                    globals.set("__arg_str", argument)?;
                    // The promise resolves when $done calls __resolve_done
                    // (JS side), rejects when the bundle throws, or rejects
                    // via the in-JS 179s guard; the interrupt handler is the
                    // last-resort teardown for loops that never await.
                    let promise: Promise = ctx
                        .eval(init_js::runner_script())
                        .map_err(|e| js_error(&ctx, e))?;
                    promise
                        .into_future::<()>()
                        .await
                        .map_err(|e| js_error(&ctx, e))?;
                    Ok::<(), EngineError>(())
                })
                .await
            }),
        )
        .await;

        scope.closed.store(true, Ordering::SeqCst);
        self.deadline_ms.store(i64::MAX, Ordering::Relaxed);

        match js_result {
            Err(_elapsed) => Err(EngineError::Timeout),
            Ok(Ok(())) => match rx.await {
                Ok((meta, body)) => Ok(parse_loon_response(&meta, &body)),
                Err(_) => Err(EngineError::Script("finished without calling $done".into())),
            },
            Ok(Err(err)) => Err(err),
        }
    }
}

fn request_map(req: &LoonRequest) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("url".into(), serde_json::Value::String(req.url.clone()));
    m.insert(
        "method".into(),
        serde_json::Value::String(req.method.clone()),
    );
    m.insert(
        "headers".into(),
        serde_json::to_value(&req.headers).unwrap_or_default(),
    );
    if !req.body.is_empty() {
        m.insert("body".into(), serde_json::Value::String(req.body.clone()));
    }
    serde_json::Value::Object(m)
}

async fn execute_http(
    scope: Arc<Scope>,
    clients: Arc<BridgeClients>,
    ctx: AsyncContext,
    method: String,
    opts_raw: String,
    js_req_id: String,
) {
    let opts: HttpOpts = serde_json::from_str(&opts_raw).unwrap_or_default();
    if opts.url.is_empty() {
        deliver(
            scope,
            &ctx,
            &js_req_id,
            &serde_json::json!({"error": "empty url"}),
        )
        .await;
        return;
    }
    let timeout_ms = opts.timeout.unwrap_or(30_000).min(300_000);
    let client = if opts.auto_redirect.unwrap_or(true) {
        &clients.follow
    } else {
        &clients.direct
    };

    let mut request = client
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET),
            &opts.url,
        )
        .timeout(Duration::from_millis(timeout_ms));
    for (k, v) in &opts.headers {
        request = request.header(k, v);
    }
    if let Some(body) = &opts.body {
        if !body.is_empty() {
            if opts.body_base64 {
                match base64::engine::general_purpose::STANDARD.decode(body) {
                    Ok(bytes) => {
                        request = request.body(bytes);
                    }
                    Err(_) => {
                        deliver(
                            scope,
                            &ctx,
                            &js_req_id,
                            &serde_json::json!({"error": "invalid base64 body"}),
                        )
                        .await;
                        return;
                    }
                }
            } else {
                request = request.body(body.clone());
            }
        }
    }

    let meta = match request.send().await {
        Err(err) => serde_json::json!({"error": err.to_string()}),
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut headers = HashMap::new();
            for (name, value) in resp.headers() {
                if let Ok(v) = value.to_str() {
                    headers
                        .entry(name.as_str().to_lowercase())
                        .or_insert_with(|| v.to_string());
                }
            }
            // Bound the read even when the server lies about content-length.
            let mut body: Vec<u8> = Vec::new();
            let mut over_limit = false;
            let mut stream = resp;
            while let Some(chunk) = match stream.chunk().await {
                Ok(Some(chunk)) => Some(chunk),
                Ok(None) => None,
                Err(err) => {
                    deliver(
                        scope,
                        &ctx,
                        &js_req_id,
                        &serde_json::json!({"error": err.to_string()}),
                    )
                    .await;
                    return;
                }
            } {
                if body.len() + chunk.len() > MAX_RESPONSE_BODY {
                    over_limit = true;
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            let _ = over_limit;
            let body_str = if opts.binary_mode {
                headers.insert("__is_binary__".into(), "true".into());
                base64::engine::general_purpose::STANDARD.encode(&body)
            } else {
                String::from_utf8_lossy(&body).into_owned()
            };
            let go_req_id = format!(
                "go-{}",
                scope.body_counter.fetch_add(1, Ordering::Relaxed) + 1
            );
            scope
                .bodies
                .lock()
                .unwrap()
                .insert(go_req_id.clone(), body_str);
            serde_json::json!({"response": {"status": status, "headers": headers}, "reqId": go_req_id})
        }
    };

    deliver(scope, &ctx, &js_req_id, &meta).await;
}

async fn deliver(scope: Arc<Scope>, ctx: &AsyncContext, js_req_id: &str, meta: &serde_json::Value) {
    if scope.closed.load(Ordering::SeqCst) {
        return;
    }
    // subs-check-pro passes the meta as a STRING literal (strconv.Quote) —
    // init.js runs JSON.parse on it. Passing an object literal instead makes
    // JSON.parse stringify it to "[object Object]" and throw a SyntaxError.
    let meta_text = meta.to_string();
    let meta_literal = serde_json::to_string(&meta_text).unwrap_or_else(|_| "\"{}".into());
    let code = format!(
        "__dispatch_http_response({}, {});",
        serde_json::to_string(js_req_id).unwrap_or_else(|_| "\"\"".into()),
        meta_literal
    );
    let _ = CURRENT
        .scope(scope.clone(), async {
            ctx.async_with(async move |ctx| {
                let _: () = ctx.eval(code.as_str())?;
                Ok::<(), rquickjs::Error>(())
            })
            .await
        })
        .await;
}

fn push_via_get(cfg: &str, title: &str, subtitle: &str, content: &str) {
    // Same GET-replace contract subs-check-pro supports for Bark-style URLs:
    // [推送标题]/[推送内容] placeholders, subtitle folded into the content.
    if !cfg.contains("[推送内容]") {
        return;
    }
    let push_msg = if subtitle.is_empty() {
        content.to_string()
    } else {
        format!("{subtitle}  \n{content}")
    };
    let url = cfg
        .replacen("[推送标题]", &percent_encode(title), 1)
        .replacen("[推送内容]", &percent_encode(&push_msg), 1);
    tokio::spawn(async move {
        match reqwest::get(&url).await {
            Ok(resp) => {
                let _ = resp.bytes().await;
            }
            Err(err) => tracing::warn!("sub-store push failed: {err}"),
        }
    });
}

/// Percent-encode everything outside the RFC 3986 unreserved set (Go's
/// `url.QueryEscape` equivalent for the `cors=` argument).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parse `$done`'s meta: `{response:{status|statusCode,headers}}` or the flat
/// form, defaulting to 200 (mirrors subs-check-pro's `parseLoonResponse`).
fn parse_loon_response(meta: &str, body: &str) -> LoonResponse {
    let mut status = 200u16;
    let mut headers = HashMap::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(meta) {
        let resp = value.get("response").unwrap_or(&value);
        let status_val = resp.get("status").or_else(|| resp.get("statusCode"));
        if let Some(s) = status_val.and_then(|v| v.as_u64()) {
            status = s.min(u16::MAX as u64) as u16;
        }
        if let Some(map) = resp.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let Some(v) = v.as_str() {
                    headers.insert(k.clone(), v.to_string());
                }
            }
        }
    }
    LoonResponse {
        status,
        headers,
        body: body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::JsonKvStore;
    use std::path::PathBuf;
    use std::sync::Once;

    static DIR_SEQ: AtomicU64 = AtomicU64::new(0);
    static LOG: Once = Once::new();

    fn test_logging() {
        LOG.call_once(|| {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .try_init()
                .ok();
        });
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "probe-substore-engine-{tag}-{}-{}",
            std::process::id(),
            DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn mini_engine(bundle: &str) -> Engine {
        test_logging();
        let dir = temp_dir("ctx");
        let kv = JsonKvStore::new_arc(&dir).unwrap();
        Engine::new(bundle, kv, None).await.unwrap()
    }

    fn loon_get(url: &str) -> LoonRequest {
        LoonRequest {
            url: url.into(),
            method: "GET".into(),
            headers: HashMap::new(),
            body: String::new(),
        }
    }

    #[tokio::test]
    async fn echoes_done_response() {
        let engine = mini_engine(
            r#"$done({response:{status:201, headers:{"x-t":"v"}, body:"ok " + $request.url}});"#,
        )
        .await;
        let resp = engine
            .execute(&loon_get("http://x.test/a?b=1"), "cors=http%3A%2F%2Fx.test")
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.headers.get("x-t").map(String::as_str), Some("v"));
        assert_eq!(resp.body, "ok http://x.test/a?b=1");
    }

    #[tokio::test]
    async fn script_error_surfaces() {
        let engine = mini_engine(r#"throw new Error("boom");"#).await;
        let err = engine
            .execute(&loon_get("http://x.test/"), "")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "unexpected: {err}");
    }

    #[tokio::test]
    async fn persistent_store_roundtrip() {
        let engine = mini_engine(
            r#"
            const before = $persistentStore.read("probe-key");
            $persistentStore.write("val-" + (before || "empty"), "probe-key");
            $done({status: 200, body: $persistentStore.read("probe-key") || ""});
            "#,
        )
        .await;
        let first = engine
            .execute(&loon_get("http://x.test/"), "")
            .await
            .unwrap();
        assert_eq!(first.body, "val-empty");
        // A fresh request sees the value written by the previous one and
        // stacks on top of it.
        let second = engine
            .execute(&loon_get("http://x.test/"), "")
            .await
            .unwrap();
        assert_eq!(second.body, "val-val-empty");
    }

    #[tokio::test]
    async fn http_bridge_roundtrip() {
        // Local server proves the $httpClient -> reqwest -> callback loop.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = axum::Router::new().fallback(|| async { "bridge-ok" });
            axum::serve(listener, app).await.unwrap();
        });
        let engine = mini_engine(&format!(
            r#"
            $httpClient.get({{url: "http://{addr}/ping"}}, function (err, resp, body) {{
                if (err) {{ $done({{status: 500, body: String(err)}}); return; }}
                $done({{status: resp.status, body: String(body).toUpperCase()}});
            }});
            "#
        ))
        .await;
        let resp = engine
            .execute(&loon_get("http://x.test/"), "")
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {}", resp.body);
        assert_eq!(resp.body, "BRIDGE-OK");
    }

    #[tokio::test]
    async fn url_and_btoa_available() {
        let engine = mini_engine(
            r#"
            const u = new URL("https://user:pw@example.test:8443/p?q=1#f");
            const v6 = new URL("udp://[::1]:443/x");
            const rel = new URL("../c", "http://a.test/x/y");
            const def = new URL("https://a.test/p");
            const vless = new URL("vless://uuid@a.test:443?type=ws&security=tls");
            const sp = new URL("https://a.test/?b=2&a=1").searchParams;
            $done({status: 200, body: [
                u.protocol, u.hostname, u.port, u.pathname, u.search, u.origin, u.username,
                v6.hostname, v6.port,
                rel.href,
                def.port === "" ? "no-port" : def.port,
                vless.protocol, vless.hostname, vless.searchParams.get("security"),
                sp.get("a"), sp.get("b"),
                btoa("hi"), typeof TextEncoder
            ].join("|")});
            "#,
        )
        .await;
        let resp = engine
            .execute(&loon_get("http://x.test/"), "")
            .await
            .unwrap();
        assert_eq!(
            resp.body,
            "https:|example.test|8443|/p|?q=1|https://example.test:8443|user|[::1]|443|http://a.test/c|no-port|vless:|a.test|tls|1|2|aGk=|function"
        );
    }

    #[tokio::test]
    async fn runaway_loop_is_interrupted() {
        let engine = mini_engine(r#"while (true) {}"#).await;
        // Tighten the deadline so the interrupt handler tears the loop down
        // within test time instead of the production 170s.
        engine.deadline_override.store(1_500, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let _err = engine
            .execute(&loon_get("http://x.test/"), "")
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "interrupt took too long: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn timers_fire() {
        let engine = mini_engine(
            r#"
            let fired = false;
            setTimeout(function () { fired = true; $done({status: 200, body: fired ? "timer" : "no"}); }, 30);
            "#,
        )
        .await;
        let resp = engine
            .execute(&loon_get("http://x.test/"), "")
            .await
            .unwrap();
        assert_eq!(resp.body, "timer");
    }

    // ---- real bundle ----

    /// The real vendored bundle must boot and answer /api/utils/env with no
    /// outbound network. This is the shim-stack acceptance test: any missing
    /// Loon global shows up here.
    #[tokio::test]
    async fn real_bundle_answers_env() {
        let dir = temp_dir("real");
        let kv = JsonKvStore::new_arc(&dir).unwrap();
        let engine = Engine::new(crate::assets::EMBEDDED_BUNDLE, kv, None)
            .await
            .expect("bundle init");
        let resp = tokio::time::timeout(
            Duration::from_secs(120),
            engine.execute(
                &loon_get("http://127.0.0.1:9/api/utils/env"),
                "cors=http%3A%2F%2Flocalhost",
            ),
        )
        .await
        .expect("env endpoint timed out")
        .expect("env endpoint failed");
        assert_eq!(
            resp.status,
            200,
            "body: {}",
            &resp.body[..resp.body.len().min(500)]
        );
        assert!(
            resp.body.contains("version"),
            "env json missing version: {}",
            &resp.body[..resp.body.len().min(500)]
        );
    }
}
