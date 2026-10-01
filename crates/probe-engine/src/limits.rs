//! Layered concurrency gate for one probe round (workstreams/06, R5).
//!
//! The Python engine runs the whole round through a single
//! `ThreadPoolExecutor(concurrency=20)` (`engine.py`), which is one flat knob:
//! it cannot stop 20 nodes that share one landing server from hitting that
//! server at once, and a slow node's retry occupies a slot that a fast node
//! could have used.
//!
//! This module replaces that with four explicit layers:
//!
//! ```text
//! global            every fast-lane job
//! ├── per_source    one subscription's nodes
//! └── per_server_ip one landing address
//!
//! diagnose          a separate lane, NOT under global
//! ```
//!
//! `diagnose` is deliberately *not* a child of `global`: it is the lane for
//! re-testing things already known to be slow, and putting it under `global`
//! would make it queue behind exactly the saturation it exists to bypass. The
//! consequence is that a round's peak in-flight count is `global + diagnose`,
//! not `global` -- size the two together.
//!
//! ## The ordering invariant (why this cannot deadlock)
//!
//! Every task acquires in the same order -- global, then source, then server
//! IP -- and never waits for a resource it already holds. A cycle would need
//! some task to hold a later resource while waiting for an earlier one, which
//! the fixed order makes impossible.
//!
//! The one way to break it is to ask for the *same* key twice in one task: the
//! semaphores are not re-entrant, so a task holding `per_server_ip` for `A`
//! that then waits for `per_server_ip` for `A` again blocks on itself. A chain
//! node touches two addresses (the front it dials and the landing behind it),
//! so the caller must pass **the address the kernel dials from this host** --
//! the front -- as `Job::server_ip`, not both.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Idle semaphores kept before the registry prunes itself.
///
/// A `Gate` lives for one round, so this is not about a process accumulating
/// semaphores over months: it bounds what a *single* round with thousands of
/// distinct landing addresses can allocate. Nothing is given up by pruning --
/// a re-created semaphore for the same key is equivalent to the old one as
/// long as no permit is outstanding, which the `strong_count` check ensures.
const REGISTRY_PRUNE_AT: usize = 1024;

/// Why a job could not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LimitError {
    /// The round was cancelled while this job was queued, or before it wrote
    /// its result. The caller must abandon the job -- and, on `check()`,
    /// abandon the ledger write.
    #[error("round cancelled")]
    Cancelled,
}

/// Per-layer ceilings for one round.
///
/// There is no `Default`: every value has to be chosen, because a silently
/// zeroed limit is a deadlock (`Semaphore::new(0)` never yields). Use
/// [`Limits::from_concurrency`] for the migration-period values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub global: usize,
    pub per_source: usize,
    pub per_server_ip: usize,
    pub diagnose: usize,
}

impl Limits {
    /// Upper bound for any single layer. Mirrors the Python lane clamp
    /// (`lane_count` is bounded to 32); a round that asked for more than this
    /// is a config error, not a request to open 100k sockets.
    pub const MAX: usize = 1024;

    /// Migration-period values.
    ///
    /// `global` keeps the deployed `test.concurrency` (20) as the fast-lane
    /// ceiling, so R5 changes *fairness* without changing how many fast-lane
    /// tests are in flight. Note the round's peak is `global + diagnose` --
    /// the diagnostic lane is outside `global` by design (see module docs).
    /// The other three are proportional starting points, **not** tuned values:
    /// workstreams/06 requires them to be set from real round data before the
    /// Rust path takes traffic. `per_server_ip` is deliberately the tightest
    /// -- several nodes on one box are the common case in a subscription.
    pub fn from_concurrency(concurrency: usize) -> Self {
        let global = concurrency.clamp(1, Self::MAX);
        Self {
            global,
            per_source: (global / 2).max(1),
            per_server_ip: (global / 4).max(1),
            diagnose: (global / 4).max(1),
        }
    }

    /// Clamp every layer into `1..=MAX`.
    ///
    /// A zero (or absurd) limit must degrade to "one at a time", never to a
    /// hang: the Python side already had to learn this with `mixed_port`
    /// defaulting to nothing and `lane_count` clamping 0 to 1.
    pub fn effective(self) -> Self {
        Self {
            global: self.global.clamp(1, Self::MAX),
            per_source: self.per_source.clamp(1, Self::MAX),
            per_server_ip: self.per_server_ip.clamp(1, Self::MAX),
            diagnose: self.diagnose.clamp(1, Self::MAX),
        }
    }
}

/// One unit of work, identified by the keys each consumer needs.
///
/// The gate only reads `source_id` and `server_ip`; the other three travel
/// with the job so the result can be attributed without a side lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub source_id: String,
    /// Ledger identity -- lands in `results.fingerprint`.
    pub fingerprint: String,
    /// Kernel proxy name -- what `GET /proxies/{name}/delay` addresses. Unique
    /// within one generated config because `prepare()` de-duplicates, but it is
    /// a display name and not stable across rounds: never key on it.
    pub proxy_name: String,
    /// `direct` / `chain`; lands in `results.category`.
    pub variant: String,
    /// The address the kernel dials from this host. For a chain node that is
    /// the *front*, not the landing -- see the module docs.
    pub server_ip: String,
}

impl Job {
    pub fn new(
        source_id: impl Into<String>,
        fingerprint: impl Into<String>,
        proxy_name: impl Into<String>,
        variant: impl Into<String>,
        server_ip: impl Into<String>,
    ) -> Self {
        Self {
            source_id: source_id.into(),
            fingerprint: fingerprint.into(),
            proxy_name: proxy_name.into(),
            variant: variant.into(),
            server_ip: server_ip.into(),
        }
    }
}

/// Holds one permit from each layer until dropped.
///
/// Release is by `Drop`, and `Vec` drops front to back -- so `global` goes
/// back first, then the source, then the address. That order is harmless (a
/// task never waits for a layer it has already released) and is not something
/// to rely on either way. There is deliberately no `release()` method: an
/// explicit one is a way to release twice, or to forget.
#[derive(Debug)]
#[must_use = "dropping the permit immediately releases the slot"]
pub struct Permit {
    held: Vec<OwnedSemaphorePermit>,
}

impl Permit {
    /// How many layers this permit occupies. Three for a fast-lane job, one
    /// for a diagnostic job.
    pub fn levels(&self) -> usize {
        self.held.len()
    }
}

type Registry = Mutex<HashMap<String, Arc<Semaphore>>>;

/// The four semaphores, shared by every task in a round.
#[derive(Debug)]
pub struct Gate {
    limits: Limits,
    global: Arc<Semaphore>,
    sources: Registry,
    server_ips: Registry,
    diagnose: Arc<Semaphore>,
}

impl Gate {
    pub fn new(limits: Limits) -> Self {
        let limits = limits.effective();
        Self {
            global: Arc::new(Semaphore::new(limits.global)),
            sources: Mutex::new(HashMap::new()),
            server_ips: Mutex::new(HashMap::new()),
            diagnose: Arc::new(Semaphore::new(limits.diagnose)),
            limits,
        }
    }

    /// The limits actually in force (after clamping).
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Free slots on the fast-lane and diagnostic pools.
    ///
    /// The fast-lane figure is the `global` pool only: the per-source and
    /// per-address pools are created lazily and are meaningless when empty.
    /// After a round has drained this must read back as the configured widths
    /// -- that is the leak check, and it is what R9 will export as a gauge.
    pub fn available_permits(&self) -> (usize, usize) {
        (
            self.global.available_permits(),
            self.diagnose.available_permits(),
        )
    }

    /// Take a slot on all three fast-lane layers.
    ///
    /// Cancellation wins over acquisition: a round that is being torn down
    /// must not have queued jobs trickling in behind it.
    pub async fn acquire(
        &self,
        job: &Job,
        cancel: &CancellationToken,
    ) -> Result<Permit, LimitError> {
        let mut held = Vec::with_capacity(3);
        held.push(acquire_one(Arc::clone(&self.global), cancel).await?);
        held.push(
            self.acquire_keyed(
                &self.sources,
                &job.source_id,
                self.limits.per_source,
                cancel,
            )
            .await?,
        );
        held.push(
            self.acquire_keyed(
                &self.server_ips,
                &job.server_ip,
                self.limits.per_server_ip,
                cancel,
            )
            .await?,
        );
        Ok(Permit { held })
    }

    /// Take a slot on the diagnostic lane only.
    ///
    /// Deliberately *not* layered on top of global/source/IP: a diagnostic job
    /// is a retry of something already known to be slow, and re-entering the
    /// fast lane's per-IP queue would let the slow nodes throttle each other
    /// while the fast lane sits idle -- the exact coupling R5 exists to break.
    pub async fn acquire_diagnose(&self, cancel: &CancellationToken) -> Result<Permit, LimitError> {
        let permit = acquire_one(Arc::clone(&self.diagnose), cancel).await?;
        Ok(Permit { held: vec![permit] })
    }

    async fn acquire_keyed(
        &self,
        registry: &Registry,
        key: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, LimitError> {
        let semaphore = semaphore_for(registry, key, limit);
        acquire_one(semaphore, cancel).await
    }

    /// Number of `(source, server-ip)` semaphores currently tracked. Test-only
    /// hook for the pruning assertion.
    #[cfg(test)]
    fn tracked_keys(&self) -> (usize, usize) {
        (
            self.sources.lock().unwrap().len(),
            self.server_ips.lock().unwrap().len(),
        )
    }
}

/// One round's shared state: the gate, plus the token that ends it.
///
/// Cloneable; every clone shares the same gate and the same token, which is
/// what lets the scheduler cancel a round that workers are still draining.
#[derive(Debug, Clone)]
pub struct RoundCtx {
    gate: Arc<Gate>,
    cancel: CancellationToken,
}

impl RoundCtx {
    pub fn new(limits: Limits) -> Self {
        Self {
            gate: Arc::new(Gate::new(limits)),
            cancel: CancellationToken::new(),
        }
    }

    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// End the round: queued acquisitions fail, and `check()` starts refusing.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Gate for the fast lane.
    pub async fn acquire(&self, job: &Job) -> Result<Permit, LimitError> {
        self.gate.acquire(job, &self.cancel).await
    }

    /// Gate for the diagnostic lane.
    pub async fn acquire_diagnose(&self) -> Result<Permit, LimitError> {
        self.gate.acquire_diagnose(&self.cancel).await
    }

    /// **Call this before every ledger write.**
    ///
    /// workstreams/06 requires that a cancelled round stops writing: once the
    /// round is torn down, a straggling worker's result belongs to no round and
    /// would advance a node's failure streak for a test that never finished.
    /// A permit does not imply this -- a task can be holding a permit when the
    /// cancel lands.
    pub fn check(&self) -> Result<(), LimitError> {
        if self.cancel.is_cancelled() {
            Err(LimitError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Wait for one permit, giving cancellation priority.
///
/// `biased` matters: without it a cancelled round could still let a job
/// through whenever the permit happened to be ready in the same poll.
async fn acquire_one(
    semaphore: Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Result<OwnedSemaphorePermit, LimitError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(LimitError::Cancelled),
        permit = semaphore.acquire_owned() => permit.map_err(|_| LimitError::Cancelled),
    }
}

/// Get (or create) the semaphore for one key, pruning idle ones first.
///
/// The `Mutex` is a `std` one on purpose: it is only ever held to clone an
/// `Arc`, never across an `await`.
fn semaphore_for(registry: &Registry, key: &str, limit: usize) -> Arc<Semaphore> {
    let mut map = registry.lock().expect("gate registry poisoned");
    if map.len() >= REGISTRY_PRUNE_AT {
        // `strong_count == 1` means only this map holds the semaphore, so no
        // permit is outstanding and the entry is safe to drop. A waiting task
        // holds a clone, which keeps its entry alive.
        map.retain(|_, semaphore| Arc::strong_count(semaphore) > 1);
    }
    Arc::clone(
        map.entry(key.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(limit))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    const HOLD: Duration = Duration::from_millis(25);

    /// Tracker key for "how many jobs are in flight overall". Not a real
    /// source name, so it cannot collide with the per-source counters.
    const GLOBAL: &str = "__global__";

    /// Peak simultaneous occupants, per key and overall.
    #[derive(Default)]
    struct Tracker {
        live: Mutex<HashMap<String, usize>>,
        peak: Mutex<HashMap<String, usize>>,
    }

    impl Tracker {
        fn enter(&self, key: &str) {
            let mut live = self.live.lock().unwrap();
            let now = live.entry(key.to_owned()).or_default();
            *now += 1;
            let mut peak = self.peak.lock().unwrap();
            let high = peak.entry(key.to_owned()).or_default();
            *high = (*high).max(*now);
        }

        fn leave(&self, key: &str) {
            let mut live = self.live.lock().unwrap();
            *live.entry(key.to_owned()).or_default() -= 1;
        }

        fn peak(&self, key: &str) -> usize {
            *self.peak.lock().unwrap().get(key).unwrap_or(&0)
        }
    }

    /// Run one job to completion, recording how many were inside at once.
    async fn run(ctx: &RoundCtx, job: Job, tracker: &Tracker) {
        let _permit = ctx.acquire(&job).await.expect("round not cancelled");
        tracker.enter(GLOBAL);
        tracker.enter(&job.source_id);
        tracker.enter(&job.server_ip);
        tokio::time::sleep(HOLD).await;
        tracker.leave(&job.server_ip);
        tracker.leave(&job.source_id);
        tracker.leave(GLOBAL);
        ctx.check().expect("round not cancelled");
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn global_limit_is_enforced() {
        let ctx = RoundCtx::new(Limits {
            global: 2,
            per_source: 8,
            per_server_ip: 8,
            diagnose: 2,
        });
        let tracker = Arc::new(Tracker::default());
        let mut tasks = Vec::new();
        for i in 0..8 {
            // Distinct sources *and* addresses, so only the global layer binds.
            let (ctx, tracker) = (ctx.clone(), Arc::clone(&tracker));
            tasks.push(tokio::spawn(async move {
                run(
                    &ctx,
                    job(&format!("s{i}"), &format!("10.0.0.{i}")),
                    &tracker,
                )
                .await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(
            tracker.peak(GLOBAL),
            2,
            "global=2 must cap the whole round at two in flight"
        );
        assert_eq!(
            tracker.peak("s0"),
            1,
            "each source holds one job here, so its own peak is 1"
        );
        let total: usize = tracker.live.lock().unwrap().values().sum();
        assert_eq!(total, 0, "every job left");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn per_source_limit_serialises_a_single_source() {
        let ctx = RoundCtx::new(Limits {
            global: 8,
            per_source: 1,
            per_server_ip: 8,
            diagnose: 2,
        });
        let tracker = Arc::new(Tracker::default());
        let mut tasks = Vec::new();
        // Contended: one source, four distinct addresses -- only per_source
        // can bind these, and it must hold them to one at a time.
        for i in 0..4 {
            let (ctx, tracker) = (ctx.clone(), Arc::clone(&tracker));
            tasks.push(tokio::spawn(async move {
                run(&ctx, job("one-source", &format!("10.1.0.{i}")), &tracker).await;
            }));
        }
        // Control: distinct sources *and* addresses, so nothing but `global`
        // can constrain them. Without these the whole test would run one job
        // at a time by construction, and the global assertion below could
        // never hold.
        for i in 0..3 {
            let (ctx, tracker) = (ctx.clone(), Arc::clone(&tracker));
            tasks.push(tokio::spawn(async move {
                run(
                    &ctx,
                    job(&format!("free-{i}"), &format!("10.1.9.{i}")),
                    &tracker,
                )
                .await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert!(
            tracker.peak(GLOBAL) > 1,
            "global=8 must actually admit several jobs, or this test would \
             pass even if the global layer were the binding one"
        );
        assert_eq!(
            tracker.peak("one-source"),
            1,
            "per_source=1 must serialise one subscription"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn per_server_ip_limit_serialises_one_address() {
        let ctx = RoundCtx::new(Limits {
            global: 8,
            per_source: 8,
            per_server_ip: 1,
            diagnose: 2,
        });
        let tracker = Arc::new(Tracker::default());
        let mut tasks = Vec::new();
        // Contended: one box, four different subscriptions -- only
        // per_server_ip can bind these.
        for i in 0..4 {
            let (ctx, tracker) = (ctx.clone(), Arc::clone(&tracker));
            tasks.push(tokio::spawn(async move {
                run(&ctx, job(&format!("s{i}"), "203.0.113.9"), &tracker).await;
            }));
        }
        // Control: distinct sources *and* addresses, so only `global` binds.
        for i in 0..3 {
            let (ctx, tracker) = (ctx.clone(), Arc::clone(&tracker));
            tasks.push(tokio::spawn(async move {
                run(
                    &ctx,
                    job(&format!("free-{i}"), &format!("203.0.113.{}", 40 + i)),
                    &tracker,
                )
                .await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert!(
            tracker.peak(GLOBAL) > 1,
            "global=8 must actually admit several jobs, or this test would \
             pass even if the global layer were the binding one"
        );
        assert_eq!(
            tracker.peak("203.0.113.9"),
            1,
            "per_server_ip=1 must serialise one landing address"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn all_three_layers_hold_at_once() {
        let limits = Limits {
            global: 6,
            per_source: 2,
            per_server_ip: 1,
            diagnose: 2,
        };
        let ctx = RoundCtx::new(limits);
        let tracker = Arc::new(Tracker::default());
        let mut tasks = Vec::new();
        for s in 0..3 {
            for ip in 0..3 {
                let (ctx, tracker) = (ctx.clone(), Arc::clone(&tracker));
                let (source, addr) = (format!("s{s}"), format!("10.2.{s}.{ip}"));
                tasks.push(tokio::spawn(async move {
                    run(&ctx, job(&source, &addr), &tracker).await;
                }));
            }
        }
        for t in tasks {
            t.await.unwrap();
        }
        let live = tracker.live.lock().unwrap().clone();
        assert!(live.values().all(|v| *v == 0), "no job outlived its permit");
        assert!(
            tracker.peak(GLOBAL) <= limits.global,
            "global peak {} exceeded {}",
            tracker.peak(GLOBAL),
            limits.global
        );
        let per_source_peak = (0..3)
            .map(|s| tracker.peak(&format!("s{s}")))
            .max()
            .unwrap();
        assert!(
            per_source_peak <= limits.per_source,
            "per_source peak {per_source_peak} exceeded {}",
            limits.per_source
        );
        let per_ip_peak = (0..3)
            .flat_map(|s| (0..3).map(move |ip| format!("10.2.{s}.{ip}")))
            .map(|addr| tracker.peak(&addr))
            .max()
            .unwrap();
        assert_eq!(per_ip_peak, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn permits_return_to_the_pool() {
        let limits = Limits {
            global: 3,
            per_source: 2,
            per_server_ip: 1,
            diagnose: 2,
        };
        let ctx = RoundCtx::new(limits);
        let mut tasks = Vec::new();
        for i in 0..12 {
            let ctx = ctx.clone();
            tasks.push(tokio::spawn(async move {
                let job = job(&format!("s{}", i % 2), &format!("10.3.0.{}", i % 4));
                let permit = ctx.acquire(&job).await.unwrap();
                assert_eq!(permit.levels(), 3);
                drop(permit);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(
            ctx.gate().global.available_permits(),
            limits.global,
            "global pool must be whole again"
        );
        assert_eq!(
            ctx.gate().diagnose.available_permits(),
            limits.diagnose,
            "diagnose pool must be untouched"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_fails_a_queued_job_instead_of_waiting() {
        let ctx = RoundCtx::new(Limits {
            global: 1,
            per_source: 1,
            per_server_ip: 1,
            diagnose: 1,
        });
        let held = ctx.acquire(&job("s", "10.4.0.1")).await.unwrap();
        let waiter = {
            let ctx = ctx.clone();
            tokio::spawn(async move { ctx.acquire(&job("s", "10.4.0.1")).await })
        };
        // Let the waiter park on the global semaphore first.
        tokio::time::sleep(Duration::from_millis(20)).await;
        ctx.cancel();
        let outcome = timeout(Duration::from_secs(2), waiter)
            .await
            .expect("cancel must wake a parked acquire, not leave it hanging")
            .unwrap();
        assert_eq!(outcome.unwrap_err(), LimitError::Cancelled);
        drop(held);
        assert_eq!(ctx.gate().global.available_permits(), 1);
    }

    #[tokio::test]
    async fn check_refuses_the_ledger_write_after_cancel() {
        let ctx = RoundCtx::new(Limits::from_concurrency(4));
        assert!(ctx.check().is_ok());
        assert!(!ctx.is_cancelled());
        ctx.cancel();
        assert!(ctx.is_cancelled());
        assert_eq!(ctx.check().unwrap_err(), LimitError::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mixed_load_finishes_within_budget() {
        // Deadlock guard. The lower bound is ~0.5s (60 jobs, 3 addresses,
        // per_server_ip=1, 25ms each); the budget is 10x that, which still
        // fails fast on a real deadlock (which would hang forever) while
        // tolerating a loaded CI box.
        let ctx = RoundCtx::new(Limits {
            global: 4,
            per_source: 2,
            per_server_ip: 1,
            diagnose: 1,
        });
        let tracker = Arc::new(Tracker::default());
        let mut tasks = Vec::new();
        for i in 0..60 {
            let (ctx, tracker) = (ctx.clone(), Arc::clone(&tracker));
            tasks.push(tokio::spawn(async move {
                // 4 sources x 3 addresses: every key is contended.
                run(
                    &ctx,
                    job(&format!("s{}", i % 4), &format!("10.5.0.{}", i % 3)),
                    &tracker,
                )
                .await;
            }));
        }
        let drain = async {
            for t in tasks {
                t.await.unwrap();
            }
        };
        timeout(Duration::from_secs(5), drain)
            .await
            .expect("layered gate deadlocked under mixed load");
        let per_ip_peak = (0..3)
            .map(|ip| tracker.peak(&format!("10.5.0.{ip}")))
            .max()
            .unwrap();
        assert_eq!(per_ip_peak, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn diagnose_lane_runs_while_the_fast_lane_is_saturated() {
        let ctx = RoundCtx::new(Limits {
            global: 1,
            per_source: 1,
            per_server_ip: 1,
            diagnose: 1,
        });
        let blocker = ctx.acquire(&job("s", "10.6.0.1")).await.unwrap();
        let diagnose = timeout(Duration::from_secs(2), ctx.acquire_diagnose())
            .await
            .expect("diagnose must not queue behind the fast lane")
            .unwrap();
        assert_eq!(diagnose.levels(), 1, "diagnose takes one layer, not three");
        drop(diagnose);
        drop(blocker);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_registry_entries_are_pruned() {
        let ctx = RoundCtx::new(Limits::from_concurrency(4));
        let mut tasks = Vec::new();
        // More than one prune window, so this exercises *repeated* pruning and
        // not just the first threshold crossing.
        for i in 0..(REGISTRY_PRUNE_AT * 2 + 50) {
            let ctx = ctx.clone();
            tasks.push(tokio::spawn(async move {
                let job = job(&format!("s{}", i % 3), &format!("198.51.100.{i}"));
                drop(ctx.acquire(&job).await.unwrap());
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let (sources, ips) = ctx.gate().tracked_keys();
        assert!(
            ips < REGISTRY_PRUNE_AT / 4,
            "server-ip registry held {ips} idle entries; pruning left far too \
             much behind (a value near {} means it never ran)",
            REGISTRY_PRUNE_AT * 2 + 50
        );
        assert!(
            sources <= 3,
            "only three sources were ever used, got {sources}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn zero_limits_degrade_to_serial_instead_of_hanging() {
        let ctx = RoundCtx::new(Limits {
            global: 0,
            per_source: 0,
            per_server_ip: 0,
            diagnose: 0,
        });
        assert_eq!(ctx.gate().limits().global, 1);
        let permit = timeout(Duration::from_secs(2), ctx.acquire(&job("s", "10.7.0.1")))
            .await
            .expect("a clamped gate must still hand out a permit")
            .unwrap();
        assert_eq!(permit.levels(), 3);
    }

    #[tokio::test]
    async fn effective_limits_clamp_at_the_ceiling() {
        let limits = Limits {
            global: usize::MAX,
            per_source: 0,
            per_server_ip: 99_999,
            diagnose: 1,
        }
        .effective();
        assert_eq!(limits.global, Limits::MAX);
        assert_eq!(limits.per_source, 1);
        assert_eq!(limits.per_server_ip, Limits::MAX);
        assert_eq!(limits.diagnose, 1);
    }

    #[test]
    fn from_concurrency_keeps_the_deployed_ceiling() {
        let limits = Limits::from_concurrency(20);
        assert_eq!(limits.global, 20, "R5 must not change in-flight count");
        assert_eq!(limits.per_source, 10);
        assert_eq!(limits.per_server_ip, 5);
        assert_eq!(limits.diagnose, 5);
    }
}
