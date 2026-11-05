"""canonical_export 单测（指导 §6 / plan §4.4）。

覆盖：同 fingerprint 多源去重 + source_aliases、重命名模板变量逐个 + 冲突
#2/#3、改名不改连接参数、固定排序稳定性（打乱输入同输出）、rename.enabled=false、
YAML 无每轮易变字段（内容 sha 稳定）。
"""
import importlib
import unittest
import uuid

import yaml

import tests._isolation as iso
from mihomo_test import canonical_export, core, engine

iso  # noqa: B018 - imported for side effect of patching data paths


def setUpModule():
    iso.isolate()


def tearDownModule():
    iso.restore()


def _proxy(name, server, port=443, proto="vless", uuid_=None, extra=None):
    proxy = {
        "name": name,
        "type": proto,
        "server": server,
        "port": port,
        "uuid": uuid_ or uuid.uuid4().hex,
        "tls": True,
        "network": "tcp",
    }
    if extra:
        proxy.update(extra)
    return proxy


class _Node:
    """One source's contribution, keyed by fingerprint for metadata lookup."""

    def __init__(self, proxy, source, alive=True, delay=123,
                 egress_country=None, best_probe="hk3-a"):
        self.proxy = proxy
        self.fp = core.fingerprint_proxy(proxy)
        self.source = source
        self.meta = {"alive": alive, "delay": delay,
                     "egress_country": egress_country, "best_probe": best_probe}

    def entry(self):
        return {"key": self.source, "proxies": [self.proxy], "groups": [],
                "meta": {self.proxy["name"]: self.meta,
                         self.fp: self.meta}}


def _names(payload_bytes):
    doc = yaml.safe_load(payload_bytes)
    return [p["name"] for p in doc["proxies"]]


class DedupTest(unittest.TestCase):
    def test_same_fingerprint_across_sources_kept_once_with_alias(self):
        proxy = _proxy("Foo", "a.example.com")
        node = _Node(proxy, "hk3-a", egress_country="HK")
        twin = _Node(dict(proxy), "sg-a")  # 同连接参数，不同来源
        body, meta = canonical_export.build([node.entry(), twin.entry()], _cfg())
        self.assertEqual(meta["count"], 1)
        aliases = meta["source_aliases"]
        self.assertEqual(aliases.get(node.fp), ["sg-a"])
        self.assertEqual(_names(body), ["HK | Foo"])

    def test_same_name_different_fingerprint_not_deduped(self):
        a = _Node(_proxy("Foo", "a.example.com"), "hk3-a")
        b = _Node(_proxy("Foo", "b.example.com"), "hk3-a")
        body, meta = canonical_export.build([a.entry(), b.entry()], _cfg())
        self.assertEqual(meta["count"], 2)
        self.assertEqual(len(set(_names(body))), 2)


class RenameTemplateTest(unittest.TestCase):
    def _one(self, template, **kw):
        proxy = _proxy("Foo", "a.example.com", extra=kw.pop("extra", None))
        node = _Node(proxy, "hk3-a", **kw)
        body, meta = canonical_export.build([node.entry()], _cfg(template))
        return _names(body)[0], meta

    def test_display_name(self):
        name, _ = self._one("{display_name}")
        self.assertEqual(name, "Foo")

    def test_country_measured(self):
        name, _ = self._one("{country}", egress_country="sg")
        self.assertEqual(name, "SG")

    def test_country_unknown_is_UN(self):
        name, _ = self._one("{country}", egress_country=None)
        self.assertEqual(name, "UN")

    def test_probe(self):
        name, _ = self._one("{probe}", best_probe="sg-a")
        self.assertEqual(name, "sg-a")

    def test_delay(self):
        name, _ = self._one("{delay}", delay=183)
        self.assertEqual(name, "183")

    def test_protocol(self):
        name, _ = self._one("{protocol}")
        self.assertEqual(name, "vless")

    def test_index_is_post_sort_position(self):
        nodes = [_Node(_proxy(f"z{i}", f"s{i}.example.com"), "hk3-a")
                 for i in range(3)]
        body, _ = canonical_export.build([n.entry() for n in nodes], _cfg("{index}"))
        self.assertEqual(_names(body), ["1", "2", "3"])

    def test_collision_suffix_appended(self):
        a = _Node(_proxy("Foo", "a.example.com"), "hk3-a")
        b = _Node(_proxy("Foo", "b.example.com"), "hk3-a")
        c = _Node(_proxy("Foo", "c.example.com"), "hk3-a")
        body, _ = canonical_export.build([n.entry() for n in (a, b, c)],
                                         _cfg("{display_name}"))
        self.assertEqual(_names(body), ["Foo", "Foo #2", "Foo #3"])

    def test_original_name_recorded(self):
        _, meta = self._one("{country}", egress_country="HK")
        self.assertEqual(meta["renames"][0]["original_name"], "Foo")
        self.assertTrue(meta["renames"][0]["renamed"])


class ConnectionParamsTest(unittest.TestCase):
    def test_rename_never_touches_connection_fields(self):
        extra = {"uuid": "deadbeef-cafe-4242-8424-424242424242",
                 "password": "s3cret", "servername": "cdn.example.com",
                 "reality-opts": {"public-key": "pk", "short-id": "123456e2"},
                 "tls": True, DIALER: "front"}
        proxy = _proxy("Foo", "a.example.com", extra=extra)
        node = _Node(proxy, "hk3-a", egress_country="HK")
        body, _ = canonical_export.build([node.entry()], _cfg())
        out = yaml.safe_load(body)["proxies"][0]
        for key, value in extra.items():
            self.assertEqual(out[key], value)
        self.assertEqual(out["server"], "a.example.com")
        self.assertEqual(out["port"], 443)

    def test_rename_disabled_keeps_original_name(self):
        proxy = _proxy("[HK] Foo", "a.example.com")
        node = _Node(proxy, "hk3-a", egress_country="SG")
        body, meta = canonical_export.build([node.entry()],
                                            _cfg(enabled=False))
        self.assertEqual(_names(body), ["[HK] Foo"])
        self.assertFalse(meta["renames"][0]["renamed"])


class SortStabilityTest(unittest.TestCase):
    def test_order_is_country_protocol_name_fp(self):
        us = _Node(_proxy("zeta", "us.example.com"), "s1", egress_country="US")
        hk = _Node(_proxy("alpha", "hk.example.com"), "s1", egress_country="HK")
        hk2 = _Node(_proxy("beta", "hk2.example.com"), "s1", egress_country="HK")
        body, _ = canonical_export.build([n.entry() for n in (us, hk, hk2)], _cfg())
        self.assertEqual(_names(body), ["HK | alpha", "HK | beta", "US | zeta"])

    def test_shuffled_input_same_output(self):
        nodes = [
            _Node(_proxy("n%d" % i, "s%d.example.com" % i), "s1",
                  egress_country=("HK" if i % 2 else "US"), delay=100 + i)
            for i in range(8)
        ]
        first, _ = canonical_export.build([n.entry() for n in nodes], _cfg())
        for _ in range(5):
            shuffled = list(reversed(nodes))
            body, _ = canonical_export.build([n.entry() for n in shuffled], _cfg())
            self.assertEqual(body, first)


class StabilityTest(unittest.TestCase):
    def test_content_sha_stable_across_rounds(self):
        proxy = _proxy("Foo", "a.example.com")
        node = _Node(proxy, "hk3-a", egress_country="HK")
        sources = [node.entry()]
        body1, meta1 = canonical_export.build(sources, _cfg())
        body2, meta2 = canonical_export.build(sources, _cfg())
        self.assertEqual(body1, body2)
        self.assertEqual(meta1["content_sha256"], meta2["content_sha256"])
        self.assertEqual(meta1["digest"], meta2["digest"])
        text = body1.decode("utf-8")
        for volatile in ("generated_at", "round_id", "run_id", "updated_at"):
            self.assertNotIn(volatile, text)

    def test_uses_export_dumper_for_yaml_11_scalars(self):
        proxy = _proxy("Foo", "a.example.com",
                       extra={"short-id": "123456e2"})
        node = _Node(proxy, "hk3-a")
        body, _ = canonical_export.build([node.entry()], _cfg())
        self.assertIn("'123456e2'", body.decode("utf-8"))

    def test_reimports_clean(self):
        importlib.reload(canonical_export)
        self.assertTrue(hasattr(canonical_export, "build"))


DIALER = engine.DIALER_FIELD


def _cfg(template=None, enabled=True):
    rename = {"enabled": enabled,
              "template": template or "{country} | {display_name}",
              "collision_suffix": " #{n}",
              "preserve_original_name": True}
    return {"rename": rename, "gist": {}, "publish": {}}


if __name__ == "__main__":
    unittest.main()
