"""Convergence policy: decide when a node is really dead.

A node is only demoted after failing several consecutive rounds. Retrying
inside one round cannot rescue a deterministic failure -- measurement on 47
nodes showed three rounds of a dead node returning the identical 503 twice
more -- so the expensive confirmation has to live across rounds, where it also
absorbs whole-round accidents (a dead test target, a VPS network blip) instead
of publishing them as mass death.
"""

ALIVE = "alive"
PENDING = "pending"
DEAD = "dead"
UNKNOWN = "unknown"
# A node whose entry sits on an ISP we cannot test through is not dead and not
# alive -- it is out of scope for this vantage point. Its streak must not
# advance, or a transient classification would eventually kill it.
EXCLUDED = "excluded"


def apply(node, ok, delay_ms, reason, policy):
    """Fold one round's outcome into a node record.

    Returns (fields_to_store, transition) where transition is one of
    "restore" (a DEAD node came back), "new" (a node passed for the first
    time), "drop", or None.
    """
    threshold = int(policy.get("drop_after_consecutive_fails", 3))
    streak = int(node.get("consec_fail") or 0)
    was = node.get("status") or UNKNOWN

    if ok:
        fields = {
            "status": ALIVE,
            "consec_fail": 0,
            "last_delay_ms": delay_ms,
            "last_reason": None,
            "last_ok": _stamp(node),
            "total_ok": int(node.get("total_ok") or 0) + 1,
        }
        # "restore" is reserved for a node the ledger had already judged DEAD.
        # That is the false-kill signal this counter is read for -- the only
        # transition that means a previous verdict was wrong. A node passing
        # for the first time (UNKNOWN) is a new arrival, not a recovery, and
        # upstream churn adds and removes dozens of nodes every round: counting
        # those as restores buried the signal, with 139 of the last 175
        # restores coming from two churn spikes alone. They are reported
        # separately as "new" so the churn stays visible.
        if was == DEAD:
            transition = "restore"
        elif was == UNKNOWN:
            transition = "new"
        else:
            transition = None
        return fields, transition

    streak += 1
    fields = {
        "consec_fail": streak,
        "last_reason": reason,
        "total_fail": int(node.get("total_fail") or 0) + 1,
    }
    if streak >= threshold:
        fields["status"] = DEAD
        return fields, "drop" if was != DEAD else None
    # Not yet confirmed dead: hold the previous verdict but surface the streak.
    fields["status"] = PENDING if was in (ALIVE, PENDING) else UNKNOWN
    return fields, None


def _stamp(node):
    import time

    # UTC, to match db.now(); a localtime stamp here would disagree with the
    # ledger columns it sits next to whenever TZ is not UTC.
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())


def round_is_suspect(alive_now, alive_prev, policy):
    """Return True when a round's result looks like an infrastructure failure.

    Publishing such a round would wipe every good node at once, which is
    exactly what the unguarded pipelines would do if the test target or the
    VPS network failed for one cycle.
    """
    if not alive_prev:
        return False
    ratio = float(policy.get("suspect_floor_ratio", 0.5))
    floor = int(policy.get("suspect_floor_absolute", 3))
    return alive_now < max(floor, alive_prev * ratio)

