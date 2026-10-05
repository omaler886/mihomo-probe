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
//! A cancelled round stops **writing results** -- a straggler's verdict belongs
//! to a round that no longer exists, and recording it would advance a failure
//! streak for a test that never finished.
//!
//! It does **not** skip closing the round row. "Do not write" and "do not
//! leave the ledger broken" are separate rules, and conflating them is how a
//! cancel turns into an orphan row that the next start-up has to reap.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use probe_domain::{DomainError, DomainResult, RoundId};
use probe_mihomo::{Controller, DelayOutcome, KernelError};
use probe_source::Role;
use probe_storage::{ResultRow, Storage, VERDICT_FAIL, VERDICT_OK};

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
}

impl NodeVerdict {
    pub fn alive(job: Job, delay_ms: u32) -> Self {
        Self {
            job,
            verdict: Verdict::Alive { delay_ms },
            attempts: 1,
            url: None,
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

    pub fn is_alive(&self) -> bool {
        matches!(self.verdict, Verdict::Alive { .. })
    }

    /// The failure class, `None` when the node answered.
    pub fn reason(&self) -> Option<&str> {
        match &self.verdict {
            Verdict::Alive { .. } => None,
            Verdict::Dead { kind, .. } => Some(kind),
        }
    }

    /// The shared `results` row. Vocabulary copied from Python: `ok` / `fail`,
    /// `detail` bounded to 200 chars.
    pub fn to_result_row(&self) -> ResultRow {
        match &self.verdict {
            Verdict::Alive { delay_ms } => ResultRow {
                source: self.job.source_id.clone(),
                fingerprint: self.job.fingerprint.clone(),
                display: Some(self.job.proxy_name.clone()),
                verdict: Some(VERDICT_OK.into()),
                delay_ms: Some(i64::from(*delay_ms)),
                reason: None,
                country: None,
                attempts: Some(i64::from(self.attempts)),
                detail: Some(String::new()),
                category: Some(self.job.variant.clone()),
            },
            Verdict::Dead { kind, message } => ResultRow {
                source: self.job.source_id.clone(),
                fingerprint: self.job.fingerprint.clone(),
                display: Some(self.job.proxy_name.clone()),
                verdict: Some(VERDICT_FAIL.into()),
                delay_ms: None,
                reason: Some(kind.clone()),
                country: None,
                attempts: Some(i64::from(self.attempts)),
                detail: Some(bounded(message, DETAIL_LIMIT)),
                category: Some(self.job.variant.clone()),
            },
        }
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
}

/// The ledger side. [`Mutex<Storage>`] implements it (see below); the runner
/// only ever sees this trait, so its tests need no database file.
pub trait Ledger: Send + Sync {
    fn start_round(&self, trigger: &str, mode: Option<&str>) -> DomainResult<RoundId>;
    /// Write one round's results. Called at most once per round, and never
    /// after a cancellation.
    fn record_verdicts(&self, round_id: RoundId, verdicts: &[NodeVerdict]) -> DomainResult<usize>;
    /// Fold the round's aggregated per-node outcomes into the nodes table
    /// (R7 convergence: streaks, transitions, history). An empty slice is a
    /// no-op returning the zero summary.
    fn converge(
        &self,
        round_id: RoundId,
        nodes: &[probe_storage::ConvergeNode],
        policy: &probe_domain::Policy,
    ) -> DomainResult<probe_storage::ConvergenceSummary>;
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
}

/// What the kernel was able to do before the node phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelState {
    Reloaded,
    /// The controller answered but refused the config: the caller recreates
    /// the kernel (Python `start_and_load`). Nodes are still tested, because
    /// the kernel may be serving the previous config.
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
}

impl RoundSettings {
    pub fn from_config(cfg: &probe_config::Config) -> Self {
        Self {
            limits: Limits::from_concurrency(cfg.test.concurrency),
            test: cfg.test.clone(),
            blocked: None,
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
/// * [`GuardDecision::MarkRoundInconclusive`] -- the kernel was unreachable
///   or there was no config to load. Nothing is tested, nothing converges,
///   no streak moves, and the round row carries `inconclusive = 1` so the
///   alive-baseline reader (`previous_alive_count`) skips it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardDecision {
    ApplyConvergence,
    PreservePreviousState,
    MarkRoundInconclusive,
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
    let planned = plan.jobs.len();

    let kernel_state = kernel.prepare().await;

    // Guard rule #1/#2 (workstreams/03): a kernel that is not there -- or a
    // round with no config to load -- measures nothing and must not be
    // recorded as mass node failure. The round is marked inconclusive
    // instead (ADR-0005).
    let (verdicts, inconclusive) = match kernel_state {
        KernelState::Unreachable(_) | KernelState::NotPrepared(_) => (Vec::new(), true),
        KernelState::Reloaded | KernelState::Refused => {
            (run_phases(&plan.jobs, &ctx, &tester).await, false)
        }
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
    let front_dead = verdicts
        .iter()
        .filter(|v| v.job.role == Role::Chain && v.reason() == Some(FRONT_DEAD))
        .count();

    let ok = verdicts.iter().filter(|v| v.is_alive()).count();
    let counts = RoundCounts {
        total: verdicts.len() as i64,
        ok: ok as i64,
        failed: (verdicts.len() - ok) as i64,
        dropped: 0,
        restored: 0,
    };

    // R7 convergence: fold the round's verdicts into the nodes table -- the
    // streaks are facts about the nodes, not about the publication. A
    // cancelled round folds nothing (its verdicts were dropped by the
    // workers' own checks), and an inconclusive round has no verdicts at all.
    let (convergence, suspect, guard) = if ctx.check().is_ok() && !inconclusive {
        let folded = fold_verdicts(&verdicts);
        let borrowed: Vec<probe_storage::ConvergeNode> = folded
            .iter()
            .map(|node| probe_storage::ConvergeNode {
                source: &node.source,
                fingerprint: &node.fingerprint,
                display: &node.display,
                category: node.category.as_deref(),
                ok: node.ok,
                delay_ms: node.delay_ms,
                reason: node.reason.as_deref(),
            })
            .collect();
        let summary = ledger.converge(round_id, &borrowed, &plan.policy)?;
        let alive_prev = ledger.previous_alive_count()?;
        if probe_domain::round_is_suspect(summary.alive_nodes, alive_prev, &plan.policy) {
            (summary, true, GuardDecision::PreservePreviousState)
        } else {
            (summary, false, GuardDecision::ApplyConvergence)
        }
    } else {
        (
            probe_storage::ConvergenceSummary::default(),
            false,
            GuardDecision::MarkRoundInconclusive,
        )
    };
    let counts = RoundCounts {
        dropped: convergence.dropped,
        restored: convergence.restored,
        ..counts
    };

    // The single guard for "a cancelled round writes no results". It reads the
    // same token the workers checked, so there is no window between the last
    // worker returning and this decision. An empty batch is skipped rather
    // than written as an empty transaction -- a round that tested nothing has
    // no rows to record, and saying so with a no-op call only makes the ledger
    // log lie about how many writes happened.
    //
    // A failed batch is held rather than propagated: `?` here would skip
    // `finish_round` below and leave the row open, which is the one outcome
    // this function promises never to produce.
    let (results_written, write_error) = if verdicts.is_empty() || ctx.check().is_err() {
        (0, None)
    } else {
        match ledger.record_verdicts(round_id, &verdicts) {
            Ok(written) => (written, None),
            Err(err) => (0, Some(err)),
        }
    };

    let cancelled = ctx.is_cancelled();
    let mut note = compose_note(&kernel_state, counts, cancelled, planned);
    if suspect {
        // Python: "alive X < floor from previous Y; not published".
        note.push_str(&format!(
            "; suspect: alive {} below floor from previous {}; publish preserved",
            convergence.alive_nodes, plan.policy.suspect_floor_absolute
        ));
    }
    // Unconditional, cancellation included: see the module docs.
    ledger.finish_round(round_id, &note, counts, suspect, inconclusive)?;
    // The row is closed; now the earlier failure, if any, is safe to report.
    if let Some(err) = write_error {
        return Err(err);
    }

    Ok(RoundOutcome {
        round_id,
        kernel: kernel_state,
        counts,
        cancelled,
        results_written,
        note,
        verdicts,
        live_fronts,
        front_dead,
        guard,
        suspect,
        dropped_nodes: convergence.dropped,
        restored_nodes: convergence.restored,
    })
}

/// Reduce the round's per-variant verdicts to one outcome per node
/// (Python `_score_bucket`, `domain_pass = "any"`): a node is alive when any
/// of its variants answered, the recorded delay is the best of the living,
/// and a dead node reports the first failing variant's reason. The category
/// follows the measurement -- the variant that actually passed -- so a chain
/// whose chain variant failed but whose direct twin passed is folded as a
/// direct pass, never as a chain the round just disproved.
fn fold_verdicts(verdicts: &[NodeVerdict]) -> Vec<FoldedNode> {
    struct Fold {
        display: String,
        ok: bool,
        delay: Option<i64>,
        reason: Option<String>,
        category: Option<String>,
    }

    let mut order: Vec<(String, String)> = Vec::new();
    let mut folds: HashMap<(String, String), Fold> = HashMap::new();
    for verdict in verdicts {
        let key = (
            verdict.job.source_id.clone(),
            verdict.job.fingerprint.clone(),
        );
        let delay = match &verdict.verdict {
            Verdict::Alive { delay_ms } => Some(i64::from(*delay_ms)),
            Verdict::Dead { .. } => None,
        };
        match folds.get_mut(&key) {
            Some(fold) => {
                if let Some(d) = delay {
                    fold.ok = true;
                    fold.delay = Some(fold.delay.map_or(d, |best| best.min(d)));
                    fold.reason = None;
                    fold.category = Some(verdict.job.variant.clone());
                } else if !fold.ok && fold.reason.is_none() {
                    fold.reason = verdict.reason().map(str::to_string);
                }
            }
            None => {
                order.push(key.clone());
                let alive = delay.is_some();
                folds.insert(
                    key,
                    Fold {
                        display: verdict.job.proxy_name.clone(),
                        ok: alive,
                        delay,
                        reason: if alive {
                            None
                        } else {
                            verdict.reason().map(str::to_string)
                        },
                        category: if alive {
                            Some(verdict.job.variant.clone())
                        } else {
                            None
                        },
                    },
                );
            }
        }
    }
    order
        .into_iter()
        .map(|key| {
            let fold = folds.remove(&key).expect("fold tracked in order");
            FoldedNode {
                source: key.0,
                fingerprint: key.1,
                display: fold.display,
                category: fold.category,
                ok: fold.ok,
                delay_ms: fold.delay,
                reason: fold.reason,
            }
        })
        .collect()
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
async fn run_phases(jobs: &[Job], ctx: &RoundCtx, tester: &Arc<dyn NodeTester>) -> Vec<NodeVerdict> {
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
    let tried: HashSet<(&str, &str)> = runnable
        .iter()
        .map(|job| (job.source_id.as_str(), job.fingerprint.as_str()))
        .collect();
    let mut failed: HashSet<(&str, &str)> = HashSet::new();
    for job in &second {
        let key = (job.source_id.as_str(), job.fingerprint.as_str());
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
    for job in jobs {
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
            Some(verdict)
        });
    }

    let mut out = Vec::with_capacity(jobs.len());
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
                Some(Ok(Some(verdict))) => out.push(verdict),
                // Cancelled or gate-closed mid-flight.
                Some(Ok(None)) => {}
                // A panicked worker is not a node verdict; the round still closes.
                Some(Err(err)) => {
                    tracing::error!(panic = err.is_panic(), %err, "probe worker ended early");
                }
            },
        }
    }
    out
}

/// The round's note. Kept short and free of anything that could carry a
/// credential: the controller error text is already bounded by the caller.
fn compose_note(
    kernel: &KernelState,
    counts: RoundCounts,
    cancelled: bool,
    planned: usize,
) -> String {
    let base = match kernel {
        KernelState::Reloaded => "kernel reloaded",
        KernelState::Refused => "kernel reload refused",
        KernelState::Unreachable(_) => {
            // "unreachable" is the word the API's own test asserts on, and the
            // word an operator greps for.
            return format!("kernel unreachable; {planned} node(s) not tested");
        }
        KernelState::NotPrepared(_) => {
            return format!("no kernel config this round; {planned} node(s) not tested");
        }
    };
    if cancelled {
        format!(
            "{base}; cancelled after {} of {planned} tested",
            counts.total
        )
    } else {
        format!(
            "{base}; tested {}/{}: ok {}, failed {}",
            counts.total, planned, counts.ok, counts.failed
        )
    }
}

/// The real ledger: one `Storage` behind a mutex.
///
/// The lock is taken inside each call and never held across an `await`, which
/// is why a `std` mutex is correct here -- and why the runner can hold this
/// while other request handlers still reach `/readyz` and `/api/v1/status`.
impl Ledger for Mutex<Storage> {
    fn start_round(&self, trigger: &str, mode: Option<&str>) -> DomainResult<RoundId> {
        self.lock()
            .map_err(|_| DomainError::Storage("ledger lock poisoned".into()))?
            .start_round(trigger, mode)
    }

    fn record_verdicts(&self, round_id: RoundId, verdicts: &[NodeVerdict]) -> DomainResult<usize> {
        let rows: Vec<ResultRow> = verdicts.iter().map(NodeVerdict::to_result_row).collect();
        self.lock()
            .map_err(|_| DomainError::Storage("ledger lock poisoned".into()))?
            .record_results(round_id, &rows)
    }

    fn converge(
        &self,
        round_id: RoundId,
        nodes: &[probe_storage::ConvergeNode],
        policy: &probe_domain::Policy,
    ) -> DomainResult<probe_storage::ConvergenceSummary> {
        self.lock()
            .map_err(|_| DomainError::Storage("ledger lock poisoned".into()))?
            .converge_nodes(round_id, nodes, policy)
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
            }
        }

        /// Mark these kernel names dead; everything else answers `alive`.
        fn killing(mut self, dead: &[&str]) -> Self {
            self.dead_names = dead.iter().map(|s| s.to_string()).collect();
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
                    NodeVerdict::dead(job, "timeout", "Timeout")
                }
            })
        }
    }

    #[derive(Default)]
    struct Recorded {
        started: Vec<(String, Option<String>)>,
        results: Vec<(RoundId, Vec<NodeVerdict>)>,
        finished: Vec<(RoundId, String, RoundCounts, bool, bool)>,
        converged: Vec<(RoundId, usize)>,
    }

    struct FakeLedger {
        recorded: Mutex<Recorded>,
        /// Make `record_verdicts` fail, to prove the round row still closes.
        fail_record: bool,
        /// Make `finish_round` fail, to prove the failure is reported.
        fail_finish: bool,
        /// What `previous_alive_count` reports: the suspect guard's baseline.
        baseline: Mutex<i64>,
    }

    impl FakeLedger {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                recorded: Mutex::new(Recorded::default()),
                fail_record: false,
                fail_finish: false,
                baseline: Mutex::new(0),
            })
        }

        /// A fake with a nonzero alive baseline: the suspect guard bites.
        fn with_baseline(baseline: i64) -> Arc<Self> {
            Arc::new(Self {
                recorded: Mutex::new(Recorded::default()),
                fail_record: false,
                fail_finish: false,
                baseline: Mutex::new(baseline),
            })
        }

        fn refusing_writes() -> Arc<Self> {
            Arc::new(Self {
                recorded: Mutex::new(Recorded::default()),
                fail_record: true,
                fail_finish: false,
                baseline: Mutex::new(0),
            })
        }

        fn refusing_close() -> Arc<Self> {
            Arc::new(Self {
                recorded: Mutex::new(Recorded::default()),
                fail_record: false,
                fail_finish: true,
                baseline: Mutex::new(0),
            })
        }
    }

    impl Ledger for FakeLedger {
        fn start_round(&self, trigger: &str, mode: Option<&str>) -> DomainResult<RoundId> {
            let mut rec = self.recorded.lock().unwrap();
            rec.started
                .push((trigger.to_string(), mode.map(str::to_string)));
            Ok(rec.started.len() as RoundId)
        }

        fn record_verdicts(
            &self,
            round_id: RoundId,
            verdicts: &[NodeVerdict],
        ) -> DomainResult<usize> {
            if self.fail_record {
                return Err(DomainError::Storage("results table is locked".into()));
            }
            self.recorded
                .lock()
                .unwrap()
                .results
                .push((round_id, verdicts.to_vec()));
            Ok(verdicts.len())
        }

        fn converge(
            &self,
            round_id: RoundId,
            nodes: &[probe_storage::ConvergeNode],
            _policy: &probe_domain::Policy,
        ) -> DomainResult<probe_storage::ConvergenceSummary> {
            // The fake folds nothing; it records that the runner asked, with
            // how many nodes, so the guard tests can assert on the handoff.
            self.recorded
                .lock()
                .unwrap()
                .converged
                .push((round_id, nodes.len()));
            Ok(probe_storage::ConvergenceSummary::default())
        }

        fn previous_alive_count(&self) -> DomainResult<i64> {
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
            self.recorded
                .lock()
                .unwrap()
                .finished
                .push((round_id, note.to_string(), counts, suspect, inconclusive));
            Ok(())
        }
    }

    struct FakeKernel(KernelState);

    impl KernelPrep for FakeKernel {
        fn prepare(&self) -> BoxFuture<'_, KernelState> {
            Box::pin(async move { self.0.clone() })
        }
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
                restored: 0
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
                restored: 0
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
    async fn a_refused_reload_still_tests_nodes() {
        // Python's `start_and_load` recreates the kernel on a refused reload;
        // the kernel may still be serving the previous config, so the node
        // phase is not skipped.
        let outcome = run_round_in_test(
            plan(vec![job("air", "10.0.0.1")]),
            loose(),
            Arc::new(FakeKernel(KernelState::Refused)),
            Arc::new(FakeTester::new(true)),
            FakeLedger::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.counts.total, 1);
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
            named_job("n #0", "fp-node", "9.9.9.9", Role::Chain, Some("__FRONT0__")),
            named_job("n #1", "fp-node", "8.8.8.8", Role::Chain, Some("__FRONT1__")),
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
            named_job("n #0", "fp-node", "9.9.9.9", Role::Chain, Some("__FRONT0__")),
            named_job("n #1", "fp-node", "8.8.8.8", Role::Chain, Some("__FRONT1__")),
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
            Job::new("air", "fp", "n #0", "chain", "9.9.9.9").with_role(Role::Chain, Some("__FRONT0__".into())),
            80,
        );
        let alive1 = NodeVerdict::alive(
            Job::new("air", "fp", "n #1", "chain", "8.8.8.8").with_role(Role::Chain, Some("__FRONT1__".into())),
            60,
        );
        let other = NodeVerdict::dead(
            Job::new("air", "other", "m", "direct", "1.1.1.1"),
            "timeout",
            "Timeout",
        );
        let folded = fold_verdicts(&[alive0, alive1, other]);
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
            Job::new("air", "fp", "n #0", "chain", "9.9.9.9").with_role(Role::Chain, Some("__FRONT0__".into())),
            "front_dead",
            "front",
        );
        let twin = NodeVerdict::dead(
            Job::new("air", "fp", "n #d", "direct", "1.2.3.4"),
            "tls_error",
            "tls",
        );
        let folded = fold_verdicts(&[chain, twin]);
        assert_eq!(folded.len(), 1);
        assert!(!folded[0].ok);
        assert_eq!(folded[0].reason.as_deref(), Some("front_dead"), "first failure recorded");
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
        assert!(rec.converged.is_empty(), "nothing folds on an inconclusive round");
        let (_, _, _, suspect, inconclusive) = &rec.finished[0];
        assert!(*inconclusive, "the rounds row carries the ADR-0005 flag");
        assert!(!*suspect);
    }
}
