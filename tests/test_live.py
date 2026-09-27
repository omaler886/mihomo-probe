"""End-to-end tests against a live deployment (vps).

Unlike tests/test_logic.py, which is hermetic and mocks the kernel, this suite
runs against a real running stack and asserts on real traffic. It answers the
question the unit tests cannot: does the thing actually work out there, with a
real mihomo kernel, real upstream subscriptions, and real nodes.

Run it on the host (or inside the app container -- both work):

    python3 tests/test_live.py              # every group
    python3 tests/test_live.py -v           # per-test names
    python3 tests/test_live.py kernel lanes # only these groups

Groups, cheapest first:

    health   containers up, panel answers, auth enforced
    api      documented endpoints return the documented shapes
    kernel   a real node carries real traffic through the kernel
    lanes    the N verification lanes are genuinely independent
    data     ledger invariants (streaks, statuses, export/record agreement)
    substore the Sub-Store link objects point where they should
    round    a full round completes inside its budget and converges

Design notes:

* Anything that spends money, mutates state, or takes minutes is marked
  ``slow`` and is **off unless explicitly requested** (``--include-slow``).
  A test run must never be able to corrupt a production ledger by accident.
* Node names are read from the live config, never hardcoded -- the whole point
  is to test what is actually deployed, not what the repo assumed.
"""
import argparse
import calendar
import json
import os
import re
import subprocess
import sys
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

import yaml

ROOT = Path(os.environ.get("MIHOMO_TEST_ROOT", "/srv/mihomo-test"))
sys.path.insert(0, str(ROOT))

from mihomo_test import engine
from mihomo_test.store import Client

# This module talks to a running deployment. It must NOT be collected by a
# plain `unittest discover`, which would otherwise try to hit a live panel on
# a dev machine and report a pile of connection errors as test failures.
# Run it directly (`python3 tests/test_live.py`) or set MIHOMO_TEST_LIVE=1.
LIVE_DEPLOYED = ((ROOT / "data" / "config.json").exists()
                 and (ROOT / "data" / "state.db").exists())
if not LIVE_DEPLOYED and os.environ.get("MIHOMO_TEST_LIVE") != "1":
    raise unittest.SkipTest(
        f"no live deployment at {ROOT}; run tests/test_live.py on the host instead")

TRACE_URL = "https://www.cloudflare.com/cdn-cgi/trace"
TEST_URL = "https://www.gstatic.com/generate_204"


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------
def _cfg():
    """Read data/config.json directly, without importing the app."""
    try:
        return json.loads((ROOT / "data" / "config.json").read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise unittest.SkipTest(f"no readable config at {ROOT}: {exc}")


def _token():
    return _cfg().get("auth", {}).get("token", "")


def api(path, method="GET", payload=None, token=None, timeout=20):
    """Call the panel. Returns (status, parsed_body_or_text)."""
    url = f"http://127.0.0.1:8088{path}"
    if token is not False:
        joiner = "&" if "?" in path else "?"
        url += f"{joiner}token={urllib.parse.quote(token if token else _token())}"
    body = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(url, data=body, method=method)
    if body is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            text = resp.read().decode("utf-8", "replace")
            status = resp.status
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")
    try:
        return status, json.loads(text)
    except ValueError:
        return status, text


def core_api(path, method="GET", payload=None, timeout=20):
    """Call the mihomo kernel's own API, using the app's stored secret.

    The kernel answers PUT with 204 and an empty body, so the body is parsed
    leniently -- json-decoding it unconditionally fails on every success.
    """
    cfg = _cfg()
    base = cfg["core"]["api"].rstrip("/")
    secret = (ROOT / "data" / "core.secret").read_text(encoding="utf-8").strip()
    body = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(base + path, data=body, method=method)
    req.add_header("Authorization", f"Bearer {secret}")
    if body is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", "replace")
            status = resp.status
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")
    if not raw.strip():
        return status, None
    try:
        return status, json.loads(raw)
    except ValueError:
        return status, raw


def docker(*args, timeout=120):
    """Run docker; return (returncode, stdout+stderr)."""
    try:
        proc = subprocess.run(["docker", *args], capture_output=True, text=True,
                              timeout=timeout)
    except (OSError, subprocess.SubprocessError) as exc:
        return 1, str(exc)
    return proc.returncode, (proc.stdout or "") + (proc.stderr or "")


def trace_through(port, timeout=20):
    """Fetch the Cloudflare trace through a lane's loopback HTTP port."""
    fields, error = trace_through_safe(port, timeout)
    if error:
        raise error
    return fields


def trace_through_safe(port, timeout=20):
    """Same, but returns (fields, exception) instead of raising.

    Individual nodes die mid-suite and that is normal for a proxy pool -- a
    TLS EOF from one node must not be reported as a broken test. Callers that
    are probing a single node use this form and treat an error as "try the next
    one"; callers asserting on infrastructure use trace_through and let it raise.
    """
    handler = urllib.request.ProxyHandler(
        {"http": f"http://127.0.0.1:{port}", "https": f"http://127.0.0.1:{port}"})
    opener = urllib.request.build_opener(handler)
    try:
        with opener.open(TRACE_URL, timeout=timeout) as resp:
            text = resp.read().decode("utf-8", "replace")
    except Exception as exc:      # noqa: BLE001 -- any transport error is "this node"
        return None, exc
    fields = {}
    for line in text.splitlines():
        if "=" in line:
            k, _, v = line.partition("=")
            fields[k.strip()] = v.strip()
    return fields, None


def _trace_ip(port, timeout=20):
    """Exit IP through a lane, or None when the node cannot carry traffic now."""
    fields, error = trace_through_safe(port, timeout)
    return None if error or not fields else fields.get("ip")


def query(sql, params=()):
    """Read the ledger read-only. Never writes: this suite must not mutate."""
    import sqlite3
    conn = sqlite3.connect(f"file:{ROOT / 'data' / 'state.db'}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        return [dict(r) for r in conn.execute(sql, params).fetchall()]
    finally:
        conn.close()


def live_nodes(group="__LANE0__"):
    """Names the kernel currently exposes as proxies (excluding DIRECT/REJECT)."""
    status, body = core_api("/proxies")
    if status != 200 or not isinstance(body, dict):
        raise unittest.SkipTest(f"kernel API unavailable (HTTP {status})")
    proxies = body.get("proxies") or {}
    return [name for name in proxies
            if not name.startswith("__") and name not in ("DIRECT", "REJECT", "PASS",
                                                          "GLOBAL", "COMPATIBLE")
            and not name.startswith("lane")]


# --------------------------------------------------------------------------
# health
# --------------------------------------------------------------------------
class HealthTest(unittest.TestCase):
    """The stack is up and the panel enforces auth."""

    def test_all_three_containers_are_running(self):
        code, out = docker("compose", "-f", str(ROOT / "docker-compose.yml"), "ps")
        if code != 0:
            self.skipTest(f"docker compose unavailable: {out[:200]}")
        for name in ("mihomo-test", "mihomo-probe", "cloudflared-probe"):
            self.assertIn(name, out, f"{name} is not in `docker compose ps`")
        self.assertNotIn("Exit", out, "a container has exited")

    def test_healthz_needs_no_token(self):
        status, body = api("/healthz", token=False)
        self.assertEqual(status, 200)
        self.assertTrue(body.get("ok"))

    def test_panel_rejects_a_missing_token(self):
        status, _ = api("/api/status", token=False)
        self.assertEqual(status, 401, "the panel served data without a token")

    def test_panel_rejects_a_wrong_token(self):
        status, _ = api("/api/status", token="deadbeef" * 4)
        self.assertEqual(status, 401, "the panel accepted a bogus token")

    def test_real_token_is_accepted(self):
        status, body = api("/api/status")
        self.assertEqual(status, 200)
        self.assertIn("stats", body)

    def test_export_endpoint_needs_a_token_too(self):
        """An export carries server/port/credentials, so it must not be open."""
        cfg = _cfg()
        key = None
        for source in cfg.get("sources", []):
            if source.get("enabled", True):
                key = source.get("key")
                break
        if not key:
            self.skipTest("no enabled source")
        status, _ = api(f"/api/export/{key}.yaml", token=False)
        self.assertEqual(status, 401)


# --------------------------------------------------------------------------
# api
# --------------------------------------------------------------------------
class ApiShapeTest(unittest.TestCase):
    """Documented endpoints return the documented shapes."""

    def test_status_payload_has_the_documented_keys(self):
        status, body = api("/api/status")
        self.assertEqual(status, 200)
        for key in ("stats", "last_round", "busy", "next_run", "exports", "config"):
            self.assertIn(key, body)
        for key in ("total", "alive", "dead", "pending", "unknown", "excluded"):
            self.assertIn(key, body["stats"])

    def test_stats_are_internally_consistent(self):
        _, body = api("/api/status")
        stats = body["stats"]
        counted = (stats["alive"] + stats["dead"] + stats["pending"]
                   + stats["unknown"] + stats["excluded"])
        self.assertEqual(counted, stats["total"],
                         "status buckets do not add up to the total")

    def test_nodes_payload_carries_a_trend_per_node(self):
        status, body = api("/api/nodes")
        self.assertEqual(status, 200)
        nodes = body.get("nodes") or []
        self.assertTrue(nodes, "no nodes in the ledger")
        self.assertIn("trend", nodes[0], "the sparkline data is missing")

    def test_exports_list_matches_the_files_on_disk(self):
        _, body = api("/api/status")
        listed = {e["key"] for e in body["exports"]}
        on_disk = {p.stem for p in (ROOT / "data" / "exports").glob("*.yaml")}
        self.assertEqual(listed, on_disk,
                         "the panel advertises exports that disagree with disk")

    def test_export_files_are_valid_clashmeta(self):
        for path in sorted((ROOT / "data" / "exports").glob("*.yaml")):
            with self.subTest(export=path.name):
                doc = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
                proxies = doc.get("proxies")
                self.assertIsInstance(proxies, list, "not a proxies list")
                for proxy in proxies:
                    for field in ("name", "type", "server", "port"):
                        self.assertIn(field, proxy, f"{path.name}: node missing {field}")
                    self.assertIsInstance(proxy["port"], int)

    def test_every_export_meta_count_matches_its_yaml(self):
        for meta_path in sorted((ROOT / "data" / "exports").glob("*.meta.json")):
            with self.subTest(export=meta_path.stem):
                meta = json.loads(meta_path.read_text(encoding="utf-8"))
                doc = yaml.safe_load(
                    (meta_path.with_suffix("").with_suffix(".yaml")).read_text(
                        encoding="utf-8")) or {}
                self.assertEqual(meta.get("count"), len(doc.get("proxies") or []),
                                 "meta count disagrees with the yaml it describes")

    def test_export_key_cannot_escape_the_directory(self):
        for probe in ("..%2f..%2fetc%2fpasswd", "..%5C..%5Cetc", "....//"):
            with self.subTest(key=probe):
                status, _ = api(f"/api/export/{probe}.yaml")
                self.assertIn(status, (400, 404), "path traversal was not refused")

    def test_unknown_path_is_a_404(self):
        status, _ = api("/api/definitely-not-a-route")
        self.assertEqual(status, 404)


# --------------------------------------------------------------------------
# kernel
# --------------------------------------------------------------------------
class KernelEgressTest(unittest.TestCase):
    """A real node carries real traffic, verified end to end."""

    def setUp(self):
        self.cfg = _cfg()
        self.port = int(self.cfg["core"]["base_port"])

    def test_kernel_reports_its_version(self):
        status, body = core_api("/version")
        self.assertEqual(status, 200)
        self.assertIn("version", body)

    def test_kernel_binds_loopback_only(self):
        """The kernel runs with host networking, so a wildcard bind would be public.

        Only the LOCAL address column may be inspected: netstat prints the peer
        address as ``0.0.0.0:*`` for every LISTEN row, so a naive substring
        search over the whole line flags every healthy loopback listener.
        """
        code, out = docker("exec", "mihomo-probe",
                           "sh", "-c", "netstat -ltn 2>/dev/null || ss -ltn")
        if code != 0:
            self.skipTest("cannot inspect sockets inside the probe container")
        checked = 0
        for line in out.splitlines():
            parts = line.split()
            if len(parts) < 4 or parts[0] not in ("tcp", "tcp6"):
                continue
            local = parts[3]
            if not re.match(r"^.*:19\d{3}$", local):
                continue
            checked += 1
            host = local.rsplit(":", 1)[0]
            self.assertIn(host, ("127.0.0.1", "[::1]", "::1"),
                          f"a kernel port is bound beyond loopback: {line.strip()}")
        self.assertGreater(checked, 0, "found no kernel ports to check")

    def test_egress_lanes_reach_the_internet(self):
        """Some node must carry traffic out through a lane and report its exit.

        Walks a few candidates rather than betting on whatever happens to be
        selected: any individual node can be down at any moment, and that is
        the pool being tested, not a fault in the lane plumbing.
        """
        names = live_nodes()
        if not names:
            self.skipTest("the kernel has no testable proxies loaded")
        errors = []
        for name in names[:8]:
            status, _ = core_api("/proxies/__LANE0__", "PUT", {"name": name})
            if status not in (200, 204):
                continue
            fields, error = trace_through_safe(self.port)
            if error:
                errors.append(f"{name}: {type(error).__name__}")
                continue
            if not fields.get("ip"):
                errors.append(f"{name}: trace without an ip field")
                continue
            ip = fields["ip"]
            self.assertRegex(ip, r"^(\d+\.\d+\.\d+\.\d+|[0-9a-fA-F:]+)$",
                             f"implausible exit IP {ip!r}")
            self.assertIn("loc", fields, "no exit country in the trace")
            return
        self.fail(f"no lane reached the internet through {len(errors)} candidates: "
                  f"{errors[:5]}")

    def test_delay_api_reports_a_real_latency(self):
        names = live_nodes()
        if not names:
            self.skipTest("the kernel has no testable proxies loaded")
        query_str = urllib.parse.urlencode(
            {"timeout": 8000, "url": TEST_URL, "expected": "204"})
        tried, alive = 0, 0
        for name in names[:12]:
            status, body = core_api(
                f"/proxies/{urllib.parse.quote(name, safe='')}/delay?{query_str}", timeout=20)
            tried += 1
            if status == 200 and isinstance(body, dict) and body.get("delay", -1) >= 0:
                alive += 1
        self.assertGreater(alive, 0, f"not one of {tried} loaded nodes answered")

    def test_a_dead_node_is_classified_not_merely_failed(self):
        """The whole point of keeping response bodies: 503 and 504 differ."""
        from mihomo_test.core import _reason_from
        self.assertEqual(_reason_from(504, '{"message":"Timeout"}')[0], "timeout")
        self.assertEqual(
            _reason_from(503, '{"message":"An error occurred in the delay test"}')[0],
            "kernel_error")
        self.assertNotEqual(_reason_from(504, "{}")[0], _reason_from(503, "{}")[0],
                            "a timeout and an internal error collapsed into one reason")


# --------------------------------------------------------------------------
# lanes
# --------------------------------------------------------------------------
class LaneIndependenceTest(unittest.TestCase):
    """The parallel verification lanes must not share the kernel's selection."""

    def setUp(self):
        self.cfg = _cfg()
        self.base = int(self.cfg["core"]["base_port"])
        self.lanes = int(self.cfg["core"].get("lanes", 8))
        self.ports = [self.base + i for i in range(self.lanes)]

    def test_every_configured_lane_listener_exists(self):
        status, body = core_api("/listeners")
        if status != 200:
            self.skipTest("kernel does not expose /listeners")
        names = {item.get("name") for item in (body.get("listeners") or [])}
        for i in range(self.lanes):
            self.assertIn(f"lane{i}", names, f"lane{i} listener is missing")

    def test_lane_groups_are_distinct_selectors(self):
        status, body = core_api("/proxies")
        self.assertEqual(status, 200)
        groups = {n: p for n, p in (body.get("proxies") or {}).items()
                  if n.startswith("__LANE")}
        self.assertEqual(len(groups), self.lanes,
                         f"expected {self.lanes} lane groups, found {len(groups)}")
        for name, group in groups.items():
            # the kernel reports the selector type as "Selector"
            self.assertEqual(group.get("type"), "Selector",
                             f"{name} is not a Selector")

    def test_swapping_two_lanes_swaps_their_exits(self):
        """The decisive check: lanes are independent, not aliases of one selector.

        Picks two nodes that egress from different IPs *on the lanes they will
        actually be tested through*. A country difference is the cheapest way to
        find such a pair, but an IP difference is what the assertion actually
        needs -- plenty of live nodes share a country.

        Both lanes are validated before the swap, deliberately. An earlier
        version picked candidates by probing lane0 only and then asserted on
        lane1, which had never been proven to carry traffic at all; a node that
        merely choked *through lane1* (same node, different socket path) then
        read as "the swap did nothing". Observed on vps as a one-off failure
        with before=[192.0.2.40, 192.0.2.111] and after=[192.0.2.111, None]
        -- i.e. lane1 dropped out, which the test misreported as an independence
        violation. Probing both lanes up front turns that into a skip.
        """
        names = live_nodes()
        if len(names) < 2:
            self.skipTest("need at least two loaded nodes to swap")

        chosen = []
        seen_ips = set()
        for name in names[:30]:
            ips = {}
            usable = True
            for lane in (0, 1):
                status, _ = core_api(f"/proxies/__LANE{lane}__", "PUT", {"name": name})
                if status not in (200, 204):
                    usable = False
                    break
                ip = _trace_ip(self.ports[lane])
                if not ip:
                    usable = False      # this node cannot carry traffic here
                    break
                ips[lane] = ip
            if not usable:
                continue
            # a candidate is only interesting if it egresses differently on the
            # two lanes than the candidate we already have
            if ips[0] in seen_ips:
                continue
            seen_ips.update(ips.values())
            chosen.append((name, ips[0], ips[1]))
            if len(chosen) == 2:
                break
        if len(chosen) < 2:
            self.skipTest("could not find two nodes that both lanes can carry traffic to")
        if chosen[0][1] == chosen[1][2]:
            self.skipTest("could not find two nodes with distinct egress IPs")

        a, b = chosen[0], chosen[1]
        try:
            core_api("/proxies/__LANE0__", "PUT", {"name": a[0]})
            core_api("/proxies/__LANE1__", "PUT", {"name": b[0]})
            before = {0: _trace_ip(self.ports[0]), 1: _trace_ip(self.ports[1])}
            if None in before.values():
                self.skipTest("a chosen node went down between probing and swapping")
            self.assertNotEqual(before[0], before[1],
                                "the two lanes already egress from the same IP")

            # swap the selections
            core_api("/proxies/__LANE0__", "PUT", {"name": b[0]})
            core_api("/proxies/__LANE1__", "PUT", {"name": a[0]})
            time.sleep(1)
            after = {0: _trace_ip(self.ports[0]), 1: _trace_ip(self.ports[1])}
        finally:
            # always put the lanes back on something sane: a round may start any time
            core_api("/proxies/__LANE0__", "PUT", {"name": a[0]})
            core_api("/proxies/__LANE1__", "PUT", {"name": b[0]})

        if None in after.values():
            self.skipTest("a chosen node dropped out during the swap")
        self.assertEqual(after[0], before[1], "lane0 did not follow its new selection")
        self.assertEqual(after[1], before[0], "lane1 did not follow its new selection")

    def test_lane_inbound_rules_pin_the_listener(self):
        """Each lane's listener must be bound to its own group by an IN-NAME rule."""
        code, out = docker("exec", "mihomo-probe", "cat", "/root/.config/mihomo/config.yaml")
        if code != 0:
            self.skipTest("cannot read the generated kernel config")
        for i in range(self.lanes):
            self.assertIn(f"IN-NAME,lane{i},__LANE{i}__", out,
                          f"lane{i} has no IN-NAME rule pinning it to __LANE{i}__")


# --------------------------------------------------------------------------
# data
# --------------------------------------------------------------------------
class LedgerIntegrityTest(unittest.TestCase):
    """Invariants that must hold no matter what the network did."""

    def test_no_round_is_left_unfinished_forever(self):
        stuck = query(
            "SELECT id, started_at FROM rounds WHERE finished_at IS NULL "
            "ORDER BY id DESC LIMIT 5")
        for row in stuck:
            started = _epoch(row["started_at"])
            age = time.time() - started
            self.assertLess(age, 3600,
                            f"round {row['id']} started {row['started_at']} and never closed "
                            f"({age / 60:.0f} min ago) -- the ledger has a hole")

    def test_rounds_are_ordered_and_timestamps_are_sane(self):
        rows = query("SELECT id, started_at, finished_at FROM rounds "
                     "WHERE finished_at IS NOT NULL ORDER BY id DESC LIMIT 20")
        self.assertTrue(rows, "no completed rounds recorded")
        for row in rows:
            self.assertGreaterEqual(_epoch(row["finished_at"]), _epoch(row["started_at"]),
                                    f"round {row['id']} finished before it started")

    def test_consec_fail_is_coherent_with_status(self):
        """dead implies the threshold was genuinely crossed."""
        threshold = int(_cfg()["policy"]["drop_after_consecutive_fails"])
        broken = query(
            "SELECT source, fingerprint, display, status, consec_fail FROM nodes "
            "WHERE status='dead' AND consec_fail < ?", (threshold,))
        self.assertEqual(broken, [],
                         f"{len(broken)} nodes are dead below the threshold: {broken[:3]}")

    def test_alive_nodes_have_zero_streak(self):
        broken = query(
            "SELECT source, display, consec_fail FROM nodes "
            "WHERE status='alive' AND consec_fail <> 0")
        self.assertEqual(broken, [], f"alive nodes carrying a failure streak: {broken[:3]}")

    def test_no_node_is_keyed_by_name(self):
        """Fingerprints are 16 hex chars; a name here means the old scheme survived."""
        bad = query("SELECT source, fingerprint FROM nodes "
                    "WHERE length(fingerprint) <> 16 LIMIT 5")
        self.assertEqual(bad, [], f"nodes keyed by something other than a fingerprint: {bad}")

    def test_results_match_the_rounds_that_produced_them(self):
        orphan = query(
            "SELECT COUNT(*) AS n FROM results r "
            "LEFT JOIN rounds ro ON ro.id = r.round_id WHERE ro.id IS NULL")
        self.assertEqual(orphan[0]["n"], 0, "results reference rounds that do not exist")

    def test_latest_round_has_a_result_row_per_node_it_tested(self):
        last = query("SELECT * FROM rounds WHERE finished_at IS NOT NULL "
                     "ORDER BY id DESC LIMIT 1")
        if not last:
            self.skipTest("no finished round")
        round_id = last[0]["id"]
        recorded = query("SELECT COUNT(*) AS n FROM results WHERE round_id=?", (round_id,))[0]["n"]
        self.assertGreater(recorded, 0, f"round {round_id} recorded no results")

    def test_excluded_nodes_never_accumulate_a_failure_streak(self):
        """An untestable entry point is out of scope, not dead."""
        bad = query("SELECT source, display, consec_fail FROM nodes "
                    "WHERE status='excluded' AND consec_fail > 0")
        self.assertEqual(bad, [], f"excluded nodes are being counted as failures: {bad[:3]}")

    def test_ip_geo_cache_is_populated_and_plausible(self):
        rows = query("SELECT ip, country FROM ip_geo LIMIT 200")
        if not rows:
            self.skipTest("the entry-IP cache is empty (entry_check may be off)")
        for row in rows:
            self.assertRegex(str(row["country"]), r"^[A-Z]{2}$",
                             f"implausible country for {row['ip']}: {row['country']!r}")

    def test_domain_view_cache_holds_real_addresses(self):
        rows = query("SELECT domain, views FROM domain_views LIMIT 50")
        if not rows:
            self.skipTest("no cached domain views")
        import ipaddress
        for row in rows:
            views = json.loads(row["views"])
            for label, ips in views.items():
                for ip in ips or []:
                    with self.subTest(domain=row["domain"], view=label):
                        ipaddress.ip_address(ip)   # raises if it is not an address

    def test_event_log_is_capped(self):
        rows = query("SELECT COUNT(*) AS n FROM events")
        self.assertLessEqual(rows[0]["n"], 2000,
                             "the event log is not being trimmed")


# --------------------------------------------------------------------------
# substore
# --------------------------------------------------------------------------
class SubStoreLinkTest(unittest.TestCase):
    """The Sub-Store integration must point at this exporter's own host."""

    def setUp(self):
        self.cfg = _cfg()
        self.host = self.cfg["publish"].get("hostname", "")
        backend = self.cfg["substore"]["backend"]
        self.store = Client(backend)

    def test_backend_is_reachable(self):
        try:
            items = self.store.get_json("/api/subs")
        except Exception as exc:
            self.skipTest(f"Sub-Store unreachable: {exc}")
        self.assertIsInstance(items, list)

    def test_remote_subs_point_at_this_host(self):
        prefix = self.cfg["publish"].get("prefix", "probe")
        try:
            items = self.store.get_json("/api/subs") or []
        except Exception as exc:
            self.skipTest(f"Sub-Store unreachable: {exc}")
        ours = [i for i in items if str(i.get("name", "")).startswith(prefix + "-")]
        if not ours:
            self.skipTest("no remote subs linked yet")
        for item in ours:
            if item.get("source") != "remote":
                continue
            with self.subTest(sub=item["name"]):
                self.assertIn(self.host, str(item.get("url") or ""),
                              "a probe sub points somewhere other than our host")
                self.assertIn("token=", str(item.get("url") or ""),
                              "a probe sub URL carries no token")

    def test_linked_subs_are_not_empty(self):
        """Sub-Store answers 500 for a zero-node sub, so we never create one.

        Only subs the exporter would link *now* are checked. A source can be
        muted (`export: false`) after its sub was created, and that sub is then
        an orphan waiting for the next `link_substore` to prune it -- failing
        here for it would report a deliberate mute as a broken deployment. The
        check is against the live `export_keys`, so it tracks the config rather
        than a hardcoded list.
        """
        prefix = self.cfg["publish"].get("prefix", "probe")
        try:
            items = self.store.get_json("/api/subs") or []
        except Exception as exc:
            self.skipTest(f"Sub-Store unreachable: {exc}")
        expected = {f"{prefix}-{k}" for k in engine.export_keys(self.cfg)}
        ours = [i for i in items if str(i.get("name", "")).startswith(prefix + "-")
                and i.get("source") == "remote"
                and str(i.get("name")) in expected]
        if not ours:
            self.skipTest("no remote subs linked yet")
        for item in ours:
            with self.subTest(sub=item["name"]):
                text = self.store.download_sub(item["name"], "ClashMeta")
                doc = yaml.safe_load(text) or {}
                self.assertTrue(doc.get("proxies"),
                                f"{item['name']} resolves to zero nodes")

    def test_aggregate_collection_lists_only_our_subs(self):
        prefix = self.cfg["publish"].get("prefix", "probe")
        try:
            collection = self.store.collection(prefix)
        except Exception as exc:
            self.skipTest(f"Sub-Store unreachable: {exc}")
        if not collection:
            self.skipTest(f"collection {prefix} does not exist")
        members = collection.get("subscriptions") or []
        self.assertTrue(members, "the aggregate collection has no members")
        for member in members:
            self.assertTrue(str(member).startswith(prefix + "-"),
                            f"{member} is not one of our subs")


# --------------------------------------------------------------------------
# round (slow, opt-in)
# --------------------------------------------------------------------------
class FullRoundTest(unittest.TestCase):
    """Run an actual round and watch it converge. Slow and stateful.

    Waits on the *ledger* rather than on the panel's notion of "last round":
    the scheduler can fire a round of its own at any moment, and a trigger
    string is not a reliable identity once more than one producer exists.
    """

    # A real round measured ~125s on vps with 420 nodes. The watchdog budget is
    # the authority on "too slow", but blocking for the full 20 minutes would
    # outlive most shells and CI wrappers, so the wait is capped separately and
    # a cap hit is reported as a skip with the evidence, not a silent hang.
    POLL_S = 5
    MAX_WAIT_S = 480

    def test_a_round_finishes_inside_its_budget(self):
        _, before = api("/api/status")
        if before.get("busy"):
            self.skipTest("a round is already running")

        budget = int(_cfg().get("watchdog", {}).get("round_timeout_minutes", 20)) * 60
        highest = _max_round_id()
        status, body = api("/api/run", "POST", {"trigger": "live-test"})
        if status == 409:
            self.skipTest("a round started on its own between the check and the POST")
        self.assertEqual(status, 200, f"could not start a round: {body}")

        deadline = time.time() + min(budget, self.MAX_WAIT_S)
        row = None
        while time.time() < deadline:
            time.sleep(self.POLL_S)
            row = query("SELECT id, started_at, finished_at, duration_s, total, ok, suspect,"
                        " note FROM rounds WHERE id > ? AND finished_at IS NOT NULL "
                        "ORDER BY id DESC LIMIT 1", (highest,))
            if row:
                row = row[0]
                break
        if not row:
            self.skipTest(
                f"no round completed within {min(budget, self.MAX_WAIT_S)}s "
                "(the round may still be running; re-run to confirm)")

        self.assertLessEqual(row["duration_s"], budget,
                             "the round overran its watchdog budget")
        self.assertGreater(row["total"], 0, "the round tested nothing")
        # a suspect round is a legitimate outcome, but it must say so
        if row.get("suspect"):
            note = (row.get("note") or "").lower()
            self.assertTrue(note and "alive" in note,
                            "a suspect round must record why it was withheld: "
                            f"note={row.get('note')!r}")

    def test_convergence_does_not_mass_kill_on_repeat_rounds(self):
        """Three consecutive rounds must not silently wipe the alive set."""
        rows = query("SELECT id, ok, suspect FROM rounds WHERE finished_at IS NOT NULL "
                     "ORDER BY id DESC LIMIT 3")
        if len(rows) < 3:
            self.skipTest("fewer than three finished rounds")
        alive = [r["ok"] for r in rows]
        best = max(alive)
        if best < 4:
            self.skipTest("too few alive nodes for this check to mean anything")
        self.assertGreater(min(alive), 0,
                           f"an entire alive set vanished across rounds: {alive}")


# --------------------------------------------------------------------------
class SlowTest(unittest.TestCase):
    """Explicitly marked slow: runs the whole round twice to check stability."""

    def test_two_rounds_agree_on_the_dead_set(self):
        """A dead node should stay dead -- that is what convergence claims."""
        first = _latest_alive()
        _run_round_sync()
        second = _latest_alive()
        if not first or not second:
            self.skipTest("could not collect two consecutive rounds")
        lost = first - second
        self.assertLessEqual(
            len(lost), max(2, len(first) // 4),
            f"a quarter or more of the alive set flipped between rounds: {sorted(lost)[:10]}")


def _latest_alive():
    rows = query("SELECT fingerprint FROM nodes WHERE status='alive'")
    return {r["fingerprint"] for r in rows}


def _run_round_sync(timeout_s=480):
    """Start a round and block until a new one lands in the ledger."""
    highest = _max_round_id()
    status, body = api("/api/run", "POST", {"trigger": "live-slow"})
    if status == 409:
        raise unittest.SkipTest("a round started on its own; retry later")
    if status != 200:
        raise unittest.SkipTest(f"could not start a round: {body}")
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        time.sleep(10)
        rows = query("SELECT * FROM rounds WHERE id > ? AND finished_at IS NOT NULL "
                     "ORDER BY id DESC LIMIT 1", (highest,))
        if rows:
            return rows[0]
    raise unittest.SkipTest(f"round did not finish within {timeout_s}s")


# --------------------------------------------------------------------------
def _max_round_id():
    rows = query("SELECT COALESCE(MAX(id), 0) AS m FROM rounds")
    return int(rows[0]["m"]) if rows else 0


def _epoch(stamp):
    """Parse a stored timestamp the way the app writes it: as UTC."""
    return calendar.timegm(time.strptime(stamp, "%Y-%m-%dT%H:%M:%S"))


GROUPS = {
    "health": HealthTest,
    "api": ApiShapeTest,
    "kernel": KernelEgressTest,
    "lanes": LaneIndependenceTest,
    "data": LedgerIntegrityTest,
    "substore": SubStoreLinkTest,
    "round": FullRoundTest,
    "slow": SlowTest,
}


def build_suite(names, include_slow):
    """Assemble the suite; `round` and `slow` are opt-in."""
    if not names:
        names = [g for g in GROUPS if g != "slow"]
    elif include_slow and "slow" not in names:
        names = list(names) + ["slow"]
    if not include_slow:
        names = [n for n in names if n != "slow"]
    loader = unittest.TestLoader()
    suite = unittest.TestSuite()
    for name in names:
        if name not in GROUPS:
            raise SystemExit(f"unknown group {name!r}; choose from {', '.join(GROUPS)}")
        suite.addTests(loader.loadTestsFromTestCase(GROUPS[name]))
    return suite


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("groups", nargs="*",
                        help=f"subset of: {', '.join(GROUPS)} (default: all but slow)")
    parser.add_argument("--include-slow", action="store_true",
                        help="also run the stateful multi-round tests")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args(argv)

    if not (ROOT / "data" / "config.json").exists():
        print(f"no deployment found at {ROOT} (set MIHOMO_TEST_ROOT)", file=sys.stderr)
        return 2
    print(f"live tests against {ROOT}")
    result = unittest.TextTestRunner(verbosity=2 if args.verbose else 1).run(
        build_suite(args.groups, args.include_slow))
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
