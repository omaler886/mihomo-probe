//! probe-cli: the Rust slice's entry point.
//!
//! Subcommands mirror the Python module's shape (`python3 -m mihomo_test
//! serve|round|status`) so the shadow run can drive both implementations with
//! the same muscle memory, plus the migration tooling GLM_5.3_Flash §18
//! requires:
//!
//!     probe-cli --root <dir> status
//!     probe-cli --root <dir> round
//!     probe-cli --root <dir> serve --host 127.0.0.1 --port 8088
//!     probe-cli --root <dir> db check | verify | backup [--out P]
//!     probe-cli --root <dir> db migrate          # backup -> apply -> verify
//!     probe-cli --root <dir> db rollback --from P [--yes]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use probe_api::AppState;
use probe_config::Config;
use probe_engine::{run_round, Ledger, RoundPlan, RoundSettings};
use probe_mihomo::Controller;
use probe_storage::{migrations, Storage};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root: Option<PathBuf> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8088;
    let mut trigger = "cli".to_string();
    let mut mode: Option<String> = None;
    let mut from: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut assume_yes = false;
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
            "--mode" => {
                i += 1;
                let value = args.get(i).cloned().unwrap_or_default();
                if value != "direct" && value != "chain" {
                    eprintln!("--mode expects direct|chain, got {value:?}");
                    std::process::exit(2);
                }
                mode = Some(value);
            }
            "--from" => {
                i += 1;
                from = args.get(i).map(PathBuf::from);
            }
            "--out" => {
                i += 1;
                out = args.get(i).map(PathBuf::from);
            }
            "--yes" => assume_yes = true,
            other if !other.starts_with("--") => positionals.push(other.to_string()),
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let root = probe_config::resolve_root(root.as_deref());
    let command = positionals
        .first()
        .cloned()
        .unwrap_or_else(|| "serve".into());
    match command.as_str() {
        "status" => cmd_status(&root),
        "round" => cmd_round(&root, &trigger, mode.as_deref()),
        "db" => {
            let sub = positionals.get(1).map(String::as_str).unwrap_or("");
            let code = cmd_db(&root, sub, from.as_deref(), out.as_deref(), assume_yes);
            std::process::exit(code);
        }
        "serve" => {
            if let Err(err) = run_async(async move { cmd_serve(&root, &host, port).await }) {
                eprintln!("serve failed: {err}");
                std::process::exit(1);
            }
        }
        "substore" => {
            if let Err(err) = run_async(async move { cmd_substore(&root, port).await }) {
                eprintln!("substore failed: {err}");
                std::process::exit(1);
            }
        }
        other => {
            eprintln!("unknown command: {other} (expected serve | round | status | db | substore)");
            std::process::exit(2);
        }
    }
}

fn load(root: &Path) -> Config {
    match Config::load(&root.join("data").join("config.json")) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("config load failed: {err}");
            std::process::exit(1);
        }
    }
}

fn db_path(root: &Path) -> PathBuf {
    root.join("data").join("state.db")
}

fn cmd_status(root: &Path) {
    let _cfg = load(root);
    let storage = Storage::open_without_migrating(&db_path(root)).unwrap_or_else(|err| {
        eprintln!("open ledger failed: {err}");
        std::process::exit(1);
    });
    let last = storage.last_round().expect("query");
    match last {
        Some(round) => println!(
            "last round: #{} trigger={} finished_at={:?} note={:?}",
            round.round_id, round.trigger, round.finished_at, round.note
        ),
        None => println!("no rounds recorded yet"),
    }
}

fn cmd_round(root: &Path, trigger: &str, mode: Option<&str>) {
    let cfg = load(root);
    tokio_block(cmd_round_async(root, cfg, trigger, mode));
}

async fn cmd_round_async(root: &Path, cfg: Config, trigger: &str, mode: Option<&str>) {
    use probe_engine::collect as collect_mod;

    let storage = Storage::open(&db_path(root)).expect("open ledger");
    let secret = Config::core_secret(&root.join("data")).expect("core secret");
    let controller = Controller::new(&cfg.core.api, Some(&secret));

    // The collection half of the round (R5/R6): fetch enabled sources and the
    // front pool, expand chains, prepare kernel proxies, derive jobs. One
    // place -- the API path below calls the same function.
    let backend = collect_mod::backend_from_env();
    let fetcher = probe_source::SubStoreClient::new(&backend);
    let admin = probe_source::SubStoreClient::new(&backend);
    let manual = collect_mod::ManualFrontCache::default();
    let chain = collect_mod::ChainContext {
        section: &cfg.chain,
        publish_prefix: &cfg.publish_prefix,
        admin: &admin,
        manual: &manual,
        mode,
    };
    let collected = collect_mod::collect(&fetcher, &cfg.sources, true, chain).await;
    for err in &collected.errors {
        eprintln!("fetch failed: {err}");
    }
    if !collected.fronts.is_empty() {
        println!(
            "front pool: {} front(s){}",
            collected.fronts.len(),
            if collected.fronts_over_cap > 0 {
                format!(" ({} over cap)", collected.fronts_over_cap)
            } else {
                String::new()
            }
        );
    }

    let written =
        probe_mihomo::write_config(&root.join("core"), &cfg.core, &secret, &collected.proxies);

    let mut settings = RoundSettings::from_config(&cfg);
    // A failed write means there is no config for the kernel to load. The
    // round still opens and closes a row, and says why.
    if let Err(err) = &written {
        settings.blocked = Some(err.to_string());
    }

    let plan = RoundPlan {
        trigger: trigger.to_string(),
        mode: mode.map(str::to_string),
        jobs: collected.jobs,
        policy: cfg.policy.clone().into(),
    };
    let ledger: Arc<dyn Ledger> = Arc::new(Mutex::new(storage));
    // The caller opens the row; the runner closes it on every path.
    let round_id = ledger.start_round(trigger, mode).unwrap_or_else(|err| {
        eprintln!("could not open a round row: {err}");
        std::process::exit(1);
    });
    let kernel = settings.kernel_prep(controller.clone(), &cfg.core.container_config_path);
    let tester = settings.tester(controller);
    let outcome = run_round(round_id, plan, settings.limits, kernel, tester, ledger)
        .await
        .unwrap_or_else(|err| {
            eprintln!("round failed: {err}");
            std::process::exit(1);
        });
    println!(
        "round {}: {} ({} node(s), {} dropped, {} fetch error(s), live fronts {})",
        outcome.round_id,
        outcome.note,
        outcome.counts.total,
        collected.dropped,
        collected.errors.len(),
        outcome.live_fronts,
    );
    if outcome.front_dead > 0 {
        eprintln!(
            "{} chain node(s) failed front_dead: no live front carried them",
            outcome.front_dead
        );
    }
    match outcome.guard {
        probe_engine::round::GuardDecision::ApplyConvergence => {}
        probe_engine::round::GuardDecision::PreservePreviousState => {
            eprintln!("guard: suspect round — previous publication preserved");
        }
        probe_engine::round::GuardDecision::MarkRoundInconclusive => {
            eprintln!("guard: round marked inconclusive — no streak advanced");
        }
    }
}

/// `db` subcommands. Returns the process exit code.
fn cmd_db(
    root: &Path,
    subcommand: &str,
    from: Option<&Path>,
    out: Option<&Path>,
    assume_yes: bool,
) -> i32 {
    match subcommand {
        "check" => db_check(root),
        "migrate" => db_migrate(root),
        "verify" => db_verify(root),
        "backup" => db_backup(root, out),
        "rollback" => db_rollback(root, from, assume_yes),
        other => {
            eprintln!("unknown db subcommand: {other:?} (expected check | migrate | verify | backup | rollback)");
            2
        }
    }
}

fn stamp() -> String {
    // Python `_backup` style: state.db.bak-YYYYmmdd-HHMMSS (UTC).
    let fmt = time::macros::format_description!("[year][month][day]-[hour][minute][second]");
    time::OffsetDateTime::now_utc()
        .format(&fmt)
        .unwrap_or_else(|_| "unknown".into())
}

fn db_check(root: &Path) -> i32 {
    let path = db_path(root);
    if !path.exists() {
        println!("no database at {}", path.display());
        return 1;
    }
    let storage = match Storage::open_without_migrating(&path) {
        Ok(storage) => storage,
        Err(err) => {
            eprintln!("open failed: {err}");
            return 1;
        }
    };
    let done = storage.applied_migrations().unwrap_or_default();
    let pending = migrations::LATEST_VERSION - done.len() as i64;
    println!("applied {} migration(s), {pending} pending", done.len());
    for (version, name, at) in &done {
        println!("  {version} {name} @ {at}");
    }
    match storage.integrity_check() {
        Ok(what) => println!("integrity_check: {what}"),
        Err(err) => {
            eprintln!("integrity_check failed: {err}");
            return 1;
        }
    }
    match storage.table_counts() {
        Ok(counts) => {
            for (table, count) in counts {
                println!("  {table}: {count}");
            }
        }
        Err(err) => println!("required tables missing (run `db migrate`): {err}"),
    }
    0
}

fn db_migrate(root: &Path) -> i32 {
    let path = db_path(root);
    if !path.exists() {
        println!("no database at {} yet; creating it", path.display());
    } else {
        // A migration that destroys data must not exist; this backup is the
        // belt to the migrations' braces (GLM_5.3_Flash §11).
        let storage = match Storage::open_without_migrating(&path) {
            Ok(storage) => storage,
            Err(err) => {
                eprintln!("pre-migration open failed: {err}");
                return 1;
            }
        };
        let backup_path = path.with_file_name(format!("state.db.bak-{}", stamp()));
        if let Err(err) = storage.backup_to(&backup_path) {
            eprintln!("pre-migration backup failed: {err}");
            return 1;
        }
        println!("backup written: {}", backup_path.display());
    }
    let storage = match Storage::open(&path) {
        Ok(storage) => storage,
        Err(err) => {
            eprintln!("migrate failed: {err}");
            return 1;
        }
    };
    let applied = storage.applied_migrations().unwrap_or_default();
    println!("schema at {} migration(s) applied", applied.len());
    db_verify_inner(&storage)
}

fn db_verify(root: &Path) -> i32 {
    let storage = match Storage::open_without_migrating(&db_path(root)) {
        Ok(storage) => storage,
        Err(err) => {
            eprintln!("open failed: {err}");
            return 1;
        }
    };
    db_verify_inner(&storage)
}

fn db_verify_inner(storage: &Storage) -> i32 {
    match storage.integrity_check() {
        Ok(what) if what == "ok" => println!("integrity_check: ok"),
        Ok(what) => {
            eprintln!("integrity_check: {what}");
            return 1;
        }
        Err(err) => {
            eprintln!("integrity_check failed: {err}");
            return 1;
        }
    }
    match storage.table_counts() {
        Ok(counts) => {
            for (table, count) in counts {
                println!("  {table}: {count}");
            }
            0
        }
        Err(err) => {
            eprintln!("required tables missing: {err}");
            1
        }
    }
}

fn db_backup(root: &Path, out: Option<&Path>) -> i32 {
    let path = db_path(root);
    if !path.exists() {
        eprintln!("no database at {}", path.display());
        return 1;
    }
    let storage = match Storage::open_without_migrating(&path) {
        Ok(storage) => storage,
        Err(err) => {
            eprintln!("open failed: {err}");
            return 1;
        }
    };
    let target = match out {
        Some(explicit) => explicit.to_path_buf(),
        None => path.with_file_name(format!("state.db.bak-{}", stamp())),
    };
    match storage.backup_to(&target) {
        Ok(()) => {
            println!("backup written: {}", target.display());
            0
        }
        Err(err) => {
            eprintln!("backup failed: {err}");
            1
        }
    }
}

fn db_rollback(root: &Path, from: Option<&Path>, assume_yes: bool) -> i32 {
    let Some(from) = from else {
        eprintln!("rollback needs --from <backup file>");
        return 2;
    };
    let path = db_path(root);
    if !from.exists() {
        eprintln!("backup not found: {}", from.display());
        return 1;
    }
    // Restoring the main file under a live WAL corrupts the database: the
    // WAL belongs to the *current* file's writer, not to the snapshot. A
    // -wal on disk means a writer may still be attached.
    let wal = path.with_file_name("state.db-wal");
    if wal.exists() {
        eprintln!(
            "refusing: {} exists -- a writer may be attached. Stop the service, then retry.",
            wal.display()
        );
        return 1;
    }
    if !assume_yes {
        eprintln!(
            "this replaces {} with {} (loses everything since the backup). Re-run with --yes.",
            path.display(),
            from.display()
        );
        return 2;
    }
    if let Err(err) = std::fs::copy(from, &path) {
        eprintln!("rollback failed: {err}");
        return 1;
    }
    println!("rolled back {} <- {}", path.display(), from.display());
    db_verify(root)
}

async fn cmd_serve(root: &Path, host: &str, port: u16) -> std::io::Result<()> {
    let cfg = load(root);
    let storage =
        Storage::open(&db_path(root)).map_err(|e| std::io::Error::other(e.to_string()))?;
    let secret = Config::core_secret(&root.join("data"))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let controller = Controller::new(&cfg.core.api, Some(&secret));

    // Embedded Sub-Store: resolve the secret path up front so the collection
    // fetcher can point at this very process. An explicit SUBSTORE_BACKEND
    // still wins -- pointing at a standalone instance stays possible without
    // editing the embedded section out of the config.
    let env_backend = std::env::var("SUBSTORE_BACKEND")
        .ok()
        .filter(|v| !v.trim().is_empty());
    // Resolve once; the embedded service repeats the same idempotent
    // resolution and lands on the identical persisted path.
    let embedded = if cfg.substore.embedded {
        let data_dir = root.join("data");
        let backend_path =
            probe_substore::resolve_backend_path(&data_dir, cfg.substore.backend_path.as_deref())
                .map_err(std::io::Error::other)?;
        let listen = cfg.substore.listen.clone();
        if env_backend.is_some() {
            tracing::warn!(
                "substore.embedded=true but SUBSTORE_BACKEND is set; the embedded instance still serves, the fetcher uses the env override"
            );
        }
        Some((
            probe_substore::SubStoreConfig {
                data_dir,
                listen: listen.clone(),
                backend_path: cfg.substore.backend_path.clone(),
                gh_proxy: cfg.substore.gh_proxy.clone(),
                auto_update: cfg.substore.auto_update,
                push_service: cfg.substore.push_service.clone(),
                sync_cron: cfg.substore.sync_cron.clone(),
                produce_cron: cfg.substore.produce_cron.clone(),
            },
            format!("http://{listen}{backend_path}"),
        ))
    } else {
        None
    };
    let backend = match (&env_backend, &embedded) {
        (Some(url), _) => url.clone(),
        (None, Some((_, url))) => url.clone(),
        (None, None) => probe_engine::collect::backend_from_env(),
    };

    let state = Arc::new(AppState::new(
        storage,
        cfg.auth_token.clone(),
        controller,
        probe_api::ServeConfig {
            kernel_config_path: cfg.core.container_config_path.clone(),
            round: RoundSettings::from_config(&cfg),
            core: cfg.core.clone(),
            sources: cfg.sources.clone(),
            backend,
            root: root.to_path_buf(),
            substore: embedded.map(|(cfg, _)| cfg),
            chain: cfg.chain.clone(),
            publish_prefix: cfg.publish_prefix.clone(),
            policy: cfg.policy.clone(),
        },
    ));
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tracing::info!(
        root = %root.display(),
        %host,
        port,
        "probe slice serving (shadow; Python remains the default implementation)"
    );
    probe_api::serve(state, host, port).await
}

/// `substore` subcommand: run the embedded Sub-Store (rquickjs + the vendored
/// sub-store.min.js script-engine bundle) on its own port. `--port` selects
/// the listen port (default 8299); the secret backend path is generated once
/// under `<root>/data/substore/` and reused.
async fn cmd_substore(root: &Path, port: u16) -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cfg = probe_substore::SubStoreConfig {
        data_dir: root.join("data"),
        listen: format!("127.0.0.1:{port}"),
        ..Default::default()
    };
    probe_substore::serve(cfg)
        .await
        .map_err(std::io::Error::other)
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
