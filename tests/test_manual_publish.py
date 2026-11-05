"""manual_publish 单测（指导 §0.6 / §9.4 / §13.5：人工三步、防重放、update-only）。

覆盖：confirm token 生命周期（签发/消费/过期/绑定不符/重放拒绝/重签作废旧
token）、diff_preview 计数与样例（改名不成对出现、凭据不进样例）、
parse_proxies 的 BAD_CONTENT、push_canonical 的 update-only 语义。
HTTP 面的鉴权/审计在 tests.test_probe_dispatch。
"""
import unittest
from unittest import mock

import tests._isolation as iso
from mihomo_test import manual_publish
from mihomo_test.store import NotFound, StoreError

iso  # noqa: B018


def setUpModule():
    iso.isolate()


def tearDownModule():
    iso.restore()


def _proxy(name, server="s.example.com", port=443, uuid="u-1"):
    return {"name": name, "type": "vless", "server": server, "port": port,
            "uuid": uuid, "tls": True}


class TokenLifecycleTest(unittest.TestCase):
    def setUp(self):
        manual_publish.reset_tokens()
        self.addCleanup(manual_publish.reset_tokens)

    def test_issue_then_consume_once(self):
        issued = manual_publish.issue_confirm_token("t1", "d1", 5)
        self.assertEqual(issued["expires_in"], 60)
        self.assertTrue(manual_publish.consume_confirm_token(
            issued["token"], "t1", "d1", 5))

    def test_replay_is_rejected(self):
        issued = manual_publish.issue_confirm_token("t1", "d1", 5)
        manual_publish.consume_confirm_token(issued["token"], "t1", "d1", 5)
        with self.assertRaises(manual_publish.PublishError) as ctx:
            manual_publish.consume_confirm_token(issued["token"], "t1", "d1", 5)
        self.assertEqual(ctx.exception.code, "CONFIRM_TOKEN_REUSED")

    def test_expired_token_is_rejected(self):
        clock = {"t": 1000.0}
        issued = manual_publish.issue_confirm_token("t1", "d1", 5,
                                                    now=lambda: clock["t"])
        clock["t"] += 61
        with self.assertRaises(manual_publish.PublishError) as ctx:
            manual_publish.consume_confirm_token(issued["token"], "t1", "d1", 5,
                                                 now=lambda: clock["t"])
        self.assertEqual(ctx.exception.code, "CONFIRM_TOKEN_EXPIRED")

    def test_binding_mismatch_is_rejected(self):
        issued = manual_publish.issue_confirm_token("t1", "d1", 5)
        for args in (("t2", "d1", 5), ("t1", "d2", 5), ("t1", "d1", 6)):
            with self.assertRaises(manual_publish.PublishError) as ctx:
                manual_publish.consume_confirm_token(issued["token"], *args)
            self.assertEqual(ctx.exception.code, "CONFIRM_TOKEN_MISMATCH")

    def test_missing_token_requires_confirmation(self):
        with self.assertRaises(manual_publish.PublishError) as ctx:
            manual_publish.consume_confirm_token("", "t1", "d1", 5)
        self.assertEqual(ctx.exception.code, "PUSH_CONFIRM_REQUIRED")

    def test_reissue_invalidates_the_old_token(self):
        first = manual_publish.issue_confirm_token("t1", "d1", 5)
        second = manual_publish.issue_confirm_token("t1", "d2", 7)
        with self.assertRaises(manual_publish.PublishError) as ctx:
            manual_publish.consume_confirm_token(first["token"], "t1", "d1", 5)
        self.assertEqual(ctx.exception.code, "CONFIRM_TOKEN_REUSED")
        self.assertTrue(manual_publish.consume_confirm_token(
            second["token"], "t1", "d2", 7))


class DiffPreviewTest(unittest.TestCase):
    def test_counts_and_rename_not_add_remove_pair(self):
        current = [_proxy("HK | old", server="a", port=1),
                   _proxy("US | keep", server="b", port=2)]
        canonical = [_proxy("HK | new", server="a", port=1),
                     _proxy("US | keep", server="b", port=2),
                     _proxy("JP | add", server="c", port=3)]
        preview = manual_publish.diff_preview(current, canonical)
        self.assertEqual(preview["added"], 1)
        self.assertEqual(preview["removed"], 0)
        self.assertEqual(preview["renamed"], 1)
        self.assertEqual(preview["samples"]["renamed"],
                         [{"from": "HK | old", "to": "HK | new"}])
        self.assertEqual(preview["unchanged"], 1)

    def test_fingerprint_identity_not_display_name(self):
        # 同指纹不同名 = 一条改名；不同指纹同名 = 新增（不再按 name 认亲）
        preview = manual_publish.diff_preview([_proxy("same-name", server="a")],
                                              [_proxy("same-name", server="b")])
        self.assertEqual(preview["added"], 1)
        self.assertEqual(preview["removed"], 1)
        self.assertEqual(preview["renamed"], 0)

    def test_samples_carry_no_credentials(self):
        preview = manual_publish.diff_preview(
            [], [_proxy("HK | x", server="secret.example", port=1234,
                        uuid="very-secret-uuid")])
        for sample in preview["samples"]["added"]:
            self.assertEqual(set(sample), {"name", "type"})


class ParseProxiesTest(unittest.TestCase):
    def test_bad_yaml_is_bad_content(self):
        with self.assertRaises(manual_publish.PublishError) as ctx:
            manual_publish.parse_proxies("proxies: [unclosed")
        self.assertEqual(ctx.exception.code, "BAD_CONTENT")

    def test_missing_proxies_is_bad_content(self):
        with self.assertRaises(manual_publish.PublishError) as ctx:
            manual_publish.parse_proxies("proxies-other: []\n")
        self.assertEqual(ctx.exception.code, "BAD_CONTENT")


class _StubStore:
    def __init__(self, behavior="ok"):
        self.behavior = behavior
        self.calls = []

    def update_only(self, kind, name, payload):
        self.calls.append((kind, name, payload))
        if self.behavior == "missing":
            raise NotFound("not found")
        if self.behavior == "error":
            raise StoreError("boom")
        return "updated"


class PushCanonicalTest(unittest.TestCase):
    _CFG = {"substore": {"target_name": "mihomo-probe-alive"}}

    def test_update_only_hits_the_bound_target(self):
        store = _StubStore()
        logs = []
        result = manual_publish.push_canonical(
            store, self._CFG, "proxies: []\n", 3,
            log=lambda level, message: logs.append((level, message)))
        self.assertEqual(result["status"], "pushed")
        self.assertEqual(result["action"], "updated")
        kind, name, payload = store.calls[0]
        self.assertEqual((kind, name), ("sub", "mihomo-probe-alive"))
        self.assertEqual(payload["source"], "local")
        self.assertEqual(payload["content"], "proxies: []\n")

    def test_missing_target_never_creates(self):
        with self.assertRaises(manual_publish.PublishError) as ctx:
            manual_publish.push_canonical(_StubStore("missing"), self._CFG,
                                          "proxies: []\n", 0)
        self.assertEqual(ctx.exception.code, "SUBSTORE_TARGET_MISSING")

    def test_store_error_is_reported(self):
        with self.assertRaises(manual_publish.PublishError) as ctx:
            manual_publish.push_canonical(_StubStore("error"), self._CFG,
                                          "proxies: []\n", 0)
        self.assertEqual(ctx.exception.code, "SUBSTORE_ERROR")


if __name__ == "__main__":
    unittest.main()
