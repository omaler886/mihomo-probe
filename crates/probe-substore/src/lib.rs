//! probe-substore: an embedded Sub-Store (script-engine build running inside
//! an rquickjs QuickJS context), the Rust port of how subs-check-pro embeds
//! Sub-Store without Node.
//!
//! Why this crate exists: the Rust probe's export formats and Sub-Store
//! linkage are built around a real Sub-Store (GLM_5.3_Flash §12 — multi-format
//! conversion stays in the official producer). Embedding it means one
//! deployment carries its own Sub-Store backend + frontend, reachable at a
//! secret path, with the probe's REST client talking to `127.0.0.1` instead of
//! a separately managed instance.
//!
//! Architecture, in one screen:
//!
//! ```text
//! axum (:8299)  --CORS-->  secret-path router
//!    │                         │
//!    │ frontend/ (dist.zip)    │  LoonRequest{url,method,headers,body} + "cors=..."
//!    ▼                         ▼
//! static files           Engine (rquickjs QuickJS)
//!                              │  $request/$argument in, $done out
//!                              │  $httpClient ─► reqwest ─► __dispatch_http_response
//!                              │  $persistentStore ─► JsonKvStore (sub-store.json + cache)
//!                              └─ sub-store.min.js (vendored 2.42.2, auto-updatable)
//! ```
//!
//! Deviations from subs-check-pro's Go implementation, all deliberate:
//! * One shared QuickJS context instead of a fresh runtime per request
//!   (rquickjs can safely re-enter; Go's quickjs-go cannot). Requests are
//!   serialized end-to-end; concurrent downloads *inside* a request still fan
//!   out. If real multi-request parallelism is ever needed, swap the dispatch
//!   mutex for a per-request runtime — `execute`'s signature does not change.
//! * A runtime interrupt handler enforces a hard 170s script deadline; Go
//!   could only leak its goroutine past that point.
//! * `$done` resolution happens on the JS side (`$done` also resolves the
//!   runner promise), so Rust never re-enters the engine just to unblock a
//!   finished request.
//! * The KV store is two JSON files exactly like the Go one (`sub-store.json`
//!   main config + debounced `sub-store-cache.json`); a SQLite KV table is a
//!   possible later swap since [`kv::KvStore`] is a trait.

pub mod assets;
pub mod cron_jobs;
pub mod engine;
pub mod init_js;
pub mod kv;
pub mod server;

use std::path::PathBuf;
use std::sync::Arc;

pub use engine::{Engine, EngineError, LoonRequest, LoonResponse};

#[derive(Debug, Clone)]
pub struct SubStoreConfig {
    /// The probe's data directory; `substore/` (bundle copy, KV files,
    /// frontend, secret-path file) lives under it.
    pub data_dir: PathBuf,
    /// Listen address for the dedicated Sub-Store HTTP server.
    pub listen: String,
    /// Explicit backend path; generated-and-persisted when None.
    pub backend_path: Option<String>,
    /// Optional gh-proxy prefix for GitHub release downloads.
    pub gh_proxy: Option<String>,
    /// Check GitHub for a newer backend bundle at startup.
    pub auto_update: bool,
    /// Optional Bark-style push URL template for `$notification`
    /// (`[推送标题]`/`[推送内容]` placeholders).
    pub push_service: Option<String>,
    /// Cron expression for the Gist-sync job (`/api/sync/artifacts`);
    /// absent = no job. Host-side cron — the Loon bundle has no scheduler.
    pub sync_cron: Option<String>,
    /// Produce-cache spec: `<cron>,<sub|col>,<names...>` entries separated
    /// by `;`; each name gets its own `/download/...` warm-up job.
    pub produce_cron: Option<String>,
}

impl Default for SubStoreConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            listen: "127.0.0.1:8299".into(),
            backend_path: None,
            gh_proxy: None,
            auto_update: false,
            push_service: None,
            sync_cron: None,
            produce_cron: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("engine: {0}")]
    Engine(#[from] EngineError),
    #[error("asset: {0}")]
    Asset(String),
}

/// Resolve (and persist) the secret backend path for a data dir without
/// starting the service. Callers that need to advertise the URL — e.g. the
/// probe API pointing its own collection fetcher at the embedded instance —
/// use this before [`serve`]; `serve` performs the identical resolution, so
/// both sides agree on one generated path.
pub fn resolve_backend_path(
    data_dir: &std::path::Path,
    explicit: Option<&str>,
) -> Result<String, ServiceError> {
    let paths = assets::ensure_local_assets(&data_dir.join("substore"))?;
    match explicit {
        Some(p) => {
            let mut s = p.to_string();
            if !s.starts_with('/') {
                s.insert(0, '/');
            }
            Ok(s)
        }
        None => assets::load_or_create_backend_path(&paths)
            .map_err(|e| ServiceError::Asset(format!("backend path: {e}"))),
    }
}

/// Resolve assets, build the engine, install the frontend in the background,
/// and serve until the process is killed. For embedding into an existing
/// axum app use [`server::router`] with a hand-built [`server::SubStoreState`].
pub async fn serve(cfg: SubStoreConfig) -> Result<(), ServiceError> {
    let paths = assets::ensure_local_assets(&cfg.data_dir.join("substore"))?;

    if cfg.auto_update {
        if let Err(err) = assets::update_backend(&paths, cfg.gh_proxy.as_deref()).await {
            tracing::warn!("sub-store backend update check failed: {err}");
        }
    }

    let (bundle_src, version) = assets::load_bundle(&paths);
    tracing::info!(version = %version, bytes = bundle_src.len(), "sub-store backend loaded");

    let kv = kv::JsonKvStore::new_arc(&paths.dir)?;
    let engine = Arc::new(Engine::new(&bundle_src, kv, cfg.push_service.clone()).await?);

    let backend_path = resolve_backend_path(&cfg.data_dir, cfg.backend_path.as_deref())?;

    // The frontend learns the same keys the Go version injects (cron lines
    // and push service), so its UI reflects the deployment.
    let mut env_extras = Vec::new();
    for (key, value) in [
        ("SUB_STORE_BACKEND_SYNC_CRON", cfg.sync_cron.as_deref()),
        ("SUB_STORE_PRODUCE_CRON", cfg.produce_cron.as_deref()),
        ("SUB_STORE_PUSH_SERVICE", cfg.push_service.as_deref()),
    ] {
        if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
            env_extras.push((key.to_string(), v.to_string()));
        }
    }
    let mut state =
        server::SubStoreState::new(engine, backend_path, paths.frontend.clone(), "mihomo-probe");
    state.env_extras = env_extras;
    let state = Arc::new(state);

    let gh_proxy = cfg.gh_proxy.clone();
    tokio::spawn(async move {
        if let Err(err) = assets::ensure_frontend(&paths.frontend, gh_proxy.as_deref()).await {
            tracing::warn!("sub-store frontend install failed (backend still works): {err}");
        }
    });

    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    let panel = format!(
        "http://{}{}/subs?api={}",
        cfg.listen, state.backend_path, state.backend_path
    );
    tracing::info!(
        listen = %cfg.listen,
        backend_path = %state.backend_path,
        "sub-store serving; panel: {panel}"
    );
    // Host-side cron (the Go StartSubStoreCronJobs port). Spawned only after
    // the listener is up, so a failed boot never leaves self-calling tasks
    // behind; they run detached for the process lifetime, like the server.
    cron_jobs::spawn_jobs(
        &cfg.listen,
        &state.backend_path,
        cfg.sync_cron.as_deref(),
        cfg.produce_cron.as_deref(),
    );
    let app = server::router(state);
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_json_preserve_order_untouched() {
        // The engine feeds Sub-Store config JSON through serde_json; the
        // workspace-wide preserve_order feature must survive (see root
        // Cargo.toml comment). A sorted-key regression would reorder every
        // sub-store.json write.
        let v: serde_json::Value = serde_json::from_str(r#"{"b":1,"a":{"z":1,"y":2}}"#).unwrap();
        let out = v.to_string();
        assert!(out.starts_with(r#"{"b":1,"a":{"z":1,"y":2}}"#), "{out}");
    }
}
