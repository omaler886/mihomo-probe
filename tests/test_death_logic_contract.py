"""Characterization tests for the death-convergence state machine.

HK3 轻量化改造 Phase 1（指导 §15「先锁死死亡逻辑」）: before the engine's main
path is restructured, the *existing* convergence behaviour has to be pinned by
tests, so a refactor that silently inverts one of these rules fails here instead
of killing live nodes in production.

Every test names the clause of the implementation guide's §7 (死亡逻辑保持不变)
it locks:

  1. 节点身份仍由连接参数指纹确定（同连接参数不同显示名 = 同一节点）。
  2. 单次成功立即恢复 alive，并清零连续失败。
  3. 连续失败达到当前配置阈值才进入 dead，默认仍为 3。
  4. 单轮失败只进入 pending 或保持 unknown，不得立即删除。
  5. excluded 和探测基础设施错误不参与死亡累计。
  6. 整轮存活异常下降时不发布，继续保留上一份稳定产物。
  7. 配置校验失败、Controller 不可达、探测点超时、结果不足均属于轮次不确定，
     不得批量推进节点失败。
  8. 升级迁移不得清空 nodes、consec_fail、最后成功时间或历史轮次。

Scope note: these are *characterization* tests. They assert what the code does
today (the contract Phase 2..6 must keep), not what a future feature should add.
Nothing here starts a real mihomo kernel or touches the network -- engine level
cases go through `engine._apply_and_publish` / `engine._run_round` with the
ledger and every data path redirected into a throwaway directory, exactly like
`tests/test_logic.py`'s PolicyTest / ApplyAndPublishTest.
"""
import os
import shutil
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import yaml

from mihomo_test import config as cfgmod
from mihomo_test import core as coremod
from mihomo_test import db, engine, policy

try:
    import _isolation
except ImportError:  # imported as a package: python -m unittest tests.test_death_logic_contract
    from tests import _isolation


def setUpModule():
    _isolation.isolate()


def tearDownModule():
    _isolation.restore()


_nolog = lambda *a, **k: None


class _LedgerRoot:
    """Point the ledger and every engine data path at a throwaway directory.

    Same shape as `tests/test_logic.py`'s ApplyAndPublishTest setUp and
    `tests/test_hardening.py`'s _TempRoot: `config.DATA`, `db.DB_PATH`,
    `engine.ROUND_STATE` and `engine.EXPORT_DIR` are module-level paths the code
    reads at call time, so redirecting them keeps the suite off a real
    deployment's `state.db`.
    """

    def __enter__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="death-contract-"))
        self._old = {
            "data": cfgmod.DATA,
            "config_path": cfgmod.CONFIG_PATH,
            "db_path": db.DB_PATH,
            "db_conn": db._conn,
            "round_state": engine.ROUND_STATE,
            "export_dir": engine.EXPORT_DIR,
        }
        cfgmod.DATA = self.tmp
        cfgmod.CONFIG_PATH = self.tmp / "config.json"
        db.DB_PATH, db._conn = self.tmp / "state.db", None
        engine.ROUND_STATE = self.tmp / "round.state.json"
        engine.EXPORT_DIR = self.tmp / "exports"
        db.connect()
        return self.tmp

    def __exit__(self, *exc):
        if db._conn is not None:
            db._conn.close()
        cfgmod.DATA = self._old["data"]
        cfgmod.CONFIG_PATH = self._old["config_path"]
        db.DB_PATH, db._conn = self._old["db_path"], self._old["db_conn"]
        engine.ROUND_STATE = self._old["round_state"]
        engine.EXPORT_DIR = self._old["export_dir"]
        shutil.rmtree(self.tmp, ignore_errors=True)
        return False


# ---------------------------------------------------------------------------
# Shared engine-level fixtures (one node, one source, no kernel, no network)
# ---------------------------------------------------------------------------

PROXY = {"name": "N", "type": "vless", "server": "203.0.113.7", "port": 443}


def _entry(name="N", fp=None):
    fp = fp if fp is not None else engine._orig_fp(PROXY)
    return {"source": "air", "fp": fp, "original": name, "name": name,
            "test_ip": "203.0.113.7"}


def _proxy(name="N"):
    return {"name": name, "type": "vless", "server": "203.0.113.7", "port": 443}


def _outcome(reason=None, delay_ms=100):
    return {"reason": reason, "delay_ms": delay_ms, "attempts": 1, "detail": ""}


def _cfg(**over):
    cfg = {"policy": {"drop_after_consecutive_fails": 3,
                      "suspect_floor_ratio": 0.5,
                      "suspect_floor_absolute": 3},
           "verify": {"exclude_countries": []},
           "publish": {"enabled": True},
           "sources": [{"key": "air", "name": "air", "enabled": True,
                        "export": True}]}
    cfg.update(over)
    return cfg


def _converge(cfg, by_name, proxies, results, countries=None):
    """Run one convergence round through the real `_apply_and_publish`."""
    round_id = db.start_round("test")
    return engine._apply_and_publish(
        cfg, round_id, None, by_name, proxies, results, countries or {},
        cfg["sources"], _nolog)


# ---------------------------------------------------------------------------
# §7.1 节点身份由连接参数指纹确定
# ---------------------------------------------------------------------------


class FingerprintIdentityTest(unittest.TestCase):
    """§7.1: identity is the connection-parameter fingerprint, never the name.

    A ledger keyed on the display name merges two distinct nodes into one row
    (one node's failures cancel the other's successes) and loses a renamed
    node's convergence history -- which is how a node that should die after 3
    consecutive failures never gets past 1.
    """

    def test_same_connection_parameters_under_a_new_name_is_the_same_node(self):
        """同连接参数、不同显示名 → 同一指纹。"""
        renamed = dict(PROXY, name="HK-01 · renamed")
        self.assertEqual(coremod.fingerprint_proxy(PROXY),
                         coremod.fingerprint_proxy(renamed))
        self.assertEqual(engine._orig_fp(PROXY), engine._orig_fp(renamed))

    def test_a_changed_credential_is_a_different_node(self):
        other = dict(PROXY, uuid="another-uuid")
        self.assertNotEqual(coremod.fingerprint_proxy(PROXY),
                            coremod.fingerprint_proxy(other))
        self.assertNotEqual(engine._orig_fp(PROXY), engine._orig_fp(other))

    def test_orig_fp_strips_the_fields_the_kernel_config_strips(self):
        """`_orig_fp` is the ONLY place a ledger identity is derived, and it
        must hash the original proxy minus `core.DROP_FIELDS` -- so a chained
        node and its direct twin keep one identity instead of being re-keyed
        (and pruned) whenever `core.prepare` normalises the proxy differently."""
        chained = dict(PROXY, **{coremod.DROP_FIELDS[0]: "cdn"})
        self.assertEqual(engine._orig_fp(chained), engine._orig_fp(PROXY))
        stripped = {k: v for k, v in PROXY.items() if k not in coremod.DROP_FIELDS}
        self.assertEqual(engine._orig_fp(PROXY),
                         coremod.fingerprint_proxy(stripped))

    def test_the_ledger_keeps_one_row_per_fingerprint_across_a_rename(self):
        with _LedgerRoot():
            fp = engine._orig_fp(PROXY)
            db.upsert_node("air", fp, "old name", status="alive", consec_fail=2,
                           total_ok=5, last_ok="2026-11-05T01:00:00")
            db.upsert_node("air", fp, "new name")  # same node, renamed upstream
            rows = db.list_nodes("air")
            self.assertEqual(len(rows), 1, "a rename forked the ledger row")
            node = db.get_node("air", fp)
            self.assertEqual(node["display"], "new name")
            # the convergence history survives the rename
            self.assertEqual(node["status"], "alive")
            self.assertEqual(node["consec_fail"], 2)
            self.assertEqual(node["total_ok"], 5)

    def test_two_nodes_sharing_a_display_name_stay_separate(self):
        with _LedgerRoot():
            twin = dict(PROXY, uuid="u1")
            other = dict(PROXY, uuid="u2")
            db.upsert_node("air", engine._orig_fp(twin), "BageVM",
                           status="alive", consec_fail=0)
            db.upsert_node("air", engine._orig_fp(other), "BageVM",
                           status="pending", consec_fail=1)
            rows = db.list_nodes("air")
            self.assertEqual(len(rows), 2)
            self.assertEqual({r["name"] for r in rows}, {"BageVM"})

    def test_aliases_of_one_endpoint_advance_the_streak_once_per_round(self):
        """Engine level: two kernel names, one fingerprint, one streak step."""
        with _LedgerRoot() as tmp:
            cfg = _cfg()
            entry = _entry()
            summary = _converge(cfg, {"N": entry, "N-copy": entry}, [_proxy()],
                                {"N": _outcome("timeout"),
                                 "N-copy": _outcome("timeout")})
            node = db.get_node("air", entry["fp"])
            self.assertEqual(len(db.list_nodes("air")), 1)
            self.assertEqual(node["consec_fail"], 1,
                             "aliases advanced the streak more than once per round")
            self.assertEqual(summary["alive"], 0)
            self.assertTrue((tmp / "exports").exists())


# ---------------------------------------------------------------------------
# §7.2 单次成功立即恢复 alive 并清零连续失败
# ---------------------------------------------------------------------------


class SingleSuccessRestoresTest(unittest.TestCase):
    """§7.2: one pass is a full recovery -- status alive, streak zero."""

    POLICY = {"drop_after_consecutive_fails": 3}

    def test_a_dead_node_is_restored_by_one_success(self):
        fields, transition = policy.apply(
            {"status": "dead", "consec_fail": 3}, True, 88, None, self.POLICY)
        self.assertEqual(fields["status"], "alive")
        self.assertEqual(fields["consec_fail"], 0)
        self.assertEqual(fields["last_delay_ms"], 88)
        self.assertEqual(fields["last_reason"], None)
        self.assertTrue(fields["last_ok"], "a recovery must re-stamp last_ok")
        self.assertEqual(transition, "restore")

    def test_a_pending_node_recovers_and_only_a_dead_one_is_a_restore(self):
        """A pending node coming back is normal operation, not a false kill.

        `restored` is read as the "the ledger was wrong" signal, so only a DEAD
        node's recovery counts as one; a first sighting is "new".
        """
        fields, transition = policy.apply(
            {"status": "pending", "consec_fail": 2}, True, 10, None, self.POLICY)
        self.assertEqual(fields["status"], "alive")
        self.assertEqual(fields["consec_fail"], 0)
        self.assertIsNone(transition)
        _, revived = policy.apply(
            {"status": "dead", "consec_fail": 3}, True, 10, None, self.POLICY)
        self.assertEqual(revived, "restore")
        _, first = policy.apply(
            {"status": "unknown", "consec_fail": 0}, True, 10, None, self.POLICY)
        self.assertEqual(first, "new")

    def test_the_ledger_records_the_recovery_immediately(self):
        """Engine level: a DEAD row that passes one round is alive, streak 0."""
        with _LedgerRoot():
            cfg = _cfg()
            entry = _entry()
            db.upsert_node("air", entry["fp"], "N", status="dead",
                           consec_fail=3, total_fail=3)
            summary = _converge(cfg, {"N": entry}, [_proxy()],
                                {"N": _outcome(None, 55)})
            node = db.get_node("air", entry["fp"])
            self.assertEqual(node["status"], "alive")
            self.assertEqual(node["consec_fail"], 0)
            self.assertEqual(node["last_delay_ms"], 55)
            self.assertEqual(summary["restored"], 1)


# ---------------------------------------------------------------------------
# §7.3 连续失败达到阈值（默认 3）才进入 dead
# ---------------------------------------------------------------------------


class ThresholdDeathTest(unittest.TestCase):
    """§7.3: `drop_after_consecutive_fails` (default 3) gates the verdict."""

    def test_the_default_threshold_is_three(self):
        """指导 §7.3: 默认仍为 3 -- the shipped default must not drift."""
        self.assertEqual(
            cfgmod.DEFAULTS["policy"]["drop_after_consecutive_fails"], 3)

    def test_death_needs_the_full_streak(self):
        node = {"status": "alive", "consec_fail": 0}
        seen = []
        for _ in range(3):
            fields, transition = policy.apply(node, False, None, "timeout",
                                              {"drop_after_consecutive_fails": 3})
            node = dict(node, **fields)
            seen.append((node["status"], transition))
        self.assertEqual(seen, [("pending", None), ("pending", None),
                                ("dead", "drop")])

    def test_a_higher_threshold_keeps_the_node_pending_longer(self):
        policy_cfg = {"drop_after_consecutive_fails": 5}
        node = {"status": "alive", "consec_fail": 0}
        for _ in range(4):
            fields, _ = policy.apply(node, False, None, "timeout", policy_cfg)
            node = dict(node, **fields)
        self.assertEqual(node["status"], "pending")
        self.assertEqual(node["consec_fail"], 4)
        fields, transition = policy.apply(node, False, None, "timeout", policy_cfg)
        self.assertEqual(fields["status"], "dead")
        self.assertEqual(transition, "drop")

    def test_a_dead_node_is_not_re_dropped_while_it_stays_down(self):
        fields, transition = policy.apply(
            {"status": "dead", "consec_fail": 3}, False, None, "timeout",
            {"drop_after_consecutive_fails": 3})
        self.assertEqual(fields["status"], "dead")
        self.assertIsNone(transition)

    def test_the_ledger_only_goes_dead_on_the_third_round(self):
        """Engine level: an alive node degrading through three failing rounds
        holds `pending` twice and only reaches `dead` on the third."""
        with _LedgerRoot():
            cfg = _cfg()
            entry = _entry()
            db.upsert_node("air", entry["fp"], "N", status="alive",
                           consec_fail=0, total_ok=4)
            failure = _outcome("timeout", None)
            statuses = []
            for _ in range(3):
                _converge(cfg, {"N": entry}, [_proxy()], {"N": dict(failure)})
                statuses.append(db.get_node("air", entry["fp"])["status"])
            self.assertEqual(statuses, ["pending", "pending", "dead"])
            self.assertEqual(db.get_node("air", entry["fp"])["consec_fail"], 3)


# ---------------------------------------------------------------------------
# §7.4 单轮失败只进入 pending 或保持 unknown，不得立即删除
# ---------------------------------------------------------------------------


class SingleFailureNeverDeletesTest(unittest.TestCase):
    """§7.4: one bad round is a hold, not a deletion."""

    def test_one_failure_holds_the_previous_verdict(self):
        fields, transition = policy.apply(
            {"status": "alive", "consec_fail": 0}, False, None, "timeout",
            {"drop_after_consecutive_fails": 3})
        self.assertEqual(fields["status"], "pending")
        self.assertEqual(fields["consec_fail"], 1)
        self.assertIsNone(transition)

    def test_a_node_that_never_passed_stays_unknown(self):
        fields, transition = policy.apply(
            {"status": "unknown", "consec_fail": 0}, False, None, "kernel_error",
            {"drop_after_consecutive_fails": 3})
        self.assertEqual(fields["status"], "unknown")
        self.assertIsNone(transition)

    def test_one_failing_round_keeps_the_row_and_records_the_reason(self):
        """Engine level: an alive node drops to `pending` (not `dead`, not
        deleted), the verdict row says `fail`, and nothing is dropped."""
        with _LedgerRoot():
            cfg = _cfg()
            entry = _entry()
            db.upsert_node("air", entry["fp"], "N", status="alive",
                           consec_fail=0, total_ok=4)
            round_id = db.start_round("test")
            summary = engine._apply_and_publish(
                cfg, round_id, None, {"N": entry}, [_proxy()],
                {"N": _outcome("timeout", None)}, {}, cfg["sources"], _nolog)
            nodes = db.list_nodes("air")
            self.assertEqual(len(nodes), 1, "a single failure deleted the row")
            self.assertEqual(nodes[0]["status"], "pending")
            self.assertEqual(nodes[0]["consec_fail"], 1)
            self.assertEqual(nodes[0]["last_reason"], "timeout")
            row = db.one("SELECT verdict, reason FROM results WHERE round_id=?",
                         (round_id,))
            self.assertEqual((row["verdict"], row["reason"]), ("fail", "timeout"))
            self.assertEqual(summary["dropped"], 0)

    def test_a_first_sighting_that_fails_stays_unknown_and_is_kept(self):
        """A node nobody has ever seen pass is not `pending` -- there is no
        previous verdict to hold -- and its row must survive the round."""
        with _LedgerRoot():
            cfg = _cfg()
            entry = _entry()
            engine._apply_and_publish(
                cfg, db.start_round("test"), None, {"N": entry}, [_proxy()],
                {"N": _outcome("timeout", None)}, {}, cfg["sources"], _nolog)
            nodes = db.list_nodes("air")
            self.assertEqual(len(nodes), 1)
            self.assertEqual(nodes[0]["status"], "unknown")
            self.assertEqual(nodes[0]["consec_fail"], 1)

    def test_a_tested_node_is_never_pruned_as_vanished(self):
        """`_prune_removed_nodes` deletes the complement of what the round saw;
        a node that was tested this round must be in that keep-list."""
        with _LedgerRoot():
            entry = _entry()
            db.upsert_node("air", entry["fp"], "N", status="pending",
                           consec_fail=1)
            engine._prune_removed_nodes({"N": entry}, {}, {}, _nolog)
            self.assertEqual(len(db.list_nodes("air")), 1)


# ---------------------------------------------------------------------------
# §7.5 excluded 和探测基础设施错误不参与死亡累计
# ---------------------------------------------------------------------------


class ExcludedAndInfrastructureTest(unittest.TestCase):
    """§7.5: two verdicts that must never feed the failure streak."""

    def test_excluded_is_a_classification_not_a_verdict(self):
        self.assertNotEqual(policy.EXCLUDED, policy.ALIVE)
        self.assertNotEqual(policy.EXCLUDED, policy.DEAD)
        self.assertNotEqual(policy.EXCLUDED, policy.PENDING)
        self.assertNotEqual(policy.EXCLUDED, policy.UNKNOWN)

    def test_recording_an_excluded_node_clears_its_streak(self):
        """A node whose entry IP is on a restricted ISP is untestable from
        here; carrying a streak across the classification would kill a node
        that never actually failed."""
        with _LedgerRoot():
            proxy = {"name": "CN relay", "type": "ss", "server": "203.0.113.10",
                     "port": 1}
            fp = engine._orig_fp(proxy)
            db.upsert_node("air", fp, "CN relay", status="pending", consec_fail=2)
            engine._record_excluded_nodes(
                1, [{"source": "air", "name": "CN relay", "proxy": proxy,
                     "index": 0, "fp": fp, "category": "direct"}])
            node = db.get_node("air", fp)
            self.assertEqual(node["status"], policy.EXCLUDED)
            self.assertEqual(node["consec_fail"], 0)
            self.assertEqual(node["last_reason"], "entry_cn")
            self.assertEqual(db.query(
                "SELECT source FROM nodes WHERE status='excluded'"
                " AND consec_fail > 0"), [])

    def test_an_excluded_node_keeps_one_ledger_row(self):
        """Flipping between testable and excluded must not fork the identity."""
        with _LedgerRoot():
            proxy = {"name": "n", "type": "vless", "server": "198.51.100.9",
                     "port": 443}
            fp = engine._orig_fp(proxy)
            db.upsert_node("air", fp, "n", status="alive")
            engine._record_excluded_nodes(
                1, [{"source": "air", "name": "n", "proxy": proxy, "index": 0,
                     "fp": fp, "category": "direct"}])
            self.assertEqual(len(db.list_nodes("air")), 1)

    def test_a_controller_blip_is_retried_not_treated_as_terminal(self):
        """探测基础设施错误: `controller_error` means our own kernel API was
        unreachable, so it must stay retryable and out of TERMINAL_REASONS --
        otherwise one blip spends no retry and lands in the ledger looking like
        a node failure."""
        self.assertNotIn("controller_error", engine.TERMINAL_REASONS)

        class FakeCore:
            def __init__(self):
                self.calls = 0

            def delay(self, name, url, timeout_ms, expected):
                self.calls += 1
                return None, "controller_error", "控制器不可达"

        fake = FakeCore()
        out = engine.test_one(fake, {"mihomo": "n"},
                              {"targets": ["https://a.example/204"],
                               "expected_status": "204", "timeout_ms": 5000,
                               "timeout_ms_retry": 9000, "max_attempts": 3,
                               "retry_pause_s": 0})
        self.assertEqual(fake.calls, 3, "a transient controller error was not retried")
        self.assertEqual(out["reason"], "controller_error")


# ---------------------------------------------------------------------------
# §7.6 整轮存活异常下降时不发布
# ---------------------------------------------------------------------------


class RoundSuspectGuardTest(unittest.TestCase):
    """§7.6: a round that looks like an infrastructure failure publishes
    nothing and leaves the previous stable artifact on disk."""

    POLICY = {"suspect_floor_ratio": 0.5, "suspect_floor_absolute": 3}

    def test_the_guard_trips_on_a_mass_drop(self):
        self.assertTrue(policy.round_is_suspect(5, 40, self.POLICY))
        self.assertFalse(policy.round_is_suspect(38, 40, self.POLICY))
        self.assertFalse(policy.round_is_suspect(3, 5, self.POLICY),
                         "three survivors is the absolute floor")
        self.assertFalse(policy.round_is_suspect(0, 0, self.POLICY),
                         "no previous round is not a suspicious round")

    def test_a_suspect_round_converges_but_publishes_nothing(self):
        """Engine level: the ledger still settles, the export file does not."""
        with _LedgerRoot() as tmp:
            cfg = _cfg()
            entry = _entry()

            exports = tmp / "exports"
            exports.mkdir(parents=True, exist_ok=True)
            (exports / "air.yaml").write_text("proxies: [{name: OLD}]\n",
                                              encoding="utf-8")
            previous = db.start_round("prev")
            db.finish_round(previous, ok=40, total=40, duration_s=1)

            summary = _converge(cfg, {"N": entry}, [_proxy()],
                                {"N": _outcome(None, 60)})
            self.assertTrue(summary["suspect"], "the guardrail should have tripped")
            self.assertIn("not published", summary["note"])
            # the node itself still converged to alive ...
            self.assertEqual(db.get_node("air", entry["fp"])["status"], "alive")
            self.assertEqual(summary["alive"], 1)
            # ... and the previous export survived untouched
            self.assertEqual((exports / "air.yaml").read_text(encoding="utf-8"),
                             "proxies: [{name: OLD}]\n",
                             "a suspect round overwrote the previous export")
            doc = yaml.safe_load((exports / "air.yaml").read_text(encoding="utf-8"))
            self.assertEqual([p["name"] for p in doc["proxies"]], ["OLD"])

    def test_a_suspect_round_is_not_used_as_the_next_baseline(self):
        """The guardrail's baseline must come from the last trustworthy round."""
        with _LedgerRoot():
            good = db.start_round("schedule")
            db.finish_round(good, ok=40, total=40)
            suspect = db.start_round("schedule")
            db.finish_round(suspect, ok=1, total=40, suspect=1)
            db.start_round("still-running")  # unfinished: no baseline either
            self.assertEqual(engine._previous_alive_count(), 40)


# ---------------------------------------------------------------------------
# §7.7 轮次不确定：配置校验失败 / Controller 不可达 / 超时 / 结果不足
# ---------------------------------------------------------------------------


class RoundUncertaintyTest(unittest.TestCase):
    """§7.7: an uncertain round fails no nodes.

    Every path below returns (or raises) before `_apply_and_publish` ever runs,
    which is what keeps a bad config, a dead kernel, an overrun budget or an
    empty upstream from being published as mass node death.
    """

    def _cfg(self, **over):
        cfg = {
            "sources": [{"key": "air", "kind": "collection", "name": "air",
                         "enabled": True}],
            "substore": {"backend": "http://127.0.0.1:1"},
            "core": {"api": "http://127.0.0.1:19190", "mixed_port": 19194,
                     "lanes": 1, "base_port": 19200},
            "test": {"targets": ["https://gstatic.example/generate_204"],
                     "expected_status": "204", "timeout_ms": 5000,
                     "timeout_ms_retry": 9000, "max_attempts": 1,
                     "retry_pause_s": 0, "concurrency": 1},
            "verify": {"enabled": False, "entry_check": False,
                       "exclude_countries": []},
            "policy": {"drop_after_consecutive_fails": 3},
            "publish": {"enabled": False, "prefix": "probe"},
            "chain": None,
            "watchdog": {"round_timeout_minutes": 20},
            "alert": {"enabled": False},
        }
        cfg.update(over)
        return cfg

    def _entry(self):
        proxy = dict(PROXY)
        return {"source": "air", "name": "N", "proxy": proxy, "index": 0,
                "fp": engine._orig_fp(proxy), "category": "direct"}

    def _mapping(self, entry):
        return [{"source": "air", "original": "N", "mihomo": "N", "index": 0,
                 "orig_proxy": entry["proxy"], "test_ip": None,
                 "fp": entry["fp"], "proto": "vless",
                 "server": "203.0.113.7", "role": None, "front": None,
                 "category": "direct"}]

    def _assert_no_node_was_failed(self):
        self.assertEqual(db.query("SELECT * FROM nodes"), [],
                         "an uncertain round wrote node verdicts")
        self.assertEqual(db.query("SELECT * FROM results"), [])

    def test_no_enabled_sources_fails_no_node(self):
        with _LedgerRoot():
            cfg = self._cfg(sources=[{"key": "air", "kind": "collection",
                                      "name": "air", "enabled": False}])
            out = engine._run_round(cfg, "manual", None, _nolog)
            self.assertEqual(out["error"], "no enabled sources")
            self._assert_no_node_was_failed()

    def test_a_round_that_fetched_nothing_fails_no_node(self):
        """结果不足: every source failing to fetch is a round-level error."""
        with _LedgerRoot():
            cfg = self._cfg()
            with unittest.mock.patch.object(
                    engine, "collect_entries",
                    return_value=([], ["air: sub-store unreachable"])):
                out = engine._run_round(cfg, "manual", None, _nolog)
            self.assertEqual(out["error"], "no nodes fetched")
            self._assert_no_node_was_failed()
            row = db.one("SELECT note FROM rounds WHERE id=?", (out["round_id"],))
            self.assertEqual(row["note"], "no nodes fetched")

    def test_an_unreachable_controller_fails_no_node(self):
        """Controller 不可达: the kernel never came up, so no node was ever
        measured -- the round is abandoned, not converged."""
        with _LedgerRoot():
            cfg = self._cfg()
            entry = self._entry()
            with unittest.mock.patch.object(
                    engine, "collect_entries", return_value=([entry], [])), \
                    unittest.mock.patch.object(
                        coremod, "make_testable",
                        return_value=([entry["proxy"]], self._mapping(entry), [])), \
                    unittest.mock.patch.object(
                        cfgmod, "core_secret", lambda: "test-secret"), \
                    unittest.mock.patch.object(
                        coremod.Core, "start_and_load",
                        side_effect=coremod.CoreError("控制器不可达")):
                with self.assertRaises(coremod.CoreError):
                    engine.run_round(cfg, log=_nolog)
            self._assert_no_node_was_failed()
            # the round is closed out as aborted rather than left "in progress"
            row = db.one("SELECT note FROM rounds ORDER BY id DESC LIMIT 1")
            self.assertIn("aborted", row["note"])

    def test_a_round_past_its_budget_fails_no_node(self):
        """轮次超时: the watchdog aborts before any verdict exists."""
        with _LedgerRoot():
            cfg = self._cfg()
            with unittest.mock.patch.object(engine, "_budget",
                                            lambda _cfg: -60), \
                    unittest.mock.patch.object(
                        engine, "collect_entries", return_value=([], [])):
                out = engine.run_round(cfg, log=_nolog)
            self.assertEqual(out["error"], "round timeout")
            self._assert_no_node_was_failed()

    def test_a_rejected_config_patch_cannot_change_the_next_round(self):
        """配置校验失败: a patch the schema refuses never reaches the stored
        config, so the next round still runs the last known-good one."""
        with _LedgerRoot():
            stored = {"sources": [{"key": "air", "name": "air"}],
                      "auth": {"token": "a" * 32},
                      "policy": {"drop_after_consecutive_fails": 3}}
            cfgmod.save(stored)
            clean, notes = cfgmod.validate_patch({"auth": {"token": ""}})
            self.assertNotIn("token", clean.get("auth", {}),
                             "an empty token disables every auth check")
            self.assertTrue(any("auth.token" in n for n in notes), notes)
            # only the validated projection is merged; the file keeps its token
            cfgmod.update(clean)
            on_disk = cfgmod.load()
            self.assertEqual(on_disk["auth"]["token"], "a" * 32)
            self.assertEqual(on_disk["policy"]["drop_after_consecutive_fails"], 3)


# ---------------------------------------------------------------------------
# §7.8 升级迁移不得清空 nodes / consec_fail / 最后成功时间 / 历史轮次
# ---------------------------------------------------------------------------


class MigrationPreservesLedgerTest(unittest.TestCase):
    """§7.8: an upgrade is additive -- streaks, last-success stamps and round
    history survive it."""

    def _seed(self):
        db.upsert_node("air", "fp-a", "A", status="alive", consec_fail=2,
                       total_ok=5, total_fail=1,
                       last_ok="2026-11-05T01:00:00", last_delay_ms=120,
                       last_reason=None)
        db.upsert_node("air", "fp-b", "B", status="pending", consec_fail=1,
                       last_reason="timeout")
        rid = db.start_round("manual", "direct")
        db.finish_round(rid, total=2, ok=1, failed=1, duration_s=12.5)
        db.record_result(rid, "air", "fp-a", "A", "ok", 120, None, "HK", 1, "")
        db.record_result(rid, "air", "fp-b", "B", "fail", None, "timeout", None,
                         3, "")
        return rid

    def _reopen(self):
        """Close the connection and connect again: `connect()` runs `_migrate`
        and then the `CREATE TABLE IF NOT EXISTS` schema, which is the whole
        upgrade path a new build performs on an existing ledger."""
        db._conn.close()
        db._conn = None
        db.connect()

    def _assert_survived(self):
        alive = db.get_node("air", "fp-a")
        self.assertEqual(alive["status"], "alive")
        self.assertEqual(alive["consec_fail"], 2, "consec_fail was reset")
        self.assertEqual(alive["total_ok"], 5)
        self.assertEqual(alive["last_ok"], "2026-11-05T01:00:00",
                         "the last-success timestamp was lost")
        self.assertEqual(alive["last_delay_ms"], 120)
        pending = db.get_node("air", "fp-b")
        self.assertEqual((pending["status"], pending["consec_fail"]),
                         ("pending", 1))
        rounds = db.query("SELECT * FROM rounds")
        self.assertEqual(len(rounds), 1, "round history was dropped")
        self.assertEqual((rounds[0]["total"], rounds[0]["ok"],
                          rounds[0]["duration_s"]), (2, 1, 12.5))
        self.assertEqual(len(db.query("SELECT * FROM results")), 2)

    def test_reconnecting_preserves_the_whole_ledger(self):
        with _LedgerRoot() as tmp:
            self._seed()
            self._reopen()
            self._assert_survived()
            # 指导 §12: the additive migration snapshots the pre-v3 ledger once.
            # The destructive `.bak-<stamp>` (no v3 marker) is the pre-fingerprint
            # rebuild, which must NOT have run on a fingerprint-keyed database.
            self.assertEqual(len(list(tmp.glob("state.db.bak-v3-*"))), 1)
            self.assertEqual(list(tmp.glob("state.db.bak-2*")), [])

    def test_migration_is_idempotent(self):
        """Re-running the migration keeps every row and adds no second copy of
        the pre-migration snapshot."""
        with _LedgerRoot() as tmp:
            self._seed()
            self._reopen()
            self._reopen()
            self._assert_survived()
            self.assertEqual(len(list(tmp.glob("state.db.bak-v3-*"))), 1)
            self.assertEqual(list(tmp.glob("state.db.bak-2*")), [])

    def test_the_nodes_table_is_not_rebuilt_on_a_current_schema(self):
        """The drop-everything branch only exists for pre-fingerprint ledgers;
        a fingerprint-keyed database must be upgraded in place."""
        with _LedgerRoot():
            self._seed()
            self._reopen()
            tables = {r["name"] for r in db.query(
                "SELECT name FROM sqlite_master WHERE type='table'")}
            self.assertIn("nodes", tables)
            self.assertIn("rounds", tables)
            self.assertIn("results", tables)
            columns = {r["name"] for r in db.query("PRAGMA table_info(nodes)")}
            self.assertIn("fingerprint", columns)
            self.assertIn("consec_fail", columns)
            self.assertIn("last_ok", columns)

    def test_an_added_table_does_not_disturb_the_ledger(self):
        """指导 §12 / §7.8: the v3 tables (probes / probe_observations /
        publish_audit) are appended with IF NOT EXISTS, and any further additive
        table must leave every existing row alone."""
        with _LedgerRoot():
            self._seed()
            self._reopen()
            tables = {r["name"] for r in db.query(
                "SELECT name FROM sqlite_master WHERE type='table'")}
            for expected in ("probes", "probe_observations", "publish_audit"):
                self.assertIn(expected, tables)
            # an unrelated additive table on top of the v3 ones
            db.execute("CREATE TABLE IF NOT EXISTS contract_scratch (id INTEGER)")
            self._reopen()
            self._assert_survived()
            tables = {r["name"] for r in db.query(
                "SELECT name FROM sqlite_master WHERE type='table'")}
            self.assertIn("contract_scratch", tables)
            self.assertIn("probe_observations", tables)


if __name__ == "__main__":
    unittest.main()
