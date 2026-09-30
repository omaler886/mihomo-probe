//! probe-cli: the Rust slice's entry point.
//!
//! Subcommands mirror the Python module's shape (`python3 -m mihomo_test
//! serve|round|status`) so the shadow run can drive both implementations with
//! the same muscle memory:
//!
//!     probe-cli --root <dir> status
//!     probe-cli --root <dir> round
//!     probe-cli --root <dir> serve --host 127.0.0.1 --port 8088

use std::path::PathBuf;
use std::sync::Arc;

use probe_api::AppState;
use probe_config::Config;
use probe_mihomo::Controller;
use probe_storage::Storage;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root: Option<PathBuf> = None;
    let mut command: Option<String> = None;
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8088;
    let mut trigger = "cli".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                i += 1;
                root = args.get(i).map(PathBuf::from);
            }
            "--host" => {
                i += 1;
                host = args.get(i).cloned().unwrap_or(host);
            }
            "--port" => {
                i += 1;
                port = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(port);
            }
            "--trigger" => {
                i += 1;
                trigger = args.get(i).cloned().unwrap_or(trigger);
            }
            other if command.is_none() && !other.starts_with("--") => {
                command = Some(other.to_string());
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let root = probe_config::resolve_root(root.as_deref());
    let command = command.unwrap_or_else(|| "serve".into());
    match command.as_str() {
        "status" => cmd_status(&root),
        "round" => cmd_round(&root, &trigger),
        "serve" => {
            if let Err(err) = run_async(async move { cmd_serve(&root, &host, port).await }) {
                eprintln!("serve failed: {err}");
                std::process::exit(1);
            }
        }
        other => {
            eprintln!("unknown command: {other} (expected serve | round | status)");
            std::process::exit(2);
        }
    }
}

fn load(root: &std::path::Path) -> Config {
    match Config::load(&root.join("data").join("config.json")) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("config load failed: {err}");
            std::process::exit(1);
        }
    }
}

fn cmd_status(root: &std::path::Path) {
    let cfg = load(root);
    let storage = Storage::open(&root.join("data").join("state.db")).expect("open ledger");
    let last = storage.last_round().expect("query");
    match last {
        Some(round) => println!(
            "last round: #{} trigger={} finished_at={:?} note={:?}",
            round.round_id, round.trigger, round.finished_at, round.note
        ),
        None => println!("no rounds recorded yet"),
    }
    let _ = &cfg;
}

fn cmd_round(root: &std::path::Path, trigger: &str) {
    let cfg = load(root);
    let storage = Storage::open(&root.join("data").join("state.db")).expect("open ledger");
    let secret = Config::core_secret(&root.join("data")).expect("core secret");
    let round_id = storage.start_round(trigger, None).expect("start round");
    // Slice scope: generate the kernel config the round would load, then try
    // the controller. Engine phases (DNS, delay, egress, publish) land in
    // R4-R7; the invariant that must hold from day one is that the round row
    // always closes.
    let proxies: Vec<serde_json::Value> = Vec::new();
    let written = probe_mihomo::write_config(&root.join("core"), &cfg.core, &secret, &proxies)
        .map_err(|e| probe_domain::DomainError::Config(e.to_string()));
    let note = match written {
        Ok(path) => {
            let controller = Controller::new(&cfg.core.api, Some(&secret));
            let outcome = tokio_block(async {
                if controller.version().await.is_err() {
                    format!("slice: controller unreachable ({})", controller.url())
                } else if controller
                    .reload(&cfg.core.container_config_path)
                    .await
                    .unwrap_or(false)
                {
                    "slice: config reloaded into kernel".to_string()
                } else {
                    "slice: reload refused; recreate the kernel container".to_string()
                }
            });
            format!("slice round: config at {}; {outcome}", path.display())
        }
        Err(err) => format!("slice round: config generation failed: {err}"),
    };
    storage
        .finish_round(round_id, Some(&note))
        .expect("finish round");
    println!("round {round_id}: {note}");
}

async fn cmd_serve(root: &std::path::Path, host: &str, port: u16) -> std::io::Result<()> {
    let cfg = load(root);
    let storage = Storage::open(&root.join("data").join("state.db"))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let secret = Config::core_secret(&root.join("data"))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let controller = Controller::new(&cfg.core.api, Some(&secret));
    let state = Arc::new(AppState::new(
        storage,
        cfg.auth_token.clone(),
        controller,
        cfg.core.container_config_path.clone(),
    ));
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tracing::info!(root = %root.display(), %host, port, "probe slice serving (shadow; Python remains the default implementation)");
    probe_api::serve(state, host, port).await
}

fn run_async<F: std::future::Future<Output = std::io::Result<()>>>(
    future: F,
) -> std::io::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

fn tokio_block<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}
