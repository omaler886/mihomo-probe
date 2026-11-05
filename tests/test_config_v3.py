"""HK3 schema v3: the unique config model, the one-shot migration, the tables.

Covers 指导 §3 (唯一配置模型与配置验证) and §12 (数据库迁移建议); the
contract itself is workstreams/20_HK3_LITE_PLAN.md §4.1/§4.2. Follows the
tests/_isolation.py pattern so nothing here can touch a real data/ directory.
"""
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import config as cfgmod
from mihomo_test import db

try:
    import _isolation
except ImportError:  # imported as a package: python -m unittest tests.test_config_v3
    from tests import _isolation


def setUpModule():
    _isolation.isolate()


def tearDownModule():
    _isolation.restore()


def _v2_config():
    """A config.json as it looked before schema v3: none of the v3 keys."""
    return {
        "sources": [{"key": "air", "kind": "collection", "name": "air",
                     "label": "air", "enabled": True}],
        "test": {"timeout_ms": 7000, "concurrency": 42,
                 "targets": ["https://www.gstatic.com/generate_204"]},
        "policy": {"drop_after_consecutive_fails": 5, "suspect_floor_ratio": 0.4,
                   "suspect_floor_absolute": 4},
        "schedule": {"interval_minutes": 15},
        "chain": {"front_text": "vless://example", "max_fronts": 8},
        "substore": {"backend": "http://127.0.0.1:3000"},
    }


class ConfigV3DefaultsTest(unittest.TestCase):
    """DEFAULTS is the v3 shape; a fresh install must boot straight into it."""

    def test_defaults_declare_the_v3_keys(self):
        d = cfgmod.DEFAULTS
        self.assertEqual(d["schema_version"], 3)
        self.assertEqual(d["probes"], [])
        self.assertIs(d["chain"]["enabled"], False)
        self.assertIs(d["substore"]["manual_push_only"], True)
        self.assertEqual(d["substore"]["target_name"], "mihomo-probe-alive")
        self.assertEqual(d["probe_policy"], {
            "interval_minutes": 30, "timeout_ms": 5000, "concurrency": 20,
            "dead_after_consecutive_failures": 3, "round_guard_ratio": 0.5,
            "round_guard_min_alive": 3})
        self.assertEqual(d["aggregation"], {
            "strategy": "any_alive", "minimum_completed_probes": 1,
            "late_result_seconds": 90})
        self.assertEqual(d["rename"], {
            "enabled": True, "template": "{country} | {display_name}",
            "collision_suffix": " #{n}", "preserve_original_name": True})
        self.assertEqual(d["gist"], {
            "enabled": True, "gist_id": "", "token_env": "GITHUB_GIST_TOKEN",
            "visibility": "secret", "canonical_file": "mihomo-alive.yaml",
            "status_file": "probe-status.json",
            "history_file": "probe-history.jsonl"})

    def test_fresh_install_loads_as_v3(self):
        cfg = cfgmod.load()
        self.assertEqual(cfg["schema_version"], 3)
        self.assertEqual(cfg["probes"], [])
        self.assertIs(cfg["chain"]["enabled"], False)
        self.assertIs(cfg["substore"]["manual_push_only"], True)
        on_disk = json.loads(cfgmod.CONFIG_PATH.read_text(encoding="utf-8"))
        self.assertEqual(on_disk["schema_version"], 3)
        self.assertEqual(on_disk["probes"], [])
        self.assertIs(on_disk["substore"]["manual_push_only"], True)

    def test_chain_enabled_missing_is_forced_false(self):
        """指导 §3: 缺失强制 false；与 normalize_chain 的 `is True` 语义一致。"""
        self.assertIs(cfgmod.normalize_chain({})["enabled"], False)
        self.assertIs(cfgmod.normalize_chain({"enabled": "yes"})["enabled"], False)
        self.assertIs(cfgmod.normalize_chain({"enabled": 1})["enabled"], False)
        self.assertIs(cfgmod.normalize_chain({"enabled": True})["enabled"], True)
        forced = cfgmod.validate_config_v3(
            {"substore": {"manual_push_only": True}, "chain": {"enabled": "yes"}})
        self.assertIs(forced["chain"]["enabled"], False)
        forced = cfgmod.validate_config_v3({"substore": {"manual_push_only": True}})
        self.assertIs(forced["chain"]["enabled"], False)


class ValidateConfigV3Test(unittest.TestCase):
    """One gate for startup / save / pre-test (指导 §3 配置验证)。"""

    def _cfg(self, **over):
        cfg = {"substore": {"manual_push_only": True}, "probes": []}
        cfg.update(over)
        return cfg

    def test_manual_push_only_false_is_refused(self):
        with self.assertRaises(ValueError):
            cfgmod.validate_config_v3({"substore": {"manual_push_only": False}})
        # 缺失同样拒绝：一个没有表态的配置不能被解释成"允许自动推送"
        with self.assertRaises(ValueError):
            cfgmod.validate_config_v3({})

    def test_duplicate_probe_id_is_refused(self):
        with self.assertRaises(ValueError):
            cfgmod.validate_config_v3(
                self._cfg(probes=[{"id": "hk3-a"}, {"id": "hk3-a"}]))

    def test_illegal_probe_id_is_refused(self):
        for bad in ("", "a", "HK3", "hk3 a", "hk3/a", "-hk3", "x" * 33,
                    42, None):
            with self.assertRaises(ValueError, msg=repr(bad)):
                cfgmod.validate_config_v3(self._cfg(probes=[{"id": bad}]))

    def test_token_env_must_be_an_environment_variable_name(self):
        for bad in ("ghp_plaintext", "my-token", "github token", "3TOKEN", 42):
            with self.assertRaises(ValueError, msg=repr(bad)):
                cfgmod.validate_config_v3(
                    self._cfg(probes=[{"id": "hk3-a", "token_env": bad}]))
        # gist.token_env 同一条规则
        with self.assertRaises(ValueError):
            cfgmod.validate_config_v3(
                self._cfg(gist={"token_env": "not a env name"}))

    def test_plaintext_token_in_a_probe_is_refused(self):
        for key in ("token", "secret", "api_key", "password"):
            with self.assertRaises(ValueError, msg=key):
                cfgmod.validate_config_v3(
                    self._cfg(probes=[{"id": "hk3-a", key: "x"}]))

    def test_valid_probes_pass(self):
        cfg = self._cfg(probes=[
            {"id": "hk3-a", "name": "HK3 主探测点", "enabled": True,
             "endpoint": "https://probe-a.example.com",
             "token_env": "PROBE_HK3_A_TOKEN", "weight": 100,
             "tags": ["HK", "HK3"]},
            {"id": "probe_sg-2", "name": "SG 备用", "enabled": False},
        ])
        out = cfgmod.validate_config_v3(cfg)
        self.assertEqual(len(out["probes"]), 2)

    def test_dead_threshold_derives_from_policy(self):
        out = cfgmod.validate_config_v3(
            {"substore": {"manual_push_only": True},
             "policy": {"drop_after_consecutive_fails": 7}})
        self.assertEqual(
            out["probe_policy"]["dead_after_consecutive_failures"], 7)
        out = cfgmod.validate_config_v3({"substore": {"manual_push_only": True}})
        self.assertEqual(
            out["probe_policy"]["dead_after_consecutive_failures"], 3)


class MigrateConfigV2ToV3Test(unittest.TestCase):
    """The one-shot v2 -> v3 migration (指导 §3/§15 Phase 2)。"""

    def setUp(self):
        # 每个迁移用例从干净的文件开始：隔离目录在本模块内是共享的，上一个
        # 用例留下的 config.json（可能是被拒绝的配置）不属于这一个用例。
        cfgmod.CONFIG_PATH.unlink(missing_ok=True)
        for bak in cfgmod.DATA.glob("config.json.bak-v3-*"):
            bak.unlink()
        cfgmod._last_migration.clear()

    def test_migration_fills_new_keys_and_keeps_old_values(self):
        new, report = cfgmod.migrate_config_v2_to_v3(_v2_config())
        self.assertEqual(new["schema_version"], 3)
        self.assertEqual(new["probes"], [])
        self.assertEqual(new["probe_policy"], {
            "interval_minutes": 15, "timeout_ms": 7000, "concurrency": 42,
            "dead_after_consecutive_failures": 5, "round_guard_ratio": 0.4,
            "round_guard_min_alive": 4})
        self.assertEqual(new["aggregation"], cfgmod.DEFAULTS["aggregation"])
        self.assertEqual(new["rename"], cfgmod.DEFAULTS["rename"])
        self.assertEqual(new["gist"], cfgmod.DEFAULTS["gist"])
        # chain 前置配置本身保留，只是开关回到关闭
        self.assertIs(new["chain"]["enabled"], False)
        self.assertEqual(new["chain"]["front_text"], "vless://example")
        self.assertEqual(new["chain"]["max_fronts"], 8)
        self.assertIs(new["substore"]["manual_push_only"], True)
        self.assertEqual(new["substore"]["target_name"], "mihomo-probe-alive")
        self.assertEqual(new["substore"]["backend"], "http://127.0.0.1:3000")
        # 旧键原值保留，不清零
        self.assertEqual(new["test"]["timeout_ms"], 7000)
        self.assertEqual(new["test"]["concurrency"], 42)
        self.assertEqual(new["policy"]["drop_after_consecutive_fails"], 5)
        self.assertEqual(new["schedule"]["interval_minutes"], 15)
        self.assertEqual(new["sources"][0]["key"], "air")
        self.assertTrue(report)
        self.assertTrue(any("chain" in line for line in report))

    def test_migration_is_idempotent(self):
        once, _ = cfgmod.migrate_config_v2_to_v3(_v2_config())
        twice, report = cfgmod.migrate_config_v2_to_v3(once)
        self.assertEqual(twice, once)
        self.assertEqual(report, [])

    def test_load_migrates_a_v2_file_and_backs_it_up_once(self):
        cfgmod.CONFIG_PATH.write_text(json.dumps(_v2_config()), encoding="utf-8")
        cfg = cfgmod.load()
        self.assertEqual(cfg["schema_version"], 3)
        self.assertEqual(cfg["probe_policy"]["timeout_ms"], 7000)
        self.assertEqual(cfg["policy"]["drop_after_consecutive_fails"], 5)
        # 迁移结果落盘：下次启动不再迁移
        on_disk = json.loads(cfgmod.CONFIG_PATH.read_text(encoding="utf-8"))
        self.assertEqual(on_disk["schema_version"], 3)
        self.assertEqual(on_disk["probe_policy"]["concurrency"], 42)
        # 备份恰好一个，且备份里是迁移前的旧配置
        backups = sorted(cfgmod.DATA.glob("config.json.bak-v3-*"))
        self.assertEqual(len(backups), 1)
        pre = json.loads(backups[0].read_text(encoding="utf-8"))
        self.assertNotIn("schema_version", pre)
        self.assertEqual(pre["test"]["timeout_ms"], 7000)
        # 迁移报告可被轮次消费一次
        report = cfgmod.take_migration_report()
        self.assertTrue(report)
        self.assertEqual(cfgmod.take_migration_report(), [])
        # 第二次 load 不再备份、不再报告
        cfgmod.load()
        self.assertEqual(len(list(cfgmod.DATA.glob("config.json.bak-v3-*"))), 1)
        self.assertEqual(cfgmod.take_migration_report(), [])

    def test_load_refuses_a_v3_config_with_the_push_gate_off(self):
        bad = cfgmod.migrate_config_v2_to_v3(_v2_config())[0]
        bad["substore"]["manual_push_only"] = False
        cfgmod.CONFIG_PATH.write_text(json.dumps(bad), encoding="utf-8")
        with self.assertRaises(ValueError):
            cfgmod.load()

    def test_update_refuses_a_forbidden_patch(self):
        cfgmod.load()
        with self.assertRaises(ValueError):
            cfgmod.update({"substore": {"manual_push_only": False}})


class _DbCase(unittest.TestCase):
    """Per-test throwaway ledger, the pattern DisabledSourceTest uses."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="db-v3-"))
        self._saved = (db.DB_PATH, db._conn)
        db.DB_PATH, db._conn = self.tmp / "state.db", None

    def tearDown(self):
        if db._conn is not None:
            db._conn.close()
        db.DB_PATH, db._conn = self._saved
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _tables(self, path=None):
        conn = sqlite3.connect(path or self.tmp / "state.db")
        try:
            return {row[0] for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table'")}
        finally:
            conn.close()

    def _reconnect(self):
        if db._conn is not None:
            db._conn.close()
        db._conn = None
        return db.connect()


class DbV3TablesTest(_DbCase):
    """probes / probe_observations / publish_audit (指导 §12)。"""

    V3 = ("probes", "probe_observations", "publish_audit")

    def test_tables_are_created_idempotently(self):
        db.connect()
        self.assertTrue(set(self.V3) <= self._tables())
        # 首次 connect 落一次快照（指导 §12：加表前备份。规则按"文件非空"判定，
        # 新库此时也只有一页空库，4KB 副本，换取规则一行可审）。
        self.assertEqual(len(list(self.tmp.glob("state.db.bak-v3-*"))), 1)
        # 第二遍不炸、不重复备份
        self._reconnect()
        self.assertTrue(set(self.V3) <= self._tables())
        self.assertEqual(len(list(self.tmp.glob("state.db.bak-v3-*"))), 1)

    def test_nodes_ledger_survives_the_v3_migration(self):
        # 先建一套完整账本，再模拟"迁移前"的库：只留既有表
        db.upsert_node("air", "f1", "node-one", status="alive", consec_fail=0,
                       total_ok=9, total_fail=1)
        db.upsert_node("air", "f2", "node-two", status="pending", consec_fail=2,
                       total_ok=3, total_fail=2)
        before = (db.get_node("air", "f1"), db.get_node("air", "f2"))
        # 首次 connect 的空库快照在这里没有意义：删掉它，让重连时的快照对应
        # 真正的"迁移前账本"——与线上升级路径一致（运维的旧库没有 .bak-v3-*）。
        for bak in self.tmp.glob("state.db.bak-v3-*"):
            bak.unlink()
        raw = sqlite3.connect(self.tmp / "state.db")
        with raw:
            for table in self.V3:
                raw.execute(f"DROP TABLE {table}")
        raw.close()
        self.assertFalse(set(self.V3) & self._tables())

        # 重连：迁移必须备份一次并重建三张表，nodes 一行不动
        self._reconnect()
        backups = list(self.tmp.glob("state.db.bak-v3-*"))
        self.assertEqual(len(backups), 1)
        self.assertTrue(set(self.V3) <= self._tables())
        after = (db.get_node("air", "f1"), db.get_node("air", "f2"))
        for old, new in zip(before, after):
            for field in ("status", "consec_fail", "total_ok", "total_fail",
                          "display", "fingerprint"):
                self.assertEqual(old[field], new[field], field)
        # 备份里是迁移前的库：有 nodes，没有三张新表
        pre_tables = self._tables(backups[0])
        self.assertIn("nodes", pre_tables)
        self.assertFalse(set(self.V3) & pre_tables)
        # 再跑一遍依旧幂等，备份不增加
        self._reconnect()
        self.assertEqual(len(list(self.tmp.glob("state.db.bak-v3-*"))), 1)

    def test_upsert_and_touch_probe(self):
        db.connect()
        db.upsert_probe("hk3-a", "HK3 主探测点", enabled=True,
                        version="1.2.3", state="online")
        row = db.one("SELECT * FROM probes WHERE probe_id=?", ("hk3-a",))
        self.assertEqual(row["name"], "HK3 主探测点")
        self.assertEqual(row["enabled"], 1)
        self.assertEqual(row["version"], "1.2.3")
        self.assertEqual(row["state"], "online")
        self.assertTrue(row["last_seen_at"])
        # 配置重同步不得重置 state / 抹掉 version
        db.upsert_probe("hk3-a", "HK3 主探测点", enabled=True)
        row = db.one("SELECT * FROM probes WHERE probe_id=?", ("hk3-a",))
        self.assertEqual(row["state"], "online")
        self.assertEqual(row["version"], "1.2.3")
        # 心跳：已知探测点返回 1
        self.assertEqual(db.touch_probe_seen("hk3-a", version="1.2.4"), 1)
        row = db.one("SELECT * FROM probes WHERE probe_id=?", ("hk3-a",))
        self.assertEqual(row["version"], "1.2.4")
        # 未知探测点只碰 0 行：审计即可，不算节点失败（指导 §4.2）
        self.assertEqual(db.touch_probe_seen("ghost-probe"), 0)
        # 禁用探测点
        db.upsert_probe("hk3-b", "备用探测点", enabled=False)
        row = db.one("SELECT * FROM probes WHERE probe_id=?", ("hk3-b",))
        self.assertEqual(row["enabled"], 0)
        self.assertEqual(row["state"], "unknown")

    def test_record_observation_is_idempotent(self):
        db.connect()
        db.record_observation("r1", "hk3-a", "f1", True, 180, None, "HK",
                              "sha256:aaa")
        db.record_observation("r1", "hk3-a", "f1", False, 5000, "timeout",
                              "HK", "sha256:aaa")
        rows = db.get_observations("r1")
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["alive"], 0)
        self.assertEqual(rows[0]["delay_ms"], 5000)
        self.assertEqual(rows[0]["reason"], "timeout")
        self.assertEqual(rows[0]["egress_country"], "HK")
        self.assertEqual(rows[0]["payload_digest"], "sha256:aaa")
        self.assertTrue(rows[0]["received_at"])
        # 换个探测点或指纹是另一行
        db.record_observation("r1", "hk3-b", "f1", True, 90, None, "HK",
                              "sha256:aaa")
        db.record_observation("r1", "hk3-a", "f2", True, 95, None, "SG",
                              "sha256:bbb")
        self.assertEqual(len(db.get_observations("r1")), 3)
        # 另一个轮次互不干扰
        self.assertEqual(db.get_observations("r2"), [])

    def test_publish_audit_round_trip(self):
        db.connect()
        db.record_publish_audit("a1", "substore", "mihomo-probe-alive", "rev-7",
                                "sha256:digest", "tester", "ok",
                                {"nodes": 12, "added": 2})
        rows = db.list_publish_audit(10)
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertEqual(row["audit_id"], "a1")
        self.assertEqual(row["kind"], "substore")
        self.assertEqual(row["target"], "mihomo-probe-alive")
        self.assertEqual(row["revision"], "rev-7")
        self.assertEqual(row["digest"], "sha256:digest")
        self.assertEqual(row["actor"], "tester")
        self.assertEqual(row["status"], "ok")
        self.assertEqual(json.loads(row["summary_json"]),
                         {"nodes": 12, "added": 2})
        self.assertTrue(row["created_at"])
        # 同一 audit_id 覆盖（重推同一审计）
        db.record_publish_audit("a1", "substore", "mihomo-probe-alive", "rev-8",
                                "sha256:digest2", "tester", "ok", {})
        rows = db.list_publish_audit(10)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["revision"], "rev-8")
        # limit 生效
        db.record_publish_audit("a2", "gist", "gist-1", None, "sha256:d2",
                                "scheduler", "ok", None)
        self.assertEqual(len(db.list_publish_audit(1)), 1)
        self.assertEqual(len(db.list_publish_audit(10)), 2)

    def test_validate_config_v3_does_not_touch_the_ledger(self):
        """配置层只校验不清账（指导 §3）：死亡阈值随配置走，streak 不动。"""
        db.connect()
        db.upsert_node("air", "f1", "n", status="pending", consec_fail=2)
        cfgmod.validate_config_v3(
            {"substore": {"manual_push_only": True},
             "policy": {"drop_after_consecutive_fails": 9}})
        node = db.get_node("air", "f1")
        self.assertEqual(node["consec_fail"], 2)
        self.assertEqual(node["status"], "pending")


if __name__ == "__main__":
    unittest.main()
