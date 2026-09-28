"""Unit tests for the pieces that decide what is dead and what gets published."""
import calendar
import collections
import os
import json
import re
import shutil
import sys
import threading
import time
import unittest
import unittest.mock
import urllib.request
from pathlib import Path
import tempfile

import yaml

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import config as cfgmod
from mihomo_test import core as coremod
from mihomo_test import db, engine, notifier, policy, server
from mihomo_test.store import Client, NotFound, StoreError

try:
    import _isolation
except ImportError:  # imported as a package: python -m unittest tests.test_logic
    from tests import _isolation


def setUpModule():
    _isolation.isolate()


def tearDownModule():
    _isolation.restore()




def _drop_temp_db(path):
    """Delete a ``NamedTemporaryFile(delete=False)`` ledger plus any migration
    backup ``db.connect()`` left beside it.

    ``connect()`` copies the ledger to ``<name>.bak-<stamp>`` before rebuilding a
    legacy schema, so unlinking only the ``.db`` leaves the copy behind -- 133 of
    them had accumulated in the system temp dir.
    """
    path = Path(path)
    path.unlink(missing_ok=True)
    for bak in path.parent.glob(path.name + ".bak-*"):
        bak.unlink(missing_ok=True)

_nolog = lambda *a, **k: None


class _JsonResp:
    """Minimal ``urlopen`` stand-in for ``with urlopen(...) as r: json.load(r)``."""

    def __init__(self, payload):
        self._body = json.dumps(payload).encode("utf-8")

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def read(self):
        return self._body


class _GeoCache:
    """In-memory stand-in for the ``ip_geo`` table.

    The real table lives in one sqlite file shared by the whole test run, and
    it survives between runs under the same MIHOMO_TEST_ROOT. A test that lets
    `lookup_countries` write through to it therefore passes on a cold cache and
    fails on a warm one -- "0 != 2", because the address was already known and
    no request was made. Substituting the cache makes these tests independent of
    both run order and prior state.
    """

    def __init__(self):
        self.rows = {}

    def get(self, ips):
        return {ip: self.rows[ip] for ip in ips if ip in self.rows}

    def put(self, rows):
        for row in rows:
            self.rows[row["ip"]] = row.get("country")


class PolicyTest(unittest.TestCase):
    def setUp(self):
        self.p = {"drop_after_consecutive_fails": 3, "suspect_floor_ratio": 0.5,
                  "suspect_floor_absolute": 3}

    def test_first_success_is_alive_immediately(self):
        fields, transition = policy.apply({"status": "unknown", "consec_fail": 0}, True, 42, None, self.p)
        self.assertEqual(fields["status"], "alive")
        self.assertEqual(fields["consec_fail"], 0)
        self.assertEqual(fields["last_delay_ms"], 42)
        self.assertEqual(transition, "new")

    def test_only_a_dead_node_counts_as_a_recovery(self):
        """`restored` is read as the false-kill signal, so churn must not inflate it.

        Upstream churn adds and removes dozens of nodes per round. Counting a
        brand-new node's first success as a "restore" buried that signal -- 139
        of the last 175 restores came from two churn spikes -- so a first
        sighting is reported as "new" instead.
        """
        _, first = policy.apply({"status": "unknown", "consec_fail": 0}, True, 42, None, self.p)
        self.assertEqual(first, "new")
        _, revived = policy.apply({"status": "dead", "consec_fail": 3}, True, 42, None, self.p)
        self.assertEqual(revived, "restore")
        _, steady = policy.apply({"status": "alive", "consec_fail": 0}, True, 42, None, self.p)
        self.assertIsNone(steady)

    def test_single_failure_does_not_drop_an_alive_node(self):
        fields, transition = policy.apply({"status": "alive", "consec_fail": 0}, False, None, "timeout", self.p)
        self.assertEqual(fields["status"], "pending")
        self.assertEqual(fields["consec_fail"], 1)
        self.assertIsNone(transition)

    def test_drop_needs_consecutive_failures(self):
        node = {"status": "alive", "consec_fail": 0}
        transitions = []
        for _ in range(3):
            fields, transition = policy.apply(node, False, None, "timeout", self.p)
            node = dict(node, **fields)
            transitions.append(transition)
        self.assertEqual(node["status"], "dead")
        self.assertEqual(transitions, [None, None, "drop"])

    def test_repeated_failure_after_death_does_not_re_drop(self):
        fields, transition = policy.apply({"status": "dead", "consec_fail": 3}, False, None, "timeout", self.p)
        self.assertEqual(fields["status"], "dead")
        self.assertIsNone(transition)

    def test_success_clears_the_streak(self):
        fields, _ = policy.apply({"status": "pending", "consec_fail": 2}, True, 10, None, self.p)
        self.assertEqual(fields["consec_fail"], 0)
        self.assertEqual(fields["status"], "alive")

    def test_failing_node_that_never_passed_stays_unknown(self):
        fields, transition = policy.apply({"status": "unknown", "consec_fail": 0}, False, None, "kernel_error", self.p)
        self.assertEqual(fields["status"], "unknown")
        self.assertIsNone(transition)

    def test_suspect_guard_trips_on_a_mass_drop(self):
        self.assertTrue(policy.round_is_suspect(5, 40, self.p))
        self.assertFalse(policy.round_is_suspect(38, 40, self.p))
        self.assertFalse(policy.round_is_suspect(3, 0, self.p))

    def test_suspect_guard_ignores_small_sets(self):
        # three survivors is the absolute floor, so it must not trip there
        self.assertFalse(policy.round_is_suspect(3, 5, self.p))


class RetryTest(unittest.TestCase):
    """Retry budget must be spent only where a retry can change the answer."""

    class FakeCore:
        def __init__(self, outcomes):
            self.outcomes = list(outcomes)
            self.calls = []

        def delay(self, name, url, timeout_ms, expected):
            self.calls.append((url, timeout_ms))
            return self.outcomes.pop(0)

    def cfg(self, attempts=3, targets=None):
        return {"targets": targets or ["u1", "u2", "u3"], "expected_status": "204",
                "timeout_ms": 5000, "timeout_ms_retry": 9000,
                "max_attempts": attempts, "retry_pause_s": 0}

    def test_success_on_first_try_costs_one_call(self):
        fake = self.FakeCore([(120, None, "")])
        out = engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertIsNone(out["reason"])
        self.assertEqual(out["attempts"], 1)
        self.assertEqual(len(fake.calls), 1)

    def test_timeout_is_retried_with_a_longer_timeout(self):
        fake = self.FakeCore([(None, "timeout", "Timeout"), (None, "timeout", "Timeout"), (88, None, "")])
        out = engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertIsNone(out["reason"])
        self.assertEqual(out["attempts"], 3)
        self.assertEqual([t for _u, t in fake.calls], [5000, 9000, 9000])

    def test_targets_rotate_across_attempts(self):
        fake = self.FakeCore([(None, "timeout", "T"), (None, "timeout", "T"), (None, "timeout", "T")])
        engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertEqual([u for u, _t in fake.calls], ["u1", "u2", "u3"])

    def test_terminal_reason_stops_immediately(self):
        fake = self.FakeCore([(None, "bad_request", "invalid")])
        out = engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertEqual(out["reason"], "bad_request")
        self.assertEqual(len(fake.calls), 1)

    def test_controller_failure_is_retried(self):
        """A controller blip is exactly what a retry can overturn.

        `controller_error` means our own kernel API was unreachable, so it must
        stay out of TERMINAL_REASONS -- otherwise one blip spends no retry and
        lands in the ledger looking like a node failure.
        """
        self.assertNotIn("controller_error", engine.TERMINAL_REASONS)
        fake = self.FakeCore([(None, "controller_error", "refused")] * 3)
        out = engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertEqual(out["reason"], "controller_error")
        self.assertEqual(len(fake.calls), 3)

    def test_all_dead_returns_the_last_reason(self):
        fake = self.FakeCore([(None, "kernel_error", "An error occurred in the delay test")] * 3)
        out = engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertEqual(out["reason"], "kernel_error")
        self.assertIn("delay test", out["detail"])


class HttpsVerdictTest(unittest.TestCase):
    """Only an HTTPS target may produce an "alive" verdict.

    The rotation once opened on a plain-HTTP connectivity endpoint, and the
    first success short-circuited the loop: 8 of 16 alive chained nodes on the
    live CDN-front chains (2026-09-28) had never been HTTPS-verified, and a
    plain-HTTP 204 says nothing about the TLS path real traffic needs. The
    HTTP target stays in rotation -- it is the only CN-reachable one -- but a
    pass there is a note, not a verdict.
    """

    H = "https://gstatic.example/generate_204"
    C = "https://cloudflare.example/generate_204"
    P = "http://hicloud.example/generate_204"

    class FakeCore:
        def __init__(self, outcomes):
            self.outcomes = list(outcomes)
            self.calls = []

        def delay(self, name, url, timeout_ms, expected):
            self.calls.append(url)
            return self.outcomes.pop(0)

    def cfg(self, attempts=3):
        return {"targets": [self.P, self.H, self.C], "expected_status": "204",
                "timeout_ms": 5000, "timeout_ms_retry": 9000,
                "max_attempts": attempts, "retry_pause_s": 0}

    def test_https_targets_are_tried_before_the_plain_http_one(self):
        # Config order puts the http:// target first; the partition must not
        # spend an attempt on a target that cannot produce a verdict.
        fake = self.FakeCore([(None, "timeout", "T"), (55, None, "")])
        out = engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertIsNone(out["reason"])
        self.assertEqual(fake.calls, [self.H, self.C])

    def test_https_success_is_alive_immediately(self):
        fake = self.FakeCore([(42, None, "")])
        out = engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertIsNone(out["reason"])
        self.assertEqual(out["attempts"], 1)

    def test_http_pass_alone_is_not_alive(self):
        # Both https targets fail, the plain-http one succeeds: the old loop
        # returned alive on the http pass; the new one fails on the https
        # outcome and notes the http pass in the detail.
        fake = self.FakeCore([(None, "timeout", "Timeout"), (None, "timeout", "Timeout"),
                              (99, None, "")])
        out = engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertEqual(out["reason"], "timeout")
        self.assertEqual(out["attempts"], 3)
        self.assertIn("HTTPS 未通过", out["detail"])
        self.assertIn("通（99ms）", out["detail"])

    def test_http_pass_after_https_failures_reports_the_https_reason(self):
        # The verdict is the LAST https attempt's outcome, matching the
        # all-failed case which also reports the last reason.
        fake = self.FakeCore([(None, "kernel_error", "delay test"), (None, "timeout", "T"),
                              (31, None, "")])
        out = engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertEqual(out["reason"], "timeout")
        self.assertIn("HTTPS 未通过", out["detail"])
        self.assertIn("通（31ms）", out["detail"])

    def test_second_https_target_can_still_save_the_node(self):
        fake = self.FakeCore([(None, "timeout", "T"), (77, None, "")])
        out = engine.test_one(fake, {"mihomo": "n"}, self.cfg())
        self.assertIsNone(out["reason"])
        self.assertEqual(out["attempts"], 2)

    def test_http_only_config_keeps_the_legacy_verdict(self):
        # A deployment with no https:// target at all has no rule to enforce:
        # the first pass is still an alive verdict.
        cfg = {"targets": [self.P], "expected_status": "204", "timeout_ms": 5000,
               "timeout_ms_retry": 9000, "max_attempts": 3, "retry_pause_s": 0}
        fake = self.FakeCore([(120, None, "")])
        out = engine.test_one(fake, {"mihomo": "n"}, cfg)
        self.assertIsNone(out["reason"])
        self.assertEqual(out["attempts"], 1)


class ReasonTest(unittest.TestCase):
    def test_timeout_body_maps_to_timeout(self):
        self.assertEqual(coremod._reason_from(504, '{"message":"Timeout"}')[0], "timeout")

    def test_delay_test_error_is_distinguished_from_timeout(self):
        reason, message = coremod._reason_from(
            503, '{"message":"An error occurred in the delay test"}')
        self.assertEqual(reason, "kernel_error")
        self.assertIn("delay test", message)

    def test_html_error_page_is_not_lost(self):
        reason, message = coremod._reason_from(500, "<html>TypeError: boom</html>")
        self.assertEqual(reason, "http_500")
        self.assertIn("TypeError", message)

    def test_status_zero_and_refused_map_to_unreachable(self):
        self.assertEqual(coremod._reason_from(0, "")[0], "unreachable")
        self.assertEqual(
            coremod._reason_from(500, '{"message":"connection refused"}')[0],
            "unreachable")

    def test_unexpected_status_keeps_its_code(self):
        """The f"http_{status}" fallback must stay greppable, not collapse."""
        self.assertEqual(coremod._reason_from(418, "")[0], "http_418")


class DelayClassificationTest(unittest.TestCase):
    """Pin the transport-level reasons; none of them had an assertion before.

    `unreachable`, `bad_response` and `bad_delay` are produced by `Core.delay`
    and decide whether a node looks dead. Without a test they could have
    drifted to another string -- or into a terminal reason that silently kills
    nodes -- and nothing would have noticed.
    """

    def _core(self):
        return coremod.Core({"api": "http://127.0.0.1:1"}, "")

    def test_controller_failure_is_its_own_reason(self):
        """It must not read as a node-side `unreachable`.

        Both used to return the string "unreachable", so the ledger could not
        tell "our kernel API is down" from "this node is dead" -- and a
        controller outage advanced every node's failure streak.
        """

        def boom(*_a, **_k):
            raise coremod.CoreError("ConnectionRefusedError: 控制器不可达")

        with unittest.mock.patch.object(coremod, "_req", boom):
            delay, reason, detail = self._core().delay("n", "http://t/204", 1000)
        self.assertIsNone(delay)
        self.assertEqual(reason, "controller_error")
        self.assertIn("不可达", detail)

    def test_a_node_side_failure_is_still_unreachable(self):
        self.assertEqual(coremod._reason_from(0, "")[0], "unreachable")

    def test_non_json_body_is_bad_response(self):
        with unittest.mock.patch.object(
                coremod, "_req", lambda *_a, **_k: (200, "<html>boom</html>")):
            delay, reason, detail = self._core().delay("n", "http://t/204", 1000)
        self.assertIsNone(delay)
        self.assertEqual(reason, "bad_response")
        self.assertIn("boom", detail)

    def test_missing_or_bogus_delay_is_bad_delay(self):
        for body in ('{"delay": null}', '{"delay": true}', '{"delay": -5}', "{}"):
            with self.subTest(body=body), \
                    unittest.mock.patch.object(
                        coremod, "_req", lambda *_a, **_k: (200, body)):
                delay, reason, _ = self._core().delay("n", "http://t/204", 1000)
            self.assertIsNone(delay)
            self.assertEqual(reason, "bad_delay")


class PrepareTest(unittest.TestCase):
    def entries(self, proxies):
        return [{"source": "s", "name": p["name"], "proxy": p, "index": i}
                for i, p in enumerate(proxies)]

    def test_duplicate_names_are_made_unique(self):
        proxies, mapping, dropped = coremod.prepare(self.entries([
            {"name": "BageVM", "type": "vless", "server": "a", "port": 1},
            {"name": "BageVM", "type": "vless", "server": "b", "port": 2},
        ]))
        self.assertEqual([p["name"] for p in proxies], ["BageVM", "BageVM #2"])
        self.assertEqual([m["original"] for m in mapping], ["BageVM", "BageVM"])
        self.assertEqual(dropped, [])

    def test_dialer_proxy_is_stripped(self):
        proxies, _m, _d = coremod.prepare(self.entries([
            {"name": "n", "type": "vless", "server": "a", "port": 1, "dialer-proxy": "cdn"},
        ]))
        self.assertNotIn("dialer-proxy", proxies[0])

    def test_incomplete_nodes_are_dropped_not_fatal(self):
        proxies, mapping, dropped = coremod.prepare(self.entries([
            {"name": "good", "type": "vless", "server": "a", "port": 1},
            {"name": "no-server", "type": "vless", "port": 2},
            {"name": "no-type", "server": "c", "port": 3},
        ]))
        self.assertEqual([p["name"] for p in proxies], ["good"])
        self.assertEqual(len(mapping), 1)
        self.assertEqual(len(dropped), 2)

    def test_string_port_is_coerced(self):
        proxies, _m, dropped = coremod.prepare(self.entries([
            {"name": "n", "type": "vless", "server": "a", "port": "443"},
        ]))
        self.assertEqual(proxies[0]["port"], 443)
        self.assertEqual(dropped, [])

    def test_duplicate_names_get_distinct_fingerprints(self):
        """Same name, different endpoint -> two independently tracked nodes."""
        _p, mapping, _d = coremod.prepare(self.entries([
            {"name": "BageVM", "type": "vless", "server": "a", "port": 1, "uuid": "x"},
            {"name": "BageVM", "type": "vless", "server": "a", "port": 1, "uuid": "y"},
        ]))
        self.assertNotEqual(mapping[0]["fp"], mapping[1]["fp"])

    def test_fingerprint_ignores_display_name(self):
        """A node renamed upstream keeps its convergence history."""
        first = coremod.fingerprint_proxy({"name": "old", "type": "vless", "server": "a", "port": 1})
        second = coremod.fingerprint_proxy({"name": "new", "type": "vless", "server": "a", "port": 1})
        self.assertEqual(first, second)

    def test_fingerprint_changes_with_credentials(self):
        first = coremod.fingerprint_proxy({"name": "n", "type": "vless", "server": "a", "port": 1, "uuid": "x"})
        second = coremod.fingerprint_proxy({"name": "n", "type": "vless", "server": "a", "port": 1, "uuid": "y"})
        self.assertNotEqual(first, second)


class StoreMissingTest(unittest.TestCase):
    """Sub-Store reports absent resources as HTTP 500; that must read as absent."""

    def test_collection_not_found_payload(self):
        body = ('{"status":"failed","error":{"code":"SUBSCRIPTION_NOT_FOUND",'
                '"type":"ResourceNotFoundError","details":404}}')
        self.assertTrue(Client._is_missing(500, body))

    def test_unhandled_typeerror_page(self):
        self.assertTrue(Client._is_missing(
            500, "<pre>TypeError: Cannot convert undefined or null to object</pre>"))

    def test_real_server_error_is_not_treated_as_missing(self):
        self.assertFalse(Client._is_missing(500, '{"status":"failed","error":{"code":"INTERNAL_SERVER_ERROR"}}'))
        self.assertFalse(Client._is_missing(500, "<html>500 Internal Server Error</html>"))

    def test_404_is_obviously_missing(self):
        self.assertFalse(Client._is_missing(404, "not found"))


class AliasTest(unittest.TestCase):
    """Two names for one endpoint must advance the failure streak once a round."""

    def outcomes(self, spec):
        return {name: {"reason": reason, "delay_ms": delay, "attempts": attempts, "detail": ""}
                for name, (reason, delay, attempts) in spec.items()}

    def test_aliases_collapse_into_one_bucket(self):
        by_name = {
            "A": {"source": "air", "fp": "same", "original": "CH BRN Buyvm"},
            "B": {"source": "air", "fp": "same", "original": "CH BRN Buyvm IPv6"},
        }
        results = self.outcomes({"A": ("kernel_error", None, 3), "B": ("kernel_error", None, 3)})
        grouped = engine.group_by_fingerprint(results, by_name)
        self.assertEqual(len(grouped), 1)
        self.assertEqual(sorted(next(iter(grouped.values()))["names"]), ["A", "B"])

    def test_distinct_fingerprints_stay_separate(self):
        by_name = {"A": {"source": "air", "fp": "f1"}, "B": {"source": "air", "fp": "f2"}}
        results = self.outcomes({"A": (None, 10, 1), "B": (None, 20, 1)})
        self.assertEqual(len(engine.group_by_fingerprint(results, by_name)), 2)

    def test_same_endpoint_in_two_sources_stays_separate(self):
        by_name = {"A": {"source": "air", "fp": "f"}, "B": {"source": "other", "fp": "f"}}
        results = self.outcomes({"A": (None, 1, 1), "B": (None, 1, 1)})
        self.assertEqual(len(engine.group_by_fingerprint(results, by_name)), 2)

    def test_excluded_exit_country_fails_the_node(self):
        country, failure = engine._resolve_exit({"A": {"country": "CN", "error": None}}, ["A"], {"CN"})
        self.assertEqual(country, "CN")
        self.assertEqual(failure, "exit_CN")

    def test_verification_error_fails_the_node(self):
        _country, failure = engine._resolve_exit({"A": {"country": None, "error": "TLS closed"}}, ["A"], {"CN"})
        self.assertEqual(failure, "verify_failed")

    def test_skipped_verification_leaves_the_node_alone(self):
        _country, failure = engine._resolve_exit({}, ["A"], {"CN"})
        self.assertIsNone(failure)

    def test_verified_alias_rescues_the_pair(self):
        country, failure = engine._resolve_exit({"B": {"country": "JP", "error": None}}, ["A", "B"], {"CN"})
        self.assertEqual(country, "JP")
        self.assertIsNone(failure)


class ExportTest(unittest.TestCase):
    def cfg(self, tag=False):
        return {"publish": {"add_region_tag": tag}}

    def test_same_endpoint_under_two_names_is_published_once(self):
        proxies = {
            "A": {"name": "CH BRN Buyvm", "type": "mieru", "server": "s", "port": 1,
                  "username": "u", "password": "p"},
            "B": {"name": "CH BRN Buyvm IPv6", "type": "mieru", "server": "s", "port": 1,
                  "username": "u", "password": "p"},
        }
        out, _ = engine._export_proxies(["A", "B"], proxies, {}, self.cfg(), "k")
        self.assertEqual(len(out), 1)

    def test_genuinely_different_nodes_are_both_published(self):
        proxies = {
            "A": {"name": "A", "type": "mieru", "server": "s", "port": 1, "username": "u"},
            "B": {"name": "B", "type": "mieru", "server": "s", "port": 2, "username": "u"},
        }
        out, _ = engine._export_proxies(["A", "B"], proxies, {}, self.cfg(), "k")
        self.assertEqual(len(out), 2)

    def test_one_nodes_two_measured_forms_are_published_once(self):
        """With both switches on, a chained node is tested twice.

        The two variants share a display name but differ in `dialer-proxy`, a
        connection field, so the fingerprint pass cannot collapse them -- and
        the client would receive two nodes called the same thing. The export
        speaks for the source's configured form, so the chained variant wins
        and the direct one stays a panel-only measurement.
        """
        proxies = {
            "HK-01": {"name": "HK-01", "type": "vless", "server": "s", "port": 1,
                      "uuid": "u", "dialer-proxy": "cdn"},
            "HK-01 #2": {"name": "HK-01", "type": "vless", "server": "s", "port": 1,
                         "uuid": "u"},
            "cdn": {"name": "cdn", "type": "vless", "server": "f", "port": 1,
                    "uuid": "f"},
        }
        out, _ = engine._export_proxies(["HK-01", "HK-01 #2", "cdn"], proxies,
                                        {}, self.cfg(), "k")
        self.assertEqual(sorted(p["name"] for p in out), ["HK-01", "cdn"])
        chained = next(p for p in out if p["name"] == "HK-01")
        self.assertEqual(chained.get("dialer-proxy"), "cdn")

    def test_a_direct_only_node_keeps_its_export(self):
        """The collapse must not eat a node that is only measured direct.

        A source with `chain` off publishes the stripped form -- there is no
        chained variant to prefer, so the direct one has to survive.
        """
        proxies = {"HK-01": {"name": "HK-01", "type": "vless", "server": "s",
                             "port": 1, "uuid": "u"}}
        out, _ = engine._export_proxies(["HK-01"], proxies, {}, self.cfg(), "k")
        self.assertEqual([p["name"] for p in out], ["HK-01"])
        self.assertNotIn("dialer-proxy", out[0])

    def test_region_tag_is_prepended_from_the_verified_exit(self):
        proxies = {"A": {"name": "node", "type": "vless", "server": "s", "port": 1}}
        out, _ = engine._export_proxies(["A"], proxies, {"A": {"country": "JP"}}, self.cfg(tag=True), "k")
        self.assertEqual(out[0]["name"], "[JP] node")

    def test_region_tag_is_not_applied_twice(self):
        proxies = {"A": {"name": "[JP] node", "type": "vless", "server": "s", "port": 1}}
        out, _ = engine._export_proxies(["A"], proxies, {"A": {"country": "JP"}}, self.cfg(tag=True), "k")
        self.assertEqual(out[0]["name"], "[JP] node")

    def test_verified_exit_overrides_a_stale_tag(self):
        proxies = {"A": {"name": "[SG] node", "type": "vless", "server": "s", "port": 1}}
        out, _ = engine._export_proxies(["A"], proxies, {"A": {"country": "JP"}}, self.cfg(tag=True), "k")
        self.assertEqual(out[0]["name"], "[JP] node")


class YamlScalarQuotingTest(unittest.TestCase):
    """A string that *looks* like a number must be quoted on the way out.

    The export is read by the client's kernel, which uses Go's `yaml.v3` and
    therefore the YAML 1.1 core schema. PyYAML, which writes the file, uses
    the 1.2 schema: it sees `123456e2` as plain text, emits it bare, and the
    client's kernel then reads it as the float 473277000.

    Measured against the live kernel (mihomo v1.19.29), changing only this
    one field of an otherwise valid node:

        short-id: 123456e2     -> exit 1  "invalid REALITY short ID"
        short-id: '123456e2'   -> exit 0
        short-id: deadbeef00     -> exit 0

    Across all 426 exported nodes exactly this one value tripped it. The probe
    never saw the fault because its own kernel config is written as inline
    JSON -- `{"short-id": "123456e2"}` is quoted there -- so only the export
    path, which is YAML, could break.
    """

    def test_a_scientific_lookalike_is_quoted(self):
        text = yaml.dump({"short-id": "123456e2"}, Dumper=engine._ExportDumper, allow_unicode=True)
        self.assertEqual(text, "short-id: '123456e2'\n")
        self.assertIsInstance(yaml.safe_load(text)["short-id"], str)

    def test_other_yaml_11_shapes_are_quoted_too(self):
        for value in ["0x1f", "0755", "true", "no", "off", "~", "null", "1:30"]:
            text = yaml.dump({"v": value}, Dumper=engine._ExportDumper, allow_unicode=True)
            loaded = yaml.safe_load(text)["v"]
            self.assertEqual(loaded, value, f"{value!r} round-tripped as {loaded!r}")

    def test_ordinary_values_are_left_bare(self):
        """A domain, uuid or server address must not sprout quotes.

        Values PyYAML itself must quote for structural reasons -- a leading
        `[`, an embedded `: ` -- are out of scope here; that behaviour predates
        this dumper and is correct.
        """
        for value in ["dlcdnets.asus.com", "00000000-0000-4000-8000-000000000004",
                      "deadbeef00", "xtls-rprx-vision", "198.51.100.72",
                      "2001:db8:85a3:0:0:8a2e:370:7334", "cdn前置"]:
            text = yaml.dump({"v": value}, Dumper=engine._ExportDumper, allow_unicode=True)
            self.assertEqual(text, f"v: {value}\n", f"{value!r} was quoted")
            self.assertEqual(yaml.safe_load(text)["v"], value)

    def test_a_display_name_with_a_leading_bracket_survives(self):
        """`[TW] ...` is quoted by PyYAML either way; the value must round-trip."""
        text = yaml.dump({"v": "[TW] US-05 · VLESS"}, Dumper=engine._ExportDumper,
                         allow_unicode=True)
        self.assertEqual(yaml.safe_load(text)["v"], "[TW] US-05 · VLESS")

    def test_non_strings_keep_their_native_form(self):
        text = yaml.dump({"port": 443, "udp": True, "ratio": 1.5},
                         Dumper=engine._ExportDumper, sort_keys=False, allow_unicode=True)
        self.assertEqual(text, "port: 443\nudp: true\nratio: 1.5\n")

    def test_a_written_export_survives_round_trip_with_types_intact(self):
        proxies = {"A": {"name": "A", "type": "vless", "server": "s", "port": 443,
                         "reality-opts": {"public-key": "k", "short-id": "123456e2"}}}
        payload = {"proxies": engine._export_proxies(
            ["A"], proxies, {}, {"publish": {"add_region_tag": False}}, "k")[0]}
        text = yaml.dump(payload, Dumper=engine._ExportDumper, sort_keys=False, allow_unicode=True)
        doc = yaml.safe_load(text)
        sid = doc["proxies"][0]["reality-opts"]["short-id"]
        self.assertEqual(sid, "123456e2")
        self.assertIsInstance(sid, str)


class DerivedDialerGroupTest(unittest.TestCase):
    """An export must be usable by a client, not just loadable.

    Two regression layers led here. The first was found by feeding a real
    export to the real kernel, which refused the whole file:

        proxy [[GB] GB-09 · SS] dialer-proxy [cdn] not found
        configuration file test failed

    `cdn` is the *upstream* author's own dialer name and exists in no export.
    The first rewrite made the file loadable but not usable: the emitted group
    listed the export's own chained nodes (every selection dialled itself) and
    the front proxies were never published, so no client could resolve a
    working front. On top of that, Sub-Store drops `proxy-groups` when
    re-rendering a subscription for download -- measured on this deployment,
    the group exists on disk but the downloaded sub has none -- so the only
    rewrite guaranteed to survive the client's pipeline points the dialer
    straight at a published front proxy.
    """

    def cfg(self, front="CM-CF", kind="sub", cap=1, sources=None):
        return {
            "publish": {"add_region_tag": False},
            "chain": {"enabled": True, "max_fronts": cap,
                      "front_source": {"kind": kind, "name": front}},
            "sources": sources if sources is not None else [
                {"key": "k", "kind": "collection", "enabled": True}],
        }

    def proxies(self, dialer="cdn"):
        return {"A": {"name": "node-a", "type": "ss", "server": "1.1.1.1", "port": 443,
                      engine.DIALER_FIELD: dialer}}

    def fronts(self, *names):
        return [{"name": n, "type": "vless", "server": f"{n}.example", "port": 443,
                 "uuid": "u", "network": "ws"} for n in names]

    def test_one_live_front_is_published_and_referenced_directly(self):
        # Single-front pool: no group, the dialer names the published front
        # proxy itself -- the form that survives Sub-Store's group-stripping.
        out, groups = engine._export_proxies(["A"], self.proxies(), {}, self.cfg(), "k",
                                             chain_fronts=self.fronts("HK-Alice"))
        by_name = {p["name"]: p for p in out}
        self.assertIn("[前置] HK-Alice", by_name)
        self.assertEqual(by_name["node-a"][engine.DIALER_FIELD], "[前置] HK-Alice")
        self.assertEqual(groups, [])
        front = by_name["[前置] HK-Alice"]
        self.assertEqual(front["server"], "HK-Alice.example")

    def test_multi_front_pool_gets_a_select_group_of_fronts(self):
        out, groups = engine._export_proxies(["A"], self.proxies(), {}, self.cfg(), "k",
                                             chain_fronts=self.fronts("F1", "F2"))
        names = [p["name"] for p in out]
        self.assertEqual(groups[0]["name"], "CM-CF")
        self.assertEqual(groups[0]["proxies"], ["[前置] F1", "[前置] F2"])
        self.assertTrue(set(groups[0]["proxies"]) <= set(names))
        node = next(p for p in out if p["name"] == "node-a")
        self.assertEqual(node[engine.DIALER_FIELD], "CM-CF")

    def test_group_name_falls_back_when_a_node_shadows_it(self):
        # mihomo keys proxies and groups in one namespace: a node named CM-CF
        # would make the group unresolvable, so the name falls back.
        proxies = {"A": {"name": "CM-CF", "type": "ss", "server": "s", "port": 1,
                         engine.DIALER_FIELD: "cdn"}}
        out, groups = engine._export_proxies(["A"], proxies, {}, self.cfg(), "k",
                                             chain_fronts=self.fronts("F1", "F2"))
        self.assertEqual(groups[0]["name"], engine.FRONT_GROUP_FALLBACK)
        self.assertEqual(groups[0]["proxies"], ["[前置] F1", "[前置] F2"])

    def test_export_without_chained_nodes_is_untouched(self):
        plain = {"A": {"name": "n", "type": "ss", "server": "s", "port": 1}}
        out, groups = engine._export_proxies(["A"], plain, {}, self.cfg(), "k",
                                             chain_fronts=self.fronts("F1"))
        self.assertEqual(groups, [])
        self.assertEqual([p["name"] for p in out], ["n"])
        self.assertNotIn(engine.DIALER_FIELD, out[0])

    def test_no_live_fronts_drops_the_chained_nodes(self):
        # An unpublishable chained node must not drag the whole file down with
        # it: one dangling dialer makes a client kernel reject the export.
        out, groups = engine._export_proxies(["A"], self.proxies(), {}, self.cfg(), "k",
                                             chain_fronts=[])
        self.assertEqual(out, [])
        self.assertEqual(groups, [])

    def test_no_front_history_drops_the_chained_nodes_too(self):
        out, groups = engine._export_proxies(["A"], self.proxies(), {}, self.cfg(), "k")
        self.assertEqual(out, [])
        self.assertEqual(groups, [])

    def test_chaining_switched_off_drops_the_chained_nodes(self):
        # With no chain block there is no pool to resolve against; publishing
        # the chained form anyway is what made the file unloadable.
        cfg = self.cfg()
        cfg["chain"] = None
        out, groups = engine._export_proxies(["A"], self.proxies(), {}, cfg, "k",
                                             chain_fronts=self.fronts("F1"))
        self.assertEqual(out, [])
        self.assertEqual(groups, [])

    def test_second_source_over_the_same_pool_resolves_to_the_same_target(self):
        # Two chained sources fed by one front pool must agree on the dialer
        # target, otherwise importing both produces two unrelated topologies.
        cfg = self.cfg(cap=3, sources=[
            {"key": "k", "kind": "collection", "enabled": True},
            {"key": "k2", "kind": "collection", "enabled": True}])
        fronts = self.fronts("F1", "F2")
        a, ga = engine._export_proxies(["A"], self.proxies(), {}, cfg, "k",
                                       chain_fronts=fronts)
        b, gb = engine._export_proxies(["A"], self.proxies(dialer="cdn"), {}, cfg, "k2",
                                       chain_fronts=fronts)
        self.assertEqual(ga[0]["name"], gb[0]["name"])
        self.assertEqual(a[0][engine.DIALER_FIELD], b[0][engine.DIALER_FIELD])

    def test_a_dialer_naming_a_published_proxy_is_left_alone(self):
        # Self-consistent upstream: the client can already resolve it, and the
        # author's own topology may be meaningful, so nothing is rewritten and
        # no front is published.
        proxies = {
            "A": {"name": "front-node", "type": "ss", "server": "f", "port": 1},
            "B": {"name": "node-b", "type": "ss", "server": "s", "port": 2,
                  engine.DIALER_FIELD: "front-node"},
        }
        out, groups = engine._export_proxies(["A", "B"], proxies, {}, self.cfg(), "k",
                                             chain_fronts=self.fronts("F1"))
        self.assertEqual([p["name"] for p in out], ["front-node", "node-b"])
        self.assertEqual(groups, [])

    def test_only_the_unresolvable_dialer_is_rewritten(self):
        # Mixed input: one node's dialer resolves locally, one does not.
        proxies = {
            "A": {"name": "front-node", "type": "ss", "server": "f", "port": 1},
            "B": {"name": "node-b", "type": "ss", "server": "s", "port": 2,
                  engine.DIALER_FIELD: "front-node"},
            "C": {"name": "node-c", "type": "ss", "server": "s", "port": 3,
                  engine.DIALER_FIELD: "dangling"},
        }
        out, groups = engine._export_proxies(["A", "B", "C"], proxies, {}, self.cfg(), "k",
                                             chain_fronts=self.fronts("F1", "F2"))
        by = {p["name"]: p for p in out}
        self.assertEqual(by["node-b"][engine.DIALER_FIELD], "front-node")
        self.assertEqual(by["node-c"][engine.DIALER_FIELD], "CM-CF")
        self.assertEqual([g["name"] for g in groups], ["CM-CF"])

    def test_front_proxies_are_sanitised_and_deduplicated(self):
        # A front carries no dialer of its own (a nested chain is not a
        # topology this deployment has), and two fronts with the same display
        # name must not collapse -- mihomo keys proxies by name, so the second
        # gets a distinguishing suffix.
        fronts = [{"name": "F1", "type": "vless", "server": "f", "port": 443,
                   "uuid": "u", engine.DIALER_FIELD: "other"},
                  {"name": "F1", "type": "vless", "server": "f2", "port": 443,
                   "uuid": "u"}]
        out, groups = engine._export_proxies(["A"], self.proxies(), {}, self.cfg(), "k",
                                             chain_fronts=fronts)
        fronts_out = [p for p in out if p["name"].startswith("[前置] ")]
        self.assertEqual([p["name"] for p in fronts_out],
                         ["[前置] F1", "[前置] F1 #2"])
        self.assertNotIn(engine.DIALER_FIELD, fronts_out[0])

    def test_written_export_carries_the_front_so_the_kernel_can_resolve_it(self):
        # End-to-end within the module: whatever `_write_export` writes is what
        # the client loads, so the rewrite has to survive serialisation.
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            original = engine.EXPORT_DIR
            engine.EXPORT_DIR = Path(d)
            try:
                path = engine._write_export(self.cfg(), "k", ["A"], self.proxies(), {},
                                            chain_fronts=self.fronts("HK-Alice"))
            finally:
                engine.EXPORT_DIR = original
            data = yaml.safe_load(path.read_text(encoding="utf-8"))
        names = {p["name"] for p in data["proxies"]}
        self.assertIn("[前置] HK-Alice", names)
        node = next(p for p in data["proxies"] if p["name"] == "node-a")
        self.assertEqual(node[engine.DIALER_FIELD], "[前置] HK-Alice")
        self.assertNotIn("proxy-groups", data)


class RoundModeTest(unittest.TestCase):
    """`engine.round_uses_chains` decides whether a manual round dials chains.

    The dashboard's 直连测活 button must not touch the front pool; 链式测活 and
    the scheduler must chain as configured. The split is at the front-dialling
    step only -- both modes still test every source, so the ledger prune and the
    export write see a complete round. (A round that skipped nodes would let
    `_prune_removed_nodes` delete their rows and `_publish_sources` truncate
    their exports.)
    """

    def _cfg(self, chain_enabled, front=""):
        return {
            "sources": [{"key": "air", "kind": "collection", "enabled": True}],
            "chain": {"enabled": chain_enabled,
                      "front_source": {"kind": "sub", "name": front}},
        }

    def test_direct_mode_never_chains(self):
        self.assertFalse(engine.round_uses_chains(self._cfg(True, "cf"), "direct"))
        self.assertFalse(engine.round_uses_chains(self._cfg(False, "cf"), "direct"))

    def test_chain_mode_chains_when_configured(self):
        self.assertTrue(engine.round_uses_chains(self._cfg(True, "cf"), "chain"))

    def test_scheduler_default_chains_when_configured(self):
        self.assertTrue(engine.round_uses_chains(self._cfg(True, "cf"), None))

    def test_no_chain_when_disabled(self):
        self.assertFalse(engine.round_uses_chains(self._cfg(False, "cf"), None))

    def test_no_chain_when_front_source_is_unconfigured(self):
        # `chain_block` returns None for "enabled but no front source", so the
        # round must not chain -- otherwise every chained node fails front_dead
        # because of a missing config field, which reads as a network problem.
        self.assertFalse(engine.round_uses_chains(self._cfg(True, ""), None))

    def test_direct_overrides_a_valid_chain_config(self):
        cfg = self._cfg(True, "cf")
        self.assertFalse(engine.round_uses_chains(cfg, "direct"))
        self.assertTrue(engine.round_uses_chains(cfg, None))


class RunInBackgroundModeTest(unittest.TestCase):
    """The dashboard's two launch buttons reach `engine.run_round` with `mode`."""

    def test_mode_is_forwarded_to_run_round(self):
        from mihomo_test import server

        captured = {}
        real_run = engine.run_round

        def fake_run(cfg, trigger="manual", only_source=None, mode=None, log=_nolog):
            captured["mode"] = mode
            captured["trigger"] = trigger
            return {"ok": True}

        with unittest.mock.patch.object(engine, "run_round", fake_run):
            t = server.run_in_background({"sources": []}, trigger="manual", mode="chain")
            t.join(timeout=5)
        self.assertEqual(captured.get("mode"), "chain")
        self.assertEqual(captured.get("trigger"), "manual")

    def test_busy_round_is_refused(self):
        from mihomo_test import server

        if not server.BUSY.acquire(blocking=False):
            self.skipTest("BUSY already held by another test")
        try:
            with self.assertRaises(engine.Busy):
                server.run_in_background({"sources": []}, mode="direct")
        finally:
            server.BUSY.release()


class RoundModePersistenceTest(unittest.TestCase):
    """The mode of a round is recorded, not just logged.

    The dashboard header names the running mode, which it can only do if the
    mode survives the round: `rounds.mode` for history, `round.state.json` and
    `engine.current_mode()` for "what is running right now".
    """

    def test_start_round_stores_the_mode(self):
        from mihomo_test import db

        rid = db.start_round("manual", "chain")
        row = db.one("SELECT mode FROM rounds WHERE id=?", (rid,))
        self.assertEqual(row["mode"], "chain")

    def test_start_round_stores_null_for_a_scheduler_round(self):
        """mode=None is a full chain-aware round, not a missing value.

        SQL NULL rather than the string "None": the UI distinguishes the two
        ("自动调度" vs a named mode), so a caller that forgot the argument must
        not look like a deliberate scheduler round.
        """
        from mihomo_test import db

        rid = db.start_round("schedule")
        row = db.one("SELECT mode FROM rounds WHERE id=?", (rid,))
        self.assertIsNone(row["mode"])

    def test_rounds_table_has_a_mode_column_after_migration(self):
        """An existing database gains the column in place, not by rebuild.

        `rounds` is never dropped by `_migrate` (only nodes/results are), so an
        additive ALTER is the only way an upgrade keeps its round history.
        """
        from mihomo_test import db

        db.connect()  # runs _migrate
        cols = {r[1] for r in db.connect().execute("PRAGMA table_info(rounds)")}
        self.assertIn("mode", cols)

    def test_write_state_persists_the_mode(self):
        """`round.state.json` carries the mode, for an operator reading it."""
        from mihomo_test import engine

        engine._write_state("delay-test", 4242, "direct")
        try:
            state = json.loads(engine.ROUND_STATE.read_text(encoding="utf-8"))
        finally:
            engine.ROUND_STATE.unlink(missing_ok=True)
        self.assertEqual(state["mode"], "direct")
        self.assertEqual(state["phase"], "delay-test")
        self.assertEqual(state["round_id"], 4242)

    def test_current_mode_is_cleared_with_the_round(self):
        from mihomo_test import engine

        engine._set_current_round(7, "chain")
        self.assertEqual(engine.current_mode(), "chain")
        engine._set_current_round(None)
        self.assertIsNone(engine.current_mode())

    def test_status_payload_reports_busy_mode_only_while_busy(self):
        """An idle dashboard must not show a mode pill.

        `current_mode()` is cleared with the round, but `busy` is the field the
        UI gates on, so both have to agree -- a stale mode with busy false
        would leave the header naming a round that already finished.
        """
        from mihomo_test import server

        cfg = {"sources": [], "auth": {}, "publish": {}}
        engine._set_current_round(7, "direct")
        try:
            if server.BUSY.locked():
                self.skipTest("BUSY already held by another test")
            # busy false -> no mode shown, whatever the engine remembers
            self.assertIsNone(server.status_payload(cfg)["busy_mode"])
            server.BUSY.acquire(blocking=False)
            try:
                self.assertEqual(server.status_payload(cfg)["busy_mode"], "direct")
            finally:
                server.BUSY.release()
        finally:
            engine._set_current_round(None)


class RoundTeardownScopeTest(unittest.TestCase):
    """`_reconcile_ledger` must not read a name that no longer exists.

    Round 225 on vps recorded `aborted: NameError: name 'chain_on' is not
    defined` with ok=0, after the nodes had been tested but before the exports
    were written -- so the round published nothing and the panel went on
    showing the previous round's numbers. A stale reference left by a rename
    compiles fine and only fires at the very end of a round.

    These call the real function with the ledger writes stubbed, so a future
    rename is caught by running the code rather than by matching source text.
    """

    def _cfg(self, chain):
        return {
            "sources": [{"key": "src-on", "kind": "collection", "name": "on",
                         "enabled": True}],
            "chain": chain,
        }

    def _run(self, cfg, fronts, chain_configured):
        seen = {"prune": None, "demote": None}
        with unittest.mock.patch.object(
                db, "delete_sources_not_in",
                side_effect=lambda k: seen.__setitem__("prune", list(k)) or 0), \
             unittest.mock.patch.object(
                db, "demote_disabled_sources",
                side_effect=lambda k: seen.__setitem__("demote", list(k)) or 0):
            keep = engine._reconcile_ledger(
                cfg, [s for s in cfg["sources"] if s.get("enabled")],
                fronts, chain_configured, _nolog)
        return keep, seen

    def test_direct_round_keeps_the_pool_when_chaining_is_configured(self):
        """The exact shape that broke: no fronts collected, chaining on."""
        cfg = self._cfg({"enabled": True,
                         "front_source": {"kind": "sub", "name": "CM-CF"},
                         "max_fronts": 1})
        keep, seen = self._run(cfg, fronts=[], chain_configured=True)
        self.assertTrue(keep)
        self.assertIn(engine.FRONT_SOURCE_KEY, seen["prune"])
        self.assertIn(engine.FRONT_SOURCE_KEY, seen["demote"])

    def test_pool_is_preserved_even_without_fronts(self):
        """`fronts` empty but configured -> still keep.

        Keying this off `fronts` alone would delete the pool on every direct
        round, so a later chain round would start from no history.
        """
        cfg = self._cfg({"enabled": True,
                         "front_source": {"kind": "sub", "name": "CM-CF"},
                         "max_fronts": 1})
        keep, seen = self._run(cfg, fronts=[], chain_configured=True)
        self.assertTrue(keep, "front pool dropped although chaining is configured")
        self.assertIn(engine.FRONT_SOURCE_KEY, seen["prune"])

    def test_pool_is_dropped_when_chaining_is_off(self):
        """The mirror case: nothing collected and nothing configured.

        Keeping the key here would preserve rows for a front pool that no
        longer exists, which is how a deleted source lingers as `unknown`.
        """
        cfg = self._cfg(None)
        keep, seen = self._run(cfg, fronts=[], chain_configured=False)
        self.assertFalse(keep)
        self.assertNotIn(engine.FRONT_SOURCE_KEY, seen["prune"])
        self.assertEqual(seen["prune"], ["src-on"])

    def test_collected_fronts_keep_the_pool_even_if_flag_is_false(self):
        """`fronts` non-empty is on its own enough. Covers the case where the
        config was reloaded mid-round and the flag disagrees."""
        cfg = self._cfg(None)
        keep, seen = self._run(cfg, fronts=[{"proxy": {"name": "__FRONT0__"}}],
                               chain_configured=False)
        self.assertTrue(keep)
        self.assertIn(engine.FRONT_SOURCE_KEY, seen["prune"])

    def test_run_round_calls_the_reconciler_at_teardown(self):
        """`_run_round` must still route through `_reconcile_ledger`.

        The NameError happened because this call was inline. Asserting the call
        site (rather than the source text) keeps the routing honest without
        matching strings, and it fails if a future edit inlines it again and
        brings a stale name along.
        """
        with unittest.mock.patch.object(engine, "_reconcile_ledger") as spy:
            spy.return_value = True
            with unittest.mock.patch.object(engine, "collect_entries",
                                            return_value=([], [])):
                engine._run_round({"substore": {"backend": "http://127.0.0.1:1"},
                                   "sources": [], "chain": None},
                                  "test", None, _nolog, mode="direct")
        spy.assert_not_called()  # no sources -> returns before teardown


class RoundLockTest(unittest.TestCase):
    """The CLI and the service are separate processes; they must not overlap."""

    def test_in_process_lock_survives_a_failing_file_unlock(self):
        """A raising file-unlock must not strand the in-process lock forever.

        `_release_file_lock` runs first in `run_round`'s `finally`. When it
        raised, `_round_lock.release()` was skipped, every later round failed
        with Busy, and `server.run_in_background` swallowed that without a log
        line -- so the service would stop testing and still look idle.
        """
        with unittest.mock.patch.object(
                engine, "_acquire_file_lock", lambda: object()), \
                unittest.mock.patch.object(
                    engine, "_release_file_lock",
                    unittest.mock.Mock(side_effect=OSError("flock blew up"))), \
                unittest.mock.patch.object(
                    engine, "_run_round", lambda *a, **k: {"ok": True}):
            with self.assertRaises(OSError):
                engine.run_round({"sources": []}, log=_nolog)
        acquired = engine._round_lock.acquire(blocking=False)
        if acquired:
            engine._round_lock.release()
        self.assertTrue(acquired, "the in-process round lock was left held")

    @unittest.skipIf(os.name != "posix", "flock is POSIX-only")
    def test_second_round_is_refused_while_one_holds_the_lock(self):
        import shutil
        import tempfile
        from pathlib import Path

        from mihomo_test import config as cfgmod

        tmp = Path(tempfile.mkdtemp())
        old_data = cfgmod.DATA
        try:
            cfgmod.DATA = tmp
            handle = engine._acquire_file_lock()
            self.assertIsNotNone(handle)
            with self.assertRaises(engine.Busy):
                engine._acquire_file_lock()
            engine._release_file_lock(handle)
            # released: a new round may start
            again = engine._acquire_file_lock()
            self.assertIsNotNone(again)
            engine._release_file_lock(again)
        finally:
            cfgmod.DATA = old_data
            shutil.rmtree(tmp, ignore_errors=True)


class ConfigPatchTest(unittest.TestCase):
    """A patch from the UI must not be able to write settings that do nothing."""

    def test_a_deployment_override_in_a_patch_is_kept(self):
        """Only the verified-dead keys are refused; a real override must pass.

        Rejecting everything not in DEFAULTS would break a working install --
        `core.mixed_port` is read by `core.build_config` and is not a default.
        """
        patch = {"core": {"mixed_port": 19194, "probe_group": "__PROBE__"}}
        clean, notes = cfgmod.validate_patch(patch)
        self.assertEqual(clean["core"], {"mixed_port": 19194})
        self.assertTrue(any("已废弃" in n for n in notes), notes)

    def test_out_of_range_numbers_are_clamped_not_rejected(self):
        clean, notes = cfgmod.validate_patch({"schedule": {"interval_minutes": 0}})
        self.assertEqual(clean["schedule"]["interval_minutes"], 1)
        self.assertTrue(any("夹取" in n for n in notes), notes)

    def test_a_non_numeric_interval_falls_back_to_the_default(self):
        """It used to raise inside the scheduler, which swallowed the error.

        The scheduler then simply stopped firing and nothing said so.
        """
        clean, notes = cfgmod.validate_patch({"schedule": {"interval_minutes": "soon"}})
        self.assertEqual(clean["schedule"]["interval_minutes"],
                         cfgmod.DEFAULTS["schedule"]["interval_minutes"])
        self.assertTrue(any("不是数字" in n for n in notes), notes)

    def test_an_empty_token_is_refused(self):
        """`auth_ok` passes on a falsy token, so empty disables every check."""
        clean, notes = cfgmod.validate_patch({"auth": {"token": ""}})
        self.assertNotIn("token", clean.get("auth", {}))
        self.assertTrue(any("auth.token" in n for n in notes), notes)

    def test_a_good_patch_passes_through_untouched(self):
        patch = {"test": {"concurrency": 30},
                 "sources": [{"key": "k", "name": "k"}]}
        clean, notes = cfgmod.validate_patch(patch)
        self.assertEqual(clean, patch)
        self.assertEqual(notes, [])


class ConfigSchemaTest(unittest.TestCase):
    """DEFAULTS is not the schema; only keys with no reader may be pruned."""

    def test_keys_the_code_reads_but_defaults_never_declared_survive(self):
        """Regression for round 99 on vps (2026-09-20).

        The deployed config.json carried `core.mixed_port`, which DEFAULTS never
        declared and `core.build_config` reads with a direct subscript
        (`core_cfg["mixed_port"]`). A prune based on "not in DEFAULTS" deleted
        it and every round died with KeyError until the file was restored. A key
        can be read by code and absent from DEFAULTS -- that is the normal shape
        of a deployment-specific override.
        """
        stored = {"core": {"mixed_port": 19194, "service": "mihomo-probe",
                           "compose_file": "/srv/mihomo-test/docker-compose.yml",
                           "api": "http://127.0.0.1:19190"}}
        kept, dropped = cfgmod.prune_dead(stored)
        self.assertEqual(dropped, [])
        self.assertEqual(kept["core"]["mixed_port"], 19194)
        self.assertEqual(kept["core"]["service"], "mihomo-probe")
        self.assertEqual(kept["core"]["compose_file"],
                         "/srv/mihomo-test/docker-compose.yml")

    def test_only_the_verified_dead_keys_are_dropped(self):
        stored = {"core": {"probe_group": "__PROBE__",
                           "config_path": "/srv/mihomo-test/core/config.yaml",
                           "mixed_port": 19194},
                  "auth": {"token": "keep-me"}}
        kept, dropped = cfgmod.prune_dead(stored)
        self.assertEqual(sorted(dropped),
                         ["core.config_path", "core.probe_group"])
        self.assertEqual(kept["core"], {"mixed_port": 19194})
        self.assertEqual(kept["auth"], {"token": "keep-me"})

    def test_user_chosen_subtrees_survive(self):
        """`dns.views` is keyed by view name, not by the schema."""
        stored = {
            "dns": {"views": {"cn": {"resolver": "r"}, "jp": {"resolver": "r2"}}},
            "sources": [{"key": "custom", "name": "custom"}],
        }
        kept, dropped = cfgmod.prune_dead(stored)
        self.assertEqual(dropped, [])
        self.assertEqual(sorted(kept["dns"]["views"]), ["cn", "jp"])
        self.assertEqual(kept["sources"], stored["sources"])

    def test_load_removes_a_dead_key_from_the_file(self):
        """The point of the prune: config.json converges, it does not accumulate."""
        tmp = Path(tempfile.mkdtemp())
        old_path, old_data, old_dropped = (cfgmod.CONFIG_PATH, cfgmod.DATA,
                                           cfgmod._last_dropped)
        cfgmod.CONFIG_PATH = tmp / "config.json"
        cfgmod.DATA = tmp
        try:
            cfgmod.CONFIG_PATH.write_text(json.dumps({
                "core": {"probe_group": "__PROBE__", "mixed_port": 19194,
                         "api": "http://127.0.0.1:1"},
            }), encoding="utf-8")
            cfg = cfgmod.load()
            self.assertEqual(cfgmod.take_dropped_keys(), ["core.probe_group"])
            # consume-once, asserted right here on purpose: the second call must
            # already be empty. Testing it after another load() proves nothing,
            # because that load overwrites the list with [] anyway -- the
            # property under test is that the *same* notice is not re-announced.
            # Without it every round re-reports a key the file no longer has,
            # since the server caches its config and load() runs rarely.
            self.assertEqual(cfgmod.take_dropped_keys(), [])
            self.assertNotIn("probe_group", cfg["core"])
            # the deployment override is still there, in the dict and on disk
            self.assertEqual(cfg["core"]["mixed_port"], 19194)
            on_disk = json.loads(cfgmod.CONFIG_PATH.read_text(encoding="utf-8"))
            self.assertNotIn("probe_group", on_disk["core"])
            self.assertEqual(on_disk["core"]["mixed_port"], 19194)
            # a later load has nothing left to report
            cfgmod.load()
            self.assertEqual(cfgmod.take_dropped_keys(), [])
        finally:
            cfgmod.CONFIG_PATH, cfgmod.DATA = old_path, old_data
            cfgmod._last_dropped[:] = old_dropped
            shutil.rmtree(tmp, ignore_errors=True)


class DisabledSourceTest(unittest.TestCase):
    """A disabled source's verdicts must stop being counted as current."""

    def test_disabled_rows_are_demoted_but_keep_their_history(self):
        """Regression for the 25 stale `alive` rows on vps (2026-09-20).

        A disabled source keeps its rows so re-enabling is cheap, and nothing
        prunes it. But its last verdict is no longer maintained, so leaving the
        rows as `alive` puts nodes into the headline count that nothing has
        tested for hours -- on vps that was 25 of 315 `alive`, frozen 21 hours
        earlier.
        """
        tmp = Path(tempfile.mkdtemp())
        old_path, old_conn = engine.db.DB_PATH, engine.db._conn
        engine.db.DB_PATH, engine.db._conn = tmp / "s.db", None
        try:
            engine.db.upsert_node("live", "f1", "a", status="alive", consec_fail=0,
                                  total_ok=9, total_fail=1)
            engine.db.upsert_node("off", "f2", "b", status="alive", consec_fail=2,
                                  total_ok=40, total_fail=3)
            engine.db.upsert_node("off", "f3", "c", status="dead", consec_fail=5,
                                  total_ok=1, total_fail=6)
            engine.db.upsert_node("off", "f4", "d", status="excluded",
                                  last_reason="entry_cn")
            demoted = engine.db.demote_disabled_sources(["live"])
            self.assertEqual(demoted, 2)
            self.assertEqual(engine.db.get_node("live", "f1")["status"], "alive")
            off_alive = engine.db.get_node("off", "f2")
            self.assertEqual(off_alive["status"], "unknown")
            # the history survives, so a re-enabled source restarts cheaply
            self.assertEqual(off_alive["total_ok"], 40)
            self.assertEqual(off_alive["consec_fail"], 2)
            self.assertEqual(engine.db.get_node("off", "f3")["status"], "unknown")
            # excluded is a classification, not a verdict
            self.assertEqual(engine.db.get_node("off", "f4")["status"], "excluded")
        finally:
            if engine.db._conn is not None:
                engine.db._conn.close()
            engine.db.DB_PATH, engine.db._conn = old_path, old_conn
            shutil.rmtree(tmp, ignore_errors=True)


class MutedSourceTest(unittest.TestCase):
    """A source can be tested and listed without being published.

    The case that motivated this: a free subscription that yields one working
    node out of seven, intermittently. Disabling it would stop measuring it and
    demote its ledger, but publishing it hands a single flaky node to every
    client. `export: false` is the third state -- watch it, do not ship it.
    """

    def cfg(self, **over):
        source = {"key": "flaky", "kind": "sub", "name": "flaky",
                  "enabled": True, "export": False}
        source.update(over)
        return {"sources": [source], "publish": {"enabled": True}}

    # --- config layer ---

    def test_export_defaults_to_true(self):
        out = cfgmod.normalize_sources([{"name": "air"}])
        self.assertTrue(out[0]["export"])

    def test_explicit_false_survives_normalisation(self):
        out = cfgmod.normalize_sources([{"name": "air", "export": False}])
        self.assertFalse(out[0]["export"])

    def test_enabled_and_export_are_independent(self):
        out = cfgmod.normalize_sources([{"name": "a", "enabled": True, "export": False},
                                        {"name": "b", "enabled": False, "export": True}])
        self.assertEqual([(s["enabled"], s["export"]) for s in out],
                         [(True, False), (False, True)])

    # --- engine.export_keys ---

    def test_export_keys_skips_muted_and_disabled(self):
        cfg = {"sources": [
            {"key": "on", "enabled": True, "export": True},
            {"key": "muted", "enabled": True, "export": False},
            {"key": "off", "enabled": False, "export": True},
        ]}
        self.assertEqual(engine.export_keys(cfg), ["on"])

    def test_export_keys_treats_a_missing_flag_as_publishing(self):
        cfg = {"sources": [{"key": "legacy", "enabled": True}]}
        self.assertEqual(engine.export_keys(cfg), ["legacy"])

    # --- cleanup ---

    def test_cleanup_removes_the_file_of_a_muted_source(self):
        """Without this the last file written before muting lingers forever."""
        tmp = Path(tempfile.mkdtemp())
        old = engine.EXPORT_DIR
        engine.EXPORT_DIR = tmp
        try:
            (tmp / "flaky.yaml").write_text("proxies: []\n", encoding="utf-8")
            (tmp / "flaky.meta.json").write_text('{"count": 3}', encoding="utf-8")
            removed = engine.cleanup_exports(self.cfg())
            self.assertEqual(removed, ["flaky"])
            self.assertFalse((tmp / "flaky.yaml").exists())
            self.assertFalse((tmp / "flaky.meta.json").exists())
        finally:
            engine.EXPORT_DIR = old
            shutil.rmtree(tmp, ignore_errors=True)

    def test_cleanup_keeps_the_file_of_an_exporting_source(self):
        tmp = Path(tempfile.mkdtemp())
        old = engine.EXPORT_DIR
        engine.EXPORT_DIR = tmp
        try:
            (tmp / "flaky.yaml").write_text("proxies: []\n", encoding="utf-8")
            self.assertEqual(engine.cleanup_exports(self.cfg(export=True)), [])
            self.assertTrue((tmp / "flaky.yaml").exists())
        finally:
            engine.EXPORT_DIR = old
            shutil.rmtree(tmp, ignore_errors=True)

    # --- publish ---

    def test_publish_writes_no_file_for_a_muted_source(self):
        tmp = Path(tempfile.mkdtemp())
        old = engine.EXPORT_DIR
        engine.EXPORT_DIR = tmp
        try:
            cfg = self.cfg()
            sources = cfg["sources"]
            wrote = engine._publish_sources(
                cfg, None, sources,
                {"flaky": [{"name": "n", "type": "vless", "server": "s", "port": 1}]},
                {"n": {"name": "n", "type": "vless", "server": "s", "port": 1}}, {}, lambda *a: None)
            self.assertTrue(wrote)
            self.assertFalse((tmp / "flaky.yaml").exists())
        finally:
            engine.EXPORT_DIR = old
            shutil.rmtree(tmp, ignore_errors=True)

    def test_publish_still_writes_the_file_when_export_is_on(self):
        tmp = Path(tempfile.mkdtemp())
        old = engine.EXPORT_DIR
        engine.EXPORT_DIR = tmp
        try:
            cfg = self.cfg(export=True)
            proxy = {"name": "n", "type": "vless", "server": "s", "port": 1}
            engine._publish_sources(cfg, None, cfg["sources"], {"flaky": ["n"]},
                                    {"n": proxy}, {}, lambda *a: None)
            self.assertTrue((tmp / "flaky.yaml").exists())
        finally:
            engine.EXPORT_DIR = old
            shutil.rmtree(tmp, ignore_errors=True)

    # --- push ---

    def test_publish_keys_excludes_a_muted_source(self):
        self.assertEqual(engine.publish_keys(self.cfg()), [])

    def test_publish_keys_includes_an_exporting_source(self):
        self.assertEqual(engine.publish_keys(self.cfg(export=True)), ["flaky"])

    def test_link_substore_skips_a_muted_source(self):
        """Sub-Store answers 500 for a sub that resolves to zero nodes.

        Reading the store is fine -- `link_substore` lists existing subs to
        prune the ones it created for sources that are no longer selected. What
        must not happen is an upsert for a muted source.
        """
        class Spy:
            def __init__(self):
                self.upserts, self.deletes = [], []

            def get_json(self, path):
                return []

            def upsert(self, kind, name, payload):
                self.upserts.append((kind, name))
                return "created"

            def _request(self, method, path, body=None):
                self.deletes.append((method, path))
                return {}

        spy = Spy()
        linked = engine.link_substore(
            {"sources": [{"key": "flaky", "kind": "sub", "name": "flaky",
                          "label": "flaky", "enabled": True, "export": False}],
             "publish": {"prefix": "probe", "hostname": "probe.example.com"}},
            spy)
        self.assertEqual(spy.upserts, [])
        # It still reports that nothing was linked -- accurately, since a muted
        # source contributes no member to the bundle. The wording matters: this
        # used to say 「暂无存活节点」 for every empty outcome, so a source the
        # operator had deliberately muted read as a source that was failing, and
        # the note sent them to look at the node table instead of the 导出 box.
        self.assertEqual(len(linked), 1)
        self.assertIn('没有启用且开启导出的来源', linked[0])


class TwinSourceKeyingTest(unittest.TestCase):
    """Two sources may share a kind and name; the dashboard must not conflate them.

    `链式聚合` is a collection named `air`, and the plain `air` entry it was
    derived from is also a collection named `air`. The panel keys configured
    sources by `kind|name`, matching Sub-Store's resource list, so a naive
    last-one-wins map let the *disabled* twin shadow the live one: the row
    rendered the wrong key, pointed its export link at the wrong file, and
    showed the disabled source as enabled.

    The fix is the precedence rule in `configuredMap`, asserted here against the
    served JavaScript because that is where the bug lived -- the Python side had
    no opinion about it.

    The script used to be a `<script>` block inside `ui.py`; it is now a real
    static asset (`mihomo_test/web/app.js`) so the same file can be deployed to a
    CDN. The assertions below are unchanged -- they were always about the JS.
    """

    UI = (Path(__file__).resolve().parent.parent
          / "mihomo_test" / "web" / "app.js")

    def _source(self):
        return self.UI.read_text(encoding="utf-8")

    def test_configured_map_prefers_the_enabled_twin(self):
        src = self._source()
        start = src.index("function configuredMap()")
        end = src.index("function renderSources()")
        body = src[start:end]
        self.assertIn("!cur.enabled && s.enabled", body,
                      "configuredMap must let an enabled entry take the slot "
                      "from a disabled twin sharing its kind|name")
        # A blind assignment is exactly the bug; it must not be the whole logic.
        self.assertNotIn("forEach(s => map[s.kind + \"|\" + s.name] = s)", body)

    def test_configured_map_still_falls_back_to_the_first_entry(self):
        src = self._source()
        start = src.index("function configuredMap()")
        end = src.index("function renderSources()")
        body = src[start:end]
        self.assertIn("if (!cur ||", body,
                      "an unconfigured key must still take the first entry, "
                      "otherwise a disabled source alone would vanish")

    def test_both_twins_reach_the_dom(self):
        """The main loop walks Sub-Store's list; the twin must land in `missing`.

        One `collection|air` in the available list means that loop can only
        produce one row, and `configuredMap` gives it to the enabled twin. The
        other source must still get a row or it becomes unmanageable from the
        panel -- which is exactly how `air` came to look like dead weight rather
        than a deliberate entry.

        The old test was `!RESOURCES.some(kind && name)`, which is false for a
        shadowed twin (it *is* in the list), so the row vanished silently.
        """
        src = self._source()
        start = src.index("const rendered = new Set(")
        body = src[start:start + 700]
        self.assertIn("!rendered.has(s.key)", body,
                      "membership must be decided by what the loop actually "
                      "consumed, not by whether the kind|name exists upstream")

    def test_shadowed_twin_is_labelled_accurately(self):
        """It is in the list; only its row was taken. Say so."""
        src = self._source()
        # 锚到 missing.forEach 开头而不是固定窗口:shadowed 标签是回调的头
        # 两条语句,中间隔着一整段主循环注释,固定长度窗口被挤出过一次。
        start = src.index("missing.forEach(s => {")
        body = src[start:start + 600]
        self.assertIn("与同名的启用来源共用资源", body)
        self.assertIn("const shadowed = RESOURCES.some(", body)


class DomainViewCacheTest(unittest.TestCase):
    """The DNS-view cache gates entry classification, so its TTL must bite."""

    def _stamp(self, offset_s):
        return time.strftime("%Y-%m-%dT%H:%M:%S",
                             time.gmtime(time.time() + offset_s))

    def test_only_genuinely_fresh_entries_are_reused(self):
        from mihomo_test import db as dbmod
        views = {"cn": ["1.2.3.4"], "overseas": ["5.6.7.8"]}
        dbmod.domain_views_put("fresh.example", views)
        dbmod.domain_views_put("old.example", views)
        dbmod.domain_views_put("future.example", views)
        dbmod.execute("UPDATE domain_views SET checked_at=? WHERE domain=?",
                      (self._stamp(-8 * 3600), "old.example"))
        # a build that stored local time leaves stamps ahead of UTC; that is
        # the shape of the rows this guard exists for
        dbmod.execute("UPDATE domain_views SET checked_at=? WHERE domain=?",
                      (self._stamp(8 * 3600), "future.example"))

        self.assertEqual(dbmod.domain_views_get("fresh.example", 6 * 3600), views)
        self.assertIsNone(dbmod.domain_views_get("old.example", 6 * 3600))
        self.assertIsNone(dbmod.domain_views_get("future.example", 6 * 3600))


class MigrationTest(unittest.TestCase):
    """A database from the name-keyed build must rebuild, not crash."""

    def test_legacy_schema_is_rebuilt_on_connect(self):
        import sqlite3
        import tempfile
        from pathlib import Path

        from mihomo_test import db as dbmod

        handle = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
        handle.close()
        legacy = sqlite3.connect(handle.name)
        legacy.executescript(
            "CREATE TABLE nodes (source TEXT, name TEXT, status TEXT,"
            " consec_fail INTEGER, PRIMARY KEY(source, name));"
            "CREATE TABLE results (round_id INTEGER, source TEXT, name TEXT);"
            "CREATE INDEX idx_results_name ON results(source, name);"
        )
        legacy.commit()
        legacy.close()

        old_path, old_conn = dbmod.DB_PATH, dbmod._conn
        try:
            dbmod.DB_PATH = Path(handle.name)
            dbmod._conn = None
            dbmod.connect()
            columns = {row["name"] for row in dbmod.query("PRAGMA table_info(nodes)")}
            self.assertIn("fingerprint", columns)
            self.assertIn("display", columns)
            # the rebuilt schema must be usable
            dbmod.upsert_node("s", "fp1", "a node")
            self.assertEqual(len(dbmod.list_nodes()), 1)
        finally:
            if dbmod._conn is not None:
                dbmod._conn.close()
            dbmod.DB_PATH, dbmod._conn = old_path, old_conn
            _drop_temp_db(handle.name)

    def test_duplicate_names_are_tracked_separately(self):
        import tempfile
        from pathlib import Path

        from mihomo_test import db as dbmod

        old_path, old_conn = dbmod.DB_PATH, dbmod._conn

        handle = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
        handle.close()
        try:
            dbmod.DB_PATH = Path(handle.name)
            dbmod._conn = None
            dbmod.upsert_node("air", "fp-a", "BageVM", status="alive", consec_fail=0)
            dbmod.upsert_node("air", "fp-b", "BageVM", status="unknown", consec_fail=0)
            nodes = dbmod.list_nodes()
            self.assertEqual(len(nodes), 2)
            self.assertEqual({n["name"] for n in nodes}, {"BageVM"})
        finally:
            if dbmod._conn is not None:
                dbmod._conn.close()
            dbmod.DB_PATH, dbmod._conn = old_path, old_conn
            _drop_temp_db(handle.name)

    def test_rebuild_backs_the_ledger_up_first(self):
        """The rebuild drops tables; the streaks it destroys are unrecoverable."""
        import sqlite3
        import tempfile
        from pathlib import Path

        from mihomo_test import db as dbmod

        workdir = Path(tempfile.mkdtemp())
        ledger = workdir / "state.db"
        legacy = sqlite3.connect(ledger)
        legacy.executescript(
            "CREATE TABLE nodes (source TEXT, name TEXT, status TEXT,"
            " consec_fail INTEGER, PRIMARY KEY(source, name));"
            "INSERT INTO nodes VALUES('air','old-node','alive',2);"
        )
        legacy.commit()
        legacy.close()

        old_path, old_conn = dbmod.DB_PATH, dbmod._conn
        try:
            dbmod.DB_PATH = ledger
            dbmod._conn = None
            dbmod.connect()
            backups = list(workdir.glob("state.db.bak-*"))
            self.assertEqual(len(backups), 1, "no pre-migration backup was taken")
            # the copy must hold the pre-migration data, not the rebuilt schema
            saved = sqlite3.connect(backups[0])
            names = {row[0] for row in saved.execute(
                "SELECT name FROM sqlite_master WHERE type='table'")}
            saved.close()
            self.assertIn("nodes", names)
            restored = sqlite3.connect(backups[0])
            self.assertEqual(
                restored.execute("SELECT name FROM nodes").fetchall(), [("old-node",)])
            restored.close()
        finally:
            if dbmod._conn is not None:
                dbmod._conn.close()
            dbmod.DB_PATH, dbmod._conn = old_path, old_conn
            shutil.rmtree(workdir, ignore_errors=True)


    def test_current_schema_needs_no_backup(self):
        """A DB already keyed by fingerprint is upgraded in place, not dropped."""
        import tempfile
        from pathlib import Path

        from mihomo_test import db as dbmod

        workdir = Path(tempfile.mkdtemp())
        ledger = workdir / "state.db"

        old_path, old_conn = dbmod.DB_PATH, dbmod._conn
        try:
            dbmod.DB_PATH = ledger
            dbmod._conn = None
            dbmod.upsert_node("air", "fp-a", "keepme", status="alive")
            if dbmod._conn is not None:
                dbmod._conn.close()
            dbmod._conn = None
            dbmod.connect()
            self.assertEqual([n["name"] for n in dbmod.list_nodes()], ["keepme"])
            self.assertEqual(list(workdir.glob("state.db.bak-*")), [])
        finally:
            if dbmod._conn is not None:
                dbmod._conn.close()
            dbmod.DB_PATH, dbmod._conn = old_path, old_conn
            shutil.rmtree(workdir, ignore_errors=True)




class CategoryStatsTest(unittest.TestCase):
    """`stats_by_category` feeds the dashboard's 分类统计 panel.

    The panel builds each of its three cards from the same code path, so all
    three buckets must expose an identical key set. An empty bucket is the
    *normal* state on a fresh install -- relay stays empty until a relay source
    is configured and chain only fills once a chain block exists -- so the empty
    shape is the one the panel meets first. A missing key there does not fail
    loudly: it renders "undefined", or poisons arithmetic with NaN.
    """

    def setUp(self):
        import tempfile
        from pathlib import Path

        from mihomo_test import db as dbmod

        self.dbmod = dbmod
        self.tmp = Path(tempfile.mkdtemp(prefix="cat-stats-"))
        self._saved = (dbmod.DB_PATH, dbmod._conn)
        dbmod.DB_PATH = self.tmp / "state.db"
        dbmod._conn = None

    def tearDown(self):
        if self.dbmod._conn is not None:
            self.dbmod._conn.close()
        self.dbmod.DB_PATH, self.dbmod._conn = self._saved
        # `mkdtemp` is called with a prefix but no `dir=`, so this lands in the
        # system temp dir, not the `_isolation` root -- nothing else will ever
        # remove it. Unremoved, every run leaks one directory per test method:
        # 30 of them had accumulated in the container's /tmp.
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _record(self, db, round_id, source, fp, display, verdict, delay_ms=None,
                category=None, reason=None, country=None):
        """`record_result` takes ten positional args; name the boring ones once."""
        db.record_result(round_id, source, fp, display, verdict, delay_ms,
                         reason, country, 1, None, category)

    def test_empty_buckets_expose_the_full_key_set(self):
        db = self.dbmod
        out = db.stats_by_category()
        self.assertEqual(set(out["categories"]), {"direct", "relay", "chain"})
        expected_nodes = {"total", "alive", "dead", "pending", "unknown", "excluded"}
        expected_tested = {"total", "ok", "fail", "skipped"}
        expected_delay = {"median", "avg", "min", "max"}
        for cat, bucket in out["categories"].items():
            self.assertEqual(set(bucket["nodes"]), expected_nodes, cat)
            self.assertEqual(set(bucket["tested"]), expected_tested, cat)
            self.assertEqual(set(bucket["delay_ms"]), expected_delay, cat)
            self.assertEqual(bucket["reasons"], [], cat)
            self.assertEqual(bucket["top_countries"], [], cat)

    def test_populated_bucket_keeps_the_same_key_set(self):
        """A populated bucket must not *add* keys the empty one lacks."""
        db = self.dbmod
        db.upsert_node("air", "fp-a", "A", status="alive", category="direct")
        self._record(db, 1, "air", "fp-a", "A", "ok", 120, "direct")

        out = db.stats_by_category()
        empty_shape = db.stats_by_category(round_id=999)["categories"]["relay"]
        full = out["categories"]["direct"]
        self.assertEqual(set(full["nodes"]), set(empty_shape["nodes"]))
        self.assertEqual(set(full["tested"]), set(empty_shape["tested"]))
        self.assertEqual(set(full["delay_ms"]), set(empty_shape["delay_ms"]))
        self.assertEqual(full["tested"]["ok"], 1)
        self.assertEqual(full["tested"]["skipped"], 0)

    def test_verdict_split_sums_to_total(self):
        """ok+fail+skipped == total, so the panel's stacked bar never overflows."""
        db = self.dbmod
        for i, verdict in enumerate(("ok", "ok", "fail", "excluded")):
            db.upsert_node("air", f"fp-{i}", f"N{i}", category="chain")
            self._record(db, 1, "air", f"fp-{i}", f"N{i}", verdict,
                         100 if verdict == "ok" else None, "chain")
        tested = db.stats_by_category()["categories"]["chain"]["tested"]
        self.assertEqual(tested["total"], 4)
        self.assertEqual(tested["ok"], 2)
        self.assertEqual(tested["fail"], 1)
        self.assertEqual(tested["skipped"], 1)
        self.assertEqual(tested["ok"] + tested["fail"] + tested["skipped"], tested["total"])

    def test_categories_are_kept_apart(self):
        """The whole point: 直连 must not absorb 中转 or 链式."""
        db = self.dbmod
        for cat in ("direct", "relay", "chain"):
            db.upsert_node(cat, f"fp-{cat}", cat, status="alive", category=cat)
            self._record(db, 1, cat, f"fp-{cat}", cat, "ok", 50, cat)
        cats = db.stats_by_category()["categories"]
        for cat in ("direct", "relay", "chain"):
            self.assertEqual(cats[cat]["nodes"]["total"], 1, cat)
            self.assertEqual(cats[cat]["tested"]["total"], 1, cat)
            self.assertEqual(cats[cat]["nodes"]["alive"], 1, cat)

    def test_legacy_rows_without_category_read_as_direct(self):
        """Pre-migration rows are NULL and were in fact tested as direct."""
        db = self.dbmod
        db.upsert_node("air", "fp-old", "Old", status="alive")  # no category
        self._record(db, 1, "air", "fp-old", "Old", "ok", 80)   # no category
        cats = db.stats_by_category()["categories"]
        self.assertEqual(cats["direct"]["nodes"]["total"], 1)
        self.assertEqual(cats["direct"]["tested"]["total"], 1)
        self.assertEqual(cats["relay"]["nodes"]["total"], 0)

    def test_delay_stats_ignore_failures(self):
        """A failed attempt stores a timeout constant, not a measurement."""
        db = self.dbmod
        db.upsert_node("air", "fp-ok", "OK", status="alive", category="direct")
        db.upsert_node("air", "fp-bad", "Bad", status="dead", category="direct")
        self._record(db, 1, "air", "fp-ok", "OK", "ok", 100, "direct")
        self._record(db, 1, "air", "fp-bad", "Bad", "fail", 9999, "direct")
        delay = db.stats_by_category()["categories"]["direct"]["delay_ms"]
        self.assertEqual(delay["max"], 100, "a failure's delay leaked into the stats")
        self.assertEqual(delay["median"], 100)

    def test_round_id_selects_that_round_only(self):
        db = self.dbmod
        db.upsert_node("air", "fp-a", "A", status="alive", category="direct")
        self._record(db, 1, "air", "fp-a", "A", "ok", 100, "direct")
        self._record(db, 2, "air", "fp-a", "A", "ok", 100, "direct")
        self._record(db, 2, "air", "fp-a", "A", "fail", None, "direct")
        self.assertEqual(db.stats_by_category(2)["categories"]["direct"]["tested"]["total"], 2)
        self.assertEqual(db.stats_by_category(1)["categories"]["direct"]["tested"]["total"], 1)

    def test_defaults_to_newest_round_with_results(self):
        """A round in flight has a `rounds` row but no results; do not blank."""
        db = self.dbmod
        db.upsert_node("air", "fp-a", "A", status="alive", category="direct")
        self._record(db, 1, "air", "fp-a", "A", "ok", 100, "direct")
        db.start_round("test")  # in flight, no results yet
        self.assertEqual(db.stats_by_category()["round_id"], 1)

    def test_no_results_at_all_is_not_an_error(self):
        out = self.dbmod.stats_by_category()
        self.assertIsNone(out["round_id"])
        self.assertEqual(len(out["categories"]), 3)

    def test_list_nodes_exposes_the_category(self):
        """The node table reads `n.category`, and a missing key is not loud.

        `renderNodes` falls back to `direct` when the field is absent, so
        dropping it from the SELECT does not blank anything -- it relabels every
        中转 and 链式 node as 直连 in the table, the filter and the badge, while
        `/api/stats` (a direct column query) keeps showing the correct split.
        The two would disagree with no error anywhere.
        """
        db = self.dbmod
        db.upsert_node("air", "fp-r", "R", status="alive", category="relay")
        db.upsert_node("air", "fp-c", "C", status="alive", category="chain")
        by_fp = {n["fingerprint"]: n for n in db.list_nodes()}
        self.assertEqual(by_fp["fp-r"]["category"], "relay")
        self.assertEqual(by_fp["fp-c"]["category"], "chain")


class DeselectTest(unittest.TestCase):
    """Deselecting a source must not leave its nodes stuck on the dashboard."""

    def _temp_db(self):
        import tempfile
        from pathlib import Path

        from mihomo_test import db as dbmod

        handle = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
        handle.close()
        return dbmod, Path(handle.name)

    def test_deselected_source_records_are_removed(self):
        dbmod, path = self._temp_db()
        old_path, old_conn = dbmod.DB_PATH, dbmod._conn
        try:
            dbmod.DB_PATH, dbmod._conn = path, None
            dbmod.upsert_node("air", "f1", "n1", status="alive")
            dbmod.upsert_node("cn", "f2", "n2", status="unknown")
            dbmod.upsert_node("cn", "f3", "n3", status="unknown")
            self.assertEqual(dbmod.delete_sources_not_in(["air"]), 2)
            self.assertEqual([n["source"] for n in dbmod.list_nodes()], ["air"])
        finally:
            if dbmod._conn is not None:
                dbmod._conn.close()
            dbmod.DB_PATH, dbmod._conn = old_path, old_conn
            _drop_temp_db(path)

    def test_empty_key_list_never_wipes_the_table(self):
        dbmod, path = self._temp_db()
        old_path, old_conn = dbmod.DB_PATH, dbmod._conn
        try:
            dbmod.DB_PATH, dbmod._conn = path, None
            dbmod.upsert_node("air", "f1", "n1", status="alive")
            self.assertEqual(dbmod.delete_sources_not_in([]), 0)
            self.assertEqual(len(dbmod.list_nodes()), 1)
        finally:
            if dbmod._conn is not None:
                dbmod._conn.close()
            dbmod.DB_PATH, dbmod._conn = old_path, old_conn
            _drop_temp_db(path)

    def test_disabled_but_configured_source_keeps_its_history(self):
        """Re-enabling a source should not start its convergence from scratch."""
        dbmod, path = self._temp_db()
        old_path, old_conn = dbmod.DB_PATH, dbmod._conn
        try:
            dbmod.DB_PATH, dbmod._conn = path, None
            dbmod.upsert_node("air", "f1", "n1", status="alive")
            dbmod.upsert_node("cn", "f2", "n2", status="alive")
            # configured list includes the disabled one, so nothing is dropped
            self.assertEqual(dbmod.delete_sources_not_in(["air", "cn"]), 0)
            self.assertEqual(len(dbmod.list_nodes()), 2)
        finally:
            if dbmod._conn is not None:
                dbmod._conn.close()
            dbmod.DB_PATH, dbmod._conn = old_path, old_conn
            _drop_temp_db(path)


class SourceKeyTest(unittest.TestCase):
    """Keys become file names and URL segments, so they need real validation."""

    def test_path_separators_are_stripped_when_deriving(self):
        self.assertEqual(cfgmod.safe_key("air/../etc"), "air..etc")
        self.assertEqual(cfgmod.safe_key("a b"), "a-b")
        self.assertEqual(cfgmod.safe_key("...hidden"), "hidden")

    def test_unicode_names_survive(self):
        # a Chinese collection name is still a perfectly usable key
        self.assertEqual(cfgmod.safe_key("中国地区"), "中国地区")

    def test_empty_name_falls_back(self):
        self.assertEqual(cfgmod.safe_key("", fallback="src"), "src")
        self.assertEqual(cfgmod.safe_key("...", fallback="src"), "src")

    def test_validate_rejects_traversal_shapes(self):
        for bad in ("..", ".", "../x", "a/b", "a\\b", ".hidden", "a:b", "a?b", ""):
            with self.assertRaises(ValueError, msg=f"should reject {bad!r}"):
                cfgmod.validate_key(bad)

    def test_validate_accepts_normal_and_unicode_keys(self):
        for good in ("air", "my-sub", "中国地区", "a_b.c", "x" * 48):
            self.assertEqual(cfgmod.validate_key(good), good)


class NormalizeSourcesTest(unittest.TestCase):
    def test_kind_defaults_and_enabled_defaults(self):
        out = cfgmod.normalize_sources([{"name": "air"}])
        self.assertEqual(out[0]["kind"], "collection")
        self.assertTrue(out[0]["enabled"])
        self.assertEqual(out[0]["key"], "air")

    def test_explicit_false_stays_disabled(self):
        out = cfgmod.normalize_sources([{"name": "air", "enabled": False}])
        self.assertFalse(out[0]["enabled"])

    def test_duplicate_keys_are_made_unique(self):
        out = cfgmod.normalize_sources([{"name": "a", "key": "same"}, {"name": "b", "key": "same"}])
        self.assertEqual([s["key"] for s in out], ["same", "same-2"])

    def test_missing_key_is_derived_from_the_name(self):
        out = cfgmod.normalize_sources([{"name": "中国地区"}])
        self.assertEqual(out[0]["key"], "中国地区")

    def test_bad_explicit_key_is_reported_not_silently_mangled(self):
        with self.assertRaises(ValueError):
            cfgmod.normalize_sources([{"name": "a", "key": "../escape"}])

    def test_entries_without_a_name_are_dropped(self):
        self.assertEqual(cfgmod.normalize_sources([{"key": "x"}, "nonsense", None]), [])

    def test_unknown_kind_falls_back_to_collection(self):
        out = cfgmod.normalize_sources([{"name": "a", "kind": "file"}])
        self.assertEqual(out[0]["kind"], "collection")

    def test_relay_defaults_to_false(self):
        out = cfgmod.normalize_sources([{"name": "air"}])
        self.assertFalse(out[0]["relay"])

    def test_relay_true_survives_normalize(self):
        out = cfgmod.normalize_sources([{"name": "air", "relay": True}])
        self.assertTrue(out[0]["relay"])

    def test_relay_requires_a_real_boolean(self):
        # `is True` on purpose: a hand-edited "yes" or 1 must read as "not
        # marked", because a truthy coercion here would classify an entire
        # source as transit and silently skew every category number.
        for value in ("yes", 1, "true", [], {}):
            out = cfgmod.normalize_sources([{"name": "air", "relay": value}])
            self.assertFalse(out[0]["relay"], f"relay={value!r} should not count as True")


class SourceFieldPersistenceTest(unittest.TestCase):
    """Source fields must survive a load/save round trip, not just normalize.

    `normalize_sources` rebuilds each entry from a whitelist, so a field it
    does not name is dropped on every load *and* every update. That failure is
    invisible: the panel POSTs, the config saves, and the setting is gone by the
    next boot -- with no test failing, because nothing asserted the field's
    presence. These tests exist so adding a source field without whitelisting it
    fails loudly instead.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="src-field-"))
        self._saved = (cfgmod.DATA, cfgmod.CONFIG_PATH)
        cfgmod.DATA = self.tmp
        cfgmod.CONFIG_PATH = self.tmp / "config.json"

    def tearDown(self):
        cfgmod.DATA, cfgmod.CONFIG_PATH = self._saved
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_relay_survives_save_and_reload(self):
        cfg = cfgmod.load()
        cfg["sources"] = [{"key": "cdn", "kind": "sub", "name": "CM-CF",
                           "label": "CDN", "enabled": True, "export": True,
                           "relay": True}]
        cfgmod.save(cfg)
        again = cfgmod.load()
        self.assertTrue(again["sources"][0]["relay"],
                        "relay was dropped by the load() round trip")

    def test_relay_survives_update_patch(self):
        cfgmod.load()
        cfgmod.update({"sources": [{"key": "cdn", "kind": "sub", "name": "CM-CF",
                                    "label": "CDN", "enabled": True,
                                    "export": True, "relay": True}]})
        self.assertTrue(cfgmod.load()["sources"][0]["relay"],
                        "relay was dropped by update()")

    def test_relay_round_trips_through_validate_patch(self):
        clean, notes = cfgmod.validate_patch(
            {"sources": [{"key": "cdn", "kind": "sub", "name": "CM-CF",
                          "relay": True}]})
        self.assertTrue(clean["sources"][0]["relay"])
        self.assertFalse(notes)


class ExportPathTest(unittest.TestCase):
    def test_traversal_key_cannot_read_outside_the_export_dir(self):
        self.assertIsNone(engine.read_export("../../etc/passwd"))
        self.assertIsNone(engine.read_export(".."))
        self.assertIsNone(engine.export_meta("../../etc/passwd"))

    def test_normal_key_reads_its_file(self):
        import shutil
        import tempfile
        from pathlib import Path

        tmp = Path(tempfile.mkdtemp())
        old = engine.EXPORT_DIR
        try:
            engine.EXPORT_DIR = tmp
            (tmp / "air.yaml").write_text("proxies: []\n", encoding="utf-8")
            self.assertEqual(engine.read_export("air"), "proxies: []\n")
            self.assertIsNone(engine.read_export("missing"))
        finally:
            engine.EXPORT_DIR = old
            shutil.rmtree(tmp, ignore_errors=True)


class SourceFetchTest(unittest.TestCase):
    """Single subs must be fetched via the download route, not their content."""

    class Stub(Client):
        def __init__(self):
            super().__init__("http://stub", retries=0)
            self.paths = []

        def _request(self, method, path, payload=None, raw=False):
            self.paths.append(path)
            return 200, "proxies:\n  - {name: n, type: vless, server: s, port: 1}\n"

    def test_collection_uses_the_collection_route(self):
        stub = self.Stub()
        proxies = stub.fetch_source("collection", "air")
        self.assertEqual(len(proxies), 1)
        self.assertTrue(stub.paths[0].startswith("/download/collection/air?"))

    def test_sub_uses_the_plain_download_route(self):
        stub = self.Stub()
        stub.fetch_source("sub", "alphasub")
        self.assertTrue(stub.paths[0].startswith("/download/alphasub?"))

    def test_missing_proxies_list_is_an_error(self):
        class Empty(self.Stub):
            def _request(self, method, path, payload=None, raw=False):
                return 200, "port: 7890\n"

        with self.assertRaises(Exception):
            Empty().fetch_source("collection", "x")


class ListResourcesTest(unittest.TestCase):
    class Stub(Client):
        def __init__(self):
            super().__init__("http://stub", retries=0)

        def get_json(self, path):
            if path == "/api/collections":
                return [{"name": "air", "subscriptions": ["a", "b"]}]
            if path == "/api/subs":
                return [{"name": "alphasub", "source": "remote"}]
            return []

    def test_both_kinds_are_listed_with_member_counts(self):
        available, errors = self.Stub().list_resources()
        self.assertEqual(errors, [])
        by_name = {r["name"]: r for r in available}
        self.assertEqual(by_name["air"]["kind"], "collection")
        self.assertEqual(by_name["air"]["members"], 2)
        self.assertEqual(by_name["alphasub"]["kind"], "sub")
        self.assertIsNone(by_name["alphasub"]["members"])


class LinkSubstoreTest(unittest.TestCase):
    HOST = "probe.example.com"
    # A key with a fixture export holding one node.
    KEYS = ("air", "my-key", "中国地区", "cn")

    def setUp(self):
        """Isolate the export directory so these tests do not depend on the host."""
        import json as _json
        import shutil
        import tempfile
        from pathlib import Path

        self.tmp = Path(tempfile.mkdtemp())
        self._old = engine.EXPORT_DIR
        engine.EXPORT_DIR = self.tmp
        for key in self.KEYS:
            (self.tmp / f"{key}.yaml").write_text(
                "proxies:\n  - {name: n, type: vless, server: s, port: 1}\n", encoding="utf-8")
            (self.tmp / f"{key}.meta.json").write_text(
                _json.dumps({"count": 1}), encoding="utf-8")
        # a key whose export has zero survivors
        (self.tmp / "zero.yaml").write_text("proxies: []\n", encoding="utf-8")
        (self.tmp / "zero.meta.json").write_text(_json.dumps({"count": 0}), encoding="utf-8")
        self._shutil = shutil

    def tearDown(self):
        engine.EXPORT_DIR = self._old
        self._shutil.rmtree(self.tmp, ignore_errors=True)

    class FakeStore:
        def __init__(self, subs=None):
            self.subs = subs or []
            self.upserts = []
            self.deleted = []

        def upsert(self, kind, name, payload):
            self.upserts.append((kind, name, payload))
            return "created"

        def get_json(self, path):
            return self.subs if path == "/api/subs" else []

        def _request(self, method, path, payload=None):
            self.deleted.append(path)
            return 200, "{}"

    def cfg(self, sources):
        return {
            "sources": sources,
            "auth": {"token": "tok"},
            "publish": {"prefix": "probe", "hostname": self.HOST},
        }

    def test_only_enabled_sources_are_linked(self):
        store = self.FakeStore()
        cfg = self.cfg([
            {"key": "air", "kind": "collection", "name": "air", "enabled": True},
            {"key": "cn", "kind": "sub", "name": "cn", "enabled": False},
        ])
        engine.link_substore(cfg, store, log=_nolog)
        names = [n for _k, n, _p in store.upserts]
        self.assertIn("probe-air", names)
        self.assertNotIn("probe-cn", names)

    def test_remote_sub_points_back_at_the_exporter(self):
        store = self.FakeStore()
        engine.link_substore(self.cfg([{"key": "air", "name": "air", "enabled": True}]), store, log=_nolog)
        _kind, _name, payload = store.upserts[0]
        self.assertEqual(payload["source"], "remote")
        self.assertIn(f"{self.HOST}/api/export/air.yaml?token=tok", payload["url"])

    def test_collection_lists_every_linked_sub(self):
        store = self.FakeStore()
        cfg = self.cfg([
            {"key": "air", "name": "air", "enabled": True},
            {"key": "cn", "name": "中国地区", "enabled": True},
        ])
        engine.link_substore(cfg, store, log=_nolog)
        collection = [p for k, _n, p in store.upserts if k == "collection"][0]
        self.assertEqual(collection["subscriptions"], ["probe-air", "probe-cn"])

    def test_explicit_key_wins_over_the_resource_name(self):
        store = self.FakeStore()
        engine.link_substore(self.cfg([{"key": "my-key", "name": "中国地区", "enabled": True}]), store, log=_nolog)
        self.assertEqual(store.upserts[0][1], "probe-my-key")
        self.assertIn("/api/export/my-key.yaml", store.upserts[0][2]["url"])

    def test_unicode_key_is_url_encoded_in_the_link(self):
        store = self.FakeStore()
        engine.link_substore(self.cfg([{"key": "中国地区", "name": "中国地区", "enabled": True}]), store, log=_nolog)
        self.assertEqual(store.upserts[0][1], "probe-中国地区")
        self.assertIn("export/%E4%B8%AD%E5%9B%BD%E5%9C%B0%E5%8C%BA.yaml", store.upserts[0][2]["url"])

    def test_prune_touches_only_our_own_remote_subs(self):
        store = self.FakeStore(subs=[
            {"name": "probe-gone", "source": "remote",
             "url": f"https://{self.HOST}/api/export/gone.yaml?token=tok"},
            {"name": "probe-air-local", "source": "local", "url": ""},
            {"name": "someone-elses", "source": "remote", "url": "https://other.example/x"},
            {"name": "probe-air", "source": "remote",
             "url": f"https://{self.HOST}/api/export/air.yaml?token=tok"},
        ])
        engine.link_substore(self.cfg([{"key": "air", "name": "air", "enabled": True}]), store, log=_nolog)
        self.assertEqual(len(store.deleted), 1)
        self.assertIn("probe-gone", store.deleted[0])
        for untouched in ("probe-air-local", "someone-elses"):
            self.assertFalse(any(untouched in d for d in store.deleted), untouched)

    def test_missing_hostname_is_reported(self):
        store = self.FakeStore()
        cfg = self.cfg([{"key": "air", "name": "air", "enabled": True}])
        cfg["publish"]["hostname"] = ""
        messages = engine.link_substore(cfg, store, log=_nolog)
        self.assertTrue(any("hostname" in m for m in messages))
        self.assertEqual(store.upserts, [])

    def test_source_without_a_key_is_skipped_with_a_message(self):
        store = self.FakeStore()
        messages = engine.link_substore(self.cfg([{"key": "", "name": "air", "enabled": True}]), store, log=_nolog)
        self.assertEqual(store.upserts, [])
        self.assertTrue(any("缺少 key" in m for m in messages))

    def test_source_with_no_export_file_is_not_linked(self):
        store = self.FakeStore()
        messages = engine.link_substore(
            self.cfg([{"key": "never-tested", "name": "never-tested", "enabled": True}]), store, log=_nolog)
        self.assertEqual(store.upserts, [])
        self.assertTrue(any("暂无存活节点" in m for m in messages))

    def test_zero_survivor_export_is_not_linked(self):
        """Sub-Store answers 500 for a zero-node subscription, so never link one."""
        store = self.FakeStore()
        messages = engine.link_substore(
            self.cfg([{"key": "zero", "name": "zero", "enabled": True}]), store, log=_nolog)
        self.assertEqual(store.upserts, [])
        self.assertTrue(any("暂无存活节点" in m for m in messages))

    def test_all_sources_empty_reports_and_leaves_the_collection_alone(self):
        store = self.FakeStore()
        messages = engine.link_substore(
            self.cfg([{"key": "zero", "name": "zero", "enabled": True}]), store, log=_nolog)
        self.assertTrue(any("聚合集合" in m for m in messages))
        self.assertFalse([u for u in store.upserts if u[0] == "collection"])

    def test_a_failed_upsert_never_deletes_the_healthy_object(self):
        """A 5xx is a retry, not a retirement.

        The prune used to read `members` -- the writes that *succeeded* -- so a
        single transient failure left the object out of the membership and the
        prune then DELETEd a subscription that was perfectly healthy. The only
        thing that may remove a link object is the source leaving the config.
        """
        class FlakyStore(self.FakeStore):
            def __init__(self, subs, fail_for):
                super().__init__(subs=subs)
                self.fail_for = fail_for

            def upsert(self, kind, name, payload):
                if name == self.fail_for:
                    raise StoreError("502 Bad Gateway")
                return super().upsert(kind, name, payload)

        store = FlakyStore(
            subs=[{"name": "probe-air", "source": "remote",
                   "url": f"https://{self.HOST}/api/export/air.yaml?token=tok"}],
            fail_for="probe-air")
        messages = engine.link_substore(
            self.cfg([{"key": "air", "name": "air", "enabled": True}]),
            store, log=_nolog)
        self.assertEqual(store.deleted, [], "一次可恢复的失败删掉了健康订阅")
        self.assertTrue(any("失败" in m for m in messages), messages)
        # And the bundle still names it: the object exists, and the name and
        # URL are deterministic, so it is exactly the right member.
        collection = [p for k, _n, p in store.upserts if k == "collection"][0]
        self.assertEqual(collection["subscriptions"], ["probe-air"])

    def test_the_collection_is_emptied_as_its_members_are_pruned(self):
        """A collection naming deleted subs is the HTTP 500 we exist to avoid.

        With every source gone (disabled, muted, or nothing alive) `expected`
        was empty, so the prune deleted every `prefix-*` remote sub -- while
        the `members`-keyed branch refused to touch the collection precisely
        because `members` was empty, leaving it pointing at what it had just
        deleted underneath it.
        """
        store = self.FakeStore(subs=[
            {"name": "probe-zero", "source": "remote",
             "url": f"https://{self.HOST}/api/export/zero.yaml?token=tok"},
            {"name": "probe-air-local", "source": "local", "url": ""}])
        engine.link_substore(
            self.cfg([{"key": "zero", "name": "zero", "enabled": True}]),
            store, log=_nolog)
        self.assertEqual(store.deleted, ["/api/sub/probe-zero"])
        collection = [p for k, _n, p in store.upserts if k == "collection"][0]
        self.assertEqual(collection["subscriptions"], [])

    def test_link_signature_ignores_the_measurement_switches(self):
        """`relay`/`direct`/`chain` never reach `link_substore`.

        These are the toggles the sources table flips most often, and every
        one of them used to pay for a full re-link whose outcome was
        byte-identical: the sync reads key/kind/name/label/enabled/export and
        the publish prefix/hostname, nothing else.
        """
        base = {"publish": {"prefix": "probe", "hostname": self.HOST},
                "sources": [{"key": "air", "kind": "collection", "name": "air",
                             "enabled": True, "export": True,
                             "direct": True, "chain": True, "relay": False}]}
        flipped = json.loads(json.dumps(base))
        flipped["sources"][0].update(direct=False, chain=False, relay=True)
        self.assertEqual(engine.link_signature(base), engine.link_signature(flipped))

    def test_link_signature_moves_with_every_field_the_link_reads(self):
        # One case per field `link_substore` actually reads: each must change
        # the signature, or a save touching only that field would silently
        # skip the sync and leave Sub-Store pointing at the old shape.
        base = {"publish": {"prefix": "probe", "hostname": self.HOST},
                "sources": [{"key": "air", "kind": "collection", "name": "air",
                             "label": "air", "enabled": True, "export": True}]}
        for field, value in (("key", "air2"), ("kind", "sub"), ("name", "air3"),
                             ("label", "Air"), ("enabled", False), ("export", False)):
            with self.subTest(field=field):
                moved = json.loads(json.dumps(base))
                moved["sources"][0][field] = value
                self.assertNotEqual(engine.link_signature(base),
                                    engine.link_signature(moved))
        for path, value in ((("publish", "prefix"), "other"),
                            (("publish", "hostname"), "elsewhere")):
            with self.subTest(path=".".join(path)):
                moved = json.loads(json.dumps(base))
                node = moved
                for part in path[:-1]:
                    node = node[part]
                node[path[-1]] = value
                self.assertNotEqual(engine.link_signature(base),
                                    engine.link_signature(moved))

    def test_link_signature_is_order_insensitive_over_sources(self):
        # The round and the panel both reorder sources; a sync skipped or
        # duplicated because two lists differed only in order would be noise.
        one = {"publish": {}, "sources": [
            {"key": "a", "name": "a", "enabled": True},
            {"key": "b", "name": "b", "enabled": False}]}
        two = {"publish": {}, "sources": [
            {"key": "b", "name": "b", "enabled": False},
            {"key": "a", "name": "a", "enabled": True}]}
        self.assertEqual(engine.link_signature(one), engine.link_signature(two))



class DashboardScriptTest(unittest.TestCase):
    """The dashboard script has to parse; a quoting slip makes the page blank.

    This is not hypothetical: an unescaped quote in a message produced a page
    that rendered its static markup but ran no script at all, so every panel
    stayed empty. Only a real parse of the served script catches that.

    The script used to be inlined in the Python page template and was pulled out
    of `ui.render()`'s output by splitting on `<script>`. It is a static asset
    now -- that is what lets the same front end be deployed to a CDN -- so it is
    read directly, and one test below pins the property that made the split
    possible: the served page must reference it rather than inline it, because
    the CSP is `script-src 'self'` with no `'unsafe-inline'`.
    """

    def rendered_script(self):
        from mihomo_test import ui

        return ui.asset_text("app.js")

    def test_the_served_page_loads_the_script_as_an_asset(self):
        from mihomo_test import ui

        page = ui.render("t", "k")
        self.assertIn('src="app.js"', page)
        # `<script>` with no attributes is what an inline block looks like, and
        # the strict CSP would block one -- the panel would paint and then never
        # run. The bootstrap is `type="application/json"`, so it is data.
        self.assertNotIn("<script>", page)
        self.assertNotIn("onclick=", page)

    def test_page_has_no_unbalanced_braces(self):
        script = self.rendered_script()
        for op, cl in (("{", "}"), ("(", ")"), ("[", "]")):
            self.assertEqual(script.count(op), script.count(cl),
                             f"unbalanced {op}{cl} in the dashboard script")

    def test_the_script_writes_no_inline_event_handlers(self):
        """`script-src 'self'` blocks inline handlers, and it does so silently.

        The shell is checked elsewhere, but the table rows are built as template
        strings *inside* app.js -- and an `onclick="copyUrl(this)"` there is
        exactly the regression that made the 复制 button dead on the live panel
        (the browser refuses the handler and nothing else reports it). No
        DOM-level offline script catches this: the CSP is enforced by the
        browser, and the mock harness does not send one.
        """
        script = self.rendered_script()
        # `\son…` and not `on…`: `data-on="…"` is a data attribute, not a
        # handler, and a looser pattern flags it.
        found = re.findall(r"""\son[a-z]+\s*=\s*["']""", script)
        self.assertEqual(found, [], f"generated HTML contains inline handlers: {found}")

    def test_the_shell_writes_no_inline_event_handlers(self):
        from mihomo_test import ui

        page = ui.render("t", "k")
        found = re.findall(r"""\son[a-z]+\s*=\s*["']""", page)
        self.assertEqual(found, [], f"shell contains inline handlers: {found}")

    def test_page_embeds_the_token_and_no_placeholder(self):
        from mihomo_test import ui

        page = ui.render("我的标题", "secret-token")
        self.assertIn("secret-token", page)
        self.assertIn("我的标题", page)
        self.assertNotIn("__BOOTSTRAP__", page)
        self.assertNotIn("__TOKEN__", page)
        self.assertNotIn("__TITLE__", page)

    @unittest.skipIf(shutil.which("node") is None, "node is not installed")
    def test_rendered_script_is_valid_javascript(self):
        import subprocess
        import tempfile

        script = self.rendered_script()
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "page.js")
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(script)
            result = subprocess.run(["node", "--check", path],
                                    capture_output=True, text=True)
        self.assertEqual(result.returncode, 0,
                         "dashboard JS does not parse: " + (result.stderr or "")[:800])

    def test_key_validation_hint_is_a_single_defined_constant(self):
        script = self.rendered_script()
        self.assertIn("const KEY_HINT =", script)
        self.assertIn("function badKey(", script)
        # the hint must never be inlined into a JS string literal
        self.assertNotIn('err.textContent = "key', script)

    @unittest.skipIf(shutil.which("node") is None, "node is not installed")
    def test_a_failed_save_restores_the_previous_sources(self):
        """Optimistic writes must not outlive the request that justified them.

        Six callers assign `CONFIG.sources` before awaiting `saveSources`, so a
        save the server rejected (or a fetch that never resolved) left the
        table showing an unsaved state -- and the old failure branch called
        `loadSources()`, which repaints from that same optimistic array. The
        rollback therefore has to happen inside `saveSources`, from a snapshot
        taken before the request, and a *rejected* fetch has to reach it at
        all: it carries no JSON `error` field, so it used to die unhandled.
        """
        import subprocess
        import tempfile

        script = self.rendered_script()
        start = script.index("async function saveSources(")
        fn = script[start:script.index("\n}\n", start) + len("\n}\n")]

        harness = f'''
"use strict";
const fs = require("fs");
let CONFIG = {{sources: [{{key: "old"}}]}};
let SOURCES_LOADED = false;
let apiMode = "reject";
const notices = [];
const paints = [];
function api(path, opts){{
  if (apiMode === "reject") return Promise.reject(new Error("server down"));
  if (apiMode === "error")
    return Promise.resolve({{json: () => Promise.resolve({{error: "配置无效"}})}});
  return Promise.resolve({{json: () => Promise.resolve({{sources: [{{key: "new"}}]}})}});
}}
function showNotice(t, b, n, level){{ notices.push([level, t]); }}
function renderSources(){{ paints.push("sources"); }}
function renderStatusRefresh(){{ paints.push("status"); }}
function loadSources(){{ return Promise.resolve(); }}

{fn}

async function main(){{
  const incoming = [{{key: "new"}}];
  // 1. the server is unreachable: fetch rejects, there is no JSON to inspect
  let r = await saveSources(incoming);
  if (!r || !r.error) throw new Error("a rejected fetch must come back as an error");
  if (JSON.stringify(CONFIG.sources) !== '[{{"key":"old"}}]')
    throw new Error("network failure left the optimistic state: "
                    + JSON.stringify(CONFIG.sources));
  if (!paints.includes("sources")) throw new Error("the rollback was never repainted");
  // 2. the server refuses it
  apiMode = "error";
  r = await saveSources(incoming);
  if (!r.error) throw new Error("expected the server's error to be returned");
  if (JSON.stringify(CONFIG.sources) !== '[{{"key":"old"}}]')
    throw new Error("a 400 left the optimistic state: " + JSON.stringify(CONFIG.sources));
  // 3. it succeeds: the server's own view wins over what we sent
  apiMode = "ok";
  r = await saveSources(incoming);
  if (r.error) throw new Error("unexpected error: " + r.error);
  if (JSON.stringify(CONFIG.sources) !== '[{{"key":"new"}}]')
    throw new Error("server state not adopted: " + JSON.stringify(CONFIG.sources));
  if (!SOURCES_LOADED) throw new Error("a successful save must mark the sources loaded");
}}
main().then(() => console.log("OK"),
            e => {{ console.error(String(e)); process.exit(1); }});
'''
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "save.js")
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(harness)
            result = subprocess.run(["node", path], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0,
                         "rollback harness failed: "
                         + (result.stderr or result.stdout or "")[:800])


class BuildConfigTest(unittest.TestCase):
    """The probe container runs with host networking, so binding matters."""

    def setUp(self):
        import shutil
        import tempfile
        from pathlib import Path

        from mihomo_test import config as cfgmod

        self.tmp = Path(tempfile.mkdtemp())
        self._old = cfgmod.CORE_DIR
        cfgmod.CORE_DIR = self.tmp
        self._shutil = shutil
        self.core_cfg = {"api": "http://127.0.0.1:19190", "mixed_port": 19194,
                         "probe_group": "__PROBE__"}

    def tearDown(self):
        from mihomo_test import config as cfgmod

        cfgmod.CORE_DIR = self._old
        self._shutil.rmtree(self.tmp, ignore_errors=True)

    def render(self, proxies=None):
        if proxies is None:
            proxies = [{"name": "n1", "type": "vless", "server": "s", "port": 1}]
        path = coremod.build_config(proxies, self.core_cfg, "sekret")
        return path.read_text(encoding="utf-8")

    def test_controller_binds_to_the_configured_loopback_port(self):
        text = self.render()
        self.assertIn("external-controller: 127.0.0.1:19190", text)

    def test_nothing_binds_to_all_interfaces(self):
        text = self.render()
        self.assertNotIn("0.0.0.0", text)
        self.assertNotIn("bind-address: '*'", text)
        self.assertIn("bind-address: 127.0.0.1", text)
        # bind-address is inert unless allow-lan is on, and with it off the
        # kernel binds the wildcard address instead
        self.assertIn("allow-lan: true", text)

    def test_ipv6_is_enabled_both_globally_and_for_dns(self):
        text = self.render()
        # global switch sits at column 0; the dns one is indented
        self.assertIn("ipv6: true", [ln.strip() for ln in text.splitlines()])
        self.assertIn("  ipv6: true", text)

    def test_every_proxy_and_every_lane_group_is_rendered(self):
        """Renamed from `test_probe_group_lists_every_proxy`.

        It never asserted anything about a probe group: it checks that both
        proxies reach the rendered config. The name was a leftover from the
        single-select-group design that `core.lane_group` replaced, and it
        sent a reader looking for a `probe_group` key that no code reads (that
        key has since been removed from DEFAULTS). The lane groups are what
        actually carry the proxies now, so they are asserted here.
        """
        text = self.render([
            {"name": "a", "type": "vless", "server": "s", "port": 1},
            {"name": "b", "type": "trojan", "server": "t", "port": 2},
        ])
        self.assertIn('"a"', text)
        self.assertIn('"b"', text)
        self.assertIn(f'"{coremod.lane_group(0)}"', text)

    def test_empty_proxy_list_still_yields_a_usable_group(self):
        text = self.render([])
        self.assertIn("DIRECT", text)

    def test_parse_port_reads_the_url_port(self):
        self.assertEqual(coremod.parse_port("http://127.0.0.1:19190", 1), 19190)
        self.assertEqual(coremod.parse_port("http://127.0.0.1", 1), 1)
        self.assertEqual(coremod.parse_port(None, 7), 7)


class SelfReferenceTest(unittest.TestCase):
    """The published collection must never be consumed as its own input."""

    def cfg(self, sources):
        return {"sources": sources, "publish": {"prefix": "probe"}}

    def test_output_collection_is_dropped(self):
        messages = []
        kept = engine._drop_self_references(
            self.cfg([]),
            [{"key": "probe", "kind": "collection", "name": "probe", "enabled": True},
             {"key": "air", "kind": "collection", "name": "air", "enabled": True}],
            lambda lvl, msg: messages.append(msg))
        self.assertEqual([s["key"] for s in kept], ["air"])
        self.assertTrue(any("自我循环" in m for m in messages))

    def test_same_name_as_a_single_sub_is_allowed(self):
        # only the collection is ours; a sub that happens to share the name is not
        kept = engine._drop_self_references(
            self.cfg([]),
            [{"key": "probe", "kind": "sub", "name": "probe", "enabled": True}],
            lambda *a: None)
        self.assertEqual(len(kept), 1)

    def test_other_collections_are_untouched(self):
        sources = [{"key": k, "kind": "collection", "name": k, "enabled": True}
                   for k in ("air", "CDN", "中国地区")]
        kept = engine._drop_self_references(self.cfg([]), sources, lambda *a: None)
        self.assertEqual(len(kept), 3)

    def test_blank_prefix_disables_the_guard(self):
        kept = engine._drop_self_references(
            {"sources": [], "publish": {"prefix": ""}},
            [{"key": "probe", "kind": "collection", "name": "probe", "enabled": True}],
            lambda *a: None)
        self.assertEqual(len(kept), 1)


class EntryClassificationTest(unittest.TestCase):
    """CN-entry nodes cannot be tested from here and must not count as dead."""

    def entry(self, server, source="s", name="n"):
        proxy = {"name": name, "type": "vless", "server": server, "port": 1}
        # A real fingerprint, computed the way `collect_entries` computes it.
        # The old helper hardcoded `fp: None`, which meant these tests never
        # exercised the identity the ledger actually keys on.
        return {"source": source, "name": name, "proxy": proxy,
                "index": 0, "fp": engine._orig_fp(proxy)}

    def test_a_fallback_round_keeps_the_same_fingerprint_as_a_resolved_one(self):
        """The fingerprint must not depend on whether DNS answered.

        `classify_and_expand` has two paths: resolved domains become per-address
        variants carrying `orig_fp`, and domains that resolve from neither
        vantage pass through untouched. The passthrough entries used to carry no
        `fp` at all, so `core.prepare` fell back to hashing the *transformed*
        proxy -- stripped of `dialer-proxy`, optionally of `ech-opts`, with
        `port` coerced to int. For a node with `port: "443"` or `ech-opts` that
        is a different hash, and `_prune_removed_nodes` uses the fingerprints
        seen this round as its keep-list, so every fallback round DELETED the
        node's ledger row and reset `consec_fail` to zero. A node that should
        converge to dead after three consecutive failures never got past one.

        Both paths must therefore produce the identical fingerprint for the
        identical node.
        """
        proxy = {"name": "ech-node", "type": "vless", "server": "cf.example",
                 "port": "443", "ech-opts": {"enable": True}, "uuid": "u"}
        base = {"source": "s", "name": "ech-node", "proxy": proxy, "index": 0}
        resolved_entry = dict(base, fp=engine._orig_fp(proxy))

        with unittest.mock.patch.object(engine.dohmod, "resolve_views",
                                        lambda name, v, t: {"cf.example": {"overseas": ["5.6.7.8"]}}), \
                unittest.mock.patch.object(engine, "lookup_countries",
                                           lambda ips, log: {"5.6.7.8": "US"}):
            expanded, _ = engine.classify_and_expand([resolved_entry], self._cfg(), _nolog)
        with unittest.mock.patch.object(engine.dohmod, "resolve_views",
                                        lambda name, v, t: {"cf.example": {}}):
            fallback, _ = engine.classify_and_expand([resolved_entry], self._cfg(), _nolog)

        self.assertEqual(len(expanded), 1)
        self.assertEqual(len(fallback), 1)
        self.assertEqual(expanded[0]["fp"], fallback[0]["fp"],
                         "the fallback path changed the node's ledger identity")

        # And `core.prepare` must agree with both, including for an entry that
        # arrives without an fp (a caller that builds entries by hand).
        expected = engine._orig_fp(proxy)
        for entries in (expanded, fallback):
            _proxies, mapping, _dropped = coremod.prepare([dict(entries[0])])
            self.assertEqual(mapping[0]["fp"], expected)
            # ...and the same for an entry carrying no fp at all.
            bare = {k: v for k, v in entries[0].items() if k != "fp"}
            _proxies, mapping, _dropped = coremod.prepare([bare])
            self.assertEqual(mapping[0]["fp"], expected)

    def test_prepare_keeps_the_unstripped_proxy_for_the_export(self):
        """A chained node must not lose `dialer-proxy` on a fallback round.

        `orig_proxy` is what gets published. It used to fall back to the
        already-stripped proxy, so a node exported from a fallback round was a
        config that could not work.
        """
        proxy = {"name": "chained", "type": "vless", "server": "a.example",
                 "port": 443, "dialer-proxy": "__front__"}
        entry = {"source": "s", "name": "chained", "proxy": proxy, "index": 0,
                 "fp": engine._orig_fp(proxy)}
        _proxies, mapping, _dropped = coremod.prepare([entry])
        self.assertEqual(mapping[0]["orig_proxy"]["dialer-proxy"], "__front__")
        # ...while the kernel-facing proxy still has it stripped.
        self.assertNotIn("dialer-proxy", _proxies[0])

    def test_the_fallback_entry_keeps_a_usable_export_form(self):
        proxy = {"name": "n", "type": "vless", "server": "cf.example",
                 "port": 443, "dialer-proxy": "__front__"}
        with unittest.mock.patch.object(engine.dohmod, "resolve_views",
                                        lambda name, v, t: {"cf.example": {}}):
            entries, _ = engine.classify_and_expand(
                [{"source": "s", "name": "n", "proxy": proxy, "index": 0,
                  "fp": engine._orig_fp(proxy)}], self._cfg(), _nolog)
        _proxies, mapping, _dropped = coremod.prepare(entries)
        self.assertEqual(mapping[0]["orig_proxy"]["server"], "cf.example")
        self.assertEqual(mapping[0]["orig_proxy"]["dialer-proxy"], "__front__")

    def _cfg(self, **over):
        cfg = {"verify": {"entry_check": True, "exclude_entry_countries": ["CN"],
                          "domain_pass": "any"},
               "dns": {"views": {"cn": {"resolver": "r-cn", "ecs": "114.114.114.0/24"},
                                 "overseas": {"resolver": "r-ov", "ecs": "8.8.8.8/24"}},
                       "timeout_s": 1, "cache_hours": 0}}
        cfg["verify"].update(over)
        return cfg

    def test_only_all_cn_hosts_are_excluded(self):
        """A multi-homed host is still worth testing through its other path."""
        views = {"cn-only.example": {"cn": ["1.2.3.4"], "overseas": []},
                 "dual.example": {"cn": ["1.2.3.4"], "overseas": ["5.6.7.8"]},
                 "us.example": {"cn": [], "overseas": ["5.6.7.8"]}}
        countries = {"1.2.3.4": "CN", "5.6.7.8": "US"}
        with unittest.mock.patch.object(engine.dohmod, "resolve_views",
                                        lambda name, v, t: views[name]),                 unittest.mock.patch.object(engine, "lookup_countries",
                                           lambda ips, log: countries):
            test_entries, excluded = engine.classify_and_expand(
                [self.entry(h, name=h) for h in views], self._cfg(),
                lambda *a: None)
        self.assertEqual({e["name"] for e in excluded}, {"cn-only.example"})
        # the multi-homed host keeps testing through its overseas address
        ips = {e["name"]: e["test_ip"] for e in test_entries}
        self.assertEqual(ips.get("dual.example"), "5.6.7.8")

    def test_domain_expands_to_one_test_per_address(self):
        views = {"multi.example": {"cn": ["1.2.3.4"], "overseas": ["5.6.7.8", "5.6.7.8"]}}
        countries = {"1.2.3.4": "US", "5.6.7.8": "US"}
        with unittest.mock.patch.object(engine.dohmod, "resolve_views",
                                        lambda name, v, t: views[name]),                 unittest.mock.patch.object(engine, "lookup_countries",
                                           lambda ips, log: countries):
            test_entries, excluded = engine.classify_and_expand(
                [self.entry("multi.example")], self._cfg(), lambda *a: None)
        self.assertEqual(excluded, [])
        self.assertEqual(len(test_entries), 2)
        self.assertEqual({e["test_ip"] for e in test_entries}, {"1.2.3.4", "5.6.7.8"})
        # every variant keeps the original domain in its export form
        self.assertTrue(all(e["orig_proxy"]["server"] == "multi.example"
                            for e in test_entries))

    def test_lookup_failure_falls_back_to_the_domain_form(self):
        """Both vantages failing must not drop the node; let the kernel try."""
        views = {"multi.example": {}}
        with unittest.mock.patch.object(engine.dohmod, "resolve_views",
                                        lambda name, v, t: views[name]):
            test_entries, excluded = engine.classify_and_expand(
                [self.entry("multi.example")], self._cfg(), lambda *a: None)
        self.assertEqual(excluded, [])
        self.assertEqual(len(test_entries), 1)
        # passthrough form: no per-address variant, kernel resolves it itself
        self.assertNotIn("test_ip", test_entries[0])

    def test_entry_check_can_be_turned_off(self):
        cfg = self._cfg(entry_check=False)
        with unittest.mock.patch.object(
                engine, "lookup_countries",
                lambda ips, log: self.fail("should not look up")):
            test_entries, excluded = engine.classify_and_expand(
                [self.entry("203.0.113.10")], cfg, lambda *a: None)
        self.assertEqual(excluded, [])
        self.assertEqual(len(test_entries), 1)

    def test_country_lookup_runs_once_per_round_not_once_per_server(self):
        """One batched lookup per round -- a call per server rate-limits the API.

        ip-api answers a request storm with HTTP 429, and a 429 here does not
        fail loudly: it silently switches entry filtering off for the whole
        round. So the call count is a correctness property, not an
        optimisation -- this test exists because the lookup used to sit inside
        the per-server loop and ran once per server.
        """
        calls = []

        def spy(ips, log):
            calls.append(list(ips))
            return {ip: "US" for ip in ips}

        entries = [self.entry("1.2.3.4"), self.entry("5.6.7.8"),
                   self.entry("9.9.9.9")]
        with unittest.mock.patch.object(engine, "lookup_countries", spy):
            test_entries, excluded = engine.classify_and_expand(
                entries, self._cfg(), _nolog)
        self.assertEqual(len(calls), 1, f"expected one batched lookup, got {calls}")
        self.assertEqual(sorted(calls[0]), ["1.2.3.4", "5.6.7.8", "9.9.9.9"])
        self.assertEqual(excluded, [])
        self.assertEqual(len(test_entries), 3)

    def test_failed_lookup_retries_once_then_reports_undecided_addresses(self):
        """A 429 is a rate limit, not a permanent failure -- but retry once only.

        The budget stays small on purpose: the round has already given up on
        filtering, so a longer retry loop only deepens the hole. The message
        must also say how much went undecided, or a skipped filter looks
        identical to "nothing needed checking".
        """
        calls = []

        def boom(req, timeout=None):
            calls.append(req)
            raise OSError("HTTP Error 429: Too Many Requests")

        logged = []
        cache = _GeoCache()
        with unittest.mock.patch.object(engine.db, "ip_geo_get", cache.get), \
                unittest.mock.patch.object(engine.db, "ip_geo_put", cache.put), \
                unittest.mock.patch.object(engine.urllib.request, "urlopen", boom), \
                unittest.mock.patch.object(engine.time, "sleep"):
            got = engine.lookup_countries(["203.0.113.7"],
                                          lambda lv, msg: logged.append((lv, msg)))
        self.assertEqual(len(calls), 2, "expected exactly one retry")
        self.assertEqual(got, {})
        self.assertEqual(len(logged), 1)
        self.assertEqual(logged[0][0], "warn")
        self.assertIn("本轮跳过入口过滤", logged[0][1])
        self.assertIn("1 个地址未定性", logged[0][1])

    def test_lookup_uses_the_second_attempt_when_it_succeeds(self):
        """The retry is worth having: a transient 429 must not lose the round."""
        calls = []

        def flaky(req, timeout=None):
            calls.append(req)
            if len(calls) == 1:
                raise OSError("HTTP Error 429: Too Many Requests")
            return _JsonResp([{"query": "203.0.113.9", "countryCode": "JP",
                               "isp": "Example"}])

        logged = []
        cache = _GeoCache()
        with unittest.mock.patch.object(engine.db, "ip_geo_get", cache.get), \
                unittest.mock.patch.object(engine.db, "ip_geo_put", cache.put), \
                unittest.mock.patch.object(engine.urllib.request, "urlopen", flaky), \
                unittest.mock.patch.object(engine.time, "sleep"):
            got = engine.lookup_countries(["203.0.113.9"],
                                          lambda lv, msg: logged.append((lv, msg)))
        self.assertEqual(len(calls), 2)
        self.assertEqual(got.get("203.0.113.9"), "JP")
        self.assertEqual(logged, [])

    def test_a_failed_chunk_reports_every_address_it_stranded(self):
        """A failure stops the whole sequence, so the count must cover all of it.

        The warning used to name only the failing chunk (90 addresses), which
        understates what lost its entry filter: the remaining addresses are
        never looked up either, so an operator reading "90" would conclude that
        110 of the 200 had been classified.
        """
        calls = []

        def boom(req, timeout=None):
            calls.append(req)
            raise OSError("HTTP Error 429: Too Many Requests")

        ips = [f"198.51.100.{i}" for i in range(1, 201)]  # 3 chunks: 90/90/20
        logged = []
        cache = _GeoCache()
        with unittest.mock.patch.object(engine.db, "ip_geo_get", cache.get), \
                unittest.mock.patch.object(engine.db, "ip_geo_put", cache.put), \
                unittest.mock.patch.object(engine.urllib.request, "urlopen", boom), \
                unittest.mock.patch.object(engine.time, "sleep"):
            got = engine.lookup_countries(
                ips, lambda lv, msg: logged.append((lv, msg)))
        self.assertEqual(got, {})
        self.assertEqual(len(calls), 2,
                         "must not keep hammering the endpoint after chunk one fails")
        self.assertEqual(len(logged), 1)
        self.assertIn("200 个地址未定性", logged[0][1])

    def test_excluded_nodes_are_recorded_and_survive_the_prune(self):
        tmp = Path(tempfile.mkdtemp())
        old_data, old_export = cfgmod.DATA, engine.EXPORT_DIR
        old_path, old_conn = engine.db.DB_PATH, engine.db._conn
        engine.db.DB_PATH, engine.db._conn = tmp / "s.db", None
        engine.EXPORT_DIR = tmp
        try:
            round_id = engine.db.start_round("test")
            excluded = [dict(self.entry("203.0.113.10", source="deltasub", name="CN relay"))]
            engine._apply_and_publish(
                {"policy": {"drop_after_consecutive_fails": 3},
                 "verify": {}, "publish": {"enabled": False}, "sources": []},
                round_id, None, {}, {}, {}, [], [], lambda *a: None,
                excluded_entries=excluded)
            fp = coremod.fingerprint_proxy(
                {"type": "vless", "server": "203.0.113.10", "port": 1})
            node = engine.db.get_node("deltasub", fp)
            self.assertIsNotNone(node)
            self.assertEqual(node["status"], policy.EXCLUDED)
            self.assertEqual(node["last_reason"], "entry_cn")
            # the prune must keep excluded records, or they would vanish each round
            self.assertEqual(engine.db.delete_nodes_not_in("deltasub", [fp]), 0)
        finally:
            if engine.db._conn is not None:
                engine.db._conn.close()
            engine.db.DB_PATH, engine.db._conn = old_path, old_conn
            cfgmod.DATA, engine.EXPORT_DIR = old_data, old_export
            shutil.rmtree(tmp, ignore_errors=True)



class EgressUnverifiedTest(unittest.TestCase):
    """A node never egress-checked must not pass as verified.

    `_resolve_exit` deliberately leaves an unverified node alone rather than
    killing it (pinned by EntryClassificationTest's skipped-verification case),
    and that is right per node. But it also left the *round* free to publish
    nodes whose exit country was never checked -- with a country filter
    configured, that can send the user's traffic out through the country the
    filter exists to refuse, and afterwards `results.detail` and
    `results.country` are both empty so nothing records it.
    """

    class _NeverRuns:
        """A core that must not be touched once the deadline has passed."""

        def select(self, group, name):
            raise AssertionError("selected a node after the deadline")

        def egress(self, port, url, timeout):
            raise AssertionError("ran egress after the deadline")

    def test_the_deadline_skipped_nodes_are_reported_not_hidden(self):
        core_cfg = {"api": "http://127.0.0.1:1", "lanes": 2, "base_port": 19200}
        verify_cfg = {"trace_url": "http://t/trace", "timeout_s": 1, "max_nodes": 0}
        out, unverified, over_limit = engine._verify_egress(
            self._NeverRuns(), core_cfg, ["n1", "n2", "n3"], verify_cfg, _nolog,
            deadline=0.0)
        self.assertEqual(out, {})
        self.assertEqual(unverified, {"n1", "n2", "n3"})
        self.assertEqual(over_limit, [])

    def test_max_nodes_truncation_is_reported(self):
        """The setting truncates on purpose; the round must still say so."""
        core_cfg = {"api": "http://127.0.0.1:1", "lanes": 1, "base_port": 19200}
        verify_cfg = {"trace_url": "http://t/trace", "timeout_s": 1, "max_nodes": 2}
        out, unverified, over_limit = engine._verify_egress(
            self._NeverRuns(), core_cfg, ["n1", "n2", "n3", "n4"], verify_cfg, _nolog,
            deadline=0.0)
        self.assertEqual(out, {})
        # the deadline skipped the queue itself; the truncation is separate and
        # must be reported even when nothing was verified
        self.assertEqual(unverified, {"n1", "n2"})
        self.assertEqual(sorted(over_limit), ["n3", "n4"])

    def _publish(self, verify_cfg, unverified):
        tmp = Path(tempfile.mkdtemp())
        old_path, old_conn = engine.db.DB_PATH, engine.db._conn
        old_export = engine.EXPORT_DIR
        engine.db.DB_PATH, engine.db._conn = tmp / "s.db", None
        engine.EXPORT_DIR = tmp
        try:
            round_id = engine.db.start_round("test")
            cfg = {"policy": {"drop_after_consecutive_fails": 3},
                   "verify": verify_cfg, "publish": {"enabled": False},
                   "sources": []}
            return engine._apply_and_publish(
                cfg, round_id, None, {}, {}, {}, [], [], _nolog,
                unverified=unverified)
        finally:
            if engine.db._conn is not None:
                engine.db._conn.close()
            engine.db.DB_PATH, engine.db._conn = old_path, old_conn
            engine.EXPORT_DIR = old_export
            shutil.rmtree(tmp, ignore_errors=True)

    def test_an_unverified_round_is_not_published_when_a_filter_is_configured(self):
        summary = self._publish({"exclude_countries": ["CN"]}, {"n1", "n2"})
        self.assertTrue(summary["suspect"])
        self.assertIn("未完成出口验证", summary["note"])

    def test_unverified_is_only_a_warning_without_a_country_filter(self):
        """With no country filter the exit check is informational, not a gate."""
        summary = self._publish({"exclude_countries": []}, {"n1"})
        self.assertFalse(summary["suspect"])


class RoundBudgetTest(unittest.TestCase):
    """Why the unverified gate is a backstop, not the primary protection.

    A deadline is the only way `_verify_egress` leaves queued nodes unchecked,
    and `_run_round` calls `_checkpoint("publish")` immediately after it. So the
    round is abandoned (and the previous export kept) before the gate in
    `_apply_and_publish` is ever reached. That ordering is the real protection;
    the gate only matters if this checkpoint is ever moved or removed.
    """

    def test_an_expired_budget_raises_at_the_next_checkpoint(self):
        # `_checkpoint` writes round.state.json before it checks the deadline.
        # Unpatched, that landed in the deployment's real data/ directory when
        # the suite ran on the host -- the tests must not touch live state.
        with unittest.mock.patch.object(engine, "_write_state"):
            with self.assertRaises(engine.RoundTimeout):
                engine._checkpoint(time.monotonic() - 1, "publish", None)

    def test_a_live_budget_passes_the_checkpoint(self):
        with unittest.mock.patch.object(engine, "_write_state"):
            engine._checkpoint(time.monotonic() + 60, "publish", None)


class _FakeFrontStore:
    """Stand-in for the Sub-Store client: one resource plus one manual sub."""

    def __init__(self, proxies, error=None, manual=(), write_error=None):
        self.proxies, self.error = proxies, error
        self.write_error = write_error
        self.manual = [dict(p) for p in manual]
        self.calls, self.writes, self.deleted = [], [], []

    def fetch_source(self, kind, name, target="ClashMeta"):
        self.calls.append((kind, name))
        if self.error:
            raise StoreError(self.error)
        return [dict(p) for p in self.proxies]

    def fetch_sub_proxies(self, name, target="ClashMeta"):
        # The manual pool is materialised as a local sub and read back, so the
        # fake has to serve it from its own list: returning the resource here
        # would make "pasted" and "from the resource" indistinguishable, and
        # that distinction is the one these tests exist to pin.
        self.calls.append(("sub", name))
        if self.error:
            raise StoreError(self.error)
        return [dict(p) for p in self.manual]

    def upsert(self, kind, name, payload):
        self.writes.append((kind, name, payload))
        if self.write_error:
            raise self.write_error
        return "created"

    def delete(self, kind, name):
        self.deleted.append((kind, name))
        raise NotFound(f"sub {name}")


def _front_proxy(name, server="front.example"):
    """A front shaped like the real one: xhttp with the x-padding family."""
    return {"name": name, "type": "vless", "server": server, "port": 443,
            "uuid": "00000000-0000-4000-8000-000000000000", "network": "xhttp",
            "xhttp-opts": {"path": "/", "mode": "stream-one",
                           "x-padding-obfs-mode": True, "x-padding-key": "_000000",
                           "x-padding-header": "abcdef",
                           "x-padding-placement": "queryInHeader",
                           "x-padding-method": "tokenish"}}


class ChainTest(unittest.TestCase):
    """Chain-proxy probing: fronts first, then chains through the live ones.

    The rule these pin is that a chain's verdict has to be about the front as
    much as the node. A node carrying `dialer-proxy` is not reachable by the
    path a direct test measures, so testing it direct reports a false alive; and
    when no front carries it the node fails with `front_dead` rather than the
    `timeout` a dial nobody made would produce.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.old_data, self.old_export = cfgmod.DATA, engine.EXPORT_DIR
        self.old_path, self.old_conn = engine.db.DB_PATH, engine.db._conn
        cfgmod.DATA = self.tmp
        engine.db.DB_PATH, engine.db._conn = self.tmp / "s.db", None
        engine.EXPORT_DIR = self.tmp / "exports"
        engine.db.connect()

    def tearDown(self):
        if engine.db._conn is not None:
            engine.db._conn.close()
        engine.db.DB_PATH, engine.db._conn = self.old_path, self.old_conn
        cfgmod.DATA, engine.EXPORT_DIR = self.old_data, self.old_export
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _cfg(self, enabled=True, front_name="cm-xhttp", **chain):
        block = {"enabled": enabled, "max_fronts": 8,
                 "front_source": {"kind": "sub", "name": front_name}}
        block.update(chain)
        return {"policy": {"drop_after_consecutive_fails": 3,
                           "suspect_floor_ratio": 0.5, "suspect_floor_absolute": 3},
                "verify": {"exclude_countries": ["CN"]},
                "publish": {"enabled": True},
                "sources": [{"key": "air", "name": "air"}],
                "chain": block}

    @staticmethod
    def _test_cfg():
        return {"targets": ["http://probe/generate_204"], "expected_status": "204",
                "timeout_ms": 100, "timeout_ms_retry": 100, "max_attempts": 1,
                "retry_pause_s": 0}

    @staticmethod
    def _res(reason=None, delay_ms=100):
        return {"reason": reason, "delay_ms": delay_ms, "attempts": 1, "detail": ""}

    # ---------------------------------------------------------------- config

    def test_chain_block_needs_both_the_switch_and_a_front_source(self):
        self.assertIsNone(engine.chain_block(self._cfg(enabled=False)))
        self.assertIsNone(engine.chain_block(self._cfg(front_name="")))
        self.assertIsNotNone(engine.chain_block(self._cfg()))

    def test_normalize_chain_repairs_a_hand_edited_block(self):
        out = cfgmod.normalize_chain({"enabled": True, "front_source": {"kind": "nope", "name": " x "}})
        self.assertEqual(out["front_source"], {"kind": "sub", "name": "x"})
        self.assertIs(out["enabled"], True)
        # absent / malformed input degrades to the disabled default instead of raising
        self.assertFalse(cfgmod.normalize_chain(None)["enabled"])
        self.assertEqual(cfgmod.normalize_chain("junk")["front_source"]["kind"], "sub")

    def test_a_saved_patch_is_clamped_and_normalised(self):
        clean, notes = cfgmod.validate_patch(
            {"chain": {"enabled": True, "max_fronts": 999,
                       "front_source": {"kind": "bogus", "name": " cm-xhttp "}}})
        self.assertEqual(clean["chain"]["max_fronts"], 64)
        self.assertTrue(any("chain.max_fronts" in n for n in notes))
        old_path = cfgmod.CONFIG_PATH
        cfgmod.CONFIG_PATH = self.tmp / "config.json"
        try:
            cfg = cfgmod.update({"chain": clean["chain"]})
        finally:
            cfgmod.CONFIG_PATH = old_path
        self.assertEqual(cfg["chain"]["front_source"], {"kind": "sub", "name": "cm-xhttp"})
        self.assertEqual(cfg["chain"]["max_fronts"], 64)

    # ----------------------------------------------------------- front pool

    def test_collect_fronts_uses_reserved_kernel_names(self):
        store = _FakeFrontStore([_front_proxy("edgetunnel"),
                                 _front_proxy("edgetunnel-RUD")])
        fronts = engine.collect_fronts(self._cfg(), store, _nolog)
        self.assertEqual([f["proxy"]["name"] for f in fronts],
                         ["__FRONT0__", "__FRONT1__"])
        self.assertEqual([f["name"] for f in fronts], ["edgetunnel", "edgetunnel-RUD"])
        self.assertTrue(all(f["source"] == engine.FRONT_SOURCE_KEY for f in fronts))
        self.assertTrue(all(f["no_expand"] for f in fronts))
        self.assertTrue(all(f["role"] == "front" for f in fronts))
        # The display name must not leak into the kernel name, or a chained
        # variant could end up dialling through one of the user's own nodes.
        self.assertNotIn("edgetunnel", fronts[0]["proxy"]["name"])

    def test_collect_fronts_caps_the_pool(self):
        store = _FakeFrontStore([_front_proxy(f"f{i}") for i in range(5)])
        fronts = engine.collect_fronts(self._cfg(max_fronts=2), store, _nolog)
        self.assertEqual(len(fronts), 2)

    def test_collect_fronts_survives_a_broken_source(self):
        store = _FakeFrontStore([], error="HTTP 500 boom")
        self.assertEqual(engine.collect_fronts(self._cfg(), store, _nolog), [])

    def test_collect_fronts_is_off_when_chaining_is_off(self):
        store = _FakeFrontStore([_front_proxy("f")])
        self.assertEqual(engine.collect_fronts(self._cfg(enabled=False), store, _nolog), [])
        self.assertEqual(store.calls, [])

    # -------------------------------------------------------------- expansion

    def _node(self, name="N", fp="a" * 16, dialer="hk_b"):
        proxy = {"name": name, "type": "vless", "server": "t.example", "port": 443,
                 "uuid": "u"}
        if dialer is not None:
            proxy["dialer-proxy"] = dialer
        return {"source": "air", "name": name, "index": 0, "fp": fp, "proxy": proxy}

    def test_expand_chains_builds_one_variant_per_front(self):
        out, chained = engine.expand_chains([self._node()], ["__FRONT0__", "__FRONT1__"])
        self.assertEqual(chained, 1)
        self.assertEqual(len(out), 2)
        self.assertEqual({e["proxy"]["dialer-proxy"] for e in out},
                         {"__FRONT0__", "__FRONT1__"})
        self.assertTrue(all(e["role"] == "chain" for e in out))
        self.assertEqual({e["front"] for e in out}, {"__FRONT0__", "__FRONT1__"})
        # Identity is the node's, not the path's: one fingerprint for every
        # variant, which is what makes "any front carried it" the verdict.
        self.assertEqual({e["fp"] for e in out}, {"a" * 16})

    def test_expand_chains_leaves_unchained_nodes_and_fronts_alone(self):
        plain = self._node(dialer=None)
        front = {"source": engine.FRONT_SOURCE_KEY, "name": "edgetunnel",
                 "index": 0, "fp": "f" * 16, "role": "front", "no_expand": True,
                 "proxy": {"name": "__FRONT0__", "type": "vless", "server": "f.example",
                           "port": 443, "dialer-proxy": "something"}}
        out, chained = engine.expand_chains([plain, front], ["__FRONT0__"])
        self.assertEqual(chained, 0)
        self.assertEqual(out, [plain, front])

    # ------------------------------------------- per-source 直连/链式 switches

    def test_measure_flags_without_a_policy_keeps_the_old_behaviour(self):
        # `None` is "no per-source policy this round"; the pre-existing
        # behaviour is chain-only, so a caller that never learned about these
        # switches sees no change at all.
        self.assertEqual(engine._measure_flags("air", None), (False, True))
        self.assertEqual(engine._measure_flags("air", {}), (False, True))

    def test_measure_flags_reads_the_source_switches(self):
        flags = {"air": (False, True), "alphasub": (True, False)}
        self.assertEqual(engine._measure_flags("air", flags), (False, True))
        self.assertEqual(engine._measure_flags("alphasub", flags), (True, False))
        # A source the caller did not mention keeps both ways on.
        self.assertEqual(engine._measure_flags("other", flags), (True, True))

    def test_measure_flags_never_leaves_a_source_unmeasured(self):
        # Both off cannot mean "measure nothing": a node this round never
        # measures is a node `_prune_removed_nodes` deletes from the ledger and
        # `_publish_sources` truncates out of the export. Direct is the fallback.
        self.assertEqual(engine._measure_flags("air", {"air": (False, False)}),
                         (True, False))

    def test_expand_chains_honours_a_direct_only_source(self):
        out, chained = engine.expand_chains([self._node()], ["__FRONT0__"],
                                            {"air": (True, False)})
        self.assertEqual(chained, 0)
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0]["role"], "direct")
        self.assertEqual(out[0]["category"], engine.CAT_DIRECT)
        self.assertNotIn("dialer-proxy", out[0]["proxy"])
        # Single variant, so the fingerprint must stay as it was -- suffixing it
        # would orphan every ledger row the source already has.
        self.assertEqual(out[0]["fp"], "a" * 16)

    def test_expand_chains_honours_a_chain_only_source(self):
        out, chained = engine.expand_chains([self._node()], ["__FRONT0__"],
                                            {"air": (False, True)})
        self.assertEqual(chained, 1)
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0]["role"], "chain")
        self.assertEqual(out[0]["fp"], "a" * 16)

    def test_expand_chains_splits_the_two_ways_onto_separate_rows(self):
        out, chained = engine.expand_chains(
            [self._node()], ["__FRONT0__", "__FRONT1__"], {"air": (True, True)})
        self.assertEqual(chained, 1)
        self.assertEqual(len(out), 3)              # 2 chain variants + 1 direct
        direct = [e for e in out if e["role"] == "direct"]
        self.assertEqual(len(direct), 1)
        self.assertNotIn("dialer-proxy", direct[0]["proxy"])
        # Different fingerprints on purpose: one ledger row each, instead of
        # the two measurements overwriting one another. Both must be real
        # fingerprints -- the ledger asserts 16 hex chars (tests/test_live.py's
        # name-keyed-scheme detector), so the twin is keyed by the fingerprint
        # of its dialer-stripped self, not by a decorated copy of the node's.
        self.assertEqual(len(direct[0]["fp"]), 16)
        self.assertTrue(all(c in "0123456789abcdef" for c in direct[0]["fp"]))
        self.assertNotEqual(direct[0]["fp"], "a" * 16)
        self.assertEqual({e["fp"] for e in out if e["role"] == "chain"}, {"a" * 16})

    def test_expand_chains_does_not_double_a_plain_node(self):
        # No dialer means the two ways are the same measurement, so a source
        # with both switches on must still test it once.
        out, chained = engine.expand_chains([self._node(dialer=None)], ["__FRONT0__"],
                                            {"air": (True, True)})
        self.assertEqual(chained, 0)
        self.assertEqual(len(out), 1)

    def test_the_direct_twin_does_not_share_the_nodes_own_fingerprint(self):
        """Keying the twin by the stripped proxy would fold it into one row.

        `_orig_fp` drops `dialer-proxy` (it is in core.DROP_FIELDS), so the
        stripped proxy hashes to the *chained* node's own fingerprint. Two
        variants sharing an fp is one ledger row, and since the direct variant
        is appended last it would win -- the chained result would silently
        disappear. The fp here has to be derived from the real one, not
        recomputed on the stripped proxy.
        """
        node = self._node()
        node["fp"] = engine._orig_fp(node["proxy"])
        out, _ = engine.expand_chains([node], ["__FRONT0__"], {"air": (True, True)})
        fps = [e["fp"] for e in out]
        self.assertEqual(len(fps), len(set(fps)), f"变体共用指纹: {fps}")

    def test_normalize_sources_defaults_both_switches_on(self):
        out = cfgmod.normalize_sources([{"kind": "collection", "name": "air"}])
        self.assertIs(out[0]["direct"], True)
        self.assertIs(out[0]["chain"], True)

    def test_normalize_sources_keeps_an_explicit_false(self):
        out = cfgmod.normalize_sources(
            [{"kind": "collection", "name": "air", "direct": False, "chain": True}])
        self.assertIs(out[0]["direct"], False)
        self.assertIs(out[0]["chain"], True)

    def test_expand_chains_without_a_front_pool_changes_nothing(self):
        entries = [self._node()]
        out, chained = engine.expand_chains(entries, [])
        self.assertEqual(out, entries)
        self.assertEqual(chained, 0)

    def test_a_round_with_chaining_off_still_falls_through_to_direct(self):
        # The empty pool means three different things, and this one is not a
        # failure: chaining is off (or this is a 直连测活 round), so measuring
        # the nodes as their own servers is what the round already announced in
        # its log. `fail_without_front` is the caller's way of saying which of
        # the two shapes it is.
        entries = [self._node()]
        out, chained = engine.expand_chains(entries, [], {"air": (True, True)})
        self.assertEqual(out, entries)
        self.assertEqual(chained, 0)

    def test_a_chain_round_with_no_pool_fails_its_nodes_instead_of_dialing_them(self):
        """The pool never arrived: `front_dead`, never a direct measurement.

        Stripping the dialer is what `core.prepare` does with a value no front
        satisfies, and it is the exact hazard its docstring names: the round
        would report the node alive on a path its owner never uses. So the
        entry is emitted as a chain-role orphan with `front=None` -- present in
        the config, never dialled, and routed into `chain_failed` by the phase
        split.
        """
        out, chained = engine.expand_chains([self._node()], [], {"air": (True, True)},
                                            fail_without_front=True)
        self.assertEqual(chained, 1)
        self.assertEqual({e["role"] for e in out}, {"chain", "direct"})
        orphan = next(e for e in out if e["role"] == "chain")
        self.assertIsNone(orphan["front"])
        self.assertEqual(orphan["category"], engine.CAT_CHAIN)
        # The ledger identity is the node's, unchanged, so `_record_chain_failures`
        # lands on the same row the node already has and the streak advances.
        self.assertEqual(orphan["fp"], "a" * 16)
        # The direct half of a dual-switch source is still measured direct.
        twin = next(e for e in out if e["role"] == "direct")
        self.assertNotIn("dialer-proxy", twin["proxy"])
        self.assertNotEqual(twin["fp"], orphan["fp"])

    def test_the_direct_twins_key_is_stable_across_resolved_addresses(self):
        """Two rounds, two address sets, one identity.

        `classify_and_expand` runs first, so `proxy["server"]` here is a
        resolved address and the old base (`_orig_fp(stripped)`) hashed it.
        Address churn then changed the twin's fingerprint, `_prune_removed_nodes`
        dropped the row it stopped seeing, and `consec_fail` reset to zero --
        a node that should converge to dead after N failures never got past 1.
        """
        node = self._node()
        node["fp"] = engine._orig_fp(node["proxy"])
        twins = []
        for ip in ("1.2.3.4", "5.6.7.8"):
            expanded = {**node, "proxy": {**node["proxy"], "server": ip},
                        "orig_proxy": node["proxy"], "test_ip": ip}
            out, _ = engine.expand_chains([expanded], ["__FRONT0__"],
                                          {"air": (True, True)})
            twins.append(next(e for e in out if e["role"] == "direct"))
        self.assertEqual(twins[0]["fp"], twins[1]["fp"])
        self.assertNotEqual(twins[0]["fp"], node["fp"])
        self.assertEqual(len(twins[0]["fp"]), 16)

    def test_prepare_keeps_a_dialer_only_when_the_front_is_in_the_config(self):
        entry = {"source": "air", "name": "N", "index": 0,
                 "proxy": {"name": "N", "type": "vless", "server": "t.example",
                           "port": 443, "dialer-proxy": "__FRONT0__"}}
        proxies, _m, _d = coremod.prepare([entry], keep_dialer=["__FRONT0__"])
        self.assertEqual(proxies[0]["dialer-proxy"], "__FRONT0__")
        proxies, _m, _d = coremod.prepare([entry], keep_dialer=["__FRONT1__"])
        self.assertNotIn("dialer-proxy", proxies[0])
        # default stays as before: no chain support means no dialer
        proxies, _m, _d = coremod.prepare([entry])
        self.assertNotIn("dialer-proxy", proxies[0])

    def test_the_kernel_sees_the_front_while_the_export_keeps_the_clients_name(self):
        """`dialer-proxy: hk_b` is the client's group; `__FRONT0__` is ours."""
        proxy = {"name": "N", "type": "vless", "server": "t.example", "port": 443,
                 "dialer-proxy": "hk_b"}
        entry = {"source": "air", "name": "N", "index": 0, "fp": "a" * 16,
                 "proxy": proxy, "orig_proxy": proxy}
        variant = {**entry, "proxy": {**proxy, "dialer-proxy": "__FRONT0__"},
                   "role": "chain", "front": "__FRONT0__"}
        proxies, mapping, _d = coremod.prepare([variant], keep_dialer=["__FRONT0__"])
        self.assertEqual(proxies[0]["dialer-proxy"], "__FRONT0__")
        self.assertEqual(mapping[0]["orig_proxy"]["dialer-proxy"], "hk_b")

    def test_classify_and_expand_does_not_split_a_front_per_address(self):
        cfg = self._cfg()
        front = {"source": engine.FRONT_SOURCE_KEY, "no_expand": True,
                 "fp": "f" * 16, "name": "edgetunnel",
                 "proxy": {"name": "__FRONT0__", "server": "f.example"}}
        node = {"source": "air", "index": 1, "fp": "a" * 16,
                "proxy": {"name": "N", "server": "t.example"}}
        with unittest.mock.patch.object(engine, "_resolve_candidates",
                                        return_value=["1.2.3.4"]), \
                unittest.mock.patch.object(engine, "lookup_countries",
                                           return_value={"1.2.3.4": "US"}):
            out, excluded = engine.classify_and_expand([front, node], cfg, _nolog)
        self.assertEqual(excluded, [])
        self.assertEqual([e for e in out if e.get("no_expand")], [front])
        self.assertEqual([e["proxy"]["server"] for e in out if not e.get("no_expand")],
                         ["1.2.3.4"])

    # ------------------------------------------------------------- two phases

    class _PhaseCore:
        """Fake kernel: a proxy answers iff its name is in `alive`."""

        def __init__(self, alive):
            self.alive, self.seen = set(alive), []

        def delay(self, name, url, timeout_ms, expected):
            self.seen.append(name)
            if name in self.alive:
                return 30, None, ""
            return None, "timeout", "Timeout"

    def _mapping(self, *rows):
        return [{"mihomo": n, "role": role, "front": front,
                 "source": src, "fp": fp}
                for n, role, front, src, fp in rows]

    def test_chains_are_tested_only_through_live_fronts(self):
        core = self._PhaseCore(alive={"__FRONT0__", "N"})
        mapping = self._mapping(
            ("__FRONT0__", "front", None, engine.FRONT_SOURCE_KEY, "f0"),
            ("__FRONT1__", "front", None, engine.FRONT_SOURCE_KEY, "f1"),
            ("N", "chain", "__FRONT0__", "air", "n"),
            ("N #2", "chain", "__FRONT1__", "air", "n"),
        )
        results, failed, live = engine._test_phases(
            core, mapping, self._test_cfg(), 4, None, True, _nolog)
        self.assertEqual(live, 1)
        self.assertIn("N", core.seen)
        self.assertNotIn("N #2", core.seen)
        # One live front was enough, so the node is not failed outright.
        self.assertEqual(failed, [])
        self.assertIsNone(results["N"]["reason"])

    def test_a_node_with_no_live_front_is_failed_without_being_dialled(self):
        core = self._PhaseCore(alive=set())
        mapping = self._mapping(
            ("__FRONT0__", "front", None, engine.FRONT_SOURCE_KEY, "f0"),
            ("N", "chain", "__FRONT0__", "air", "n"),
        )
        results, failed, live = engine._test_phases(
            core, mapping, self._test_cfg(), 4, None, True, _nolog)
        self.assertEqual(live, 0)
        self.assertEqual([m["mihomo"] for m in failed], ["N"])
        self.assertNotIn("N", results)
        self.assertNotIn("N", core.seen)

    def test_the_failed_entries_are_deduplicated_per_node(self):
        """One entry per node, not one per (front x address) variant."""
        core = self._PhaseCore(alive=set())
        mapping = self._mapping(
            ("__FRONT0__", "front", None, engine.FRONT_SOURCE_KEY, "f0"),
            ("__FRONT1__", "front", None, engine.FRONT_SOURCE_KEY, "f1"),
            ("N", "chain", "__FRONT0__", "air", "n"),
            ("N #2", "chain", "__FRONT1__", "air", "n"),
        )
        _r, failed, _live = engine._test_phases(
            core, mapping, self._test_cfg(), 4, None, True, _nolog)
        self.assertEqual(len(failed), 1)

    def test_without_chains_it_is_a_single_pass(self):
        core = self._PhaseCore(alive={"N"})
        mapping = self._mapping(("N", None, None, "air", "n"))
        results, failed, live = engine._test_phases(
            core, mapping, self._test_cfg(), 4, None, False, _nolog)
        self.assertEqual((failed, live), ([], 0))
        self.assertIsNone(results["N"]["reason"])

    # ----------------------------------------------------------- ledger/export

    def _chain_entry(self, **over):
        entry = {"source": "air", "fp": "a" * 16, "original": "N", "mihomo": "N",
                 "proto": "vless", "server": "t.example", "role": "chain"}
        entry.update(over)
        return entry

    def test_front_dead_failure_advances_the_streak_with_an_honest_reason(self):
        round_id = engine.db.start_round("test")
        _by_source, fps = engine._record_chain_failures(
            self._cfg(), round_id, [self._chain_entry()], _nolog)
        node = engine.db.get_node("air", "a" * 16)
        self.assertEqual(node["consec_fail"], 1)
        self.assertEqual(node["last_reason"], engine.FRONT_DEAD_REASON)
        # A node that never passed stays `unknown`; the streak is what counts.
        self.assertEqual(node["status"], policy.UNKNOWN)
        row = engine.db.one("SELECT verdict, reason FROM results WHERE round_id=?",
                            (round_id,))
        self.assertEqual((row["verdict"], row["reason"]),
                         ("fail", engine.FRONT_DEAD_REASON))
        self.assertEqual(fps["air"], {"a" * 16})

    def test_a_chain_failure_moves_a_previously_alive_node_to_pending(self):
        """The verdict is a real failure: it must advance the state machine."""
        entry = self._chain_entry()
        engine.db.upsert_node("air", entry["fp"], "N", status=policy.ALIVE, consec_fail=0)
        round_id = engine.db.start_round("test")
        engine._record_chain_failures(self._cfg(), round_id, [entry], _nolog)
        node = engine.db.get_node("air", entry["fp"])
        self.assertEqual(node["status"], policy.PENDING)
        self.assertEqual(node["consec_fail"], 1)

    def test_a_chain_failure_reaches_dead_at_the_threshold(self):
        entry = self._chain_entry()
        engine.db.upsert_node("air", entry["fp"], "N", status=policy.PENDING, consec_fail=2)
        round_id = engine.db.start_round("test")
        engine._record_chain_failures(self._cfg(), round_id, [entry], _nolog)
        node = engine.db.get_node("air", entry["fp"])
        self.assertEqual(node["status"], policy.DEAD)
        self.assertEqual(node["consec_fail"], 3)

    def test_the_prune_keeps_a_chain_failure_but_would_drop_it_otherwise(self):
        """The row was never dialled, so it is absent from `by_name`.

        Without the fingerprints handed back by `_record_chain_failures`, the
        prune sees a source whose only surviving node is the one it tested, and
        deletes the chain-failed row -- resetting the streak it just advanced,
        so a node whose front is permanently dead would never converge.
        """
        round_id = engine.db.start_round("test")
        entry = self._chain_entry()
        _by_source, fps = engine._record_chain_failures(self._cfg(), round_id, [entry], _nolog)
        # A sibling that WAS dialled, so the source is in scope for the prune.
        tested = {"source": "air", "fp": "b" * 16, "original": "M", "mihomo": "M",
                  "test_ip": "m.example"}
        empty = collections.defaultdict(set)
        engine._prune_removed_nodes({"M": tested}, {}, empty, _nolog)
        self.assertIsNone(engine.db.get_node("air", entry["fp"]))
        engine._record_chain_failures(self._cfg(), round_id, [entry], _nolog)
        engine._prune_removed_nodes({"M": tested}, {}, empty, _nolog, chain_failed=fps)
        self.assertIsNotNone(engine.db.get_node("air", entry["fp"]))

    def test_a_chain_that_works_through_one_front_is_alive(self):
        cfg = self._cfg()
        entry = self._chain_entry(role="chain")
        by_name = {"N": entry, "N #2": entry}
        results = {"N": self._res(), "N #2": self._res(reason="timeout", delay_ms=None)}
        proxies = [
            {"name": "N", "type": "vless", "server": "t.example", "port": 443,
             "dialer-proxy": "__FRONT0__"},
            {"name": "N #2", "type": "vless", "server": "t.example", "port": 443,
             "dialer-proxy": "__FRONT1__"},
        ]
        round_id = engine.db.start_round("test")
        summary = engine._apply_and_publish(cfg, round_id, None, by_name, proxies,
                                            results, {}, cfg["sources"], _nolog)
        self.assertEqual(summary["alive"], 1)
        self.assertEqual(engine.db.get_node("air", "a" * 16)["status"], policy.ALIVE)

    def test_a_chain_that_no_front_carries_is_dead(self):
        cfg = self._cfg()
        entry = self._chain_entry(role="chain")
        by_name = {"N": entry, "N #2": entry}
        results = {"N": self._res(reason="timeout", delay_ms=None),
                   "N #2": self._res(reason="timeout", delay_ms=None)}
        proxies = [
            {"name": "N", "type": "vless", "server": "t.example", "port": 443,
             "dialer-proxy": "__FRONT0__"},
            {"name": "N #2", "type": "vless", "server": "t.example", "port": 443,
             "dialer-proxy": "__FRONT1__"},
        ]
        round_id = engine.db.start_round("test")
        summary = engine._apply_and_publish(cfg, round_id, None, by_name, proxies,
                                            results, {}, cfg["sources"], _nolog)
        self.assertEqual(summary["alive"], 0)
        self.assertEqual(engine.db.get_node("air", "a" * 16)["consec_fail"], 1)

    def test_a_front_is_counted_but_never_exported(self):
        cfg = self._cfg()
        entry = {"source": engine.FRONT_SOURCE_KEY, "fp": "f" * 16,
                 "original": "edgetunnel", "mihomo": "__FRONT0__",
                 "test_ip": "f.example", "role": "front"}
        proxies = [{"name": "__FRONT0__", "type": "vless", "server": "f.example",
                    "port": 443}]
        round_id = engine.db.start_round("test")
        summary = engine._apply_and_publish(
            cfg, round_id, None, {"__FRONT0__": entry}, proxies,
            {"__FRONT0__": self._res()}, {}, cfg["sources"], _nolog)
        self.assertEqual(summary["alive"], 1)
        self.assertEqual(summary["total"], 1)
        self.assertFalse((self.tmp / "exports" / f"{engine.FRONT_SOURCE_KEY}.yaml").exists())


class FrontPoolInputsTest(unittest.TestCase):
    """The three ways to fill the front pool: resource, pick list, pasted text.

    The pool is capped by `max_fronts` and the pasted entries come first, so
    these pin the *composition* rule, not just each input on its own -- the
    bug this feature could reintroduce is a second input silently overriding
    the first, or the cap being applied per input instead of to the result.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.old_data, self.old_export = cfgmod.DATA, engine.EXPORT_DIR
        self.old_path, self.old_conn = engine.db.DB_PATH, engine.db._conn
        cfgmod.DATA = self.tmp
        engine.db.DB_PATH, engine.db._conn = self.tmp / "s.db", None
        engine.EXPORT_DIR = self.tmp / "exports"
        engine.db.connect()
        # The manual pool is materialised once per process and skipped when the
        # text is unchanged, so a test that leaves the cache dirty makes the
        # *next* test's upsert invisible. Reset both between cases.
        self._synced, self._cleaned = dict(engine._MANUAL_FRONT_SYNCED), set(engine._MANUAL_FRONT_CLEANED)
        engine._MANUAL_FRONT_SYNCED.clear()
        engine._MANUAL_FRONT_CLEANED.clear()

    def tearDown(self):
        if engine.db._conn is not None:
            engine.db._conn.close()
        engine.db.DB_PATH, engine.db._conn = self.old_path, self.old_conn
        cfgmod.DATA, engine.EXPORT_DIR = self.old_data, self.old_export
        engine._MANUAL_FRONT_SYNCED.clear()
        engine._MANUAL_FRONT_SYNCED.update(self._synced)
        engine._MANUAL_FRONT_CLEANED.clear()
        engine._MANUAL_FRONT_CLEANED.update(self._cleaned)
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _cfg(self, enabled=True, front_name="cm-xhttp", **chain):
        block = {"enabled": enabled, "max_fronts": 8,
                 "front_source": {"kind": "sub", "name": front_name}}
        block.update(chain)
        return cfgmod.normalize_chain(block)

    def _pool(self, proxies, manual=(), **chain):
        cfg = {"publish": {"prefix": "probe"}, "chain": self._cfg(**chain)}
        store = _FakeFrontStore(proxies, manual=manual)
        return engine.collect_fronts(cfg, store, _nolog), store

    # ------------------------------------------------------------- normalize

    def test_normalize_keeps_pick_order_and_drops_duplicates(self):
        out = cfgmod.normalize_chain({"front_pick": [" b ", "a", "b", "", None, "a"]})
        self.assertEqual(out["front_pick"], ["b", "a"])
        self.assertEqual(cfgmod.normalize_chain({"front_pick": "nope"})["front_pick"], [])
        self.assertEqual(cfgmod.normalize_chain(None)["front_text"], "")

    def test_normalize_truncates_a_runaway_paste(self):
        out = cfgmod.normalize_chain({"front_text": "x" * (cfgmod.MAX_FRONT_TEXT + 10)})
        self.assertEqual(len(out["front_text"]), cfgmod.MAX_FRONT_TEXT)

    def test_an_oversized_paste_is_refused_not_truncated(self):
        # Truncating a base64 body yields something that still parses, into
        # garbage nodes -- so the failure would read as "the front pool is
        # empty" with nothing pointing at the paste.
        clean, notes = cfgmod.validate_patch(
            {"chain": {"front_text": "x" * (cfgmod.MAX_FRONT_TEXT + 10)}})
        self.assertNotIn("front_text", clean.get("chain", {}))
        self.assertTrue(any("chain.front_text" in n for n in notes))

    # ----------------------------------------------------------- chain_block

    def test_pasted_text_alone_counts_as_a_configured_pool(self):
        # Requiring `front_source` would make a manual-only pool read as
        # "chaining is off" and silently turn 链式测活 into a direct round.
        self.assertIsNotNone(engine.chain_block({"chain": self._cfg(front_name="", front_text="vless://x")}))
        self.assertIsNone(engine.chain_block({"chain": self._cfg(front_name="", front_text="   ")}))
        self.assertIsNone(engine.chain_block({"chain": self._cfg(enabled=False, front_text="vless://x")}))

    # --------------------------------------------------------- manual_fronts

    def test_pasted_text_is_written_once_and_reused(self):
        cfg = {"publish": {"prefix": "probe"},
               "chain": self._cfg(front_name="", front_text="vless://one")}
        store = _FakeFrontStore([], manual=[{"name": "M", "type": "vless",
                                            "server": "m.example"}])
        self.assertEqual([f["name"] for f in engine.collect_fronts(cfg, store, _nolog)], ["M"])
        self.assertEqual(len(store.writes), 1)
        self.assertEqual(store.writes[0][1], "probe-front-manual")
        self.assertEqual(store.writes[0][2]["content"], "vless://one")
        # unchanged text -> no second write
        engine.collect_fronts(cfg, store, _nolog)
        self.assertEqual(len(store.writes), 1)
        # changed text -> written again
        cfg["chain"]["front_text"] = "vless://two"
        engine.collect_fronts(cfg, store, _nolog)
        self.assertEqual(len(store.writes), 2)
        self.assertEqual(store.writes[1][2]["content"], "vless://two")

    def test_a_failed_manual_write_does_not_take_the_resource_down(self):
        cfg = {"publish": {"prefix": "probe"},
               "chain": self._cfg(front_text="vless://one")}
        store = _FakeFrontStore([{"name": "R", "type": "vless", "server": "r.example"}],
                                manual=[{"name": "M", "type": "vless", "server": "m.example"}],
                                write_error=StoreError("boom"))
        # The manual half is unavailable, but the resource half still is: one
        # broken input must not empty a pool the other input can fill. And a
        # write that failed must not be remembered as done, or the next round
        # would skip the retry and the pool would stay empty forever.
        pool = engine.collect_fronts(cfg, store, _nolog)
        self.assertEqual([f["name"] for f in pool], ["R"])
        self.assertEqual(engine._MANUAL_FRONT_SYNCED, {})

    def test_clearing_the_paste_removes_the_materialised_sub(self):
        cfg = {"publish": {"prefix": "probe"},
               "chain": self._cfg(front_name="", front_text="vless://one")}
        store = _FakeFrontStore([], manual=[{"name": "M", "type": "vless",
                                            "server": "m.example"}])
        engine.collect_fronts(cfg, store, _nolog)
        cfg["chain"]["front_text"] = ""
        engine.collect_fronts(cfg, store, _nolog)
        self.assertEqual(store.deleted, [("sub", "probe-front-manual")])

    # --------------------------------------------------------- pool assembly

    def test_pasted_fronts_come_first_and_are_capped_with_the_rest(self):
        resource = [_front_proxy("R0"), _front_proxy("R1"), _front_proxy("R2")]
        manual = [{"name": "M0", "type": "vless", "server": "m.example"}]
        pool, _ = self._pool(resource, manual=manual, max_fronts=2,
                             front_text="vless://one")
        # The pasted list is the explicit, just-typed choice; the resource is
        # the standing default. A cap of 2 therefore keeps the pasted one plus
        # exactly one resource entry -- not two of either.
        self.assertEqual([f["name"] for f in pool], ["M0", "R0"])

    def test_pick_narrows_the_resource_instead_of_supplementing_it(self):
        resource = [_front_proxy("R0"), _front_proxy("R1"), _front_proxy("R2")]
        pool, _ = self._pool(resource, front_pick=["R2"])
        self.assertEqual([f["name"] for f in pool], ["R2"])

    def test_a_pick_list_without_a_source_is_ignored_not_fatal(self):
        pool, _ = self._pool([], front_name="", front_pick=["R2"])
        self.assertEqual(pool, [])

    def test_every_front_keeps_the_reserved_kernel_name(self):
        resource = [_front_proxy("R0")]
        manual = [{"name": "M0", "type": "vless", "server": "m.example"}]
        pool, _ = self._pool(resource, manual=manual, front_text="vless://one")
        self.assertEqual([f["proxy"]["name"] for f in pool],
                         [f"{engine.FRONT_NAME_PREFIX}0__", f"{engine.FRONT_NAME_PREFIX}1__"])
        self.assertTrue(all(f["category"] == engine.CAT_RELAY for f in pool))


class FrontPoolUiTest(unittest.TestCase):
    """The front-pool inputs must reach the panel, not only the config schema.

    A config key nothing renders is a switch the operator believes in and
    cannot use, and the reverse is worse here: a picker whose selection is not
    sent back leaves the pool reading as "all nodes" while the panel shows a
    ticked subset.

    The panel script moved out of `ui.py` into the static `web/app.js` so the
    same front end can be deployed to a CDN. The assertions are about *what the
    panel sends*, not about how it is indented, so they run against a
    whitespace-collapsed copy -- pinning exact spacing made them fail on a pure
    reformat, which says nothing about the behaviour they exist to protect.
    """

    UI = (Path(__file__).resolve().parent.parent
          / "mihomo_test" / "web" / "app.js")

    @staticmethod
    def _squash(text):
        return re.sub(r"\s+", "", text)

    def _source(self):
        return self._squash(self.UI.read_text(encoding="utf-8"))

    def test_settings_render_both_new_inputs(self):
        src = self._source()
        for anchor in ('id="s-chain-text"', 'id="btn-front-pick"',
                       'id="btn-front-pick-clear"', 'id="front-picker"',
                       'id="front-pick-info"'):
            self.assertIn(anchor, src)

    def test_saving_sends_the_pick_list_and_the_paste(self):
        src = self._source()
        start = src.index("chain:(()=>{")
        body = src[start:src.index("publish:{", start)]
        # `front_pick` has to be sent even when empty: the config merge is per
        # key, so omitting it would leave the previous list in place and make
        # 清空名单 silently do nothing.
        self.assertIn("front_pick:[...FRONT_PICK]", body)
        self.assertIn('front_text:val("s-chain-text")', body)

    def test_the_picker_reads_its_nodes_from_the_server(self):
        src = self._source()
        self.assertIn("/api/substore-nodes?", src)
        # Seeded from the server config, so a rejected save cannot leave the
        # panel claiming a selection that was never stored.
        self.assertIn("newSet(((cfg.chain||{}).front_pick)||[])", src)


class RejectedNodeTest(unittest.TestCase):
    """A node the kernel refuses must not be published as alive.

    Found live: one `gammasub` node carried `short-id: 123456e2`, which mihomo
    rejects with "invalid REALITY short ID". `core.make_testable` pruned it and
    logged the prune, but the pruning never reached the ledger -- so the node
    kept the previous round's `alive`, was written to `gammasub.yaml`, and the
    published file failed to load:

        proxy 147: invalid REALITY short ID
        configuration file test failed

    Same shape as the dangling `dialer-proxy`: the round succeeded and the
    export was unusable.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)

    def _cfg(self):
        return {
            "policy": {"drop_after_consecutive_fails": 3},
            "publish": {"add_region_tag": False, "enabled": True},
        }

    def _entry(self, name, server="s.example"):
        proxy = {"name": name, "type": "vless", "server": server, "port": 443, "uuid": "u"}
        return {"source": "k", "name": name, "proxy": proxy,
                "index": 0, "fp": engine._orig_fp(proxy)}

    def test_a_rejected_node_is_failed_and_kept_out_of_the_export(self):
        entry = self._entry("bad")
        rejected = [entry]
        alive_by_source = {"k": ["bad", "good"]}
        # The publish-path guard is what this asserts: `alive_by_source` is the
        # only thing `_write_export` reads, so a node failed in the ledger but
        # still listed here would ship anyway.
        names = {e["name"] for e in rejected}
        trimmed = {k: [n for n in v if n not in names]
                   for k, v in alive_by_source.items()}
        self.assertEqual(trimmed["k"], ["good"])

    def test_recording_a_rejection_advances_the_failure_streak(self):
        entry = self._entry("bad")
        cfg = self._cfg()
        with unittest.mock.patch.object(engine, "db") as mock_db:
            mock_db.get_node.return_value = {
                "source": "k", "fingerprint": entry["fp"], "consec_fail": 0,
                "status": "alive", "total_ok": 5, "total_fail": 0}
            by_source, fps = engine._record_rejected_nodes(cfg, 1, [entry], _nolog)
        self.assertEqual(sorted(fps["k"]), [entry["fp"]])
        self.assertEqual(by_source["k"], [entry])
        fields = mock_db.upsert_node.call_args.kwargs
        self.assertEqual(fields["last_reason"], engine.REJECTED_REASON)
        self.assertEqual(fields["consec_fail"], 1)

    def test_rejected_fingerprints_count_as_seen_so_the_prune_spares_them(self):
        # These entries are absent from `by_name` (nothing was dialled), so
        # without being passed here the prune would delete the very streak the
        # rejection just advanced.
        entry = self._entry("bad")
        with unittest.mock.patch.object(engine, "db") as mock_db:
            mock_db.delete_nodes_not_in.return_value = 0
            engine._prune_removed_nodes({}, {}, {}, _nolog,
                                        rejected={"k": {entry["fp"]}})
        mock_db.delete_nodes_not_in.assert_called_once_with("k", [entry["fp"]])

    def test_a_rejected_entry_is_matched_back_by_name(self):
        # `make_testable` reports `{name, why}` because `prepare` sees proxies,
        # not entries; the caller has to reattach source and fp.
        entries = [self._entry("bad"), self._entry("good", "g.example")]
        dropped = [{"name": "bad", "why": "kernel config error"}]
        names = {i["name"] for i in dropped}
        picked = [e for e in entries if e["name"] in names]
        self.assertEqual([e["name"] for e in picked], ["bad"])
        self.assertEqual(picked[0]["source"], "k")

    def test_a_rejection_carries_the_entry_category_through(self):
        """A rejected node must land in its own bucket, not in 直连.

        `record_result`'s `category` is what `stats_by_category` groups on, and
        a NULL there reads as `direct`. A rejected *chained* node would then be
        counted against 直连 in both units -- the panel's 链式 failures would
        under-report and 直连's would over-report, for a node that has nothing
        to do with direct dialling.
        """
        entry = self._entry("bad")
        entry["category"] = engine.CAT_CHAIN
        with unittest.mock.patch.object(engine, "db") as mock_db:
            mock_db.get_node.return_value = {
                "source": "k", "fingerprint": entry["fp"], "consec_fail": 0,
                "status": "alive", "total_ok": 1, "total_fail": 0}
            engine._record_rejected_nodes(self._cfg(), 1, [entry], _nolog)
        self.assertEqual(mock_db.record_result.call_args.kwargs.get("category"),
                         engine.CAT_CHAIN,
                         "a rejected chained node was recorded without a category")
        self.assertEqual(mock_db.upsert_node.call_args.kwargs.get("category"),
                         engine.CAT_CHAIN)

    def test_a_rejected_entry_without_a_category_defaults_to_direct(self):
        """Entries built by hand carry no category; direct is the safe reading."""
        entry = self._entry("bad")
        with unittest.mock.patch.object(engine, "db") as mock_db:
            mock_db.get_node.return_value = {
                "source": "k", "fingerprint": entry["fp"], "consec_fail": 0,
                "status": "alive", "total_ok": 1, "total_fail": 0}
            engine._record_rejected_nodes(self._cfg(), 1, [entry], _nolog)
        self.assertEqual(mock_db.record_result.call_args.kwargs.get("category"),
                         engine.CAT_DIRECT)


class ChainFailureCategoryTest(unittest.TestCase):
    """`_record_chain_failures` always writes a chain verdict.

    Its entries are the chained ones a dead front pool killed -- the path exists
    precisely so they are *not* dialled. Recording them without a category put
    every such failure in 直连's tally.
    """

    def _entry(self, name):
        proxy = {"name": name, "type": "vmess", "server": "s.example",
                 "dialer-proxy": "g"}
        return {"source": "s", "name": name, "original": name,
                "proxy": proxy, "fp": engine._orig_fp(proxy),
                "proto": "vmess", "server": "s.example",
                "category": engine.CAT_CHAIN}

    def test_a_front_dead_chain_is_recorded_as_a_chain(self):
        entry = self._entry("c1")
        cfg = {"policy": {"drop_after_consecutive_fails": 3}}
        with unittest.mock.patch.object(engine, "db") as mock_db:
            mock_db.get_node.return_value = {
                "source": "s", "fingerprint": entry["fp"], "consec_fail": 0,
                "status": "alive", "total_ok": 1, "total_fail": 0}
            engine._record_chain_failures(cfg, 1, [entry], _nolog)
        self.assertEqual(mock_db.record_result.call_args.kwargs.get("category"),
                         engine.CAT_CHAIN,
                         "a chain failure was recorded without the chain category")
        self.assertEqual(mock_db.upsert_node.call_args.kwargs.get("category"),
                         engine.CAT_CHAIN)
        self.assertEqual(mock_db.record_result.call_args.args[6],
                         engine.FRONT_DEAD_REASON)


class CategoryClassificationTest(unittest.TestCase):
    """The three-way split the 分类统计 panel reports on.

    `direct` / `relay` / `chain` is a property of the node plus one flag on its
    source, and it has to be decided *once*, in `collect_entries`, while that
    flag is still in hand -- `core.prepare` only ever sees proxies, and a proxy
    alone cannot tell a marked transit hop from an ordinary node.
    """

    def _proxy(self, **extra):
        return {"name": "n", "type": "vmess", "server": "a.example", **extra}

    def test_a_plain_node_is_direct(self):
        self.assertEqual(engine.classify_category(self._proxy()), engine.CAT_DIRECT)

    def test_a_node_from_a_relay_source_is_a_relay(self):
        self.assertEqual(
            engine.classify_category(self._proxy(), source_relay=True),
            engine.CAT_RELAY)

    def test_dialer_proxy_makes_it_a_chain(self):
        self.assertEqual(
            engine.classify_category(self._proxy(**{"dialer-proxy": "g"})),
            engine.CAT_CHAIN)

    def test_both_spellings_of_dialer_proxy_are_recognised(self):
        """mihomo normalises `_` to `-`; upstream subscriptions use both."""
        self.assertEqual(
            engine.classify_category(self._proxy(dialer_proxy="g")),
            engine.CAT_CHAIN)

    def test_relay_beats_chain(self):
        """A marked transit hop that also carries `dialer-proxy` is a relay.

        Reporting it as a chain would double-count it -- it is already one of
        the fronts the chain is measured through -- and the chain total would
        stop agreeing with the front pool.
        """
        self.assertEqual(
            engine.classify_category(self._proxy(dialer_proxy="g"), source_relay=True),
            engine.CAT_RELAY)

    def test_labels_cover_every_category(self):
        for cat in engine.CATEGORIES:
            self.assertIn(cat, engine.CATEGORY_LABELS)
        self.assertEqual(engine.CATEGORIES,
                         (engine.CAT_DIRECT, engine.CAT_RELAY, engine.CAT_CHAIN))

    def test_expand_chains_leaves_a_relay_alone(self):
        """A relay is already the hop a chain dials through.

        Giving it a dialer-proxy would invent a topology this deployment does
        not have, and because the ledger folds variants by fingerprint, the
        variant's `chain` stamp would silently overwrite the `relay` one -- the
        panel would then report the front pool as chain traffic.
        """
        relay = {"source": "cdn前置", "name": "F", "category": engine.CAT_RELAY,
                 "proxy": self._proxy(**{"dialer-proxy": "g"})}
        chain = {"source": "链式聚合", "name": "C", "category": engine.CAT_CHAIN,
                 "proxy": self._proxy(**{"dialer-proxy": "g"})}
        plain = {"source": "gammasub", "name": "D", "category": engine.CAT_DIRECT,
                 "proxy": self._proxy()}

        out, chained = engine.expand_chains([relay, chain, plain], ["PF0__", "PF1__"])
        by_name = {}
        for entry in out:
            by_name.setdefault(entry["name"], []).append(entry)

        self.assertEqual(len(by_name["F"]), 1, "the relay node was expanded")
        self.assertEqual(by_name["F"][0]["category"], engine.CAT_RELAY)
        self.assertEqual(len(by_name["C"]), 2, "the chain node should get one per front")
        self.assertTrue(all(e["category"] == engine.CAT_CHAIN for e in by_name["C"]))
        self.assertEqual(len(by_name["D"]), 1, "a direct node must pass through")
        self.assertEqual(by_name["D"][0]["category"], engine.CAT_DIRECT)
        self.assertEqual(chained, 1, "only the chain node should count as chained")

    def test_expand_chains_keeps_each_variant_on_the_original_fingerprint(self):
        """Variants must fold into one ledger row, or the node is over-counted."""
        entry = {"source": "s", "name": "C", "category": engine.CAT_CHAIN,
                 "fp": "fp-c", "proxy": self._proxy(**{"dialer-proxy": "g"})}
        out, _ = engine.expand_chains([entry], ["PF0__", "PF1__", "PF2__"])
        self.assertEqual(len(out), 3)
        self.assertEqual({e["fp"] for e in out}, {"fp-c"})
        self.assertEqual({e["category"] for e in out}, {engine.CAT_CHAIN})


class ChainRoundTest(unittest.TestCase):
    """End-to-end wiring for a whole round with chaining on.

    The unit tests above cover each piece in isolation; this covers the joins --
    the order of collect/classify/expand, `keep_dialer` actually reaching
    `prepare`, and the front pool surviving the round's ledger hygiene. Those
    joins are where a feature like this really breaks, and no unit test would
    notice a `keep_dialer` that was computed and then never passed.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.old = (cfgmod.DATA, cfgmod.CORE_DIR, cfgmod.CORE_SECRET_PATH,
                    engine.EXPORT_DIR, engine.db.DB_PATH, engine.db._conn)
        cfgmod.DATA = self.tmp
        cfgmod.CORE_DIR = self.tmp / "core"
        # Bound at import time from DATA, so patching DATA alone is not enough.
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

    class _RoundCore:
        """Fake kernel that answers for the front and for anything chained."""

        def __init__(self, core_cfg, secret):
            self.seen = []

        def start_and_load(self, log=None):
            return "started"

        def delay(self, name, url, timeout_ms, expected):
            self.seen.append(name)
            return (30, None, "") if name.startswith(engine.FRONT_NAME_PREFIX) \
                else (99, None, "")

        def select(self, group, name):
            pass

        def egress(self, port, trace_url, timeout_s=15):
            return {"loc": "US", "ip": "1.2.3.4"}, None

    def _cfg(self):
        return {
            "substore": {"backend": "http://127.0.0.1:3000"},
            "core": {"api": "http://127.0.0.1:19190", "lanes": 2, "base_port": 19300,
                     "container": "mihomo-probe",
                     # Read with a direct subscript by `build_config` and absent
                     # from DEFAULTS -- this is the deployment-specific override
                     # config.py's DEAD_KEYS comment warns about.
                     "mixed_port": 19194,
                     "container_config_path": "/root/.config/mihomo/config.yaml"},
            "sources": [{"key": "air", "kind": "sub", "name": "air",
                         "label": "air", "enabled": True}],
            "chain": {"enabled": True, "max_fronts": 4,
                      "front_source": {"kind": "sub", "name": "cm-xhttp"}},
            "test": {"targets": ["http://probe/generate_204"], "expected_status": "204",
                     "timeout_ms": 100, "timeout_ms_retry": 100, "max_attempts": 1,
                     "retry_pause_s": 0, "concurrency": 2},
            "dns": {"views": {}, "timeout_s": 1, "cache_hours": 0},
            "verify": {"enabled": False, "entry_check": True,
                       "exclude_entry_countries": ["CN"],
                       "exclude_countries": ["CN"]},
            "policy": {"drop_after_consecutive_fails": 3,
                       "suspect_floor_ratio": 0.5, "suspect_floor_absolute": 3},
            "schedule": {"interval_minutes": 30, "enabled": False},
            "publish": {"enabled": True, "prefix": "probe", "add_region_tag": False},
            "watchdog": {"round_timeout_minutes": 20},
            "alert": {"enabled": False},
        }

    def _store(self):
        node = {"name": "N", "type": "vless", "server": "t.example", "port": 443,
                "uuid": "u", "dialer-proxy": "hk_b"}
        front = _front_proxy("edgetunnel")

        class _Store:
            def __init__(self, backend):
                pass

            def fetch_source(self, kind, name, target="ClashMeta"):
                return [dict(front)] if name == "cm-xhttp" else [dict(node)]

            def fetch_sub_proxies(self, name, target="ClashMeta"):
                return []

            def delete(self, kind, name):
                raise NotFound(name)

        return _Store

    def _run(self):
        cfg = self._cfg()
        store_cls = self._store()
        core_cls = self._RoundCore
        with unittest.mock.patch.object(engine, "Client", store_cls), \
                unittest.mock.patch.object(coremod, "Core", core_cls), \
                unittest.mock.patch.object(coremod, "config_test",
                                           return_value=(True, "")), \
                unittest.mock.patch.object(engine, "_resolve_candidates",
                                           return_value=["1.2.3.4"]), \
                unittest.mock.patch.object(engine, "lookup_countries",
                                           return_value={"1.2.3.4": "US"}), \
                unittest.mock.patch.object(notifier, "send", lambda *a, **k: None):
            return engine._run_round(cfg, "test", None, _nolog)

    def test_the_round_builds_the_chain_into_the_kernel_config(self):
        self._run()
        text = (cfgmod.CORE_DIR / "config.yaml").read_text(encoding="utf-8")
        self.assertIn('"dialer-proxy": "__FRONT0__"', text)
        self.assertIn('"name": "__FRONT0__"', text)
        # The node's own address is what gets dialled -- through the front.
        self.assertIn('"server": "1.2.3.4"', text)

    def test_the_export_leaks_no_probe_internal_names(self):
        """`__FRONT0__` is the probe's own reservation and must never ship.

        This used to assert `dialer-proxy: hk_b` was kept, on the theory that
        the upstream author's own group name should survive publishing. The
        real kernel rejects that file: the live `air` collection downloads as
        `proxies` with no `proxy-groups` at all, so `hk_b` (like the `cdn` this
        deployment actually hits) names nothing a client can resolve --
        `dialer-proxy [hk_b] not found`, whole file refused. Keeping the name
        was preserving the one thing that made the export unusable.

        The pool's one live front is published under its `[前置] ` tag and the
        dialer names it directly -- no group for Sub-Store to drop.
        """
        self._run()
        text = (engine.EXPORT_DIR / "air.yaml").read_text(encoding="utf-8")
        self.assertNotIn(engine.FRONT_NAME_PREFIX, text)
        doc = yaml.safe_load(text)
        dialers = {p["dialer-proxy"] for p in doc["proxies"] if "dialer-proxy" in p}
        self.assertEqual(dialers, {"[前置] edgetunnel"})
        names = {p["name"] for p in doc["proxies"]}
        self.assertIn("[前置] edgetunnel", names)
        self.assertNotIn("proxy-groups", doc)

    def test_the_front_gets_ledger_rows_and_no_export_of_its_own(self):
        summary = self._run()
        self.assertIsNotNone(engine.db.get_node(engine.FRONT_SOURCE_KEY, _front_fp()))
        # Three, not two: a chained node is measured both ways by default --
        # once per front (chain) and once with its dialer stripped (direct) --
        # and the two variants deliberately carry different fingerprints, so
        # they are two ledger rows rather than one overwriting the other.
        self.assertEqual(summary["alive"], 3)      # front + chain variant + direct variant
        self.assertFalse((engine.EXPORT_DIR / f"{engine.FRONT_SOURCE_KEY}.yaml").exists())

    def test_a_dead_front_fails_the_chain_with_front_dead(self):
        class _DeadFrontCore(self._RoundCore):
            def delay(self, name, url, timeout_ms, expected):
                self.seen.append(name)
                return (None, "timeout", "Timeout") if name.startswith(engine.FRONT_NAME_PREFIX) \
                    else (99, None, "")

        cfg = self._cfg()
        store_cls = self._store()
        with unittest.mock.patch.object(engine, "Client", store_cls), \
                unittest.mock.patch.object(coremod, "Core", _DeadFrontCore), \
                unittest.mock.patch.object(coremod, "config_test",
                                           return_value=(True, "")), \
                unittest.mock.patch.object(engine, "_resolve_candidates",
                                           return_value=["1.2.3.4"]), \
                unittest.mock.patch.object(engine, "lookup_countries",
                                           return_value={"1.2.3.4": "US"}), \
                unittest.mock.patch.object(notifier, "send", lambda *a, **k: None):
            engine._run_round(cfg, "test", None, _nolog)

        # The chained variant specifically: its direct twin is alive (the dialer
        # is stripped, so the dead front never enters its path) and would
        # otherwise win the race in `next()` depending on ledger order.
        node = next(n for n in engine.db.list_nodes(source="air")
                    if n["category"] == engine.CAT_CHAIN)
        self.assertEqual(node["last_reason"], engine.FRONT_DEAD_REASON)
        self.assertEqual(node["consec_fail"], 1)

    def test_an_empty_front_pool_fails_the_chain_and_still_alerts(self):
        """The pool never arrived at all -- the shape no unit test covered.

        The round logs 「这些节点本轮全部判失败」 when `collect_fronts` comes
        back empty, and it used to do the opposite: `expand_chains` returned
        early, `keep_dialer` was `[]`, `prepare` stripped the dialer, and the
        chained node was measured as its own server. A node reachable only
        through its front then read `timeout` instead of `front_dead` (so the
        streak needed extra rounds to converge), and one that happened to be
        reachable direct was published as alive on a path its owner never
        uses. The alert never fired either, because it was keyed on
        `bool(fronts)`.
        """
        holder = {}
        alerts = []

        class _NoPoolStore:
            def __init__(self, backend):
                pass

            def fetch_source(self, kind, name, target="ClashMeta"):
                if name == "cm-xhttp":
                    return []
                return [{"name": "N", "type": "vless", "server": "t.example",
                         "port": 443, "uuid": "u", "dialer-proxy": "hk_b"}]

            def fetch_sub_proxies(self, name, target="ClashMeta"):
                return []

            def delete(self, kind, name):
                raise NotFound(name)

        class _SpyCore(self._RoundCore):
            def __init__(self, core_cfg, secret):
                super().__init__(core_cfg, secret)
                holder["core"] = self

        with unittest.mock.patch.object(engine, "Client", _NoPoolStore), \
                unittest.mock.patch.object(coremod, "Core", _SpyCore), \
                unittest.mock.patch.object(coremod, "config_test",
                                           return_value=(True, "")), \
                unittest.mock.patch.object(engine, "_resolve_candidates",
                                           return_value=["1.2.3.4"]), \
                unittest.mock.patch.object(engine, "lookup_countries",
                                           return_value={"1.2.3.4": "US"}), \
                unittest.mock.patch.object(
                    notifier, "send",
                    lambda *a, **k: alerts.append((a, k))):
            summary = engine._run_round(self._cfg(), "test", None, _nolog)

        dialed = holder["core"].seen
        # Exactly one dial: the direct twin. The chain-role orphan shares the
        # kernel config with it but must never reach `delay`.
        self.assertEqual(len(dialed), 1, f"链式孤儿被拨号了: {dialed}")
        self.assertFalse(any(n.startswith(engine.FRONT_NAME_PREFIX) for n in dialed))

        nodes = engine.db.list_nodes(source="air")
        chain_node = next(n for n in nodes if n["category"] == engine.CAT_CHAIN)
        self.assertEqual(chain_node["last_reason"], engine.FRONT_DEAD_REASON)
        self.assertEqual(chain_node["consec_fail"], 1)
        direct_node = next(n for n in nodes if n["category"] == engine.CAT_DIRECT)
        self.assertIsNone(direct_node["last_reason"])
        self.assertEqual(summary["alive"], 1)

        fired = [a for a, _k in alerts if len(a) > 1 and a[1] == "front_dead"]
        self.assertEqual(len(fired), 1, f"空前置池没有告警: {[a[1] for a, _ in alerts]}")
        self.assertIn("前置池为空", fired[0][3])


def _front_fp():
    """Fingerprint of the fixture front, as `collect_fronts` computes it."""
    return engine._orig_fp(_front_proxy("edgetunnel"))


class ApplyAndPublishTest(unittest.TestCase):
    """The converge-then-publish step: scoring, guardrail, and export writes.

    These branches are the ones the `_apply_and_publish` split moved around, and
    they were previously only exercised indirectly by the live suite -- which
    cannot run on a dev machine. Each test here pins one decision that a
    restructuring could silently invert.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.old_data, self.old_export = cfgmod.DATA, engine.EXPORT_DIR
        self.old_path, self.old_conn = engine.db.DB_PATH, engine.db._conn
        self.old_round_state = engine.ROUND_STATE
        cfgmod.DATA = self.tmp
        engine.db.DB_PATH, engine.db._conn = self.tmp / "s.db", None
        # _write_export uses this module global directly, so point it at the
        # same place the real layout uses rather than the temp root.
        engine.EXPORT_DIR = self.tmp / "exports"
        engine.db.connect()

    def tearDown(self):
        if engine.db._conn is not None:
            engine.db._conn.close()
        engine.db.DB_PATH, engine.db._conn = self.old_path, self.old_conn
        cfgmod.DATA, engine.EXPORT_DIR = self.old_data, self.old_export
        engine.ROUND_STATE = self.old_round_state
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _cfg(self, **over):
        cfg = {"policy": {"drop_after_consecutive_fails": 3,
                          "suspect_floor_ratio": 0.5,
                          "suspect_floor_absolute": 3},
               "verify": {"exclude_countries": ["CN"]},
               "publish": {"enabled": True},
               "sources": [{"key": "air", "name": "air"}]}
        cfg.update(over)
        return cfg

    def _entry(self, server="host.example", source="air", name="N", fp="0" * 16):
        return {"source": source, "fp": fp, "original": name, "name": name, "test_ip": server}

    def _proxy(self, server="host.example", name="N"):
        return {"name": name, "type": "vless", "server": server, "port": 443}

    def _run(self, cfg, buckets, proxies, countries=None, excluded_entries=None):
        """Invoke the pipeline with one round and return (summary, round_id).

        The real engine gives every resolved address its own kernel-level name
        (`host__1`, `host__2`) and folds them back by fingerprint. Reproducing
        that here is what makes the `domain_pass` branches reachable: a bucket
        with two outcomes must arrive as two names, or `group_by_fingerprint`
        sees a single-address domain and never exercises the strict path.
        """
        round_id = engine.db.start_round("test")
        by_name, results, proxies = {}, {}, list(proxies)
        for bucket in buckets:
            entry = bucket["entry"]
            names = bucket["names"]
            for i, outcome in enumerate(bucket["outcomes"]):
                # aliases reuse an existing name; extra addresses get a suffix
                name = names[i] if i < len(names) else f"{names[0]}__{i + 1}"
                by_name[name] = entry
                results[name] = outcome
                if not any(p["name"] == name for p in proxies):
                    proxies.append({"name": name, "type": "vless",
                                    "server": entry.get("test_ip", "host.example"),
                                    "port": 443})
        summary = engine._apply_and_publish(
            cfg, round_id, None, by_name, proxies, results, countries or {},
            cfg["sources"], lambda *a: None, excluded_entries=excluded_entries)
        return summary, round_id

    @staticmethod
    def _mk_outcome(reason=None, delay_ms=100, attempts=1, detail=""):
        return {"reason": reason, "delay_ms": delay_ms, "attempts": attempts, "detail": detail}

    @staticmethod
    def _bucket(entry, names=("N",), outcomes=None):
        return {"entry": entry, "names": list(names),
                "outcomes": outcomes or [ApplyAndPublishTest._mk_outcome()]}

    def test_one_address_alive_is_enough_by_default(self):
        """domain_pass=any: a multi-address domain is alive if any address answers."""
        entry = self._entry()
        bucket = self._bucket(entry, names=("N",), outcomes=[
            self._mk_outcome(), self._mk_outcome(reason="timeout", delay_ms=None)])
        summary, _ = self._run(self._cfg(), [bucket], [self._proxy()])
        self.assertEqual(summary["alive"], 1)
        node = engine.db.get_node("air", entry["fp"])
        self.assertEqual(node["status"], policy.ALIVE)
        self.assertEqual((node["ip_alive"], node["ip_total"]), (1, 2))

    def test_domain_pass_all_requires_every_address(self):
        """domain_pass=all: one dead address among many makes the domain dead."""
        cfg = self._cfg(verify={"exclude_countries": ["CN"], "domain_pass": "all"})
        entry = self._entry()
        bucket = self._bucket(entry, outcomes=[
            self._mk_outcome(), self._mk_outcome(reason="timeout", delay_ms=None)])
        summary, _ = self._run(cfg, [bucket], [self._proxy()])
        self.assertEqual(summary["alive"], 0)
        node = engine.db.get_node("air", entry["fp"])
        self.assertNotEqual(node["status"], policy.ALIVE)
        # The recorded reason must name the address that actually failed. Under
        # strict mode a healthy address can still be first in the list, so a
        # naive outcomes[0] would store None here.
        self.assertEqual(node["last_reason"], "timeout")
        self.assertEqual((node["ip_alive"], node["ip_total"]), (1, 2))

    def test_domain_pass_all_passes_when_every_address_answers(self):
        cfg = self._cfg(verify={"exclude_countries": ["CN"], "domain_pass": "all"})
        bucket = self._bucket(self._entry(), outcomes=[self._mk_outcome(), self._mk_outcome()])
        summary, _ = self._run(cfg, [bucket], [self._proxy()])
        self.assertEqual(summary["alive"], 1)

    def test_alive_node_is_reported_with_its_minimum_delay(self):
        """The published latency is the best of the addresses, not the first."""
        bucket = self._bucket(self._entry(), names=("N",), outcomes=[
            self._mk_outcome(delay_ms=310), self._mk_outcome(delay_ms=120)])
        summary, round_id = self._run(self._cfg(), [bucket], [self._proxy()])
        row = engine.db.one("SELECT delay_ms FROM results WHERE round_id=?", (round_id,))
        self.assertEqual(row["delay_ms"], 120)

    def test_refused_exit_country_turns_a_pass_into_a_failure(self):
        """A node that answers but egresses in an excluded country must not be alive."""
        bucket = self._bucket(self._entry())
        summary, round_id = self._run(
            self._cfg(), [bucket], [self._proxy()],
            countries={"N": {"country": "CN"}})
        self.assertEqual(summary["alive"], 0)
        node = engine.db.get_node("air", "0" * 16)
        self.assertEqual(node["last_reason"], "exit_CN")
        row = engine.db.one("SELECT verdict, reason FROM results WHERE round_id=?", (round_id,))
        self.assertEqual((row["verdict"], row["reason"]), ("fail", "exit_CN"))

    def test_all_exit_verifications_failing_is_a_verify_failure(self):
        bucket = self._bucket(self._entry())
        _, round_id = self._run(self._cfg(), [bucket], [self._proxy()],
                                countries={"N": {"error": "tls eof"}})
        node = engine.db.get_node("air", "0" * 16)
        self.assertEqual(node["last_reason"], "verify_failed")

    def test_an_allowed_exit_country_is_recorded_and_published(self):
        bucket = self._bucket(self._entry())
        summary, _ = self._run(self._cfg(), [bucket], [self._proxy()],
                               countries={"N": {"country": "US"}})
        self.assertEqual(summary["alive"], 1)
        self.assertEqual(engine.db.get_node("air", "0" * 16)["country"], "US")

    def test_probe_failure_records_the_first_reason(self):
        bucket = self._bucket(self._entry(), outcomes=[
            self._mk_outcome(reason="kernel_error", delay_ms=None, detail="HTTP 503")])
        _, round_id = self._run(self._cfg(), [bucket], [self._proxy()])
        row = engine.db.one("SELECT verdict, reason, detail FROM results WHERE round_id=?",
                            (round_id,))
        self.assertEqual(row["verdict"], "fail")
        self.assertEqual(row["reason"], "kernel_error")
        self.assertEqual(row["detail"], "HTTP 503")

    def test_export_is_written_for_the_source_key(self):
        """The whole point: an alive node must reach exports/<key>.yaml."""
        bucket = self._bucket(self._entry())
        self._run(self._cfg(), [bucket], [self._proxy()])
        path = self.tmp / "exports" / "air.yaml"
        self.assertTrue(path.exists(), "no export was written")
        doc = yaml.safe_load(path.read_text(encoding="utf-8"))
        self.assertEqual(len(doc["proxies"]), 1)
        self.assertEqual(doc["proxies"][0]["server"], "host.example")

    def test_publish_disabled_writes_nothing_and_says_so(self):
        cfg = self._cfg(publish={"enabled": False})
        summary, _ = self._run(cfg, [self._bucket(self._entry())], [self._proxy()])
        self.assertFalse((self.tmp / "exports" / "air.yaml").exists())
        self.assertEqual(summary["note"], "publish disabled")

    def test_suspect_round_keeps_the_previous_export_on_disk(self):
        """The guardrail's entire purpose: never overwrite good output with bad.

        Seeds a healthy export, then converges a round whose alive count trips
        the floor. The old file must survive untouched.
        """
        exports = self.tmp / "exports"
        exports.mkdir(parents=True, exist_ok=True)
        (exports / "air.yaml").write_text("proxies: [{name: OLD}]\n", encoding="utf-8")

        # a previous round with a high alive count sets the floor
        prev = engine.db.start_round("prev")
        engine.db.finish_round(prev, ok=40, total=40, duration_s=1)

        # this round: one alive node, far under 40 * 0.5 and under the abs floor
        cfg = self._cfg(policy={"drop_after_consecutive_fails": 3,
                                "suspect_floor_ratio": 0.5,
                                "suspect_floor_absolute": 3})
        summary, _ = self._run(cfg, [self._bucket(self._entry())], [self._proxy()])
        self.assertTrue(summary["suspect"], "the guardrail should have tripped")
        self.assertIn("not published", summary["note"])
        self.assertEqual((exports / "air.yaml").read_text(encoding="utf-8"),
                         "proxies: [{name: OLD}]\n",
                         "a suspect round overwrote the previous export")

    def test_ledger_and_export_agree_on_which_nodes_are_alive(self):
        """A node marked alive in the ledger must be the one in the export."""
        bucket = self._bucket(self._entry(), names=("N",))
        self._run(self._cfg(), [bucket], [self._proxy()])
        node = engine.db.get_node("air", "0" * 16)
        self.assertEqual(node["status"], policy.ALIVE)
        doc = yaml.safe_load((self.tmp / "exports" / "air.yaml").read_text(encoding="utf-8"))
        self.assertEqual([p["name"] for p in doc["proxies"]], ["N"])

    def test_dropped_node_leaves_the_export(self):
        """Convergence must actually remove a node from the published list."""
        entry = self._entry()
        bucket = self._bucket(entry)
        cfg = self._cfg()
        self._run(cfg, [bucket], [self._proxy()])
        self.assertTrue((self.tmp / "exports" / "air.yaml").exists())

        # three consecutive failures demote it
        for _ in range(3):
            round_id = engine.db.start_round("test")
            engine._apply_and_publish(
                cfg, round_id, None, {"N": entry}, [self._proxy()],
                {"N": self._mk_outcome(reason="timeout", delay_ms=None)},
                {}, cfg["sources"], lambda *a: None)
        self.assertEqual(engine.db.get_node("air", entry["fp"])["status"], policy.DEAD)
        doc = yaml.safe_load((self.tmp / "exports" / "air.yaml").read_text(encoding="utf-8"))
        self.assertEqual(doc["proxies"], [])

    def test_alias_group_advances_the_streak_only_once(self):
        """Two names, one endpoint: one ledger row, one streak step per round."""
        entry = self._entry()
        bucket = self._bucket(entry, names=("N", "N-copy"), outcomes=[
            self._mk_outcome(reason="timeout", delay_ms=None),
            self._mk_outcome(reason="timeout", delay_ms=None)])
        cfg = self._cfg()
        for _ in range(2):
            round_id = engine.db.start_round("test")
            engine._apply_and_publish(
                cfg, round_id, None, {"N": entry, "N-copy": entry},
                [self._proxy()],
                {"N": bucket["outcomes"][0], "N-copy": bucket["outcomes"][1]},
                {}, cfg["sources"], lambda *a: None)
        node = engine.db.get_node("air", entry["fp"])
        self.assertEqual(node["consec_fail"], 2,
                         "aliases advanced the streak more than once per round")


class TimestampTest(unittest.TestCase):
    """Every stored timestamp is UTC, and readers agree with the writer.

    The vps deployment runs the app in a container with TZ=Asia/Shanghai on a
    UTC host, so a localtime stamp in one place and a UTC one in another are
    8 hours apart -- which is how rounds 55/56 got a finished_at that preceded
    their started_at.
    """

    def test_db_now_is_utc(self):
        """db.now() must not depend on the container's TZ."""
        import time as _t
        from mihomo_test import db as dbmod

        stamp = dbmod.now()
        parsed = calendar.timegm(_t.strptime(stamp, "%Y-%m-%dT%H:%M:%S"))
        self.assertLess(abs(parsed - _t.time()), 5,
                        f"db.now() is not UTC: {stamp} vs now {_t.time():.0f}")

    def test_db_now_is_utc_even_under_a_non_utc_tz(self):
        """The defect only shows up when TZ is not UTC, so set one."""
        old_tz = os.environ.get("TZ")
        os.environ["TZ"] = "Asia/Shanghai"
        try:
            import time as _t
            if hasattr(_t, "tzset"):
                _t.tzset()
            from mihomo_test import db as dbmod

            stamp = dbmod.now()
            parsed = calendar.timegm(_t.strptime(stamp, "%Y-%m-%dT%H:%M:%S"))
            self.assertLess(abs(parsed - _t.time()), 5,
                            "db.now() followed the TZ instead of using UTC")
        finally:
            if old_tz is None:
                os.environ.pop("TZ", None)
            else:
                os.environ["TZ"] = old_tz
            import time as _t
            if hasattr(_t, "tzset"):
                _t.tzset()

    def test_to_epoch_round_trips_a_stored_stamp(self):
        import time as _t
        from mihomo_test import db as dbmod

        stamp = dbmod.now()
        self.assertLess(abs(dbmod.to_epoch(stamp) - _t.time()), 5)

    def test_to_epoch_is_zero_for_rubbish(self):
        from mihomo_test import db as dbmod

        for bad in ("", "not-a-time", None, "2026-13-45T99:99:99"):
            with self.subTest(value=bad):
                self.assertEqual(dbmod.to_epoch(bad), 0)

    def test_scheduler_parse_matches_the_writer(self):
        """server._parse must read back what db.now() wrote."""
        import time as _t
        from mihomo_test import db as dbmod
        from mihomo_test import server

        stamp = dbmod.now()
        self.assertLess(abs(server._parse(stamp) - _t.time()), 5,
                        "the scheduler parses timestamps on a different clock")

    def test_policy_stamp_is_utc(self):
        import time as _t
        from mihomo_test import policy

        parsed = calendar.timegm(_t.strptime(policy._stamp({}), "%Y-%m-%dT%H:%M:%S"))
        self.assertLess(abs(parsed - _t.time()), 5, "policy._stamp is not UTC")

    def test_alert_text_label_matches_its_clock(self):
        """The alert footer says UTC; it must actually be UTC."""
        import unittest.mock as mock
        from mihomo_test import notifier

        cfg = {"alert": {"enabled": True, "cooldown_minutes": 1,
                         "telegram": {"enabled": True, "token": "t", "chat_id": "c"},
                         "webhook": {"enabled": False}}}
        captured = {}

        def fake_send(cfg_alert, text):
            captured["text"] = text
            return True, "ok"

        # This temp dir is created here rather than by `_isolation`, so nothing
        # else will ever remove it. `addCleanup` is the right home for it: the
        # patch below only lives for the `with` block, the directory does not.
        _state_dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, _state_dir, True)
        with mock.patch.object(notifier, "telegram_send", fake_send), \
             mock.patch.object(notifier, "STATE_PATH", _state_dir / "s.json"):
            notifier.send(cfg, "probe_key", "题目", "正文", level="warn")
        import time as _t
        utc_hm = _t.strftime("%m-%d %H:%M", _t.gmtime())
        self.assertIn(utc_hm, captured["text"],
                      f"the alert stamped a non-UTC time under a UTC label: "
                      f"{captured['text']!r}")


class AbandonedRoundTest(unittest.TestCase):
    """A round that dies mid-flight must be closed out, not left "in progress"."""

    def test_a_failed_state_write_still_closes_the_round(self):
        """Without this the row stays open forever (`finished_at` NULL).

        `_abandon_round` recovered the round id from round.state.json while
        `_write_state` swallowed a write failure, so a crash on a machine where
        the file could not be written left a ghost round -- and
        `_previous_alive_count` skips unfinished rows, which silently moves the
        guardrail's baseline to a much older round.
        """
        tmp = Path(tempfile.mkdtemp())
        old_data, old_path, old_conn = cfgmod.DATA, engine.db.DB_PATH, engine.db._conn
        old_state = engine.ROUND_STATE
        old_current = engine._CURRENT_ROUND_ID
        cfgmod.DATA = tmp
        engine.db.DB_PATH, engine.db._conn = tmp / "s.db", None
        engine.ROUND_STATE = tmp / "round.state.json"
        try:
            engine.db.connect()
            rid = engine.db.start_round("schedule")
            engine._set_current_round(rid)
            with unittest.mock.patch.object(
                    Path, "write_text", side_effect=OSError("read-only file system")):
                engine._write_state("delay-test", rid)
            # the failure is on the record, not swallowed
            messages = [e["message"] for e in engine.db.query(
                "SELECT message FROM events WHERE level='warn'")]
            self.assertTrue(any("状态文件写入失败" in m for m in messages), messages)

            engine._abandon_round({"alert": {"enabled": False}},
                                  engine._CURRENT_ROUND_ID, "boom",
                                  lambda *a: None, "round_failed", "轮次执行异常")
            row = engine.db.one("SELECT finished_at FROM rounds WHERE id=?", (rid,))
            self.assertIsNotNone(row["finished_at"], "ghost round left open")
        finally:
            if engine.db._conn is not None:
                engine.db._conn.close()
            engine.db.DB_PATH, engine.db._conn = old_path, old_conn
            cfgmod.DATA, engine.ROUND_STATE = old_data, old_state
            engine._set_current_round(old_current)
            shutil.rmtree(tmp, ignore_errors=True)

    def test_abandon_closes_the_round_recorded_in_the_state_file(self):
        tmp = Path(tempfile.mkdtemp())
        old_data, old_path, old_conn = cfgmod.DATA, engine.db.DB_PATH, engine.db._conn
        old_state = engine.ROUND_STATE
        cfgmod.DATA = tmp
        engine.db.DB_PATH, engine.db._conn = tmp / "s.db", None
        engine.ROUND_STATE = tmp / "round.state.json"
        sent = []
        try:
            engine.db.connect()
            rid = engine.db.start_round("schedule")
            engine.ROUND_STATE.write_text(json.dumps(
                {"phase": "delay-test", "round_id": rid, "pid": 1,
                 "ts": "now", "epoch": 1}), encoding="utf-8")
            engine._abandon_round({"alert": {"enabled": False}}, None,
                                  "boom: KeyError", lambda *a: None,
                                  "round_failed", "轮次执行异常")
            row = engine.db.one("SELECT note, finished_at FROM rounds WHERE id=?", (rid,))
            self.assertIsNotNone(row["finished_at"])
            self.assertIn("boom", row["note"])
            self.assertEqual(len(sent), 0)
        finally:
            if engine.db._conn is not None:
                engine.db._conn.close()
            engine.db.DB_PATH, engine.db._conn = old_path, old_conn
            cfgmod.DATA, engine.ROUND_STATE = old_data, old_state
            shutil.rmtree(tmp, ignore_errors=True)

    def test_stale_state_file_does_not_close_a_finished_round(self):
        """Regression for vps rounds 55/56.

        A round that crashes leaves round.state.json behind. The next round
        overwrites it, but if the service restarts in between, the leftover
        names an *older* round -- and closing that one stamped a healthy round
        with a failure note and an out-of-order finish time.
        """
        tmp = Path(tempfile.mkdtemp())
        old_data, old_path, old_conn = cfgmod.DATA, engine.db.DB_PATH, engine.db._conn
        old_state = engine.ROUND_STATE
        cfgmod.DATA = tmp
        engine.db.DB_PATH, engine.db._conn = tmp / "s.db", None
        engine.ROUND_STATE = tmp / "round.state.json"
        try:
            engine.db.connect()
            finished = engine.db.start_round("schedule")
            engine.db.finish_round(finished, note=None, duration_s=110.0)
            later = engine.db.start_round("cli")

            # leftover state names the round that already completed
            engine.ROUND_STATE.write_text(json.dumps(
                {"phase": "delay-test", "round_id": finished, "pid": 1,
                 "ts": "2026-09-19T12:38:09", "epoch": 1}), encoding="utf-8")

            engine._abandon_round({"alert": {"enabled": False}}, None,
                                  "previous crash", lambda *a: None,
                                  "round_failed", "轮次执行异常")

            row = engine.db.one("SELECT note, duration_s FROM rounds WHERE id=?", (finished,))
            self.assertIsNone(row["note"], "a finished round was re-closed as aborted")
            self.assertEqual(row["duration_s"], 110.0, "a healthy duration was overwritten")

            # and the genuinely-open round must be left alone too
            open_row = engine.db.one("SELECT finished_at FROM rounds WHERE id=?", (later,))
            self.assertIsNone(open_row["finished_at"],
                              "the stale state was used to close an unrelated round")
        finally:
            if engine.db._conn is not None:
                engine.db._conn.close()
            engine.db.DB_PATH, engine.db._conn = old_path, old_conn
            cfgmod.DATA, engine.ROUND_STATE = old_data, old_state
            shutil.rmtree(tmp, ignore_errors=True)

    def test_abandon_still_closes_the_round_it_was_given(self):
        """The explicit round_id is authoritative and must not be second-guessed."""
        tmp = Path(tempfile.mkdtemp())
        old_data, old_path, old_conn = cfgmod.DATA, engine.db.DB_PATH, engine.db._conn
        old_state = engine.ROUND_STATE
        cfgmod.DATA = tmp
        engine.db.DB_PATH, engine.db._conn = tmp / "s.db", None
        engine.ROUND_STATE = tmp / "round.state.json"
        try:
            engine.db.connect()
            rid = engine.db.start_round("cli")
            engine._abandon_round({"alert": {"enabled": False}}, rid,
                                  "timeout at phase publish", lambda *a: None,
                                  "round_timeout", "轮次超时")
            row = engine.db.one("SELECT note, finished_at FROM rounds WHERE id=?", (rid,))
            self.assertIsNotNone(row["finished_at"])
            self.assertIn("timeout", row["note"])
        finally:
            if engine.db._conn is not None:
                engine.db._conn.close()
            engine.db.DB_PATH, engine.db._conn = old_path, old_conn
            cfgmod.DATA, engine.ROUND_STATE = old_data, old_state
            shutil.rmtree(tmp, ignore_errors=True)

    def test_excluded_fps_do_not_reach_the_ledger_prune_unhashed(self):
        """Regression: the prune read entry["fp"], which excluded entries lack."""
        import collections

        excluded = [{"source": "deltasub", "name": "CN relay",
                     "proxy": {"name": "CN relay", "type": "ss",
                               "server": "203.0.113.10", "port": 1}}]
        by_name = {"tested": {"source": "deltasub", "fp": "testedfp",
                              "name": "tested", "proxy": {}}}
        # emulate the fixed code path: fps come from the stripped proxy
        excluded_fps = collections.defaultdict(set)
        for entry in excluded:
            stripped = {k: v for k, v in entry["proxy"].items()
                        if k not in coremod.DROP_FIELDS}
            excluded_fps[entry["source"]].add(coremod.fingerprint_proxy(stripped))
        seen = sorted({e["fp"] for e in by_name.values()
                       if e["source"] == "deltasub"} | excluded_fps.get("deltasub", set()))
        self.assertEqual(len(seen), 2)
        self.assertIn(coremod.fingerprint_proxy(
            {"type": "ss", "server": "203.0.113.10", "port": 1}), seen)


class StripEchTest(unittest.TestCase):
    """mihomo's ECH handling is flaky against CF-fronted nodes; stripping it
    measures the node instead of the kernel's ECH implementation."""

    def entry(self, name="n"):
        return {"source": "s", "name": name,
                "proxy": {"name": name, "type": "vless", "server": "s", "port": 1,
                          "ech-opts": {"enable": True}}, "index": 0}

    def test_stripped_when_enabled(self):
        proxies, mapping, dropped = coremod.prepare([self.entry()], strip_ech=True)
        self.assertNotIn("ech-opts", proxies[0])

    def test_kept_when_disabled(self):
        proxies, mapping, dropped = coremod.prepare([self.entry()], strip_ech=False)
        self.assertIn("ech-opts", proxies[0])

    def test_off_by_default(self):
        proxies, _m, _d = coremod.prepare([self.entry()])
        self.assertIn("ech-opts", proxies[0])


class ReapOrphanRoundsTest(unittest.TestCase):
    """A round cannot outlive the process that was running it.

    `_abandon_round` only runs in-process, so `docker compose up -d --build`
    -- the documented way to deploy, and a container restart in the middle of a
    round -- left the ledger row open forever. The watchdog deadline is a
    monotonic timer that dies with the process, and the scheduler only starts
    rounds, never closes them. On vps that stranded round 172.
    """

    def setUp(self):
        from mihomo_test import db as dbmod

        self.db = dbmod
        self.cfg = {"watchdog": {"round_timeout_minutes": 20}}
        self.rounds = []
        for age_min in (175, 25, 1):
            round_id = dbmod.start_round("test")
            stamp = time.strftime("%Y-%m-%dT%H:%M:%S",
                                  time.gmtime(time.time() - age_min * 60))
            dbmod.execute("UPDATE rounds SET started_at=? WHERE id=?",
                          (stamp, round_id))
            self.rounds.append(round_id)

    def tearDown(self):
        for round_id in self.rounds:
            self.db.execute("DELETE FROM rounds WHERE id=?", (round_id,))

    def open_ids(self):
        return {row["id"] for row in self.db.open_rounds()}

    def test_only_rounds_past_the_budget_are_closed(self):
        old, past, fresh = self.rounds
        closed = engine.reap_orphan_rounds(self.cfg, log=_nolog)
        self.assertEqual(sorted(closed), sorted([old, past]))
        still_open = self.open_ids()
        self.assertIn(fresh, still_open)
        self.assertNotIn(old, still_open)
        self.assertNotIn(past, still_open)

    def test_a_reaped_round_is_marked_so_the_panel_can_explain_it(self):
        engine.reap_orphan_rounds(self.cfg, log=_nolog)
        row = self.db.one("SELECT * FROM rounds WHERE id=?", (self.rounds[0],))
        self.assertIsNotNone(row["finished_at"])
        self.assertIn("orphaned", row["note"])

    def test_a_second_pass_is_a_no_op(self):
        engine.reap_orphan_rounds(self.cfg, log=_nolog)
        self.assertEqual(engine.reap_orphan_rounds(self.cfg, log=_nolog), [])

    def test_reaping_is_logged(self):
        seen = []
        engine.reap_orphan_rounds(self.cfg, log=lambda level, msg: seen.append((level, msg)))
        self.assertTrue(any("残留轮次" in msg for _lvl, msg in seen), seen)

    def test_a_healthy_recent_round_is_never_touched(self):
        self.db.execute("DELETE FROM rounds WHERE id IN (?, ?)",
                        (self.rounds[0], self.rounds[1]))
        self.assertEqual(engine.reap_orphan_rounds(self.cfg, log=_nolog), [])
        self.assertIn(self.rounds[2], self.open_ids())


class _FakePushStore:
    """Records upserts; can be told to fail on a given subscription name.

    `existing` seeds `/api/subs` so the prune path has something to walk;
    `deletes` records what it removed.
    """

    def __init__(self, fail_on=(), existing=()):
        self.upserts = []
        self.fail_on = tuple(fail_on)
        self.existing = list(existing)
        self.deletes = []

    def upsert(self, kind, name, payload):
        if name in self.fail_on:
            raise StoreError("sub-store said no")
        self.upserts.append((kind, name, payload))
        return "updated"

    def get_json(self, path):
        if path == "/api/subs":
            return list(self.existing)
        return []

    def _request(self, method, path, body=None):
        self.deletes.append((method, path))
        return {}

    def names(self):
        return [name for _kind, name, _payload in self.upserts]


class PushExportsTest(unittest.TestCase):
    """A disabled source is not published, so it must not be pushed.

    `push_exports` was fed `[s["key"] for s in cfg["sources"]]`, which put the
    sources the operator had just switched off into the push. They have no
    export file (`cleanup_exports` deleted it), so the panel reported them as
    "还没有输出文件，先跑一轮" -- a line that reads like a failure for something
    deliberately turned off. Worse, in the window between disabling a source
    and the next round's cleanup its stale YAML is still on disk, so the push
    resurrected it in Sub-Store -- and `link_substore` never prunes `-local`
    subs, so nothing would have undone that.

    `link_substore` and `exports_summary` already filtered on `enabled`; the
    push path was the one that did not.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self._old = engine.EXPORT_DIR
        engine.EXPORT_DIR = self.tmp

    def tearDown(self):
        engine.EXPORT_DIR = self._old
        shutil.rmtree(self.tmp, ignore_errors=True)

    def export(self, key, count=1):
        (self.tmp / f"{key}.yaml").write_text(
            "proxies:\n  - {name: n, type: vless, server: s, port: 1}\n", encoding="utf-8")
        (self.tmp / f"{key}.meta.json").write_text(
            json.dumps({"count": count}), encoding="utf-8")

    def cfg(self, sources):
        return {"sources": sources, "publish": {"prefix": "probe"}}

    def push(self, cfg, store, keys=None):
        keys = engine.publish_keys(cfg) if keys is None else keys
        return engine.push_exports(cfg, store, keys, log=_nolog)

    def test_publish_keys_keeps_only_enabled_sources_that_have_a_key(self):
        cfg = self.cfg([
            {"key": "air", "enabled": True},
            {"key": "eps1", "enabled": False},
            {"key": "", "enabled": True},
            {"name": "no-key-field", "enabled": True},
            {"key": "deltasub"},
        ])
        self.assertEqual(engine.publish_keys(cfg), ["air", "deltasub"])

    def test_a_disabled_source_is_never_pushed_even_with_a_stale_export(self):
        self.export("air")
        self.export("eps1")
        store = _FakePushStore()
        cfg = self.cfg([{"key": "air", "enabled": True},
                        {"key": "eps1", "enabled": False}])
        self.push(cfg, store)
        self.assertEqual(store.names(), ["probe-air-local"])

    def test_disabled_sources_do_not_appear_in_the_report(self):
        self.export("air")
        store = _FakePushStore()
        cfg = self.cfg([{"key": "air", "enabled": True},
                        {"key": "eps1", "enabled": False}])
        lines = engine.push_report(self.push(cfg, store))
        self.assertEqual(len(lines), 1)
        self.assertNotIn("eps1", " ".join(lines))

    def test_missing_export_is_reported_once_and_writes_nothing(self):
        store = _FakePushStore()
        records = self.push(self.cfg([{"key": "fresh", "enabled": True}]), store)
        self.assertEqual(store.upserts, [])
        self.assertFalse(records[0]["ok"])
        self.assertEqual(records[0]["level"], "warn")
        self.assertIn("还没有输出文件", records[0]["text"])

    def test_records_carry_the_count_and_the_subscription_name(self):
        self.export("air", count=7)
        store = _FakePushStore()
        records = self.push(self.cfg([{"key": "air", "enabled": True}]), store)
        self.assertTrue(records[0]["ok"])
        self.assertEqual(records[0]["count"], 7)
        self.assertEqual(records[0]["name"], "probe-air-local")

    def test_a_store_failure_is_reported_at_error_level(self):
        self.export("air")
        store = _FakePushStore(fail_on=("probe-air-local",))
        records = self.push(self.cfg([{"key": "air", "enabled": True}]), store)
        self.assertFalse(records[0]["ok"])
        self.assertEqual(records[0]["level"], "error")

    def test_summary_counts_pushed_and_failed(self):
        self.assertEqual(engine.push_summary([]), "没有已启用的来源，未推送任何内容")
        ok = {"ok": True, "count": 1}
        self.assertEqual(engine.push_summary([ok, ok]), "已推送 2 个订阅")
        self.assertIn("1 个未推送", engine.push_summary([ok, {"ok": False}]))

    def test_cli_report_keeps_the_original_one_line_per_key_text(self):
        self.export("air", count=3)
        store = _FakePushStore()
        records = self.push(self.cfg([{"key": "air", "enabled": True}]), store)
        self.assertEqual(engine.push_report(records),
                         ["probe-air-local: updated (3 节点)"])


class PruneLocalSubsTest(unittest.TestCase):
    """A push only ever wrote; nothing ever removed a retired `-local` sub.

    `push_exports` upserts the keys it is handed and stops there, so a source
    that stops being published keeps its last `-local` subscription forever.
    The content is embedded at push time, so that copy keeps serving the nodes
    it had when it was retired -- and for a muted source the export file is
    gone, so it resolves to zero nodes and Sub-Store answers HTTP 500 for it.
    On the live box `probe-alphasub-local` sat in exactly that state.

    Scope matters as much as the deletion: only `source == "local"` names under
    our prefix are ours to remove. A `remote` sub of the same name belongs to
    `link_substore`.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self._old = engine.EXPORT_DIR
        engine.EXPORT_DIR = self.tmp

    def tearDown(self):
        engine.EXPORT_DIR = self._old
        shutil.rmtree(self.tmp, ignore_errors=True)

    def export(self, key, count=3):
        (self.tmp / f"{key}.yaml").write_text(
            "proxies:\n  - {name: n, type: vless, server: s, port: 1}\n", encoding="utf-8")
        (self.tmp / f"{key}.meta.json").write_text(
            json.dumps({"count": count}), encoding="utf-8")

    def run_push(self, keys, existing):
        store = _FakePushStore(existing=existing)
        for key in keys:
            self.export(key)
        return engine.push_exports({"publish": {"prefix": "probe"}}, store, keys), store

    def test_a_retired_local_sub_is_removed(self):
        records, store = self.run_push(
            ["alive"],
            existing=[{"name": "probe-alive-local", "source": "local"},
                      {"name": "probe-retired-local", "source": "local"}])
        self.assertIn(("DELETE", "/api/sub/probe-retired-local"), store.deletes)

    def test_the_still_published_local_sub_is_kept(self):
        _records, store = self.run_push(
            ["alive"],
            existing=[{"name": "probe-alive-local", "source": "local"}])
        self.assertEqual(store.deletes, [])

    def test_remote_subs_of_the_same_name_are_left_alone(self):
        """`link_substore` owns those; deleting here would fight it."""
        _records, store = self.run_push(
            ["alive"],
            existing=[{"name": "probe-retired-local", "source": "remote"}])
        self.assertEqual(store.deletes, [])

    def test_someone_elses_sub_is_left_alone(self):
        _records, store = self.run_push(
            ["alive"],
            existing=[{"name": "other-retired-local", "source": "local"},
                      {"name": "probe-retired", "source": "local"}])
        self.assertEqual(store.deletes, [])

    def test_a_non_local_name_under_our_prefix_is_left_alone(self):
        """Only the `-local` shape is ours to prune."""
        _records, store = self.run_push(
            ["alive"],
            existing=[{"name": "probe-retired", "source": "local"}])
        self.assertEqual(store.deletes, [])

    def test_the_removal_is_reported(self):
        records, _store = self.run_push(
            ["alive"],
            existing=[{"name": "probe-retired-local", "source": "local"}])
        texts = engine.push_report(records)
        self.assertTrue(any("probe-retired-local" in t and "已移除" in t for t in texts),
                        f"removal not reported: {texts}")

    def test_a_removal_failure_is_reported_at_error_level(self):
        store = _FakePushStore(
            existing=[{"name": "probe-retired-local", "source": "local"}])
        store._request = lambda *a, **k: (_ for _ in ()).throw(StoreError("nope"))
        self.export("alive")
        records = engine.push_exports({"publish": {"prefix": "probe"}}, store, ["alive"])
        bad = [r for r in records if "移除失败" in r["text"]]
        self.assertEqual(len(bad), 1)
        self.assertEqual(bad[0]["level"], "error")

    def test_an_unreachable_store_does_not_break_the_push(self):
        """The prune is housekeeping; it must not fail the push that preceded it."""
        def boom(_path):
            raise StoreError("sub-store unreachable")

        store = _FakePushStore()
        store.get_json = boom
        self.export("alive")
        records = engine.push_exports({"publish": {"prefix": "probe"}}, store, ["alive"])
        self.assertTrue(records[0]["ok"])

    def test_a_single_source_round_keeps_the_other_sources_subscriptions(self):
        """`--source air` hands `push_exports` one key; the prune must not care.

        `keep` was built from the keys of *this* push, so a round scoped to one
        source -- `POST /api/run {"source": ...}`, or `--source` on the CLI --
        deleted every other source's `-local` subscription. An unrelated source
        lost its published list because the operator tested one of them. The
        keys that may still exist come from `publish_keys(cfg)`, which is also
        what keeps retired sources deletable.
        """
        existing = [{"name": "probe-air-local", "source": "local"},
                    {"name": "probe-other-local", "source": "local"}]
        self.export("air")
        store = _FakePushStore(existing=existing)
        cfg = {"publish": {"prefix": "probe"},
               "sources": [{"key": "air", "enabled": True},
                           {"key": "other", "enabled": True}]}
        engine.push_exports(cfg, store, ["air"])
        self.assertEqual(store.deletes, [],
                         f"单来源轮次删掉了别人的订阅: {store.deletes}")

        # A source that genuinely left the config is still removed: absence
        # from `publish_keys` is the retirement signal, not absence from `keys`.
        store = _FakePushStore(existing=existing)
        cfg["sources"][1]["enabled"] = False
        engine.push_exports(cfg, store, ["air"])
        self.assertEqual(store.deletes, [("DELETE", "/api/sub/probe-other-local")])


class PushEndpointTest(unittest.TestCase):
    """The defect was at the call site, so the endpoint itself is exercised.

    `engine.push_exports` was always correct; what fed it was not. A unit test
    on `publish_keys` alone would have stayed green through the whole bug, so
    this drives the real handler over a socket.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self._old_dir = engine.EXPORT_DIR
        engine.EXPORT_DIR = self.tmp
        self._old_cfg = server._State.cfg
        self.store = _FakePushStore()
        for key, count in (("air", 5), ("eps1", 9)):
            (self.tmp / f"{key}.yaml").write_text("proxies: []\n", encoding="utf-8")
            (self.tmp / f"{key}.meta.json").write_text(
                json.dumps({"count": count}), encoding="utf-8")

    def tearDown(self):
        engine.EXPORT_DIR = self._old_dir
        server._State.cfg = self._old_cfg
        shutil.rmtree(self.tmp, ignore_errors=True)

    def post_push(self, cfg):
        # `cfg["substore"]["backend"]` is still evaluated as the argument, so
        # the stub has to be a real key even though Client itself is patched.
        cfg.setdefault("substore", {"backend": "http://stub"})
        with unittest.mock.patch.object(server, "Client", lambda *a, **k: self.store):
            httpd = server.serve(cfg, "127.0.0.1", 0)
            thread = threading.Thread(target=httpd.serve_forever, daemon=True)
            thread.start()
            try:
                port = httpd.server_address[1]
                request = urllib.request.Request(
                    f"http://127.0.0.1:{port}/api/push", data=b"{}",
                    headers={"Content-Type": "application/json",
                             "X-Auth-Token": "tok"})
                with urllib.request.urlopen(request, timeout=15) as response:
                    return json.loads(response.read().decode("utf-8"))
            finally:
                httpd.shutdown()
                httpd.server_close()

    def test_endpoint_pushes_enabled_sources_and_skips_disabled_ones(self):
        cfg = {
            "auth": {"token": "tok"},
            "publish": {"prefix": "probe"},
            "sources": [{"key": "air", "enabled": True},
                        {"key": "eps1", "enabled": False}],
        }
        payload = self.post_push(cfg)
        self.assertEqual(self.store.names(), ["probe-air-local"])
        self.assertEqual(payload["message"], "已推送 1 个订阅")
        self.assertEqual([r["key"] for r in payload["detail"]], ["air"])
        self.assertNotIn("还没有输出文件", json.dumps(payload, ensure_ascii=False))

    def test_endpoint_returns_a_summary_plus_records_not_one_long_sentence(self):
        cfg = {
            "auth": {"token": "tok"},
            "publish": {"prefix": "probe"},
            "sources": [{"key": "air", "enabled": True}],
        }
        payload = self.post_push(cfg)
        self.assertEqual(payload["message"], "已推送 1 个订阅")
        self.assertEqual(payload["detail"][0]["text"], "probe-air-local: updated (5 节点)")
        self.assertTrue(payload["detail"][0]["ok"])




class RegionTagIdempotenceTest(unittest.TestCase):
    """Re-tagging must collapse *every* tag we may have added, not just one.

    The old pattern matched a single `[XX]` and required the name to start with
    `[`. A name that had already been through the flag operator (`flag [HK] Foo`)
    therefore did not match, so the tag was appended instead of replaced. In a
    source that feeds itself that leaks one tag per round -- the live `air`
    collection reached `[HK] flag [HK]  [HK]  HK-Kwu...` after three passes.
    """

    def _strip(self, name):
        return engine._LEADING_TAG.sub("", name)

    def test_a_single_tag_is_removed(self):
        self.assertEqual(self._strip("[HK] Foo"), "Foo")

    def test_every_repeated_tag_is_removed(self):
        self.assertEqual(self._strip("[HK] [HK] [HK] Foo"), "Foo")

    def test_a_flag_between_tags_does_not_stop_the_strip(self):
        self.assertEqual(self._strip("🇭🇰 [HK]  [HK]  Foo"), "Foo")

    def test_the_live_air_shape_collapses(self):
        """The exact name three round-trips through air produced."""
        self.assertEqual(
            self._strip("[HK] 🇭🇰 [HK]  [HK]  HK-Kwu Tung-h-6491845-wjhg"),
            "HK-Kwu Tung-h-6491845-wjhg")

    def test_stripping_is_idempotent(self):
        name = "🇭🇰 [HK]  [HK]  Foo"
        once = self._strip(name)
        self.assertEqual(self._strip(once), once)

    def test_a_hyphenated_region_tag_is_removed(self):
        self.assertEqual(self._strip("[US-CA] Foo"), "Foo")

    def test_a_tag_inside_the_name_is_left_alone(self):
        """Only the leading run is ours; the rest belongs to upstream."""
        self.assertEqual(self._strip("Foo [HK] Bar"), "Foo [HK] Bar")

    def test_a_name_without_any_tag_is_untouched(self):
        self.assertEqual(self._strip("HK-Kwu Tung-h-6491845-wjhg"),
                         "HK-Kwu Tung-h-6491845-wjhg")

if __name__ == "__main__":
    unittest.main(verbosity=2)
