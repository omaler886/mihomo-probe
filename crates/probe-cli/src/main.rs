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
//!     probe-cli --root <dir> migrate scan [--prefix P] [--report FILE]
//!     probe-cli --root <dir> migrate apply --plan FILE [--execute]
//!     probe-cli --root <dir> migrate rollback --report FILE
//!     probe-cli --root <dir> migrate clean [--prefix P] [--ipmap-prefix P]
//!                                            [--report FILE | --plan FILE]
//!                                            [--execute]
//!     probe-cli --root <dir> migrate clean-rollback --backup NAME

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use probe_api::AppState;
use probe_config::Config;
use probe_engine::{Ledger, RoundPlan, RoundSettings};
use probe_mihomo::Controller;
// `healthcheck`/`publish` are trait methods; the trait must be in scope at
// the `gist` subcommands.
use probe_publish::Publisher;
use probe_source::{
    ApplyReport, CleanupCaps, CleanupPlan, CleanupRules, MigrationPlan, ScanCaps, SubStoreClient,
    SubStoreHttpStore,
};
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
    // `migrate` subcommand flags.
    let mut prefix: Option<String> = None;
    let mut plan_file: Option<PathBuf> = None;
    let mut report_file: Option<PathBuf> = None;
    let mut execute = false;
    let mut quarantine_hours: Option<u64> = None;
    // `migrate clean` flags (B8).
    let mut ipmap_prefix: Option<String> = None;
    let mut backup_file: Option<String> = None;
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
            "--prefix" => {
                prefix = Some(flag_value(&args, &mut i, "--prefix"));
            }
            "--plan" => {
                plan_file = Some(PathBuf::from(flag_value(&args, &mut i, "--plan")));
            }
            "--report" => {
                report_file = Some(PathBuf::from(flag_value(&args, &mut i, "--report")));
            }
            "--execute" => execute = true,
            "--quarantine-hours" => {
                let value = flag_value(&args, &mut i, "--quarantine-hours");
                match value.parse() {
                    Ok(parsed) => quarantine_hours = Some(parsed),
                    Err(_) => {
                        eprintln!("--quarantine-hours expects a number of hours, got {value:?}");
                        std::process::exit(2);
                    }
                }
            }
            "--ipmap-prefix" => {
                ipmap_prefix = Some(flag_value(&args, &mut i, "--ipmap-prefix"));
            }
            "--backup" => {
                backup_file = Some(flag_value(&args, &mut i, "--backup"));
            }
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
        "gist" => {
            let sub = positionals.get(1).map(String::as_str).unwrap_or("");
            std::process::exit(cmd_gist(&root, sub));
        }
        "webdav" => {
            let sub = positionals.get(1).map(String::as_str).unwrap_or("");
            std::process::exit(cmd_webdav(&root, sub));
        }
        "migrate" => {
            let sub = positionals.get(1).map(String::as_str).unwrap_or("");
            let args = MigrateArgs {
                prefix: prefix.clone(),
                ipmap_prefix: ipmap_prefix.clone(),
                plan_file: plan_file.clone(),
                report_file: report_file.clone(),
                backup_file: backup_file.clone(),
                quarantine_hours,
                execute,
            };
            std::process::exit(cmd_migrate(&root, sub, &args));
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
\x20   gist <test|init>       test the Gist destination / create the one Gist\n\
\x20   webdav <test|init>     test the WebDAV collection / MKCOL the directory\n\
\x20   migrate <scan|apply|rollback>\n\
\x20                          Sub-Store duplicate migration (F-058: plan-only\n\
\x20                          output; never creates, never deletes)\n\
\x20   migrate <clean|clean-rollback>\n\
\x20                          B8 测活产物 cleanup: scan/classify, back up then\n\
\x20                          delete (--execute AND substore.allow_delete\n\
\x20                          required), restore from a backup\n\
\n\
\x20 flags:\n\
\x20   --root <dir>          service root (default: $MIHOMO_TEST_ROOT or /srv/mihomo-test)\n\
\x20   --host <addr>         serve bind address\n\
\x20   --port <port>         serve port; the substore subcommand defaults to 8299\n\
\x20   --trigger <name>      round trigger label (manual|schedule|...)\n\
\x20   --mode direct|chain   round measurement mode\n\
\x20   --from <file> --out <file> --yes   db backup/rollback arguments\n\
\x20   --prefix <P>          migrate scan prefix (default: <publish.prefix>-)\n\
\x20   --report <file>       migrate scan: write the plan JSON here;\n\
\x20                         migrate rollback: the apply report to restore from\n\
\x20   --plan <file>         migrate apply: the plan JSON to apply\n\
\x20   --execute             migrate apply: really write (WITHOUT it the apply\n\
\x20                         is a dry run)\n\
\x20   --quarantine-hours N  migrate apply: quarantine window recorded on the\n\
\x20                         report (default: the plan's, itself 24)\n\
\x20   --ipmap-prefix <P>    migrate clean: the ipmap naming prefix (default:\n\
\x20                         ipmap-); display names decide, this only nominates\n\
\x20   --backup <name>       migrate clean-rollback: the backup_file a previous\n\
\x20                         clean --execute reported (resolved under\n\
\x20                         <root>/data/cleanup-backups/)\n\
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
    // Workstreams/18: the configured sources are the ledger's registry. This
    // runs before the round opens so the per-source run slots below resolve a
    // real source_id, and it is idempotent -- a rename updates one row and
    // never mints a second source or artifact for the same key.
    {
        let storage = storage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(err) = probe_engine::sync_ledger_sources(&storage, &cfg.sources) {
            eprintln!("source registry sync failed: {err}");
        }
    }
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
    // Workstreams/18 B2: the remote publish leg (Gist / composite). It runs
    // after the local pipeline committed and NEVER changes the round's
    // outcome; its per-source results are recorded on the source's run slot
    // (`publish_status`). The same lock/runtime shape as the push above: the
    // storage guard must not cross an await, and the HTTP calls run on a
    // blocking thread with their own current-thread runtime.
    if cfg.publish.provider != probe_config::PublishProvider::Local {
        let publish_cfg = cfg.publish.clone();
        let sources_cfg = cfg.sources.clone();
        let export_root_remote = export_root.clone();
        let remote_outcome = published.clone();
        let remote_storage = Arc::clone(&storage);
        let report = tokio::task::spawn_blocking(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    eprintln!("publish: cannot start a runtime: {err}");
                    return None;
                }
            };
            let guard = remote_storage
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let request = probe_engine::publish_remote::RemotePublishRequest {
                publish: &publish_cfg,
                export_root: &export_root_remote,
                outcome: &remote_outcome,
                round_id,
                sources: &sources_cfg,
            };
            Some(
                runtime.block_on(probe_engine::publish_remote::publish_round_remote(
                    &guard, request,
                )),
            )
        })
        .await
        .ok()
        .flatten();
        if let Some(report) = report {
            if !report.constructed {
                eprintln!("publish: remote publisher not configured; nothing was sent");
            }
            for entry in &report.entries {
                let note = entry.error_code.as_deref().unwrap_or("-");
                match entry.status {
                    probe_storage::runs::PUBLISH_PUBLISHED if entry.error_code.is_none() => {
                        println!(
                            "remote publish {}: published ({})",
                            entry.key, entry.gist_file
                        );
                    }
                    probe_storage::runs::PUBLISH_NO_CHANGE => {
                        println!("remote publish {}: no change", entry.key);
                    }
                    probe_storage::runs::PUBLISH_PUBLISHED => {
                        // Composite: the local artifact is authoritative; the
                        // remote-side failure rides along as a note.
                        println!(
                            "remote publish {}: published (remote note: {note})",
                            entry.key
                        );
                    }
                    _ => {
                        eprintln!("remote publish {}: {}: {note}", entry.key, entry.status);
                    }
                }
            }
        }
    }
    // Workstreams/27: the canonical four-file publish (G-2 integration).
    // Same fire-and-forget contract as the remote leg above: the report is
    // printed, the round's outcome is never touched, and a failure leaves the
    // previous four files standing. The local parity materialization
    // (export_root/mihomo-alive.yaml + meta) happens even without a remote
    // channel, but only when the guard allowed a publication this round.
    {
        // spawn_blocking 的闭包是 'static：请求所需的轮次状态先克隆成
        // owned 值，请求本身在闭包内借它们构造（与上方远程腿同型）。
        let canonical_publish_cfg = cfg.publish.clone();
        let canonical_sources = cfg.sources.clone();
        let canonical_collected = collected.clone();
        let canonical_verdicts = outcome.verdicts.clone();
        let canonical_guard = outcome.guard;
        let canonical_measured = !outcome.cancelled && outcome.results_written > 0;
        let canonical_round_id = outcome.round_id;
        let canonical_counts = outcome.counts;
        let canonical_suspect = outcome.suspect;
        let resolved = match probe_engine::canonical_round::CanonicalPublisher::resolve(
            &cfg.publish,
        ) {
            None => None,
            Some(Ok(pair)) => Some(pair),
            Some(Err(error)) => {
                eprintln!(
                    "canonical publish: remote publisher not configured ({error}); local parity only"
                );
                None
            }
        };
        let canonical_storage = Arc::clone(&storage);
        let canonical_export_root = export_root.clone();
        let report = tokio::task::spawn_blocking(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    eprintln!("canonical publish: cannot start a runtime: {err}");
                    return None;
                }
            };
            let guard = canonical_storage
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (publisher, target) = resolved?;
            let request = probe_engine::canonical_round::CanonicalRoundRequest {
                publish: &canonical_publish_cfg,
                sources: &canonical_sources,
                collected: &canonical_collected,
                verdicts: &canonical_verdicts,
                guard: canonical_guard,
                measured: canonical_measured,
                round_id: canonical_round_id,
                counts: canonical_counts,
                suspect: canonical_suspect,
            };
            Some(
                runtime.block_on(probe_engine::canonical_round::publish_round_canonical(
                    Some(&publisher),
                    &target,
                    &guard,
                    &canonical_export_root,
                    &request,
                )),
            )
        })
        .await
        .ok()
        .flatten();
        match report {
            Some(report) if report.status == probe_storage::runs::PUBLISH_FAILED => {
                eprintln!(
                    "canonical publish: failed ({}): {}",
                    report.error_code.as_deref().unwrap_or("-"),
                    report.message
                );
            }
            Some(report) if report.status == probe_storage::runs::PUBLISH_PUBLISHED => {
                println!(
                    "canonical publish: published (revision {})",
                    report.revision.as_deref().unwrap_or("-")
                );
            }
            Some(report) if report.status == probe_storage::runs::PUBLISH_NO_CHANGE => {
                println!("canonical publish: no change");
            }
            _ => {}
        }
    }
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

/// `gist` subcommands (workstreams/18 B2): `test` probes the configured Gist
/// destination, `init` creates the ONE Gist the publisher will ever patch.
/// Both are masked: the token (which lives only in its file) and any gist
/// content are never printed -- configured/ok/error codes only.
fn cmd_gist(root: &Path, subcommand: &str) -> i32 {
    match subcommand {
        "test" => cmd_gist_test(root),
        "init" => cmd_gist_init(root),
        other => {
            eprintln!("unknown gist subcommand: {other:?} (expected test | init)");
            2
        }
    }
}

/// `gist test`: one healthcheck against the configured destination. Every
/// answer is `configured`/`ok`/`error_code` -- never a message, never a URL,
/// never the token. Exit 0 only when the destination answers.
fn cmd_gist_test(root: &Path) -> i32 {
    let cfg = load(root);
    tokio_block(async {
        let publisher = match probe_publish::GistPublisher::from_config(&cfg.publish.gist) {
            Ok(publisher) => publisher,
            // Construction failure IS the unconfigured case: no token file,
            // no request, and nothing to say beyond the reason code.
            Err(_) => {
                println!("configured=false");
                println!("error_code=config");
                return 1;
            }
        };
        let gist_id = cfg.publish.gist.gist_id.as_deref().unwrap_or("");
        match publisher.healthcheck(gist_id).await {
            Ok(()) => {
                println!("configured=true");
                println!("ok=true");
                0
            }
            Err(error) => {
                let configured = !matches!(error.code, probe_publish::PublishErrorCode::Config);
                println!("configured={configured}");
                println!("ok=false");
                println!("error_code={}", error.code.as_str());
                1
            }
        }
    })
}

/// The `gist init` idempotency decision, pure for tests: an existing gist id
/// that still healthchecks means the dedicated Gist already exists. Every
/// other combination creates -- because `POST /gists` has no idempotency
/// key, the caller (not the publisher) owns the create-once rule (F-032).
fn gist_init_decision(existing_id: Option<&str>, healthcheck_ok: Option<bool>) -> GistInitDecision {
    match (
        existing_id.map(str::trim).filter(|id| !id.is_empty()),
        healthcheck_ok,
    ) {
        (Some(_), Some(true)) => GistInitDecision::AlreadyInitialized,
        _ => GistInitDecision::Create,
    }
}

/// What `gist init` decided to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GistInitDecision {
    /// The configured id is live: report and exit.
    AlreadyInitialized,
    /// No usable id (or it stopped answering): create once.
    Create,
}

/// `gist init`: create the dedicated Gist once, then tell the operator where
/// the id goes. Idempotent: an existing, healthy gist id short-circuits to
/// `already-initialized` without a second POST. This repo has no config.json
/// writer, so persistence is the printed snippet -- exact, and the only
/// thing an operator has to paste.
fn cmd_gist_init(root: &Path) -> i32 {
    let cfg = load(root);
    let section = cfg.publish.gist.clone();
    tokio_block(async move {
        if let Some(existing) = section
            .gist_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            let health = match probe_publish::GistPublisher::from_config(&section) {
                Ok(publisher) => publisher.healthcheck(existing).await.is_ok(),
                Err(_) => false,
            };
            match gist_init_decision(Some(existing), Some(health)) {
                GistInitDecision::AlreadyInitialized => {
                    println!("already-initialized");
                    println!("gist_id={existing}");
                    return 0;
                }
                GistInitDecision::Create => {
                    eprintln!(
                        "the configured gist_id did not healthcheck; creating a new Gist \
                         (the old id stays in the config until you paste the new one)"
                    );
                }
            }
        }
        let publisher = match probe_publish::GistPublisher::from_config(&section) {
            Ok(publisher) => publisher,
            Err(_) => {
                println!("configured=false");
                println!("error_code=config");
                return 1;
            }
        };
        match publisher.create_gist("mihomo-probe exports").await {
            Ok(gist_id) => {
                let visibility = if section.public { "public" } else { "secret" };
                println!("gist created ({visibility})");
                println!("gist_id={gist_id}");
                println!("persist it in <root>/data/config.json (merge into publish.gist):");
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "publish": {"gist": {"gist_id": gist_id}},
                    }))
                    .unwrap_or_else(|_| "{{\"publish\": {}}}".to_string())
                );
                0
            }
            Err(error) => {
                println!("ok=false");
                println!("error_code={}", error.code.as_str());
                1
            }
        }
    })
}

/// `webdav` subcommands (workstreams/26): `test` PROPFINDs the configured
/// collection, `init` MKCOLs the directory chain. Both are masked: the
/// password (which lives only in its file) and any content are never
/// printed -- configured/ok/error codes only.
fn cmd_webdav(root: &Path, subcommand: &str) -> i32 {
    match subcommand {
        "test" => cmd_webdav_test(root),
        "init" => cmd_webdav_init(root),
        other => {
            eprintln!("unknown webdav subcommand: {other:?} (expected test | init)");
            2
        }
    }
}

/// `webdav test`: one PROPFIND against the configured collection. Every
/// answer is `configured`/`ok`/`error_code` -- never a message, never a URL,
/// never the password. Exit 0 only when the destination answers.
fn cmd_webdav_test(root: &Path) -> i32 {
    let cfg = load(root);
    tokio_block(async {
        let publisher = match probe_publish::WebDavPublisher::from_config(&cfg.publish.webdav) {
            Ok(publisher) => publisher,
            // Construction failure IS the unconfigured case: no URL or no
            // readable password file, no request, nothing beyond the code.
            Err(_) => {
                println!("configured=false");
                println!("error_code=config");
                return 1;
            }
        };
        match publisher.healthcheck("").await {
            Ok(()) => {
                println!("configured=true");
                println!("ok=true");
                0
            }
            Err(error) => {
                let configured = !matches!(error.code, probe_publish::PublishErrorCode::Config);
                println!("configured={configured}");
                println!("ok=false");
                println!("error_code={}", error.code.as_str());
                1
            }
        }
    })
}

/// `webdav init`: MKCOL the configured directory chain. Idempotent by
/// protocol (405 = already exists is success), so unlike `gist init` there
/// is no create-once decision to guard -- re-running is always safe.
fn cmd_webdav_init(root: &Path) -> i32 {
    let cfg = load(root);
    tokio_block(async {
        let publisher = match probe_publish::WebDavPublisher::from_config(&cfg.publish.webdav) {
            Ok(publisher) => publisher,
            Err(_) => {
                println!("configured=false");
                println!("error_code=config");
                return 1;
            }
        };
        match probe_publish::create_collection(&publisher).await {
            Ok(()) => {
                println!("collection ready");
                0
            }
            Err(error) => {
                println!("ok=false");
                println!("error_code={}", error.code.as_str());
                1
            }
        }
    })
}

/// Every flag the `migrate` verbs read, bundled so the dispatcher stays
/// under clippy's argument budget.
struct MigrateArgs {
    prefix: Option<String>,
    ipmap_prefix: Option<String>,
    plan_file: Option<PathBuf>,
    report_file: Option<PathBuf>,
    backup_file: Option<String>,
    quarantine_hours: Option<u64>,
    execute: bool,
}

/// `migrate` subcommands (workstreams/18 §B5 + B8 cleanup): driven directly
/// against the configured backend. The safety contract mirrors the API
/// module (`probe_api::migrations`):
///
/// * `scan` is read-only by construction;
/// * `apply` WITHOUT `--execute` is a DRY RUN -- the flag is opt-in, so a
///   forgotten flag can only ever preview, never write;
/// * `rollback` restores from the report a previous apply printed (update
///   path only; the core has no delete to call);
/// * `clean` plans a 测活产物 cleanup; a real delete needs `--execute` AND
///   the deployment gate, and backs everything up before deleting;
/// * `clean-rollback` restores from such a backup.
fn cmd_migrate(root: &Path, subcommand: &str, args: &MigrateArgs) -> i32 {
    let MigrateArgs {
        prefix,
        ipmap_prefix,
        plan_file,
        report_file,
        backup_file,
        quarantine_hours,
        execute,
    } = args;
    match subcommand {
        "scan" => migrate_scan(root, prefix.as_deref(), report_file.as_deref()),
        "apply" => migrate_apply(root, plan_file.as_deref(), *execute, *quarantine_hours),
        "rollback" => migrate_rollback(root, report_file.as_deref()),
        "clean" => migrate_clean(
            root,
            prefix.as_deref(),
            ipmap_prefix.as_deref(),
            plan_file.as_deref(),
            report_file.as_deref(),
            *execute,
        ),
        "clean-rollback" => migrate_clean_rollback(root, backup_file.as_deref()),
        other => {
            eprintln!(
                "unknown migrate subcommand: {other:?} (expected scan | apply | rollback | \
                 clean | clean-rollback)"
            );
            2
        }
    }
}

/// The Sub-Store backend the migrate commands talk to, resolved the same way
/// `serve` resolves it: `SUBSTORE_BACKEND` first, then the embedded
/// section's listen + backend path (explicit, or the path a previous
/// embedded run generated under `<root>/data/substore/`), then the plain
/// environment default. The asset-creating resolver is deliberately not
/// called here: a migration command must not materialize a Sub-Store
/// install as a side effect of resolving a URL.
fn migrate_backend(root: &Path, cfg: &Config) -> String {
    if let Ok(url) = std::env::var("SUBSTORE_BACKEND") {
        if !url.trim().is_empty() {
            return url;
        }
    }
    if cfg.substore.embedded {
        let listen = cfg.substore.listen.trim_end_matches('/');
        if let Some(explicit) = cfg.substore.backend_path.as_deref() {
            let path = if explicit.starts_with('/') {
                explicit.to_string()
            } else {
                format!("/{explicit}")
            };
            return format!("http://{listen}{path}");
        }
        let generated = root.join("data").join("substore").join("backend-path.txt");
        if let Ok(text) = std::fs::read_to_string(&generated) {
            let path = text.trim();
            if !path.is_empty() {
                return format!("http://{listen}{path}");
            }
        }
        eprintln!(
            "migrate: substore.embedded is on but no backend path is known yet \
             (run `substore` once, or set SUBSTORE_BACKEND); using the default"
        );
    }
    probe_engine::collect::backend_from_env()
}

/// The ledger's key→source_id registry, for the stable-URL resolution. A
/// missing or unreadable ledger degrades to an empty map (the scan's tier-1
/// evidence is simply absent) rather than failing a read-only command.
fn ledger_source_map(root: &Path) -> HashMap<String, String> {
    let path = db_path(root);
    if !path.exists() {
        return HashMap::new();
    }
    let storage = match Storage::open_without_migrating(&path) {
        Ok(storage) => storage,
        Err(err) => {
            eprintln!("migrate: cannot open the ledger ({err}); continuing without it");
            return HashMap::new();
        }
    };
    match storage.list_sources() {
        Ok(rows) => rows
            .into_iter()
            .map(|row| (row.key, row.source_id))
            .collect(),
        Err(err) => {
            eprintln!("migrate: cannot read sources ({err}); continuing without them");
            HashMap::new()
        }
    }
}

/// The scan prefix when `--prefix` is silent: the configured publish prefix
/// plus the dash every sub name carries (`<prefix>-<key>`), matching the API
/// module's default.
fn migrate_prefix(cfg: &Config, prefix: Option<&str>) -> String {
    match prefix.map(str::trim).filter(|prefix| !prefix.is_empty()) {
        Some(prefix) => prefix.to_string(),
        None if cfg.publish_prefix.is_empty() => probe_source::migrate::DEFAULT_PREFIX.to_string(),
        None => format!("{}-", cfg.publish_prefix),
    }
}

/// `migrate scan [--prefix P] [--report FILE]`: the read-only duplicate
/// scan. The plan (redacted URLs only) prints to stdout, and with `--report`
/// the same pretty JSON is written to a file for `migrate apply`.
fn migrate_scan(root: &Path, prefix: Option<&str>, report: Option<&Path>) -> i32 {
    let cfg = load(root);
    let backend = migrate_backend(root, &cfg);
    let prefix = migrate_prefix(&cfg, prefix);
    let map = ledger_source_map(root);
    println!(
        "scanning {} for objects named {prefix}* (read-only; nothing is written)",
        backend_display(&backend)
    );
    tokio_block(async move {
        let store = SubStoreHttpStore::new(&backend);
        match probe_source::scan(&store, &prefix, Some(&map), ScanCaps::default()).await {
            Ok(plan) => {
                let json = match serde_json::to_string_pretty(&plan) {
                    Ok(json) => json,
                    Err(err) => {
                        eprintln!("scan failed to serialize the plan: {err}");
                        return 1;
                    }
                };
                if let Some(report) = report {
                    match std::fs::write(report, format!("{json}\n")) {
                        Ok(()) => println!("plan written: {}", report.display()),
                        Err(err) => {
                            eprintln!("cannot write {}: {err}", report.display());
                            return 1;
                        }
                    }
                }
                println!("{json}");
                0
            }
            Err(err) => {
                eprintln!("scan failed: {err}");
                1
            }
        }
    })
}

/// `migrate apply --plan FILE [--execute] [--quarantine-hours N]`. Without
/// `--execute` this is a dry run: the fact is printed, the report says
/// `dry_run: true`, and no request that writes is built.
fn migrate_apply(
    root: &Path,
    plan_file: Option<&Path>,
    execute: bool,
    quarantine_hours: Option<u64>,
) -> i32 {
    let Some(plan_file) = plan_file else {
        eprintln!("apply needs --plan <file> (run `migrate scan --report` first)");
        return 2;
    };
    let text = match std::fs::read_to_string(plan_file) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("cannot read {}: {err}", plan_file.display());
            return 1;
        }
    };
    let mut plan: MigrationPlan = match serde_json::from_str(&text) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("{} is not a migration plan: {err}", plan_file.display());
            return 1;
        }
    };
    if let Some(hours) = quarantine_hours {
        // Advisory only: the window is recorded on the report so an operator
        // reading it later knows how long the `deprecated-*` objects have
        // been quarantined. Nothing is ever deleted, whatever the value.
        plan.quarantine_hours = hours;
    }
    let dry_run = !execute;
    if dry_run {
        println!("dry run: no writes will be made (re-run with --execute to apply)");
    } else {
        println!(
            "REAL APPLY: update-only writes will be sent (never creates, \
             never deletes)"
        );
    }
    let cfg = load(root);
    let backend = migrate_backend(root, &cfg);
    let hostname = cfg.publish.hostname.trim().to_string();
    let map = ledger_source_map(root);
    tokio_block(async move {
        let store = SubStoreHttpStore::new(&backend);
        let report = probe_source::apply(
            &plan,
            &store,
            |artifact| probe_api::migrations::stable_artifact_url(&hostname, artifact, &map),
            dry_run,
        )
        .await;
        let json = match serde_json::to_string_pretty(&report) {
            Ok(json) => json,
            Err(err) => {
                eprintln!("apply failed to serialize the report: {err}");
                return 1;
            }
        };
        println!("{json}");
        if dry_run {
            println!("dry run: nothing was written");
        } else {
            println!(
                "applied={} failed={} skipped={}",
                report.counts.applied, report.counts.failed, report.counts.skipped
            );
        }
        if report.counts.failed > 0 {
            1
        } else {
            0
        }
    })
}

/// `migrate rollback --report FILE`: restore from the apply report (the
/// exact document a previous apply printed).
fn migrate_rollback(root: &Path, report_file: Option<&Path>) -> i32 {
    let Some(report_file) = report_file else {
        eprintln!("rollback needs --report <file> (the apply report to restore from)");
        return 2;
    };
    let text = match std::fs::read_to_string(report_file) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("cannot read {}: {err}", report_file.display());
            return 1;
        }
    };
    let report: ApplyReport = match serde_json::from_str(&text) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("{} is not an apply report: {err}", report_file.display());
            return 1;
        }
    };
    let cfg = load(root);
    let backend = migrate_backend(root, &cfg);
    println!(
        "restoring from {} (update-only writes; never deletes)",
        backend_display(&backend)
    );
    tokio_block(async move {
        let store = SubStoreHttpStore::new(&backend);
        let rollback = probe_source::rollback(&report, &store).await;
        let json = match serde_json::to_string_pretty(&rollback) {
            Ok(json) => json,
            Err(err) => {
                eprintln!("rollback failed to serialize the report: {err}");
                return 1;
            }
        };
        println!("{json}");
        println!(
            "restored={} already_restored={} failed={} skipped={}",
            rollback.counts.restored,
            rollback.counts.already_restored,
            rollback.counts.failed,
            rollback.counts.skipped
        );
        if rollback.counts.failed > 0 {
            1
        } else {
            0
        }
    })
}

/// The B8 cleanup execute decision, pure for the test: a real delete needs
/// BOTH the operator flag (`--execute`) and the deployment gate
/// (`substore.allow_delete` after the environment merge at config load).
fn clean_execute_decision(execute: bool, gate_open: bool) -> bool {
    execute && gate_open
}

/// `migrate clean [--prefix P] [--ipmap-prefix P] [--report FILE |
/// --plan FILE] [--execute]`: scan the backend for 测活产物, classify, and
/// (only with `--execute` AND the deployment gate) back up and delete.
/// Without `--execute` this is a plan-plus-dry-run: the plan prints and the
/// report records what WOULD happen, with zero write requests built.
///
/// `--plan FILE` applies a reviewed plan instead of a fresh scan; the gate
/// check runs either way.
fn migrate_clean(
    root: &Path,
    prefix: Option<&str>,
    ipmap_prefix: Option<&str>,
    plan_file: Option<&Path>,
    report_file: Option<&Path>,
    execute: bool,
) -> i32 {
    let cfg = load(root);
    // The operator asked for a delete; the decision table decides whether
    // the deployment actually grants one.
    let delete_allowed = clean_execute_decision(execute, cfg.substore.allow_delete);
    if execute && !delete_allowed {
        eprintln!(
            "clean --execute refused: substore.allow_delete is false (set it in config, or \
             ALLOW_SUBSTORE_DELETE=1). Plans and dry runs run without it."
        );
        return 2;
    }
    let rules = CleanupRules {
        push_prefix: prefix
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| cfg.publish_prefix.clone()),
        ipmap_prefix: ipmap_prefix
            .map(|prefix| prefix.trim().to_string())
            .filter(|prefix| !prefix.is_empty())
            .unwrap_or_else(|| probe_source::DEFAULT_IPMAP_PREFIX.to_string()),
    };
    let dir = probe_source::backup_dir(root);
    if execute {
        println!(
            "REAL CLEAN: matched 测活产物 will be backed up to {} and then deleted",
            dir.display()
        );
    }
    let backend = migrate_backend(root, &cfg);
    tokio_block(async move {
        let store = SubStoreClient::new(&backend);
        let plan: CleanupPlan = match plan_file {
            Some(path) => {
                let text = match std::fs::read_to_string(path) {
                    Ok(text) => text,
                    Err(err) => {
                        eprintln!("cannot read {}: {err}", path.display());
                        return 1;
                    }
                };
                match serde_json::from_str(&text) {
                    Ok(plan) => plan,
                    Err(err) => {
                        eprintln!("{} is not a cleanup plan: {err}", path.display());
                        return 1;
                    }
                }
            }
            None => {
                match probe_source::scan_cleanup(&store, &rules, CleanupCaps::default()).await {
                    Ok(plan) => plan,
                    Err(err) => {
                        eprintln!("clean scan failed: {err}");
                        return 1;
                    }
                }
            }
        };
        let plan_json = match serde_json::to_string_pretty(&plan) {
            Ok(json) => json,
            Err(err) => {
                eprintln!("clean failed to serialize the plan: {err}");
                return 1;
            }
        };
        if let Some(report) = report_file {
            match std::fs::write(report, format!("{plan_json}\n")) {
                Ok(()) => println!("plan written: {}", report.display()),
                Err(err) => {
                    eprintln!("cannot write {}: {err}", report.display());
                    return 1;
                }
            }
        }
        println!("{plan_json}");
        println!(
            "counts: keep={} duplicate={} uncertain={}",
            plan.counts.keep, plan.counts.duplicate, plan.counts.uncertain
        );
        let report = match probe_source::apply_cleanup(&plan, &store, &dir, execute).await {
            Ok(report) => report,
            Err(err) => {
                eprintln!("clean apply failed: {err}");
                return 1;
            }
        };
        let report_json = match serde_json::to_string_pretty(&report) {
            Ok(json) => json,
            Err(err) => {
                eprintln!("clean failed to serialize the report: {err}");
                return 1;
            }
        };
        println!("{report_json}");
        if report.dry_run {
            println!("dry run: nothing was written (re-run with --execute to apply)");
        } else {
            println!(
                "deleted={} skipped={} failed={} backup={}",
                report.deleted.len(),
                report.skipped.len(),
                report.failed.len(),
                report.backup_file.as_deref().unwrap_or("-")
            );
        }
        if report.failed.is_empty() {
            0
        } else {
            1
        }
    })
}

/// `migrate clean-rollback --backup NAME`: recreate the subs a previous
/// `migrate clean --execute` deleted, from the backup document it wrote
/// under `<root>/data/cleanup-backups/`. The name is jailed to that
/// directory by `read_backup`.
fn migrate_clean_rollback(root: &Path, backup_file: Option<&str>) -> i32 {
    let Some(name) = backup_file.map(str::trim).filter(|name| !name.is_empty()) else {
        eprintln!(
            "clean-rollback needs --backup <file name> (the backup_file a previous \
             clean --execute reported)"
        );
        return 2;
    };
    let dir = probe_source::backup_dir(root);
    let backup = match probe_source::read_backup(&dir, name) {
        Ok(backup) => backup,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    let cfg = load(root);
    let backend = migrate_backend(root, &cfg);
    println!(
        "restoring {} entries from {name} (upsert writes; recreates what was deleted)",
        backup
            .get("entries")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len)
            .unwrap_or(0)
    );
    tokio_block(async move {
        let store = SubStoreClient::new(&backend);
        let report = probe_source::rollback_cleanup(&backup, &store).await;
        let json = match serde_json::to_string_pretty(&report) {
            Ok(json) => json,
            Err(err) => {
                eprintln!("clean-rollback failed to serialize the report: {err}");
                return 1;
            }
        };
        println!("{json}");
        println!(
            "restored={} failed={}",
            report.restored.len(),
            report.failed.len()
        );
        if report.failed.is_empty() {
            0
        } else {
            1
        }
    })
}

/// The backend host:port only. The full URL (including the secret backend
/// path) is a credential and never printed.
fn backend_display(backend: &str) -> String {
    let trimmed = backend.trim_end_matches('/');
    match (trimmed.find("://"), trimmed.find('/')) {
        (Some(scheme_end), Some(path_start)) if path_start > scheme_end + 2 => {
            format!("{} (path redacted)", &trimmed[..path_start])
        }
        _ => trimmed.to_string(),
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
                min_run_interval_seconds: cfg.schedule.min_run_interval_seconds,
                substore_allow_delete: cfg.substore.allow_delete,
                probes: cfg.probes.clone(),
                substore_target_name: cfg.substore.target_name.clone(),
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

#[cfg(test)]
mod gist_init_tests {
    use super::{gist_init_decision, GistInitDecision};

    #[test]
    fn an_existing_healthy_gist_id_is_already_initialized() {
        assert_eq!(
            gist_init_decision(Some("gist0001abcd"), Some(true)),
            GistInitDecision::AlreadyInitialized
        );
        // Whitespace around the stored id does not defeat the guard.
        assert_eq!(
            gist_init_decision(Some("  gist0001abcd  "), Some(true)),
            GistInitDecision::AlreadyInitialized
        );
    }

    /// Every other combination creates: no id, a blank id, a failed or
    /// unknown healthcheck. The create itself stays one POST (the publisher
    /// never retries); re-running `init` is the operator's retry.
    #[test]
    fn everything_else_needs_a_create() {
        assert_eq!(
            gist_init_decision(None, Some(true)),
            GistInitDecision::Create
        );
        assert_eq!(
            gist_init_decision(Some(""), Some(true)),
            GistInitDecision::Create,
            "a blank stored id reads as unconfigured"
        );
        assert_eq!(
            gist_init_decision(Some("   "), None),
            GistInitDecision::Create
        );
        assert_eq!(
            gist_init_decision(Some("gist0001abcd"), Some(false)),
            GistInitDecision::Create
        );
        assert_eq!(
            gist_init_decision(Some("gist0001abcd"), None),
            GistInitDecision::Create
        );
    }
}

#[cfg(test)]
mod clean_gate_tests {
    use super::clean_execute_decision;

    /// B8: a real cleanup delete is the AND of the operator flag and the
    /// deployment gate -- either alone must never be enough, and a dry run
    /// runs whatever the gate says.
    #[test]
    fn execution_needs_both_the_flag_and_the_gate() {
        assert!(clean_execute_decision(true, true));
        assert!(
            !clean_execute_decision(true, false),
            "flag without the gate"
        );
        assert!(
            !clean_execute_decision(false, true),
            "gate without the flag"
        );
        assert!(!clean_execute_decision(false, false));
    }
}
