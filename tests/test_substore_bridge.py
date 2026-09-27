"""Contract tests for the official Sub-Store bridge (ARCHITECTURE §3/§4).

Three surfaces, one file:

- `mihomo_test/substore_bridge.probe_nodes_payload/snapshot` (N-03): the pure
  projection that owns the 9-field whitelist. These tests pin the field set,
  the `display→name` / `last_delay_ms→delay_ms` mapping and the exclusion of
  every bookkeeping/credential key, so the endpoint and the script can never
  drift apart on what a node snapshot may reveal.
- `GET /api/probe/nodes` (N-01): a real loopback server (the `_LiveServer`
  pattern from test_hardening) exercises auth wiring -- the publish-token scope
  grows by exactly one path and nothing else -- plus the query filters, the
  edge-rejected `status` typo and the response headers/body.
- `substore_bridge/probe_filter.script.js` (N-02): a Node subprocess harness
  loads the script the way the official backend does (a fresh function scope
  over its text), points `$arguments` and the global `fetch` at scenario data,
  and asserts the filter/annotate/missing behaviour, the name→server matching
  rules and the "token only in the header" rule.

No test here touches the network: HTTP is loopback-only and every fetch the
script would make is answered by an in-process mock.

Run with the rest of the offline suite:
    python3 -m unittest discover -s tests
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
import unittest.mock
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import config as cfgmod
from mihomo_test import db, engine, policy, server, substore_bridge

try:
    import _isolation
except ImportError:  # imported as a package: python -m unittest tests.test_substore_bridge
    from tests import _isolation


def setUpModule():
    _isolation.isolate()


def tearDownModule():
    _isolation.restore()


class _IsolatedDb:
    """Point the ledger at a throwaway path for one test.

    Same idea as test_hardening's `_TempRoot`, narrowed to the db module: the
    projection tests need real `db.list_nodes` rows, but must never share rows
    (or the connection) with a neighbouring test.
    """

    def __enter__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="mihomo-test-bridge-db-"))
        self._old = (db.DB_PATH, db._conn)
        db.DB_PATH, db._conn = self.tmp / "state.db", None
        db.connect()
        return self.tmp

    def __exit__(self, *exc):
        if db._conn is not None:
            db._conn.close()
        db.DB_PATH, db._conn = self._old
        shutil.rmtree(self.tmp, ignore_errors=True)
        return False


# ---------------------------------------------------------------- N-03 payload


class ProbeNodesPayloadTest(unittest.TestCase):
    """The 9-field projection is the single source of truth for the payload."""

    def full_row(self):
        """A row shaped like `db.list_nodes` output, plus hostile extras."""
        return {
            "name": "hk-01", "source": "air", "status": "alive",
            "country": "HK", "consec_fail": 0, "last_delay_ms": 234,
            "server": "a.example.com", "proto": "vless", "category": "direct",
            # ledger bookkeeping the contract deliberately excludes:
            "fingerprint": "f" * 64, "last_reason": "dial tcp: timeout",
            "first_seen": "2026-01-01T00:00:00", "last_seen": "2026-01-01T00:00:00",
            "last_ok": "2026-01-01T00:00:00", "total_ok": 3, "total_fail": 1,
            "ip_alive": 2, "ip_total": 2,
            # keys that must never ride along even if a caller adds them:
            "uuid": "uuid-value-xyz", "password": "password-value-xyz",
            "token": "token-value-xyz",
        }

    def test_the_payload_carries_exactly_the_nine_fields(self):
        out = substore_bridge.probe_nodes_payload([self.full_row()])
        self.assertEqual(len(out), 1)
        self.assertEqual(set(out[0]), set(substore_bridge.PROBE_NODE_FIELDS))
        self.assertEqual(len(substore_bridge.PROBE_NODE_FIELDS), 9)

    def test_ledger_bookkeeping_and_credentials_cannot_leak(self):
        blob = json.dumps(substore_bridge.probe_nodes_payload([self.full_row()]))
        for secret in ("f" * 64, "dial tcp: timeout", "uuid-value-xyz",
                       "password-value-xyz", "token-value-xyz",
                       "fingerprint", "last_reason", "first_seen", "last_seen",
                       "last_ok", "total_ok", "total_fail", "ip_alive", "ip_total"):
            self.assertNotIn(secret, blob)

    def test_last_delay_ms_maps_to_delay_ms(self):
        out = substore_bridge.probe_nodes_payload([self.full_row()])
        self.assertEqual(out[0]["delay_ms"], 234)
        self.assertNotIn("last_delay_ms", out[0])

    def test_none_fields_stay_none(self):
        row = self.full_row()
        row.update(country=None, server=None, proto=None, last_delay_ms=None)
        out = substore_bridge.probe_nodes_payload([row])[0]
        self.assertIsNone(out["country"])
        self.assertIsNone(out["server"])
        self.assertIsNone(out["proto"])
        self.assertIsNone(out["delay_ms"])

    def test_a_none_consec_fail_becomes_zero(self):
        """The contract types `consec_fail` as int; a missing stamp reads 0."""
        row = self.full_row()
        row["consec_fail"] = None
        out = substore_bridge.probe_nodes_payload([row])[0]
        self.assertEqual(out["consec_fail"], 0)
        self.assertIsInstance(out["consec_fail"], int)

    def test_empty_rows_are_an_empty_payload(self):
        self.assertEqual(substore_bridge.probe_nodes_payload([]), [])
        self.assertEqual(substore_bridge.probe_nodes_payload(None), [])

    def test_rows_from_list_nodes_project_display_and_delay(self):
        """The endpoint passes `db.list_nodes` rows; they must land intact."""
        with _IsolatedDb():
            db.upsert_node("air", "fp1", "hk-01", status=policy.ALIVE,
                           server="a.example.com", proto="vless", country="HK",
                           last_delay_ms=234, consec_fail=0, category="direct")
            payload = substore_bridge.probe_nodes_payload(db.list_nodes())
            self.assertEqual(len(payload), 1)
            self.assertEqual(payload[0]["name"], "hk-01")
            self.assertEqual(payload[0]["delay_ms"], 234)
            self.assertEqual(payload[0]["status"], "alive")
            self.assertEqual(set(payload[0]), set(substore_bridge.PROBE_NODE_FIELDS))

    def test_a_null_category_reads_as_direct(self):
        """A missing `category` stamp must not surface as a fourth node kind.

        `category` was added by migration and is only stamped during rounds;
        every other consumer in the repo reads the column as
        `n.category || "direct"` (db.py list_nodes comment), so the payload
        must normalise the same way (ARCHITECTURE §3.4 types it as string).
        """
        with _IsolatedDb():
            db.upsert_node("air", "fp1", "hk-01", status=policy.ALIVE)
            self.assertIsNone(db.get_node("air", "fp1")["category"])
            payload = substore_bridge.probe_nodes_payload(db.list_nodes())
            self.assertEqual(payload[0]["category"], "direct")

            db.upsert_node("air", "fp2", "gz-01", status=policy.ALIVE,
                           category="relay")
            payload = substore_bridge.probe_nodes_payload(db.list_nodes())
            by_name = {p["name"]: p for p in payload}
            self.assertEqual(by_name["gz-01"]["category"], "relay",
                             "an explicit category must not be overwritten")

    def test_a_raw_row_falls_back_to_display_for_name(self):
        """Rows straight from the nodes table keep working.

        `list_nodes` aliases `display AS name`, so for those rows the fallback
        is a no-op -- the payload's `name` can only ever mean the display name.
        """
        row = {"display": "raw-row", "source": "air", "status": "pending",
               "consec_fail": 0}
        out = substore_bridge.probe_nodes_payload([row])[0]
        self.assertEqual(out["name"], "raw-row")


# --------------------------------------------------------------- N-03 envelope


class ProbeNodesSnapshotTest(unittest.TestCase):
    """The response envelope has exactly one author: `probe_nodes_snapshot`."""

    def test_the_envelope_has_exactly_four_keys(self):
        snap = substore_bridge.probe_nodes_snapshot([])
        self.assertEqual(set(snap), {"ok", "generated_at", "count", "nodes"})
        self.assertIs(snap["ok"], True)

    def test_count_matches_the_nodes_list(self):
        rows = [{"name": f"n{i}", "source": "air", "status": "alive",
                 "consec_fail": 0} for i in range(3)]
        snap = substore_bridge.probe_nodes_snapshot(rows)
        self.assertEqual(snap["count"], 3)
        self.assertEqual(len(snap["nodes"]), 3)

    def test_generated_at_is_the_utc_ledger_stamp_without_a_suffix(self):
        snap = substore_bridge.probe_nodes_snapshot([])
        self.assertRegex(snap["generated_at"],
                         r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}$")

    def test_generated_at_comes_from_db_now(self):
        """The endpoint must not make up its own clock (one stamp author)."""
        with unittest.mock.patch.object(db, "now",
                                        lambda: "2026-09-28T12:34:56"):
            snap = substore_bridge.probe_nodes_snapshot([])
        self.assertEqual(snap["generated_at"], "2026-09-28T12:34:56")

    def test_an_explicit_generated_at_is_honoured(self):
        snap = substore_bridge.probe_nodes_snapshot([], generated_at="fixed")
        self.assertEqual(snap["generated_at"], "fixed")

    def test_an_empty_ledger_is_a_valid_snapshot(self):
        """200 + count 0, never a 404: "path wrong" and "ledger empty" must
        stay distinguishable for the script's fail-open decision."""
        snap = substore_bridge.probe_nodes_snapshot([])
        self.assertEqual(snap["count"], 0)
        self.assertEqual(snap["nodes"], [])
        self.assertIs(snap["ok"], True)


# --------------------------------------------------------------- N-01 endpoint


class _LiveServer:
    """A real loopback instance of the service (test_hardening's pattern).

    The data directory, config file and database are redirected per instance,
    so tests seed rows and export files without touching anything real.
    """

    def __init__(self, cfg):
        self.tmp = Path(tempfile.mkdtemp(prefix="mihomo-test-bridge-http-"))
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
        import threading
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    def get(self, path, headers=None):
        return self.request(path, headers=headers)

    def request(self, path, method="GET", headers=None):
        merged = {"User-Agent": "test"}
        merged.update(headers or {})
        req = urllib.request.Request(self.url(path), method=method,
                                     headers=merged)
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
                return resp.status, resp.read().decode("utf-8")
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read().decode("utf-8")

    def url(self, path):
        return f"http://127.0.0.1:{self.port}{path}"

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


class ProbeNodesEndpointTest(unittest.TestCase):
    """`GET /api/probe/nodes`: auth scope, filters, edge rejection, headers."""

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
        db.connect()

    def seed(self, source="air", fp="fp1", name="hk-01", status=policy.ALIVE,
             server="a.example.com", proto="vless", country="HK",
             delay=234, consec=0, category="direct"):
        db.upsert_node(source, fp, name, status=status, server=server,
                       proto=proto, country=country, last_delay_ms=delay,
                       consec_fail=consec, category=category)

    def get(self, path, token=None, headers=None):
        merged = dict(headers or {})
        if token:
            merged["X-Auth-Token"] = token
        code, body, resp_headers = self.srv.get(path, headers=merged or None)
        return code, body, resp_headers

    def test_a_snapshot_needs_a_token(self):
        code, body, _ = self.srv.get("/api/probe/nodes")
        self.assertEqual(code, 401)
        self.assertEqual(json.loads(body)["error"], "unauthorized")

    def test_the_publish_token_reads_the_snapshot(self):
        self.seed()
        code, body, _ = self.srv.get(f"/api/probe/nodes?token={self.PUBLISH}")
        self.assertEqual(code, 200)
        payload = json.loads(body)
        self.assertIs(payload["ok"], True)
        self.assertEqual(payload["count"], 1)
        self.assertEqual(len(payload["nodes"]), 1)
        node = payload["nodes"][0]
        self.assertEqual(set(node), set(substore_bridge.PROBE_NODE_FIELDS))
        self.assertEqual(node["name"], "hk-01")
        self.assertEqual(node["delay_ms"], 234)
        self.assertRegex(payload["generated_at"],
                         r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}$")

    def test_the_publish_token_can_travel_in_the_header(self):
        """How the Script Operator actually sends it (S-16: no query token)."""
        code, _, _ = self.srv.get("/api/probe/nodes",
                                  headers={"X-Auth-Token": self.PUBLISH})
        self.assertEqual(code, 200)

    def test_the_admin_token_still_reads_the_snapshot(self):
        code, _, _ = self.srv.get(f"/api/probe/nodes?token={self.ADMIN}")
        self.assertEqual(code, 200)

    def test_the_publish_scope_is_not_widened(self):
        """The scope grew by exactly one path -- nothing else opens up."""
        for path in ("/api/status", "/api/config"):
            with self.subTest(path=path):
                code, _, _ = self.srv.get(f"{path}?token={self.PUBLISH}")
                self.assertEqual(code, 401, path)

    def test_the_publish_token_still_reads_exports(self):
        """The pre-existing half of the publish scope must not regress."""
        (engine.EXPORT_DIR / "air.yaml").write_text("proxies: []\n",
                                                    encoding="utf-8")
        code, body, _ = self.srv.get(f"/api/export/air.yaml?token={self.PUBLISH}")
        self.assertEqual(code, 200)
        self.assertIn("proxies", body)

    def test_the_source_filter_selects_one_source(self):
        self.seed("air", "fp1", "hk-01")
        self.seed("air", "fp2", "hk-02")
        self.seed("vnet", "fp3", "us-01")
        code, body, _ = self.srv.get(
            f"/api/probe/nodes?token={self.PUBLISH}&source=air")
        self.assertEqual(code, 200)
        payload = json.loads(body)
        self.assertEqual(payload["count"], 2)
        self.assertEqual({n["name"] for n in payload["nodes"]}, {"hk-01", "hk-02"})
        code, body, _ = self.srv.get(
            f"/api/probe/nodes?token={self.PUBLISH}&source=vnet")
        self.assertEqual(json.loads(body)["count"], 1)

    def test_an_unknown_source_answers_an_empty_snapshot(self):
        """A read-only publish surface: an empty answer is the right answer,
        and cfg["sources"] semantics stay off the endpoint."""
        self.seed()
        code, body, _ = self.srv.get(
            f"/api/probe/nodes?token={self.PUBLISH}&source=nope")
        self.assertEqual(code, 200)
        payload = json.loads(body)
        self.assertEqual(payload["count"], 0)
        self.assertEqual(payload["nodes"], [])

    def test_the_status_filter_selects_one_status(self):
        self.seed("air", "fp1", "alive-one", status=policy.ALIVE)
        self.seed("air", "fp2", "dead-one", status=policy.DEAD)
        self.seed("air", "fp3", "pending-one", status=policy.PENDING)
        code, body, _ = self.srv.get(
            f"/api/probe/nodes?token={self.PUBLISH}&status=dead")
        self.assertEqual(code, 200)
        payload = json.loads(body)
        self.assertEqual([n["name"] for n in payload["nodes"]], ["dead-one"])
        code, body, _ = self.srv.get(
            f"/api/probe/nodes?token={self.PUBLISH}&status=alive")
        self.assertEqual([n["name"] for n in json.loads(body)["nodes"]],
                         ["alive-one"])

    def test_all_five_statuses_are_accepted(self):
        for status in substore_bridge.PROBE_NODE_STATUSES:
            with self.subTest(status=status):
                code, _, _ = self.srv.get(
                    f"/api/probe/nodes?token={self.PUBLISH}&status={status}")
                self.assertEqual(code, 200, status)

    def test_an_unknown_status_is_refused_400(self):
        """A typo'd status would otherwise read as "no such nodes" (a silent
        empty 200); the `/api/run` style is edge rejection instead."""
        self.seed()
        code, body, _ = self.srv.get(
            f"/api/probe/nodes?token={self.PUBLISH}&status=bogus")
        self.assertEqual(code, 400)
        error = json.loads(body)["error"]
        self.assertIn("bogus", error)
        for status in substore_bridge.PROBE_NODE_STATUSES:
            self.assertIn(status, error, "the error must name the whitelist")

    def test_the_response_carries_the_security_headers_and_content_type(self):
        _, _, headers = self.srv.get(
            f"/api/probe/nodes?token={self.PUBLISH}")
        lowered = {k.lower(): v for k, v in headers.items()}
        self.assertEqual(lowered.get("x-content-type-options"), "nosniff")
        self.assertEqual(lowered.get("x-frame-options"), "DENY")
        self.assertEqual(lowered.get("referrer-policy"), "no-referrer")
        self.assertIn("frame-ancestors 'none'",
                      lowered.get("content-security-policy", ""))
        self.assertEqual(lowered.get("cache-control"), "no-store")
        self.assertTrue(lowered.get("content-type", "").startswith("application/json"))

    def test_a_trailing_path_does_not_enter_the_publish_scope(self):
        """`/api/probe/nodes` is exact-matched on purpose: a trailing path must
        not read the snapshot with the handed-out token."""
        for path in ("/api/probe/nodes/extra", "/api/probe/nodes/"):
            with self.subTest(path=path):
                code, _, _ = self.srv.get(f"{path}?token={self.PUBLISH}")
                self.assertEqual(code, 401, path)
        # With the admin token the same paths are merely unrouted.
        code, _, _ = self.srv.get(
            f"/api/probe/nodes/extra?token={self.ADMIN}")
        self.assertEqual(code, 404)

    def test_post_is_not_a_routed_method(self):
        code, _ = self.srv.post(f"/api/probe/nodes?token={self.ADMIN}", {})
        self.assertEqual(code, 404)
        # The publish token passes auth (read scope) but still finds no route.
        code, _ = self.srv.post(f"/api/probe/nodes?token={self.PUBLISH}", {})
        self.assertEqual(code, 404)

    def test_the_body_carries_no_credentials(self):
        self.seed("air", "fp1", "hk-01")
        self.seed("vnet", "fp2", "us-01", category=None)
        code, body, _ = self.srv.get(f"/api/probe/nodes?token={self.PUBLISH}")
        self.assertEqual(code, 200)
        self.assertNotIn(self.ADMIN, body)
        self.assertNotIn(self.PUBLISH, body)
        # The fingerprints were seeded as "fp1"/"fp2"; neither may surface.
        for secret in (self.ADMIN, self.PUBLISH, "fp1", "fp2", "fingerprint",
                       "last_reason", "uuid", "password", "first_seen",
                       "last_seen", "total_ok"):
            self.assertNotIn(secret, body)
        payload = json.loads(body)
        self.assertEqual(payload["count"], 2)
        for node in payload["nodes"]:
            self.assertEqual(set(node), set(substore_bridge.PROBE_NODE_FIELDS))
        # The NULL category (migration-added column, stamped per round) reads
        # as "direct" end-to-end, not as a null or a fourth kind.
        self.assertEqual({n["category"] for n in payload["nodes"]}, {"direct"})

    def test_an_empty_ledger_answers_200_with_count_zero(self):
        code, body, _ = self.srv.get(f"/api/probe/nodes?token={self.PUBLISH}")
        self.assertEqual(code, 200)
        payload = json.loads(body)
        self.assertIs(payload["ok"], True)
        self.assertEqual(payload["count"], 0)
        self.assertEqual(payload["nodes"], [])


# ------------------------------------------------------- N-02 script (Node 24)

HARNESS_SOURCE = """\
// probe_filter harness -- generated at test time by tests/test_substore_bridge.py.
// Usage: node probe_filter_harness.mjs <script.js> <scenario.json>
// Loads the Script Operator the way the official backend does (a fresh function
// scope over the script text), points $arguments and the global fetch at the
// scenario, runs operator(proxies, "ClashMeta", {source:"test"}) and reports
// one JSON object on stdout:
//   {result, error, fetchCalls: [{url, headers}], logs}
import { readFileSync } from "node:fs";

const [, , scriptPath, scenarioPath] = process.argv;
const scenario = JSON.parse(readFileSync(scenarioPath, "utf8"));
const src = readFileSync(scriptPath, "utf8");

const fetchCalls = [];
const spec = scenario.respond || {};
globalThis.fetch = async (url, init) => {
  fetchCalls.push({
    url: String(url),
    headers: init && init.headers ? JSON.parse(JSON.stringify(init.headers)) : null,
  });
  if (spec.reject) {
    throw new Error(String(spec.reject));
  }
  const status = typeof spec.status === "number" ? spec.status : 200;
  const body = typeof spec.body === "string"
    ? spec.body
    : JSON.stringify(spec.body === undefined ? {} : spec.body);
  return { ok: status >= 200 && status < 300, status: status, text: async () => body };
};

const logs = [];
const realLog = console.log;
console.log = (...parts) => { logs.push(parts.map(String).join(" ")); };

globalThis.$arguments = scenario.args || {};
const operator = new Function(src + "\\n;return operator;")();

const out = { result: null, error: null, fetchCalls: fetchCalls, logs: logs };
try {
  let proxies = JSON.parse(JSON.stringify(scenario.proxies || []));
  const runs = scenario.runs || 1;
  for (let i = 0; i < runs; i += 1) {
    proxies = await operator(proxies, "ClashMeta", { source: "test" });
  }
  out.result = JSON.parse(JSON.stringify(proxies));
} catch (e) {
  out.error = String((e && e.message) || e);
} finally {
  console.log = realLog;
}
process.stdout.write(JSON.stringify(out));
"""


def ledger(*nodes):
    """A probe envelope as `GET /api/probe/nodes` would serve it."""
    return {"ok": True, "generated_at": "2026-09-28T00:00:00",
            "count": len(nodes), "nodes": list(nodes)}


def entry(name, status, server=None, country=None):
    return {"name": name, "source": "air", "status": status, "country": country,
            "delay_ms": 100, "consec_fail": 0, "server": server,
            "proto": "vless", "category": "direct"}


LEDGER_NODES = (
    entry("alive-one", "alive", server="a.example.com", country="hk"),
    entry("dead-one", "dead", server="d.example.com", country="US"),
    entry("pending-one", "pending", server="p.example.com"),
    entry("unknown-one", "unknown", server="u.example.com"),
    entry("excluded-one", "excluded", server="x.example.com", country="JP"),
)

PROXIES = [{"name": n, "server": s} for n, s in (
    ("alive-one", "a.example.com"),
    ("dead-one", "d.example.com"),
    ("pending-one", "p.example.com"),
    ("unknown-one", "u.example.com"),
    ("excluded-one", "x.example.com"),
    ("stray-one", "nowhere.example.com"),  # not in the ledger
)]

PROBE_URL = "https://probe.example/api/probe/nodes"


class ProbeFilterScriptTest(unittest.TestCase):
    """The Script Operator, driven through a real Node subprocess.

    The harness file is generated into a system temp dir at setUp and removed
    at tearDown; nothing lands in the repository.
    """

    def setUp(self):
        self.script_path = (Path(__file__).resolve().parent.parent
                            / "substore_bridge" / "probe_filter.script.js")
        self.assertTrue(self.script_path.exists(),
                        f"missing script: {self.script_path}")
        self.node = shutil.which("node")
        if self.node is None:
            self.skipTest("node is not installed")
        self.tmp = Path(tempfile.mkdtemp(prefix="mihomo-test-bridge-js-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.harness = self.tmp / "probe_filter_harness.mjs"
        self.harness.write_text(HARNESS_SOURCE, encoding="utf-8")

    def run_operator(self, args, proxies, respond=None, runs=None):
        scenario = {"args": args, "proxies": proxies}
        if respond is not None:
            scenario["respond"] = respond
        if runs is not None:
            scenario["runs"] = runs
        scenario_path = self.tmp / "scenario.json"
        scenario_path.write_text(json.dumps(scenario), encoding="utf-8")
        proc = subprocess.run(
            [self.node, str(self.harness), str(self.script_path),
             str(scenario_path)],
            capture_output=True, text=True, encoding="utf-8", timeout=120)
        self.assertEqual(proc.returncode, 0,
                         "harness crashed: " + (proc.stderr or proc.stdout)[:500])
        return json.loads(proc.stdout)

    def ok_ledger(self, *nodes):
        return {"status": 200, "body": ledger(*nodes)}

    def names(self, out):
        return [p["name"] for p in out["result"]]

    # -- gate ---------------------------------------------------------

    def test_the_script_passes_the_node_syntax_check(self):
        proc = subprocess.run([self.node, "--check", str(self.script_path)],
                              capture_output=True, text=True, encoding="utf-8")
        self.assertEqual(proc.returncode, 0,
                         "the operator script does not parse: "
                         + (proc.stderr or "")[:400])

    # -- filter -------------------------------------------------------

    def test_filter_removes_only_dead(self):
        """mode/missing defaults: dead is the only deletion, and a node the
        probe has never seen stays (fail-open, 不误杀)."""
        out = self.run_operator({"probe_url": PROBE_URL}, PROXIES,
                                respond=self.ok_ledger(*LEDGER_NODES))
        self.assertIsNone(out["error"])
        self.assertEqual(self.names(out),
                         ["alive-one", "pending-one", "unknown-one",
                          "excluded-one", "stray-one"])

    def test_filter_missing_drop_also_drops_unrecorded(self):
        out = self.run_operator(
            {"probe_url": PROBE_URL, "missing": "drop"}, PROXIES,
            respond=self.ok_ledger(*LEDGER_NODES))
        self.assertEqual(self.names(out),
                         ["alive-one", "pending-one", "unknown-one",
                          "excluded-one"])

    def test_an_unknown_mode_falls_back_to_filter(self):
        out = self.run_operator(
            {"probe_url": PROBE_URL, "mode": "prune", "missing": "delete"},
            PROXIES, respond=self.ok_ledger(*LEDGER_NODES))
        self.assertEqual(self.names(out),
                         ["alive-one", "pending-one", "unknown-one",
                          "excluded-one", "stray-one"],
                         "an unknown mode/missing must read as filter/keep")

    def test_an_http_500_fails_open_per_missing(self):
        ledger_fail = {"status": 500, "body": "boom"}
        out = self.run_operator({"probe_url": PROBE_URL}, PROXIES,
                                respond=ledger_fail)
        self.assertEqual(len(out["fetchCalls"]), 1)
        self.assertTrue(out["logs"], "a probe failure must log, not stay silent")
        self.assertEqual(self.names(out), [p["name"] for p in PROXIES])
        out = self.run_operator({"probe_url": PROBE_URL, "missing": "drop"},
                                PROXIES, respond=ledger_fail)
        self.assertEqual(out["result"], [])

    def test_a_401_fails_open_per_missing(self):
        refuse = {"status": 401, "body": {"error": "unauthorized"}}
        out = self.run_operator({"probe_url": PROBE_URL}, PROXIES,
                                respond=refuse)
        self.assertEqual(self.names(out), [p["name"] for p in PROXIES])
        out = self.run_operator({"probe_url": PROBE_URL, "missing": "drop"},
                                PROXIES, respond=refuse)
        self.assertEqual(out["result"], [])

    def test_a_non_json_body_fails_open_per_missing(self):
        junk = {"status": 200, "body": "<html>not json</html>"}
        out = self.run_operator({"probe_url": PROBE_URL}, PROXIES, respond=junk)
        self.assertEqual(self.names(out), [p["name"] for p in PROXIES])
        out = self.run_operator({"probe_url": PROBE_URL, "missing": "drop"},
                                PROXIES, respond=junk)
        self.assertEqual(out["result"], [])

    def test_an_ok_false_envelope_fails_open(self):
        lying = {"status": 200,
                 "body": {"ok": False, "generated_at": "x", "count": 1,
                          "nodes": LEDGER_NODES}}
        out = self.run_operator({"probe_url": PROBE_URL}, PROXIES,
                                respond=lying)
        self.assertEqual(self.names(out), [p["name"] for p in PROXIES])

    def test_an_empty_ledger_follows_the_missing_policy(self):
        empty = self.ok_ledger()
        out = self.run_operator({"probe_url": PROBE_URL}, PROXIES,
                                respond=empty)
        self.assertEqual(self.names(out), [p["name"] for p in PROXIES])
        out = self.run_operator({"probe_url": PROBE_URL, "missing": "drop"},
                                PROXIES, respond=empty)
        self.assertEqual(out["result"], [])

    # -- annotate / both ----------------------------------------------

    def test_annotate_adds_country_and_dead_suffix(self):
        out = self.run_operator(
            {"probe_url": PROBE_URL, "mode": "annotate"}, PROXIES,
            respond=self.ok_ledger(*LEDGER_NODES))
        by_name = {p["name"] for p in out["result"]}
        self.assertIn("alive-one [HK]", by_name, "country is uppercased")
        self.assertIn("dead-one ·dead", by_name)
        self.assertIn("pending-one", by_name)
        self.assertIn("unknown-one", by_name)
        self.assertIn("excluded-one", by_name, "不测 ≠ 死: excluded is untouched")
        self.assertIn("stray-one", by_name)
        self.assertEqual(len(by_name), 6, "annotate deletes nothing")

    def test_annotate_skips_a_non_two_letter_country(self):
        nodes = (entry("alive-one", "alive", server="a.example.com"),
                 entry("dead-one", "dead", server="d.example.com", country="USA"))
        out = self.run_operator(
            {"probe_url": PROBE_URL, "mode": "annotate"},
            [{"name": "alive-one"}, {"name": "dead-one"}],
            respond=self.ok_ledger(*nodes))
        self.assertEqual(self.names(out), ["alive-one", "dead-one ·dead"],
                         "null or non-2-letter countries add no suffix")

    def test_annotate_is_idempotent(self):
        out = self.run_operator(
            {"probe_url": PROBE_URL, "mode": "annotate"}, PROXIES,
            respond=self.ok_ledger(*LEDGER_NODES), runs=2)
        by_name = {p["name"] for p in out["result"]}
        self.assertIn("alive-one [HK]", by_name)
        self.assertIn("dead-one ·dead", by_name)
        self.assertNotIn("alive-one [HK] [HK]", by_name)
        self.assertNotIn("dead-one ·dead ·dead", by_name)
        # A stale suffix from a previous pass is replaced, not stacked.
        nodes = (entry("foo", "alive", server="a.example.com", country="JP"),)
        out = self.run_operator(
            {"probe_url": PROBE_URL, "mode": "annotate"},
            [{"name": "foo [US]", "server": "a.example.com"}],
            respond=self.ok_ledger(*nodes), runs=2)
        self.assertEqual(self.names(out), ["foo [JP]"])

    def test_both_filters_then_renames(self):
        out = self.run_operator({"probe_url": PROBE_URL, "mode": "both"},
                                PROXIES,
                                respond=self.ok_ledger(*LEDGER_NODES))
        names = self.names(out)
        self.assertEqual(names,
                         ["alive-one [HK]", "pending-one", "unknown-one",
                          "excluded-one", "stray-one"])
        self.assertFalse(any("·dead" in n for n in names),
                         "dead nodes are deleted, never renamed")

    # -- matching ------------------------------------------------------

    def test_a_prefixed_name_hits_via_server_fallback(self):
        """The probe export prefixes names with `[CC] ` when add_region_tag is
        on, so the subscription name cannot match the ledger name -- the
        (normalised) server match is what catches it."""
        nodes = (entry("foo", "alive", server="A.example.COM.", country="jp"),)
        out = self.run_operator(
            {"probe_url": PROBE_URL, "mode": "annotate"},
            [{"name": "[HK] foo", "server": "a.example.com"}],
            respond=self.ok_ledger(*nodes))
        self.assertEqual(self.names(out), ["[HK] foo [JP]"])

    def test_consistent_server_records_hit_via_fallback(self):
        """Several records on one host, all with the same status: that is a
        hit (missing=drop would have deleted a no-record node)."""
        nodes = (entry("host-a", "alive", server="shared.example.com"),
                 entry("host-b", "alive", server="shared.example.com"))
        out = self.run_operator(
            {"probe_url": PROBE_URL, "missing": "drop"},
            [{"name": "mystery", "server": "shared.example.com"}],
            respond=self.ok_ledger(*nodes))
        self.assertEqual(self.names(out), ["mystery"])

    def test_conflicting_server_records_read_as_unrecorded(self):
        """The ledger has no port column, so same-host records with opposing
        verdicts cannot be told apart -- 宁可不裁, treat as no record."""
        nodes = (entry("host-a", "alive", server="shared.example.com"),
                 entry("host-b", "dead", server="shared.example.com"))
        proxy = [{"name": "mystery", "server": "shared.example.com"}]
        out = self.run_operator({"probe_url": PROBE_URL}, proxy,
                                respond=self.ok_ledger(*nodes))
        self.assertEqual(self.names(out), ["mystery"])
        out = self.run_operator({"probe_url": PROBE_URL, "missing": "drop"},
                                proxy, respond=self.ok_ledger(*nodes))
        self.assertEqual(out["result"], [])
        out = self.run_operator({"probe_url": PROBE_URL, "mode": "annotate"},
                                proxy, respond=self.ok_ledger(*nodes))
        self.assertEqual(self.names(out), ["mystery"])

    def test_a_node_without_a_server_reads_as_unrecorded(self):
        out = self.run_operator(
            {"probe_url": PROBE_URL, "missing": "drop"},
            [{"name": "alive-one"}, {"name": "no-server"}],
            respond=self.ok_ledger(*LEDGER_NODES))
        self.assertEqual(self.names(out), ["alive-one"])

    # -- configuration & transport --------------------------------------

    def test_a_missing_probe_url_throws_even_on_an_empty_subscription(self):
        """Configuration errors are fail-loud: an empty inbound list must not
        be an exit ramp for a missing probe_url."""
        out = self.run_operator({}, [])
        self.assertIsNotNone(out["error"])
        self.assertIn("probe_url", out["error"])
        out = self.run_operator({"probe_token": "p" * 32}, [])
        self.assertIsNotNone(out["error"])
        self.assertIn("probe_url", out["error"])

    def test_an_empty_subscription_short_circuits_without_fetching(self):
        out = self.run_operator({"probe_url": PROBE_URL}, [])
        self.assertEqual(out["result"], [])
        self.assertEqual(out["fetchCalls"], [])

    def test_the_token_travels_only_in_the_header(self):
        """S-16: no new query-token usage -- the token rides `X-Auth-Token`
        and never appears in the URL."""
        out = self.run_operator(
            {"probe_url": PROBE_URL, "probe_token": "p" * 32},
            PROXIES, respond=self.ok_ledger(*LEDGER_NODES))
        self.assertEqual(len(out["fetchCalls"]), 1, "one fetch per produce")
        call = out["fetchCalls"][0]
        self.assertEqual(call["url"], PROBE_URL)
        self.assertNotIn("p" * 32, call["url"])
        self.assertEqual(call["headers"], {"X-Auth-Token": "p" * 32})

    def test_a_fetch_failure_fails_open(self):
        out = self.run_operator({"probe_url": PROBE_URL}, PROXIES,
                                respond={"reject": "getaddrinfo ENOTFOUND"})
        self.assertEqual(self.names(out), [p["name"] for p in PROXIES])
        out = self.run_operator({"probe_url": PROBE_URL, "missing": "drop"},
                                PROXIES,
                                respond={"reject": "getaddrinfo ENOTFOUND"})
        self.assertEqual(out["result"], [])


if __name__ == "__main__":
    unittest.main()
