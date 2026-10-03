//! `engine.test_one`: one node, up to `max_attempts` dials.
//!
//! The Python loop has four behaviours that a naive "try, then retry on
//! failure" would miss, and each one exists because of a real measurement:
//!
//! 1. **Targets are stably partitioned, HTTPS first**, and `https_required`
//!    follows from the head of that partition. A bare 204 over `http://` proves
//!    reachability, not usability -- once published, an HTTPS-broken node fails
//!    in real use. But when the config has *no* HTTPS target at all, Python
//!    stops requiring one rather than marking every node dead.
//! 2. **Only a timeout escalates the budget** (`timeout_ms` -> `timeout_ms_retry`).
//!    It is the one failure a bigger budget can overturn; giving a `bad_request`
//!    more time just spends more time.
//! 3. **`TERMINAL_REASONS` short-circuit the loop.** A kernel that says
//!    `bad_delay` will say it again.
//! 4. **A plain-HTTP pass is remembered, not reported.** If HTTPS then fails,
//!    the verdict is `fail` with a detail that says the HTTP probe worked --
//!    which is what tells an operator the node is up but the TLS path is broken,
//!    rather than "unreachable".

use std::sync::Arc;
use std::time::Duration;

use probe_config::TestConfig;
use probe_mihomo::Controller;

use crate::limits::Job;
use crate::round::{dial_once, NodeTester, NodeVerdict};

/// `engine.TERMINAL_REASONS`: retrying cannot change these.
pub const TERMINAL_REASONS: [&str; 4] = ["bad_request", "bad_delay", "bad_response", "unreachable"];

/// `(delay_ms, reason, detail)`; `reason` is `None` when the node answered.
pub type DialResult = (Option<u32>, Option<String>, String);

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// One dial. Behind a trait so the retry policy is testable without a kernel.
pub trait Dialer: Send + Sync {
    fn dial<'a>(
        &'a self,
        proxy: &'a str,
        url: &'a str,
        timeout_ms: u64,
        expected: &'a str,
    ) -> BoxFuture<'a, DialResult>;
}

impl Dialer for Controller {
    fn dial<'a>(
        &'a self,
        proxy: &'a str,
        url: &'a str,
        timeout_ms: u64,
        expected: &'a str,
    ) -> BoxFuture<'a, DialResult> {
        Box::pin(async move { dial_once(self, proxy, url, timeout_ms, expected).await })
    }
}

/// The retry policy, resolved from the `test` config.
#[derive(Debug, Clone)]
pub struct TestPolicy {
    /// Stable partition: every `https://` target before every other one.
    pub targets: Vec<String>,
    /// True when the head of the partition is HTTPS -- i.e. the config has at
    /// least one HTTPS target.
    pub https_required: bool,
    pub expected: String,
    pub max_attempts: usize,
    pub base_timeout_ms: u64,
    pub long_timeout_ms: u64,
    pub retry_pause: Duration,
}

impl TestPolicy {
    pub fn from_config(cfg: &TestConfig) -> Self {
        let targets = partition(&cfg.targets);
        let https_required = targets
            .first()
            .map(|t| t.starts_with("https://"))
            .unwrap_or(false);
        Self {
            targets,
            https_required,
            expected: cfg.expected_status.clone(),
            max_attempts: cfg.max_attempts.max(1),
            base_timeout_ms: cfg.timeout_ms,
            long_timeout_ms: cfg.timeout_ms_retry,
            retry_pause: Duration::from_secs_f64(cfg.retry_pause_s.max(0.0)),
        }
    }
}

/// `engine.test_one`'s stable partition: HTTPS targets keep their relative
/// order, then everything else keeps its.
pub fn partition(targets: &[String]) -> Vec<String> {
    let mut out: Vec<String> = targets
        .iter()
        .filter(|t| t.starts_with("https://"))
        .cloned()
        .collect();
    out.extend(
        targets
            .iter()
            .filter(|t| !t.starts_with("https://"))
            .cloned(),
    );
    out
}

/// The tester a round uses.
pub struct TestOne {
    dialer: Arc<dyn Dialer>,
    policy: TestPolicy,
}

impl TestOne {
    pub fn new(controller: Controller, cfg: &TestConfig) -> Self {
        Self::with_dialer(Arc::new(controller), cfg)
    }

    /// Build against any [`Dialer`] -- the seam the tests use.
    pub fn with_dialer(dialer: Arc<dyn Dialer>, cfg: &TestConfig) -> Self {
        Self {
            dialer,
            policy: TestPolicy::from_config(cfg),
        }
    }

    pub fn policy(&self) -> &TestPolicy {
        &self.policy
    }
}

impl NodeTester for TestOne {
    fn test<'a>(&'a self, job: Job) -> BoxFuture<'a, NodeVerdict> {
        Box::pin(async move {
            let policy = &self.policy;
            // `TestConfig` guarantees a non-empty list; this is the belt for a
            // hand-built policy, because indexing an empty list would panic a
            // worker and cost the round its verdict.
            if policy.targets.is_empty() {
                return NodeVerdict::dead(job, "target_error", "no test targets configured")
                    .with_attempts(0);
            }

            let mut timeout = policy.base_timeout_ms;
            let mut attempts = 0usize;
            let mut delay: Option<u32> = None;
            let mut reason = "unknown".to_string();
            let mut detail = String::new();
            let mut url = policy.targets[0].clone();
            // A plain-HTTP success is not a verdict; it is evidence kept for
            // the failure message.
            let mut http_pass: Option<(u32, String)> = None;
            let mut last_https: Option<(String, String)> = None;
            let mut last_fail: Option<(String, String)> = None;

            while attempts < policy.max_attempts {
                url = policy.targets[attempts % policy.targets.len()].clone();
                attempts += 1;
                let (dialed, failed, text) = self
                    .dialer
                    .dial(&job.proxy_name, &url, timeout, &policy.expected)
                    .await;
                delay = dialed;
                match failed {
                    None => {
                        if !policy.https_required || url.starts_with("https://") {
                            return NodeVerdict::alive(job, dialed.unwrap_or(0))
                                .with_attempts(attempts as u32)
                                .with_url(url);
                        }
                        http_pass = Some((dialed.unwrap_or(0), url.clone()));
                    }
                    Some(kind) => {
                        reason = kind.clone();
                        detail = text.clone();
                        last_fail = Some((kind.clone(), text.clone()));
                        if !policy.https_required || url.starts_with("https://") {
                            last_https = Some((kind.clone(), text.clone()));
                        }
                        if TERMINAL_REASONS.contains(&kind.as_str()) {
                            break;
                        }
                        if kind == "timeout" {
                            // The one failure a bigger budget can overturn.
                            timeout = policy.long_timeout_ms;
                        }
                    }
                }
                if attempts < policy.max_attempts {
                    tokio::time::sleep(policy.retry_pause).await;
                }
            }

            let (reason, detail) = match last_https {
                None => {
                    // Either the config has no HTTPS target (then `https_required`
                    // is false and a success would have returned above), or every
                    // attempt failed before any HTTPS target was dialled.
                    last_fail.unwrap_or((reason, detail))
                }
                Some((kind, text)) => match http_pass {
                    Some((http_delay, http_url)) => {
                        let combined = format!(
                            "{text}；plain-HTTP 探测点 {http_url} 通（{http_delay}ms），但 HTTPS 未通过，不判活"
                        );
                        (kind, combined.trim_matches('；').to_string())
                    }
                    None => (kind, text),
                },
            };

            let _ = delay;
            NodeVerdict::dead(job, reason, detail)
                .with_attempts(attempts as u32)
                .with_url(url)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A dialer that replays a script and records what it was asked for.
    struct ScriptedDialer {
        script: Mutex<Vec<DialResult>>,
        calls: Mutex<Vec<(String, u64)>>,
    }

    impl ScriptedDialer {
        fn new(script: Vec<DialResult>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<(String, u64)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Dialer for ScriptedDialer {
        fn dial<'a>(
            &'a self,
            _proxy: &'a str,
            url: &'a str,
            timeout_ms: u64,
            _expected: &'a str,
        ) -> BoxFuture<'a, DialResult> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((url.to_string(), timeout_ms));
                let mut script = self.script.lock().unwrap();
                if script.is_empty() {
                    // Exhausting the script is a test bug, not a node failure.
                    (None, Some("script_exhausted".into()), String::new())
                } else {
                    script.remove(0)
                }
            })
        }
    }

    fn ok(delay: u32) -> DialResult {
        (Some(delay), None, String::new())
    }

    fn fail(kind: &str, detail: &str) -> DialResult {
        (None, Some(kind.to_string()), detail.to_string())
    }

    fn cfg(targets: &[&str], attempts: usize) -> TestConfig {
        TestConfig {
            targets: targets.iter().map(|t| (*t).to_string()).collect(),
            expected_status: "204".into(),
            timeout_ms: 5_000,
            timeout_ms_retry: 9_000,
            concurrency: 4,
            max_attempts: attempts,
            // Zero pause: the policy is under test, not the clock.
            retry_pause_s: 0.0,
        }
    }

    fn job() -> Job {
        Job::new("air", "fp-1", "proxy-1", "direct", "1.2.3.4")
    }

    async fn run(dialer: Arc<ScriptedDialer>, config: &TestConfig) -> NodeVerdict {
        TestOne::with_dialer(dialer, config).test(job()).await
    }

    #[test]
    fn the_partition_puts_https_first_and_keeps_relative_order() {
        let targets: Vec<String> = ["http://a", "https://b", "http://c", "https://d"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            partition(&targets),
            vec!["https://b", "https://d", "http://a", "http://c"]
        );
    }

    #[test]
    fn https_is_required_exactly_when_the_partition_leads_with_it() {
        assert!(TestPolicy::from_config(&cfg(&["http://a", "https://b"], 3)).https_required);
        assert!(!TestPolicy::from_config(&cfg(&["http://a", "http://b"], 3)).https_required);
    }

    #[tokio::test]
    async fn a_first_attempt_success_costs_one_dial() {
        let dialer = ScriptedDialer::new(vec![ok(233)]);
        let verdict = run(dialer.clone(), &cfg(&["https://a"], 3)).await;
        assert!(verdict.is_alive());
        assert_eq!(verdict.attempts, 1);
        assert_eq!(dialer.calls().len(), 1);
    }

    #[tokio::test]
    async fn a_timeout_escalates_the_budget_for_the_retry() {
        let dialer = ScriptedDialer::new(vec![fail("timeout", "Timeout"), ok(400)]);
        let verdict = run(dialer.clone(), &cfg(&["https://a"], 3)).await;
        assert!(verdict.is_alive());
        assert_eq!(verdict.attempts, 2);
        let calls = dialer.calls();
        assert_eq!(calls[0].1, 5_000, "first dial uses the base budget");
        assert_eq!(calls[1].1, 9_000, "a timeout escalates to the retry budget");
    }

    #[tokio::test]
    async fn a_non_timeout_failure_does_not_escalate() {
        let dialer = ScriptedDialer::new(vec![fail("kernel_error", "no"), ok(1)]);
        run(dialer.clone(), &cfg(&["https://a"], 3)).await;
        assert_eq!(dialer.calls()[1].1, 5_000, "only a timeout escalates");
    }

    #[tokio::test]
    async fn a_terminal_reason_stops_the_loop_immediately() {
        // A kernel that says bad_delay will say it again; retrying only spends
        // the round's budget.
        let dialer = ScriptedDialer::new(vec![fail("bad_delay", "nope"), ok(1)]);
        let verdict = run(dialer.clone(), &cfg(&["https://a"], 5)).await;
        assert_eq!(verdict.attempts, 1);
        assert_eq!(verdict.reason(), Some("bad_delay"));
        assert_eq!(dialer.calls().len(), 1);
    }

    #[tokio::test]
    async fn a_retryable_failure_uses_the_whole_budget() {
        let dialer = ScriptedDialer::new(vec![
            fail("kernel_error", "a"),
            fail("kernel_error", "b"),
            fail("kernel_error", "c"),
            ok(1),
        ]);
        let verdict = run(dialer.clone(), &cfg(&["https://a"], 3)).await;
        assert!(!verdict.is_alive());
        assert_eq!(verdict.attempts, 3, "max_attempts caps the loop");
        assert_eq!(dialer.calls().len(), 3);
        assert_eq!(verdict.reason(), Some("kernel_error"));
    }

    #[tokio::test]
    async fn attempts_rotate_through_the_target_list() {
        let dialer = ScriptedDialer::new(vec![
            fail("kernel_error", "a"),
            fail("kernel_error", "b"),
            fail("kernel_error", "c"),
        ]);
        run(dialer.clone(), &cfg(&["https://one", "https://two"], 3)).await;
        let urls: Vec<String> = dialer.calls().into_iter().map(|(u, _)| u).collect();
        assert_eq!(urls, vec!["https://one", "https://two", "https://one"]);
    }

    #[tokio::test]
    async fn an_http_pass_does_not_count_when_https_is_required() {
        // The node answers over plain HTTP but fails HTTPS. Reporting it alive
        // would publish a node that breaks in real use.
        let dialer = ScriptedDialer::new(vec![fail("timeout", "Timeout")]);
        let verdict = run(dialer.clone(), &cfg(&["https://a"], 1)).await;
        assert!(!verdict.is_alive());
        assert_eq!(verdict.reason(), Some("timeout"));
    }

    #[tokio::test]
    async fn a_plain_http_pass_is_reported_in_the_failure_detail() {
        // The config lists an HTTP target and an HTTPS one; the HTTP dial
        // succeeds, the HTTPS one times out.
        let dialer = ScriptedDialer::new(vec![fail("timeout", "Timeout"), ok(120)]);
        let verdict = run(dialer.clone(), &cfg(&["http://plain", "https://secure"], 2)).await;
        assert!(!verdict.is_alive(), "only an HTTPS pass can mark it alive");
        assert_eq!(verdict.reason(), Some("timeout"));
        let detail = match &verdict.verdict {
            crate::round::Verdict::Dead { message, .. } => message.clone(),
            other => panic!("expected a failure, got {other:?}"),
        };
        assert!(
            detail.contains("plain-HTTP 探测点 http://plain 通（120ms）"),
            "the HTTP evidence must survive into the ledger: {detail}"
        );
        // HTTPS is tried first, so the HTTP dial happens on the second attempt.
        let urls: Vec<String> = dialer.calls().into_iter().map(|(u, _)| u).collect();
        assert_eq!(urls, vec!["https://secure", "http://plain"]);
    }

    #[tokio::test]
    async fn an_all_http_config_accepts_an_http_pass() {
        // Python's `https_required` is False here; treating it as "nothing to
        // test" would be a divergence on every round.
        let dialer = ScriptedDialer::new(vec![ok(88)]);
        let verdict = run(dialer.clone(), &cfg(&["http://plain"], 3)).await;
        assert!(verdict.is_alive());
        assert_eq!(verdict.attempts, 1);
    }

    #[tokio::test]
    async fn an_empty_target_list_is_a_verdict_not_a_panic() {
        let dialer = ScriptedDialer::new(vec![ok(1)]);
        let verdict = run(dialer.clone(), &cfg(&[], 3)).await;
        assert!(!verdict.is_alive());
        assert_eq!(verdict.reason(), Some("target_error"));
        assert!(dialer.calls().is_empty(), "nothing may be dialled");
    }

    #[tokio::test]
    async fn the_final_url_and_attempt_count_reach_the_ledger_row() {
        let dialer = ScriptedDialer::new(vec![fail("timeout", "Timeout"), ok(500)]);
        let verdict = run(dialer, &cfg(&["https://a"], 3)).await;
        assert_eq!(verdict.attempts, 2);
        assert_eq!(verdict.url.as_deref(), Some("https://a"));
        let row = verdict.to_result_row();
        assert_eq!(
            row.attempts,
            Some(2),
            "Python writes the real attempt count"
        );
        assert_eq!(row.verdict.as_deref(), Some("ok"));
        assert_eq!(row.delay_ms, Some(500));
    }
}
