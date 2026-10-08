"""Tests for alerting, parallel lanes, the round watchdog, and export cleanup."""
import json
import os
import shutil
import sys
import threading
import time
import unittest
import unittest.mock
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import config as cfgmod
from mihomo_test import core as coremod
from mihomo_test import engine, notifier

try:
    import _isolation
except ImportError:  # imported as a package
    from tests import _isolation


def setUpModule():
    _isolation.isolate()


def tearDownModule():
    _isolation.restore()


class NotifierTest(unittest.TestCase):
    """Alerts must actually arrive, and must not repeat inside the cooldown."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self._old = notifier.STATE_PATH
        notifier.STATE_PATH = self.tmp / "alerts.json"

    def tearDown(self):
        notifier.STATE_PATH = self._old
        shutil.rmtree(self.tmp, ignore_errors=True)

    def cfg(self, **over):
        alert = {"enabled": True, "cooldown_minutes": 240, "alive_floor": 0,
                 "telegram": {"enabled": False, "token": "", "chat_id": ""},
                 "webhook": {"enabled": False, "url": ""}}
        alert.update(over)
        return {"alert": alert}

    def test_disabled_alerting_sends_nothing(self):
        cfg = self.cfg()
        cfg["alert"]["enabled"] = False
        self.assertEqual(notifier.send(cfg, "k", "t", "b"), [])

    def test_unconfigured_channels_report_cleanly(self):
        self.assertEqual(notifier.send(self.cfg(), "k", "t", "b"), [])

    def test_webhook_receives_the_payload(self):
        received = []

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                received.append(json.loads(self.rfile.read(length).decode("utf-8")))
                self.send_response(200)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"ok")

            def log_message(self, *args):
                pass

        server = HTTPServer(("127.0.0.1", 0), Handler)
        port = server.server_address[1]
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            cfg = self.cfg(webhook={"enabled": True, "url": f"http://127.0.0.1:{port}/hook"})
            out = notifier.send(cfg, "alive_low", "存活过低", "本轮 3 个", level="warn")
            self.assertIn("webhook: 已发送", out)
            self.assertEqual(received[0]["key"], "alive_low")
            self.assertEqual(received[0]["level"], "warn")
        finally:
            server.shutdown()
            server.server_close()

    def test_cooldown_suppresses_a_repeat_within_the_window(self):
        calls = []

        def fake(cfg_alert, payload):
            calls.append(payload)
            return True, "HTTP 200"

        cfg = self.cfg(webhook={"enabled": True, "url": "http://127.0.0.1:1/hook"})
        with unittest.mock.patch.object(notifier, "webhook_send", fake):
            notifier.send(cfg, "alive_low", "t", "b")
            notifier.send(cfg, "alive_low", "t", "b")
        self.assertEqual(len(calls), 1)

    def test_cooldown_is_per_key(self):
        calls = []

        def fake(cfg_alert, payload):
            calls.append(payload)
            return True, "HTTP 200"

        cfg = self.cfg(webhook={"enabled": True, "url": "http://127.0.0.1:1/hook"})
        with unittest.mock.patch.object(notifier, "webhook_send", fake):
            notifier.send(cfg, "alive_low", "t", "b")
            notifier.send(cfg, "suspect_round", "t", "b")
        self.assertEqual(len(calls), 2)

    def test_reset_clears_the_cooldown(self):
        calls = []

        def fake(cfg_alert, payload):
            calls.append(payload)
            return True, "HTTP 200"

        cfg = self.cfg(webhook={"enabled": True, "url": "http://127.0.0.1:1/hook"})
        with unittest.mock.patch.object(notifier, "webhook_send", fake):
            notifier.send(cfg, "alive_low", "t", "b")
            notifier.reset_cooldown("alive_low")
            notifier.send(cfg, "alive_low", "t", "b")
        self.assertEqual(len(calls), 2)


class WatchdogTest(unittest.TestCase):
    def test_budget_comes_from_config(self):
        self.assertEqual(engine._budget({"watchdog": {"round_timeout_minutes": 7}}), 420)
        self.assertEqual(engine._budget({}), 20 * 60)
        self.assertEqual(engine._budget({"watchdog": {"round_timeout_minutes": "x"}}), 20 * 60)

    def test_checkpoint_raises_when_past_the_deadline(self):
        with self.assertRaises(engine.RoundTimeout):
            engine._checkpoint(time.monotonic() - 1, "delay-test", 1)

    def test_checkpoint_passes_inside_the_budget(self):
        engine._checkpoint(time.monotonic() + 60, "delay-test", 1)


class LaneConfigTest(unittest.TestCase):
    """Lanes exist so egress verification can run concurrently."""

    def cfg(self, lanes=8, base=19200):
        return {"api": "http://127.0.0.1:19190", "lanes": lanes, "base_port": base,
                "probe_group": "__PROBE__", "mixed_port": 19194}

    def render(self, cfg, proxies=None):
        if proxies is None:
            proxies = [{"name": "a", "type": "vless", "server": "s", "port": 1}]
        tmp = Path(tempfile.mkdtemp())
        old = cfgmod.CORE_DIR
        cfgmod.CORE_DIR = tmp
        try:
            path = coremod.build_config(proxies, cfg, "sekret")
            return path.read_text(encoding="utf-8")
        finally:
            cfgmod.CORE_DIR = old
            shutil.rmtree(tmp, ignore_errors=True)

    def test_lane_count_is_bounded(self):
        self.assertEqual(coremod.lane_count(self.cfg(lanes=3)), 3)
        self.assertEqual(coremod.lane_count(self.cfg(lanes=999)), 32)
        self.assertEqual(coremod.lane_count(self.cfg(lanes="bogus")), 8)

    def test_ports_run_from_the_base(self):
        self.assertEqual(coremod.lane_ports(self.cfg(base=19300), 3), [19300, 19301, 19302])

    def test_each_lane_gets_a_group_a_listener_and_a_rule(self):
        text = self.render(self.cfg(lanes=2))
        self.assertIn("IN-NAME,lane0,__LANE0__", text)
        self.assertIn("IN-NAME,lane1,__LANE1__", text)
        self.assertIn("port: 19200", text)
        self.assertIn("port: 19201", text)
        self.assertIn("listen: 127.0.0.1", text)

    def test_every_proxy_lands_in_exactly_one_lane_group(self):
        """The lane groups partition the proxies; they do not each list them all.

        Replaces `test_every_lane_lists_every_proxy`, which asserted the exact
        layout that caused the 2026-09-29 outage: 16 lanes x 486 nodes = 7776
        group members, a 450KB config, and a `PUT /configs?force=true` that
        outlived the controller's timeout on every round for nine days.

        The invariant that has to hold instead is a partition, because
        `engine._verify_egress` / `_verify_chain_payload` select a node only on
        the lane `build_config` filed it under (`index % lanes`). A missing
        node loses its exit check; a node in two groups would not break
        anything but means the config grew back.
        """
        proxies = [{"name": "a", "type": "vless", "server": "s", "port": 1},
                   {"name": "b", "type": "trojan", "server": "t", "port": 2},
                   {"name": "c", "type": "vmess", "server": "u", "port": 3},
                   {"name": "d", "type": "ss", "server": "v", "port": 4}]
        lanes = 3
        text = self.render(self.cfg(lanes=lanes), proxies)

        groups, current = {}, None
        for line in text.splitlines():
            if line.startswith('  - name: "__LANE'):
                current = line.split('"')[1]
                groups[current] = []
            elif current is not None and line.startswith("      - "):
                groups[current].append(line.strip()[2:].strip('"'))
            elif line.startswith("listeners:"):
                current = None

        # names[i::lanes]: lane0 -> a, d; lane1 -> b; lane2 -> c
        self.assertEqual(groups.get("__LANE0__"), ["a", "d"])
        self.assertEqual(groups.get("__LANE1__"), ["b"])
        self.assertEqual(groups.get("__LANE2__"), ["c"])
        for name in ("a", "b", "c", "d"):
            self.assertEqual(text.count(f'      - "{name}"'), 1,
                             f"{name} must appear in exactly one lane group")

    def test_match_fallback_targets_lane_zero(self):
        self.assertIn("MATCH,__LANE0__", self.render(self.cfg(lanes=4)))


class StaleExportTest(unittest.TestCase):
    def test_removes_only_exports_whose_key_is_gone(self):
        tmp = Path(tempfile.mkdtemp())
        old = engine.EXPORT_DIR
        engine.EXPORT_DIR = tmp
        try:
            for key in ("air", "probe", "gone"):
                (tmp / f"{key}.yaml").write_text("proxies: []", encoding="utf-8")
                (tmp / f"{key}.meta.json").write_text("{}", encoding="utf-8")
            removed = engine.cleanup_exports(
                {"sources": [{"key": "air"}, {"key": "probe"}]})
            self.assertEqual(sorted(removed), ["gone"])
            self.assertTrue((tmp / "air.yaml").exists())
            self.assertFalse((tmp / "gone.yaml").exists())
            self.assertFalse((tmp / "gone.meta.json").exists())
        finally:
            engine.EXPORT_DIR = old
            shutil.rmtree(tmp, ignore_errors=True)


class SelfReferenceConfigTest(unittest.TestCase):
    def test_rejects_an_enabled_source_named_like_the_output_collection(self):
        with self.assertRaises(ValueError):
            cfgmod.reject_self_reference(
                [{"key": "probe", "kind": "collection", "name": "probe",
                  "enabled": True}], "probe")

    def test_a_disabled_entry_is_allowed(self):
        """只有「启用」它才构成自我循环。

        原来不管 enabled 一律拒绝，后果是死锁：配置里留着一条未勾选的
        `collection/<prefix>` 时，**任何**保存都会失败 —— 连错误提示里说的
        「请在面板里取消勾选它」都做不到（它本来就是没勾的）。未启用的条目是
        惰性的，`engine.export_keys` 和轮次都只走 enabled 来源。
        """
        cfgmod.reject_self_reference(
            [{"key": "probe", "kind": "collection", "name": "probe",
              "enabled": False}], "probe")

    def test_allows_other_names_and_kinds(self):
        cfgmod.reject_self_reference(
            [{"key": "air", "kind": "collection", "name": "air", "enabled": True},
             {"key": "probe", "kind": "sub", "name": "probe", "enabled": True}], "probe")

    def test_blank_prefix_disables_the_check(self):
        cfgmod.reject_self_reference(
            [{"key": "probe", "kind": "collection", "name": "probe",
              "enabled": True}], "")

    def test_update_surfaces_the_error(self):
        tmp = Path(tempfile.mkdtemp())
        old_data, old_path = cfgmod.DATA, cfgmod.CONFIG_PATH
        try:
            cfgmod.DATA = tmp
            cfgmod.CONFIG_PATH = tmp / "config.json"
            cfgmod.load()
            with self.assertRaises(ValueError):
                cfgmod.update({"sources": [
                    {"key": "air", "kind": "collection", "name": "air", "enabled": True},
                    {"key": "probe", "kind": "collection", "name": "probe", "enabled": True}]})
        finally:
            cfgmod.DATA, cfgmod.CONFIG_PATH = old_data, old_path
            shutil.rmtree(tmp, ignore_errors=True)



class EntryLookupTest(unittest.TestCase):
    """The ip-api batch reply uses `query` for the address; normalise it."""

    def test_response_rows_are_mapped_into_the_cache(self):
        tmp = Path(tempfile.mkdtemp())
        old_data, old_path, old_conn = cfgmod.DATA, engine.db.DB_PATH, engine.db._conn
        cfgmod.DATA = tmp
        engine.db.DB_PATH, engine.db._conn = tmp / "s.db", None
        try:
            engine.db.connect()
            rows = [{"query": "1.2.3.4", "countryCode": "CN", "isp": "ZHAOONE"},
                    {"query": "5.6.7.8", "countryCode": "US", "isp": "Buyvm"}]
            engine.db.ip_geo_put(
                [{"ip": r["query"], "country": r["countryCode"], "isp": r["isp"]}
                 for r in rows])
            cached = engine.db.ip_geo_get(["1.2.3.4", "5.6.7.8", "9.9.9.9"])
            self.assertEqual(cached, {"1.2.3.4": "CN", "5.6.7.8": "US"})
        finally:
            if engine.db._conn is not None:
                engine.db._conn.close()
            engine.db.DB_PATH, engine.db._conn = old_path, old_conn
            cfgmod.DATA = old_data
            shutil.rmtree(tmp, ignore_errors=True)

if __name__ == "__main__":
    unittest.main(verbosity=2)