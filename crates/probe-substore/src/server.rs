//! The axum face of the embedded Sub-Store: CORS everywhere, a secret-path
//! routed backend dispatching into the JS engine, `/download/*` request
//! coalescing, the `/api/utils/env` metadata rewrite, and static frontend
//! serving with SPA fallback. Ported from subs-check-pro's `loon_server.go`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use tokio::sync::watch;

use crate::engine::{Engine, LoonRequest, LoonResponse};

const MAX_BACKEND_BODY: usize = 30 << 20;
type SharedResult = Arc<Result<LoonResponse, String>>;

pub struct SubStoreState {
    pub engine: Arc<Engine>,
    /// Secret URL prefix that exposes the backend API, e.g. `/3f9c...`.
    /// Empty disables backend routing (frontend-only mode).
    pub backend_path: String,
    pub frontend_dir: std::path::PathBuf,
    pub backend_name: String,
    /// Extra `SUB_STORE_*` keys injected into `/api/utils/env` (cron lines,
    /// push service) so the frontend reflects the deployment; the Go version
    /// injects the same set.
    pub env_extras: Vec<(String, String)>,
    pub(crate) inflight:
        tokio::sync::Mutex<HashMap<String, Arc<watch::Sender<Option<SharedResult>>>>>,
}

impl SubStoreState {
    pub fn new(
        engine: Arc<Engine>,
        backend_path: String,
        frontend_dir: std::path::PathBuf,
        backend_name: impl Into<String>,
    ) -> Self {
        Self {
            engine,
            backend_path,
            frontend_dir,
            backend_name: backend_name.into(),
            env_extras: Vec::new(),
            inflight: tokio::sync::Mutex::default(),
        }
    }
}

pub fn router(state: Arc<SubStoreState>) -> Router {
    Router::new()
        .route("/__substore_health", any(|| async { "ok" }))
        .fallback(handle_all)
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

async fn cors(req: Request, next: Next) -> Response {
    let allow_headers = req
        .headers()
        .get("access-control-request-headers")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if req.method() == Method::OPTIONS {
        // Chrome's Private Network Access: a public page (the official
        // frontend) probing a LAN/local backend must get an explicit opt-in.
        let pna = req
            .headers()
            .get("access-control-request-private-network")
            .and_then(|v| v.to_str().ok())
            == Some("true");
        let mut res = Response::new(Body::empty());
        apply_cors(&mut res, allow_headers.as_deref(), pna);
        res.headers_mut()
            .insert("access-control-max-age", HeaderValue::from_static("600"));
        *res.status_mut() = StatusCode::NO_CONTENT;
        return res;
    }
    let mut res = next.run(req).await;
    apply_cors(&mut res, allow_headers.as_deref(), false);
    res
}

fn apply_cors(res: &mut Response, allow_headers: Option<&str>, pna: bool) {
    let headers = res.headers_mut();
    headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS"),
    );
    // Reflect whatever the preflight declares: hard-coded allow-lists get
    // rejected by browsers when the official frontend sends custom headers.
    let reflected = allow_headers
        .map(|s| s.to_string())
        .unwrap_or_else(|| "Origin, X-Requested-With, Content-Type, Accept, Authorization".into());
    if let Ok(value) = HeaderValue::from_str(&reflected) {
        headers.insert("access-control-allow-headers", value);
    }
    if pna {
        headers.insert(
            "access-control-allow-private-network",
            HeaderValue::from_static("true"),
        );
    }
}

async fn handle_all(State(state): State<Arc<SubStoreState>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h).to_string());

    let mut is_backend = false;
    if !state.backend_path.is_empty() && path.starts_with(&state.backend_path) {
        is_backend = true;
    }
    // Rule B: LAN hijack of the sub.store domain keeps working behind proxies.
    if host.as_deref() == Some("sub.store")
        && (path.starts_with("/api/") || path.starts_with("/download/"))
    {
        is_backend = true;
    }

    if is_backend {
        backend(state, req, &path).await
    } else {
        frontend(state, &path).await
    }
}

async fn backend(state: Arc<SubStoreState>, req: Request, path: &str) -> Response {
    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, MAX_BACKEND_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return text(StatusCode::BAD_REQUEST, "body too large or unreadable"),
    };

    let mut headers = HashMap::new();
    for (name, value) in parts.headers.iter() {
        if let Ok(v) = value.to_str() {
            headers
                .entry(name.as_str().to_string())
                .and_modify(|existing: &mut String| {
                    existing.push_str(", ");
                    existing.push_str(v);
                })
                .or_insert_with(|| v.to_string());
        }
    }

    let mut stripped = path
        .strip_prefix(&state.backend_path)
        .unwrap_or(path)
        .to_string();
    if !stripped.starts_with('/') {
        stripped.insert(0, '/');
    }

    let scheme = if parts
        .headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        == Some("https")
    {
        "https"
    } else {
        "http"
    };
    let host = parts
        .headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    let mut full_url = format!("{scheme}://{host}{stripped}");
    if let Some(query) = parts.uri.query() {
        full_url.push('?');
        full_url.push_str(query);
    }

    let origin = parts
        .headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{scheme}://{host}"));
    let argument = format!("cors={}", crate::engine::percent_encode(&origin));

    let loon_req = LoonRequest {
        url: full_url.clone(),
        method: parts.method.to_string(),
        headers,
        body: String::from_utf8_lossy(&body_bytes).into_owned(),
    };

    // /download/* is idempotent and slow: coalesce identical requests and let
    // the script finish even if the first client disconnects (the client's
    // pull timeout is often shorter than a big subscription's build time).
    let detach =
        matches!(parts.method, Method::GET | Method::HEAD) && stripped.starts_with("/download/");
    let result = if detach {
        let ua = parts
            .headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        detached_execute(
            &state,
            &format!("{full_url}|{ua}|{argument}"),
            loon_req,
            argument,
        )
        .await
    } else {
        state
            .engine
            .execute(&loon_req, &argument)
            .await
            .map_err(|e| e.to_string())
    };

    let resp = match result {
        Ok(resp) => resp,
        Err(err) => {
            tracing::error!(url = %full_url, error = %err, "sub-store script failed");
            return text(StatusCode::INTERNAL_SERVER_ERROR, err);
        }
    };

    if full_url.contains("/api/utils/env") {
        return env_response(resp, &state);
    }

    let mut builder =
        Response::builder().status(StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK));
    for (k, v) in &resp.headers {
        let lower = k.to_lowercase();
        if lower == "content-length" || lower == "transfer-encoding" {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(lower.as_str()),
            HeaderValue::from_str(v),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from(resp.body))
        .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "response build failed"))
}

/// Execute detached from the client connection with request coalescing keyed
/// on method|url|UA|cors (UA matters: different clients get different
/// generated subscriptions).
async fn detached_execute(
    state: &Arc<SubStoreState>,
    key: &str,
    req: LoonRequest,
    argument: String,
) -> Result<LoonResponse, String> {
    let mut rx = {
        let mut map = state.inflight.lock().await;
        match map.get(key) {
            Some(tx) => tx.subscribe(),
            None => {
                let (tx, _) = watch::channel(None);
                let tx = Arc::new(tx);
                map.insert(key.to_string(), tx.clone());
                let rx = tx.subscribe();
                let engine = state.engine.clone();
                let key = key.to_string();
                let state2 = state.clone();
                tokio::spawn(async move {
                    let result = Arc::new(
                        engine
                            .execute(&req, &argument)
                            .await
                            .map_err(|e| e.to_string()),
                    );
                    let _ = tx.send(Some(result));
                    let mut map = state2.inflight.lock().await;
                    if map
                        .get(&key)
                        .map(|current| Arc::ptr_eq(current, &tx))
                        .unwrap_or(false)
                    {
                        map.remove(&key);
                    }
                });
                rx
            }
        }
    };
    let outcome = {
        let awaited = rx.wait_for(|value| value.is_some()).await;
        match awaited {
            Ok(value) => {
                let shared = (*value).clone().unwrap();
                (*shared).clone()
            }
            Err(_) => Err("coalesced request was dropped".to_string()),
        }
    };
    outcome
}

/// Rewrite `/api/utils/env` so the frontend learns our backend path and
/// branding (subs-check-pro injects the same keys into the same JSON).
fn env_response(resp: LoonResponse, state: &SubStoreState) -> Response {
    let mut body = resp.body.clone();
    if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&resp.body) {
        // The script-engine bundle reports `meta.loon` instead of the Node
        // backend's `meta.node`; create the node.env chain explicitly (same
        // as subs-check-pro) so the frontend finds our keys regardless.
        let obj = match root.as_object_mut() {
            Some(o) => o,
            None => return response_with_body(resp.status, body),
        };
        let data = obj.entry("data").or_insert_with(|| serde_json::json!({}));
        let data_obj = match data.as_object_mut() {
            Some(o) => o,
            None => return response_with_body(resp.status, body),
        };
        let meta = data_obj
            .entry("meta")
            .or_insert_with(|| serde_json::json!({}));
        let meta_obj = match meta.as_object_mut() {
            Some(o) => o,
            None => return response_with_body(resp.status, body),
        };
        let node = meta_obj
            .entry("node")
            .or_insert_with(|| serde_json::json!({}));
        let node_obj = match node.as_object_mut() {
            Some(o) => o,
            None => return response_with_body(resp.status, body),
        };
        let env = node_obj
            .entry("env")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(env) = env.as_object_mut() {
            env.insert(
                "SUB_STORE_BACKEND_CUSTOM_NAME".into(),
                serde_json::Value::String(state.backend_name.clone()),
            );
            env.insert(
                "SUB_STORE_FRONTEND_BACKEND_PATH".into(),
                serde_json::Value::String(state.backend_path.clone()),
            );
            env.insert(
                "SUB_STORE_BODY_JSON_LIMIT".into(),
                serde_json::Value::String("30mb".into()),
            );
            env.insert(
                "SUB_STORE_CORS_ALLOWED_ORIGINS".into(),
                serde_json::Value::String("*".into()),
            );
            for (key, value) in &state.env_extras {
                env.insert(key.clone(), serde_json::Value::String(value.clone()));
            }
            if let Ok(pretty) = serde_json::to_string(&root) {
                body = pretty;
            }
        }
    }
    response_with_body(resp.status, body)
}

fn response_with_body(status: u16, body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    response
}

async fn frontend(state: Arc<SubStoreState>, path: &str) -> Response {
    let root = &state.frontend_dir;
    if !root.exists() {
        return text(
            StatusCode::SERVICE_UNAVAILABLE,
            "sub-store frontend not downloaded yet; the API under the backend path still works",
        );
    }
    let rel = path.trim_start_matches('/');
    if rel.contains("..") {
        return text(StatusCode::FORBIDDEN, "illegal path");
    }
    let candidate = if rel.is_empty() {
        None
    } else {
        Some(root.join(rel))
    };

    let serve_file = |file: std::path::PathBuf| async move {
        match tokio::fs::read(&file).await {
            Ok(bytes) => {
                let mime = mime_for(&file);
                let mut res = Response::new(Body::from(bytes));
                if let Ok(value) = HeaderValue::from_str(&mime) {
                    res.headers_mut().insert("content-type", value);
                }
                res.headers_mut().insert(
                    "cache-control",
                    HeaderValue::from_static("no-store, no-cache, must-revalidate"),
                );
                res
            }
            Err(_) => text(StatusCode::NOT_FOUND, "not found"),
        }
    };

    if let Some(candidate) = candidate {
        // Real asset hit serves the file; a miss with a concrete non-html
        // extension is a hard 404, everything else is an SPA route and falls
        // back to index.html (mirrors loon_server.go's handleFrontend).
        if tokio::fs::metadata(&candidate)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            return serve_file(candidate).await;
        }
        let is_asset = candidate
            .extension()
            .map(|e| !e.is_empty() && e != "html")
            .unwrap_or(false);
        if is_asset {
            return text(StatusCode::NOT_FOUND, "not found");
        }
    }
    serve_file(root.join("index.html")).await
}

fn mime_for(file: &std::path::Path) -> String {
    match file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn text(status: StatusCode, body: impl Into<String>) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(body.into()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
