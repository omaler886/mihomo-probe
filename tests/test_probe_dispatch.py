"""HK3 P3b: probe_dispatch（Job 分发 / ProbeResult 校验）、server /api/v1 面、
agent 模式。对照指导 §4.1（Job 下发）、§4.2（结果回传）、§10（API 契约）、
§13（安全要求）、§14（测试门禁）。

与 P3a 的契约（brief）：engine._dispatch_probe_jobs 内部转调
probe_dispatch.dispatch_round；engine.config_digest 未就绪时本模块用同一
算法的本地实现（probe_dispatch.config_digest 优先调用 engine 版）。
tests/_isolation.py 保证整个模块不会碰真实 data/ 目录。
"""
import http.server
import json
import os
import shutil
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import mihomo_test.__main__ as agent_mod
from mihomo_test import config as cfgmod
from mihomo_test import core as coremod
from mihomo_test import db
from mihomo_test import gist_store
from mihomo_test import manual_publish
from mihomo_test import probe_dispatch
from mihomo_test import server
from mihomo_test.store import NotFound

try:
    import _isolation
except ImportError:  # imported as a package: python -m unittest tests.test_probe_dispatch
    from tests import _isolation


def setUpModule():
    _isolation.isolate()


def tearDownModule():
    _isolation.restore()


PROBE_TOKEN = "probe-token-hk3-a-0123456789abcdef"
PROBE_TOKEN_ENV = "PROBE_TEST_TOKEN"
ADMIN_TOKEN = "admin-token-0123456789abcdef"
ROUND_ID = 7


def _cfg(stub=None, probe_id="hk3-a", extra_probes=None):
    """A minimal v3 config: one probe wired at `stub`, everything else inert."""
    probes = []
    if stub is not None:
        probes.append({"id": probe_id, "name": "HK3 主探测点", "enabled": True,
                       "endpoint": stub.endpoint,
                       "token_env": PROBE_TOKEN_ENV, "weight": 100,
                       "tags": ["HK", "HK3"]})
    probes.extend(extra_probes or [])
    return {
        "schema_version": 3,
        "sources": [{"key": "air", "kind": "collection", "name": "air",
                     "enabled": True}],
        "test": {"targets": ["https://www.gstatic.com/generate_204"],
                 "timeout_ms": 5000, "concurrency": 20, "expected_status": "204"},
        "probe_policy": {"timeout_ms": 5000, "concurrency": 20,
                         "dead_after_consecutive_failures": 3},
        "probes": probes,
        "aggregation": {"strategy": "any_alive", "minimum_completed_probes": 1,
                        "late_result_seconds": 90},
        "chain": {"enabled": False},
        "substore": {"manual_push_only": True, "target_name": "mihomo-probe-alive",
                     "backend": "http://127.0.0.1:3000"},
        "gist": {"enabled": True, "gist_id": "", "token_env": "GITHUB_GIST_TOKEN"},
        "auth": {"token": ADMIN_TOKEN},
        "publish": {"token": "p" * 32, "prefix": "probe",
                    "hostname": "probe.example", "enabled": True},
        "ui": {"title": "测试面板"},
        "schedule": {"enabled": False, "interval_minutes": 30},
        "core": {"container": "mihomo-probe"},
    }


def _job(round_id=ROUND_ID, probe_id="hk3-a", deadline=None, digest="sha256:test"):
    return {
        "round_id": round_id,
        "probe_id": probe_id,
        "config_digest": digest,
        "deadline": deadline or probe_dispatch.iso_now(),
        "chain_enabled": False,
        "nodes": [
            {"fingerprint": "fp1", "mihomo_name": "__P_000001",
             "proxy": {"name": "__P_000001", "type": "ss", "server": "127.0.0.1",
                       "port": 1234, "cipher": "aes-128-gcm",
                       "password": "s3cret-node-credential"}},
        ],
    }


def _result_payload(round_id=ROUND_ID, probe_id="hk3-a", digest="sha256:test",
                    results=None):
    return {
        "round_id": round_id,
        "probe_id": probe_id,
        "config_digest": digest,
        "started_at": probe_dispatch.iso_now(),
        "finished_at": probe_dispatch.iso_now(),
        "results": results if results is not None else [
            {"fingerprint": "fp1", "alive": True, "delay_ms": 120,
             "reason": None, "egress_country": None},
        ],
    }


def _events():
    return [row["message"] for row in db.recent_events(200)]


class _StubProbe:
    """A stand-in probe endpoint that records exactly what the Controller sent."""

    def __init__(self, mode="ok", sleep_s=0.0):
        self.mode = mode
        self.sleep_s = sleep_s
        self.requests = []
        self._lock = threading.Lock()
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, fmt, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length)
                with outer._lock:
                    outer.requests.append({"path": self.path,
                                           "headers": dict(self.headers),
                                           "body": raw})
                if outer.mode == "sleep":
                    time.sleep(outer.sleep_s)
                # sleep 模式下客户端（dispatch 的超时只有零点几秒）早就挂断了，
                # 处理线程睡醒后写响应必然失败——这正是这个桩点要模拟的场景，
                # 写失败不是用例失败，所以吞掉 OSError，不给 stderr 留噪音。
                try:
                    if outer.mode == "http500":
                        self.send_response(500)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                    payload = b'{"ok": true}'
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                except OSError:
                    pass

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever,
                                       daemon=True)
        self.thread.start()

    @property
    def endpoint(self):
        return f"http://127.0.0.1:{self.port}"

    def last(self):
        with self._lock:
            return self.requests[-1] if self.requests else None

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


def _closed_port():
    """A loopback port nobody listens on (probe-offline fixture)."""
    import socket
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


class HmacTest(unittest.TestCase):
    """sign_request / verify_signature：纯函数，指导 §13.1 的签名与时间窗。"""

    HMAC_KEY = "probe-token-value"  # test-only fixture, not a credential

    def test_roundtrip(self):
        body = b'{"round_id": 7}'
        ts = str(int(time.time()))
        signature = probe_dispatch.sign_request(self.HMAC_KEY, body, ts)
        self.assertTrue(probe_dispatch.verify_signature(
            self.HMAC_KEY, body, ts, signature))

    def test_expired_timestamp_is_refused(self):
        body = b"{}"
        ts = str(int(time.time()) - probe_dispatch.SIGNATURE_WINDOW_S - 5)
        signature = probe_dispatch.sign_request(self.HMAC_KEY, body, ts)
        self.assertFalse(probe_dispatch.verify_signature(
            self.HMAC_KEY, body, ts, signature))

    def test_future_timestamp_outside_the_window_is_refused(self):
        body = b"{}"
        ts = str(int(time.time()) + probe_dispatch.SIGNATURE_WINDOW_S + 5)
        signature = probe_dispatch.sign_request(self.HMAC_KEY, body, ts)
        self.assertFalse(probe_dispatch.verify_signature(
            self.HMAC_KEY, body, ts, signature))

    def test_tampered_body_is_refused(self):
        body = b'{"round_id": 7}'
        ts = str(int(time.time()))
        signature = probe_dispatch.sign_request(self.HMAC_KEY, body, ts)
        self.assertFalse(probe_dispatch.verify_signature(
            self.HMAC_KEY, body + b" ", ts, signature))

    def test_wrong_secret_is_refused(self):
        body = b"{}"
        ts = str(int(time.time()))
        signature = probe_dispatch.sign_request(self.HMAC_KEY, body, ts)
        self.assertFalse(probe_dispatch.verify_signature(
            "another-probe-token", body, ts, signature))

    def test_non_numeric_timestamp_is_refused(self):
        body = b"{}"
        signature = probe_dispatch.sign_request(self.HMAC_KEY, body, "1700000000")
        self.assertFalse(probe_dispatch.verify_signature(
            self.HMAC_KEY, body, "not-a-number", signature))

    def test_empty_signature_is_refused(self):
        body = b"{}"
        ts = str(int(time.time()))
        self.assertFalse(probe_dispatch.verify_signature(
            self.HMAC_KEY, body, ts, ""))


class ConfigDigestTest(unittest.TestCase):
    """config_digest：稳定、随测量配置变、不随 token/调度变。"""

    def test_digest_is_stable_for_equal_configs(self):
        self.assertEqual(probe_dispatch.config_digest(_cfg()),
                         probe_dispatch.config_digest(_cfg()))

    def test_digest_changes_when_the_measured_config_changes(self):
        before = probe_dispatch.config_digest(_cfg())
        cfg = _cfg()
        cfg["test"]["timeout_ms"] = 9000
        self.assertNotEqual(before, probe_dispatch.config_digest(cfg))

    def test_digest_ignores_volatile_keys(self):
        """轮次中途轮换 admin token / 改调度间隔不该作废在途结果。"""
        before = probe_dispatch.config_digest(_cfg())
        cfg = _cfg()
        cfg["auth"] = {"token": "rotated-" + "x" * 32}
        cfg["schedule"] = {"enabled": True, "interval_minutes": 5}
        cfg["ui"] = {"title": "换个标题"}
        self.assertEqual(before, probe_dispatch.config_digest(cfg))

    def test_local_fallback_matches_engine_verbatim(self):
        """回退实现必须与 engine.config_digest 逐字节一致（P3a 对齐）。

        两份实现若哈希不同的键集，会互相拒绝对方的结果——Job 带的是 engine
        算的 digest，API 侧校验时若走了回退分支就必须算出同一个值。
        """
        from mihomo_test import engine
        with mock.patch.object(probe_dispatch, "_engine_config_digest",
                               lambda: None):
            self.assertEqual(probe_dispatch.config_digest(_cfg()),
                             engine.config_digest(_cfg()))
            self.assertEqual(probe_dispatch.config_digest({}),
                             engine.config_digest({}))

    def test_a_broken_engine_digest_falls_back_instead_of_raising(self):
        """engine.config_digest 抛错时走回退，而不是 NameError/崩溃。"""
        def boom(_cfg):
            raise RuntimeError("engine digest is broken")

        with mock.patch.object(probe_dispatch, "_engine_config_digest",
                               lambda: boom):
            digest = probe_dispatch.config_digest(_cfg())
        self.assertTrue(digest.startswith("sha256:"))
        self.assertEqual(len(digest), len("sha256:") + 64)


class DispatchRoundTest(unittest.TestCase):
    """dispatch_round：指导 §4.1 下发 + §13.1 传输安全 + §4.3 缺失兜底。"""

    def setUp(self):
        probe_dispatch.reset_deadlines()
        self.logs = []
        self.log = lambda level, message: self.logs.append((level, message))
        self.stub = _StubProbe()
        self.addCleanup(self.stub.close)
        self.env = mock.patch.dict(os.environ,
                                   {PROBE_TOKEN_ENV: PROBE_TOKEN})
        self.env.start()
        self.addCleanup(self.env.stop)
        db.connect()

    def test_job_is_posted_with_token_and_signature(self):
        job = _job()
        summary = probe_dispatch.dispatch_round(_cfg(self.stub), ROUND_ID,
                                                [job], self.log)
        self.assertEqual(summary, {"hk3-a": "ok"})
        request = self.stub.last()
        self.assertIsNotNone(request)
        self.assertEqual(request["path"], "/jobs")
        self.assertEqual(request["headers"].get("Authorization"),
                         f"Bearer {PROBE_TOKEN}")
        self.assertEqual(request["headers"].get("Content-Type"),
                         "application/json")
        ts = request["headers"].get(probe_dispatch.TIMESTAMP_HEADER)
        signature = request["headers"].get(probe_dispatch.SIGNATURE_HEADER)
        self.assertTrue(ts and signature)
        self.assertTrue(probe_dispatch.verify_signature(
            PROBE_TOKEN, request["body"], ts, signature))
        self.assertEqual(json.loads(request["body"].decode("utf-8")), job)

    def test_token_and_credentials_never_reach_the_log(self):
        probe_dispatch.dispatch_round(_cfg(self.stub), ROUND_ID, [_job()],
                                      self.log)
        blob = "\n".join(message for _level, message in self.logs)
        for secret in (PROBE_TOKEN, "s3cret-node-credential"):
            self.assertNotIn(secret, blob)
        self.assertTrue(self.logs)

    def test_deadline_is_registered_for_later_verification(self):
        job = _job(deadline="2030-01-01T00:00:00Z")
        probe_dispatch.dispatch_round(_cfg(self.stub), ROUND_ID, [job], self.log)
        self.assertEqual(probe_dispatch.deadline_for("hk3-a", ROUND_ID),
                         "2030-01-01T00:00:00Z")

    def test_offline_probe_does_not_raise_and_leaves_no_observations(self):
        cfg = _cfg()
        cfg["probes"] = [{"id": "hk3-a", "name": "离线探测点", "enabled": True,
                          "endpoint": f"http://127.0.0.1:{_closed_port()}",
                          "token_env": PROBE_TOKEN_ENV}]
        summary = probe_dispatch.dispatch_round(cfg, ROUND_ID, [_job()], self.log)
        self.assertEqual(summary, {"hk3-a": "err"})
        self.assertEqual(db.get_observations(ROUND_ID), [])
        self.assertTrue(any(level == "warn" for level, _ in self.logs))

    def test_timeout_is_err_not_a_raise(self):
        slow = _StubProbe(mode="sleep", sleep_s=3.0)
        self.addCleanup(slow.close)
        with mock.patch.object(probe_dispatch, "PROBE_TIMEOUT_S", 0.4):
            summary = probe_dispatch.dispatch_round(_cfg(slow), ROUND_ID,
                                                    [_job()], self.log)
        self.assertEqual(summary, {"hk3-a": "err"})

    def test_http_500_is_err(self):
        broken = _StubProbe(mode="http500")
        self.addCleanup(broken.close)
        summary = probe_dispatch.dispatch_round(_cfg(broken), ROUND_ID,
                                                [_job()], self.log)
        self.assertEqual(summary, {"hk3-a": "err"})

    def test_disabled_probe_is_skipped_without_any_request(self):
        cfg = _cfg(self.stub)
        cfg["probes"][0]["enabled"] = False
        summary = probe_dispatch.dispatch_round(cfg, ROUND_ID, [_job()], self.log)
        self.assertEqual(summary, {"hk3-a": "err"})
        self.assertIsNone(self.stub.last())

    def test_missing_token_env_is_err_without_any_request(self):
        env = mock.patch.dict(os.environ, {}, clear=False)
        env.start()
        self.addCleanup(env.stop)
        os.environ.pop(PROBE_TOKEN_ENV, None)
        summary = probe_dispatch.dispatch_round(_cfg(self.stub), ROUND_ID,
                                                [_job()], self.log)
        self.assertEqual(summary, {"hk3-a": "err"})
        self.assertIsNone(self.stub.last())

    def test_plain_http_off_loopback_is_refused(self):
        cfg = _cfg(self.stub)
        cfg["probes"][0]["endpoint"] = "http://probe.example.invalid"
        summary = probe_dispatch.dispatch_round(cfg, ROUND_ID, [_job()], self.log)
        self.assertEqual(summary, {"hk3-a": "err"})
        self.assertIsNone(self.stub.last())
        self.assertTrue(any("HTTPS" in message for _level, message in self.logs))

    def test_https_endpoint_is_accepted(self):
        """HTTPS 放行（§13.1）：用假 urlopen 断言 URL 原样到达。"""
        seen = []

        class _FakeResponse:
            status = 200

            def read(self):
                return b"{}"

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

        def fake_urlopen(request, timeout=None):
            seen.append(request.full_url)
            return _FakeResponse()

        cfg = _cfg(self.stub)
        cfg["probes"][0]["endpoint"] = "https://probe-a.example.com"
        with mock.patch.object(probe_dispatch.urllib.request, "urlopen",
                               fake_urlopen):
            summary = probe_dispatch.dispatch_round(cfg, ROUND_ID, [_job()],
                                                    self.log)
        self.assertEqual(summary, {"hk3-a": "ok"})
        self.assertEqual(seen, ["https://probe-a.example.com/jobs"])


class VerifyResultTest(unittest.TestCase):
    """verify_result：指导 §4.2 的四项校验 + 审计-only 拒绝 + 幂等落账。"""

    def setUp(self):
        probe_dispatch.reset_deadlines()
        db.connect()
        self.cfg = _cfg()
        db.upsert_probe("hk3-a", "HK3 主探测点", enabled=True)

    def _open_round(self):
        return db.start_round("test")

    def test_legal_result_is_recorded_idempotently(self):
        round_id = self._open_round()
        payload = _result_payload(
            round_id=round_id, digest=probe_dispatch.config_digest(self.cfg))
        self.assertIsNone(probe_dispatch.verify_result(payload, self.cfg))
        rows = db.get_observations(round_id)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["fingerprint"], "fp1")
        self.assertEqual(rows[0]["alive"], 1)
        self.assertEqual(rows[0]["delay_ms"], 120)
        # 心跳：探测点被 touch
        seen = db.one("SELECT last_seen_at FROM probes WHERE probe_id=?",
                      ("hk3-a",))
        self.assertTrue(seen["last_seen_at"])
        # 重放同一份结果：覆盖而不是重复（指导 §2：重复上报必须幂等）
        replay = _result_payload(round_id=round_id,
                                 digest=probe_dispatch.config_digest(self.cfg),
                                 results=[{"fingerprint": "fp1", "alive": False,
                                           "delay_ms": 5000,
                                           "reason": "timeout",
                                           "egress_country": None}])
        self.assertIsNone(probe_dispatch.verify_result(replay, self.cfg))
        rows = db.get_observations(round_id)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["alive"], 0)
        self.assertEqual(rows[0]["reason"], "timeout")

    def test_round_id_as_string_is_accepted(self):
        round_id = self._open_round()
        payload = _result_payload(round_id=str(round_id),
                                  digest=probe_dispatch.config_digest(self.cfg))
        self.assertIsNone(probe_dispatch.verify_result(payload, self.cfg))
        self.assertEqual(len(db.get_observations(round_id)), 1)

    def test_unknown_round_is_stale_and_audit_only(self):
        payload = _result_payload(round_id=4242,
                                  digest=probe_dispatch.config_digest(self.cfg))
        self.assertEqual(probe_dispatch.verify_result(payload, self.cfg),
                         "ROUND_STALE")
        self.assertEqual(db.get_observations(4242), [])
        self.assertTrue(any("ROUND_STALE" in m for m in _events()))

    def test_settled_round_is_stale(self):
        round_id = self._open_round()
        db.finish_round(round_id, note="done")
        payload = _result_payload(
            round_id=round_id, digest=probe_dispatch.config_digest(self.cfg))
        self.assertEqual(probe_dispatch.verify_result(payload, self.cfg),
                         "ROUND_STALE")
        self.assertEqual(db.get_observations(round_id), [])

    def test_superseded_round_is_stale(self):
        old = self._open_round()
        self._open_round()  # a newer round took over
        payload = _result_payload(round_id=old,
                                  digest=probe_dispatch.config_digest(self.cfg))
        self.assertEqual(probe_dispatch.verify_result(payload, self.cfg),
                         "ROUND_STALE")
        self.assertEqual(db.get_observations(old), [])

    def test_unknown_probe_is_refused(self):
        round_id = self._open_round()
        payload = _result_payload(
            round_id=round_id, probe_id="ghost-probe",
            digest=probe_dispatch.config_digest(self.cfg))
        self.assertEqual(probe_dispatch.verify_result(payload, self.cfg),
                         "UNKNOWN_PROBE")
        self.assertEqual(db.get_observations(round_id), [])
        self.assertTrue(any("UNKNOWN_PROBE" in m for m in _events()))

    def test_digest_mismatch_is_refused(self):
        round_id = self._open_round()
        payload = _result_payload(round_id=round_id,
                                  digest="sha256:someone-elses-config")
        self.assertEqual(probe_dispatch.verify_result(payload, self.cfg),
                         "DIGEST_MISMATCH")
        self.assertEqual(db.get_observations(round_id), [])
        self.assertTrue(any("DIGEST_MISMATCH" in m for m in _events()))

    def test_late_result_is_refused(self):
        round_id = self._open_round()
        probe_dispatch.note_deadline("hk3-a", round_id, "2020-01-01T00:00:00Z")
        payload = _result_payload(
            round_id=round_id, digest=probe_dispatch.config_digest(self.cfg))
        self.assertEqual(probe_dispatch.verify_result(payload, self.cfg),
                         "RESULT_LATE")
        self.assertEqual(db.get_observations(round_id), [])
        self.assertTrue(any("RESULT_LATE" in m for m in _events()))

    def test_finished_at_past_the_deadline_is_refused(self):
        round_id = self._open_round()
        deadline = probe_dispatch.iso_now()
        probe_dispatch.note_deadline("hk3-a", round_id, deadline)
        payload = _result_payload(
            round_id=round_id, digest=probe_dispatch.config_digest(self.cfg))
        # 回报里的 finished_at 远超 deadline（即便此刻还没过）
        payload["finished_at"] = "2030-01-01T00:00:00Z"
        self.assertEqual(probe_dispatch.verify_result(payload, self.cfg),
                         "RESULT_LATE")
        self.assertEqual(db.get_observations(round_id), [])

    def test_deadline_survives_a_controller_restart(self):
        """登记表落盘：重启后在途的过期结果仍然拦得住。"""
        round_id = self._open_round()
        probe_dispatch.note_deadline("hk3-a", round_id, "2020-01-01T00:00:00Z")
        probe_dispatch._deadline_cache = None  # 模拟重启：只丢内存，不丢文件
        payload = _result_payload(
            round_id=round_id, digest=probe_dispatch.config_digest(self.cfg))
        self.assertEqual(probe_dispatch.verify_result(payload, self.cfg),
                         "RESULT_LATE")

    def test_malformed_payloads_are_refused(self):
        round_id = self._open_round()
        digest = probe_dispatch.config_digest(self.cfg)
        bad_payloads = [
            {},
            {"round_id": round_id, "config_digest": digest, "results": []},
            {"round_id": round_id, "probe_id": "hk3-a", "config_digest": digest},
            _result_payload(round_id=round_id, digest=digest,
                            results="not-a-list"),
            _result_payload(round_id=round_id, digest=digest,
                            results=[{"alive": True}]),
            _result_payload(round_id=round_id, digest=digest,
                            results=[{"fingerprint": "fp1", "alive": "yes"}]),
            _result_payload(round_id=round_id, digest=digest,
                            results=[{"fingerprint": "fp1", "alive": True,
                                      "delay_ms": -5}]),
        ]
        for payload in bad_payloads:
            with self.subTest(payload=payload):
                self.assertEqual(probe_dispatch.verify_result(payload, self.cfg),
                                 "BAD_PAYLOAD")
        self.assertEqual(db.get_observations(round_id), [])

    def test_expected_digest_override(self):
        """调用方可显式指定期望 digest（轮次自己的，而非当前配置的）。"""
        round_id = self._open_round()
        payload = _result_payload(round_id=round_id,
                                  digest="sha256:the-rounds-own")
        self.assertIsNone(probe_dispatch.verify_result(
            payload, self.cfg, expected_digest="sha256:the-rounds-own"))
        self.assertEqual(len(db.get_observations(round_id)), 1)


class _V1Server:
    """A loopback Controller carrying the /api/v1 surface (指导 §10).

    Same redirection discipline as test_hardening's _LiveServer: the data
    directory and config file are per-instance, because these routes write
    (heartbeat/result/audit) and a shared path would leak between cases.
    """

    def __init__(self, cfg):
        self.tmp = Path(tempfile.mkdtemp(prefix="mihomo-v1-"))
        self._old = {"db_path": db.DB_PATH, "db_conn": db._conn,
                     "export_dir": None, "state": server._State.cfg,
                     "data": cfgmod.DATA, "config_path": cfgmod.CONFIG_PATH}
        db.DB_PATH, db._conn = self.tmp / "state.db", None
        cfgmod.DATA = self.tmp
        cfgmod.CONFIG_PATH = self.tmp / "config.json"
        cfgmod.save(cfg)
        self.cfg = cfg
        self.httpd = server.serve(cfg, "127.0.0.1", 0)
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever,
                                       daemon=True)
        self.thread.start()

    def url(self, path):
        return f"http://127.0.0.1:{self.port}{path}"

    def _request(self, method, path, payload=None, headers=None,
                 ctype="application/json"):
        data = None if payload is None else json.dumps(payload).encode("utf-8")
        merged = {"User-Agent": "test"}
        merged.update(headers or {})
        if data is not None:
            merged["Content-Type"] = ctype
        req = urllib.request.Request(self.url(path), data=data, method=method,
                                     headers=merged)
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return resp.status, resp.read().decode("utf-8")
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read().decode("utf-8")

    def get(self, path, headers=None):
        return self._request("GET", path, headers=headers)

    def post(self, path, payload=None, headers=None, ctype="application/json"):
        return self._request("POST", path, payload=payload, headers=headers,
                             ctype=ctype)

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()
        if db._conn is not None:
            db._conn.close()
        db.DB_PATH, db._conn = self._old["db_path"], self._old["db_conn"]
        server._State.cfg = self._old["state"]
        cfgmod.DATA = self._old["data"]
        cfgmod.CONFIG_PATH = self._old["config_path"]
        shutil.rmtree(self.tmp, ignore_errors=True)


class ServerV1Test(unittest.TestCase):
    """/api/v1 面：鉴权矩阵、投影、稳定错误码、占位审计（指导 §10/§13）。"""

    ADMIN_HEADER = {"X-Auth-Token": ADMIN_TOKEN}
    PROBE_HEADER = {"Authorization": f"Bearer {PROBE_TOKEN}"}

    def setUp(self):
        probe_dispatch.reset_deadlines()
        self.env = mock.patch.dict(os.environ,
                                   {PROBE_TOKEN_ENV: PROBE_TOKEN})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.cfg = _cfg()
        self.cfg["probes"] = [{"id": "hk3-a", "name": "HK3 主探测点",
                               "enabled": True,
                               "endpoint": "https://probe-a.example.com",
                               "token_env": PROBE_TOKEN_ENV, "weight": 100,
                               "tags": ["HK"]}]
        self.srv = _V1Server(self.cfg)
        self.addCleanup(self.srv.close)
        db.connect()

    # -- 读接口 ---------------------------------------------------------

    def test_dashboard_reports_round_nodes_and_probes(self):
        db.start_round("test")
        db.upsert_node("air", "f1", "node-one", status="alive")
        db.upsert_node("air", "f2", "node-two", status="dead")
        code, body = self.srv.get("/api/v1/dashboard", self.ADMIN_HEADER)
        self.assertEqual(code, 200)
        payload = json.loads(body)
        self.assertEqual(payload["nodes"],
                         {"total": 2, "alive": 1, "pending": 0, "dead": 1,
                          "unknown": 0, "excluded": 0})
        self.assertEqual(payload["probes"],
                         {"total": 1, "enabled": 1, "online": 0})
        self.assertEqual(payload["round"]["state"], "running")
        # P4/P5 占位：发布时间先返回 null（brief 任务 C）
        self.assertIsNone(payload["gist"]["last_published_at"])
        self.assertIsNone(payload["substore"]["last_pushed_at"])
        self.assertNotIn(PROBE_TOKEN, body)
        self.assertNotIn(ADMIN_TOKEN, body)

    def test_dashboard_requires_admin(self):
        for headers in (None, self.PROBE_HEADER, {"X-Auth-Token": "wrong"}):
            with self.subTest(headers=headers):
                code, _ = self.srv.get("/api/v1/dashboard", headers)
                self.assertEqual(code, 401)

    def test_rounds_list_and_detail(self):
        first = db.start_round("test")
        second = db.start_round("test")
        db.finish_round(first, note="done")
        code, body = self.srv.get("/api/v1/rounds?limit=1", self.ADMIN_HEADER)
        self.assertEqual(code, 200)
        self.assertEqual(len(json.loads(body)["rounds"]), 1)
        code, body = self.srv.get(f"/api/v1/rounds/{second}", self.ADMIN_HEADER)
        self.assertEqual(code, 200)
        detail = json.loads(body)
        self.assertEqual(detail["round"]["id"], second)
        self.assertIn("results", detail)
        self.assertIn("observations", detail)
        code, body = self.srv.get("/api/v1/rounds/4242", self.ADMIN_HEADER)
        self.assertEqual(code, 404)
        self.assertEqual(json.loads(body)["error"], "ROUND_NOT_FOUND")
        code, body = self.srv.get("/api/v1/rounds?limit=0", self.ADMIN_HEADER)
        self.assertEqual(code, 400)

    def test_nodes_filters_and_pagination(self):
        db.upsert_node("air", "f1", "alpha", proto="ss", status="alive",
                       server="1.2.3.4")
        db.upsert_node("air", "f2", "beta", proto="trojan", status="dead",
                       server="5.6.7.8")
        db.upsert_node("sea", "f3", "gamma", proto="ss", status="alive",
                       server="9.9.9.9")
        code, body = self.srv.get("/api/v1/nodes", self.ADMIN_HEADER)
        self.assertEqual(code, 200)
        payload = json.loads(body)
        self.assertEqual(payload["total"], 3)
        self.assertEqual(len(payload["nodes"]), 3)
        # 分页
        code, body = self.srv.get("/api/v1/nodes?page=2&page_size=2",
                                  self.ADMIN_HEADER)
        payload = json.loads(body)
        self.assertEqual(payload["total"], 3)
        self.assertEqual(len(payload["nodes"]), 1)
        # 搜索 / 状态 / 协议 / 来源
        self.assertEqual(json.loads(self.srv.get(
            "/api/v1/nodes?q=alpha", self.ADMIN_HEADER)[1])["total"], 1)
        self.assertEqual(json.loads(self.srv.get(
            "/api/v1/nodes?status=dead", self.ADMIN_HEADER)[1])["total"], 1)
        self.assertEqual(json.loads(self.srv.get(
            "/api/v1/nodes?proto=ss", self.ADMIN_HEADER)[1])["total"], 2)
        self.assertEqual(json.loads(self.srv.get(
            "/api/v1/nodes?source=sea", self.ADMIN_HEADER)[1])["total"], 1)
        code, body = self.srv.get("/api/v1/nodes?status=bogus", self.ADMIN_HEADER)
        self.assertEqual(code, 400)
        # 投影里没有任何凭据字段（指导 §10）
        code, body = self.srv.get("/api/v1/nodes", self.ADMIN_HEADER)
        for secret in ("password", "uuid", "secret", "s3cret"):
            self.assertNotIn(secret, body)

    def test_probes_lists_config_entries_and_heartbeat_flips_online(self):
        code, body = self.srv.get("/api/v1/probes", self.ADMIN_HEADER)
        self.assertEqual(code, 200)
        probes = json.loads(body)["probes"]
        self.assertEqual(len(probes), 1)
        self.assertEqual(probes[0]["probe_id"], "hk3-a")
        self.assertFalse(probes[0]["online"])
        self.assertIsNone(probes[0]["last_seen_at"])
        code, _ = self.srv.post("/api/v1/probes/hk3-a/heartbeat",
                                {"version": "1.0.0", "state": "online"},
                                self.PROBE_HEADER)
        self.assertEqual(code, 200)
        code, body = self.srv.get("/api/v1/probes", self.ADMIN_HEADER)
        probes = json.loads(body)["probes"]
        self.assertTrue(probes[0]["online"])
        self.assertTrue(probes[0]["last_seen_at"])
        self.assertEqual(probes[0]["version"], "1.0.0")

    # -- heartbeat 鉴权矩阵 ----------------------------------------------

    def test_heartbeat_requires_the_probe_token(self):
        path = "/api/v1/probes/hk3-a/heartbeat"
        code, _ = self.srv.post(path, {}, None)
        self.assertEqual(code, 401)
        code, _ = self.srv.post(path, {}, {"Authorization": "Bearer wrong"})
        self.assertEqual(code, 401)
        # admin token 不能调 probe 面（两套凭证，指导 §13.1）
        code, _ = self.srv.post(path, {}, self.ADMIN_HEADER)
        self.assertEqual(code, 401)

    def test_heartbeat_unknown_probe_is_404(self):
        code, body = self.srv.post("/api/v1/probes/ghost/heartbeat", {},
                                   self.PROBE_HEADER)
        self.assertEqual(code, 404)
        self.assertEqual(json.loads(body)["error"], "UNKNOWN_PROBE")

    def test_heartbeat_needs_json_content_type(self):
        code, _ = self.srv.post("/api/v1/probes/hk3-a/heartbeat", {},
                                self.PROBE_HEADER, ctype="text/plain")
        self.assertEqual(code, 415)

    def test_heartbeat_with_unset_token_env_is_503(self):
        cfg = _cfg(extra_probes=[{"id": "hk3-b", "name": "B",
                                  "enabled": True,
                                  "endpoint": "https://b.example.com",
                                  "token_env": "PROBE_TEST_TOKEN_B"}])
        srv = _V1Server(cfg)
        self.addCleanup(srv.close)
        code, body = srv.post("/api/v1/probes/hk3-b/heartbeat", {},
                              self.PROBE_HEADER)
        self.assertEqual(code, 503)
        self.assertEqual(json.loads(body)["error"], "PROBE_TOKEN_UNSET")

    # -- result 路由 -----------------------------------------------------

    def _legal_payload(self, round_id=None):
        round_id = db.start_round("test") if round_id is None else round_id
        payload = _result_payload(round_id=round_id,
                                  digest=probe_dispatch.config_digest(self.cfg))
        return round_id, payload

    def test_result_round_trip_records_observations(self):
        round_id, payload = self._legal_payload()
        code, body = self.srv.post(
            f"/api/v1/probes/hk3-a/jobs/{round_id}/result", payload,
            self.PROBE_HEADER)
        self.assertEqual(code, 200)
        parsed = json.loads(body)
        self.assertTrue(parsed["ok"])
        self.assertEqual(parsed["recorded"], 1)
        self.assertTrue(parsed["audit_id"])
        self.assertNotIn(PROBE_TOKEN, body)
        rows = db.get_observations(round_id)
        self.assertEqual(len(rows), 1)
        # 重放：幂等覆盖，不新增
        code, _ = self.srv.post(
            f"/api/v1/probes/hk3-a/jobs/{round_id}/result", payload,
            self.PROBE_HEADER)
        self.assertEqual(code, 200)
        self.assertEqual(len(db.get_observations(round_id)), 1)

    def test_result_auth_matrix(self):
        round_id, payload = self._legal_payload()
        path = f"/api/v1/probes/hk3-a/jobs/{round_id}/result"
        code, _ = self.srv.post(path, payload, None)
        self.assertEqual(code, 401)
        code, _ = self.srv.post(path, payload,
                                {"Authorization": "Bearer wrong-token"})
        self.assertEqual(code, 401)
        code, _ = self.srv.post(path, payload, self.ADMIN_HEADER)
        self.assertEqual(code, 401)
        code, _ = self.srv.post(path, payload, self.PROBE_HEADER,
                                ctype="text/plain")
        self.assertEqual(code, 415)
        # probe token 不能调 admin 面
        code, _ = self.srv.post("/api/v1/gist/publish", {}, self.PROBE_HEADER)
        self.assertEqual(code, 401)
        self.assertEqual(db.get_observations(round_id), [])

    def test_result_stable_error_codes(self):
        round_id, payload = self._legal_payload()
        path = f"/api/v1/probes/hk3-a/jobs/{round_id}/result"
        # 路径轮次与报文轮次不一致
        code, body = self.srv.post(
            "/api/v1/probes/hk3-a/jobs/9999/result", payload, self.PROBE_HEADER)
        self.assertEqual(code, 400)
        self.assertEqual(json.loads(body)["error"], "ROUND_MISMATCH")
        # digest 不符
        stale = dict(payload, config_digest="sha256:other")
        code, body = self.srv.post(path, stale, self.PROBE_HEADER)
        self.assertEqual(code, 409)
        self.assertEqual(json.loads(body)["error"], "DIGEST_MISMATCH")
        # 旧轮次：再来一轮新的，这个 round 就被取代了
        db.start_round("test")
        code, body = self.srv.post(path, payload, self.PROBE_HEADER)
        self.assertEqual(code, 409)
        self.assertEqual(json.loads(body)["error"], "ROUND_STALE")
        # 过期结果
        fresh, fresh_payload = self._legal_payload()
        probe_dispatch.note_deadline("hk3-a", fresh, "2020-01-01T00:00:00Z")
        code, body = self.srv.post(
            f"/api/v1/probes/hk3-a/jobs/{fresh}/result", fresh_payload,
            self.PROBE_HEADER)
        self.assertEqual(code, 409)
        self.assertEqual(json.loads(body)["error"], "RESULT_LATE")
        # 每一次拒绝都有审计 ID 且只审计不落账
        self.assertTrue(json.loads(body)["audit_id"])
        self.assertEqual(db.get_observations(fresh), [])

    # -- 发布占位（P4/P5）------------------------------------------------

    def test_gist_publish_rejects_without_artifact_and_keeps_auth(self):
        # 未有 canonical 产物 → 409 + 审计（拒绝也留痕）；匿名与探针 token 都进不来。
        code, body = self.srv.post("/api/v1/gist/publish", {}, self.ADMIN_HEADER)
        self.assertEqual(code, 409)
        self.assertEqual(json.loads(body)["error"], "NO_CANONICAL_ARTIFACT")
        self.assertTrue(json.loads(body)["audit_id"])
        code, _ = self.srv.post("/api/v1/gist/publish", {}, None)
        self.assertEqual(code, 401)
        code, _ = self.srv.post("/api/v1/gist/publish", {}, self.PROBE_HEADER)
        self.assertEqual(code, 401)
        rows = db.list_publish_audit(10)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["kind"], "gist")
        self.assertEqual(rows[0]["status"], "no_artifact")

    def test_gist_publish_success_flow_and_dashboard(self):
        exports = cfgmod.DATA / "exports"
        exports.mkdir(parents=True, exist_ok=True)
        (exports / "mihomo-alive.yaml").write_bytes(b"proxies: []\n")
        (exports / "mihomo-alive.meta.json").write_text(
            json.dumps({"count": 0, "digest": "d1", "content_sha256": "s1"}),
            encoding="utf-8")
        captured = {}

        def fake_publish(cfg, content, meta, log=None, status_payload=None,
                         history_text=None, **kw):
            captured["content"], captured["meta"] = content, meta
            captured["status_payload"] = status_payload
            return {"status": "no_change", "digest": "d1",
                    "content_sha256": "s1", "revision": "r9"}

        with mock.patch.object(gist_store, "publish", fake_publish):
            code, body = self.srv.post("/api/v1/gist/publish", {},
                                       self.ADMIN_HEADER)
        self.assertEqual(code, 200)
        parsed = json.loads(body)
        self.assertEqual(parsed["status"], "no_change")
        self.assertEqual(parsed["revision"], "r9")
        self.assertEqual(parsed["digest"], "d1")
        # 磁盘上的 canonical 产物原样进发布器：不重渲染、不重建（§8 去重保证）
        self.assertEqual(captured["content"], b"proxies: []\n")
        self.assertEqual(captured["meta"]["digest"], "d1")
        self.assertEqual(captured["status_payload"]["digest"], "d1")
        rows = db.list_publish_audit(10)
        self.assertEqual(rows[0]["status"], "no_change")
        self.assertEqual(rows[0]["revision"], "r9")
        self.assertEqual(rows[0]["digest"], "d1")
        # dashboard 的 gist 块来自 publish_audit，不再是恒 null
        code, body = self.srv.get("/api/v1/dashboard", self.ADMIN_HEADER)
        self.assertEqual(code, 200)
        gist_block = json.loads(body)["gist"]
        self.assertEqual(gist_block["revision"], "r9")
        self.assertTrue(gist_block["consistent_with_stable_round"])

    def test_gist_publish_upstream_failure_is_audited_and_502(self):
        exports = cfgmod.DATA / "exports"
        exports.mkdir(parents=True, exist_ok=True)
        (exports / "mihomo-alive.yaml").write_bytes(b"proxies: []\n")
        (exports / "mihomo-alive.meta.json").write_text(
            json.dumps({"count": 0, "digest": "d1"}), encoding="utf-8")

        def fake_publish(cfg, *args, **kw):
            return {"status": "publish_failed", "error_code": "GIST_HTTP_500",
                    "message": "GitHub 返回 500"}

        with mock.patch.object(gist_store, "publish", fake_publish):
            code, body = self.srv.post("/api/v1/gist/publish", {},
                                       self.ADMIN_HEADER)
        self.assertEqual(code, 502)
        parsed = json.loads(body)
        self.assertEqual(parsed["error"], "GIST_PUBLISH_FAILED")
        self.assertEqual(parsed["error_code"], "GIST_HTTP_500")
        rows = db.list_publish_audit(10)
        self.assertEqual(rows[0]["status"], "publish_failed")
        summary = json.loads(rows[0]["summary_json"])
        self.assertEqual(summary["error_code"], "GIST_HTTP_500")

    def test_substore_push_requires_confirmation_and_audits(self):
        # 无 artifact：三个口都 409 NO_CANONICAL_ARTIFACT（拒绝也留痕）
        for action in ("preview", "confirm-token", "push"):
            with self.subTest(action=action):
                code, body = self.srv.post(f"/api/v1/substore/{action}", {},
                                           self.ADMIN_HEADER)
                self.assertEqual(code, 409)
                self.assertEqual(json.loads(body)["error"],
                                 "NO_CANONICAL_ARTIFACT")
                self.assertTrue(json.loads(body)["audit_id"])
        code, _ = self.srv.post("/api/v1/substore/push", {}, None)
        self.assertEqual(code, 401)
        # 有 artifact 但无有效 token → 409 PUSH_CONFIRM_REQUIRED（不再 501）
        (cfgmod.DATA / "exports").mkdir(parents=True, exist_ok=True)
        (cfgmod.DATA / "exports" / "mihomo-alive.yaml").write_bytes(
            b"proxies: []\n")
        (cfgmod.DATA / "exports" / "mihomo-alive.meta.json").write_text(
            json.dumps({"count": 1, "digest": "d1"}), encoding="utf-8")
        code, body = self.srv.post("/api/v1/substore/push",
                                   {"confirm_token": "nope"},
                                   self.ADMIN_HEADER)
        self.assertEqual(code, 409)
        self.assertEqual(json.loads(body)["error"], "PUSH_CONFIRM_REQUIRED")
        code, body = self.srv.post("/api/v1/substore/push", {},
                                   self.ADMIN_HEADER)
        self.assertEqual(code, 409)
        self.assertEqual(json.loads(body)["error"], "PUSH_CONFIRM_REQUIRED")
        rows = db.list_publish_audit(20)
        self.assertEqual({row["kind"] for row in rows}, {"substore"})
        self.assertEqual({row["target"] for row in rows},
                         {"mihomo-probe-alive"})
        # created_at 秒级分辨率下同秒行序由 uuid 决定，断言集合而非首行
        statuses = {row["status"] for row in rows}
        self.assertIn("rejected_PUSH_CONFIRM_REQUIRED", statuses)

    def test_substore_manual_push_full_flow(self):
        manual_publish.reset_tokens()
        self.addCleanup(manual_publish.reset_tokens)
        exports = cfgmod.DATA / "exports"
        exports.mkdir(parents=True, exist_ok=True)
        (exports / "mihomo-alive.yaml").write_bytes(
            b"proxies:\n  - {name: a, type: vless, server: s, port: 1,"
            b" uuid: u}\n")
        (exports / "mihomo-alive.meta.json").write_text(
            json.dumps({"count": 1, "digest": "d1"}), encoding="utf-8")

        class _StubStore:
            def __init__(self):
                self.calls = []

            def fetch_sub_proxies(self, name):
                raise NotFound("no such sub")

            def update_only(self, kind, name, payload):
                self.calls.append((kind, name, payload))
                return "updated"

        stub = _StubStore()
        with mock.patch.object(server, "store_client", lambda cfg: stub):
            # 1) 预览：全为新增
            code, body = self.srv.post("/api/v1/substore/preview", {},
                                       self.ADMIN_HEADER)
            self.assertEqual(code, 200)
            parsed = json.loads(body)
            self.assertTrue(parsed["missing_target"])
            self.assertEqual(parsed["preview"]["added"], 1)
            self.assertEqual(parsed["preview"]["samples"]["added"],
                             [{"name": "a", "type": "vless"}])
            # 2) 签发一次性确认 token
            code, body = self.srv.post("/api/v1/substore/confirm-token", {},
                                       self.ADMIN_HEADER)
            self.assertEqual(code, 200)
            token = json.loads(body)["confirm_token"]
            self.assertTrue(token)
            # 3) 推送：token 消费后 update-only 命中绑定对象
            code, body = self.srv.post("/api/v1/substore/push",
                                       {"confirm_token": token},
                                       self.ADMIN_HEADER)
            self.assertEqual(code, 200)
            parsed = json.loads(body)
            self.assertEqual(parsed["status"], "pushed")
            self.assertEqual(parsed["substore_action"], "updated")
            self.assertEqual(stub.calls,
                             [("sub", "mihomo-probe-alive",
                               mock.ANY)])
            self.assertEqual(stub.calls[0][2]["content"],
                             "proxies:\n  - {name: a, type: vless, server: s,"
                             " port: 1, uuid: u}\n")
        # 4) 同一 token 重放 → 409 CONFIRM_TOKEN_REUSED（防重放），审计留痕
        code, body = self.srv.post("/api/v1/substore/push",
                                   {"confirm_token": token},
                                   self.ADMIN_HEADER)
        self.assertEqual(code, 409)
        self.assertEqual(json.loads(body)["error"], "CONFIRM_TOKEN_REUSED")
        statuses = [row["status"] for row in db.list_publish_audit(20)]
        self.assertIn("preview", statuses)
        self.assertIn("confirm_issued", statuses)
        self.assertIn("pushed", statuses)
        self.assertIn("rejected_CONFIRM_TOKEN_REUSED", statuses)

    def test_substore_push_refuses_changed_content(self):
        # 签发后 canonical 变化（digest/节点数变）→ CONFIRM_TOKEN_MISMATCH
        manual_publish.reset_tokens()
        self.addCleanup(manual_publish.reset_tokens)
        exports = cfgmod.DATA / "exports"
        exports.mkdir(parents=True, exist_ok=True)
        (exports / "mihomo-alive.yaml").write_bytes(b"proxies: []\n")
        (exports / "mihomo-alive.meta.json").write_text(
            json.dumps({"count": 0, "digest": "d1"}), encoding="utf-8")
        with mock.patch.object(server, "store_client",
                               lambda cfg: type("S", (), {
                                   "fetch_sub_proxies": lambda self, n: [],
                                   "update_only": lambda self, k, n, p: "updated"})()):
            code, body = self.srv.post("/api/v1/substore/confirm-token", {},
                                       self.ADMIN_HEADER)
            token = json.loads(body)["confirm_token"]
            # 内容变化：digest 变了
            (exports / "mihomo-alive.meta.json").write_text(
                json.dumps({"count": 2, "digest": "d2"}), encoding="utf-8")
            code, body = self.srv.post("/api/v1/substore/push",
                                       {"confirm_token": token},
                                       self.ADMIN_HEADER)
        self.assertEqual(code, 409)
        self.assertEqual(json.loads(body)["error"], "CONFIRM_TOKEN_MISMATCH")

    def test_api_push_requires_confirm_token_too(self):
        # 旧的一键推送收编进同一条 confirm-token 路（§0.6：无绕过人工确认的推送）
        exports = cfgmod.DATA / "exports"
        exports.mkdir(parents=True, exist_ok=True)
        (exports / "mihomo-alive.yaml").write_bytes(b"proxies: []\n")
        (exports / "mihomo-alive.meta.json").write_text(
            json.dumps({"count": 0, "digest": "d1"}), encoding="utf-8")
        code, body = self.srv.post("/api/push", {}, self.ADMIN_HEADER)
        self.assertEqual(code, 409)
        self.assertEqual(json.loads(body)["error"], "PUSH_CONFIRM_REQUIRED")


class _StubController:
    """Captures the ProbeResult an agent posts back (指导 §4.2 回传路由)。"""

    def __init__(self):
        self.requests = []
        self._lock = threading.Lock()
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, fmt, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length)
                with outer._lock:
                    outer.requests.append({"path": self.path,
                                           "headers": dict(self.headers),
                                           "body": raw})
                payload = b'{"ok": true}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever,
                                       daemon=True)
        self.thread.start()

    def wait_for_result(self, timeout=15.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self._lock:
                if self.requests:
                    return self.requests[-1]
            time.sleep(0.05)
        return None

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


class AgentModeTest(unittest.TestCase):
    """agent 模式：收 Job、测、回传（指导 §4.1/§4.2、§13.1/§13.2）。

    Kernel start/stop and Core.delay are patched: what is under test is the
    agent's protocol surface and its ProbeResult assembly, not docker.
    """

    def setUp(self):
        db.connect()
        # 探测点的内核配置目录落在 ROOT 下（docker -v 要宿主路径），测试里把
        # ROOT 指到一次性目录，避免在真实 ROOT 下留下任何东西。
        self._old_root = cfgmod.ROOT
        cfgmod.ROOT = Path(tempfile.mkdtemp(prefix="mihomo-agent-root-"))
        self.addCleanup(self._restore_root)
        self.controller = _StubController()
        self.addCleanup(self.controller.close)
        opts = agent_mod.AgentOptions(
            probe_id="hk3-a", token=PROBE_TOKEN,
            controller=f"http://127.0.0.1:{self.controller.port}",
            port=0, timeout_ms=1000, concurrency=2,
            container="mihomo-agent-test", api_port=29192,
            mixed_port=29196, base_port=29320)
        self.httpd = agent_mod.agent_serve(opts)
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever,
                                       daemon=True)
        self.thread.start()
        # 清理顺序：addCleanup 是 LIFO——先注册 server_close、后注册
        # shutdown，执行时才会先停 serve_forever 循环再关监听套接字。反过来
        # 会在 Windows 上让循环里的 select() 撞上已关闭的套接字
        # （WinError 10038 / ConnectionAbortedError 噪音，非失败但难看）。
        self.addCleanup(self.httpd.server_close)
        self.addCleanup(self.httpd.shutdown)

    def _restore_root(self):
        shutil.rmtree(cfgmod.ROOT, ignore_errors=True)
        cfgmod.ROOT = self._old_root

    def _post_job(self, job, token=PROBE_TOKEN, ts=None, tamper=False):
        body = json.dumps(job).encode("utf-8")
        ts = str(int(time.time())) if ts is None else str(ts)
        signature = probe_dispatch.sign_request(token, body, ts)
        if tamper:
            body = body + b" "
        req = urllib.request.Request(
            f"http://127.0.0.1:{self.port}/jobs", data=body, method="POST",
            headers={"Content-Type": "application/json",
                     "Authorization": f"Bearer {token}",
                     probe_dispatch.TIMESTAMP_HEADER: ts,
                     probe_dispatch.SIGNATURE_HEADER: signature})
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return resp.status, json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            return exc.code, json.loads(exc.read().decode("utf-8") or "{}")

    def _job(self):
        return {
            "round_id": db.start_round("test"),
            "probe_id": "hk3-a",
            "config_digest": "sha256:test",
            "deadline": "2030-01-01T00:00:00Z",
            "chain_enabled": False,
            "nodes": [
                {"fingerprint": "fp-alive-1", "mihomo_name": "alive-ok-1",
                 "proxy": {"name": "alive-ok-1", "type": "ss",
                           "server": "127.0.0.1", "port": 1234,
                           "cipher": "aes-128-gcm", "password": "s3cret"}},
                {"fingerprint": "fp-alive-2", "mihomo_name": "alive-ok-2",
                 "proxy": {"name": "alive-ok-2", "type": "ss",
                           "server": "127.0.0.1", "port": 1235,
                           "cipher": "aes-128-gcm", "password": "s3cret"}},
                {"fingerprint": "fp-dead-1", "mihomo_name": "dead-1",
                 "proxy": {"name": "dead-1", "type": "ss",
                           "server": "127.0.0.1", "port": 1236,
                           "cipher": "aes-128-gcm", "password": "s3cret"}},
                {"fingerprint": "fp-bad-1", "mihomo_name": "bad-1",
                 "proxy": {"name": "bad-1", "type": "ss"}},
            ],
        }

    def _fake_delay(self, name, url, timeout_ms, expected="204"):
        if "ok" in name:
            return 150, None, ""
        return None, "timeout", "fake timeout"

    def test_agent_runs_job_and_posts_result(self):
        job = self._job()
        # patch 必须活到结果回传为止：agent 是在后台线程里执行 Job 的，POST
        # 一返回就撤 patch，等于让真的 docker 路径跑起来。
        patches = [
            mock.patch.object(agent_mod, "_start_kernel",
                              lambda core_cfg, secret, host_dir, log:
                              coremod.Core(core_cfg, secret)),
            mock.patch.object(agent_mod, "_stop_kernel",
                              lambda *args, **kwargs: None),
            mock.patch.object(coremod.Core, "delay", self._fake_delay),
        ]
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)
        code, body = self._post_job(job)
        self.assertEqual(code, 202)
        self.assertTrue(body["accepted"])
        request = self.controller.wait_for_result()
        self.assertIsNotNone(request, "agent 未在超时内回传结果")
        self.assertEqual(
            request["path"],
            f"/api/v1/probes/hk3-a/jobs/{job['round_id']}/result")
        self.assertEqual(request["headers"].get("Authorization"),
                         f"Bearer {PROBE_TOKEN}")
        self.assertEqual(request["headers"].get("Content-Type"),
                         "application/json")
        payload = json.loads(request["body"].decode("utf-8"))
        self.assertEqual(payload["round_id"], job["round_id"])
        self.assertEqual(payload["probe_id"], "hk3-a")
        self.assertEqual(payload["config_digest"], "sha256:test")
        self.assertIsNotNone(probe_dispatch.parse_iso8601(
            payload["started_at"]))
        self.assertIsNotNone(probe_dispatch.parse_iso8601(
            payload["finished_at"]))
        by_fp = {item["fingerprint"]: item for item in payload["results"]}
        self.assertEqual(len(by_fp), 4)
        self.assertTrue(by_fp["fp-alive-1"]["alive"])
        self.assertEqual(by_fp["fp-alive-1"]["delay_ms"], 150)
        self.assertIsNone(by_fp["fp-alive-1"]["reason"])
        self.assertTrue(by_fp["fp-alive-2"]["alive"])
        self.assertFalse(by_fp["fp-dead-1"]["alive"])
        self.assertEqual(by_fp["fp-dead-1"]["reason"], "timeout")
        self.assertIsNone(by_fp["fp-dead-1"]["delay_ms"])
        # 内核配置就拒绝的节点：判败且带原因，不当成超时
        self.assertFalse(by_fp["fp-bad-1"]["alive"])
        self.assertTrue(by_fp["fp-bad-1"]["reason"].startswith("dropped:"))
        # agent 不做出口验证：egress_country 统一 null（Controller 侧统一做）
        for item in payload["results"]:
            self.assertIsNone(item["egress_country"])
        # 临时内核配置目录已清理（指导 §4.1）
        leftovers = list((cfgmod.ROOT / agent_mod.AGENT_CORE_SUBDIR
                          ).glob("job-*"))
        self.assertEqual(leftovers, [])

    def test_agent_healthz(self):
        code, body = self.srv_get("/healthz")
        self.assertEqual(code, 200)
        self.assertEqual(json.loads(body), {"id": "hk3-a", "ok": True})

    def srv_get(self, path):
        req = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}")
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return resp.status, resp.read().decode("utf-8")
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read().decode("utf-8")

    def test_agent_rejects_bad_token_and_bad_signature(self):
        job = self._job()
        code, _ = self._post_job(job, token="wrong-token")
        self.assertEqual(code, 401)
        code, body = self._post_job(job, ts=int(time.time()) - 600)
        self.assertEqual(code, 403)
        self.assertEqual(body["error"], "SIGNATURE_STALE")
        code, body = self._post_job(job, tamper=True)
        self.assertEqual(code, 403)
        self.assertEqual(body["error"], "SIGNATURE_INVALID")
        # 一个都没被接受
        self.assertIsNone(self.controller.wait_for_result(timeout=1.0))

    def test_agent_refuses_a_second_concurrent_job(self):
        started = threading.Event()
        release = threading.Event()

        def blocking_execute(job, opts, log):
            started.set()
            release.wait(10)

        job = self._job()
        with mock.patch.object(agent_mod, "_execute_job", blocking_execute):
            code, _ = self._post_job(job)
            self.assertEqual(code, 202)
            self.assertTrue(started.wait(10))
            code, body = self._post_job(self._job())
            self.assertEqual(code, 409)
            self.assertEqual(body["error"], "BUSY")
            release.set()

    def test_agent_does_not_read_the_business_config(self):
        """探测点启动不加载 config.json：连坏配置都不该拦住它。"""
        cfgmod.CONFIG_PATH.write_text("{ not json", encoding="utf-8")
        with mock.patch.dict(os.environ, {PROBE_TOKEN_ENV: PROBE_TOKEN}):
            httpd = agent_mod.agent_serve(agent_mod.AgentOptions(
                probe_id="hk3-a", token=PROBE_TOKEN,
                controller="http://127.0.0.1:1", port=0))
        # 这个实例没有 serve_forever 线程：shutdown() 会永远等一个不会到来
        # 的循环退出，所以只关 socket。
        self.addCleanup(httpd.server_close)
        self.assertEqual(httpd.opts.probe_id, "hk3-a")
