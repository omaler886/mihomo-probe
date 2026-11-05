"""多探测点观察聚合与 engine 接线（HK3 轻量化改造 Phase 3 A 半，指导 §14）。

覆盖指导 §14 单元测试清单里属于本 Phase 的条目：

* 同一节点三个探测点：一活两死，聚合为 alive。
* 全部节点失败但探测点自身 Controller 错误，不推进 streak（inconclusive）。
* 结果不足 `minimum_completed_probes`，聚合为 inconclusive。
* 重复结果幂等（同 round + probe + fingerprint 覆盖）。
* digest 不匹配拒绝落账。
* chain 默认关闭；前置失败不杀目标节点。
* 本地单探测点模式聚合结论与直接结算一致（灰度第 1 步，指导 §16）。

没有一条测试触网或启动内核：远程探测点一律由 monkeypatch 替换
`engine._dispatch_probe_jobs` 模拟，观测直接写进临时 SQLite。
"""
import os
import shutil
import sys
import tempfile
import time
import unittest
import unittest.mock
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import aggregation, config as cfgmod, db, engine, policy

try:
    import _isolation
except ImportError:  # imported as a package: python -m unittest tests.test_aggregation
    from tests import _isolation


def setUpModule():
    _isolation.isolate()


def tearDownModule():
    _isolation.restore()


_nolog = lambda *a, **k: None


def _obs(round_id, probe_id, fingerprint, alive, delay_ms=None, reason=None,
         egress_country=None, payload_digest=""):
    """One observation in the shape `db.get_observations()` returns."""
    return {"round_id": round_id, "probe_id": probe_id, "fingerprint": fingerprint,
            "alive": alive, "delay_ms": delay_ms, "reason": reason,
            "egress_country": egress_country, "payload_digest": payload_digest}


AGG = {"strategy": "any_alive", "minimum_completed_probes": 2,
       "late_result_seconds": 90}


# ---------------------------------------------------------------------------
# 纯函数层：aggregation 不读库、不触网、不读时钟
# ---------------------------------------------------------------------------


class AggregateRoundTest(unittest.TestCase):
    """指导 §4.3 的三条聚合语义。"""

    FP = "a" * 16

    def test_one_alive_of_two_dead_is_alive(self):
        """§14：一活两死聚合 alive——任一探测点成功即活。"""
        out = aggregation.aggregate_round([
            _obs(1, "hk3-a", self.FP, True, 210, None, "HK"),
            _obs(1, "hk3-b", self.FP, False, None, "timeout"),
            _obs(1, "sg-a", self.FP, False, None, "timeout"),
        ], AGG)
        node = out[self.FP]
        self.assertEqual(node["verdict"], aggregation.VERDICT_ALIVE)
        self.assertTrue(node["alive"])
        # The representative latency is the best alive probe's, not the first
        # row's and not an average.
        self.assertEqual(node["delay_ms"], 210)
        self.assertEqual(node["egress_country"], "HK")
        self.assertIsNone(node["reason"])
        self.assertEqual(node["best_probe"], "hk3-a")
        self.assertEqual(node["completed_probes"], 3)
        self.assertEqual(out.summary["completed_probes"], ["hk3-a", "hk3-b", "sg-a"])
        self.assertEqual(out.summary["alive"], [self.FP])
        self.assertEqual(out.summary["failed"], [])

    def test_the_best_alive_probe_wins_the_latency_and_the_name(self):
        out = aggregation.aggregate_round([
            _obs(1, "hk3-a", self.FP, True, 310, None, "HK"),
            _obs(1, "hk3-b", self.FP, True, 120, None, "SG"),
            _obs(1, "sg-a", self.FP, False, None, "timeout"),
        ], AGG)
        self.assertEqual(out[self.FP]["delay_ms"], 120)
        self.assertEqual(out[self.FP]["best_probe"], "hk3-b")
        self.assertEqual(out[self.FP]["egress_country"], "SG")

    def test_all_failed_at_the_minimum_is_a_failure(self):
        out = aggregation.aggregate_round([
            _obs(1, "hk3-a", self.FP, False, None, "timeout"),
            _obs(1, "hk3-b", self.FP, False, None, "unreachable"),
        ], AGG)
        node = out[self.FP]
        self.assertEqual(node["verdict"], aggregation.VERDICT_FAIL)
        self.assertFalse(node["alive"])
        self.assertEqual(node["completed_probes"], 2)
        # The representative reason is deterministic (probe_id order), so the
        # panel's 失败原因 column does not flap between rounds.
        self.assertEqual(node["reason"], "timeout")
        self.assertEqual(out.summary["failed"], [self.FP])

    def test_an_exit_country_failure_survives_the_aggregation(self):
        """The exit-country verdict is a real failure, and its country is kept."""
        out = aggregation.aggregate_round([
            _obs(1, "local", self.FP, False, None, "exit_CN", "CN"),
            _obs(1, "hk3-b", self.FP, False, None, "exit_CN", "CN"),
        ], AGG)
        node = out[self.FP]
        self.assertEqual(node["verdict"], aggregation.VERDICT_FAIL)
        self.assertEqual(node["reason"], "exit_CN")
        self.assertEqual(node["egress_country"], "CN")

    def test_below_the_minimum_is_inconclusive_and_names_no_reason(self):
        """§14：结果不足 minimum_completed_probes → inconclusive。"""
        out = aggregation.aggregate_round([
            _obs(1, "hk3-a", self.FP, False, None, "timeout"),
        ], AGG)
        node = out[self.FP]
        self.assertEqual(node["verdict"], aggregation.VERDICT_INCONCLUSIVE)
        self.assertFalse(node["alive"])
        # Not the probe's reason: nothing about the node is known this round.
        self.assertEqual(node["reason"], aggregation.INSUFFICIENT_PROBES)
        self.assertEqual(node["completed_probes"], 1)
        self.assertEqual(node["minimum_completed_probes"], 2)
        self.assertEqual(out.summary["inconclusive"], [self.FP])

    def test_a_single_completed_probe_is_enough_at_the_default_minimum(self):
        """默认 minimum_completed_probes=1：本地单探测点兼容模式即可判定。"""
        out = aggregation.aggregate_round(
            [_obs(1, "local", self.FP, True, 88, None, "HK")],
            {"strategy": "any_alive", "minimum_completed_probes": 1})
        self.assertEqual(out[self.FP]["verdict"], aggregation.VERDICT_ALIVE)

    def test_one_alive_probe_is_enough_even_below_the_minimum(self):
        """指导 §4.3 的规则顺序：任一探测点成功即活，先于 minimum 判定。

        `minimum_completed_probes` 只拦住"宣判失败"，不拦住"确认存活"——
        一个探测点说活就是正面证据，两个探测点说死才需要凑够人数。
        """
        out = aggregation.aggregate_round([
            _obs(1, "local", self.FP, True, 88, None, "HK"),
        ], AGG)
        self.assertEqual(out[self.FP]["verdict"], aggregation.VERDICT_ALIVE)
        self.assertEqual(out[self.FP]["completed_probes"], 1)
        self.assertEqual(out[self.FP]["minimum_completed_probes"], 2)

    def test_a_missing_probe_is_not_a_node_failure(self):
        """§4.3：探测点离线/超时未回传 = 观测缺失，不计完成数。

        Two probes configured, one answered and failed: 1 < 2, so the round is
        inconclusive rather than a failure. This is the whole guard against
        "探测点越多，节点死得越快".
        """
        out = aggregation.aggregate_round([
            _obs(1, "hk3-a", self.FP, False, None, "timeout"),
        ], AGG, probe_states={"hk3-a": "online", "hk3-b": "offline"})
        self.assertEqual(out[self.FP]["verdict"], aggregation.VERDICT_INCONCLUSIVE)
        self.assertEqual(out.summary["completed_probes"], ["hk3-a"])
        self.assertEqual(out.summary["offline_probes"], ["hk3-b"])
        # An offline probe is not "missing" -- it is known-dark. Only a probe
        # that was expected and never answered is missing.
        self.assertEqual(out.summary["missing_probes"], [])
        self.assertEqual(out.summary["failed"], [])

    def test_an_online_probe_that_never_answered_is_reported_missing(self):
        out = aggregation.aggregate_round([
            _obs(1, "hk3-a", self.FP, False, None, "timeout"),
        ], AGG, probe_states={"hk3-a": "online", "hk3-b": "online"})
        self.assertEqual(out.summary["missing_probes"], ["hk3-b"])
        self.assertEqual(out.summary["offline_probes"], [])

    def test_a_probe_with_no_verdict_is_not_a_completed_probe(self):
        """§7.7：探测点自己也不知道 = 轮次不确定，不是节点失败。"""
        out = aggregation.aggregate_round([
            _obs(1, "hk3-a", self.FP, None, None, None),
            _obs(1, "hk3-b", self.FP, None, None, None),
        ], AGG)
        self.assertEqual(out[self.FP]["verdict"], aggregation.VERDICT_INCONCLUSIVE)
        self.assertEqual(out[self.FP]["reason"], aggregation.INSUFFICIENT_PROBES)
        self.assertEqual(out[self.FP]["completed_probes"], 0)
        self.assertEqual(out.summary["completed_probes"], [])
        self.assertEqual(out.summary["unresolved_probes"], ["hk3-a", "hk3-b"])

    def test_two_nodes_are_aggregated_independently(self):
        out = aggregation.aggregate_round([
            _obs(1, "hk3-a", "a" * 16, True, 50),
            _obs(1, "hk3-a", "b" * 16, False, None, "timeout"),
            _obs(1, "hk3-b", "a" * 16, True, 70),
            _obs(1, "hk3-b", "b" * 16, False, None, "timeout"),
        ], AGG)
        self.assertEqual(out["a" * 16]["verdict"], aggregation.VERDICT_ALIVE)
        self.assertEqual(out["b" * 16]["verdict"], aggregation.VERDICT_FAIL)
        self.assertEqual(out.summary["nodes"], 2)

    def test_the_result_is_a_mapping_of_fingerprints_plus_a_summary(self):
        """指导 §4.3 的返回形状：{fingerprint: ...} + 汇总 completed_probes。"""
        out = aggregation.aggregate_round([_obs(1, "hk3-a", self.FP, True, 5)], AGG)
        self.assertEqual(set(out), {self.FP})
        self.assertIsInstance(out.summary, dict)
        self.assertIn("completed_probes", out.summary)
        self.assertEqual(aggregation.node_verdicts(out), dict(out))

    def test_an_empty_round_says_nothing(self):
        out = aggregation.aggregate_round([], AGG)
        self.assertEqual(dict(out), {})
        self.assertEqual(out.summary["completed_probes"], [])


class ObservationFilterTest(unittest.TestCase):
    """指导 §4.2：旧轮次、未知探测点、digest 不符只入审计，不落账。"""

    FP = "a" * 16

    def test_digest_mismatch_is_refused(self):
        """§14：digest 不匹配拒绝落账。"""
        accepted, rejected, deferred = aggregation.filter_observations(
            [_obs(7, "hk3-a", self.FP, True, 100, payload_digest="sha256:stale")],
            round_id=7, known_probes={"hk3-a"}, config_digest="sha256:current")
        self.assertEqual(accepted, [])
        self.assertEqual(deferred, [])
        self.assertEqual([r["why"] for r in rejected], ["digest_mismatch"])
        self.assertEqual(rejected[0]["observation"]["probe_id"], "hk3-a")

    def test_a_matching_digest_is_accepted(self):
        accepted, rejected, deferred = aggregation.filter_observations(
            [_obs(7, "hk3-a", self.FP, True, 100, payload_digest="sha256:current")],
            round_id=7, known_probes={"hk3-a"}, config_digest="sha256:current")
        self.assertEqual(len(accepted), 1)
        self.assertEqual(rejected, [])
        self.assertEqual(deferred, [])

    def test_a_payload_digest_is_deferred_when_the_api_layer_owns_the_check(self):
        """P3b stores a hash of the whole ProbeResult in `payload_digest`, which
        the aggregation layer cannot recompute; `verify_result` already compared
        the payload's own `config_digest` field (指导 §4.2). Naming that gate
        defers the check instead of refusing every remote observation."""
        accepted, rejected, deferred = aggregation.filter_observations(
            [_obs(7, "hk3-a", self.FP, True, 100,
                  payload_digest="sha256:6f1e...payload-hash")],
            round_id=7, known_probes={"hk3-a"}, config_digest="sha256:current",
            digest_gate="probe_dispatch.verify_result")
        self.assertEqual(len(accepted), 1)
        self.assertEqual(rejected, [])
        self.assertEqual(len(deferred), 1)
        self.assertEqual(deferred[0]["why"], "digest_deferred")
        self.assertEqual(deferred[0]["gate"], "probe_dispatch.verify_result")

    def test_the_digest_check_stays_strict_when_this_process_is_the_only_gate(self):
        """本地单探测点兼容模式: no API layer, so the engine enforces §4.2."""
        accepted, rejected, deferred = aggregation.filter_observations(
            [_obs(7, "local", self.FP, True, 100, payload_digest="sha256:stale")],
            round_id=7, known_probes={"local"}, config_digest="sha256:current")
        self.assertEqual(accepted, [])
        self.assertEqual(deferred, [])
        self.assertEqual([r["why"] for r in rejected], ["digest_mismatch"])

    def test_a_stale_round_is_refused(self):
        accepted, rejected, deferred = aggregation.filter_observations(
            [_obs(6, "hk3-a", self.FP, True, 100)],
            round_id=7, known_probes={"hk3-a"})
        self.assertEqual(accepted, [])
        self.assertEqual(deferred, [])
        self.assertEqual([r["why"] for r in rejected], ["stale_round"])

    def test_an_unknown_probe_is_refused(self):
        accepted, rejected, deferred = aggregation.filter_observations(
            [_obs(7, "rogue", self.FP, True, 100)],
            round_id=7, known_probes={"hk3-a", "local"})
        self.assertEqual(accepted, [])
        self.assertEqual(deferred, [])
        self.assertEqual([r["why"] for r in rejected], ["unknown_probe"])

    def test_a_malformed_row_is_refused_rather_than_raising(self):
        accepted, rejected, deferred = aggregation.filter_observations(
            [{"round_id": 7, "probe_id": "hk3-a", "fingerprint": "",
              "alive": True, "delay_ms": 1}],
            round_id=7, known_probes={"hk3-a"})
        self.assertEqual(accepted, [])
        self.assertEqual(deferred, [])
        self.assertEqual([r["why"] for r in rejected], ["malformed"])

    def test_an_empty_digest_is_not_refused(self):
        """A caller that never had a digest is not a probe answering for a
        config it never saw -- only a *different* digest is refused."""
        accepted, _r, _d = aggregation.filter_observations(
            [_obs(7, "local", self.FP, True, 100, payload_digest="")],
            round_id=7, known_probes={"local"}, config_digest="sha256:current")
        self.assertEqual(len(accepted), 1)

    def test_stored_rows_and_local_observations_normalise_alike(self):
        """`alive` is 0/1 in SQLite and a bool in `local_observation`."""
        row = aggregation.normalize_observation(
            {"round_id": 1, "probe_id": "local", "fingerprint": self.FP,
             "alive": 1, "delay_ms": "120", "reason": "", "egress_country": None,
             "payload_digest": ""})
        self.assertIs(row["alive"], True)
        self.assertEqual(row["delay_ms"], 120)
        self.assertIsNone(row["reason"])
        local = aggregation.normalize_observation(
            aggregation.local_observation(1, self.FP, True, 120))
        self.assertEqual(row, local)


class LocalObservationTest(unittest.TestCase):
    """本地测量包装成一条 local 观测（指导 §16 灰度第 1 步）。"""

    def test_the_shape_matches_a_remote_result_row(self):
        obs = aggregation.local_observation(
            12, "a" * 16, True, 183, None, "HK", "sha256:abc")
        self.assertEqual(obs, {"round_id": 12, "probe_id": "local",
                               "fingerprint": "a" * 16, "alive": True,
                               "delay_ms": 183, "reason": None,
                               "egress_country": "HK",
                               "payload_digest": "sha256:abc"})
        # The keys a ProbeResult row carries (指导 §4.2), so the aggregation
        # path cannot tell the two apart.
        self.assertEqual(set(obs), {"round_id", "probe_id", "fingerprint", "alive",
                                    "delay_ms", "reason", "egress_country",
                                    "payload_digest"})

    def test_it_aggregates_identically_to_a_remote_row(self):
        local = aggregation.aggregate_round(
            [aggregation.local_observation(1, "a" * 16, True, 183, None, "HK")],
            {"minimum_completed_probes": 1})
        remote = aggregation.aggregate_round(
            [_obs(1, "hk3-a", "a" * 16, True, 183, None, "HK")],
            {"minimum_completed_probes": 1})
        # Same verdict, same latency, same country -- only the probe's own
        # name differs, which is the point of the reserved `local` id.
        strip = lambda node: {k: v for k, v in node.items()
                              if k not in ("probes", "best_probe")}
        self.assertEqual(strip(local["a" * 16]), strip(remote["a" * 16]))
        self.assertEqual(local["a" * 16]["best_probe"], "local")
        self.assertEqual(remote["a" * 16]["best_probe"], "hk3-a")


# ---------------------------------------------------------------------------
# engine 接线：db 落库 + _apply_and_publish
# ---------------------------------------------------------------------------


class _Ledger:
    """Redirect the ledger and every engine data path into a temp directory."""

    def __enter__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="aggregation-"))
        self.old = {"data": cfgmod.DATA, "db_path": db.DB_PATH, "db_conn": db._conn,
                    "export": engine.EXPORT_DIR, "state": engine.ROUND_STATE}
        cfgmod.DATA = self.tmp
        db.DB_PATH, db._conn = self.tmp / "state.db", None
        engine.EXPORT_DIR = self.tmp / "exports"
        engine.ROUND_STATE = self.tmp / "round.state.json"
        db.connect()
        return self.tmp

    def __exit__(self, *exc):
        if db._conn is not None:
            db._conn.close()
        cfgmod.DATA = self.old["data"]
        db.DB_PATH, db._conn = self.old["db_path"], self.old["db_conn"]
        engine.EXPORT_DIR = self.old["export"]
        engine.ROUND_STATE = self.old["state"]
        shutil.rmtree(self.tmp, ignore_errors=True)
        return False


def _entry(server="host.example", source="air", name="N", fp="0" * 16):
    return {"source": source, "fp": fp, "original": name, "name": name,
            "test_ip": server}


def _proxy(server="host.example", name="N"):
    return {"name": name, "type": "vless", "server": server, "port": 443}


def _cfg(**over):
    cfg = {"policy": {"drop_after_consecutive_fails": 3,
                      "suspect_floor_ratio": 0.5, "suspect_floor_absolute": 3},
           "verify": {"exclude_countries": ["CN"]},
           "publish": {"enabled": True},
           "sources": [{"key": "air", "name": "air"}],
           "aggregation": dict(AGG)}
    cfg.update(over)
    return cfg


def _outcome(reason=None, delay_ms=100, detail=""):
    return {"reason": reason, "delay_ms": delay_ms, "attempts": 1, "detail": detail}


class ObservationLedgerTest(unittest.TestCase):
    """db 侧：观测幂等落库，且只服务本轮。"""

    def test_a_repeated_result_overwrites_instead_of_duplicating(self):
        """§14：重复结果幂等（同 round + probe + fingerprint 覆盖）。"""
        with _Ledger():
            fp = "a" * 16
            db.record_observation(1, "hk3-a", fp, False, None, "timeout")
            db.record_observation(1, "hk3-a", fp, True, 120, None, "HK")
            rows = db.get_observations(1)
            self.assertEqual(len(rows), 1, "a re-reported result duplicated")
            self.assertEqual((rows[0]["alive"], rows[0]["delay_ms"],
                              rows[0]["egress_country"]), (1, 120, "HK"))
            # A different probe is a different row, not an overwrite.
            db.record_observation(1, "hk3-b", fp, False, None, "timeout")
            self.assertEqual(len(db.get_observations(1)), 2)

    def test_observations_are_scoped_to_their_round(self):
        with _Ledger():
            db.record_observation(1, "local", "a" * 16, True, 10)
            db.record_observation(2, "local", "a" * 16, False, None, "timeout")
            self.assertEqual(len(db.get_observations(1)), 1)
            self.assertEqual(len(db.get_observations(2)), 1)
            self.assertEqual(db.get_observations(1)[0]["alive"], 1)


class ApplyAndPublishAggregationTest(unittest.TestCase):
    """engine 侧：聚合后一次性调状态机；本地模式与直接结算逐字节一致。"""

    def _run(self, cfg, buckets, proxies, countries=None):
        """`_apply_and_publish` with the real grouping the engine does."""
        round_id = db.start_round("test")
        by_name, results = {}, {}
        for bucket in buckets:
            entry, names = bucket["entry"], bucket["names"]
            for i, outcome in enumerate(bucket["outcomes"]):
                name = names[i] if i < len(names) else f"{names[0]}__{i + 1}"
                by_name[name] = entry
                results[name] = outcome
        summary = engine._apply_and_publish(
            cfg, round_id, None, by_name, list(proxies), results, countries or {},
            cfg["sources"], _nolog)
        return summary, round_id

    def test_local_mode_matches_a_direct_settlement(self):
        """灰度第 1 步（指导 §16）：单探测点结论与改造前一致。

        Runs the pipeline twice -- once through the observation path (what
        `_apply_and_publish` now always does) and once by scoring the bucket
        directly -- and requires the ledger to agree on status, streak, delay,
        reason and country. A refactor that lets the aggregation layer invent
        a verdict fails here instead of in production.
        """
        entry = _entry()
        bucket = {"entry": entry, "names": ["N"], "outcomes": [_outcome(delay_ms=140)]}
        cfg = _cfg()
        with _Ledger():
            summary, round_id = self._run(cfg, [bucket], [_proxy()])
            node = engine.db.get_node("air", entry["fp"])
            self.assertEqual(summary["alive"], 1)
            self.assertEqual(node["status"], policy.ALIVE)
            self.assertEqual(node["consec_fail"], 0)
            self.assertEqual(node["last_delay_ms"], 140)

            # The same verdict computed the pre-refactor way, straight from
            # the bucket -- no observation layer in between.
            domain_pass = "any"
            ok, delay, reason, _d, _a, _t = engine._score_bucket(bucket, domain_pass)
            country, exit_failure = engine._resolve_exit({}, bucket["names"], {"CN"})
            if ok and exit_failure:
                ok, reason = False, exit_failure
            self.assertTrue(ok)
            self.assertEqual(delay, 140)
            self.assertIsNone(reason)
            self.assertIsNone(country)

            row = engine.db.one("SELECT verdict, delay_ms, reason FROM results"
                                " WHERE round_id=?", (round_id,))
            self.assertEqual((row["verdict"], row["delay_ms"], row["reason"]),
                             ("ok", 140, None))

    def test_local_mode_agrees_on_a_failure_too(self):
        entry = _entry()
        bucket = {"entry": entry, "names": ["N"],
                  "outcomes": [_outcome(reason="timeout", delay_ms=None)]}
        # Local compatibility mode defaults to minimum_completed_probes=1: one
        # probe (itself) is enough to settle a node either way.
        cfg = _cfg(aggregation={"strategy": "any_alive",
                                "minimum_completed_probes": 1,
                                "late_result_seconds": 90})
        with _Ledger():
            _summary, round_id = self._run(cfg, [bucket], [_proxy()])
            node = engine.db.get_node("air", entry["fp"])
            self.assertEqual(node["status"], policy.UNKNOWN)
            self.assertEqual(node["consec_fail"], 1)
            self.assertEqual(node["last_reason"], "timeout")
            row = engine.db.one("SELECT verdict, reason FROM results WHERE round_id=?",
                                (round_id,))
            self.assertEqual((row["verdict"], row["reason"]), ("fail", "timeout"))

    def test_one_live_probe_rescues_a_node_the_local_probe_failed(self):
        """§4.3：任一探测点成功即活——本地失败不改判。"""
        entry = _entry()
        bucket = {"entry": entry, "names": ["N"],
                  "outcomes": [_outcome(reason="timeout", delay_ms=None)]}
        cfg = _cfg(probes=[{"id": "hk3-a", "name": "HK3 A", "enabled": True}])
        with _Ledger():
            round_id = db.start_round("test")
            # The probe answered and succeeded; the Controller's own
            # measurement failed. `minimum_completed_probes` is 2 here, and
            # exactly 2 probes completed, so the verdict is conclusive.
            db.record_observation(round_id, "hk3-a", entry["fp"], True, 95, None, "HK",
                                  engine.config_digest(cfg))
            summary = engine._apply_and_publish(
                cfg, round_id, None, {"N": entry}, [_proxy()],
                {"N": _outcome(reason="timeout", delay_ms=None)}, {}, cfg["sources"],
                _nolog)
            self.assertEqual(summary["alive"], 1)
            node = engine.db.get_node("air", entry["fp"])
            self.assertEqual(node["status"], policy.ALIVE)
            self.assertEqual(node["consec_fail"], 0)
            self.assertEqual(node["last_delay_ms"], 95)
            row = engine.db.one("SELECT verdict, delay_ms FROM results WHERE round_id=?",
                                (round_id,))
            self.assertEqual((row["verdict"], row["delay_ms"]), ("ok", 95))

    def test_every_probe_failing_advances_the_streak_once(self):
        """§4.3 末段：聚合完成后再调状态机，禁止每个探测点分别推进。"""
        entry = _entry()
        bucket = {"entry": entry, "names": ["N"],
                  "outcomes": [_outcome(reason="timeout", delay_ms=None)]}
        cfg = _cfg(probes=[{"id": "hk3-a", "enabled": True},
                           {"id": "hk3-b", "enabled": True}])
        with _Ledger():
            for expected in (1, 2):
                round_id = db.start_round("test")
                for probe_id in ("hk3-a", "hk3-b"):
                    db.record_observation(round_id, probe_id, entry["fp"], False,
                                          None, "timeout", None,
                                          engine.config_digest(cfg))
                engine._apply_and_publish(
                    cfg, round_id, None, {"N": entry}, [_proxy()],
                    {"N": _outcome(reason="timeout", delay_ms=None)}, {},
                    cfg["sources"], _nolog)
                node = engine.db.get_node("air", entry["fp"])
                self.assertEqual(node["consec_fail"], expected,
                                 "two probes advanced the streak twice in one round")

    def test_too_few_probes_leaves_the_streak_alone(self):
        """§14：结果不足 minimum_completed_probes → 不推进 streak。

        Three probes configured, one answered and failed. The node keeps
        whatever the previous round said -- this is the guard that stops
        "探测点越多，节点死得越快".
        """
        entry = _entry()
        cfg = _cfg(probes=[{"id": "hk3-a", "enabled": True},
                           {"id": "hk3-b", "enabled": True},
                           {"id": "sg-a", "enabled": True}])
        with _Ledger():
            db.upsert_node("air", entry["fp"], "N", status=policy.ALIVE,
                           consec_fail=0, total_ok=9)
            round_id = db.start_round("test")
            db.record_observation(round_id, "hk3-a", entry["fp"], False, None,
                                  "timeout", None, engine.config_digest(cfg))
            summary = engine._apply_and_publish(
                cfg, round_id, None, {"N": entry}, [_proxy()],
                {"N": _outcome(reason="timeout", delay_ms=None)}, {},
                cfg["sources"], _nolog)
            node = engine.db.get_node("air", entry["fp"])
            self.assertEqual(node["status"], policy.ALIVE,
                             "an inconclusive round moved the node's status")
            self.assertEqual(node["consec_fail"], 0,
                             "an inconclusive round advanced the streak")
            row = engine.db.one("SELECT verdict, reason FROM results WHERE round_id=?",
                                (round_id,))
            self.assertEqual((row["verdict"], row["reason"]),
                             (aggregation.VERDICT_INCONCLUSIVE,
                              aggregation.INSUFFICIENT_PROBES))
            # The node is counted in the round but is not alive.
            self.assertEqual(summary["alive"], 0)
            self.assertEqual(summary["total"], 1)

    def test_an_inconclusive_node_still_keeps_its_display_columns(self):
        """An alive probe still rescues the node (指导 §4.3 rule order), but a
        *failing* node below the minimum keeps its status and streak while its
        display columns are refreshed."""
        entry = _entry()
        cfg = _cfg(probes=[{"id": "hk3-a", "enabled": True}])
        with _Ledger():
            db.upsert_node("air", entry["fp"], "N", status=policy.PENDING,
                           consec_fail=2, proto="vless", server="old.example")
            round_id = db.start_round("test")
            engine._apply_and_publish(
                cfg, round_id, None, {"N": entry}, [_proxy()],
                {"N": _outcome(reason="timeout", delay_ms=None)}, {},
                cfg["sources"], _nolog)
            node = engine.db.get_node("air", entry["fp"])
            self.assertEqual(node["status"], policy.PENDING)
            self.assertEqual(node["consec_fail"], 2)
            # The display columns are refreshed; the convergence columns are not.
            self.assertEqual(node["server"], "host.example")
            self.assertEqual(node["proto"], "vless")

    def test_a_stale_observation_is_refused_when_the_engine_is_the_only_gate(self):
        """指导 §4.2：digest 不符的观测只入审计，不推进死亡计数。

        本地单探测点兼容模式（或 P3b 未部署）下引擎自己是唯一的门禁，所以
        这里的 digest 不符必须被拒，本地测量照常落库并生效。
        """
        entry = _entry()
        cfg = _cfg(probes=[{"id": "hk3-a", "enabled": True}])
        with _Ledger():
            db.upsert_node("air", entry["fp"], "N", status=policy.ALIVE, consec_fail=0)
            round_id = db.start_round("test")
            # A probe answering for a config this Controller no longer holds.
            db.record_observation(round_id, "hk3-a", entry["fp"], False, None,
                                  "timeout", None, "sha256:stale")
            with unittest.mock.patch.object(engine, "_digest_gate", lambda: None):
                summary = engine._apply_and_publish(
                    cfg, round_id, None, {"N": entry}, [_proxy()],
                    {"N": _outcome()}, {}, cfg["sources"], _nolog)
            # Only the local observation survived the filter, and it says alive.
            self.assertEqual(summary["alive"], 1)
            self.assertEqual(engine.db.get_node("air", entry["fp"])["consec_fail"], 0)

    def test_an_unverifiable_payload_digest_is_deferred_to_the_api_gate(self):
        """P3b 在 payload_digest 里存的是整个 ProbeResult 的哈希，引擎无法重算；
        `verify_result` 已经在 API 侧比对过报文自带的 config_digest（指导 §4.2）。
        此时引擎采用该观测，而不是把它当成 digest 不符全部拒掉——否则每一条
        远程观测都会被拒，多探测点等于没接。
        """
        entry = _entry()
        cfg = _cfg(probes=[{"id": "hk3-a", "enabled": True}],
                   aggregation={"strategy": "any_alive",
                                "minimum_completed_probes": 1,
                                "late_result_seconds": 90})
        with _Ledger():
            db.upsert_node("air", entry["fp"], "N", status=policy.PENDING, consec_fail=2)
            round_id = db.start_round("test")
            # The probe said alive, with a payload digest the engine cannot
            # recompute -- and the Controller's own measurement said timeout.
            db.record_observation(round_id, "hk3-a", entry["fp"], True, 61, None,
                                  "HK", "sha256:9c2a...payload-hash")
            summary = engine._apply_and_publish(
                cfg, round_id, None, {"N": entry}, [_proxy()],
                {"N": _outcome(reason="timeout", delay_ms=None)}, {},
                cfg["sources"], _nolog)
            self.assertEqual(summary["alive"], 1)
            node = engine.db.get_node("air", entry["fp"])
            self.assertEqual(node["status"], policy.ALIVE)
            self.assertEqual(node["consec_fail"], 0)
            self.assertEqual(node["last_delay_ms"], 61)

    def test_an_unknown_probe_is_audited_not_applied(self):
        entry = _entry()
        cfg = _cfg(probes=[{"id": "hk3-a", "enabled": True}])
        with _Ledger():
            round_id = db.start_round("test")
            db.record_observation(round_id, "rogue", entry["fp"], False, None,
                                  "timeout")
            summary = engine._apply_and_publish(
                cfg, round_id, None, {"N": entry}, [_proxy()],
                {"N": _outcome()}, {}, cfg["sources"], _nolog)
            self.assertEqual(summary["alive"], 1)
            self.assertIsNone(engine.db.get_node("air", entry["fp"])["last_reason"])


class DispatchHookTest(unittest.TestCase):
    """契约：`_dispatch_probe_jobs` 是钩子，缺模块或抛错都不能让轮次失败。"""

    # P3b's probe_dispatch.py is a real module now, so "missing" has to be
    # simulated on both paths `from . import probe_dispatch` can take: the
    # `mihomo_test` package attribute (which a successful import sets, and
    # which then shadows whatever sys.modules holds) and sys.modules itself.
    class _Unimportable:
        """Binds as the module; touching any attribute raises ImportError."""

        def __getattr__(self, name):
            raise ImportError("probe_dispatch is blocked for this test")

    def _patch_missing(self):
        import mihomo_test
        return unittest.mock.patch.multiple(
            mihomo_test, probe_dispatch=self._Unimportable(), create=True), \
            unittest.mock.patch.dict(
                sys.modules, {"mihomo_test.probe_dispatch": None})

    def test_a_dispatch_module_that_cannot_be_imported_degrades_to_local_only(self):
        calls = []
        patches = self._patch_missing()
        for patch in patches:
            patch.start()
        try:
            out = engine._dispatch_probe_jobs(_cfg(), 1, [], lambda l, m: calls.append(m))
        finally:
            for patch in reversed(patches):
                patch.stop()
        self.assertEqual(out, {})
        self.assertTrue(any("probe_dispatch" in m for m in calls),
                        f"no warning was logged: {calls}")

    def test_a_raising_probe_dispatch_does_not_fail_the_round(self):
        import types

        import mihomo_test
        module = types.ModuleType("mihomo_test.probe_dispatch")

        def boom(*_a, **_k):
            raise RuntimeError("probe transport is down")

        module.dispatch_round = boom
        calls = []
        with unittest.mock.patch.object(mihomo_test, "probe_dispatch", module,
                                        create=True), \
                unittest.mock.patch.dict(sys.modules,
                                         {"mihomo_test.probe_dispatch": module}):
            out = engine._dispatch_probe_jobs(
                _cfg(), 1, [], lambda l, m: calls.append(m))
        self.assertEqual(out, {})
        self.assertTrue(any("不影响本轮" in m for m in calls), calls)

    def test_the_real_dispatch_module_receives_the_jobs(self):
        """The hook forwards (cfg, round_id, jobs, log) verbatim."""
        import types

        import mihomo_test
        module = types.ModuleType("mihomo_test.probe_dispatch")
        seen = []

        def dispatch_round(cfg, round_id, jobs, log):
            seen.append((cfg, round_id, jobs, log))
            return {"hk3-a": "ok"}

        module.dispatch_round = dispatch_round
        jobs = [{"round_id": "9", "probe_id": "hk3-a"}]
        with unittest.mock.patch.object(mihomo_test, "probe_dispatch", module,
                                        create=True), \
                unittest.mock.patch.dict(sys.modules,
                                         {"mihomo_test.probe_dispatch": module}):
            out = engine._dispatch_probe_jobs(_cfg(), 9, jobs, _nolog)
        self.assertEqual(out, {"hk3-a": "ok"})
        self.assertEqual(seen[0][1], 9)
        self.assertEqual(seen[0][2], jobs)

    def test_the_dispatch_result_is_parsed_for_the_probes_that_took_the_job(self):
        self.assertEqual(
            engine._dispatched_probe_ids({"dispatched": ["hk3-a"]}, ["hk3-a", "hk3-b"]),
            {"hk3-a"})
        self.assertEqual(
            engine._dispatched_probe_ids({"probes": {"hk3-a": {}, "hk3-b": {}}},
                                         ["hk3-a", "hk3-b"]),
            {"hk3-a", "hk3-b"})
        self.assertEqual(engine._dispatched_probe_ids(["hk3-a"], ["hk3-a"]), {"hk3-a"})
        self.assertEqual(engine._dispatched_probe_ids({}, ["hk3-a"]), set())
        self.assertEqual(engine._dispatched_probe_ids(None, ["hk3-a"]), set())
        # probe_dispatch.dispatch_round's actual shape: {probe_id: "ok"|"err"}.
        self.assertEqual(
            engine._dispatched_probe_ids({"hk3-a": "ok", "hk3-b": "err"},
                                         ["hk3-a", "hk3-b"]),
            {"hk3-a"}, "a probe whose dispatch failed must not be waited for")
        self.assertEqual(
            engine._dispatched_probe_ids({"hk3-a": "err"}, ["hk3-a"]), set())
        self.assertEqual(
            engine._dispatched_probe_ids({"ok": 2, "failed": ["hk3-b"]},
                                         ["hk3-a", "hk3-b"]),
            {"hk3-a"})
        # An unknown status word counts as sent: waiting costs time, not
        # waiting silently drops a probe's observations from the round.
        self.assertEqual(
            engine._dispatched_probe_ids({"hk3-a": "queued"}, ["hk3-a"]), {"hk3-a"})


class CollectObservationsTest(unittest.TestCase):
    """`_run_round` 的插入点：下发 Job → 等回传 → 落本地观测。"""

    ENTRY = _entry()
    PROXY = _proxy()

    def _cfg(self, **over):
        base = {"probes": [{"id": "hk3-a", "name": "HK3 A", "enabled": True}],
                "probe_policy": {"interval_minutes": 30},
                "aggregation": dict(AGG)}
        base.update(over)
        return _cfg(**base)

    def test_local_only_mode_records_one_observation_per_node(self):
        """`probes == []`：不下发、不等待，只有一条 local 观测。"""
        cfg = _cfg()
        with _Ledger():
            round_id = db.start_round("test")
            seen = []
            with unittest.mock.patch.object(
                    engine, "_dispatch_probe_jobs",
                    lambda *a, **k: seen.append(a) or {}):
                out = engine._collect_probe_observations(
                    cfg, round_id, {"N": self.ENTRY}, [self.PROXY],
                    {"N": _outcome(delay_ms=77)}, {}, None, _nolog)
            self.assertEqual(seen, [], "本地单探测点模式不该下发 Job")
            self.assertEqual(out["local"], 1)
            rows = db.get_observations(round_id)
            self.assertEqual([r["probe_id"] for r in rows], ["local"])
            self.assertEqual(rows[0]["delay_ms"], 77)

    def test_the_job_shape_follows_the_contract(self):
        """指导 §4.1：round_id / probe_id / config_digest / deadline /
        chain_enabled / nodes[fingerprint, mihomo_name, proxy]。"""
        cfg = self._cfg()
        with _Ledger():
            jobs = engine._probe_jobs(cfg, 42, {"N": self.ENTRY}, [self.PROXY],
                                      "2026-11-05T02:00:00")
        self.assertEqual(len(jobs), 1)
        job = jobs[0]
        self.assertEqual(job["round_id"], "42")
        self.assertEqual(job["probe_id"], "hk3-a")
        self.assertEqual(job["config_digest"], engine.config_digest(cfg))
        self.assertTrue(job["config_digest"].startswith("sha256:"))
        self.assertEqual(job["deadline"], "2026-11-05T02:00:00")
        self.assertFalse(job["chain_enabled"])
        self.assertEqual(job["nodes"], [{"fingerprint": self.ENTRY["fp"],
                                         "mihomo_name": "N",
                                         "proxy": self.PROXY}])

    def test_chain_enabled_rides_along_on_the_job(self):
        cfg = self._cfg(chain={"enabled": True})
        with _Ledger():
            jobs = engine._probe_jobs(cfg, 1, {"N": self.ENTRY}, [self.PROXY], "x")
        self.assertTrue(jobs[0]["chain_enabled"])

    def test_a_probe_result_is_collected_and_the_local_one_too(self):
        """探测点回传被等待并落库，本地观测随后落下，两者都在。"""
        cfg = self._cfg()
        with _Ledger():
            round_id = db.start_round("test")
            digest = engine.config_digest(cfg)

            def fake_dispatch(_cfg, _round_id, jobs, _log):
                # Simulates the probe agent answering before the poll's first
                # pass -- exactly what a fast probe does.
                for job in jobs:
                    db.record_observation(_round_id, job["probe_id"],
                                          self.ENTRY["fp"], True, 60, None, "HK",
                                          digest)
                return {"dispatched": [j["probe_id"] for j in jobs]}

            with unittest.mock.patch.object(engine, "_dispatch_probe_jobs",
                                            fake_dispatch):
                out = engine._collect_probe_observations(
                    cfg, round_id, {"N": self.ENTRY}, [self.PROXY],
                    {"N": _outcome(delay_ms=310)}, {},
                    time.monotonic() + 600, _nolog)
            self.assertEqual(out["collected"], ["hk3-a"])
            self.assertEqual(out["local"], 1)
            rows = {r["probe_id"]: r for r in db.get_observations(round_id)}
            self.assertEqual(sorted(rows), ["hk3-a", "local"])
            self.assertEqual(rows["hk3-a"]["delay_ms"], 60)
            self.assertEqual(rows["local"]["delay_ms"], 310)

    def test_a_probe_that_never_answers_is_not_a_node_failure(self):
        """§4.3：探测点未回传 = 观测缺失，聚合为 inconclusive。"""
        cfg = _cfg(probes=[{"id": "hk3-a", "enabled": True},
                           {"id": "hk3-b", "enabled": True}])
        with _Ledger():
            round_id = db.start_round("test")
            with unittest.mock.patch.object(
                    engine, "_dispatch_probe_jobs",
                    lambda *a, **k: {"dispatched": ["hk3-a"]}):
                out = engine._collect_probe_observations(
                    cfg, round_id, {"N": self.ENTRY}, [self.PROXY],
                    {"N": _outcome(reason="timeout", delay_ms=None)}, {},
                    # Two seconds of budget, 60s of it reserved: the wait is
                    # skipped rather than blocking the test for an interval.
                    time.monotonic() + 2.0, _nolog)
            # hk3-a took the job and never answered; hk3-b was never asked.
            self.assertEqual(out["collected"], [])
            self.assertEqual(out["local"], 1)
            aggregated = aggregation.aggregate_round(
                db.get_observations(round_id), cfg["aggregation"])
            self.assertEqual(aggregated[self.ENTRY["fp"]]["verdict"],
                             aggregation.VERDICT_INCONCLUSIVE)

    def test_the_wait_is_clamped_to_the_round_budget(self):
        """A slow probe must not get the round killed by its own watchdog."""
        cfg = self._cfg(probe_policy={"interval_minutes": 30})
        with _Ledger():
            round_id = db.start_round("test")
            with unittest.mock.patch.object(
                    engine, "_dispatch_probe_jobs",
                    lambda *a, **k: {"dispatched": ["hk3-a"]}):
                engine._collect_probe_observations(
                    cfg, round_id, {"N": self.ENTRY}, [self.PROXY],
                    {"N": _outcome()}, {}, time.monotonic() + 0.0, _nolog)
            # With no budget left the wait is skipped entirely rather than
            # blocking; the local observation still lands.
            self.assertEqual(len(db.get_observations(round_id)), 1)

    def test_nothing_dispatched_skips_the_wait_entirely(self):
        cfg = self._cfg()
        with _Ledger():
            round_id = db.start_round("test")
            with unittest.mock.patch.object(engine, "_dispatch_probe_jobs",
                                            lambda *a, **k: {}):
                out = engine._collect_probe_observations(
                    cfg, round_id, {"N": self.ENTRY}, [self.PROXY],
                    {"N": _outcome()}, {}, None, _nolog)
            self.assertEqual(out["collected"], [])
            self.assertEqual(out["local"], 1)

    def test_a_collection_failure_never_fails_the_round(self):
        """The observations are an optimisation over the local measurement."""
        cfg = self._cfg()
        with _Ledger():
            round_id = db.start_round("test")
            with unittest.mock.patch.object(
                    engine, "_dispatch_probe_jobs",
                    side_effect=RuntimeError("probe transport is down")):
                out = engine._collect_probe_observations(
                    cfg, round_id, {"N": self.ENTRY}, [self.PROXY],
                    {"N": _outcome()}, {}, None, _nolog)
            self.assertEqual(out, {"dispatched": [], "collected": [], "local": 0})


class FrontDeadDoesNotKillTheNodeTest(unittest.TestCase):
    """指导 §5.4 / §7.5：前置失败不推进目标节点死亡计数。"""

    def _entry(self, **over):
        entry = {"source": "air", "fp": "a" * 16, "original": "N", "mihomo": "N",
                 "proto": "vless", "server": "t.example", "role": "chain"}
        entry.update(over)
        return entry

    def test_the_streak_and_status_survive_a_dead_front(self):
        with _Ledger():
            db.upsert_node("air", "a" * 16, "N", status=policy.ALIVE, consec_fail=2)
            round_id = db.start_round("test")
            engine._record_chain_failures(
                _cfg(), round_id, [self._entry()], _nolog)
            node = engine.db.get_node("air", "a" * 16)
            self.assertEqual(node["status"], policy.ALIVE)
            self.assertEqual(node["consec_fail"], 2)
            self.assertIsNone(node["last_reason"])
            row = engine.db.one("SELECT verdict, reason, category FROM results"
                                " WHERE round_id=?", (round_id,))
            self.assertEqual((row["verdict"], row["reason"], row["category"]),
                             ("fail", engine.FRONT_DEAD_REASON, engine.CAT_CHAIN))

    def test_a_node_on_the_threshold_is_not_pushed_over_it(self):
        """consec_fail=2 with a threshold of 3: the third front-dead round must
        not be the one that kills it."""
        with _Ledger():
            db.upsert_node("air", "a" * 16, "N", status=policy.PENDING, consec_fail=2)
            round_id = db.start_round("test")
            engine._record_chain_failures(
                _cfg(), round_id, [self._entry()], _nolog)
            node = engine.db.get_node("air", "a" * 16)
            self.assertEqual(node["status"], policy.PENDING)
            self.assertEqual(node["consec_fail"], 2)

    def test_the_row_survives_the_prune(self):
        """The fingerprints handed back keep the row from being deleted."""
        with _Ledger():
            round_id = db.start_round("test")
            _by_source, fps = engine._record_chain_failures(
                _cfg(), round_id, [self._entry()], _nolog)
            tested = {"source": "air", "fp": "b" * 16, "original": "M",
                      "mihomo": "M", "test_ip": "m.example"}
            engine._prune_removed_nodes({"M": tested}, {}, {}, _nolog,
                                        chain_failed=fps)
            self.assertIsNotNone(engine.db.get_node("air", "a" * 16))

    def test_a_dead_front_round_writes_no_chain_observation(self):
        """chain 默认关闭时无链式观测：前置死只写 results 行，不进观测层。"""
        with _Ledger():
            round_id = db.start_round("test")
            engine._record_chain_failures(
                _cfg(chain={"enabled": False}), round_id, [self._entry()], _nolog)
            self.assertEqual(db.get_observations(round_id), [])

    def test_a_dead_front_does_not_stop_the_node_from_recovering(self):
        """A real success on a later round still restores it immediately."""
        with _Ledger():
            db.upsert_node("air", "a" * 16, "N", status=policy.PENDING, consec_fail=2)
            round_id = db.start_round("test")
            engine._record_chain_failures(
                _cfg(), round_id, [self._entry()], _nolog)
            fields, transition = policy.apply(
                engine.db.get_node("air", "a" * 16), True, 40, None,
                {"drop_after_consecutive_fails": 3})
            self.assertEqual(transition, None)
            self.assertEqual(fields["status"], policy.ALIVE)
            self.assertEqual(fields["consec_fail"], 0)


class ConfigDigestTest(unittest.TestCase):
    """`config_digest` 供 Job 与 API 校验共用（P3b import 它）。"""

    def test_it_is_stable_and_prefixed(self):
        cfg = _cfg()
        self.assertEqual(engine.config_digest(cfg), engine.config_digest(cfg))
        self.assertTrue(engine.config_digest(cfg).startswith("sha256:"))
        self.assertEqual(len(engine.config_digest(cfg)), len("sha256:") + 64)

    def test_a_probe_relevant_change_moves_it(self):
        cfg = _cfg()
        self.assertNotEqual(engine.config_digest(cfg),
                            engine.config_digest(_cfg(aggregation={
                                "strategy": "any_alive",
                                "minimum_completed_probes": 3})))

    def test_an_export_switch_does_not_move_it(self):
        """A node's verdict cannot change because a publish switch moved, so
        hashing `publish` would invalidate every in-flight probe for nothing."""
        cfg = _cfg()
        self.assertEqual(engine.config_digest(cfg),
                         engine.config_digest(_cfg(publish={"enabled": False})))

    def test_key_order_does_not_move_it(self):
        one = {"policy": {"a": 1, "b": 2}, "verify": {"x": 1}}
        two = {"verify": {"x": 1}, "policy": {"b": 2, "a": 1}}
        self.assertEqual(engine.config_digest(one), engine.config_digest(two))

    def test_a_missing_config_does_not_raise(self):
        self.assertTrue(engine.config_digest({}).startswith("sha256:"))
        self.assertTrue(engine.config_digest(None).startswith("sha256:"))


if __name__ == "__main__":
    unittest.main()
