"""F-001 Sub-Store write-zero: a round records ZERO mutating store calls.

The final feature list (F-001) demands "the test environment records all
requests, ensuring the Sub-Store write request count is 0". The unit tests in
`test_logic.PushExportsTest` pin the double gate on `push_exports` directly;
this module runs a WHOLE round (`engine._run_round`, the same entry the panel
and the scheduler take) against a store that records every mutation and
refuses it, then asserts nothing was refused and the local export still
landed. A regression that reopens per-round Sub-Store upserts -- by default,
by flag alone, or via a new call site -- fails here with the offending call
named.
"""
import os
import shutil
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import config as cfgmod
from mihomo_test import core as coremod
from mihomo_test import engine, notifier
from mihomo_test.store import StoreError

try:
    import _isolation
except ImportError:  # imported as a package: python -m unittest tests.test_substore_write_zero
    from tests import _isolation


def setUpModule():
    _isolation.isolate()


def tearDownModule():
    _isolation.restore()


_nolog = lambda *a, **k: None


class _RecordingStore:
    """A Sub-Store client that answers reads and records every mutation.

    Reads (`fetch_source`, `fetch_sub_proxies`, `get_json`) are answered from
    the fixture's seed data. ANY other attribute -- `upsert`, `update_only`,
    `delete`, `_request`, tomorrow's new helper -- is recorded as a write
    attempt and refused, so a default round must sail through with an empty
    attempt list or the test names the call.
    """

    def __init__(self, backend):
        self.backend = backend
        self.write_attempts = []
        self.reads = []

    def fetch_source(self, kind, name, target="ClashMeta"):
        self.reads.append(("fetch_source", kind, name))
        return [dict(_NODE)]

    def fetch_sub_proxies(self, name, target="ClashMeta"):
        self.reads.append(("fetch_sub_proxies", name))
        return []

    def get_json(self, path):
        self.reads.append(("get_json", path))
        if path == "/api/subs":
            return []
        raise StoreError(f"no fixture for {path}")

    def __getattr__(self, name):
        if name.startswith("_") and name.endswith("_RecordingStore__"):
            raise AttributeError(name)

        def _recorded_write(*args, **kwargs):
            self.write_attempts.append((name,) + tuple(repr(a)[:80] for a in args))
            raise StoreError(f"F-001: recorded a mutating store call: {name}")

        return _recorded_write


class _RoundCore:
    """Fake kernel: every node answers 30ms, no payload phase needed."""

    def __init__(self, core_cfg, secret):
        self.seen = []

    def start_and_load(self, log=None):
        return "started"

    def delay(self, name, url, timeout_ms, expected):
        self.seen.append(name)
        return (30, None, "")

    def select(self, group, name):
        pass

    def egress(self, port, trace_url, timeout_s=15):
        return {"loc": "US", "ip": "1.2.3.4"}, None

    def fetch(self, port, url, timeout_s=15):
        return 302, 372, None


_NODE = {
    "name": "hk-node",
    "type": "vless",
    "server": "t.example",
    "port": 443,
    "uuid": "00000000-0000-4000-8000-000000000001",
}


class SubStoreWriteZeroTest(unittest.TestCase):
    """A default round (and a legacy-flagged-but-ungated one) writes nothing."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.old = (cfgmod.DATA, cfgmod.CORE_DIR, cfgmod.CORE_SECRET_PATH,
                    engine.EXPORT_DIR, engine.db.DB_PATH, engine.db._conn)
        cfgmod.DATA = self.tmp
        cfgmod.CORE_DIR = self.tmp / "core"
        cfgmod.CORE_SECRET_PATH = self.tmp / "core.secret"
        engine.db.DB_PATH, engine.db._conn = self.tmp / "s.db", None
        engine.EXPORT_DIR = self.tmp / "exports"
        engine.db.connect()

    def tearDown(self):
        if engine.db._conn is not None:
            engine.db._conn.close()
        (cfgmod.DATA, cfgmod.CORE_DIR, cfgmod.CORE_SECRET_PATH,
         engine.EXPORT_DIR, engine.db.DB_PATH, engine.db._conn) = self.old
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _cfg(self, publish_extra=None):
        publish = {"enabled": True, "prefix": "probe", "add_region_tag": False}
        if publish_extra:
            publish.update(publish_extra)
        return {
            "substore": {"backend": "http://127.0.0.1:3000"},
            "core": {"api": "http://127.0.0.1:19190", "lanes": 2, "base_port": 19300,
                     "container": "mihomo-probe",
                     "mixed_port": 19194,
                     "container_config_path": "/root/.config/mihomo/config.yaml"},
            "sources": [{"key": "air", "kind": "sub", "name": "air",
                         "label": "air", "enabled": True}],
            "chain": {"enabled": False},
            "test": {"targets": ["http://probe/generate_204"], "expected_status": "204",
                     "timeout_ms": 100, "timeout_ms_retry": 100, "max_attempts": 1,
                     "retry_pause_s": 0, "concurrency": 2},
            "dns": {"views": {}, "timeout_s": 1, "cache_hours": 0},
            "verify": {"enabled": False},
            "policy": {"drop_after_consecutive_fails": 3,
                       "suspect_floor_ratio": 0.5, "suspect_floor_absolute": 3},
            "schedule": {"interval_minutes": 30, "enabled": False},
            "publish": publish,
            "watchdog": {"round_timeout_minutes": 20},
            "alert": {"enabled": False},
        }

    def _run(self, cfg):
        store = _RecordingStore(cfg["substore"]["backend"])
        with unittest.mock.patch.object(engine, "Client", lambda backend: store), \
                unittest.mock.patch.object(coremod, "Core", _RoundCore), \
                unittest.mock.patch.object(coremod, "config_test",
                                           return_value=(True, "")), \
                unittest.mock.patch.object(engine, "_resolve_candidates",
                                           return_value=["1.2.3.4"]), \
                unittest.mock.patch.object(engine, "lookup_countries",
                                           return_value={"1.2.3.4": "US"}), \
                unittest.mock.patch.object(notifier, "send", lambda *a, **k: None):
            summary = engine._run_round(cfg, "test", None, _nolog)
        return summary, store

    def test_a_default_round_records_zero_substore_writes(self):
        """The default config: publish to disk, never touch Sub-Store."""
        summary, store = self._run(self._cfg())
        self.assertNotIn("error", summary, f"the round must succeed: {summary}")
        self.assertEqual(store.write_attempts, [],
                         f"F-001 violated: {store.write_attempts}")
        # The round must still publish locally -- write-zero must not be
        # trivially true because the round did nothing at all.
        export = engine.EXPORT_DIR / "air.yaml"
        self.assertTrue(export.exists(), "the local export must exist")
        body = export.read_text(encoding="utf-8")
        self.assertIn("hk-node", body)

    def test_the_legacy_push_flag_alone_still_records_zero_writes(self):
        """`push_to_substore: true` without the legacy flag writes nothing."""
        cfg = self._cfg({"push_to_substore": True})
        summary, store = self._run(cfg)
        self.assertNotIn("error", summary, f"the round must succeed: {summary}")
        self.assertEqual(store.write_attempts, [],
                         f"F-001 violated: {store.write_attempts}")
        self.assertTrue((engine.EXPORT_DIR / "air.yaml").exists())

    def test_a_retyped_source_never_reaches_the_store(self):
        """A disabled source is not fetched and not written either."""
        cfg = self._cfg()
        cfg["sources"] = [{"key": "air", "kind": "sub", "name": "air",
                           "enabled": False},
                          {"key": "live", "kind": "sub", "name": "live",
                           "enabled": True}]
        _, store = self._run(cfg)
        self.assertEqual(store.write_attempts, [])
        fetched = [name for kind, name in
                   [read[1:3] if read[0] == "fetch_source" else ("", "")
                    for read in store.reads]
                   if name]
        self.assertNotIn("air", fetched,
                         "a disabled source must not even be read from Sub-Store")


if __name__ == "__main__":
    unittest.main()
