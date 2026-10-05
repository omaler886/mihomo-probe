//! Convergence policy (workstreams/03, port of `mihomo_test/policy.py`).
//!
//! A node is only demoted after failing several consecutive rounds. Retrying
//! inside one round cannot rescue a deterministic failure -- measurement on 47
//! nodes showed three rounds of a dead node returning the identical 503 twice
//! more -- so the expensive confirmation has to live across rounds, where it
//! also absorbs whole-round accidents (a dead test target, a VPS network
//! blip) instead of publishing them as mass death.
//!
//! This module is pure: no clock, no database. The caller stamps [`now`] and
//! persists [`NodeUpdate`]; shadow diffing (workstreams/13) compares these
//! decisions against the Python implementation field by field.

/// Node statuses. `EXCLUDED` is not produced by [`apply`] -- it is written by
/// the entry-IP filter (R4/R6 收尾) explicitly, and its streak must not
/// advance: a transient classification must never kill a node.
pub const STATUS_ALIVE: &str = "alive";
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_DEAD: &str = "dead";
pub const STATUS_UNKNOWN: &str = "unknown";
pub const STATUS_EXCLUDED: &str = "excluded";

/// The thresholds one round folds a node under. Defaults mirror
/// `config.DEFAULTS["policy"]`; nothing here may be hard-coded at call sites
/// -- 0.5 / 3 are experience values, to be re-checked against real round
/// history before the Rust path takes traffic (workstreams/03).
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// Failures this many consecutive rounds before a node is judged dead.
    pub drop_after_consecutive_fails: i64,
    /// Round guard: alive_now below `alive_prev * ratio` is suspect.
    pub suspect_floor_ratio: f64,
    /// Round guard: never trust a floor smaller than this.
    pub suspect_floor_absolute: i64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            drop_after_consecutive_fails: 3,
            suspect_floor_ratio: 0.5,
            suspect_floor_absolute: 3,
        }
    }
}

/// The stored state a fold reads from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeSnapshot {
    /// `nodes.status`; empty/null reads as [`STATUS_UNKNOWN`].
    pub status: String,
    pub consec_fail: i64,
    pub total_ok: i64,
    pub total_fail: i64,
}

/// One round's verdict for one node, before folding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub ok: bool,
    pub delay_ms: Option<i64>,
    /// The failure class (`timeout`, `front_dead`, ...). `None` when alive.
    pub reason: Option<String>,
}

/// What [`apply`] decided to persist. Every field maps to a `nodes` column;
/// `last_seen` is stamped by the persistence layer, not here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeUpdate {
    pub status: Option<String>,
    pub consec_fail: Option<i64>,
    pub last_delay_ms: Option<Option<i64>>,
    pub last_reason: Option<Option<String>>,
    /// The caller stamps this with [`now`]; `Some` only on a passing fold.
    pub last_ok: Option<String>,
    pub total_ok: Option<i64>,
    pub total_fail: Option<i64>,
}

/// Why this fold matters. `Restore` is reserved for a node the ledger had
/// already judged DEAD -- that is the false-kill signal this counter is read
/// for, the only transition that means a previous verdict was wrong. A node
/// passing for the first time (UNKNOWN) is a new arrival, not a recovery:
/// upstream churn adds and removes dozens of nodes every round, and counting
/// those as restores buried the signal (139 of the last 175 restores came
/// from two churn spikes alone). They are reported separately as [`Transition::New`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// A DEAD node came back.
    Restore,
    /// A node passed for the first time.
    New,
    /// A node crossed the streak threshold and is now DEAD.
    Drop,
    /// No noteworthy transition.
    None,
}

/// One round's outcome folded into a node record.
///
/// Port of `policy.apply`, byte-for-byte in semantics:
///
/// * a pass is immediate: status → alive, streak → 0, delay recorded;
/// * a fail advances the streak; at the threshold the node is judged dead;
/// * below the threshold the previous verdict is held but surfaced --
///   alive/pending read as pending, unknown stays unknown. A demotion to
///   pending is not a verdict change; only the streak crossing is.
pub fn apply(
    node: &NodeSnapshot,
    observation: &Observation,
    policy: &Policy,
    now: &str,
) -> (NodeUpdate, Transition) {
    let was = if node.status.is_empty() {
        STATUS_UNKNOWN
    } else {
        node.status.as_str()
    };

    if observation.ok {
        let update = NodeUpdate {
            status: Some(STATUS_ALIVE.into()),
            consec_fail: Some(0),
            last_delay_ms: Some(observation.delay_ms),
            last_reason: Some(None),
            last_ok: Some(now.to_string()),
            total_ok: Some(node.total_ok + 1),
            total_fail: None,
        };
        let transition = match was {
            STATUS_DEAD => Transition::Restore,
            STATUS_UNKNOWN => Transition::New,
            _ => Transition::None,
        };
        return (update, transition);
    }

    let streak = node.consec_fail + 1;
    let mut update = NodeUpdate {
        status: None,
        consec_fail: Some(streak),
        last_delay_ms: None,
        last_reason: Some(observation.reason.clone()),
        last_ok: None,
        total_ok: None,
        total_fail: Some(node.total_fail + 1),
    };
    if streak >= policy.drop_after_consecutive_fails {
        update.status = Some(STATUS_DEAD.into());
        let transition = if was != STATUS_DEAD {
            Transition::Drop
        } else {
            Transition::None
        };
        return (update, transition);
    }
    // Not yet confirmed dead: hold the previous verdict but surface the streak.
    update.status = Some(
        if was == STATUS_ALIVE || was == STATUS_PENDING {
            STATUS_PENDING
        } else {
            STATUS_UNKNOWN
        }
        .into(),
    );
    (update, Transition::None)
}

/// Whether a round's result looks like an infrastructure failure (port of
/// `policy.round_is_suspect`). Publishing such a round would wipe every good
/// node at once -- exactly what the unguarded pipelines would do if the test
/// target or the VPS network failed for one cycle.
///
/// With no previous round (`alive_prev == 0`) there is nothing to compare
/// against: a first round is never suspect, whatever it measured.
pub fn round_is_suspect(alive_now: i64, alive_prev: i64, policy: &Policy) -> bool {
    if alive_prev <= 0 {
        return false;
    }
    let floor = (alive_prev as f64 * policy.suspect_floor_ratio) as i64;
    alive_now < floor.max(policy.suspect_floor_absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(status: &str, consec_fail: i64) -> NodeSnapshot {
        NodeSnapshot {
            status: status.into(),
            consec_fail,
            total_ok: 0,
            total_fail: 0,
        }
    }

    fn pass(delay: i64) -> Observation {
        Observation {
            ok: true,
            delay_ms: Some(delay),
            reason: None,
        }
    }

    fn fail(reason: &str) -> Observation {
        Observation {
            ok: false,
            delay_ms: None,
            reason: Some(reason.into()),
        }
    }

    const P: Policy = Policy {
        drop_after_consecutive_fails: 3,
        suspect_floor_ratio: 0.5,
        suspect_floor_absolute: 3,
    };

    #[test]
    fn a_pass_is_immediate_and_resets_the_streak() {
        let (update, transition) = apply(&node("pending", 2), &pass(120), &P, "T");
        assert_eq!(update.status.as_deref(), Some(STATUS_ALIVE));
        assert_eq!(update.consec_fail, Some(0));
        assert_eq!(update.last_delay_ms, Some(Some(120)));
        assert_eq!(update.last_reason, Some(None));
        assert_eq!(update.last_ok.as_deref(), Some("T"));
        assert_eq!(update.total_ok, Some(1));
        assert_eq!(
            transition,
            Transition::None,
            "pending→alive is not noteworthy"
        );
    }

    #[test]
    fn restore_is_reserved_for_a_dead_node_coming_back() {
        let (update, transition) = apply(&node(STATUS_DEAD, 3), &pass(50), &P, "T");
        assert_eq!(update.status.as_deref(), Some(STATUS_ALIVE));
        assert_eq!(transition, Transition::Restore);
    }

    #[test]
    fn first_pass_from_unknown_is_new_not_restore() {
        let (_, transition) = apply(&NodeSnapshot::default(), &pass(50), &P, "T");
        assert_eq!(transition, Transition::New);
    }

    #[test]
    fn the_streak_must_reach_the_threshold_before_a_drop() {
        let (first, _) = apply(&node(STATUS_ALIVE, 0), &fail("timeout"), &P, "T");
        assert_eq!(first.status.as_deref(), Some(STATUS_PENDING));
        assert_eq!(first.consec_fail, Some(1));

        let (second, _) = apply(&node(STATUS_ALIVE, 1), &fail("timeout"), &P, "T");
        assert_eq!(second.status.as_deref(), Some(STATUS_PENDING));
        assert_eq!(second.consec_fail, Some(2));

        let (third, transition) = apply(&node(STATUS_ALIVE, 2), &fail("timeout"), &P, "T");
        assert_eq!(third.status.as_deref(), Some(STATUS_DEAD));
        assert_eq!(transition, Transition::Drop);
    }

    #[test]
    fn unknown_stays_unknown_below_the_threshold() {
        let (update, transition) = apply(&node(STATUS_UNKNOWN, 0), &fail("timeout"), &P, "T");
        assert_eq!(update.status.as_deref(), Some(STATUS_UNKNOWN));
        assert_eq!(update.consec_fail, Some(1));
        assert_eq!(transition, Transition::None);
    }

    #[test]
    fn a_dead_node_staying_dead_does_not_re_drop() {
        let (update, transition) = apply(&node(STATUS_DEAD, 3), &fail("timeout"), &P, "T");
        assert_eq!(update.status.as_deref(), Some(STATUS_DEAD));
        assert_eq!(update.consec_fail, Some(4));
        assert_eq!(transition, Transition::None);
    }

    #[test]
    fn a_fail_records_its_reason_but_not_the_delay() {
        let (update, _) = apply(&node(STATUS_ALIVE, 0), &fail("tls_error"), &P, "T");
        assert_eq!(update.last_reason, Some(Some("tls_error".into())));
        assert_eq!(update.last_delay_ms, None, "the previous delay is kept");
        assert_eq!(update.total_fail, Some(1));
    }

    #[test]
    fn the_suspect_floor_is_max_of_ratio_and_absolute() {
        // 100 alive last round: 0.5 ratio wins over the absolute floor.
        assert!(round_is_suspect(49, 100, &P));
        assert!(!round_is_suspect(50, 100, &P));
        // 4 alive last round: ratio gives 2, the absolute floor of 3 wins.
        assert!(round_is_suspect(2, 4, &P));
        assert!(!round_is_suspect(3, 4, &P));
        // Strictly less-than: exactly at the floor is fine.
        assert!(!round_is_suspect(6, 12, &P));
    }

    #[test]
    fn a_first_round_is_never_suspect() {
        assert!(!round_is_suspect(0, 0, &P));
        assert!(!round_is_suspect(1, 0, &P));
    }

    #[test]
    fn the_pass_stamp_travels_from_the_caller() {
        // No clock here: the caller stamps `last_ok` with the ledger's UTC
        // format, and this module only carries it through.
        let (update, _) = apply(&node(STATUS_ALIVE, 0), &pass(1), &P, "2026-10-05T08:00:00");
        assert_eq!(update.last_ok.as_deref(), Some("2026-10-05T08:00:00"));
    }
}
