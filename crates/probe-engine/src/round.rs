//! Round orchestration: plan in, closed round row out.
//!
//! Before this module the round path existed twice -- once in `probe-cli`'s
//! `cmd_round`, once in `probe-api`'s `start_round` -- and both were the same
//! placeholder: probe the controller, reload, close the row. Neither touched
//! the concurrency gate (`crate::limits`), so the gate had no callers and no
//! way to prove itself against a real round.
//!
//! [`run_round`] is that path, once. It owns four things:
//!
//! 1. **The round row.** The caller opens it (`Ledger::start_round`) -- the API
//!    must answer `POST /api/v1/rounds` with the id, so it cannot wait for a
//!    spawned task. [`run_round`] closes it, last, always. A row left open is
//!    an orphan (`db.open_rounds()`), and the Python service treats that as a
//!    crashed round -- so closing is not conditional on success.
//! 2. **Kernel preparation.** Injected as [`KernelPrep`]; when the controller
//!    is gone the node phase is skipped, because a test through a missing
//!    kernel measures nothing and would advance every node's failure streak.
//! 3. **The node phase**, driven through [`RoundCtx`] so every test occupies a
//!    slot on all three layers.
//! 4. **The ledger write**, guarded by [`RoundCtx::check`].
//!
//! ## Cancellation: two different "stop"s
//!
//! Cancellation is checked while preparing/testing and immediately before
//! committing measurements. A cancelled round drops its observations. Once
//! the synchronous storage transaction starts, it completes atomically;
//! cancellation arriving after that boundary cannot undo a committed round.
//!
//! It does **not** skip closing the round row. "Do not write" and "do not
//! leave the ledger broken" are separate rules, and conflating them is how a
//! cancel turns into an orphan row that the next start-up has to reap.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use probe_domain::{DomainError, DomainPass, DomainResult, RoundId};
use probe_mihomo::{Controller, DelayOutcome, KernelError};
use probe_source::Role;
use probe_storage::{ResultRow, Storage, VERDICT_EXCLUDED, VERDICT_FAIL, VERDICT_OK};

use crate::limits::{Job, Limits, RoundCtx};

/// Python `engine.py` truncates the per-result detail with `detail[:200]`;
/// matching the bound keeps the two ledgers' `detail` columns comparable.
const DETAIL_LIMIT: usize = 200;
/// Kernel/controller error text is bounded the same way Python bounds it
/// (`str(exc)[:160]`), and for the same reason: an unbounded error string can
/// carry a URL with credentials into the ledger.
const ERROR_LIMIT: usize = 160;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Truncate on a character boundary. Byte slicing would panic on the first
/// multi-byte character, and node names in this project are routinely CJK.
fn bounded(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// What a tester decided about one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Alive { delay_ms: u32 },
    Dead { kind: String, message: String },
    Excluded { kind: String },
}

/// One job's outcome, carrying the job so the ledger row can be built without
/// a side lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeVerdict {
    pub job: Job,
    pub verdict: Verdict,
    /// Dials spent on this verdict (Python `attempts`). A single-shot tester
    /// reports 1.
    pub attempts: u32,
    /// The target the final dial used. Not a ledger column -- Python does not
    /// store it either -- but it is what makes "timed out on gstatic but
    /// answered on cloudflare" debuggable.
    pub url: Option<String>,
    pub exit: Option<probe_domain::ExitIdentity>,
    pub verification_error: Option<String>,
    pub verification_bytes: u64,
}

impl NodeVerdict {
    pub fn alive(job: Job, delay_ms: u32) -> Self {
        Self {
            job,
            verdict: Verdict::Alive { delay_ms },
            attempts: 1,
            url: None,
            exit: None,
            verification_error: None,
            verification_bytes: 0,
        }
    }

    pub fn dead(job: Job, kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            job,
            verdict: Verdict::Dead {
                kind: kind.into(),
                message: message.into(),
            },
            attempts: 1,
            url: None,
            exit: None,
            verification_error: None,
            verification_bytes: 0,
        }
    }

    pub fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
        self
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Record an entry-policy exclusion without reporting a connection attempt.
    pub fn excluded(job: Job, kind: impl Into<String>) -> Self {
        Self {
            job,
            verdict: Verdict::Excluded { kind: kind.into() },
            attempts: 0,
            url: None,
            exit: None,
            verification_error: None,
            verification_bytes: 0,
        }
    }

    pub fn is_alive(&self) -> bool {
        matches!(self.verdict, Verdict::Alive { .. }) && self.verification_error.is_none()
    }

    /// Represent an unmeasured dependency without manufacturing a failed dial.
    pub fn deferred(job: Job, reason: &str) -> Self {
        let mut verdict = Self::dead(job, reason, "").with_attempts(0);
        verdict.verification_error = Some(reason.to_string());
        verdict
    }

    /// Whether this observation is a classification rather than a failed dial.
    pub fn is_excluded(&self) -> bool {
        matches!(self.verdict, Verdict::Excluded { .. })
    }

    /// The failure class, `None` when the node answered.
    pub fn reason(&self) -> Option<&str> {
        match &self.verdict {
            Verdict::Alive { .. } => None,
            Verdict::Dead { kind, .. } | Verdict::Excluded { kind } => Some(kind),
        }
    }

    /// The shared `results` row. Vocabulary copied from Python: `ok` / `fail`,
    /// `detail` bounded to 200 chars.
    pub fn to_result_row(&self) -> ResultRow {
        let mut row = ResultRow {
            source: self.job.source_id.clone(),
            fingerprint: self.job.fingerprint.clone(),
            display: Some(self.job.display.clone()),
            attempts: Some(i64::from(self.attempts)),
            detail: Some(String::new()),
            category: Some(self.job.variant.clone()),
            endpoint_ip: self.job.test_ip.clone(),
            original_server: self.job.original_server.clone(),
            protocol: self.job.protocol.clone(),
            dns_incomplete: self.job.dns_incomplete,
            exit_ip: self.exit.as_ref().and_then(|identity| identity.ip.clone()),
            exit_asn: self.exit.as_ref().and_then(|identity| identity.asn.clone()),
            verification_error: self.verification_error.clone(),
            verification_bytes: self.verification_bytes.min(i64::MAX as u64) as i64,
            ..Default::default()
        };
        match &self.verdict {
            Verdict::Alive { delay_ms } => {
                row.verdict = Some(VERDICT_OK.into());
                row.delay_ms = Some(i64::from(*delay_ms));
            }
            Verdict::Dead { kind, message } => {
                row.verdict = Some(VERDICT_FAIL.into());
                row.reason = Some(kind.clone());
                row.detail = Some(bounded(message, DETAIL_LIMIT));
            }
            Verdict::Excluded { kind } => {
                row.verdict = Some(VERDICT_EXCLUDED.into());
                row.reason = Some(kind.clone());
            }
        }
        if let Some(error) = &self.verification_error {
            row.verdict = Some("unverified".into());
            row.reason = Some("verify_failed".into());
            row.detail = Some(bounded(error, DETAIL_LIMIT));
        }
        row.country = self
            .exit
            .as_ref()
            .and_then(|identity| identity.country.clone());
        row
    }
}

/// Runs one node test. Implemented by [`KernelDelayTester`] for real rounds
/// and by a fake in the tests, so the gate and the orchestration are testable
/// without a kernel.
///
/// The future is boxed rather than an `async fn`: a trait with an RPITIT method
/// is not dyn-compatible, and this one has to be a trait object -- the runner
/// hands the same tester to every spawned task.
pub trait NodeTester: Send + Sync {
    fn test<'a>(&'a self, job: Job) -> BoxFuture<'a, NodeVerdict>;
    /// Enrich a completed batch without another proxy dial (for example, ASN lookup).
    fn enrich<'a>(&'a self, _verdicts: &'a mut [NodeVerdict]) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// G07: bounded throughput samples for a spread subset of alive nodes.
    /// The default measures nothing -- only the lane-aware tester can.
    fn sample_throughput<'a>(
        &'a self,
        _verdicts: &'a [NodeVerdict],
    ) -> BoxFuture<'a, Vec<crate::verify::ThroughputSample>> {
        Box::pin(async { Vec::new() })
    }
}

/// The ledger side. [`Mutex<Storage>`] implements it (see below); the runner
/// only ever sees this trait, so its tests need no database file.
pub trait Ledger: Send + Sync {
    fn start_round(&self, trigger: &str, mode: Option<&str>) -> DomainResult<RoundId>;
    /// Atomically record verdicts and fold nodes/history under policy; return
    /// the result count and transitions, or leave all three tables unchanged.
    fn commit_verdicts(
        &self,
        round_id: RoundId,
        verdicts: &[NodeVerdict],
        nodes: &[probe_storage::ConvergeNode],
        policy: &probe_domain::Policy,
    ) -> DomainResult<(usize, probe_storage::ConvergenceSummary)>;
    /// The alive baseline for the suspect guard (last finished round's ok
    /// count, skipping suspect and inconclusive rounds).
    fn previous_alive_count(&self) -> DomainResult<i64>;
    /// Close the round. Called on every path, including cancellation.
    fn finish_round(
        &self,
        round_id: RoundId,
        note: &str,
        counts: RoundCounts,
        suspect: bool,
        inconclusive: bool,
    ) -> DomainResult<()>;

    /// Close the round and record its measured wall-clock duration in seconds.
    ///
    /// The duration is an observation, not a precondition: the default body
    /// closes without it, so a ledger with nowhere to put the value keeps
    /// working. The runner measures before it calls, so nothing about the
    /// clock can fail here -- and an implementor that stores the duration must
    /// keep that property: a bad measurement may be dropped, never a close.
    fn finish_round_timed(
        &self,
        round_id: RoundId,
        note: &str,
        counts: RoundCounts,
        suspect: bool,
        inconclusive: bool,
        duration_s: f64,
    ) -> DomainResult<()> {
        let _ = duration_s;
        self.finish_round(round_id, note, counts, suspect, inconclusive)
    }

    /// Every node's stored status, keyed by (source, fingerprint).
    ///
    /// The default returns nothing: a ledger that cannot answer makes every
    /// node look unmeasured, which only changes ordering -- never a verdict.
    fn node_statuses(&self) -> DomainResult<std::collections::HashMap<(String, String), String>> {
        Ok(std::collections::HashMap::new())
    }

    /// Record G07 throughput samples for this round. Best effort by contract:
    /// the default body stores nothing, and the runner logs a failure instead
    /// of failing the round -- a sample is an observation, not a measurement
    /// the ledger must accept.
    fn record_quality_samples(
        &self,
        round_id: RoundId,
        samples: &[probe_storage::QualitySampleRow],
    ) -> DomainResult<usize> {
        let _ = (round_id, samples);
        Ok(0)
    }
}

/// What the kernel was able to do before the node phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelState {
    Reloaded,
    /// The controller refused this round's config. Rust has no recreate
    /// fallback yet: testing stale aliases would measure different nodes.
    Refused,
    /// The controller is gone. Nothing can be measured through it.
    Unreachable(String),
    /// There was no config to load at all -- generation failed. Distinct from
    /// `Unreachable`: blaming the controller for a local write error sends an
    /// operator looking in the wrong place.
    NotPrepared(String),
}

/// Brings the kernel up on this round's config.
pub trait KernelPrep: Send + Sync {
    fn prepare(&self) -> BoxFuture<'_, KernelState>;
}

/// [`KernelPrep`] against a real controller: `GET /version`, then
/// `PUT /configs?force=true`.
pub struct ControllerPrep {
    pub controller: Controller,
    pub config_path: String,
}

impl ControllerPrep {
    pub fn new(controller: Controller, config_path: impl Into<String>) -> Self {
        Self {
            controller,
            config_path: config_path.into(),
        }
    }
}

impl KernelPrep for ControllerPrep {
    fn prepare(&self) -> BoxFuture<'_, KernelState> {
        Box::pin(async move {
            if let Err(err) = self.controller.version().await {
                return KernelState::Unreachable(bounded(&err.to_string(), ERROR_LIMIT));
            }
            match self.controller.reload(&self.config_path).await {
                Ok(true) => KernelState::Reloaded,
                Ok(false) => KernelState::Refused,
                Err(err) => KernelState::Unreachable(bounded(&err.to_string(), ERROR_LIMIT)),
            }
        })
    }
}

/// A [`KernelPrep`] for a round that never got a config to load.
///
/// Used when config generation itself failed: the round still has to open and
/// close a row, and the reason must not read as a controller outage.
pub struct NotPrepared(pub String);

impl KernelPrep for NotPrepared {
    fn prepare(&self) -> BoxFuture<'_, KernelState> {
        Box::pin(async move { KernelState::NotPrepared(self.0.clone()) })
    }
}

/// One `GET /proxies/{name}/delay`, classified exactly as Python `core.delay`
/// classifies it.
///
/// Returns `(delay_ms, reason, detail)`: `reason` is `None` when the node
/// answered. `controller_error` is a *node-level* verdict here because every
/// job needs one, but Python deliberately keeps it out of the terminal reasons
/// so a retry can overturn it -- the round-level guard (workstreams/03) is what
/// stops it being read as "this node is dead".
pub(crate) async fn dial_once(
    controller: &Controller,
    proxy: &str,
    url: &str,
    timeout_ms: u64,
    expected: &str,
) -> (Option<u32>, Option<String>, String) {
    match controller.delay(proxy, url, timeout_ms, expected).await {
        Ok(DelayOutcome::Ok(delay_ms)) => (Some(delay_ms), None, String::new()),
        Ok(DelayOutcome::Failed { kind, message }) => {
            (None, Some(kind), bounded(&message, ERROR_LIMIT))
        }
        Err(KernelError::Node { kind, message }) => {
            (None, Some(kind), bounded(&message, ERROR_LIMIT))
        }
        Err(KernelError::Controller(message)) => (
            None,
            Some("controller_error".to_string()),
            bounded(&message, ERROR_LIMIT),
        ),
    }
}

/// What a round needs from the deployment config, assembled in one place.
///
/// Both entry points (`probe-cli round`, `POST /api/v1/rounds`) need the same
/// values and the same "is there a config to load" decision. Building them
/// separately is how the two paths drifted apart in the first place.
#[derive(Debug, Clone)]
pub struct RoundSettings {
    pub limits: Limits,
    /// The `test` section verbatim: the tester needs the whole retry policy,
    /// not just the first target.
    pub test: probe_config::TestConfig,
    /// Set when there is no kernel config to load at all (generation failed).
    /// The round then opens and closes a row saying so instead of testing
    /// anything. This is **not** set for an all-HTTP target list: Python tests
    /// those.
    pub blocked: Option<String>,
    pub dns: probe_config::DnsSection,
    pub verify: probe_config::VerifySection,
    pub watchdog: probe_config::WatchdogSection,
    /// G07 quality sampling policy, carried so the verified tester can opt in.
    pub quality: probe_config::QualitySection,
}

impl RoundSettings {
    pub fn from_config(cfg: &probe_config::Config) -> Self {
        Self {
            limits: Limits::from_concurrency(cfg.test.concurrency),
            test: cfg.test.clone(),
            blocked: None,
            dns: cfg.dns.clone(),
            verify: cfg.verify.clone(),
            watchdog: cfg.watchdog.clone(),
            quality: cfg.quality.clone(),
        }
    }

    /// The kernel prep for this round: a real one, or the blocked reason.
    pub fn kernel_prep(&self, controller: Controller, config_path: &str) -> Arc<dyn KernelPrep> {
        match &self.blocked {
            Some(reason) => Arc::new(NotPrepared(reason.clone())),
            None => Arc::new(ControllerPrep::new(controller, config_path)),
        }
    }

    /// The node tester for this round: the full `test_one` policy.
    pub fn tester(&self, controller: Controller) -> Arc<dyn NodeTester> {
        Arc::new(crate::measure::TestOne::new(controller, &self.test))
    }

    /// Use actual payload/exit checks with lane assignments matching the rendered proxies.
    pub fn verified_tester(
        &self,
        controller: Controller,
        core: &probe_config::CoreConfig,
        proxies: &[serde_json::Value],
    ) -> Arc<dyn NodeTester> {
        let inner = self.tester(controller.clone());
        Arc::new(
            crate::verify::VerifiedTester::new(inner, controller, core, proxies, &self.verify)
                .with_quality(self.quality.clone()),
        )
    }
}

/// Round counters, written onto the `rounds` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoundCounts {
    pub total: i64,
    pub ok: i64,
    pub failed: i64,
    /// Nodes the convergence pass demoted to dead this round.
    pub dropped: i64,
    /// Dead nodes that came back this round (`Restore`).
    pub restored: i64,
    pub excluded: i64,
    pub inconclusive: i64,
}

/// The round guard's verdict (workstreams/03, `GuardDecision`). One value per
/// round, decided before anything is written:
///
/// * [`GuardDecision::ApplyConvergence`] -- the round ran on a healthy kernel
///   and stayed above the alive floor: fold every verdict into the nodes
///   table and publish as usual.
/// * [`GuardDecision::PreservePreviousState`] -- the round ran but its alive
///   count fell below the floor (`round_is_suspect`). The convergence STILL
///   lands in the ledger -- the streaks are facts -- but the round is marked
///   `suspect` and the previous round's publication is preserved (the
///   publish half of that is R8; the marker and the skipped baseline are R7).
/// * [`GuardDecision::MarkRoundInconclusive`] -- nothing usable was measured
///   or committed (ADR-0005 write conditions: kernel unreachable, refused
///   reload, empty measurement, cancellation, worker panic, controller_error,
///   ledger failure). Uncertain nodes keep their state; independently
///   measured nodes may converge. The round is never used as the next global
///   baseline. A source whose *fetch* failed is deliberately not on this
///   list: its nodes simply keep their previous state, and the note names
///   the source instead -- one dead upstream must not flag every round
///   inconclusive, which would disable the suspect alive-floor guard and
///   leave the baseline chain empty (2026-10-21 correction; see the close
///   path in `close_round`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardDecision {
    ApplyConvergence,
    PreservePreviousState,
    MarkRoundInconclusive,
}

/// Statuses that mean "this node has no measurement worth trusting yet".
const UNMEASURED_STATUSES: [&str; 2] = ["pending", "unknown"];

/// Move nodes with no measurement to the front, keeping the rest in order.
///
/// A stable sort by rank only: jobs within a group keep the order the caller
/// produced, so a source's nodes stay together and the round stays
/// reproducible. A node absent from the ledger ranks with the unmeasured ones
/// -- it has never been tested either.
fn prioritize_unmeasured(plan: &mut RoundPlan, ledger: &dyn Ledger) {
    let Ok(statuses) = ledger.node_statuses() else {
        return;
    };
    if statuses.is_empty() || plan.jobs.is_empty() {
        return;
    }
    plan.jobs.sort_by_key(|job| {
        let key = (job.source_id.clone(), job.fingerprint.clone());
        match statuses.get(&key) {
            None => 0,
            Some(status) if UNMEASURED_STATUSES.contains(&status.as_str()) => 0,
            Some(_) => 1,
        }
    });
    let unmeasured = plan
        .jobs
        .iter()
        .filter(|job| {
            let key = (job.source_id.clone(), job.fingerprint.clone());
            match statuses.get(&key) {
                None => true,
                Some(status) => UNMEASURED_STATUSES.contains(&status.as_str()),
            }
        })
        .count();
    tracing::info!(
        unmeasured,
        measured = plan.jobs.len().saturating_sub(unmeasured),
        "unmeasured nodes scheduled first"
    );
}

/// What to run this round.
#[derive(Debug, Clone)]
pub struct RoundPlan {
    pub trigger: String,
    pub mode: Option<String>,
    pub jobs: Vec<Job>,
    /// Convergence thresholds and the guard floor. The runner folds every
    /// verdict under this policy; nothing reads a hard-coded value.
    pub policy: probe_domain::Policy,
    pub domain_pass: DomainPass,
    pub preserved_sources: Vec<String>,
}

/// What happened.
#[derive(Debug, Clone)]
pub struct RoundOutcome {
    pub round_id: RoundId,
    pub kernel: KernelState,
    pub counts: RoundCounts,
    pub cancelled: bool,
    /// 0 when the round was cancelled: results are dropped, not deferred.
    pub results_written: usize,
    pub note: String,
    pub verdicts: Vec<NodeVerdict>,
    /// Fronts that carried traffic in the first phase. 0 without chaining,
    /// and 0 on the round-level "front pool is down" condition.
    pub live_fronts: usize,
    /// Chain nodes failed `front_dead` without dialling (deduplicated by
    /// ledger identity).
    pub front_dead: usize,
    /// The guard's verdict for this round.
    pub guard: GuardDecision,
    /// True when the alive count fell below the floor: the convergence still
    /// landed, but the publication is preserved (R8 reads this flag).
    pub suspect: bool,
    /// Convergence transition stats, from the fold.
    pub dropped_nodes: i64,
    pub restored_nodes: i64,
    /// These source exports must retain their previous snapshots this round.
    pub preserved_sources: Vec<String>,
}

/// Round a measured second count to one decimal, like Python's
/// `round(elapsed, 1)` -- the panel prints the stored value directly.
///
/// The clamp is the contract's other half: anything a clock could report that
/// is not a duration (negative, NaN, infinite) becomes 0, so a broken
/// measurement can never reach the ledger as a value or fail a close.
fn measured_seconds(seconds: f64) -> f64 {
    let tenths = (seconds * 10.0).round();
    if !tenths.is_finite() || tenths < 0.0 {
        return 0.0;
    }
    tenths / 10.0
}

/// Run one round with a fresh gate.
///
/// `round_id` is opened by the caller (`Ledger::start_round`) rather than here,
/// because the API has to answer `POST /api/v1/rounds` with the id and cannot
/// wait for a spawned task to produce it. The other half of the contract is
/// this function's: it closes that row on **every** path.
pub async fn run_round(
    round_id: RoundId,
    plan: RoundPlan,
    limits: Limits,
    kernel: Arc<dyn KernelPrep>,
    tester: Arc<dyn NodeTester>,
    ledger: Arc<dyn Ledger>,
) -> DomainResult<RoundOutcome> {
    run_round_with_ctx(
        round_id,
        plan,
        RoundCtx::new(limits),
        kernel,
        tester,
        ledger,
    )
    .await
}

/// Run one round on a caller-supplied [`RoundCtx`].
///
/// The caller owning the context is what makes cancellation reachable from
/// outside the round: a scheduler holds the same `RoundCtx`, so `cancel()` on
/// it stops this round. It is also how the tests drive the cancel path without
/// a scheduler.
pub async fn run_round_with_ctx(
    round_id: RoundId,
    plan: RoundPlan,
    ctx: RoundCtx,
    kernel: Arc<dyn KernelPrep>,
    tester: Arc<dyn NodeTester>,
    ledger: Arc<dyn Ledger>,
) -> DomainResult<RoundOutcome> {
    // The round's wall clock, read once: Python takes `started = time.time()`
    // at the top of its round, and the panel's duration is measured the same
    // way. `SystemTime` is infallible to read; a stepped-back clock is handled
    // where the elapsed value is turned into seconds.
    let started = std::time::SystemTime::now();
    let mut plan = plan;
    // Nodes nobody has measured run first. Nothing here changes what is
    // measured, only the order: a round that is cancelled or runs into its
    // deadline still makes progress on the nodes with no history instead of
    // re-testing the head of the list every time.
    prioritize_unmeasured(&mut plan, ledger.as_ref());
    let planned = plan.jobs.len();

    let kernel_state = tokio::select! {
        biased;
        _ = ctx.cancel_token().cancelled() => {
            KernelState::NotPrepared("round cancelled during kernel preparation".into())
        }
        state = kernel.prepare() => state,
    };

    // Only a confirmed reload binds this plan's aliases to this plan's nodes.
    let mut verdicts = match kernel_state {
        KernelState::Reloaded => run_phases(&plan.jobs, &ctx, &tester).await,
        _ => Vec::new(),
    };
    tokio::select! {
        biased;
        _ = ctx.cancel_token().cancelled() => {},
        _ = tester.enrich(&mut verdicts) => {},
    }
    // G07 quality sampling: only after enrichment, so it sees the nodes that
    // actually survived the round, and cancellable like every other phase.
    let quality_samples = tokio::select! {
        biased;
        _ = ctx.cancel_token().cancelled() => Vec::new(),
        samples = tester.sample_throughput(&verdicts) => samples,
    };

    // Python `_test_phases`' two outputs, recomputed from the verdicts: the
    // fronts that carried traffic, and the chains that were failed untried.
    // A front's verdict has to exist before a chain's verdict can mean
    // anything, so "front pool down" is readable right off the outcome.
    let live_fronts = verdicts
        .iter()
        .filter(|v| v.job.role == Role::Front && v.is_alive())
        .map(|v| v.job.proxy_name.clone())
        .collect::<HashSet<_>>()
        .len();
    let folded = fold_verdicts(&verdicts, plan.domain_pass);
    let front_dead = folded
        .iter()
        .filter(|node| !node.is_inconclusive && node.reason.as_deref() == Some(FRONT_DEAD))
        .count();
    let ok = folded.iter().filter(|node| node.ok).count();
    let excluded = folded.iter().filter(|node| node.is_excluded).count();
    let uncertain = folded.iter().filter(|node| node.is_inconclusive).count();
    let mut counts = RoundCounts {
        total: folded.len() as i64,
        ok: ok as i64,
        failed: (folded.len() - ok - excluded - uncertain) as i64,
        dropped: 0,
        restored: 0,
        excluded: excluded as i64,
        inconclusive: uncertain as i64,
    };
    let mut preserved_sources = plan.preserved_sources.clone();
    preserved_sources.extend(
        folded
            .iter()
            .filter(|node| node.is_inconclusive)
            .map(|node| node.source.clone()),
    );
    preserved_sources.sort();
    preserved_sources.dedup();

    let controller_failed = verdicts
        .iter()
        .any(|verdict| verdict.reason() == Some("controller_error"));
    let committed = if verdicts.is_empty() || controller_failed {
        Ok(None)
    } else {
        persist_measurements(
            round_id,
            &verdicts,
            &folded,
            &plan.policy,
            &ctx,
            ledger.as_ref(),
        )
    };
    // Keep every storage failure until after the unconditional close.
    let (committed, write_error) = match committed {
        Ok(committed) => (committed, None),
        Err(err) => (None, Some(err)),
    };
    let has_commit = committed.is_some();
    // ADR-0005 write conditions, and nothing else: a round is inconclusive
    // when nothing was committed (kernel unreachable, refused reload,
    // cancellation, worker panic, controller_error, ledger failure) or when
    // nothing was measured at all. A source whose fetch failed is NOT on
    // that list -- every node that WAS measured is still a valid measurement.
    // Marking such a round inconclusive on a host with one permanently
    // broken upstream left every round inconclusive, which (a) disabled the
    // suspect alive-floor guard, because `suspect` is only evaluated on
    // conclusive rounds, and (b) left `previous_alive_count` with no usable
    // baseline at all. Fetch-failed sources keep their previous node state;
    // the note names them instead.
    let inconclusive = !has_commit || counts.ok + counts.failed == 0;
    let cancelled = !has_commit && ctx.is_cancelled();
    let committed = committed.unwrap_or_default();
    let convergence = committed.convergence;
    counts.dropped = convergence.dropped;
    counts.restored = convergence.restored;
    let suspect = !inconclusive
        && probe_domain::round_is_suspect(counts.ok, committed.alive_previous, &plan.policy);
    let guard = if inconclusive {
        GuardDecision::MarkRoundInconclusive
    } else if suspect {
        GuardDecision::PreservePreviousState
    } else {
        GuardDecision::ApplyConvergence
    };
    let mut note = compose_note(&kernel_state, counts, cancelled, planned, verdicts.len());
    if suspect {
        note.push_str(&format!(
            "; suspect: alive {} below floor from previous {}; publish preserved",
            counts.ok, committed.alive_previous
        ));
    }
    if write_error.is_some() {
        note.push_str("; ledger update failed; node state preserved");
    } else if controller_failed {
        note.push_str("; controller failed during testing; node state preserved");
    } else if inconclusive && !cancelled {
        note.push_str("; incomplete coverage; unmeasured node state preserved");
    }
    // A fetch-failed source is not an inconclusive round (see above), but the
    // operator reading the round row still needs to see the hole: name the
    // sources whose nodes kept their previous state this round.
    if !plan.preserved_sources.is_empty() {
        note.push_str(&format!(
            "; sources not fetched: {}",
            plan.preserved_sources.join(", ")
        ));
    }
    // Unconditional, cancellation included: see the module docs. The measured
    // duration rides this same close, and a clock that stepped backwards reads
    // as zero instead of turning a close into a failure.
    ledger.finish_round_timed(
        round_id,
        &note,
        counts,
        suspect,
        inconclusive,
        measured_seconds(started.elapsed().unwrap_or_default().as_secs_f64()),
    )?;
    // Samples ride along after the close: a sample is an observation, so a
    // ledger that cannot store one must not turn a finished round into a
    // failure. `Ledger` defaults to storing nothing for exactly this reason.
    if !quality_samples.is_empty() {
        let rows: Vec<probe_storage::QualitySampleRow> = quality_samples
            .iter()
            .map(|sample| probe_storage::QualitySampleRow {
                source: sample.source.clone(),
                fingerprint: sample.fingerprint.clone(),
                ts: String::new(),
                bytes: sample.bytes as i64,
                elapsed_ms: sample.elapsed_ms,
            })
            .collect();
        if let Err(err) = ledger.record_quality_samples(round_id, &rows) {
            tracing::warn!(%err, round_id, "quality samples were not recorded");
        }
    }
    // The row is closed; now the earlier failure, if any, is safe to report.
    if let Some(err) = write_error {
        return Err(err);
    }

    Ok(RoundOutcome {
        round_id,
        kernel: kernel_state,
        counts,
        cancelled,
        results_written: committed.results_written,
        note,
        verdicts,
        live_fronts,
        front_dead,
        guard,
        suspect,
        dropped_nodes: convergence.dropped,
        restored_nodes: convergence.restored,
        preserved_sources,
    })
}

#[derive(Default)]
struct CommittedMeasurements {
    results_written: usize,
    convergence: probe_storage::ConvergenceSummary,
    alive_previous: i64,
}

/// Read the baseline, then commit verdicts/folds atomically; None means cancelled.
fn persist_measurements(
    round_id: RoundId,
    verdicts: &[NodeVerdict],
    folded: &[FoldedNode],
    policy: &probe_domain::Policy,
    ctx: &RoundCtx,
    ledger: &dyn Ledger,
) -> DomainResult<Option<CommittedMeasurements>> {
    if ctx.is_cancelled() {
        return Ok(None);
    }
    let alive_previous = ledger.previous_alive_count()?;
    let nodes: Vec<_> = folded
        .iter()
        .filter(|node| !node.is_inconclusive)
        .map(|node| probe_storage::ConvergeNode {
            source: &node.source,
            fingerprint: &node.fingerprint,
            display: &node.display,
            category: node.category.as_deref(),
            ok: node.ok,
            delay_ms: node.delay_ms,
            reason: node.reason.as_deref(),
            is_excluded: node.is_excluded,
            proto: node.protocol.as_deref(),
            server: node.server.as_deref(),
            ip_alive: Some(node.ip_alive as i64),
            ip_total: Some(node.ip_total as i64),
            country: node
                .exit
                .as_ref()
                .and_then(|identity| identity.country.as_deref()),
            exit_ip: node
                .exit
                .as_ref()
                .and_then(|identity| identity.ip.as_deref()),
            exit_asn: node
                .exit
                .as_ref()
                .and_then(|identity| identity.asn.as_deref()),
        })
        .collect();
    // This is the commit boundary: cancellation during a synchronous commit
    // must not relabel an already committed round as cancelled.
    if ctx.is_cancelled() {
        return Ok(None);
    }
    let (results_written, convergence) =
        ledger.commit_verdicts(round_id, verdicts, &nodes, policy)?;
    Ok(Some(CommittedMeasurements {
        results_written,
        convergence,
        alive_previous,
    }))
}

/// Aggregate alternative fronts per endpoint, then apply the node's any/all policy.
fn fold_verdicts(verdicts: &[NodeVerdict], policy: DomainPass) -> Vec<FoldedNode> {
    let mut order = Vec::new();
    let mut folds: HashMap<(String, String), NodeFold> = HashMap::new();
    for verdict in verdicts {
        let key = (
            verdict.job.source_id.clone(),
            verdict.job.fingerprint.clone(),
        );
        if !folds.contains_key(&key) {
            order.push(key.clone());
            folds.insert(
                key.clone(),
                NodeFold {
                    job: verdict.job.clone(),
                    endpoints: HashMap::new(),
                    order: Vec::new(),
                    exclusion: None,
                    dns_incomplete: false,
                },
            );
        }
        folds
            .get_mut(&key)
            .expect("node fold exists")
            .observe(verdict);
    }
    order
        .into_iter()
        .map(|key| {
            folds
                .remove(&key)
                .expect("ordered node fold")
                .finish(policy)
        })
        .collect()
}

#[derive(Default)]
struct EndpointFold {
    delay: Option<i64>,
    reason: Option<String>,
    category: Option<String>,
    uncertain: bool,
    exit: Option<probe_domain::ExitIdentity>,
}

struct NodeFold {
    job: Job,
    endpoints: HashMap<String, EndpointFold>,
    order: Vec<String>,
    exclusion: Option<String>,
    dns_incomplete: bool,
}

impl NodeFold {
    /// Record a variant without counting alternative fronts as different endpoints.
    fn observe(&mut self, verdict: &NodeVerdict) {
        self.dns_incomplete |= verdict.job.dns_incomplete;
        if verdict.is_excluded() {
            self.exclusion = verdict.reason().map(str::to_string);
            return;
        }
        let endpoint = verdict.job.test_ip.clone().unwrap_or_default();
        if !self.endpoints.contains_key(&endpoint) {
            self.order.push(endpoint.clone());
        }
        let fold = self.endpoints.entry(endpoint).or_default();
        if verdict.verification_error.is_some() {
            fold.uncertain = true;
            return;
        }
        match &verdict.verdict {
            Verdict::Alive { delay_ms } => {
                let delay = i64::from(*delay_ms);
                if fold.delay.is_none_or(|best| delay < best) {
                    fold.delay = Some(delay);
                    fold.category = Some(verdict.job.variant.clone());
                    fold.exit = verdict.exit.clone();
                }
                fold.reason = None;
            }
            Verdict::Dead { kind, .. } if fold.delay.is_none() && fold.reason.is_none() => {
                fold.reason = Some(kind.clone());
            }
            _ => {}
        }
    }

    /// Derive one trustworthy node decision, retaining unknown coverage explicitly.
    fn finish(self, policy: DomainPass) -> FoldedNode {
        let ip_total = self.endpoints.len();
        let ip_alive = self
            .endpoints
            .values()
            .filter(|endpoint| endpoint.delay.is_some())
            .count();
        let is_excluded = ip_total == 0 && self.exclusion.is_some();
        let unknown_endpoint = self
            .endpoints
            .values()
            .any(|endpoint| endpoint.uncertain && endpoint.delay.is_none());
        let is_inconclusive = !is_excluded
            && (self.dns_incomplete || unknown_endpoint)
            && (policy == DomainPass::All || ip_alive == 0);
        let ok = !is_inconclusive && !is_excluded && policy.accepts(ip_alive, ip_total);
        let best = self
            .order
            .iter()
            .filter_map(|key| {
                let endpoint = &self.endpoints[key];
                endpoint
                    .delay
                    .map(|delay| (delay, endpoint.category.clone(), endpoint.exit.clone()))
            })
            .min_by_key(|(delay, _, _)| *delay);
        let reason = if ok {
            None
        } else if is_excluded {
            self.exclusion
        } else if is_inconclusive {
            Some(
                if self.dns_incomplete {
                    "dns_incomplete"
                } else {
                    "verify_failed"
                }
                .into(),
            )
        } else {
            self.order
                .iter()
                .filter_map(|key| {
                    let endpoint = &self.endpoints[key];
                    (endpoint.delay.is_none())
                        .then(|| endpoint.reason.clone())
                        .flatten()
                })
                .next()
        };
        let category = if ok {
            best.as_ref().and_then(|(_, category, _)| category.clone())
        } else if is_excluded {
            Some(self.job.variant.clone())
        } else {
            None
        };
        FoldedNode {
            source: self.job.source_id,
            fingerprint: self.job.fingerprint,
            display: self.job.display,
            server: self.job.original_server,
            protocol: self.job.protocol,
            category,
            ok,
            delay_ms: if ok {
                best.as_ref().map(|(delay, _, _)| *delay)
            } else {
                None
            },
            reason,
            is_excluded,
            is_inconclusive,
            ip_alive,
            ip_total,
            exit: if ok {
                best.and_then(|(_, _, identity)| identity)
            } else {
                None
            },
        }
    }
}

/// The owned per-node aggregation `fold_verdicts` produces; the ledger borrows
/// it as [`probe_storage::ConvergeNode`] for the duration of the fold.
struct FoldedNode {
    source: String,
    fingerprint: String,
    display: String,
    category: Option<String>,
    ok: bool,
    delay_ms: Option<i64>,
    reason: Option<String>,
    protocol: Option<String>,
    server: Option<String>,
    is_excluded: bool,
    is_inconclusive: bool,
    ip_alive: usize,
    ip_total: usize,
    exit: Option<probe_domain::ExitIdentity>,
}

/// The failure class Python records for a chain whose every front was dead:
/// the node is not "timing out", the front pool is.
const FRONT_DEAD: &str = "front_dead";

/// Test the jobs in two passes when the round has chains
/// (Python `_test_phases`): fronts and direct nodes first, then the chain
/// variants -- restricted to fronts that actually carried traffic. A chain
/// through a dead front cannot succeed, so dialling it would spend a timeout
/// per front to re-learn the front's own result.
///
/// A chain node whose every front is dead is *failed*, never measured direct:
/// `collect` already stripped that option when it expanded the variants.
/// The verdict is `front_dead` so the ledger blames the front, and
/// deduplicated by (source, fingerprint) so the node writes one row however
/// many dead-front variants it had.
async fn run_phases(
    jobs: &[Job],
    ctx: &RoundCtx,
    tester: &Arc<dyn NodeTester>,
) -> Vec<NodeVerdict> {
    let mut excluded = Vec::new();
    let mut active = Vec::new();
    for job in jobs {
        match &job.excluded_reason {
            Some(reason) => excluded.push(NodeVerdict::excluded(job.clone(), reason.clone())),
            None => active.push(job.clone()),
        }
    }
    excluded.extend(run_active_phases(&active, ctx, tester).await);
    excluded
}

/// Measure eligible jobs in front/direct and chain phases; exclusions never enter here.
async fn run_active_phases(
    jobs: &[Job],
    ctx: &RoundCtx,
    tester: &Arc<dyn NodeTester>,
) -> Vec<NodeVerdict> {
    if !jobs.iter().any(|job| job.role == Role::Chain) {
        return run_jobs(jobs, ctx, tester).await;
    }
    let first: Vec<Job> = jobs
        .iter()
        .filter(|job| job.role != Role::Chain)
        .cloned()
        .collect();
    let second: Vec<Job> = jobs
        .iter()
        .filter(|job| job.role == Role::Chain)
        .cloned()
        .collect();

    let mut out = run_jobs(&first, ctx, tester).await;
    if ctx.is_cancelled() {
        return out;
    }
    let known_fronts: HashSet<String> = out
        .iter()
        .filter(|verdict| verdict.job.role == Role::Front)
        .map(|verdict| verdict.job.proxy_name.clone())
        .collect();
    let uncertain_fronts: HashSet<String> = out
        .iter()
        .filter(|verdict| verdict.job.role == Role::Front && verdict.verification_error.is_some())
        .map(|verdict| verdict.job.proxy_name.clone())
        .collect();

    // Only fronts whose own test came back clean carry a chain.
    let live: HashSet<&str> = out
        .iter()
        .filter(|v| v.job.role == Role::Front && v.is_alive())
        .map(|v| v.job.proxy_name.as_str())
        .collect();
    let runnable: Vec<Job> = second
        .iter()
        .filter(|job| job.front.as_deref().is_some_and(|f| live.contains(f)))
        .cloned()
        .collect();
    if !runnable.is_empty() {
        out.extend(run_jobs(&runnable, ctx, tester).await);
    }

    // Every node with nothing left to try is failed outright -- once per
    // node, not once per dead-front variant.
    let tried: HashSet<(&str, &str, &str)> = runnable
        .iter()
        .map(|job| {
            (
                job.source_id.as_str(),
                job.fingerprint.as_str(),
                job.test_ip.as_deref().unwrap_or(""),
            )
        })
        .collect();
    let mut failed = HashSet::new();
    let mut deferred = HashSet::new();
    for job in &second {
        let key = (
            job.source_id.as_str(),
            job.fingerprint.as_str(),
            job.test_ip.as_deref().unwrap_or(""),
        );
        let front_uncertain = match job.front.as_ref() {
            Some(front) => !known_fronts.contains(front) || uncertain_fronts.contains(front),
            None => known_fronts.is_empty() || !uncertain_fronts.is_empty(),
        };
        if front_uncertain {
            if deferred.insert(key) {
                out.push(NodeVerdict::deferred(job.clone(), "front_unverified"));
            }
            continue;
        }
        if tried.contains(&key) || !failed.insert(key) {
            continue;
        }
        out.push(
            NodeVerdict::dead(job.clone(), FRONT_DEAD, "前置全部不通，链式未测").with_attempts(0),
        );
    }
    out
}

/// Fan the jobs out through the gate and collect what came back.
///
/// Returns only the verdicts that are still valid: a worker whose round was
/// cancelled while it waited for a slot, or after its test finished but before
/// it could be recorded, drops its result rather than reporting it.
async fn run_jobs(jobs: &[Job], ctx: &RoundCtx, tester: &Arc<dyn NodeTester>) -> Vec<NodeVerdict> {
    let mut set = tokio::task::JoinSet::new();
    for (index, job) in jobs.iter().enumerate() {
        let ctx = ctx.clone();
        let tester = Arc::clone(tester);
        let job = job.clone();
        set.spawn(async move {
            // `Err` here means the round was cancelled while this job queued.
            let permit = ctx.acquire(&job).await.ok()?;
            let verdict = tester.test(job).await;
            // Release before the checkpoint so a cancelled round's stragglers
            // are not holding slots while the runner drains.
            drop(permit);
            ctx.check().ok()?;
            Some((index, verdict))
        });
    }

    // Preserve plan order, not completion order: the first failure and tied
    // best variant must not change when network timings change.
    let mut out = vec![None; jobs.len()];
    loop {
        tokio::select! {
            biased;
            // Cancel first: a cancelled round must not keep draining a queue
            // that can only produce results it will throw away.
            _ = ctx.cancel_token().cancelled() => {
                set.shutdown().await;
                break;
            }
            joined = set.join_next() => match joined {
                None => break,
                Some(Ok(Some((index, verdict)))) => out[index] = Some(verdict),
                // Cancelled or gate-closed mid-flight.
                Some(Ok(None)) => {}
                // Partial results after a worker failure are not a valid round.
                Some(Err(err)) => {
                    tracing::error!(panic = err.is_panic(), "probe worker ended early; cancelling round");
                    ctx.cancel();
                }
            },
        }
    }
    out.into_iter().flatten().collect()
}

/// Describe the kernel, variant progress and unique-node counts without error text.
fn compose_note(
    kernel: &KernelState,
    counts: RoundCounts,
    cancelled: bool,
    planned: usize,
    measured: usize,
) -> String {
    let base = match kernel {
        KernelState::Reloaded => "kernel reloaded",
        KernelState::Refused => "kernel reload refused",
        KernelState::Unreachable(_) => "kernel unreachable",
        KernelState::NotPrepared(_) => "no kernel config this round",
    };
    if cancelled {
        format!("{base}; cancelled after {measured} of {planned} variants tested")
    } else if !matches!(kernel, KernelState::Reloaded) {
        format!("{base}; {planned} variant(s) not tested")
    } else {
        format!(
            "{base}; tested {measured}/{planned} variants: {} nodes, ok {}, failed {}, excluded {}, inconclusive {}",
            counts.total, counts.ok, counts.failed, counts.excluded, counts.inconclusive
        )
    }
}

/// The real ledger: one `Storage` behind a mutex.
///
/// The lock is taken inside each call and never held across an `await`, which
/// is why a `std` mutex is correct here -- and why the runner can hold this
/// while other request handlers still reach `/readyz` and `/api/v1/status`.
impl Ledger for Mutex<Storage> {
    fn node_statuses(&self) -> DomainResult<std::collections::HashMap<(String, String), String>> {
        self.lock()
            .map_err(|_| DomainError::Storage("ledger lock poisoned".into()))?
            .node_statuses()
    }

    fn record_quality_samples(
        &self,
        round_id: RoundId,
        samples: &[probe_storage::QualitySampleRow],
    ) -> DomainResult<usize> {
        self.lock()
            .map_err(|_| DomainError::Storage("ledger lock poisoned".into()))?
            .record_quality_samples(round_id, samples)
    }

    fn start_round(&self, trigger: &str, mode: Option<&str>) -> DomainResult<RoundId> {
        self.lock()
            .map_err(|_| DomainError::Storage("ledger lock poisoned".into()))?
            .start_round(trigger, mode)
    }

    fn commit_verdicts(
        &self,
        round_id: RoundId,
        verdicts: &[NodeVerdict],
        nodes: &[probe_storage::ConvergeNode],
        policy: &probe_domain::Policy,
    ) -> DomainResult<(usize, probe_storage::ConvergenceSummary)> {
        let rows: Vec<ResultRow> = verdicts.iter().map(NodeVerdict::to_result_row).collect();
        self.lock()
            .map_err(|_| DomainError::Storage("ledger lock poisoned".into()))?
            .commit_measurements(round_id, &rows, nodes, policy)
    }

    fn previous_alive_count(&self) -> DomainResult<i64> {
        self.lock()
            .map_err(|_| DomainError::Storage("ledger lock poisoned".into()))?
            .previous_alive_count()
    }

    fn finish_round(
        &self,
        round_id: RoundId,
        note: &str,
        counts: RoundCounts,
        suspect: bool,
        inconclusive: bool,
    ) -> DomainResult<()> {
        self.lock()
            .map_err(|_| DomainError::Storage("ledger lock poisoned".into()))?
            .finish_round_full(
                round_id,
                Some(note),
                &probe_storage::RoundGuardSummary {
                    total: counts.total,
                    ok: counts.ok,
                    failed: counts.failed,
                    dropped: counts.dropped,
                    restored: counts.restored,
                    suspect,
                    inconclusive,
                },
            )
    }

    fn finish_round_timed(
        &self,
        round_id: RoundId,
        note: &str,
        counts: RoundCounts,
        suspect: bool,
        inconclusive: bool,
        duration_s: f64,
    ) -> DomainResult<()> {
        self.lock()
            .map_err(|_| DomainError::Storage("ledger lock poisoned".into()))?
            .finish_round_full_timed(
                round_id,
                Some(note),
                &probe_storage::RoundGuardSummary {
                    total: counts.total,
                    ok: counts.ok,
                    failed: counts.failed,
                    dropped: counts.dropped,
                    restored: counts.restored,
                    suspect,
                    inconclusive,
                },
                duration_s,
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// A tester that answers a fixed verdict, records peak concurrency, and can
    /// cancel the round from inside a test.
    struct FakeTester {
        alive: bool,
        delay: Duration,
        live: Mutex<HashMap<String, usize>>,
        peak: Mutex<HashMap<String, usize>>,
        global_peak: AtomicUsize,
        live_global: AtomicUsize,
        /// Cancel the round once this many tests have started.
        cancel_after: Option<usize>,
        started: AtomicUsize,
        cancel: Mutex<Option<CancellationToken>>,
        /// Kernel names that always come back dead, whatever `alive` says.
        dead_names: Vec<String>,
        failure_kind: &'static str,
    }

    impl FakeTester {
        fn new(alive: bool) -> Self {
            Self {
                alive,
                delay: Duration::from_millis(20),
                live: Mutex::new(HashMap::new()),
                peak: Mutex::new(HashMap::new()),
                global_peak: AtomicUsize::new(0),
                live_global: AtomicUsize::new(0),
                cancel_after: None,
                started: AtomicUsize::new(0),
                cancel: Mutex::new(None),
                dead_names: Vec::new(),
                failure_kind: "timeout",
            }
        }

        /// Mark these kernel names dead; everything else answers `alive`.
        fn killing(mut self, dead: &[&str]) -> Self {
            self.dead_names = dead.iter().map(|s| s.to_string()).collect();
            self
        }

        /// Set the reason returned for failures; keep all other tester behavior.
        fn failing_with(mut self, reason: &'static str) -> Self {
            self.failure_kind = reason;
            self
        }

        fn slow(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }

        fn cancelling_after(mut self, n: usize) -> Self {
            self.cancel_after = Some(n);
            self
        }

        /// Hand the tester the round's token so it can cancel from inside.
        fn arm(self, token: CancellationToken) -> Self {
            *self.cancel.lock().unwrap() = Some(token);
            self
        }

        fn peak(&self, key: &str) -> usize {
            *self.peak.lock().unwrap().get(key).unwrap_or(&0)
        }

        fn global_peak(&self) -> usize {
            self.global_peak.load(Ordering::Acquire)
        }

        fn started(&self) -> usize {
            self.started.load(Ordering::Acquire)
        }
    }

    impl NodeTester for FakeTester {
        fn test<'a>(&'a self, job: Job) -> BoxFuture<'a, NodeVerdict> {
            Box::pin(async move {
                let key = job.server_ip.clone();
                let started = self.started.fetch_add(1, Ordering::AcqRel) + 1;
                if self.cancel_after == Some(started) {
                    if let Some(token) = self.cancel.lock().unwrap().as_ref() {
                        token.cancel();
                    }
                }
                {
                    let mut live = self.live.lock().unwrap();
                    let now = live.entry(key.clone()).or_default();
                    *now += 1;
                    let mut peak = self.peak.lock().unwrap();
                    let high = peak.entry(key.clone()).or_default();
                    *high = (*high).max(*now);
                }
                let now_global = self.live_global.fetch_add(1, Ordering::AcqRel) + 1;
                self.global_peak.fetch_max(now_global, Ordering::AcqRel);

                tokio::time::sleep(self.delay).await;

                self.live_global.fetch_sub(1, Ordering::AcqRel);
                *self.live.lock().unwrap().entry(key).or_default() -= 1;

                if self.alive && !self.dead_names.contains(&job.proxy_name) {
                    NodeVerdict::alive(job, 42)
                } else {
                    NodeVerdict::dead(job, self.failure_kind, "Timeout")
                }
            })
        }
    }

    #[derive(Default)]
    struct Recorded {
        started: Vec<(String, Option<String>)>,
        results: Vec<(RoundId, Vec<NodeVerdict>)>,
        finished: Vec<(RoundId, String, RoundCounts, bool, bool)>,
        /// The duration each close carried, in call order.
        durations: Vec<(RoundId, f64)>,
        converged: Vec<(RoundId, usize)>,
    }

    #[derive(Default)]
    struct FakeLedger {
        recorded: Mutex<Recorded>,
        /// Make the result portion of `commit_verdicts` fail.
        fail_record: bool,
        /// Make `finish_round` fail, to prove the failure is reported.
        fail_finish: bool,
        fail_converge: bool,
        fail_baseline: bool,
        cancel_on_baseline: Option<CancellationToken>,
        cancel_on_commit: Option<CancellationToken>,
        /// What `previous_alive_count` reports: the suspect guard's baseline.
        baseline: Mutex<i64>,
        /// What `node_statuses` reports: the (source, fingerprint) -> status map.
        statuses: Mutex<std::collections::HashMap<(String, String), String>>,
    }

    impl FakeLedger {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// A fake with a nonzero alive baseline: the suspect guard bites.
        fn with_baseline(baseline: i64) -> Arc<Self> {
            Arc::new(Self {
                baseline: Mutex::new(baseline),
                ..Self::default()
            })
        }

        fn refusing_writes() -> Arc<Self> {
            Arc::new(Self {
                fail_record: true,
                ..Self::default()
            })
        }

        fn refusing_close() -> Arc<Self> {
            Arc::new(Self {
                fail_finish: true,
                ..Self::default()
            })
        }
    }

    impl Ledger for FakeLedger {
        fn node_statuses(
            &self,
        ) -> DomainResult<std::collections::HashMap<(String, String), String>> {
            Ok(self.statuses.lock().unwrap().clone())
        }

        fn start_round(&self, trigger: &str, mode: Option<&str>) -> DomainResult<RoundId> {
            let mut rec = self.recorded.lock().unwrap();
            rec.started
                .push((trigger.to_string(), mode.map(str::to_string)));
            Ok(rec.started.len() as RoundId)
        }

        fn commit_verdicts(
            &self,
            round_id: RoundId,
            verdicts: &[NodeVerdict],
            nodes: &[probe_storage::ConvergeNode],
            _policy: &probe_domain::Policy,
        ) -> DomainResult<(usize, probe_storage::ConvergenceSummary)> {
            if self.fail_record {
                return Err(DomainError::Storage("results table is locked".into()));
            }
            if self.fail_converge {
                return Err(DomainError::Storage("convergence failed".into()));
            }
            let mut recorded = self.recorded.lock().unwrap();
            recorded.results.push((round_id, verdicts.to_vec()));
            recorded.converged.push((round_id, nodes.len()));
            if let Some(token) = &self.cancel_on_commit {
                token.cancel();
            }
            Ok((
                verdicts.len(),
                probe_storage::ConvergenceSummary {
                    alive_nodes: nodes.iter().filter(|node| node.ok).count() as i64,
                    ..Default::default()
                },
            ))
        }

        fn previous_alive_count(&self) -> DomainResult<i64> {
            if self.fail_baseline {
                return Err(DomainError::Storage("baseline unavailable".into()));
            }
            if let Some(token) = &self.cancel_on_baseline {
                token.cancel();
            }
            Ok(*self.baseline.lock().unwrap())
        }

        fn finish_round(
            &self,
            round_id: RoundId,
            note: &str,
            counts: RoundCounts,
            suspect: bool,
            inconclusive: bool,
        ) -> DomainResult<()> {
            if self.fail_finish {
                return Err(DomainError::Storage("rounds table is locked".into()));
            }
            self.recorded.lock().unwrap().finished.push((
                round_id,
                note.to_string(),
                counts,
                suspect,
                inconclusive,
            ));
            Ok(())
        }

        /// Record the duration the runner measured, then close exactly like the
        /// real ledger -- the tests assert both halves of that contract.
        fn finish_round_timed(
            &self,
            round_id: RoundId,
            note: &str,
            counts: RoundCounts,
            suspect: bool,
            inconclusive: bool,
            duration_s: f64,
        ) -> DomainResult<()> {
            self.recorded
                .lock()
                .unwrap()
                .durations
                .push((round_id, duration_s));
            self.finish_round(round_id, note, counts, suspect, inconclusive)
        }
    }

    struct FakeKernel(KernelState);

    impl KernelPrep for FakeKernel {
        fn prepare(&self) -> BoxFuture<'_, KernelState> {
            Box::pin(async move { self.0.clone() })
        }
    }

    struct PendingKernel(Arc<tokio::sync::Notify>);

    impl KernelPrep for PendingKernel {
        /// Announce preparation and wait forever, so only cancellation can finish.
        fn prepare(&self) -> BoxFuture<'_, KernelState> {
            Box::pin(async move {
                self.0.notify_one();
                std::future::pending().await
            })
        }
    }

    struct PanickingTester;

    impl NodeTester for PanickingTester {
        /// Simulate a worker bug without producing a node verdict.
        fn test<'a>(&'a self, _job: Job) -> BoxFuture<'a, NodeVerdict> {
            Box::pin(async { panic!("test worker failure") })
        }
    }

    struct ReorderedTester;

    impl NodeTester for ReorderedTester {
        /// Complete the second job first; each failure carries its plan name.
        fn test<'a>(&'a self, job: Job) -> BoxFuture<'a, NodeVerdict> {
            Box::pin(async move {
                if job.proxy_name == "first" {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let reason = job.proxy_name.clone();
                NodeVerdict::dead(job, reason, "test failure")
            })
        }
    }

    /// Nodes with no measurement run first; everything else keeps its order.
    ///
    /// The point is coverage under cancellation: a round that is stopped early
    /// must still have moved the nodes nobody has ever seen, instead of
    /// re-testing the same head of the list every time.
    #[test]
    fn unmeasured_nodes_run_first_and_measured_ones_keep_their_order() {
        let ledger = FakeLedger::new();
        {
            let mut statuses = ledger.statuses.lock().unwrap();
            statuses.insert(("air".into(), "air-1-fp".into()), "alive".into());
            statuses.insert(("air".into(), "air-2-fp".into()), "pending".into());
            statuses.insert(("air".into(), "air-3-fp".into()), "dead".into());
            // air-4 is not in the ledger at all: also unmeasured.
        }
        let mut round_plan = plan(vec![
            job("air", "1"),
            job("air", "2"),
            job("air", "3"),
            job("air", "4"),
        ]);
        prioritize_unmeasured(&mut round_plan, ledger.as_ref());
        let order: Vec<&str> = round_plan
            .jobs
            .iter()
            .map(|job| job.fingerprint.as_str())
            .collect();
        assert_eq!(order, vec!["air-2-fp", "air-4-fp", "air-1-fp", "air-3-fp"]);
    }

    /// A ledger with nothing to say must not reorder anything.
    #[test]
    fn an_empty_ledger_leaves_the_plan_alone() {
        let ledger = FakeLedger::new();
        let mut round_plan = plan(vec![job("air", "1"), job("air", "2")]);
        prioritize_unmeasured(&mut round_plan, ledger.as_ref());
        let order: Vec<&str> = round_plan
            .jobs
            .iter()
            .map(|job| job.fingerprint.as_str())
            .collect();
        assert_eq!(order, vec!["air-1-fp", "air-2-fp"]);
    }

    fn job(source: &str, ip: &str) -> Job {
        Job::new(
            source,
            format!("{source}-{ip}-fp"),
            format!("{source}-{ip}"),
            "direct",
            ip,
        )
    }

    fn plan(jobs: Vec<Job>) -> RoundPlan {
        RoundPlan {
            trigger: "test".into(),
            mode: None,
            jobs,
            policy: probe_domain::Policy::default(),
            domain_pass: DomainPass::Any,
            preserved_sources: Vec::new(),
        }
    }

    fn loose() -> Limits {
        Limits::from_concurrency(8)
    }

    fn tight_per_ip() -> Limits {
        Limits {
            global: 8,
            per_source: 8,
            per_server_ip: 1,
            diagnose: 2,
        }
    }

    /// Open a round the way a caller must, then run it. Keeps every test on the
    /// same two-step contract instead of hiding the open inside the runner.
    async fn run_round_in_test(
        plan: RoundPlan,
        limits: Limits,
        kernel: Arc<dyn KernelPrep>,
        tester: Arc<dyn NodeTester>,
        ledger: Arc<FakeLedger>,
    ) -> DomainResult<RoundOutcome> {
        let round_id = ledger
            .start_round(&plan.trigger, plan.mode.as_deref())
            .unwrap();
        run_round(round_id, plan, limits, kernel, tester, ledger).await
    }

    /// Same, on a caller-supplied context -- the cancellation tests need to
    /// hold the token the round is running under.
    async fn run_round_with_ctx_in_test(
        plan: RoundPlan,
        ctx: RoundCtx,
        kernel: Arc<dyn KernelPrep>,
        tester: Arc<dyn NodeTester>,
        ledger: Arc<FakeLedger>,
    ) -> DomainResult<RoundOutcome> {
        let round_id = ledger
            .start_round(&plan.trigger, plan.mode.as_deref())
            .unwrap();
        run_round_with_ctx(round_id, plan, ctx, kernel, tester, ledger).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_round_records_every_verdict_and_closes_once() {
        let ledger = FakeLedger::new();
        let outcome = run_round_in_test(
            plan(vec![job("air", "10.0.0.1"), job("air", "10.0.0.2")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(FakeTester::new(true)),
            ledger.clone(),
        )
        .await
        .unwrap();

        assert_eq!(
            outcome.counts,
            RoundCounts {
                total: 2,
                ok: 2,
                failed: 0,
                dropped: 0,
                restored: 0,
                ..Default::default()
            }
        );
        assert_eq!(outcome.results_written, 2);
        assert!(!outcome.cancelled);
        let rec = ledger.recorded.lock().unwrap();
        assert_eq!(rec.started.len(), 1, "exactly one round row opened");
        assert_eq!(rec.finished.len(), 1, "exactly one round row closed");
        assert_eq!(rec.finished[0].0, outcome.round_id);
        assert_eq!(rec.finished[0].2.total, 2);
        assert_eq!(rec.results.len(), 1, "one batched write, not one per node");
        assert!(
            rec.finished[0].1.contains("ok 2"),
            "note: {}",
            rec.finished[0].1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failures_are_counted_and_carry_their_class() {
        let outcome = run_round_in_test(
            plan(vec![job("air", "10.0.0.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(FakeTester::new(false)),
            FakeLedger::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            outcome.counts,
            RoundCounts {
                total: 1,
                ok: 0,
                failed: 1,
                dropped: 0,
                restored: 0,
                ..Default::default()
            }
        );
        assert_eq!(outcome.verdicts[0].reason(), Some("timeout"));
        let row = outcome.verdicts[0].to_result_row();
        assert_eq!(row.verdict.as_deref(), Some(VERDICT_FAIL));
        assert_eq!(row.reason.as_deref(), Some("timeout"));
        assert_eq!(row.attempts, Some(1));
        assert_eq!(row.category.as_deref(), Some("direct"));
        assert_eq!(row.fingerprint, "air-10.0.0.1-fp");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_gate_actually_bounds_the_node_phase() {
        // One address, many nodes: per_server_ip=1 must serialise them even
        // though the round has plenty of global room. This is the assertion
        // that fails if `run_round` stops going through `RoundCtx`.
        let tester = Arc::new(FakeTester::new(true));
        let jobs: Vec<Job> = (0..5)
            .map(|i| job(&format!("s{i}"), "203.0.113.9"))
            .collect();
        run_round_in_test(
            plan(jobs),
            tight_per_ip(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            tester.clone(),
            FakeLedger::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            tester.peak("203.0.113.9"),
            1,
            "per_server_ip=1 must serialise the node phase"
        );
        assert_eq!(tester.started(), 5, "every job must still run");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn distinct_addresses_still_run_concurrently() {
        // The control for the test above: without it, a runner that ran
        // everything serially would pass that one.
        let tester = Arc::new(FakeTester::new(true).slow(Duration::from_millis(40)));
        let jobs: Vec<Job> = (0..5)
            .map(|i| job(&format!("s{i}"), &format!("203.0.113.{i}")))
            .collect();
        run_round_in_test(
            plan(jobs),
            tight_per_ip(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            tester.clone(),
            FakeLedger::new(),
        )
        .await
        .unwrap();

        assert!(
            tester.global_peak() > 1,
            "five distinct addresses under global=8 must overlap, peak was {}",
            tester.global_peak()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unreachable_kernel_skips_the_node_phase_and_still_closes() {
        let tester = Arc::new(FakeTester::new(true));
        let ledger = FakeLedger::new();
        let outcome = run_round_in_test(
            plan(vec![job("air", "10.0.0.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Unreachable(
                "connection refused".into(),
            ))),
            tester.clone(),
            ledger.clone(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.counts, RoundCounts::default());
        assert_eq!(outcome.results_written, 0);
        assert!(
            outcome.note.contains("unreachable"),
            "note: {}",
            outcome.note
        );
        assert_eq!(
            tester.started(),
            0,
            "no test may run against a missing kernel"
        );
        let rec = ledger.recorded.lock().unwrap();
        assert_eq!(rec.finished.len(), 1, "the row must still close");
        assert!(rec.results.is_empty(), "no node verdicts to record");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_refused_reload_skips_nodes_and_preserves_the_ledger() {
        // Rust has no recreate fallback: old aliases cannot prove this plan.
        let ledger = FakeLedger::with_baseline(10);
        let tester = Arc::new(FakeTester::new(true));
        let outcome = run_round_in_test(
            plan(vec![job("air", "10.0.0.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Refused)),
            tester.clone(),
            ledger.clone(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.counts.total, 0);
        assert_eq!(tester.started(), 0);
        assert_eq!(outcome.guard, GuardDecision::MarkRoundInconclusive);
        let recorded = ledger.recorded.lock().unwrap();
        assert!(recorded.results.is_empty());
        assert!(recorded.converged.is_empty());
        assert!(recorded.finished[0].4);
        assert!(outcome.note.contains("refused"), "note: {}", outcome.note);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancellation_drops_results_but_still_closes_the_row() {
        let ctx = RoundCtx::new(tight_per_ip());
        // The tester cancels the round itself once the first test is running,
        // so the cancel lands mid-round rather than before it starts.
        let tester = Arc::new(
            FakeTester::new(true)
                .slow(Duration::from_millis(60))
                .cancelling_after(1)
                .arm(ctx.cancel_token().clone()),
        );
        let ledger = FakeLedger::new();
        let jobs: Vec<Job> = (0..6)
            .map(|i| job(&format!("s{i}"), "203.0.113.9"))
            .collect();

        let outcome = run_round_with_ctx_in_test(
            plan(jobs),
            ctx.clone(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            tester.clone(),
            ledger.clone(),
        )
        .await
        .unwrap();

        assert!(outcome.cancelled, "the round must report the cancellation");
        assert_eq!(
            outcome.results_written, 0,
            "a cancelled round writes no results"
        );
        assert!(
            outcome.counts.total < 6,
            "cancellation must stop the phase early, ran {}",
            outcome.counts.total
        );
        assert!(outcome.note.contains("cancelled"), "note: {}", outcome.note);
        assert!(ctx.is_cancelled());
        let rec = ledger.recorded.lock().unwrap();
        assert!(rec.results.is_empty(), "no result batch after a cancel");
        assert_eq!(
            rec.finished.len(),
            1,
            "the round row must still close -- an open row is an orphan"
        );
        assert_eq!(rec.finished[0].2.total, outcome.counts.total);
        assert!(
            rec.finished[0].4,
            "a cancelled round is not a valid baseline"
        );
        assert!(rec.converged.is_empty());
        // Aborting the drain must still return every slot: the cancelled
        // tasks hold permits when `shutdown()` unwinds them.
        let limits = ctx.gate().limits();
        assert_eq!(
            ctx.gate().available_permits(),
            (limits.global, limits.diagnose),
            "cancelling a round must not leak permits"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_round_already_cancelled_writes_nothing_but_closes() {
        let ctx = RoundCtx::new(loose());
        ctx.cancel();
        let tester = Arc::new(FakeTester::new(true));
        let ledger = FakeLedger::new();
        let outcome = run_round_with_ctx_in_test(
            plan(vec![job("air", "10.0.0.1")]),
            ctx,
            Arc::new(FakeKernel(KernelState::Reloaded)),
            tester.clone(),
            ledger.clone(),
        )
        .await
        .unwrap();

        assert!(outcome.cancelled);
        assert_eq!(outcome.results_written, 0);
        assert_eq!(tester.started(), 0, "a cancelled gate admits no job");
        let rec = ledger.recorded.lock().unwrap();
        assert_eq!(rec.finished.len(), 1);
    }

    #[test]
    fn the_note_never_carries_a_controller_message() {
        // The unreachable branch deliberately drops the error text from the
        // note: it can contain a URL, and the note is displayed in the panel.
        let note = compose_note(
            &KernelState::Unreachable("http://user:pw@10.0.0.1:9090".into()),
            RoundCounts::default(),
            false,
            3,
            0,
        );
        assert!(note.contains("unreachable"));
        assert!(!note.contains("pw"), "no error text in the note: {note}");
        assert!(!note.contains("http"), "no URL in the note: {note}");
    }

    #[test]
    fn detail_truncation_is_character_safe() {
        // A byte slice would panic here; node names in this project are CJK.
        let job = job("air", "10.0.0.1");
        let verdict = NodeVerdict::dead(job, "timeout", "超时".repeat(300));
        let row = verdict.to_result_row();
        let detail = row.detail.unwrap();
        assert_eq!(detail.chars().count(), DETAIL_LIMIT);
        assert!(detail.starts_with('超'));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_result_batch_still_closes_the_round() {
        // The one thing this function promises: no open round row. A failing
        // result write must not `?` its way past `finish_round`.
        let ledger = FakeLedger::refusing_writes();
        let err = run_round_in_test(
            plan(vec![job("air", "10.0.0.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(FakeTester::new(true)),
            ledger.clone(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("results table is locked"), "{err}");
        let rec = ledger.recorded.lock().unwrap();
        assert_eq!(
            rec.finished.len(),
            1,
            "the row must close even though the result batch failed"
        );
        assert!(rec.results.is_empty());
        assert!(
            rec.converged.is_empty(),
            "a failed batch must not advance streaks"
        );
        assert!(rec.finished[0].4, "failed persistence is inconclusive");
        assert_eq!(
            rec.durations.len(),
            1,
            "the close still carries its measurement on the failure path"
        );
        assert!(
            rec.durations[0].1.is_finite() && rec.durations[0].1 >= 0.0,
            "a measured duration is finite and non-negative: {}",
            rec.durations[0].1
        );
    }

    #[test]
    fn measured_seconds_rounds_to_one_decimal_and_clamps_broken_readings() {
        assert_eq!(measured_seconds(0.0), 0.0);
        assert_eq!(measured_seconds(1.234), 1.2);
        assert_eq!(measured_seconds(1.25), 1.3);
        assert_eq!(measured_seconds(12.349), 12.3);
        assert_eq!(measured_seconds(60.0), 60.0);
        // One decimal, always: the panel prints this value directly.
        for seconds in [0.0, 0.001, 0.05, 0.999, 1.005, 61.234] {
            let value = measured_seconds(seconds);
            assert_eq!(value, (value * 10.0).round() / 10.0, "{seconds}");
            assert!(value.is_finite() && value >= 0.0, "{seconds} -> {value}");
        }
        // A clock that reported nonsense must not reach the ledger as one. The
        // close still happens and the round reads as "no measurable duration".
        for broken in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, -0.05] {
            assert_eq!(measured_seconds(broken), 0.0, "{broken}");
        }
    }

    /// The runner's measurement lands in the column Python's panel reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stored_round_records_its_measured_duration() {
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().unwrap()));
        // A seeded round closed by the untimed entry: its column stays NULL and
        // the reader keeps working on it.
        let seeded = storage.lock().unwrap().start_round("seed", None).unwrap();
        storage
            .lock()
            .unwrap()
            .finish_round_with_counts(seeded, None, 1, 1, 0)
            .unwrap();
        let outcome = run_stored(
            plan(vec![job("air", "10.0.0.1")]),
            Arc::new(FakeTester::new(true).slow(Duration::from_millis(250))),
            storage.clone(),
        )
        .await;
        let stored = storage.lock().unwrap();
        let row = stored.round_by_id(outcome.round_id).unwrap().unwrap();
        let duration = row
            .duration_s
            .expect("running a round must record how long it took");
        assert!(
            (0.2..10.0).contains(&duration),
            "measured from the round's own start, not fabricated: {duration}"
        );
        assert_eq!(duration, (duration * 10.0).round() / 10.0, "one decimal");
        assert_eq!(
            stored.round_by_id(seeded).unwrap().unwrap().duration_s,
            None,
            "an untimed close still leaves the column NULL"
        );
    }

    /// Both ledger error paths must close the round without partially updating nodes.
    #[tokio::test]
    async fn convergence_and_baseline_errors_still_close_the_round() {
        for fail_baseline in [false, true] {
            let ledger = Arc::new(FakeLedger {
                fail_converge: !fail_baseline,
                fail_baseline,
                ..Default::default()
            });
            let result = run_round_in_test(
                plan(vec![job("air", "10.0.0.1")]),
                loose(),
                Arc::new(FakeKernel(KernelState::Reloaded)),
                Arc::new(FakeTester::new(true)),
                ledger.clone(),
            )
            .await;
            assert!(result.is_err());
            let recorded = ledger.recorded.lock().unwrap();
            assert_eq!(recorded.finished.len(), 1);
            assert!(recorded.finished[0].4);
            assert!(recorded.results.is_empty());
            assert!(recorded.converged.is_empty());
        }
    }

    /// An empty collection cannot replace the guard's last measured baseline.
    #[tokio::test]
    async fn a_round_with_no_jobs_is_inconclusive() {
        let ledger = FakeLedger::with_baseline(10);
        let outcome = run_round_in_test(
            plan(vec![]),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(FakeTester::new(true)),
            ledger.clone(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.guard, GuardDecision::MarkRoundInconclusive);
        let recorded = ledger.recorded.lock().unwrap();
        assert!(recorded.finished[0].4);
        assert!(recorded.converged.is_empty());
        assert!(recorded.results.is_empty());
    }

    /// Cancellation interrupts preparation too, including a pre-cancelled round.
    #[tokio::test]
    async fn cancellation_does_not_wait_for_kernel_preparation() {
        for already_cancelled in [false, true] {
            let ctx = RoundCtx::new(loose());
            let started = Arc::new(tokio::sync::Notify::new());
            let ledger = FakeLedger::new();
            if already_cancelled {
                ctx.cancel();
            }
            let round = run_round_with_ctx_in_test(
                plan(vec![job("air", "192.0.2.1")]),
                ctx.clone(),
                Arc::new(PendingKernel(started.clone())),
                Arc::new(FakeTester::new(true)),
                ledger.clone(),
            );
            let cancel = async {
                if !already_cancelled {
                    started.notified().await;
                    ctx.cancel();
                }
            };
            let (outcome, ()) = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::join!(round, cancel)
            })
            .await
            .expect("cancellation must wake preparation");
            let outcome = outcome.unwrap();
            assert!(outcome.cancelled);
            assert_eq!(outcome.guard, GuardDecision::MarkRoundInconclusive);
            let recorded = ledger.recorded.lock().unwrap();
            assert!(recorded.results.is_empty());
            assert!(recorded.finished[0].4);
        }
    }

    /// Re-check after the baseline read, but do not undo a transaction already entered.
    #[tokio::test]
    async fn cancellation_has_one_explicit_commit_boundary() {
        for cancel_before_commit in [false, true] {
            let ctx = RoundCtx::new(loose());
            let token = ctx.cancel_token().clone();
            let ledger = Arc::new(FakeLedger {
                cancel_on_baseline: cancel_before_commit.then_some(token.clone()),
                cancel_on_commit: (!cancel_before_commit).then_some(token),
                ..Default::default()
            });
            let outcome = run_round_with_ctx_in_test(
                plan(vec![job("air", "192.0.2.1")]),
                ctx.clone(),
                Arc::new(FakeKernel(KernelState::Reloaded)),
                Arc::new(FakeTester::new(true)),
                ledger.clone(),
            )
            .await
            .unwrap();
            assert!(ctx.is_cancelled());
            assert_eq!(outcome.cancelled, cancel_before_commit);
            assert_eq!(outcome.results_written, usize::from(!cancel_before_commit));
            assert_eq!(
                ledger.recorded.lock().unwrap().finished[0].4,
                cancel_before_commit
            );
        }
    }

    /// A worker panic is a round failure, not a smaller successful measurement set.
    #[tokio::test]
    async fn a_panicked_worker_cannot_commit_a_partial_round() {
        let ledger = FakeLedger::new();
        let outcome = run_round_in_test(
            plan(vec![job("air", "192.0.2.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(PanickingTester),
            ledger.clone(),
        )
        .await
        .unwrap();
        assert!(outcome.cancelled);
        assert_eq!(outcome.guard, GuardDecision::MarkRoundInconclusive);
        let recorded = ledger.recorded.lock().unwrap();
        assert!(recorded.converged.is_empty());
        assert!(recorded.results.is_empty());
        assert!(recorded.finished[0].4);
    }

    /// Controller outages after reload cannot advance node failure streaks.
    #[tokio::test]
    async fn a_mid_round_controller_failure_preserves_node_state() {
        let ledger = FakeLedger::with_baseline(10);
        let outcome = run_round_in_test(
            plan(vec![job("air", "192.0.2.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(FakeTester::new(false).failing_with("controller_error")),
            ledger.clone(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.guard, GuardDecision::MarkRoundInconclusive);
        assert!(outcome.note.contains("controller failed during testing"));
        let recorded = ledger.recorded.lock().unwrap();
        assert!(recorded.converged.is_empty());
        assert!(recorded.results.is_empty());
        assert!(recorded.finished[0].4);
    }

    /// Network completion order must not choose a node's displayed failure reason.
    #[tokio::test]
    async fn verdict_order_matches_the_plan_not_completion_timing() {
        let jobs = vec![
            Job::new("air", "node", "first", "direct", "192.0.2.1"),
            Job::new("air", "node", "second", "direct", "192.0.2.2"),
        ];
        let outcome = run_round_in_test(
            plan(jobs),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(ReorderedTester),
            FakeLedger::new(),
        )
        .await
        .unwrap();
        let names: Vec<_> = outcome
            .verdicts
            .iter()
            .map(|v| v.job.proxy_name.as_str())
            .collect();
        assert_eq!(names, ["first", "second"]);
        assert_eq!(
            fold_verdicts(&outcome.verdicts, DomainPass::Any)[0]
                .reason
                .as_deref(),
            Some("first")
        );
    }

    /// Per-variant rows stay detailed, while the round and guard count unique nodes.
    #[tokio::test]
    async fn variant_counts_cannot_inflate_the_next_rounds_alive_baseline() {
        let ledger = Arc::new(Mutex::new(Storage::open_in_memory().unwrap()));
        let jobs: Vec<_> = (0..3)
            .flat_map(|node| {
                (0..3).map(move |variant| {
                    Job::new(
                        "air",
                        format!("node-{node}"),
                        format!("node-{node}-variant-{variant}"),
                        "direct",
                        "192.0.2.1",
                    )
                })
            })
            .collect();
        for _ in 0..2 {
            let round_id = ledger.start_round("test", None).unwrap();
            let outcome = run_round(
                round_id,
                plan(jobs.clone()),
                loose(),
                Arc::new(FakeKernel(KernelState::Reloaded)),
                Arc::new(FakeTester::new(true)),
                ledger.clone(),
            )
            .await
            .unwrap();
            assert_eq!((outcome.counts.total, outcome.counts.ok), (3, 3));
            assert_eq!(outcome.results_written, 9);
            assert_eq!(outcome.guard, GuardDecision::ApplyConvergence);
            assert_eq!(ledger.previous_alive_count().unwrap(), 3);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_close_is_reported_rather_than_swallowed() {
        let ledger = FakeLedger::refusing_close();
        let err = run_round_in_test(
            plan(vec![job("air", "10.0.0.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(FakeTester::new(true)),
            ledger.clone(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("rounds table is locked"), "{err}");
        assert!(ledger.recorded.lock().unwrap().finished.is_empty());
    }

    fn named_job(
        proxy_name: &str,
        fingerprint: &str,
        server_ip: &str,
        role: Role,
        front: Option<&str>,
    ) -> Job {
        Job::new("air", fingerprint, proxy_name, "chain", server_ip)
            .with_role(role, front.map(str::to_string))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn chains_run_only_through_fronts_that_carry_traffic() {
        // One node, two variants: __FRONT0__ lives, __FRONT1__ is dead. The
        // variant through the dead front must not be dialled at all -- the
        // front's own result is the answer.
        let tester = Arc::new(FakeTester::new(true).killing(&["__FRONT1__"]));
        let jobs = vec![
            named_job("__FRONT0__", "fp-front-0", "9.9.9.9", Role::Front, None),
            named_job("__FRONT1__", "fp-front-1", "8.8.8.8", Role::Front, None),
            named_job(
                "n #0",
                "fp-node",
                "9.9.9.9",
                Role::Chain,
                Some("__FRONT0__"),
            ),
            named_job(
                "n #1",
                "fp-node",
                "8.8.8.8",
                Role::Chain,
                Some("__FRONT1__"),
            ),
        ];
        let outcome = run_round_in_test(
            plan(jobs),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            tester.clone(),
            FakeLedger::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.live_fronts, 1);
        // Three dialled (two fronts + the live-front variant) + one failed
        // untried. The dead-front variant of a node whose other variant ran
        // is NOT re-failed: the node was tried.
        assert_eq!(tester.started(), 3);
        assert_eq!(outcome.front_dead, 0);
        assert_eq!(outcome.counts.total, 3);
        assert!(
            outcome
                .verdicts
                .iter()
                .all(|v| v.reason() != Some("front_dead")),
            "no front_dead rows when a live front carried the node: {:?}",
            outcome.verdicts
        );
        // The dead front itself still has its own real verdict.
        assert_eq!(
            outcome
                .verdicts
                .iter()
                .filter(|v| v.job.proxy_name == "__FRONT1__")
                .map(|v| v.reason())
                .collect::<Vec<_>>(),
            vec![Some("timeout")]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_node_with_every_front_dead_fails_front_dead_without_dialling() {
        let tester = Arc::new(FakeTester::new(true).killing(&["__FRONT0__", "__FRONT1__"]));
        let jobs = vec![
            named_job("__FRONT0__", "fp-front-0", "9.9.9.9", Role::Front, None),
            named_job("__FRONT1__", "fp-front-1", "8.8.8.8", Role::Front, None),
            // Two variants of the same node, both fronts dead: one row.
            named_job(
                "n #0",
                "fp-node",
                "9.9.9.9",
                Role::Chain,
                Some("__FRONT0__"),
            ),
            named_job(
                "n #1",
                "fp-node",
                "8.8.8.8",
                Role::Chain,
                Some("__FRONT1__"),
            ),
            // A variant with no front at all (the pool collapsed after
            // expansion): the same treatment.
            named_job("m", "fp-other", "9.9.9.9", Role::Chain, None),
        ];
        let ledger = FakeLedger::new();
        let outcome = run_round_in_test(
            plan(jobs),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            tester.clone(),
            ledger.clone(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.live_fronts, 0);
        assert_eq!(tester.started(), 2, "only the two fronts were dialled");
        assert_eq!(outcome.front_dead, 2, "one row per node, not per variant");
        let dead_rows: Vec<_> = outcome
            .verdicts
            .iter()
            .filter(|v| v.reason() == Some("front_dead"))
            .collect();
        assert_eq!(dead_rows.len(), 2);
        for row in &dead_rows {
            assert_eq!(row.attempts, 0, "front_dead is a no-dial verdict");
        }
        let row = dead_rows[0].to_result_row();
        assert_eq!(row.reason.as_deref(), Some("front_dead"));
        assert_eq!(row.detail.as_deref(), Some("前置全部不通，链式未测"));
        assert_eq!(row.category.as_deref(), Some("chain"));
        // The write carries the no-dial verdicts too: the streak advances
        // through policy exactly as a dial failure would.
        assert_eq!(outcome.results_written as i64, outcome.counts.total);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_round_without_chains_stays_single_pass() {
        // Roles other than Chain never split the phase: fronts and direct
        // nodes test together, as before R6.
        let tester = Arc::new(FakeTester::new(true).killing(&["__FRONT1__"]));
        let jobs = vec![
            named_job("__FRONT0__", "fp-front-0", "9.9.9.9", Role::Front, None),
            named_job("plain", "fp-plain", "1.1.1.1", Role::Direct, None),
        ];
        let outcome = run_round_in_test(
            plan(jobs),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            tester,
            FakeLedger::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.counts.total, 2);
        assert_eq!(outcome.live_fronts, 1);
        assert_eq!(outcome.front_dead, 0);
    }

    // --- R7 guard + convergence (workstreams/03) ---

    #[test]
    fn folding_merges_variants_of_one_node_any_alive_wins() {
        let alive0 = NodeVerdict::alive(
            Job::new("air", "fp", "n #0", "chain", "9.9.9.9")
                .with_role(Role::Chain, Some("__FRONT0__".into())),
            80,
        );
        let alive1 = NodeVerdict::alive(
            Job::new("air", "fp", "n #1", "chain", "8.8.8.8")
                .with_role(Role::Chain, Some("__FRONT1__".into())),
            60,
        );
        let other = NodeVerdict::dead(
            Job::new("air", "other", "m", "direct", "1.1.1.1"),
            "timeout",
            "Timeout",
        );
        let folded = fold_verdicts(&[alive0, alive1, other], DomainPass::Any);
        assert_eq!(folded.len(), 2, "two nodes, not three verdicts");

        let node = &folded[0];
        assert_eq!(node.source, "air");
        assert_eq!(node.fingerprint, "fp");
        assert!(node.ok, "one front carrying traffic is enough");
        assert_eq!(node.delay_ms, Some(60), "the best of the living delays");
        assert_eq!(node.reason, None);
        assert_eq!(node.category.as_deref(), Some("chain"));

        let dead = &folded[1];
        assert!(!dead.ok);
        assert_eq!(dead.reason.as_deref(), Some("timeout"));
        assert_eq!(dead.category, None, "a failed bucket keeps no category");
    }

    #[test]
    fn a_dead_twin_does_not_rescue_a_dead_node_but_carries_its_reason() {
        // Chain variant and direct twin of the SAME node, both dead with
        // different reasons: first failure wins, node folds once.
        let chain = NodeVerdict::dead(
            Job::new("air", "fp", "n #0", "chain", "9.9.9.9")
                .with_role(Role::Chain, Some("__FRONT0__".into())),
            "front_dead",
            "front",
        );
        let twin = NodeVerdict::dead(
            Job::new("air", "fp", "n #d", "direct", "1.2.3.4"),
            "tls_error",
            "tls",
        );
        let folded = fold_verdicts(&[chain, twin], DomainPass::Any);
        assert_eq!(folded.len(), 1);
        assert!(!folded[0].ok);
        assert_eq!(
            folded[0].reason.as_deref(),
            Some("front_dead"),
            "first failure recorded"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_healthy_round_converges_and_is_not_suspect() {
        let ledger = FakeLedger::new();
        let round_id = ledger.start_round("test", None).unwrap();
        let outcome = run_round(
            round_id,
            plan(vec![job("air", "10.0.0.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(FakeTester::new(true)),
            ledger.clone(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.guard, GuardDecision::ApplyConvergence);
        assert!(!outcome.suspect);
        assert_eq!(
            ledger.recorded.lock().unwrap().converged,
            vec![(outcome.round_id, 1)],
            "the runner handed the fold exactly one node"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_round_below_the_alive_floor_is_marked_suspect() {
        // baseline 10, and the fake converge reports no alive nodes: 0 < the
        // absolute floor of 3 → suspect, publish preserved.
        let ledger = FakeLedger::with_baseline(10);
        let round_id = ledger.start_round("test", None).unwrap();
        let outcome = run_round(
            round_id,
            plan(vec![job("air", "10.0.0.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(FakeTester::new(false)),
            ledger.clone(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.guard, GuardDecision::PreservePreviousState);
        assert!(outcome.suspect);
        let rec = ledger.recorded.lock().unwrap();
        let (_, note, _, suspect, inconclusive) = &rec.finished[0];
        assert!(*suspect, "the rounds row must carry the suspect flag");
        assert!(!*inconclusive);
        assert!(note.contains("suspect: alive"), "note: {note}");
    }

    /// A source whose fetch failed is a hole in coverage, not an inconclusive
    /// round (2026-10-21): the nodes that WERE measured are a valid
    /// measurement, so the round converges, publishes and stays usable as
    /// the next round's baseline. The note must name the source instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fetch_failed_source_is_named_but_not_inconclusive() {
        let ledger = FakeLedger::new();
        let mut round_plan = plan(vec![job("air", "10.0.0.1")]);
        round_plan.preserved_sources = vec!["saki".into()];
        let outcome = run_round_in_test(
            round_plan,
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(FakeTester::new(true)),
            ledger.clone(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.guard, GuardDecision::ApplyConvergence);
        assert!(!outcome.suspect);
        assert_eq!(outcome.preserved_sources, ["saki"]);
        assert!(
            outcome.note.contains("sources not fetched: saki"),
            "note names the hole: {}",
            outcome.note
        );
        let rec = ledger.recorded.lock().unwrap();
        let (_, _, _, suspect, inconclusive) = &rec.finished[0];
        assert!(!*suspect);
        assert!(
            !*inconclusive,
            "a dead upstream must not flag the round inconclusive"
        );
    }

    /// The regression the 2026-10-21 fix exists for: with a fetch-failed
    /// source in the plan, a mass-death round must STILL be flagged suspect.
    /// Before the fix, `preserved_sources` made every such round
    /// inconclusive, which silently disabled the alive-floor guard for any
    /// deployment with one permanently broken upstream (ct186's saki) -- the
    /// guard could never fire, not even at ok=0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fetch_failed_source_does_not_mask_the_suspect_guard() {
        let ledger = FakeLedger::with_baseline(10);
        let mut round_plan = plan(vec![job("air", "10.0.0.1")]);
        round_plan.preserved_sources = vec!["saki".into()];
        let outcome = run_round_in_test(
            round_plan,
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            Arc::new(FakeTester::new(false)),
            ledger.clone(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.guard, GuardDecision::PreservePreviousState);
        assert!(
            outcome.suspect,
            "0 alive of a baseline of 10 must be suspect even with a broken source"
        );
        let rec = ledger.recorded.lock().unwrap();
        let (_, note, _, suspect, inconclusive) = &rec.finished[0];
        assert!(*suspect);
        assert!(!*inconclusive);
        assert!(note.contains("sources not fetched: saki"), "note: {note}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unreachable_kernel_marks_the_round_inconclusive() {
        let ledger = FakeLedger::with_baseline(10);
        let round_id = ledger.start_round("test", None).unwrap();
        let outcome = run_round(
            round_id,
            plan(vec![job("air", "10.0.0.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Unreachable("refused".into()))),
            Arc::new(FakeTester::new(true)),
            ledger.clone(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.guard, GuardDecision::MarkRoundInconclusive);
        assert!(!outcome.suspect, "an untested round is not a dead round");
        let rec = ledger.recorded.lock().unwrap();
        assert!(
            rec.converged.is_empty(),
            "nothing folds on an inconclusive round"
        );
        let (_, _, _, suspect, inconclusive) = &rec.finished[0];
        assert!(*inconclusive, "the rounds row carries the ADR-0005 flag");
        assert!(!*suspect);
    }

    /// Build an endpoint variant whose display/identity remain in original domain form.
    fn endpoint_job(name: &str, ip: &str) -> Job {
        let mut job = Job::new("air", "node", name, "direct", ip).with_origin(
            "upstream-node".into(),
            Some("origin.example".into()),
            Some("trojan".into()),
        );
        job.test_ip = Some(ip.into());
        job
    }

    /// Run a complete round against a real isolated SQLite ledger.
    async fn run_stored(
        plan: RoundPlan,
        tester: Arc<dyn NodeTester>,
        storage: Arc<Mutex<Storage>>,
    ) -> RoundOutcome {
        let id = storage.start_round("test", None).unwrap();
        run_round(
            id,
            plan,
            loose(),
            Arc::new(FakeKernel(KernelState::Reloaded)),
            tester,
            storage,
        )
        .await
        .unwrap()
    }

    /// Seed a formerly alive node with two consecutive failures for false-death regressions.
    fn pending_ledger() -> Arc<Mutex<Storage>> {
        let storage = Storage::open_in_memory().unwrap();
        for ok in [true, false, false] {
            let id = storage.start_round("seed", None).unwrap();
            storage
                .converge_nodes(
                    id,
                    &[probe_storage::ConvergeNode {
                        source: "air",
                        fingerprint: "node",
                        display: "upstream-node",
                        ok,
                        category: Some("direct"),
                        proto: Some("trojan"),
                        server: Some("origin.example"),
                        reason: if ok { None } else { Some("timeout") },
                        ..Default::default()
                    }],
                    &probe_domain::Policy::default(),
                )
                .unwrap();
            storage
                .finish_round_with_counts(id, None, 1, i64::from(ok), i64::from(!ok))
                .unwrap();
        }
        Arc::new(Mutex::new(storage))
    }

    /// Any/all decisions and persisted endpoint metadata follow the measured address set.
    #[tokio::test]
    async fn any_and_all_rounds_record_original_names_and_endpoint_counts() {
        for policy in [DomainPass::Any, DomainPass::All] {
            let storage = Arc::new(Mutex::new(Storage::open_in_memory().unwrap()));
            let mut plan = plan(vec![
                endpoint_job("good", "192.0.2.1"),
                endpoint_job("bad", "2001:db8::1"),
            ]);
            plan.domain_pass = policy;
            let outcome = run_stored(
                plan,
                Arc::new(FakeTester::new(true).killing(&["bad"])),
                storage.clone(),
            )
            .await;
            let stored = storage.lock().unwrap();
            let node = stored.get_node("air", "node").unwrap().unwrap();
            assert_eq!(node.ip_alive, Some(1));
            assert_eq!(node.ip_total, Some(2));
            assert_eq!(node.display.as_deref(), Some("upstream-node"));
            assert_eq!(node.server.as_deref(), Some("origin.example"));
            assert_eq!(node.proto.as_deref(), Some("trojan"));
            assert_eq!(node.status == "alive", policy == DomainPass::Any);
            let rows = stored.results_for_round(outcome.round_id).unwrap();
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|row| row.endpoint_ip.is_some()
                && row.original_server.as_deref() == Some("origin.example")));
            if policy == DomainPass::All {
                assert_eq!(node.last_reason.as_deref(), Some("timeout"));
                assert_eq!(node.consec_fail, 1);
            }
        }
    }

    /// Alternative fronts are not additional IPs that every "all" policy must pass.
    #[test]
    fn all_policy_requires_each_endpoint_not_each_front() {
        let variants = [
            NodeVerdict::alive(endpoint_job("front-a-ip1", "192.0.2.1"), 30),
            NodeVerdict::dead(
                endpoint_job("front-b-ip1", "192.0.2.1"),
                "timeout",
                "failed",
            ),
            NodeVerdict::alive(endpoint_job("front-a-ip2", "192.0.2.2"), 50),
        ];
        let folded = fold_verdicts(&variants, DomainPass::All);
        assert!(folded[0].ok);
        assert_eq!((folded[0].ip_alive, folded[0].ip_total), (2, 2));
    }

    /// Incomplete DNS cannot consume the third failure; unrelated sources still converge.
    #[tokio::test]
    async fn uncertain_dns_preserves_the_node_and_isolates_its_source() {
        let storage = pending_ledger();
        let before = storage.lock().unwrap().get_node("air", "node").unwrap();
        let mut uncertain = endpoint_job("uncertain", "192.0.2.1");
        uncertain.dns_incomplete = true;
        let good = Job::new("backup", "healthy", "healthy", "direct", "192.0.2.2");
        let outcome = run_stored(
            plan(vec![uncertain, good]),
            Arc::new(FakeTester::new(true).killing(&["uncertain"])),
            storage.clone(),
        )
        .await;
        let stored = storage.lock().unwrap();
        assert_eq!(stored.get_node("air", "node").unwrap(), before);
        assert_eq!(
            stored
                .get_node("backup", "healthy")
                .unwrap()
                .unwrap()
                .status,
            "alive"
        );
        assert_eq!(outcome.preserved_sources, ["air"]);
        assert_eq!(
            (
                outcome.counts.ok,
                outcome.counts.failed,
                outcome.counts.inconclusive
            ),
            (1, 0, 1)
        );
        // 2026-10-21: a round that measured a node is NOT round-inconclusive
        // (ADR-0005 write conditions only). The uncertain NODE is preserved
        // (its state and streak are untouched above); the round itself
        // measured something, so it converges the healthy node and can serve
        // as a baseline. No suspect flag either: this ledger has no previous
        // round, and a first round is never suspect.
        assert_eq!(outcome.guard, GuardDecision::ApplyConvergence);
        assert!(!outcome.suspect);
        assert!(
            !outcome.note.contains("incomplete coverage"),
            "note must not claim the whole round was uncovered: {}",
            outcome.note
        );
        assert_eq!(
            outcome.results_written, 2,
            "real endpoint observations remain inspectable"
        );
    }

    /// A known passing endpoint is enough for any, but cannot prove all unknown endpoints.
    #[tokio::test]
    async fn incomplete_dns_with_a_pass_respects_any_vs_all() {
        for policy in [DomainPass::Any, DomainPass::All] {
            let storage = pending_ledger();
            let mut job = endpoint_job("passing", "192.0.2.1");
            job.dns_incomplete = true;
            let mut plan = plan(vec![job]);
            plan.domain_pass = policy;
            let outcome = run_stored(plan, Arc::new(FakeTester::new(true)), storage.clone()).await;
            let node = storage
                .lock()
                .unwrap()
                .get_node("air", "node")
                .unwrap()
                .unwrap();
            assert_eq!(
                node.consec_fail,
                if policy == DomainPass::Any { 0 } else { 2 }
            );
            assert_eq!(
                outcome.preserved_sources.is_empty(),
                policy == DomainPass::Any
            );
        }
    }

    /// Entry exclusions beat front failure and are persisted without running the tester.
    #[tokio::test]
    async fn excluded_chains_clear_streaks_without_a_dial() {
        let storage = pending_ledger();
        let mut job = endpoint_job("excluded", "192.0.2.1").with_role(Role::Chain, None);
        job.excluded_reason = Some("entry_cn".into());
        let tester = Arc::new(FakeTester::new(false));
        let outcome = run_stored(plan(vec![job]), tester.clone(), storage.clone()).await;
        assert_eq!(tester.started(), 0);
        assert_eq!((outcome.counts.failed, outcome.counts.excluded), (0, 1));
        let stored = storage.lock().unwrap();
        let node = stored.get_node("air", "node").unwrap().unwrap();
        assert_eq!(node.status, "excluded");
        assert_eq!(node.consec_fail, 0);
        assert_eq!(node.total_fail, 2);
        let rows = stored.results_for_round(outcome.round_id).unwrap();
        assert_eq!(rows[0].verdict.as_deref(), Some("excluded"));
        assert_eq!(rows[0].attempts, Some(0));
    }

    /// A live total-budget timer cancels; dropping its owner prevents later cancellation.
    #[tokio::test]
    async fn total_round_deadlines_are_owned_and_cancellable() {
        let ctx = RoundCtx::new(loose());
        let deadline = crate::RoundDeadline::arm(&ctx, Duration::from_millis(10));
        tokio::time::timeout(Duration::from_secs(1), ctx.cancel_token().cancelled())
            .await
            .unwrap();
        drop(deadline);
        let ctx = RoundCtx::new(loose());
        let deadline = crate::RoundDeadline::arm(&ctx, Duration::from_millis(10));
        drop(deadline);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!ctx.is_cancelled());
    }
}
