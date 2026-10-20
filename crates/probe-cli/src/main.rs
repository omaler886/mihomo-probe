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
use probe_engine::{Ledger, RoundPlan, RoundSettings};
use probe_mihomo::Controller;
use probe_storage::{migrations, Storage};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root: Option<PathBuf> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8088;
    let mut port_given = false;
    let mut trigger = "cli".to_string();
    let mut mode: Option<String> = None;
    let mut from: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut assume_yes = false;
    // A value flag with no value token is not a default worth guessing: a
    // truncated `probe-cli --root` would silently operate on the fallback
    // root, and `db backup --from` reading an unintended file is worse. The
    // same rule for a value that will not parse (`--port 8O88`).
    let flag_value = |args: &Vec<String>, i: &mut usize, flag: &str| -> String {
        *i += 1;
        match args.get(*i) {
            Some(value) => value.clone(),
            None => {
                eprintln!("{flag} expects a value");
                std::process::exit(2);
            }
        }
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                root = Some(PathBuf::from(flag_value(&args, &mut i, "--root")));
            }
            "--host" => {
                host = flag_value(&args, &mut i, "--host");
            }
            "--port" => {
                let value = flag_value(&args, &mut i, "--port");
                match value.parse() {
                    Ok(parsed) => {
                        port = parsed;
                        port_given = true;
                    }
                    Err(_) => {
                        eprintln!("--port expects a port number, got {value:?}");
                        std::process::exit(2);
                    }
                }
            }
            "--trigger" => {
                trigger = flag_value(&args, &mut i, "--trigger");
            }
            "--mode" => {
                let value = flag_value(&args, &mut i, "--mode");
                if value != "direct" && value != "chain" {
                    eprintln!("--mode expects direct|chain, got {value:?}");
                    std::process::exit(2);
                }
                mode = Some(value);
            }
            "--from" => {
                from = Some(PathBuf::from(flag_value(&args, &mut i, "--from")));
            }
            "--out" => {
                out = Some(PathBuf::from(flag_value(&args, &mut i, "--out")));
            }
            "--yes" => assume_yes = true,
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
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
            // The embedded Sub-Store's own default is 8299 (documented, and
            // `substore.listen` says the same): falling through to serve's
            // 8088 made the documented port a lie and collided with `serve`.
            let substore_port = if port_given { port } else { 8299 };
            if let Err(err) = run_async(async move { cmd_substore(&root, substore_port).await }) {
                eprintln!("substore failed: {err}");
                std::process::exit(1);
            }
        }
        other => {
            eprintln!("unknown command: {other}");
            print_usage();
            std::process::exit(2);
        }
    }
}

/// The one place an operator learns the surface; also the answer to `--help`.
fn print_usage() {
    println!(
        "probe-cli [command] [flags]\n\
\n\
\x20 commands:\n\
\x20   serve                 (default) run the API, scheduler and round timer\n\
\x20   round                 run one round now\n\
\x20   status                print the last round from the ledger\n\
\x20   db <migrate|check|backup|verify|rollback>\n\
\x20   substore [init|status]\n\
\n\
\x20 flags:\n\
\x20   --root <dir>          service root (default: $MIHOMO_TEST_ROOT or /srv/mihomo-test)\n\
\x20   --host <addr>         serve bind address\n\
\x20   --port <port>         serve port; the substore subcommand defaults to 8299\n\
\x20   --trigger <name>      round trigger label (manual|schedule|...)\n\
\x20   --mode direct|chain   round measurement mode\n\
\x20   --from <file> --out <file> --yes   db backup/rollback arguments\n\
\x20   -h, --help            this text\n\
\n\
\x20 notes: RUST_LOG=info|debug|trace (round and serve both honour it).\n\
\x20   db backup reads the live ledger through SQLite's online backup; run it\n\
\x20   with the service stopped (or between rounds) so the copy can finish."
    );
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
    // The path must exist first: `Connection::open` CREATES an empty file, so
    // a `status` on a fresh install used to leave a stray zero-byte state.db
    // behind and then panic on "no such table: rounds" when querying it.
    let path = db_path(root);
    if !path.exists() {
        println!(
            "no ledger at {} (run `serve` or `db migrate` first)",
            path.display()
        );
        return;
    }
    let storage = Storage::open_without_migrating(&path).unwrap_or_else(|err| {
        eprintln!("open ledger failed: {err}");
        std::process::exit(1);
    });
    let last = storage.last_round().unwrap_or_else(|err| {
        // An unreadable ledger (wrong schema, corrupt header) is a report, not
        // a panic: the operator asked for status, and this is the status.
        eprintln!("cannot read rounds: {err}");
        std::process::exit(1);
    });
    match last {
        Some(round) => println!(
            "last round: #{} trigger={} finished_at={:?} note={:?}",
            round.round_id, round.trigger, round.finished_at, round.note
        ),
        None => println!("no rounds recorded yet"),
    }
}

fn cmd_round(root: &Path, trigger: &str, mode: Option<&str>) {
    // The round path logs through `tracing` (front-pool fetch failures,
    // preflight notes, region-tag fallbacks). Without a subscriber those
    // messages vanish and an empty front pool looks like an empty
    // configuration instead of a failed fetch. `RUST_LOG` still wins.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();
    let cfg = load(root);
    tokio_block(cmd_round_async(root, cfg, trigger, mode));
}

async fn cmd_round_async(root: &Path, cfg: Config, trigger: &str, mode: Option<&str>) {
    use probe_engine::collect as collect_mod;

    // The service has the same guard: a hand-started round must not race the
    // scheduled one into the same kernel config.
    let _round_lock = match probe_engine::RoundLock::acquire(
        root,
        std::time::Duration::from_secs(cfg.watchdog.round_timeout_minutes.saturating_mul(60)),
    ) {
        Ok(lock) => lock,
        Err(reason) => {
            eprintln!("round refused: {reason}");
            std::process::exit(2);
        }
    };
    let storage = Arc::new(Mutex::new(
        Storage::open(&db_path(root)).expect("open ledger"),
    ));
    let secret = Config::core_secret(&root.join("data")).expect("core secret");
    let controller = Controller::new(&cfg.core.api, Some(&secret));
    let ledger: Arc<dyn Ledger> = storage.clone();
    let round_id = ledger.start_round(trigger, mode).unwrap_or_else(|err| {
        eprintln!("could not open a round row: {err}");
        std::process::exit(1);
    });
    let ctx =
        probe_engine::RoundCtx::new(probe_engine::Limits::from_concurrency(cfg.test.concurrency));
    let _deadline = probe_engine::RoundDeadline::arm(
        &ctx,
        std::time::Duration::from_secs(cfg.watchdog.round_timeout_minutes.saturating_mul(60)),
    );
    let mut settings = RoundSettings::from_config(&cfg);

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
    // `services` takes ownership of a handle; keep ours for the publication step.
    let services =
        probe_engine::network::NetworkServices::new(Arc::clone(&storage), &settings.verify);
    let mut collected = match collect_mod::collect_and_resolve(
        &fetcher,
        &cfg.sources,
        &settings,
        chain,
        &services,
        &ctx,
    )
    .await
    {
        Ok(collected) => collected,
        Err(err) => {
            eprintln!("round preparation failed: {err}");
            settings.blocked = Some(err.to_string());
            probe_engine::Collected::default()
        }
    };
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

    if settings.blocked.is_none() {
        // G03: the kernel validates the config it is about to load. A node it
        // refuses is pruned and the write retried (Python `make_testable`); a
        // config it still refuses blocks the round instead of testing against
        // something that never loaded. `make_testable` owns the write, so the
        // returned list is exactly what is on disk.
        let mut preflight_log = Vec::new();
        let (proxies, verdict) = probe_engine::make_testable(
            &root.join("core"),
            &cfg.core,
            &secret,
            std::mem::take(&mut collected.proxies),
            &probe_engine::MihomoValidator,
            &mut preflight_log,
        );
        for line in &preflight_log {
            eprintln!("preflight: {line}");
        }
        match &verdict {
            probe_engine::PreflightVerdict::Accepted => {}
            probe_engine::PreflightVerdict::Skipped { reason } => {
                println!("preflight: validation skipped ({reason})");
            }
            probe_engine::PreflightVerdict::Pruned { dropped } => {
                println!(
                    "preflight: pruned {} node(s) the kernel refused",
                    dropped.len()
                );
                // A pruned node has no kernel proxy left, so its jobs go too.
                let pruned: std::collections::HashSet<&str> =
                    dropped.iter().map(String::as_str).collect();
                let before = collected.jobs.len();
                collected
                    .jobs
                    .retain(|job| !pruned.contains(job.fingerprint.as_str()));
                if before > collected.jobs.len() {
                    println!(
                        "preflight: {} job(s) dropped with their node",
                        before - collected.jobs.len()
                    );
                }
            }
            probe_engine::PreflightVerdict::Rejected { detail } => {
                eprintln!("preflight: kernel rejects the generated config: {detail}");
                settings.blocked = Some(format!("kernel rejected the generated config: {detail}"));
            }
        }
        collected.proxies = proxies;
    }

    let plan = RoundPlan {
        trigger: trigger.to_string(),
        mode: mode.map(str::to_string),
        // `take` keeps `collected` whole for the publication step below.
        jobs: std::mem::take(&mut collected.jobs),
        policy: cfg.policy.clone().into(),
        domain_pass: settings.verify.domain_pass,
        preserved_sources: std::mem::take(&mut collected.preserved_sources),
    };
    let kernel = settings.kernel_prep(controller.clone(), &cfg.core.container_config_path);
    let tester = settings.verified_tester(controller, &cfg.core, &collected.proxies);
    let outcome = probe_engine::run_round_with_ctx(round_id, plan, ctx, kernel, tester, ledger)
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
            eprintln!(
                "guard: inconclusive round — nothing usable was measured or committed; \
                 previous exports preserved"
            );
        }
    }

    // R8: publish this round's per-source exports. The guard owns the
    // decision -- a disabled publish section or a round that did not apply
    // convergence skips every source and leaves the previous immutable file
    // and ledger pointer untouched.
    let export_root = cfg.publish.export_root(root);
    // The engine never creates the export root: it must already be a private,
    // service-owned directory (Unix 0700), so provision it here and never
    // widen an existing one.
    if cfg.publish.enabled {
        if let Err(err) = std::fs::create_dir_all(&export_root) {
            eprintln!("publish: cannot create {}: {err}", export_root.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&export_root, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut published = {
        let storage_guard = storage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        probe_engine::publish_round(
            &storage_guard,
            &export_root,
            probe_engine::publish::PublishRoundRequest {
                publish: &cfg.publish,
                sources: &cfg.sources,
                collected: &collected,
                verdicts: &outcome.verdicts,
                guard: outcome.guard,
                // A cancelled round, or one that committed nothing, has no
                // baseline to publish from; partial coverage still publishes
                // (Python parity).
                measured: !outcome.cancelled && outcome.results_written > 0,
                actor: "cli-round",
            },
        )
    };
    // G13 push mode: mirror the just-published exports into Sub-Store as
    // `<prefix>-<key>-local`. A failed push is recorded, never fatal.
    //
    // The push needs `&Storage` and does HTTP, so it cannot run while this
    // task holds the ledger guard (a std guard must never cross an await) and
    // it cannot block a runtime thread that owns the reactor. It runs on a
    // blocking thread with its own current-thread runtime -- the same shape
    // the API path uses.
    let fallback = published.clone();
    let (publish_cfg, sources_cfg, push_backend, push_storage) = (
        cfg.publish.clone(),
        cfg.sources.clone(),
        backend.clone(),
        Arc::clone(&storage),
    );
    published = tokio::task::spawn_blocking(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                eprintln!("push: cannot start a runtime: {err}");
                return published;
            }
        };
        let client = probe_source::SubStoreClient::new(&push_backend);
        let guard = push_storage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        runtime.block_on(probe_engine::push_round_exports(
            &client,
            &guard,
            &publish_cfg,
            &sources_cfg,
            &mut published,
        ));
        published
    })
    .await
    .unwrap_or_else(|err| {
        eprintln!("push: task failed: {err}");
        fallback
    });
    if let Some(reason) = published.skipped {
        println!("publish: skipped ({reason:?})");
    }
    for source in &published.sources {
        match source {
            probe_engine::publish::SourcePublication::Published {
                key, node_count, ..
            } => {
                println!("published {key}: {node_count} node(s)");
            }
            probe_engine::publish::SourcePublication::Skipped { key, reason } => {
                println!("publish {key}: skipped ({reason:?})");
            }
            probe_engine::publish::SourcePublication::Failed { key, reason } => {
                eprintln!("publish {key}: failed: {reason}");
            }
        }
    }
    if let Some(pruned) = &published.pruned {
        if pruned.snapshots_deleted > 0 || pruned.files_deleted > 0 {
            println!(
                "retention: {} snapshot(s) and {} file(s) pruned across {} key(s)",
                pruned.snapshots_deleted, pruned.files_deleted, pruned.keys
            );
        }
    }
    for record in &published.push {
        if record.ok {
            println!(
                "pushed {}: {} node(s) -> {}",
                record.key, record.node_count, record.name
            );
        } else {
            eprintln!("push {}: failed: {}", record.key, record.detail);
        }
    }
    if !published.dropped.is_empty() {
        println!(
            "publish: {} node(s) left out of their export body",
            published.dropped.len()
        );
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
    // The subscriber goes up FIRST: the SUBSTORE_BACKEND/embedded warning
    // below fires during state assembly, and a warn emitted before `.init()`
    // is dropped silently -- an operator running both would never learn a
    // second Sub-Store is serving.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
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

    let state = Arc::new(
        AppState::new(
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
                publish: cfg.publish.clone(),
                schedule: cfg.schedule.clone(),
                policy: cfg.policy.clone(),
            },
        )
        .with_publish(cfg.publish.clone()),
    );
    tracing::info!(
        root = %root.display(),
        %host,
        port,
        "probe slice serving"
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
