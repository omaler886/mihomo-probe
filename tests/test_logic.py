"""Unit tests for the pieces that decide what is dead and what gets published."""
import calendar
import os
import json
import shutil
import sys
import time
import unittest
import unittest.mock
from pathlib import Path
import tempfile

import yaml

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import config as cfgmod
from mihomo_test import core as coremod
from mihomo_test import engine, policy
from mihomo_test.store import Client

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

    def cfg(self, attempts=3):
        return {"targets": ["u1", "u2", "u3"], "expected_status": "204",
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
        out = engine._export_proxies(["A", "B"], proxies, {}, self.cfg())
        self.assertEqual(len(out), 1)

    def test_genuinely_different_nodes_are_both_published(self):
        proxies = {
            "A": {"name": "A", "type": "mieru", "server": "s", "port": 1, "username": "u"},
            "B": {"name": "B", "type": "mieru", "server": "s", "port": 2, "username": "u"},
        }
        out = engine._export_proxies(["A", "B"], proxies, {}, self.cfg())
        self.assertEqual(len(out), 2)

    def test_region_tag_is_prepended_from_the_verified_exit(self):
        proxies = {"A": {"name": "node", "type": "vless", "server": "s", "port": 1}}
        out = engine._export_proxies(["A"], proxies, {"A": {"country": "JP"}}, self.cfg(tag=True))
        self.assertEqual(out[0]["name"], "[JP] node")

    def test_region_tag_is_not_applied_twice(self):
        proxies = {"A": {"name": "[JP] node", "type": "vless", "server": "s", "port": 1}}
        out = engine._export_proxies(["A"], proxies, {"A": {"country": "JP"}}, self.cfg(tag=True))
        self.assertEqual(out[0]["name"], "[JP] node")

    def test_verified_exit_overrides_a_stale_tag(self):
        proxies = {"A": {"name": "[SG] node", "type": "vless", "server": "s", "port": 1}}
        out = engine._export_proxies(["A"], proxies, {"A": {"country": "JP"}}, self.cfg(tag=True))
        self.assertEqual(out[0]["name"], "[JP] node")


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
            os.unlink(handle.name)

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
            os.unlink(handle.name)

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
            os.unlink(path)

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
            os.unlink(path)

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
            os.unlink(path)


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



class DashboardScriptTest(unittest.TestCase):
    """The dashboard is one big HTML string; a quoting slip makes it blank.

    This is not hypothetical: an unescaped quote in a message produced a page
    that rendered its static markup but ran no script at all, so every panel
    stayed empty. Only a real parse of the served script catches that.
    """

    def rendered_script(self):
        from mihomo_test import ui

        page = ui.render("title", "token")
        self.assertIn("<script>", page)
        return page.split("<script>", 1)[1].split("</script>", 1)[0]

    def test_page_has_no_unbalanced_braces(self):
        script = self.rendered_script()
        for op, cl in (("{", "}"), ("(", ")"), ("[", "]")):
            self.assertEqual(script.count(op), script.count(cl),
                             f"unbalanced {op}{cl} in the dashboard script")

    def test_page_embeds_the_token_and_no_placeholder(self):
        from mihomo_test import ui

        page = ui.render("我的标题", "secret-token")
        self.assertIn("secret-token", page)
        self.assertIn("我的标题", page)
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
        return {"source": source, "name": name,
                "proxy": {"name": name, "type": "vless", "server": server, "port": 1},
                "index": 0, "fp": None}

    def test_literal_ips_pass_through_unresolved(self):
        hosts = engine.resolve_servers([self.entry("203.0.113.10"),
                                        self.entry("2406:da18::1")])
        self.assertEqual(hosts["203.0.113.10"], ["203.0.113.10"])
        self.assertEqual(hosts["2406:da18::1"], ["2406:da18::1"])

    def test_hostnames_are_resolved_once_each(self):
        calls = []
        real = engine.socket.getaddrinfo

        def spy(host, *args, **kwargs):
            calls.append(host)
            return real(host, *args, **kwargs)

        with unittest.mock.patch.object(engine.socket, "getaddrinfo", spy):
            engine.resolve_servers([self.entry("example.com"), self.entry("example.com")])
        self.assertEqual(calls, ["example.com"])

    def test_unresolvable_host_yields_no_addresses(self):
        with unittest.mock.patch.object(
                engine.socket, "getaddrinfo",
                unittest.mock.Mock(side_effect=engine.socket.gaierror(1, "nx"))):
            hosts = engine.resolve_servers([self.entry("nope.invalid")])
        self.assertEqual(hosts["nope.invalid"], [])

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
        with self.assertRaises(engine.RoundTimeout):
            engine._checkpoint(time.monotonic() - 1, "publish", None)

    def test_a_live_budget_passes_the_checkpoint(self):
        engine._checkpoint(time.monotonic() + 60, "publish", None)


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

        with mock.patch.object(notifier, "telegram_send", fake_send), \
             mock.patch.object(notifier, "STATE_PATH", Path(tempfile.mkdtemp()) / "s.json"):
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

if __name__ == "__main__":
    unittest.main(verbosity=2)
