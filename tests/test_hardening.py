"""Regression tests for the 2026-09-21 audit fixes.

Each class here pins one defect that was found by reading the code rather than
by a failure, so without these the fix would be unverifiable and the bug would
come back silently. The pattern throughout: state the failure mode that was
actually observed, then assert the property that rules it out.

Run with the rest of the offline suite:
    python3 -m unittest discover -s tests
"""
import json
import os
import re
import shutil
import sys
import tempfile
import threading
import time
import unittest
import unittest.mock
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import config as cfgmod
from mihomo_test import core as coremod
from mihomo_test import db, engine, notifier, policy, server, ui

try:
    import _isolation
except ImportError:  # imported as a package: python -m unittest tests.test_hardening
    from tests import _isolation


def setUpModule():
    _isolation.isolate()


def tearDownModule():
    _isolation.restore()


_nolog = lambda *a, **k: None


class _TempRoot:
    """Point the app's data directory at a throwaway path for one test.

    `config.DATA` is read by several modules at call time, so patching it here
    is enough to keep a test off the deployment's real ledger.
    """

    def __enter__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="mihomo-test-hardening-"))
        self._old = {
            "data": cfgmod.DATA,
            "config": cfgmod.CONFIG_PATH,
            "db_path": db.DB_PATH,
            "db_conn": db._conn,
            "round_state": engine.ROUND_STATE,
            "export_dir": engine.EXPORT_DIR,
            "notify_state": notifier.STATE_PATH,
        }
        cfgmod.DATA = self.tmp
        cfgmod.CONFIG_PATH = self.tmp / "config.json"
        db.DB_PATH, db._conn = self.tmp / "state.db", None
        engine.ROUND_STATE = self.tmp / "round.state.json"
        engine.EXPORT_DIR = self.tmp / "exports"
        notifier.STATE_PATH = self.tmp / "alert-state.json"
        return self.tmp

    def __exit__(self, *exc):
        if db._conn is not None:
            db._conn.close()
        cfgmod.DATA = self._old["data"]
        cfgmod.CONFIG_PATH = self._old["config"]
        db.DB_PATH, db._conn = self._old["db_path"], self._old["db_conn"]
        engine.ROUND_STATE = self._old["round_state"]
        engine.EXPORT_DIR = self._old["export_dir"]
        notifier.STATE_PATH = self._old["notify_state"]
        shutil.rmtree(self.tmp, ignore_errors=True)
        return False


class ConfigTokenTest(unittest.TestCase):
    """A config that cannot be read must not silently disable authentication."""

    def test_a_truncated_config_file_still_yields_both_tokens(self):
        """The P0: an unparseable config.json fell through to DEFAULTS.

        `load()` only minted a token when the file was *absent*. A file that
        existed but failed to parse hit `except (OSError, ValueError): stored =
        {}`, so the merge produced an empty `auth.token` -- and `auth_ok` read a
        falsy token as "no auth configured" and allowed everything. On a
        tunnelled deployment that is an anonymous dashboard, an anonymous
        /api/export/*.yaml (every live node's credentials) and an anonymous
        POST /api/config.
        """
        with _TempRoot() as tmp:
            (tmp / "config.json").write_text('{"auth": {"token": "abc', encoding="utf-8")
            cfg = cfgmod.load()
            self.assertGreaterEqual(len(cfg["auth"]["token"]), cfgmod.MIN_TOKEN_LEN)
            self.assertGreaterEqual(len(cfg["publish"]["token"]), cfgmod.MIN_TOKEN_LEN)
            # ...and it is persisted, or every restart would invalidate every
            # URL already handed out.
            stored = json.loads((tmp / "config.json").read_text(encoding="utf-8"))
            self.assertEqual(stored["auth"]["token"], cfg["auth"]["token"])
            self.assertEqual(stored["publish"]["token"], cfg["publish"]["token"])

    def test_a_missing_config_file_yields_both_tokens(self):
        with _TempRoot() as tmp:
            cfg = cfgmod.load()
            self.assertTrue(cfg["auth"]["token"])
            self.assertTrue(cfg["publish"]["token"])
            self.assertTrue((tmp / "config.json").exists())

    def test_an_env_supplied_admin_token_is_kept_and_a_publish_token_is_added(self):
        with _TempRoot() as tmp:
            (tmp / "config.json").write_text(
                json.dumps({"auth": {"token": "a" * 32}}), encoding="utf-8")
            cfg = cfgmod.load()
            self.assertEqual(cfg["auth"]["token"], "a" * 32)
            self.assertTrue(cfg["publish"]["token"])
            self.assertNotEqual(cfg["publish"]["token"], cfg["auth"]["token"])

    def test_the_two_tokens_are_never_the_same(self):
        """The publish token exists to be handed out; sharing one defeats it."""
        with _TempRoot():
            cfg = cfgmod.load()
            self.assertNotEqual(cfg["auth"]["token"], cfg["publish"]["token"])


class ValidatePatchTest(unittest.TestCase):
    """A patch must not be able to write a setting that breaks or endangers."""

    def test_core_container_cannot_be_changed_through_the_api(self):
        """`core.container` is passed straight to `docker restart`.

        The app holds a mounted /var/run/docker.sock, so a remotely editable
        container name turns a token leak into "stop any container on the host".
        Deployments set it with MIHOMO_TEST_CORE_CONTAINER instead.
        """
        clean, notes = cfgmod.validate_patch(
            {"core": {"container": "sub-store", "mixed_port": 19194}})
        self.assertNotIn("container", clean["core"])
        self.assertEqual(clean["core"]["mixed_port"], 19194,
                         "the immutable list must not prune unrelated keys")
        self.assertTrue(any("core.container" in n for n in notes), notes)

    def test_an_empty_target_list_is_refused(self):
        """`test_one` raises ValueError on it, so every round would then fail.

        The failure surfaced only as one `error` line every 30 minutes; nothing
        in the panel said the config could not run.
        """
        clean, notes = cfgmod.validate_patch({"test": {"targets": ["", "  "]}})
        self.assertEqual(clean["test"]["targets"], cfgmod.DEFAULTS["test"]["targets"])
        self.assertTrue(any("targets" in n for n in notes), notes)

    def test_a_real_target_list_is_left_alone(self):
        clean, notes = cfgmod.validate_patch({"test": {"targets": ["https://a/204"]}})
        self.assertEqual(clean["test"]["targets"], ["https://a/204"])
        self.assertEqual(notes, [])

    def test_the_alert_cooldown_is_clamped(self):
        """It used to be unvalidated, and `notifier.send` does `int()` on it."""
        clean, _ = cfgmod.validate_patch({"alert": {"cooldown_minutes": 999999}})
        self.assertEqual(clean["alert"]["cooldown_minutes"],
                         cfgmod.NUMERIC_BOUNDS["alert.cooldown_minutes"][1])

    def test_a_non_numeric_alert_cooldown_falls_back_to_the_default(self):
        clean, notes = cfgmod.validate_patch({"alert": {"cooldown_minutes": "soon"}})
        self.assertEqual(clean["alert"]["cooldown_minutes"],
                         cfgmod.DEFAULTS["alert"]["cooldown_minutes"])
        self.assertTrue(any("cooldown" in n for n in notes), notes)

    def test_a_short_publish_token_is_refused(self):
        clean, notes = cfgmod.validate_patch({"publish": {"token": "short"}})
        self.assertNotIn("token", clean.get("publish", {}))
        self.assertTrue(any("publish.token" in n for n in notes), notes)

    def test_a_deployment_override_still_passes(self):
        """The immutable list must not become a DEFAULTS whitelist."""
        patch = {"core": {"mixed_port": 19194, "probe_group": "__PROBE__"}}
        clean, notes = cfgmod.validate_patch(patch)
        self.assertEqual(clean["core"], {"mixed_port": 19194})
        self.assertTrue(any("已废弃" in n for n in notes), notes)


class _HandlerStub:
    """Just enough of a BaseHTTPRequestHandler for `auth_ok`."""

    def __init__(self, path, headers=None):
        self.path = path
        self.headers = headers or {}


class AuthLogicTest(unittest.TestCase):
    """`auth_ok` is the only security boundary in the service."""

    def cfg(self, admin="a" * 32, publish="p" * 32):
        return {"auth": {"token": admin}, "publish": {"token": publish}}

    def test_an_empty_admin_token_denies_instead_of_allowing(self):
        cfg = self.cfg(admin="")
        self.assertFalse(server.auth_ok(_HandlerStub("/api/status"), cfg, "/api/status"))
        self.assertFalse(server.auth_ok(_HandlerStub("/api/status?token="), cfg, "/api/status"))

    def test_the_admin_token_is_accepted_in_all_three_forms(self):
        cfg = self.cfg()
        self.assertTrue(server.auth_ok(_HandlerStub(f"/api/status?token={'a' * 32}"),
                                       cfg, "/api/status"))
        self.assertTrue(server.auth_ok(_HandlerStub("/api/status", {"X-Auth-Token": "a" * 32}),
                                       cfg, "/api/status"))
        self.assertTrue(server.auth_ok(_HandlerStub("/api/status", {"Authorization": "Bearer " + "a" * 32}),
                                       cfg, "/api/status"))

    def test_a_wrong_token_is_refused(self):
        cfg = self.cfg()
        self.assertFalse(server.auth_ok(_HandlerStub(f"/api/status?token={'b' * 32}"),
                                        cfg, "/api/status"))
        self.assertFalse(server.auth_ok(_HandlerStub("/api/status", {"Authorization": "Bearer "}),
                                        cfg, "/api/status"))

    def test_the_publish_token_reads_exports_but_nothing_else(self):
        """This is the point of splitting the credential.

        The export URL is pasted into Sub-Store and rendered into the page, so
        it must not carry the credential that can rewrite the config.
        """
        cfg = self.cfg()
        self.assertTrue(server.auth_ok(_HandlerStub(f"/api/export/air.yaml?token={'p' * 32}"),
                                       cfg, "/api/export/air.yaml"))
        self.assertFalse(server.auth_ok(_HandlerStub(f"/api/config?token={'p' * 32}"),
                                        cfg, "/api/config"))
        self.assertFalse(server.auth_ok(_HandlerStub(f"/api/status?token={'p' * 32}"),
                                        cfg, "/api/status"))

    def test_the_admin_token_still_reads_exports(self):
        """Backwards compatibility: Sub-Store links minted before the split."""
        cfg = self.cfg()
        self.assertTrue(server.auth_ok(_HandlerStub(f"/api/export/air.yaml?token={'a' * 32}"),
                                       cfg, "/api/export/air.yaml"))

    def test_an_empty_publish_token_does_not_open_exports(self):
        cfg = self.cfg(publish="")
        self.assertFalse(server.auth_ok(_HandlerStub("/api/export/air.yaml?token="),
                                        cfg, "/api/export/air.yaml"))

    def test_a_non_ascii_token_authenticates_instead_of_crashing(self):
        """`compare_digest` raises TypeError on a non-ASCII str."""
        cfg = self.cfg(admin="令牌" * 10)
        self.assertTrue(server.auth_ok(_HandlerStub("/api/status", {"X-Auth-Token": "令牌" * 10}),
                                       cfg, "/api/status"))
        self.assertFalse(server.auth_ok(_HandlerStub("/api/status", {"X-Auth-Token": "别的" * 10}),
                                        cfg, "/api/status"))


class _LiveServer:
    """A real loopback instance of the service, for the HTTP-level assertions.

    `server.py` had no offline coverage at all: its routes, its auth wiring and
    its response headers were only ever exercised against a live deployment,
    which cannot run on a dev machine.

    The data directory and the config file are redirected per instance, because
    `POST /api/config` rewrites `config.json` -- pointing that at the real one
    is how this suite used to modify a running deployment.
    """

    def __init__(self, cfg):
        self.tmp = Path(tempfile.mkdtemp(prefix="mihomo-test-http-"))
        self._old = {"db_path": db.DB_PATH, "db_conn": db._conn,
                     "export_dir": engine.EXPORT_DIR, "state": server._State.cfg,
                     "data": cfgmod.DATA, "config_path": cfgmod.CONFIG_PATH}
        db.DB_PATH, db._conn = self.tmp / "state.db", None
        engine.EXPORT_DIR = self.tmp / "exports"
        engine.EXPORT_DIR.mkdir(parents=True, exist_ok=True)
        cfgmod.DATA = self.tmp
        cfgmod.CONFIG_PATH = self.tmp / "config.json"
        cfgmod.save(cfg)
        self.cfg = cfg
        self.httpd = server.serve(cfg, "127.0.0.1", 0)
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    def stored_config(self):
        return json.loads((self.tmp / "config.json").read_text(encoding="utf-8"))

    def url(self, path):
        return f"http://127.0.0.1:{self.port}{path}"

    def get(self, path):
        req = urllib.request.Request(self.url(path), headers={"User-Agent": "test"})
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return resp.status, resp.read().decode("utf-8"), dict(resp.headers)
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read().decode("utf-8"), dict(exc.headers)

    def post(self, path, payload=None):
        body = json.dumps(payload or {}).encode("utf-8")
        req = urllib.request.Request(self.url(path), data=body, method="POST",
                                     headers={"User-Agent": "test",
                                              "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return resp.status, json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            return exc.code, json.loads(exc.read().decode("utf-8") or "{}")

    def request(self, path, method="GET", headers=None, payload=None):
        """An arbitrary request, so Origin / OPTIONS can be exercised.

        `get` and `post` deliberately stay as they are -- they are used by every
        other assertion here and the extra headers would change nothing for
        them. Cross-origin behaviour needs its own entry point because a
        same-origin request simply never sends an `Origin`.
        """
        data = None if payload is None else json.dumps(payload).encode("utf-8")
        merged = {"User-Agent": "test"}
        merged.update(headers or {})
        if data is not None:
            merged["Content-Type"] = "application/json"
        req = urllib.request.Request(self.url(path), data=data, method=method,
                                     headers=merged)
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return resp.status, resp.read().decode("utf-8"), dict(resp.headers)
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read().decode("utf-8"), dict(exc.headers)

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()
        if db._conn is not None:
            db._conn.close()
        db.DB_PATH, db._conn = self._old["db_path"], self._old["db_conn"]
        engine.EXPORT_DIR = self._old["export_dir"]
        server._State.cfg = self._old["state"]
        cfgmod.DATA = self._old["data"]
        cfgmod.CONFIG_PATH = self._old["config_path"]
        shutil.rmtree(self.tmp, ignore_errors=True)


class HttpSurfaceTest(unittest.TestCase):
    """The served surface: auth wiring, redaction, headers."""

    ADMIN = "a" * 32
    PUBLISH = "p" * 32

    def setUp(self):
        self.cfg = {
            "auth": {"token": self.ADMIN},
            "publish": {"token": self.PUBLISH, "prefix": "probe",
                        "hostname": "probe.example", "enabled": True},
            "ui": {"title": "测试面板"},
            "sources": [{"key": "air", "name": "air", "kind": "collection",
                         "enabled": True}],
            "schedule": {"enabled": False, "interval_minutes": 30},
            "core": {"container": "mihomo-probe"},
            "alert": {"enabled": False},
            "test": {"targets": ["https://a/204"]},
        }
        self.srv = _LiveServer(self.cfg)
        self.addCleanup(self.srv.close)

    def test_healthz_needs_no_token(self):
        code, body, _ = self.srv.get("/healthz")
        self.assertEqual(code, 200)
        self.assertTrue(json.loads(body)["ok"])

    def test_everything_else_401s_without_a_token(self):
        for path in ("/api/status", "/api/nodes", "/api/logs", "/",
                     "/api/export/air.yaml"):
            with self.subTest(path=path):
                code, _, _ = self.srv.get(path)
                self.assertEqual(code, 401, path)

    def test_the_dashboard_renders_for_the_admin_token(self):
        code, body, _ = self.srv.get(f"/?token={self.ADMIN}")
        self.assertEqual(code, 200)
        self.assertIn("测试面板", body)
        self.assertIn(self.ADMIN, body, "the page must embed the token it was loaded with")

    def test_status_never_echoes_the_admin_token(self):
        """It is polled every 5s and its payload is one screenshot from public."""
        code, body, _ = self.srv.get(f"/api/status?token={self.ADMIN}")
        self.assertEqual(code, 200)
        payload = json.loads(body)
        self.assertEqual(payload["config"]["auth"]["token"], "***")
        self.assertNotIn(self.ADMIN, body)
        # The publish token is a lesser credential -- it only reads
        # `/api/export/*` -- but the settings form never reads it back, so a
        # copy of it in a five-second poll buys nothing.
        self.assertEqual(payload["config"]["publish"]["token"], "***")
        # It *does* still appear in `exports[].url`, and that is the point of
        # it: that URL is what the operator copies into Sub-Store, so it has to
        # carry the read-only credential. Masking it there would break the one
        # thing the panel exists to do -- the separation that matters is that
        # this token cannot change anything.
        self.assertIn(self.PUBLISH, payload["exports"][0]["url"])
        self.assertIn("config", payload)

    def test_export_is_readable_with_the_publish_token(self):
        (engine.EXPORT_DIR / "air.yaml").write_text("proxies: []\n", encoding="utf-8")
        code, body, _ = self.srv.get(f"/api/export/air.yaml?token={self.PUBLISH}")
        self.assertEqual(code, 200)
        self.assertIn("proxies", body)

    def test_export_rejects_an_unknown_token(self):
        (engine.EXPORT_DIR / "air.yaml").write_text("proxies: []\n", encoding="utf-8")
        code, _, _ = self.srv.get(f"/api/export/air.yaml?token={'z' * 32}")
        self.assertEqual(code, 401)

    def test_responses_carry_the_security_headers(self):
        _, _, headers = self.srv.get("/healthz")
        lowered = {k.lower(): v for k, v in headers.items()}
        self.assertEqual(lowered.get("x-content-type-options"), "nosniff")
        self.assertEqual(lowered.get("x-frame-options"), "DENY")
        self.assertEqual(lowered.get("referrer-policy"), "no-referrer")
        self.assertIn("frame-ancestors 'none'", lowered.get("content-security-policy", ""))
        self.assertEqual(lowered.get("cache-control"), "no-store")

    def test_core_container_survives_a_config_post_unchanged(self):
        """Asserted against the file the service actually rewrites.

        `POST /api/config` calls `config.update`, which merges into whatever
        `load()` reads from disk and saves the result -- so checking the
        response body alone would not prove the stored value was protected.
        """
        code, payload = self.srv.post(f"/api/config?token={self.ADMIN}",
                                      {"core": {"container": "sub-store"}})
        self.assertEqual(code, 200)
        self.assertTrue(any("core.container" in n for n in payload.get("notes", [])),
                        payload.get("notes"))
        self.assertEqual(self.srv.stored_config()["core"]["container"], "mihomo-probe")
        self.assertEqual(payload["config"]["core"]["container"], "mihomo-probe")

    def test_run_rejects_an_unknown_mode(self):
        """`mode` is only ever compared, so a typo would run the *other* round.

        `run_round` tests `mode == "direct"` / `mode == "chain"` and nothing
        else; an unrecognized value falls through to a full round while
        `rounds.mode` records the typo and the header falls back to a label
        reading 「测试」. argparse already rejects it on the CLI; the endpoint
        has to do the same or a direct `curl` writes the bad value.
        """
        started = []
        with unittest.mock.patch.object(
                server, "run_in_background",
                lambda cfg, **k: started.append(k)):
            for bad in ("Direct", "full", ""):
                with self.subTest(mode=bad):
                    code, payload = self.srv.post(f"/api/run?token={self.ADMIN}",
                                                  {"mode": bad})
                    self.assertEqual(code, 400, payload)
            self.assertEqual(started, [], "一个被拒绝的 mode 仍然启动了一轮")

            # mode=chain is only forwardable once chaining is actually
            # configured; unconfigured it is refused outright -- see
            # `test_chain_mode_is_refused_while_chaining_is_unconfigured`.
            code, _ = self.srv.post(f"/api/config?token={self.ADMIN}",
                                    {"chain": {"enabled": True,
                                               "front_source": {"kind": "sub",
                                                                "name": "cm-xhttp"}}})
            self.assertEqual(code, 200)
            code, payload = self.srv.post(f"/api/run?token={self.ADMIN}",
                                          {"mode": "chain"})
            self.assertEqual(code, 200, payload)
            self.assertEqual([k["mode"] for k in started], ["chain"])

            code, payload = self.srv.post(f"/api/run?token={self.ADMIN}", {})
            self.assertEqual(code, 200, payload)
            self.assertIsNone(started[-1]["mode"])

    def test_chain_mode_is_refused_while_chaining_is_unconfigured(self):
        """The 链式测活 button used to start a round it knew was a 直连测活.

        With `chain_block` returning None the round measures every node direct
        and only explains itself in a log line -- three minutes and a full dial
        of every node after the click. The endpoint knows the round is doomed
        to be something else, so it refuses at the edge with the fix, and the
        same gate (`engine.chain_block`) the round itself applies keeps the two
        from ever disagreeing.
        """
        started = []
        with unittest.mock.patch.object(
                server, "run_in_background",
                lambda cfg, **k: started.append(k)):
            code, payload = self.srv.post(f"/api/run?token={self.ADMIN}",
                                          {"mode": "chain"})
            self.assertEqual(code, 400, payload)
            self.assertIn("链式未生效", payload["error"])
            self.assertIn("chain.enabled", payload["error"])
            self.assertEqual(started, [], "被拒绝的链式测活仍然启动了一轮")

            # Half-configured is refused too, with the other reason.
            code, _ = self.srv.post(f"/api/config?token={self.ADMIN}",
                                    {"chain": {"enabled": True}})
            self.assertEqual(code, 200)
            code, payload = self.srv.post(f"/api/run?token={self.ADMIN}",
                                          {"mode": "chain"})
            self.assertEqual(code, 400, payload)
            self.assertIn("前置来源", payload["error"])

            code, _ = self.srv.post(f"/api/config?token={self.ADMIN}",
                                    {"chain": {"front_source": {"kind": "sub",
                                                                "name": "cm-xhttp"}}})
            self.assertEqual(code, 200)
            code, payload = self.srv.post(f"/api/run?token={self.ADMIN}",
                                          {"mode": "chain"})
            self.assertEqual(code, 200, payload)
            self.assertEqual([k["mode"] for k in started], ["chain"])

    def test_push_is_refused_while_publishing_is_disabled(self):
        """A push reads whatever file is on disk; with publish off that is a
        stale snapshot no round maintains, and upserting it hands Sub-Store
        subscriptions that serve nodes nothing keeps current -- reported as
        success, which reads as a working integration built on stale data."""
        (engine.EXPORT_DIR / "air.yaml").write_text("proxies: []\n", encoding="utf-8")
        code, _ = self.srv.post(f"/api/config?token={self.ADMIN}",
                                {"publish": {"enabled": False}})
        self.assertEqual(code, 200)
        code, payload = self.srv.post(f"/api/push?token={self.ADMIN}", {})
        self.assertEqual(code, 400, payload)
        self.assertIn("publish.enabled", payload["error"])

    def test_a_measurement_only_sources_save_skips_the_substore_sync(self):
        """Flipping 直连/链式 cannot change what `link_substore` would write.

        Every sources save used to pay a full sub listing plus one upsert per
        source -- on this fixture's single source that is two backend requests
        per checkbox click, for an outcome that is byte-identical, and the
        sync messages then rode along on a notice about a measurement toggle.
        """
        calls = []
        with unittest.mock.patch.object(
                engine, "link_substore",
                lambda *a, **k: calls.append(a) or []):
            code, payload = self.srv.post(
                f"/api/config?token={self.ADMIN}",
                {"sources": [{"key": "air", "name": "air", "kind": "collection",
                              "enabled": True, "direct": False, "chain": True}]})
            self.assertEqual(code, 200, payload)
            self.assertEqual(calls, [], "纯测量开关的保存触发了一次 Sub-Store 同步")
            # The switch itself must still have been saved.
            stored = self.srv.stored_config()["sources"]
            self.assertEqual([s.get("direct") for s in stored], [False])

            # A link-relevant change still syncs immediately.
            code, payload = self.srv.post(
                f"/api/config?token={self.ADMIN}",
                {"sources": [{"key": "air", "name": "air", "kind": "collection",
                              "enabled": False}]})
            self.assertEqual(code, 200, payload)
            self.assertEqual(len(calls), 1)

    def test_a_normal_setting_still_saves(self):
        code, _ = self.srv.post(f"/api/config?token={self.ADMIN}",
                                {"schedule": {"interval_minutes": 45}})
        self.assertEqual(code, 200)
        self.assertEqual(self.srv.stored_config()["schedule"]["interval_minutes"], 45)

    def test_a_config_post_needs_the_admin_token(self):
        code, _ = self.srv.post(f"/api/config?token={self.PUBLISH}",
                                {"schedule": {"interval_minutes": 60}})
        self.assertEqual(code, 401)

    def test_nodes_endpoint_answers_with_the_trend_field(self):
        code, body, _ = self.srv.get(f"/api/nodes?token={self.ADMIN}")
        self.assertEqual(code, 200)
        self.assertIn("nodes", json.loads(body))


class RoundLockTest(unittest.TestCase):
    """`BUSY` must be acquired atomically, and released only by its owner."""

    def setUp(self):
        self.cfg = {"auth": {"token": "a" * 32}, "sources": []}

    def test_a_second_start_is_refused_and_does_not_release_the_first(self):
        """The loser used to clear the winner's hold in its `finally`.

        After that the panel reported "not running", the scheduler started a
        fresh thread every poll (each logging "已有一轮在运行"), and a manual run
        answered `{"started": true}` while doing nothing.
        """
        release = threading.Event()
        started = threading.Event()

        def slow_round(*a, **k):
            started.set()
            release.wait(10)
            return {"alive": 1}

        with unittest.mock.patch.object(engine, "run_round", slow_round):
            server.run_in_background(self.cfg, trigger="test")
            self.assertTrue(started.wait(5))
            self.assertTrue(server.BUSY.locked())
            with self.assertRaises(engine.Busy):
                server.run_in_background(self.cfg, trigger="test")
            self.assertTrue(server.BUSY.locked(),
                            "the refused caller released a lock it never took")
            release.set()
            for _ in range(100):
                if not server.BUSY.locked():
                    break
                time.sleep(0.05)
            self.assertFalse(server.BUSY.locked(), "the winner never released")

    def test_a_crashing_round_still_releases_the_lock(self):
        def boom(*a, **k):
            raise RuntimeError("kaboom")

        with unittest.mock.patch.object(engine, "run_round", boom), \
                unittest.mock.patch.object(db, "log"):
            server.run_in_background(self.cfg, trigger="test")
            for _ in range(100):
                if not server.BUSY.locked():
                    break
                time.sleep(0.05)
            self.assertFalse(server.BUSY.locked())


class WatchdogTest(unittest.TestCase):
    """The deadline must reach the longest stage of a round."""

    def test_test_one_refuses_to_start_an_attempt_past_the_deadline(self):
        core = unittest.mock.Mock()
        cfg = {"targets": ["https://a/204"], "max_attempts": 3,
               "timeout_ms": 5000, "timeout_ms_retry": 9000, "retry_pause_s": 0}
        with self.assertRaises(engine.RoundTimeout):
            engine.test_one(core, {"mihomo": "n"}, cfg, deadline=time.monotonic() - 1)
        core.delay.assert_not_called()

    def test_test_one_runs_normally_without_a_deadline(self):
        core = unittest.mock.Mock()
        core.delay.return_value = (12, None, "")
        cfg = {"targets": ["https://a/204"], "max_attempts": 1, "timeout_ms": 100}
        out = engine.test_one(core, {"mihomo": "n"}, cfg)
        self.assertEqual(out["delay_ms"], 12)
        self.assertIsNone(out["reason"])

    def test_test_all_propagates_the_deadline(self):
        """`pool.map` used to wait for every node regardless of the budget."""
        mapping = [{"mihomo": f"n{i}"} for i in range(30)]
        core = unittest.mock.Mock()
        core.delay.return_value = (5, None, "")
        cfg = {"targets": ["https://a/204"], "max_attempts": 1, "timeout_ms": 100}
        with self.assertRaises(engine.RoundTimeout):
            engine._test_all(core, mapping, cfg, 4, deadline=time.monotonic() - 1)


class ExcludedStreakTest(unittest.TestCase):
    """An untestable node must not carry a failure streak into the next verdict."""

    def test_recording_an_excluded_node_clears_its_streak(self):
        with _TempRoot():
            db.connect()
            proxy = {"name": "n", "type": "vless", "server": "a.example", "port": 443}
            fp = engine._orig_fp(proxy)
            db.upsert_node("air", fp, "n", status=policy.PENDING, consec_fail=2)
            self.assertEqual(db.get_node("air", fp)["consec_fail"], 2)

            engine._record_excluded_nodes(
                1, [{"source": "air", "name": "n", "proxy": proxy, "index": 0, "fp": fp}])

            node = db.get_node("air", fp)
            self.assertEqual(node["status"], policy.EXCLUDED)
            self.assertEqual(node["consec_fail"], 0,
                             "an excluded node kept a streak it must not have")
            # `policy.py` states the invariant; the live suite asserts it too.
            self.assertEqual(db.query("SELECT source FROM nodes WHERE status='excluded'"
                                      " AND consec_fail > 0"), [])

    def test_an_excluded_node_keeps_one_ledger_row_across_the_flip(self):
        with _TempRoot():
            db.connect()
            proxy = {"name": "n", "type": "vless", "server": "a.example", "port": 443}
            fp = engine._orig_fp(proxy)
            db.upsert_node("air", fp, "n", status=policy.ALIVE)
            engine._record_excluded_nodes(
                1, [{"source": "air", "name": "n", "proxy": proxy, "index": 0, "fp": fp}])
            self.assertEqual(len(db.list_nodes("air")), 1)


class NotifierCooldownTest(unittest.TestCase):
    """The cooldown must be consumed by delivery, not by an attempt."""

    def cfg(self, **over):
        alert = {"enabled": True, "cooldown_minutes": 240, "alive_floor": 0,
                 "telegram": {"enabled": True, "token": "t", "chat_id": "c"},
                 "webhook": {"enabled": False, "url": ""}}
        alert.update(over)
        return {"alert": alert}

    def test_a_failed_delivery_does_not_start_the_cooldown(self):
        with _TempRoot():
            attempts = []

            def fail(cfg_alert, text):
                attempts.append(text)
                return False, "HTTP 500"

            with unittest.mock.patch.object(notifier, "telegram_send", fail):
                notifier.send(self.cfg(), "alive_low", "t", "b", log=_nolog)
                notifier.send(self.cfg(), "alive_low", "t", "b", log=_nolog)
            self.assertEqual(len(attempts), 2,
                             "a delivery failure consumed the cooldown window")

    def test_a_successful_delivery_starts_the_cooldown(self):
        with _TempRoot():
            attempts = []

            def ok(cfg_alert, text):
                attempts.append(text)
                return True, "ok"

            with unittest.mock.patch.object(notifier, "telegram_send", ok):
                notifier.send(self.cfg(), "alive_low", "t", "b", log=_nolog)
                notifier.send(self.cfg(), "alive_low", "t", "b", log=_nolog)
            self.assertEqual(len(attempts), 1)

    def test_a_failed_delivery_is_logged_as_an_error(self):
        with _TempRoot():
            levels = []
            with unittest.mock.patch.object(notifier, "telegram_send",
                                            lambda c, t: (False, "HTTP 500")):
                notifier.send(self.cfg(), "alive_low", "t", "b",
                              log=lambda level, msg: levels.append(level))
            self.assertIn("error", levels,
                          "a total delivery failure must not read like a cooldown skip")

    def test_a_junk_cooldown_value_does_not_raise(self):
        """`int()` on it used to escape into the round and mark it aborted."""
        with _TempRoot(), unittest.mock.patch.object(
                notifier, "telegram_send", lambda c, t: (True, "ok")):
            notifier.send(self.cfg(cooldown_minutes="soon"), "k", "t", "b", log=_nolog)


class MakeTestableTest(unittest.TestCase):
    """One bad node must cost one node, not the round."""

    def entries(self, names):
        out = []
        for index, name in enumerate(names):
            proxy = {"name": name, "type": "vless", "server": f"s{index}.example",
                     "port": 443}
            out.append({"source": "air", "name": name, "proxy": proxy,
                        "index": index, "fp": engine._orig_fp(proxy)})
        return out

    def test_a_duplicate_name_is_actually_removed(self):
        """`prepare` renames duplicates, so matching on the name removed nothing.

        Two nodes called "BageVM" become "BageVM" and "BageVM #2" in the kernel
        config. When the kernel rejected the second, the culprit name matched no
        entry in `working`, so the loop burned every attempt and raised -- one
        bad node killed the whole round, which is what this function exists to
        prevent.
        """
        entries = self.entries(["BageVM", "BageVM", "good"])
        calls = {"n": 0}

        def fake_test(core_cfg, host_dir=None):
            calls["n"] += 1
            if calls["n"] == 1:
                return False, 'proxy 1: initialize proxy "BageVM #2" failed'
            return True, ""

        with unittest.mock.patch.object(coremod, "build_config"), \
                unittest.mock.patch.object(coremod, "config_test", fake_test):
            proxies, mapping, dropped = coremod.make_testable(
                entries, {"api": "http://x"}, "s")
        self.assertEqual(len(dropped), 1)
        self.assertEqual(dropped[0]["name"], "BageVM #2")
        # the first BageVM and `good` survive
        self.assertEqual(sorted(p["name"] for p in proxies), ["BageVM", "good"])

    def test_a_short_name_is_not_matched_against_unrelated_output(self):
        proxies = [{"name": "jp", "server": "1.2.3.4"},
                   {"name": "tokyo-premium", "server": "5.6.7.8"}]
        output = 'level=error msg="initialize proxy tokyo-premium failed"'
        self.assertEqual(coremod._culprit_from(output, proxies)["name"], "tokyo-premium")

    def test_an_unidentifiable_error_raises_rather_than_pruning_at_random(self):
        proxies = [{"name": "alpha-node", "server": "1.2.3.4"}]
        self.assertIsNone(coremod._culprit_from("unrecognised failure", proxies))

    def test_the_fallback_fingerprint_uses_the_original_proxy(self):
        """An expanded variant carries the IP in `proxy`, the domain in `orig_proxy`."""
        original = {"name": "n", "type": "vless", "server": "a.example",
                    "port": "443", "ech-opts": {"enable": True}}
        variant = dict(original, server="5.6.7.8")
        entry = {"source": "air", "name": "n", "proxy": variant, "index": 0,
                 "orig_proxy": original}
        _proxies, mapping, _dropped = coremod.prepare([entry])
        self.assertEqual(mapping[0]["fp"], engine._orig_fp(original))


class RoundAccountingTest(unittest.TestCase):
    """`total`/`failed` must be in the same unit as `ok`."""

    def test_a_multi_address_node_counts_as_one(self):
        """A domain resolving to two addresses is one node, not two.

        `ok` counts endpoints (one per fingerprint). `total` was `len(by_name)`
        -- one row per address variant -- so a node with two addresses that is
        alive reported "1 of 2, 1 failed", which reads as mass death in the
        panel and the history table. The guardrail reads `ok` only, so it was
        never affected.
        """
        with _TempRoot():
            db.connect()
            domain = {"name": "n", "type": "vless", "server": "a.example", "port": 443}
            fp = engine._orig_fp(domain)
            entries = [
                {"source": "air", "name": "n", "index": 0, "fp": fp,
                 "proxy": dict(domain, server=ip), "orig_proxy": domain,
                 "test_ip": ip}
                for ip in ("5.6.7.8", "9.9.9.9")
            ]
            proxies, mapping, _dropped = coremod.prepare(entries)
            self.assertEqual(len(mapping), 2, "expected one variant per address")
            by_name = {m["mihomo"]: m for m in mapping}
            cfg = {"policy": {"drop_after_consecutive_fails": 3,
                              "suspect_floor_ratio": 0.5,
                              "suspect_floor_absolute": 3},
                   "verify": {"exclude_countries": []},
                   "publish": {"enabled": False},
                   "sources": [{"key": "air", "name": "air"}]}
            results = {name: {"delay_ms": 10, "reason": None, "detail": "",
                              "attempts": 1} for name in by_name}
            summary = engine._apply_and_publish(
                cfg, db.start_round("test"), None, by_name, proxies, results,
                {}, cfg["sources"], _nolog)
            self.assertEqual(summary["total"], 1,
                             "two address variants of one node counted as two nodes")
            self.assertEqual(summary["alive"], 1)


class TrimResultsTest(unittest.TestCase):
    """`results` grows forever; the panel sorts it every 5 seconds."""

    def test_only_the_newest_rounds_are_kept(self):
        with _TempRoot():
            db.connect()
            for round_index in range(1, 11):
                round_id = db.start_round("test")
                db.finish_round(round_id, ok=1, total=1, duration_s=0)
                db.record_result(round_id, "air", f"fp{round_index}", "n", "alive",
                                 None, None, None, 1, "")
            self.assertEqual(len(db.query("SELECT * FROM results")), 10)
            db.trim_results(keep_rounds=4)
            kept = sorted(row["round_id"] for row in
                          db.query("SELECT DISTINCT round_id FROM results"))
            self.assertEqual(kept, [7, 8, 9, 10])
            # the rounds themselves are the convergence ledger's backbone
            self.assertEqual(len(db.query("SELECT * FROM rounds")), 10)

    def test_trimming_an_empty_table_is_a_no_op(self):
        with _TempRoot():
            db.connect()
            self.assertEqual(db.trim_results(keep_rounds=5), 0)


class ChunkedQueryTest(unittest.TestCase):
    """SQLite's placeholder limit must not be able to fail a whole round."""

    def test_ip_geo_get_handles_more_addresses_than_the_variable_limit(self):
        with _TempRoot():
            db.connect()
            ips = [f"10.{i // 250}.{i % 250}.1" for i in range(1200)]
            db.ip_geo_put([{"ip": ip, "country": "US", "isp": "x"} for ip in ips])
            got = db.ip_geo_get(ips)
            self.assertEqual(len(got), len(ips))
            self.assertEqual(got[ips[0]], "US")

    def test_ip_geo_get_deduplicates_and_ignores_blanks(self):
        with _TempRoot():
            db.connect()
            db.ip_geo_put([{"ip": "1.2.3.4", "country": "US", "isp": "x"}])
            self.assertEqual(db.ip_geo_get(["1.2.3.4", "1.2.3.4", "", None]),
                             {"1.2.3.4": "US"})

    def test_delete_nodes_not_in_handles_a_long_keep_list(self):
        with _TempRoot():
            db.connect()
            keep = []
            for index in range(1200):
                fp = f"{index:016d}"
                db.upsert_node("air", fp, f"n{index}")
                keep.append(fp)
            db.upsert_node("air", "9" * 16, "stale")
            self.assertEqual(db.delete_nodes_not_in("air", keep), 1)
            self.assertIsNone(db.get_node("air", "9" * 16))


class TrendsTest(unittest.TestCase):
    """`/api/nodes` fetches trends for every source in one pass."""

    def test_trends_are_grouped_by_source_and_newest_first(self):
        with _TempRoot():
            db.connect()
            for verdict in ("fail", "fail", "alive"):
                round_id = db.start_round("test")
                db.record_result(round_id, "air", "fpA", "n", verdict, None,
                                 None, None, 1, "")
                db.record_result(round_id, "other", "fpB", "m", "alive", None,
                                 None, None, 1, "")
            trends = db.recent_trends_all()
            self.assertEqual(trends["air"]["fpA"], ["alive", "fail", "fail"])
            self.assertEqual(trends["other"]["fpB"], ["alive"] * 3)

    def test_the_per_node_window_is_respected(self):
        with _TempRoot():
            db.connect()
            for _ in range(20):
                round_id = db.start_round("test")
                db.record_result(round_id, "air", "fpA", "n", "alive", None,
                                 None, None, 1, "")
            self.assertEqual(len(db.recent_trends_all(limit_per_node=5)["air"]["fpA"]), 5)


class UiRenderTest(unittest.TestCase):
    """A hostile token or title must not be able to take the dashboard with it.

    The token is no longer interpolated into a JavaScript string literal -- it
    travels inside a `<script type="application/json">` bootstrap block. The
    failure mode is the same shape though, and worse in one way: the block is
    *data*, so the browser will parse whatever survives into an object that
    holds the admin token. A value able to close the element early would hand
    an attacker the rest of the page as markup.
    """

    def bootstrap(self, page):
        match = re.search(
            r'<script type="application/json" id="bootstrap">(.*?)</script>',
            page, re.S)
        self.assertIsNotNone(match, "the bootstrap block is missing from the page")
        return match.group(1)

    def test_a_token_that_tries_to_close_the_tag_cannot_escape(self):
        hostile = 'ab</script><script>alert(1)</script>'
        page = ui.render("标题", hostile)
        blob = self.bootstrap(page)
        # `<` is escaped, so the block cannot be terminated from inside it and
        # the injected markup never becomes an element.
        self.assertNotIn("</script>", blob)
        self.assertNotIn("<script", blob)
        self.assertIn("\\u003c/script", blob)
        # ...and the value still round-trips exactly, so escaping lost nothing.
        self.assertEqual(json.loads(blob)["token"], hostile)

    def test_a_token_with_a_quote_stays_one_json_string(self):
        hostile = 'ab"; alert(1); //'
        page = ui.render("标题", hostile)
        blob = self.bootstrap(page)
        self.assertEqual(json.loads(blob)["token"], hostile)
        # The old bug's shape: the literal ending early and the rest running.
        self.assertNotIn('"token": "ab"; alert(1)', blob)

    def test_the_title_cannot_inject_markup(self):
        hostile = "<img src=x onerror=alert(1)>"
        page = ui.render(hostile, "t" * 32)
        # The title only ever appears inside the JSON block, with `<` escaped.
        self.assertNotIn("<img src=x", page)
        self.assertEqual(json.loads(self.bootstrap(page))["title"], hostile)

    def test_a_normal_render_still_embeds_both_values(self):
        page = ui.render("我的面板", "secret-token-value")
        self.assertIn("我的面板", page)
        self.assertIn("secret-token-value", page)
        self.assertNotIn("__BOOTSTRAP__", page)
        self.assertNotIn("__TOKEN__", page)
        self.assertNotIn("__TITLE__", page)

    def test_the_api_base_is_baked_in_for_a_split_deployment(self):
        """The CDN build serves the same HTML with an absolute backend."""
        page = ui.render("t", "k", api_base="https://probe.example")
        self.assertEqual(json.loads(self.bootstrap(page))["apiBase"],
                         "https://probe.example")
        # The same-origin default must stay empty so `/api/...` resolves against
        # whatever host served the page.
        self.assertEqual(json.loads(self.bootstrap(ui.render("t", "k")))["apiBase"], "")

    def test_the_page_carries_no_inline_script(self):
        """`script-src 'self'` is only honest if nothing needs 'unsafe-inline'.

        An inline `<script>` or an `onclick=` attribute anywhere in the shell
        would be blocked by that CSP -- the panel would paint and then do
        nothing, with the only clue in the browser console.
        """
        page = ui.render("t", "k")
        self.assertNotIn("<script>", page)
        self.assertNotIn("onclick=", page)
        self.assertIn('src="app.js"', page)
        self.assertIn('src="theme.js"', page)
        self.assertIn('href="app.css"', page)

    def test_the_rendered_script_parses(self):
        # The script is a static asset now, so a token cannot reach it at all --
        # but it still has to parse, and a syntax error here blanks the panel
        # exactly the way a quoting slip used to.
        script = ui.asset_text("app.js")
        if shutil.which("node") is None:
            self.skipTest("node is not installed")
        import subprocess

        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "page.js")
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(script)
            result = subprocess.run(["node", "--check", path],
                                    capture_output=True, text=True)
        self.assertEqual(result.returncode, 0,
                         "the dashboard script does not parse: " + (result.stderr or "")[:400])


class StaticAssetTest(unittest.TestCase):
    """The front end is served as files, and only as the files it declares."""

    ADMIN = "a" * 32

    def setUp(self):
        self.cfg = {
            "auth": {"token": self.ADMIN},
            "publish": {"token": "p" * 32, "prefix": "probe",
                        "hostname": "probe.example", "enabled": True},
            "ui": {"title": "测试面板"},
            "sources": [], "schedule": {"enabled": False, "interval_minutes": 30},
            "core": {"container": "mihomo-probe"}, "alert": {"enabled": False},
            "test": {"targets": ["https://a/204"]},
        }
        self.srv = _LiveServer(self.cfg)
        self.addCleanup(self.srv.close)

    def test_assets_are_served_without_a_token(self):
        """A browser fetches `<link>` and `<script src>` with no custom header.

        Gating these would mean the panel loads its shell and then never gets a
        stylesheet or a script: a blank page and a 401 in the network tab, with
        nothing in the UI saying why.
        """
        for name, ctype in (("app.css", "text/css"), ("app.js", "javascript"),
                            ("theme.js", "javascript")):
            with self.subTest(asset=name):
                code, body, headers = self.srv.get("/" + name)
                self.assertEqual(code, 200)
                self.assertIn(ctype, headers.get("Content-Type", ""))
                self.assertTrue(body.strip(), "served an empty asset")
                self.assertEqual(headers.get("Cache-Control"), "no-cache")

    def test_only_declared_assets_are_served(self):
        """No generic static handler over the directory.

        `web/` also holds build inputs (`_headers`) and a future build could drop
        anything in there; a catch-all would publish all of it.
        """
        for path in ("/index.html", "/_headers", "/app.js.bak", "/data/config.json"):
            with self.subTest(path=path):
                code, _, _ = self.srv.get(path)
                self.assertEqual(code, 401, "must fall through to auth, not be served")

    def test_the_shell_still_needs_the_admin_token(self):
        """It carries the token in its bootstrap block, so it stays private."""
        code, _, _ = self.srv.get("/")
        self.assertEqual(code, 401)
        code, body, headers = self.srv.get(f"/?token={self.ADMIN}")
        self.assertEqual(code, 200)
        self.assertIn("text/html", headers.get("Content-Type", ""))
        # No-store, unlike the assets: this response is the credential.
        self.assertEqual(headers.get("Cache-Control"), "no-store")
        self.assertIn('href="app.css"', body)
        self.assertIn('src="app.js"', body)


class CrossOriginTest(unittest.TestCase):
    """The API must be usable from a front end hosted on a CDN.

    Front end on a CDN, backend on the VPS: two origins, so every `/api/*` call
    is cross-origin and carries a custom header -- which means a preflight, and
    a response the browser only hands to JavaScript when the origin is on the
    allowlist. Getting any of that wrong looks like "the API is down" in the
    browser, because the failure happens before any of our code runs.
    """

    ADMIN = "a" * 32
    CDN = "https://panel.example"

    def cfg(self, origins):
        return {
            "auth": {"token": self.ADMIN},
            "publish": {"token": "p" * 32, "prefix": "probe",
                        "hostname": "probe.example", "enabled": True},
            "ui": {"title": "测试面板"},
            "server": {"cors_origins": origins},
            "sources": [], "schedule": {"enabled": False, "interval_minutes": 30},
            "core": {"container": "mihomo-probe"}, "alert": {"enabled": False},
            "test": {"targets": ["https://a/204"]},
        }

    def server(self, origins):
        srv = _LiveServer(self.cfg(origins))
        self.addCleanup(srv.close)
        return srv

    def test_preflight_is_answered_without_a_token(self):
        """A preflight cannot carry a token, so requiring one breaks everything."""
        srv = self.server([self.CDN])
        code, _, headers = srv.request(
            "/api/status", method="OPTIONS",
            headers={"Origin": self.CDN,
                     "Access-Control-Request-Method": "GET",
                     "Access-Control-Request-Headers": "x-auth-token"})
        self.assertEqual(code, 204)
        self.assertEqual(headers.get("Access-Control-Allow-Origin"), self.CDN)
        self.assertIn("X-Auth-Token", headers.get("Access-Control-Allow-Headers", ""))
        self.assertIn("POST", headers.get("Access-Control-Allow-Methods", ""))
        self.assertIn("Origin", headers.get("Vary", ""))

    def test_an_allowed_origin_gets_the_response(self):
        srv = self.server([self.CDN])
        code, _, headers = srv.request(
            "/api/status", headers={"Origin": self.CDN, "X-Auth-Token": self.ADMIN})
        self.assertEqual(code, 200)
        self.assertEqual(headers.get("Access-Control-Allow-Origin"), self.CDN)

    def test_a_disallowed_origin_is_told_nothing(self):
        """The default allowlist is empty: no site may read this API cross-origin."""
        srv = self.server([])
        code, _, headers = srv.request(
            "/api/status", headers={"Origin": "https://evil.example",
                                    "X-Auth-Token": self.ADMIN})
        # The request is still answered -- the token was valid -- but the browser
        # is not told it may hand the body to that page, which is the point.
        self.assertEqual(code, 200)
        self.assertNotIn("Access-Control-Allow-Origin", headers)

    def test_a_same_origin_request_needs_no_cors_header(self):
        srv = self.server([self.CDN])
        code, _, headers = srv.request("/healthz")
        self.assertEqual(code, 200)
        self.assertNotIn("Access-Control-Allow-Origin", headers)

    def test_the_allowlist_is_exact_not_a_suffix_match(self):
        """`panel.example.evil.com` must not ride along on a prefix rule."""
        srv = self.server([self.CDN])
        _, _, headers = srv.request(
            "/api/status", headers={"Origin": self.CDN + ".evil.com",
                                    "X-Auth-Token": self.ADMIN})
        self.assertNotIn("Access-Control-Allow-Origin", headers)

    def test_the_token_can_travel_in_the_header(self):
        """What the CDN front end actually does; query strings leak into logs."""
        srv = self.server([self.CDN])
        code, _, _ = srv.request("/api/status", headers={"X-Auth-Token": self.ADMIN})
        self.assertEqual(code, 200)

    def test_the_csp_no_longer_needs_unsafe_inline_for_scripts(self):
        srv = self.server([])
        _, _, headers = srv.request("/healthz")
        csp = headers.get("Content-Security-Policy", "")
        self.assertIn("script-src 'self'", csp)
        self.assertNotIn("script-src 'unsafe-inline'", csp)
        self.assertIn("frame-ancestors 'none'", csp)

    def test_a_disallowed_origin_is_refused_at_the_preflight_too(self):
        """A preflight is where the browser decides; saying nothing there is the
        cheapest possible refusal, and it must not be forgotten just because
        the real response is also filtered."""
        srv = self.server([self.CDN])
        code, _, headers = srv.request(
            "/api/status", method="OPTIONS",
            headers={"Origin": "https://evil.example",
                     "Access-Control-Request-Method": "GET"})
        self.assertEqual(code, 204)
        self.assertNotIn("Access-Control-Allow-Origin", headers)
        self.assertNotIn("Access-Control-Allow-Headers", headers)

    def test_a_different_port_is_a_different_origin(self):
        """Origins are scheme + host + port; a port-blind comparison would let
        anything on the same host in."""
        srv = self.server([self.CDN])
        _, _, headers = srv.request(
            "/api/status", headers={"Origin": self.CDN + ":8443",
                                    "X-Auth-Token": self.ADMIN})
        self.assertNotIn("Access-Control-Allow-Origin", headers)

    def test_a_null_origin_is_refused(self):
        """`Origin: null` is what a sandboxed iframe or a file:// page sends."""
        srv = self.server([self.CDN])
        _, _, headers = srv.request(
            "/api/status", headers={"Origin": "null", "X-Auth-Token": self.ADMIN})
        self.assertNotIn("Access-Control-Allow-Origin", headers)

    def test_a_trailing_slash_in_the_allowlist_still_matches(self):
        """A hand-edited config.json should not silently break the panel."""
        srv = self.server([self.CDN + "/"])
        _, _, headers = srv.request(
            "/api/status", headers={"Origin": self.CDN, "X-Auth-Token": self.ADMIN})
        self.assertEqual(headers.get("Access-Control-Allow-Origin"), self.CDN)


class CdnBuildGuardTest(unittest.TestCase):
    """The published bundle must not be able to carry a credential.

    `dist/` is served from a public bucket, so a token inside it is a published
    token -- and a page that *reads* a token from there works perfectly, which
    is why nobody would notice by looking at the site. The build refuses instead
    of trusting a reviewer to spot it.

    It also has to refuse *before* writing: the first version wrote `dist/` and
    then checked, so a rejected build left a page on disk that the next command
    in the chain would happily deploy.
    """

    @staticmethod
    def _load():
        import importlib.util

        path = Path(__file__).resolve().parent.parent / "tools" / "build_web.py"
        spec = importlib.util.spec_from_file_location("_build_web", path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def setUp(self):
        self.mod = self._load()

    def _clean_page(self):
        # Exactly the payload `build()` writes: token is null, not "".
        return ui.index_template().replace(
            "__BOOTSTRAP__",
            ui.bootstrap_json({"title": "t", "token": None, "apiBase": ""}))

    def test_a_clean_bundle_passes(self):
        files = {"index.html": self._clean_page().encode("utf-8"),
                 "app.js": b"var x = 1;",
                 "_headers": b"/*\n  Cache-Control: no-cache\n"}
        self.assertEqual(self.mod.validate_output(files), [])

    def test_a_baked_token_is_refused(self):
        files = {"index.html": ui.render("t", "a" * 32).encode("utf-8")}
        self.assertTrue(self.mod.validate_output(files))

    def test_a_hex_run_anywhere_is_refused(self):
        files = {"index.html": b'{"token": null}',
                 "app.js": b"// " + b"deadbeef" * 4}
        problems = self.mod.validate_output(files)
        self.assertTrue(any("app.js" in p for p in problems), problems)

    def test_a_suspicious_api_base_is_refused(self):
        for bad in ("http://probe.example",          # not https
                    "https://probe.example/?x=1",    # query
                    "https://probe.example/#token",  # fragment
                    "https://evil.example/?token=1"):
            with self.subTest(base=bad):
                with self.assertRaises(ValueError):
                    self.mod.validate_api_base(bad)

    def test_a_normal_api_base_is_accepted_and_normalised(self):
        self.assertEqual(self.mod.validate_api_base("https://probe.example/"),
                         "https://probe.example")
        self.assertEqual(self.mod.validate_api_base(""), "")

    def test_a_real_build_is_clean_and_carries_no_token(self):
        with tempfile.TemporaryDirectory() as tmp:
            out, manifest = self.mod.build("https://probe.example", tmp)
            page = (Path(tmp) / "index.html").read_text(encoding="utf-8")
            self.assertIn('"token": null', page)
            self.assertIn("https://probe.example", page)
            self.assertIn("connect-src 'self' https://probe.example",
                          (Path(tmp) / "_headers").read_text(encoding="utf-8"))
            self.assertEqual(sorted(manifest),
                             ["_headers", "app.css", "app.js", "index.html",
                              "theme.js"])
            self.assertEqual(out, Path(tmp))

    def test_a_rejected_build_leaves_no_page_behind(self):
        with tempfile.TemporaryDirectory() as tmp:
            good, _ = self.mod.build("https://probe.example", tmp)
            self.assertTrue((Path(tmp) / "index.html").exists())
            # Second run fails validation; the stale page must not survive it.
            with unittest.mock.patch.object(self.mod, "validate_output",
                                            return_value=["boom"]):
                with self.assertRaises(SystemExit):
                    self.mod.build("https://probe.example", tmp)
            self.assertFalse((Path(tmp) / "index.html").exists())
            self.assertEqual(good, Path(tmp))


class ExportTokenTest(unittest.TestCase):
    """The published URL must carry the read-only credential."""

    def cfg(self):
        return {"auth": {"token": "a" * 32},
                "publish": {"token": "p" * 32, "hostname": "probe.example",
                            "prefix": "probe"}}

    def test_export_url_uses_the_publish_token(self):
        url = engine.export_url(self.cfg(), "air")
        self.assertIn("p" * 32, url)
        self.assertNotIn("a" * 32, url)

    def test_export_url_quotes_a_non_ascii_key(self):
        url = engine.export_url(self.cfg(), "中国地区")
        self.assertNotIn("中国地区", url)
        self.assertIn("%E4%B8%AD", url)

    def test_export_url_falls_back_when_no_publish_token_exists(self):
        cfg = {"auth": {"token": "a" * 32}, "publish": {}}
        self.assertIn("a" * 32, engine.export_url(cfg, "air"))

    def test_link_substore_never_ships_the_admin_token(self):
        cfg = self.cfg()
        cfg["sources"] = [{"key": "air", "name": "air", "label": "air",
                           "enabled": True}]
        payloads = []

        class FakeStore:
            def upsert(self, kind, name, payload):
                payloads.append(payload)
                return "created"

            def get_json(self, path):
                return []

        with _TempRoot() as tmp:
            db.connect()
            (tmp / "exports").mkdir(parents=True, exist_ok=True)
            (tmp / "exports" / "air.yaml").write_text("proxies: []\n", encoding="utf-8")
            (tmp / "exports" / "air.meta.json").write_text(
                json.dumps({"count": 3}), encoding="utf-8")
            engine.link_substore(cfg, FakeStore(), log=_nolog)

        self.assertTrue(payloads, "no Sub-Store payload was produced")
        linked = [p for p in payloads if p.get("url")]
        self.assertTrue(linked, "no remote-sub payload was produced")
        for payload in linked:
            self.assertIn("p" * 32, payload["url"])
            self.assertNotIn("a" * 32, payload["url"],
                             "the admin token was handed to the Sub-Store backend")
        # The aggregate collection names its members rather than carrying URLs.
        for payload in payloads:
            self.assertNotIn("a" * 32, json.dumps(payload))


class StatusRedactionTest(unittest.TestCase):
    """The five-second poll must not carry live sender credentials (S-12 → P0).

    `redacted_config` used to mask only the two panel tokens, so every
    /api/status response carried the Telegram bot token, the webhook URL and
    the Sub-Store backend URL -- whose path segment is an auth bypass on the
    backend. One screenshot of the panel's network tab leaked all three.
    """

    def setUp(self):
        self.cfg = {
            "auth": {"token": "a" * 32},
            "publish": {"token": "p" * 32},
            "substore": {"backend": "https://sub.example.invalid/UNGUESSABLEPATH123"},
            "alert": {"telegram": {"token": "123456789:" + "T" * 35, "chat_id": "42"},
                      "webhook": {"url": "https://hooks.example.invalid/TOKENPART"}},
        }
        self.out = server.redacted_config(self.cfg)

    def test_the_telegram_bot_token_is_masked(self):
        self.assertEqual(self.out["alert"]["telegram"]["token"], cfgmod.SECRET_MASK)
        self.assertNotIn("T" * 35, json.dumps(self.out))

    def test_the_webhook_url_is_masked(self):
        self.assertEqual(self.out["alert"]["webhook"]["url"], cfgmod.SECRET_MASK)
        self.assertNotIn("TOKENPART", json.dumps(self.out))

    def test_the_backend_path_is_masked_but_the_host_is_kept(self):
        """The operator must still see *which* backend is configured."""
        masked = self.out["substore"]["backend"]
        self.assertEqual(masked, "https://sub.example.invalid/***")
        self.assertNotIn("UNGUESSABLEPATH123", json.dumps(self.out))

    def test_a_backend_without_a_secret_path_is_left_readable(self):
        self.cfg["substore"]["backend"] = "http://127.0.0.1:3000"
        out = server.redacted_config(self.cfg)
        self.assertEqual(out["substore"]["backend"], "http://127.0.0.1:3000")


class MaskablePatchTest(unittest.TestCase):
    """A save that round-trips the masked payload must not write the mask.

    The settings form reads the redacted /api/status config back and posts it
    on save. Without sentinel dropping, the first save after the S-12 fix
    would have replaced every real credential with "***" -- breaking alert
    delivery and the Sub-Store integration while reporting a successful save.
    """

    def test_a_masked_admin_token_is_dropped_not_written(self):
        clean, notes = cfgmod.validate_patch({"auth": {"token": cfgmod.SECRET_MASK}})
        self.assertNotIn("token", clean.get("auth", {}))
        self.assertTrue(any("auth.token" in n for n in notes), notes)

    def test_a_masked_alert_token_is_dropped_and_siblings_survive(self):
        clean, _ = cfgmod.validate_patch(
            {"alert": {"telegram": {"token": cfgmod.SECRET_MASK,
                                    "chat_id": "42"}}})
        self.assertNotIn("token", clean["alert"]["telegram"])
        self.assertEqual(clean["alert"]["telegram"]["chat_id"], "42")

    def test_real_values_still_pass_through(self):
        clean, notes = cfgmod.validate_patch(
            {"alert": {"webhook": {"url": "https://hooks.example.invalid/real"}}})
        self.assertEqual(clean["alert"]["webhook"]["url"],
                         "https://hooks.example.invalid/real")
        self.assertEqual(notes, [])

    def test_the_masked_backend_is_dropped(self):
        clean, notes = cfgmod.validate_patch(
            {"substore": {"backend": cfgmod.SECRET_MASK}})
        self.assertNotIn("backend", clean.get("substore", {}))
        self.assertTrue(any("substore.backend" in n for n in notes), notes)

    def test_a_settings_round_trip_preserves_the_stored_credentials(self):
        """The end-to-end shape of the bug: form read → save → file intact."""
        with _TempRoot() as tmp:
            cfg = cfgmod.load()
            real_admin = cfg["auth"]["token"]
            # The patch a form built from the *redacted* payload would send.
            clean, _ = cfgmod.validate_patch({"auth": {"token": cfgmod.SECRET_MASK}})
            cfgmod.update(clean)
            stored = json.loads((tmp / "config.json").read_text(encoding="utf-8"))
            self.assertEqual(stored["auth"]["token"], real_admin)


class MixedPortDefaultTest(unittest.TestCase):
    """A fresh install must be able to build a kernel config out of the box.

    `core.build_config` reads `core.mixed_port` with a direct subscript, but
    the key lived only in the deployed config.json -- DEFAULTS had none -- so
    a new environment died on its first round with a bare KeyError before a
    single node was tested (ARCHITECTURE §mixed_port 陷阱).
    """

    def test_the_default_is_present_and_documented(self):
        port = cfgmod.DEFAULTS["core"]["mixed_port"]
        self.assertIsInstance(port, int)
        low, high = cfgmod.NUMERIC_BOUNDS["core.mixed_port"]
        self.assertTrue(low <= port <= high)

    def test_build_config_works_with_defaults_alone(self):
        with _TempRoot() as tmp:
            # `out_dir` is required: build_config's default is cfgmod.CORE_DIR,
            # which lives under ROOT and is *not* isolated by _TempRoot.
            path = coremod.build_config(
                [{"name": "n1", "type": "socks5", "server": "203.0.113.10",
                  "port": 1080}],
                dict(cfgmod.DEFAULTS["core"]), "secret",
                out_dir=tmp / "core")
            # Read inside the block: the temp root is deleted on exit.
            text = path.read_text(encoding="utf-8")
        self.assertIn(f"mixed-port: {cfgmod.DEFAULTS['core']['mixed_port']}", text)
        self.assertIn("external-controller: 127.0.0.1:19190", text)


class QueryTokenCompatTest(unittest.TestCase):
    """Query tokens are compat, header tokens are the recommendation.

    The channel matters: a token in the URL lands in access logs and browser
    history. `auth_kind` reports which (channel, scope) authenticated so the
    handler can warn once about the legacy form without spamming the 5s poll.
    """

    def cfg(self):
        return {"auth": {"token": "a" * 32}, "publish": {"token": "p" * 32}}

    def test_query_header_and_bearer_report_their_channel(self):
        stub_query = _HandlerStub(f"/api/status?token={'a' * 32}")
        self.assertEqual(server.auth_kind(stub_query, self.cfg(), "/api/status"),
                         ("query", "admin"))
        stub_header = _HandlerStub("/api/status", {"X-Auth-Token": "a" * 32})
        self.assertEqual(server.auth_kind(stub_header, self.cfg(), "/api/status"),
                         ("header", "admin"))
        stub_bearer = _HandlerStub("/api/status",
                                   {"Authorization": "Bearer " + "a" * 32})
        self.assertEqual(server.auth_kind(stub_bearer, self.cfg(), "/api/status"),
                         ("header", "admin"))

    def test_the_publish_scope_is_reported_not_the_admin_one(self):
        stub = _HandlerStub(f"/api/export/air.yaml?token={'p' * 32}")
        self.assertEqual(server.auth_kind(stub, self.cfg(), "/api/export/air.yaml"),
                         ("query", "publish"))
        # The publish token still does not grant admin paths.
        stub = _HandlerStub(f"/api/status?token={'p' * 32}")
        self.assertIsNone(server.auth_kind(stub, self.cfg(), "/api/status"))

    def test_auth_ok_still_accepts_all_three_forms(self):
        cfg = self.cfg()
        self.assertTrue(server.auth_ok(_HandlerStub(f"/api/status?token={'a' * 32}"),
                                       cfg, "/api/status"))
        self.assertTrue(server.auth_ok(_HandlerStub("/api/status",
                                                    {"X-Auth-Token": "a" * 32}),
                                       cfg, "/api/status"))
        self.assertTrue(server.auth_ok(_HandlerStub(
            "/api/status", {"Authorization": "Bearer " + "a" * 32}),
            cfg, "/api/status"))


if __name__ == "__main__":
    unittest.main()
