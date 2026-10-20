//! G03 preflight: produce the config the kernel will actually accept.
//!
//! Python `core.make_testable` (mihomo_test/core.py:418): write the config,
//! ask the kernel itself to validate it (`mihomo -t`), and if the kernel names
//! a culprit, drop that node and try again. One malformed upstream entry used
//! to fail the whole round -- every source, every node -- which is what this
//! exists to prevent.
//!
//! Three things are deliberate here:
//!
//! * **Validation is injected** ([`KernelValidator`]). The round needs a real
//!   kernel; the tests need a script, and the real implementation
//!   ([`MihomoValidator`]) is one line over `probe_mihomo::config_check`.
//! * **No verdict is not a failure.** `Degraded` -- no docker, no MIHOMO_BIN,
//!   a validator that timed out -- skips the check and lets the round run,
//!   exactly like Python's "docker unavailable; config validation skipped". The
//!   kernel still refuses a bad config at load time, just less precisely.
//! * **Removal is by ledger identity, never by name.** `prepare` renames
//!   duplicate entries (`BageVM #2`) while the kernel reports the renamed
//!   name, so a name match removed nothing, burned every attempt and lost the
//!   round -- the exact failure `make_testable` was written to prevent
//!   (core.py:428-434).

use std::path::Path;

use probe_config::CoreConfig;
use probe_mihomo::config_check::{culprit_from, ConfigCheck, ProxyRef};
use serde_json::Value;

/// Kernel validations per call: the initial attempt plus one retry per
/// pruning. Python `core.py:441` is `for attempt in range(max_prune + 1)` with
/// `max_prune=5`, which is the same budget -- one attempt more or less is a
/// difference a shadow comparison would see.
pub const MAX_PRUNE: usize = 5;

/// How much of a kernel message may be kept: in a log line or in a verdict.
/// Python keeps `output.splitlines()[-1][:200]` in its log and the last 400
/// characters in its exception; one bound covers both.
const KERNEL_TEXT_LIMIT: usize = 200;

/// The kernel check, injected: a round needs the real `mihomo -t`, a test
/// needs a script, and neither should know about the other.
pub trait KernelValidator {
    fn validate(&self, core_dir: &Path) -> ConfigCheck;
}

/// The real validator: the kernel's own `-t` against `<core_dir>/config.yaml`
/// (a native binary through `MIHOMO_BIN`, otherwise the pinned image), with
/// the verdict rules `probe_mihomo::config_check` already owns.
pub struct MihomoValidator;

impl KernelValidator for MihomoValidator {
    fn validate(&self, core_dir: &Path) -> ConfigCheck {
        probe_mihomo::config_check::validate(core_dir)
    }
}

/// What preflight decided about the config it hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightVerdict {
    /// The kernel accepted the config on the first attempt.
    Accepted,
    /// The kernel refused the config; these nodes were removed and the
    /// rewritten one was accepted.
    ///
    /// `dropped` holds **ledger identities**, not names: each entry is a
    /// `probe_source::fingerprint_proxy` value, the same namespace as
    /// `Job::fingerprint` and `nodes.fingerprint`, and appears exactly once
    /// however many kernel proxies shared it (a renamed duplicate included).
    /// Callers filter their jobs by it: a node left in the test plan while its
    /// proxy is gone from the config is dialled through a kernel that no
    /// longer knows the name, and the round reports a failure it never earned.
    /// The kernel-facing names -- what an operator recognises -- are in `log`.
    Pruned { dropped: Vec<String> },
    /// No verdict was available. The round runs unchecked, like Python's
    /// skipped validation -- this is not a failure.
    Skipped { reason: String },
    /// The kernel keeps rejecting the config: the caller blocks the round.
    /// `detail` is the bounded, secret-free tail of the last kernel message.
    Rejected { detail: String },
}

/// Write the config, validate it, drop the culprit and retry -- Python
/// `core.make_testable` (mihomo_test/core.py:418).
///
/// The returned proxies are the ones in `<core_dir>/config.yaml` when this
/// returns: the caller never writes the config again. `Accepted`, `Pruned` and
/// `Skipped` all mean that file is what the kernel saw; `Rejected` means the
/// caller must block the round.
pub fn make_testable(
    core_dir: &Path,
    core: &CoreConfig,
    secret: &str,
    proxies: Vec<Value>,
    validator: &dyn KernelValidator,
    log: &mut Vec<String>,
) -> (Vec<Value>, PreflightVerdict) {
    let mut working = proxies;
    let mut dropped: Vec<String> = Vec::new();
    let mut last_detail = String::new();

    for attempt in 1..=MAX_PRUNE + 1 {
        if let Err(error) = probe_mihomo::write_config(core_dir, core, secret, &working) {
            // Without a config there is nothing to validate, and the caller
            // cannot fix that: blocking the round is the only honest verdict.
            return (
                working,
                PreflightVerdict::Rejected {
                    detail: bounded_kernel_text(&error.to_string(), secret),
                },
            );
        }
        match validator.validate(core_dir) {
            // The kernel's own output stays in memory: it can quote the config.
            ConfigCheck::Passed(_) => return (working, verdict_after_pruning(dropped)),
            ConfigCheck::Degraded(reason) => {
                let reason = bounded_kernel_text(&reason, secret);
                log.push(format!("config validation skipped: {reason}"));
                return (working, PreflightVerdict::Skipped { reason });
            }
            ConfigCheck::Failed(output) => {
                last_detail = bounded_kernel_text(&output, secret);
                log.push(format!(
                    "config test failed (attempt {attempt}): {last_detail}"
                ));
                let Some(index) = culprit_from(&output, &proxy_refs(&working)) else {
                    // Nothing in the list is to blame, so retrying would send
                    // the same config again: Python raises here instead of
                    // burning the remaining attempts.
                    return (
                        working,
                        PreflightVerdict::Rejected {
                            detail: last_detail,
                        },
                    );
                };
                let identity = probe_source::fingerprint_proxy(&working[index]);
                let names: Vec<String> = working
                    .iter()
                    .filter(|proxy| probe_source::fingerprint_proxy(proxy) == identity)
                    .map(|proxy| proxy_label(proxy, &identity))
                    .collect();
                if names.len() == working.len() || attempt == MAX_PRUNE + 1 {
                    // Either every remaining node is the culprit (Python
                    // breaks out of its loop here) or the budget is spent.
                    // Both return the config the kernel just refused, so the
                    // list still matches the file on disk.
                    log.push(format!(
                        "no further config to try after {attempt} attempt(s); the round is blocked"
                    ));
                    return (
                        working,
                        PreflightVerdict::Rejected {
                            detail: last_detail,
                        },
                    );
                }
                working.retain(|proxy| probe_source::fingerprint_proxy(proxy) != identity);
                // Names for the operator, the identity for the caller's job
                // filter -- one entry per identity, however many aliases it
                // carried.
                log.push(format!(
                    "pruned {identity} ({}) after kernel rejection",
                    names.join(", ")
                ));
                if !dropped.contains(&identity) {
                    dropped.push(identity);
                }
            }
        }
    }
    // Every iteration returns above; this keeps the function total rather than
    // making a future edit turn a blocked round into a panic.
    (
        working,
        PreflightVerdict::Rejected {
            detail: last_detail,
        },
    )
}

/// An accepted config is `Accepted` only when nothing had to be removed.
fn verdict_after_pruning(dropped: Vec<String>) -> PreflightVerdict {
    if dropped.is_empty() {
        PreflightVerdict::Accepted
    } else {
        PreflightVerdict::Pruned { dropped }
    }
}

/// The kernel-facing identity of every proxy, for `culprit_from`.
///
/// A written config has unique names (`prepare` renames duplicates), which is
/// what makes the quoted name in the kernel's text an index into this list.
fn proxy_refs(proxies: &[Value]) -> Vec<ProxyRef> {
    proxies
        .iter()
        .map(|proxy| ProxyRef {
            name: proxy
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            server: proxy
                .get("server")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect()
}

/// How a pruned node is reported: the name the kernel saw, or the ledger
/// identity when the entry has no usable name.
fn proxy_label(proxy: &Value, identity: &str) -> String {
    proxy
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(identity)
        .to_string()
}

/// The tail of a kernel message: bounded, and with the shared secret masked.
///
/// Kernel output can quote the config it refused. `probe_mihomo::config_check`
/// is explicit that the text is sensitive, and this text reaches a log line
/// and a round verdict, so it is bounded first and redacted second -- on
/// character boundaries, because node names here are routinely CJK.
fn bounded_kernel_text(text: &str, secret: &str) -> String {
    let trimmed = text.trim();
    let redacted = if secret.is_empty() {
        trimmed.to_string()
    } else {
        trimmed.replace(secret, "[redacted]")
    };
    let characters: Vec<char> = redacted.chars().collect();
    if characters.len() <= KERNEL_TEXT_LIMIT {
        return redacted;
    }
    let tail: String = characters[characters.len() - (KERNEL_TEXT_LIMIT - 1)..]
        .iter()
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Own a unique temporary directory; remove only this test's own tree.
    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "probe-preflight-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let base = std::env::temp_dir().canonicalize().unwrap();
            assert!(self.0.starts_with(&base) && self.0 != base);
            assert!(self
                .0
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("probe-preflight-test-"));
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    /// What the fake kernel answers, and what it saw.
    #[derive(Clone)]
    enum Reply {
        Passed,
        Degraded(String),
        /// Reject, blaming the first proxy of the config the kernel just read.
        BlameFirst,
        /// Reject with this literal kernel text.
        Reject(String),
    }

    struct FakeValidator {
        replies: Mutex<VecDeque<Reply>>,
        calls: AtomicUsize,
        seen: Mutex<Vec<String>>,
    }

    impl FakeValidator {
        fn new(replies: Vec<Reply>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// The config text the kernel was handed, once per call.
        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl KernelValidator for FakeValidator {
        fn validate(&self, core_dir: &Path) -> ConfigCheck {
            let config = std::fs::read_to_string(core_dir.join("config.yaml")).unwrap_or_default();
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(config.clone());
            match self.replies.lock().unwrap().pop_front() {
                Some(Reply::Passed) | None => ConfigCheck::Passed("accepted".into()),
                Some(Reply::Degraded(reason)) => ConfigCheck::Degraded(reason),
                Some(Reply::Reject(detail)) => ConfigCheck::Failed(detail),
                Some(Reply::BlameFirst) => ConfigCheck::Failed(format!(
                    "proxy [\"{}\"] failed to load",
                    first_proxy_name(&config)
                )),
            }
        }
    }

    /// The first proxy of a written config: the `proxies:` section holds one
    /// inline JSON object per line.
    fn first_proxy_name(config: &str) -> String {
        config
            .lines()
            .filter_map(|line| line.strip_prefix("  - "))
            .filter_map(|json| serde_json::from_str::<Value>(json).ok())
            .find_map(|proxy| {
                proxy
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .expect("the written config lists proxies")
    }

    fn ss(name: &str, server: &str) -> Value {
        json!({"name": name, "type": "ss", "server": server, "port": 8388,
               "cipher": "aes-128-gcm", "password": "pw"})
    }

    fn names(proxies: &[Value]) -> Vec<String> {
        proxies
            .iter()
            .map(|proxy| {
                proxy
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    fn core() -> CoreConfig {
        CoreConfig::defaults()
    }

    /// The returned list is the file, not a copy of it.
    fn assert_file_matches(root: &TestRoot, secret: &str, returned: &[Value]) {
        let written = std::fs::read_to_string(root.0.join("config.yaml")).unwrap();
        assert_eq!(
            written,
            probe_mihomo::build_config(&core(), secret, returned)
        );
    }

    #[test]
    fn an_accepted_config_is_written_unchanged_and_returned() {
        let root = TestRoot::new();
        let validator = FakeValidator::new(vec![Reply::Passed]);
        let proxies = vec![ss("a", "1.1.1.1"), ss("b", "2.2.2.2")];
        let mut log = Vec::new();
        let (returned, verdict) = make_testable(
            &root.0,
            &core(),
            "s3cr3t",
            proxies.clone(),
            &validator,
            &mut log,
        );
        assert_eq!(verdict, PreflightVerdict::Accepted);
        assert_eq!(returned, proxies);
        assert_eq!(validator.calls(), 1, "a pass needs no retry");
        assert_file_matches(&root, "s3cr3t", &returned);
        assert!(log.is_empty(), "{log:?}");
    }

    #[test]
    fn the_culprit_is_pruned_and_the_config_is_rewritten() {
        let root = TestRoot::new();
        let validator = FakeValidator::new(vec![
            Reply::Reject("proxy [\"bad\"] failed to load".into()),
            Reply::Passed,
        ]);
        let proxies = vec![
            ss("keep1", "1.1.1.1"),
            ss("bad", "9.9.9.9"),
            ss("keep2", "2.2.2.2"),
        ];
        // Callers filter jobs against this, so it has to be the identity.
        let bad_identity = probe_source::fingerprint_proxy(&proxies[1]);
        let mut log = Vec::new();
        let (returned, verdict) =
            make_testable(&root.0, &core(), "s3cr3t", proxies, &validator, &mut log);
        assert_eq!(
            verdict,
            PreflightVerdict::Pruned {
                dropped: vec![bad_identity.clone()]
            }
        );
        assert_eq!(
            bad_identity.len(),
            16,
            "a ledger identity, not a name: {bad_identity}"
        );
        assert_eq!(names(&returned), ["keep1", "keep2"]);
        assert_eq!(validator.calls(), 2);
        // The kernel really was handed a rewritten file: attempt 1 carried the
        // bad node, attempt 2 did not.
        let seen = validator.seen();
        assert!(seen[0].contains("\"bad\""), "{}", seen[0]);
        assert!(!seen[1].contains("\"bad\""), "{}", seen[1]);
        assert!(seen[1].contains("\"keep2\""));
        assert_file_matches(&root, "s3cr3t", &returned);
        assert!(log.iter().any(|line| line.contains("attempt 1")), "{log:?}");
        // The operator still gets the kernel-facing name next to the identity.
        assert!(
            log.iter()
                .any(|line| line.contains("bad") && line.contains(&bad_identity)),
            "{log:?}"
        );
    }

    /// The Python lesson (core.py:428-434): `prepare` renames duplicates, so a
    /// name match removes nothing and burns every attempt. The identity ignores
    /// the name, so the renamed twin goes with the culprit.
    #[test]
    fn a_renamed_duplicate_is_pruned_with_the_culprit() {
        let root = TestRoot::new();
        let validator = FakeValidator::new(vec![
            Reply::Reject("proxy [\"twin #2\"] failed to load".into()),
            Reply::Passed,
        ]);
        // Same connection parameters, different names: one ledger identity.
        let proxies = vec![
            ss("twin", "9.9.9.9"),
            ss("twin #2", "9.9.9.9"),
            ss("other", "2.2.2.2"),
        ];
        let identity = probe_source::fingerprint_proxy(&proxies[0]);
        assert_eq!(identity, probe_source::fingerprint_proxy(&proxies[1]));
        let mut log = Vec::new();
        let (returned, verdict) =
            make_testable(&root.0, &core(), "s3cr3t", proxies, &validator, &mut log);
        assert_eq!(
            verdict,
            PreflightVerdict::Pruned {
                dropped: vec![identity.clone()]
            },
            "one identity, not one entry per alias"
        );
        assert_eq!(names(&returned), ["other"]);
        assert_eq!(validator.calls(), 2, "one prune, not five burnt attempts");
        // Both kernel-facing names stay in the log for whoever reads it.
        let logged = log.join("\n");
        assert!(
            logged.contains("twin") && logged.contains("twin #2"),
            "{log:?}"
        );
    }

    #[test]
    fn a_degraded_kernel_skips_the_check_without_failing_the_round() {
        let root = TestRoot::new();
        let reason = "no available native or Docker validator; no verdict";
        let validator = FakeValidator::new(vec![Reply::Degraded(reason.into())]);
        let proxies = vec![ss("a", "1.1.1.1")];
        let mut log = Vec::new();
        let (returned, verdict) = make_testable(
            &root.0,
            &core(),
            "s3cr3t",
            proxies.clone(),
            &validator,
            &mut log,
        );
        assert_eq!(
            verdict,
            PreflightVerdict::Skipped {
                reason: reason.into()
            }
        );
        assert_eq!(returned, proxies, "a skipped check changes nothing");
        assert_eq!(validator.calls(), 1);
        assert!(log.iter().any(|line| line.contains(reason)), "{log:?}");
        assert!(root.0.join("config.yaml").is_file());
        assert_file_matches(&root, "s3cr3t", &returned);
    }

    #[test]
    fn a_kernel_that_never_accepts_costs_the_python_budget_then_blocks() {
        let root = TestRoot::new();
        // Seven nodes, every attempt blaming the first one still in the config.
        let proxies: Vec<Value> = (0..7)
            .map(|index| ss(&format!("n{index}"), &format!("10.0.0.{index}")))
            .collect();
        let validator = FakeValidator::new(vec![Reply::BlameFirst; MAX_PRUNE + 1]);
        let mut log = Vec::new();
        let (returned, verdict) =
            make_testable(&root.0, &core(), "s3cr3t", proxies, &validator, &mut log);
        let PreflightVerdict::Rejected { detail } = verdict else {
            panic!("{verdict:?}")
        };
        assert!(
            detail.contains("n5"),
            "the last kernel text is kept: {detail}"
        );
        // core.py:441 is `range(max_prune + 1)`: the first attempt plus one
        // retry per pruning, so five prunes and six kernel runs.
        assert_eq!(validator.calls(), MAX_PRUNE + 1);
        assert_eq!(
            names(&returned),
            ["n5", "n6"],
            "nothing is pruned after the last attempt"
        );
        assert_file_matches(&root, "s3cr3t", &returned);
    }

    #[test]
    fn an_unattributable_rejection_blocks_immediately() {
        let root = TestRoot::new();
        // The kernel's text names nothing in the config: retrying would send
        // the very same config again.
        let validator =
            FakeValidator::new(vec![Reply::Reject("panic: cannot parse config".into())]);
        let mut log = Vec::new();
        let (returned, verdict) = make_testable(
            &root.0,
            &core(),
            "s3cr3t",
            vec![ss("a", "1.1.1.1")],
            &validator,
            &mut log,
        );
        assert!(matches!(verdict, PreflightVerdict::Rejected { .. }));
        assert_eq!(validator.calls(), 1, "no retry without a culprit");
        assert_eq!(names(&returned), ["a"]);
    }

    #[test]
    fn the_last_node_being_the_culprit_blocks_without_an_extra_attempt() {
        let root = TestRoot::new();
        let validator = FakeValidator::new(vec![Reply::BlameFirst, Reply::Passed]);
        let mut log = Vec::new();
        let (returned, verdict) = make_testable(
            &root.0,
            &core(),
            "s3cr3t",
            vec![ss("only", "1.1.1.1")],
            &validator,
            &mut log,
        );
        // Python breaks out of its loop here and raises; the config on disk is
        // still the one the kernel refused, and that is what comes back.
        assert!(matches!(verdict, PreflightVerdict::Rejected { .. }));
        assert_eq!(validator.calls(), 1);
        assert_eq!(names(&returned), ["only"]);
        assert_file_matches(&root, "s3cr3t", &returned);
    }

    #[test]
    fn the_kept_kernel_text_is_bounded_and_carries_no_secret() {
        let root = TestRoot::new();
        let secret = "s3cr3t-token";
        // The secret sits at the very end, where the kept tail is taken from.
        let noisy = format!("{}\nsecret: \"{secret}\"", "E".repeat(10_000));
        let validator = FakeValidator::new(vec![Reply::Reject(noisy)]);
        let mut log = Vec::new();
        let (_, verdict) = make_testable(
            &root.0,
            &core(),
            secret,
            vec![ss("a", "1.1.1.1")],
            &validator,
            &mut log,
        );
        let PreflightVerdict::Rejected { detail } = verdict else {
            panic!("{verdict:?}")
        };
        assert!(
            detail.chars().count() <= KERNEL_TEXT_LIMIT,
            "{}",
            detail.chars().count()
        );
        assert!(!detail.contains(secret), "{detail}");
        assert!(detail.contains("[redacted]"), "{detail}");
        for line in &log {
            assert!(!line.contains(secret), "{line}");
            assert!(line.chars().count() <= KERNEL_TEXT_LIMIT + 64, "{line}");
        }
    }
}
