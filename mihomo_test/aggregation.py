"""多探测点观察聚合（HK3 轻量化改造 Phase 3，指导 §4.3 / §7.7）。

Controller 每轮从若干探测点收集同一批节点的观测，按 fingerprint 聚合成**一条**
结论，再一次性交给原状态机（`policy.apply`）。聚合层是纯函数：不读数据库、
不触网、不读时钟——所有输入由调用方（`engine`）准备好，输出可直接断言。

三条不可协商的语义（指导 §4.3）：

* 任一已完成探测点 alive → 该节点本轮 alive（`any_alive`）。
* 所有已完成探测点均失败、且完成数达到 `minimum_completed_probes` → 失败。
* 完成数不足 `minimum_completed_probes` → `inconclusive`，不得推进 consec_fail。

探测点自身的问题——离线、未知 probe_id、digest 不符、超时未回传——表现为
「观测缺失」，因此天然不计入完成数，更不计入节点失败。这条是防止
「探测点越多，节点死得越快」的关键门禁（指导 §4.3 末段）。

观测行形状与 `db.get_observations()` 返回的行一致（见指导 §4.2），本地测量
经 `local_observation()` 包装成 `probe_id="local"` 的同形状观测，因此本地
单探测点兼容模式与远程多探测点走的是同一条聚合路径。
"""
from __future__ import annotations

# The Controller's own measurement. It is a probe like any other -- it either
# answers or it does not -- and giving it a reserved id keeps it out of the
# `probes` registry while still counting towards `minimum_completed_probes`.
LOCAL_PROBE_ID = "local"

STRATEGY_ANY_ALIVE = "any_alive"

VERDICT_ALIVE = "alive"
VERDICT_FAIL = "fail"
VERDICT_INCONCLUSIVE = "inconclusive"

# Recorded on a results row whose verdict is `inconclusive`. Distinct from a
# node reason on purpose: nothing about the node is known this round.
INSUFFICIENT_PROBES = "insufficient_probes"

# An observation whose `alive` is NULL: the probe reported, but reported no
# verdict. That is the probe's own uncertainty, not the node's failure, so it
# never counts as a completed probe (指导 §7.7).
_PROBE_NO_VERDICT = "probe_no_verdict"

_FAR = float("inf")


# --------------------------------------------------------------- 观测行规整

def _clean(value):
    if value is None:
        return None
    text = str(value).strip()
    return text or None


def _as_bool(value):
    """NULL-safe bool: None stays None, everything else is a verdict."""
    if value is None:
        return None
    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        return bool(value)
    if isinstance(value, str):
        return value.strip().lower() in ("1", "true", "yes", "y", "alive", "ok")
    return bool(value)


def _as_int(value):
    if value is None or isinstance(value, bool):
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def normalize_observation(row, round_id=None):
    """Coerce one stored/回传 observation into the canonical dict shape.

    Both `db.get_observations()` rows (`alive` as 0/1/NULL) and a fresh
    `local_observation()` land here, so nothing downstream has to care which
    side of the wire a row came from.
    """
    if not isinstance(row, dict):
        raise TypeError(f"observation must be a dict, got {type(row).__name__}")
    return {
        "round_id": row.get("round_id") if row.get("round_id") is not None else round_id,
        "probe_id": _clean(row.get("probe_id")) or "",
        "fingerprint": _clean(row.get("fingerprint")) or "",
        "alive": _as_bool(row.get("alive")),
        "delay_ms": _as_int(row.get("delay_ms")),
        "reason": _clean(row.get("reason")),
        "egress_country": _clean(row.get("egress_country")),
        "payload_digest": _clean(row.get("payload_digest")),
    }


def local_observation(round_id, fingerprint, alive, delay_ms=None, reason=None,
                      egress_country=None, payload_digest=""):
    """Wrap the Controller's own measurement as one observation.

    Returns the same shape as a remote `ProbeResult` row (指导 §4.2), so the
    aggregation path is identical in 本地单探测点兼容模式 and in multi-probe
    mode -- which is what makes 灰度第 1 步（指导 §16）a no-op on the node
    verdicts.
    """
    return {
        "round_id": round_id,
        "probe_id": LOCAL_PROBE_ID,
        "fingerprint": fingerprint,
        "alive": alive,
        "delay_ms": delay_ms,
        "reason": reason,
        "egress_country": egress_country,
        "payload_digest": payload_digest or "",
    }


def filter_observations(observations, round_id=None, known_probes=None,
                        config_digest=None, digest_gate=None):
    """Drop observations that must never reach the ledger (指导 §4.2).

    Returns (accepted, rejected, deferred). A rejected row keeps its parsed form
    plus a `why` so the caller can audit it as an event instead of silently
    dropping it -- 旧轮次、未知探测点、digest 不一致 only ever produce an audit
    line.

    A row with no `payload_digest` is *not* rejected on digest grounds: the
    digest check exists to catch a probe answering for a config it never saw,
    and an empty digest is a caller that never had one to compare.

    `digest_gate` names the layer that owns the digest check when this process
    is not it. `probe_dispatch.verify_result` compares the ProbeResult's own
    `config_digest` field against the Controller's -- which is 指导 §4.2's
    requirement, enforced at the only place the payload exists -- and it stores
    a *payload* digest in `probe_observations.payload_digest`: a hash of the
    whole result, which the aggregation layer cannot recompute and so cannot
    compare. Naming that gate here keeps the engine's check where it is
    meaningful (本地单探测点兼容模式, where this process is the only gate) and
    defers where it is not, instead of refusing every remote observation over a
    column-convention mismatch. Deferred rows come back in the third slot so
    the caller still audits them.
    """
    accepted, rejected, deferred = [], [], []
    for raw in observations or []:
        row = normalize_observation(raw, round_id=round_id)
        why = None
        defer = False
        if round_id is not None and row["round_id"] is not None \
                and str(row["round_id"]) != str(round_id):
            why = "stale_round"
        elif not row["probe_id"] or not row["fingerprint"]:
            why = "malformed"
        elif known_probes is not None and row["probe_id"] not in known_probes:
            why = "unknown_probe"
        elif config_digest and row["payload_digest"] \
                and row["payload_digest"] != config_digest:
            if digest_gate:
                # Used, not dropped: the row is valid, only its digest column
                # means something this layer cannot recompute.
                defer = True
            else:
                why = "digest_mismatch"
        if defer:
            deferred.append({"observation": row, "why": "digest_deferred",
                             "gate": digest_gate})
        if why is None:
            accepted.append(row)
        else:
            rejected.append({"observation": row, "why": why})
    return accepted, rejected, deferred


# ------------------------------------------------------------------- 聚合

class RoundAggregate(dict):
    """`{fingerprint: verdict}` plus the round-level summary as `.summary`.

    A dict rather than a small class so callers can index it exactly like the
    mapping the guide describes (`指导 §4.3`), while the round-level roll-up --
    `completed_probes` above all -- stays out of the node keyspace and cannot
    collide with a fingerprint.
    """

    def __init__(self, nodes, summary):
        super().__init__(nodes)
        self.summary = summary


def summary_of(aggregated):
    """The round-level roll-up of a `RoundAggregate` (or a bare dict)."""
    if isinstance(aggregated, RoundAggregate):
        return aggregated.summary
    return aggregated.get("__summary__", {}) if isinstance(aggregated, dict) else {}


def node_verdicts(aggregated):
    """Just the `{fingerprint: verdict-dict}` part of a `RoundAggregate`."""
    if isinstance(aggregated, RoundAggregate):
        return dict(aggregated)
    return {k: v for k, v in (aggregated or {}).items() if k != "__summary__"}


def _probe_states_map(probe_states):
    """`{probe_id: state}` from whatever the caller handed over.

    Accepts `db.upsert_probe`-shaped rows ({"probe_id", "state", "enabled"})
    as well as the flat `{probe_id: "online"}` mapping, because the engine
    reads the registry and the tests hand-write the mapping.
    """
    out = {}
    if isinstance(probe_states, dict):
        items = probe_states.items()
        for probe_id, state in items:
            if isinstance(state, dict):
                enabled = state.get("enabled", True)
                out[str(probe_id)] = "disabled" if not enabled \
                    else (state.get("state") or "unknown")
            else:
                out[str(probe_id)] = str(state) if state is not None else "unknown"
    elif isinstance(probe_states, (list, tuple, set)):
        for row in probe_states:
            if isinstance(row, dict):
                probe_id = row.get("probe_id") or row.get("id")
                if probe_id:
                    enabled = row.get("enabled", True)
                    out[str(probe_id)] = "disabled" if not enabled \
                        else (row.get("state") or "unknown")
            elif row:
                out[str(row)] = "unknown"
    return out


def _offline(state):
    return state in ("offline", "disabled", "down", "error")


def aggregate_round(observations, aggregation_cfg, probe_states=None):
    """Fold one round's observations into one verdict per fingerprint.

    `observations` is everything the round collected for that round_id --
    local and remote alike. `aggregation_cfg` is `cfg["aggregation"]`
    (strategy / minimum_completed_probes / late_result_seconds).
    `probe_states` is optional and only informs the summary: which registered
    probes never answered. An offline probe has no observations, so it is
    already excluded from the completed count; naming it here only keeps the
    round summary honest for the 探测点 panel.

    Returns a `RoundAggregate`: `{fingerprint: {...}}` plus `.summary`.
    """
    cfg = aggregation_cfg if isinstance(aggregation_cfg, dict) else {}
    strategy = str(cfg.get("strategy") or STRATEGY_ANY_ALIVE).strip().lower()
    minimum = _as_int(cfg.get("minimum_completed_probes"))
    if minimum is None or minimum < 0:
        minimum = 1
    states = _probe_states_map(probe_states)

    by_fp = {}
    unresolved = {}
    for raw in observations or []:
        row = normalize_observation(raw)
        if not row["fingerprint"]:
            continue
        if row["alive"] is None:
            # The probe answered but carried no verdict: its own uncertainty,
            # so it never counts towards the completion floor (指导 §7.7).
            unresolved.setdefault(row["fingerprint"], []).append(row["probe_id"])
            continue
        by_fp.setdefault(row["fingerprint"], []).append(row)

    nodes = {}
    for fingerprint in sorted(set(by_fp) | set(unresolved)):
        if fingerprint not in by_fp:
            # Nothing but no-verdict rows: the round heard from a probe and
            # learned nothing, which is `inconclusive` -- not "absent", which
            # would let the caller invent a verdict of its own.
            nodes[fingerprint] = {
                "verdict": VERDICT_INCONCLUSIVE,
                "alive": False,
                "delay_ms": None,
                "egress_country": None,
                "reason": INSUFFICIENT_PROBES,
                "best_probe": None,
                "completed_probes": 0,
                "minimum_completed_probes": minimum,
                "probes": {},
            }
            continue
        # Deterministic order: the representative reason of a failed node must
        # not depend on the order the probes happened to answer in.
        rows = sorted(by_fp[fingerprint], key=lambda r: (r["probe_id"], r["delay_ms"] or 0))
        alive_rows = [r for r in rows if r["alive"]]
        completed = len(rows)
        if alive_rows:
            # `any_alive` and the guide's rule order (指导 §4.3): one probe
            # saying alive IS the verdict, before `minimum_completed_probes`
            # is consulted at all. The minimum exists to stop a round from
            # *declaring a failure* on the word of too few probes -- positive
            # evidence from one is still evidence.
            best = min(alive_rows, key=lambda r: (r["delay_ms"] if r["delay_ms"] is not None
                                                  else _FAR, r["probe_id"]))
            verdict = VERDICT_ALIVE
            delay = best["delay_ms"]
            reason = None
            country = best["egress_country"]
            best_probe = best["probe_id"]
        else:
            verdict = VERDICT_FAIL if completed >= minimum else VERDICT_INCONCLUSIVE
            # The representative failure is the first failing probe in
            # probe_id order -- stable across rounds, so the panel's 失败原因
            # column does not flap between probes.
            first = rows[0]
            delay = None
            country = next((r["egress_country"] for r in rows if r["egress_country"]), None)
            reason = first["reason"] if verdict == VERDICT_FAIL else INSUFFICIENT_PROBES
            best_probe = first["probe_id"]
        nodes[fingerprint] = {
            "verdict": verdict,
            "alive": verdict == VERDICT_ALIVE,
            "delay_ms": delay,
            "egress_country": country,
            "reason": reason,
            "best_probe": best_probe,
            "completed_probes": completed,
            "minimum_completed_probes": minimum,
            "probes": {r["probe_id"]: (VERDICT_ALIVE if r["alive"] else VERDICT_FAIL)
                       for r in rows},
        }

    completed_probes = sorted({r["probe_id"] for rows in by_fp.values() for r in rows})
    expected = [p for p, s in states.items() if not _offline(s)]
    summary = {
        "strategy": strategy,
        "minimum_completed_probes": minimum,
        "completed_probes": completed_probes,
        "missing_probes": sorted(set(expected) - set(completed_probes)),
        "offline_probes": sorted(p for p, s in states.items() if _offline(s)),
        "unresolved_probes": sorted({p for ps in unresolved.values() for p in ps}),
        "alive": sorted(fp for fp, n in nodes.items() if n["verdict"] == VERDICT_ALIVE),
        "failed": sorted(fp for fp, n in nodes.items() if n["verdict"] == VERDICT_FAIL),
        "inconclusive": sorted(fp for fp, n in nodes.items()
                               if n["verdict"] == VERDICT_INCONCLUSIVE),
        "nodes": len(nodes),
    }
    return RoundAggregate(nodes, summary)


def observation_for(aggregated, fingerprint):
    """The one observation `engine._converge_bucket` wants for a fingerprint.

    Returns None when the round said nothing about this node at all -- the
    caller then keeps its own (pre-refactor) path rather than inventing a
    verdict the probes never gave.
    """
    if isinstance(aggregated, RoundAggregate):
        return dict(aggregated).get(fingerprint)
    if isinstance(aggregated, dict):
        return aggregated.get(fingerprint)
    return None
