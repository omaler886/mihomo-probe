"""ipmap 的离线单元测试:族过滤、回显解析、标注、报告、来源解析。

全部无网络、无 docker:probe_one 的内核与回显请求都被 mock,只验证逻辑。
起真内核、经真节点 curl 的部分由实战覆盖(vps 上 `python3 -m mihomo_test ipmap`)。
"""
import base64
import os
import sys
import unittest
import unittest.mock
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import core as coremod
from mihomo_test import ipmap

try:
    import _isolation
except ImportError:  # imported as a package
    from tests import _isolation


def setUpModule():
    _isolation.isolate()


def tearDownModule():
    _isolation.restore()


V6 = "2001:db8:1001:175:807a:dfff:fe59:e901"
V4 = "198.51.100.7"


def _node(name, server, port=50049, extra=None):
    proxy = {"name": name, "type": "vless", "server": server, "port": port,
             "uuid": "00000000-0000-4000-8000-000000000000", "tls": True}
    proxy.update(extra or {})
    return proxy


class FamilyTest(unittest.TestCase):
    def test_literal_v4(self):
        self.assertEqual(ipmap.server_family("192.0.2.10"), "v4")

    def test_literal_v6(self):
        self.assertEqual(ipmap.server_family(V6), "v6")

    def test_domain_and_empty(self):
        self.assertEqual(ipmap.server_family("example.com"), "domain")
        self.assertEqual(ipmap.server_family(""), "domain")
        self.assertEqual(ipmap.server_family(None), "domain")


class FilterTest(unittest.TestCase):
    def setUp(self):
        self.proxies = [
            _node("v4节点", "192.0.2.10"),
            _node("v6节点", V6),
            _node("域名节点", "example.com"),
        ]

    def test_v6_keeps_only_v6_literals(self):
        kept = ipmap.filter_family(self.proxies, "v6")
        self.assertEqual([p["name"] for p in kept], ["v6节点"])

    def test_v4_keeps_only_v4_literals(self):
        kept = ipmap.filter_family(self.proxies, "v4")
        self.assertEqual([p["name"] for p in kept], ["v4节点"])

    def test_all_keeps_everything_and_does_not_mutate(self):
        kept = ipmap.filter_family(self.proxies, "all")
        self.assertEqual(len(kept), 3)
        self.assertEqual(len(self.proxies), 3)


class EchoParseTest(unittest.TestCase):
    def test_plain_and_trailing_newline(self):
        self.assertEqual(ipmap.parse_echo_ip(V4 + "\n", "v4"), V4)
        self.assertEqual(ipmap.parse_echo_ip(V6, "v6"), V6)

    def test_family_mismatch_is_rejected(self):
        # v6 端点前面挂了双栈前置、回了 v4 地址:这不是本族的答案。
        self.assertIsNone(ipmap.parse_echo_ip(V4, "v6"))
        self.assertIsNone(ipmap.parse_echo_ip(V6, "v4"))

    def test_garbage_and_empty(self):
        self.assertIsNone(ipmap.parse_echo_ip("<html>blocked</html>", "v4"))
        self.assertIsNone(ipmap.parse_echo_ip("", "v4"))
        self.assertIsNone(ipmap.parse_echo_ip(None, "v4"))
        self.assertIsNone(ipmap.parse_echo_ip("1.2.3.999", "v4"))

    def test_any_family_accepts_both(self):
        self.assertEqual(ipmap.parse_echo_ip(V4, None), V4)
        self.assertEqual(ipmap.parse_echo_ip(V6, None), V6)


class _FakeCore:
    def __init__(self, fail_select=False):
        self.selected = []
        self.fail_select = fail_select

    def select(self, group, name):
        if self.fail_select:
            raise coremod.CoreError(f"could not select {name}: HTTP 500")
        self.selected.append((group, name))


class ProbeOneTest(unittest.TestCase):
    def _patch_fetch(self, answers):
        return unittest.mock.patch.object(
            ipmap, "fetch_via",
            side_effect=lambda port, url, timeout_s: answers[url])

    def test_full_success(self):
        core = _FakeCore()
        answers = {
            "http://api.ipify.org/": V4 + "\n",
            "http://api6.ipify.org/": V6,
            ipmap.TRACE_URL: f"ip={V6}\nloc=US\ncolo=LAX\n",
        }
        with self._patch_fetch(answers):
            row = ipmap.probe_one(core, 19300, "__LANE0__", "节点A", 12)
        self.assertEqual(core.selected, [("__LANE0__", "节点A")])
        self.assertEqual(row["exit_v4"], V4)
        self.assertEqual(row["exit_v6"], V6)
        self.assertEqual(row["country"], "US")
        self.assertEqual(row["colo"], "LAX")
        self.assertEqual(row["trace_ip"], V6)
        self.assertEqual(row["errors"], {})

    def test_v4_failure_keeps_v6_answer(self):
        core = _FakeCore()
        answers = {
            "http://api6.ipify.org/": V6,
            ipmap.TRACE_URL: f"ip={V6}\nloc=JP\ncolo=NRT\n",
        }
        with self._patch_fetch(answers):
            row = ipmap.probe_one(core, 19300, "__LANE0__", "节点A", 12)
        self.assertIsNone(row["exit_v4"])
        self.assertEqual(row["exit_v6"], V6)
        self.assertEqual(row["country"], "JP")
        self.assertIn("v4", row["errors"])

    def test_select_failure_raises(self):
        core = _FakeCore(fail_select=True)
        with self.assertRaises(coremod.CoreError):
            ipmap.probe_one(core, 19300, "__LANE0__", "节点A", 12)


class AnnotateTest(unittest.TestCase):
    def test_full(self):
        row = {"exit_v6": V6, "exit_v4": V4, "country": "US", "errors": {}}
        self.assertEqual(
            ipmap.annotate_name("原名", row),
            f"原名 · [US] {V6} {V4}")

    def test_no_country_no_v4(self):
        row = {"exit_v6": V6, "country": None, "errors": {}}
        self.assertEqual(ipmap.annotate_name("原名", row), f"原名 · {V6}")

    def test_failure_names_the_stage(self):
        row = {"exit_v4": None, "exit_v6": None,
               "errors": {"v6": "http://api6.ipify.org/ URLError"}}
        self.assertIn("实测失败", ipmap.annotate_name("原名", row))
        self.assertIn("api6", ipmap.annotate_name("原名", row))

    def test_empty_row(self):
        self.assertIn("实测失败", ipmap.annotate_name("原名", {}))


class AnnotatedProxiesTest(unittest.TestCase):
    def test_original_name_kept_and_fields_untouched(self):
        entries = [
            {"source": "ipmap", "name": "同名节点", "index": 0,
             "proxy": _node("同名节点", V6, extra={"tls": True})},
            {"source": "ipmap", "name": "同名节点", "index": 1,
             "proxy": _node("同名节点", "2607:8700:360:2b50::2", port=50013)},
        ]
        proxies, mapping, dropped = coremod.prepare(entries)
        self.assertEqual(dropped, [])
        rows = {mapping[0]["mihomo"]: {"exit_v6": V6, "country": "JP", "errors": {}},
                mapping[1]["mihomo"]: {"exit_v4": None, "exit_v6": None,
                                       "errors": {"v6": "timeout"}}}
        out = ipmap.annotated_proxies(mapping, rows)
        # 原名保留(不带 prepare 的去重后缀),server/port/uuid 原样。
        self.assertTrue(out[0]["name"].startswith("同名节点 · "))
        self.assertIn(V6, out[0]["name"])
        self.assertEqual(out[0]["server"], V6)
        self.assertEqual(out[0]["port"], 50049)
        self.assertIn("实测失败", out[1]["name"])
        self.assertEqual(out[1]["server"], "2607:8700:360:2b50::2")


class ExtractProxiesTest(unittest.TestCase):
    YAML = ("proxies:\n"
            "  - {name: a, type: vless, server: 2001:db8::1, port: 1}\n"
            "  - {name: b, type: vless, server: 1.2.3.4, port: 2}\n")

    def test_plain_yaml(self):
        out = ipmap.extract_proxies(self.YAML)
        self.assertEqual(len(out), 2)
        self.assertEqual(out[0]["name"], "a")

    def test_base64_wrapped_yaml(self):
        wrapped = base64.b64encode(self.YAML.encode()).decode()
        out = ipmap.extract_proxies(wrapped)
        self.assertEqual(len(out), 2)

    def test_share_links_are_refused_with_direction(self):
        body = "vless://uuid@example.com:443?type=ws\nvless://uuid@example.org:443"
        with self.assertRaises(ipmap.ImapError) as ctx:
            ipmap.extract_proxies(body)
        self.assertIn("--source", str(ctx.exception))

    def test_yaml_without_proxies_is_refused(self):
        with self.assertRaises(ipmap.ImapError):
            ipmap.extract_proxies("mixed-port: 7890\nmode: rule\n")

    def test_garbage_is_refused(self):
        with self.assertRaises(ipmap.ImapError):
            ipmap.extract_proxies("this is not a subscription body at all")


class LandingNoteTest(unittest.TestCase):
    def test_direct_landing(self):
        m = {"server": V6}
        row = {"exit_v6": V6, "exit_v4": None}
        self.assertIn("直落", ipmap.landing_note(m, row))

    def test_direct_landing_ignores_leading_zero_compression(self):
        # 订阅写 04fa,回显正文规范化成 4fa:同一个地址,必须判直落。
        m = {"server": "2605:52c0:1:f66:04fa:27ff:fed5:bfac"}
        row = {"exit_v6": "2605:52c0:1:f66:4fa:27ff:fed5:bfac", "exit_v4": None}
        self.assertIn("直落", ipmap.landing_note(m, row))

    def test_relayed(self):
        m = {"server": "2405:84c0:8025:8000::"}
        row = {"exit_v6": "2606:4700::1", "exit_v4": None}
        self.assertIn("中转", ipmap.landing_note(m, row))

    def test_domain_entry(self):
        m = {"server": "example.com"}
        row = {"exit_v6": V6, "exit_v4": V4}
        self.assertIn("域名", ipmap.landing_note(m, row))

    def test_failure(self):
        m = {"server": V6}
        self.assertIn("实测失败", ipmap.landing_note(m, {}))

    def test_other_family_only(self):
        m = {"server": V6}
        row = {"exit_v6": None, "exit_v4": V4}
        self.assertIn("另一族", ipmap.landing_note(m, row))


class SummarizeTest(unittest.TestCase):
    def test_counts_by_note(self):
        mapping = [
            {"mihomo": "a", "server": V6},
            {"mihomo": "b", "server": "2405:84c0:8025:8000::"},
            {"mihomo": "c", "server": "2a12:a304:4:3d::a"},
        ]
        rows = {
            "a": {"exit_v6": V6, "exit_v4": None},
            "b": {"exit_v6": "2606:4700::1", "exit_v4": None},
            "c": {"exit_v6": None, "exit_v4": None, "errors": {"v6": "timeout"}},
        }
        direct, relayed, failed = ipmap.summarize(mapping, rows)
        self.assertEqual((direct, relayed, failed), (1, 1, 1))


class ReportTest(unittest.TestCase):
    def test_report_has_mapping_columns_and_escapes_pipes(self):
        mapping = [
            {"mihomo": "a", "original": "带|竖线", "server": V6},
        ]
        rows = {"a": {"exit_v6": V6, "exit_v4": V4, "country": "US",
                      "colo": "LAX", "errors": {}}}
        text = ipmap.render_report("测试源", mapping, rows)
        self.assertIn("写明的 server", text)
        self.assertIn("带\\|竖线", text)
        self.assertIn(V6, text)
        self.assertIn("US/LAX", text)
        self.assertIn("直落", text)


if __name__ == "__main__":
    unittest.main()
