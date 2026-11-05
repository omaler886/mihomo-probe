"""gist_store 单测（指导 §8 发布规则 / §13 安全）。

覆盖：NoChange 零 PATCH、变化只 PATCH 变化文件、四文件固定、首次为空建齐四文件、
429/500 退避重试后成功、终态 401/403/404 不重试、失败返回 publish_failed 且旧产物
不受影响、token 从环境变量读 + 日志脱敏、history 截断到 100、transport 注入不触网。
"""
import json
import os
import unittest
import urllib.request
from unittest import mock

import tests._isolation as iso
from mihomo_test import engine
from mihomo_test import gist_store

iso  # noqa: B018


def setUpModule():
    iso.isolate()


def tearDownModule():
    iso.restore()


CANONICAL = b"proxies: []\n"
META = {"count": 0, "digest": "abc123", "content_sha256": "deadbeef"}


class StubTransport:
    """A GitHub Gist API that never touches the network.

    `errors` maps status -> how many times to answer with that status before
    falling through to normal behaviour, so one transport can simulate
    "429 twice then succeed".
    """

    def __init__(self, files=None, errors=None, revision=1):
        self.files = dict(files or {})
        self.errors = dict(errors or {})
        self.revision = revision
        self.requests = []

    def __call__(self, method, url, headers, data):
        self.requests.append((method, url))
        fail = self._maybe_fail()
        if fail is not None:
            return fail
        if method == "GET":
            return 200, {}, json.dumps({"files": self._files_payload(),
                                         "history_url":
                                             f"https://api.github.com/gists/g1/{self.revision}"}).encode()
        if method == "PATCH":
            body = json.loads(data)
            for name, entry in (body.get("files") or {}).items():
                self.files[name] = entry.get("content", "")
            self.revision += 1
            return 200, {}, json.dumps({
                "files": self._files_payload(),
                "history_url": f"https://api.github.com/gists/g1/{self.revision}",
            }).encode()
        return 405, {}, b"method not allowed"

    def _maybe_fail(self):
        """One queued failure per call, returned verbatim as a response.

        The transport owns HTTP semantics: `gh` with an injected transport does
        not retry (that is the real urllib path's job), so a stubbed 429 answers
        the single GET it was given and `publish` decides what to do with it.
        """
        for status in (429, 500, 503, 401, 403, 404):
            if self.errors.get(status, 0) > 0:
                self.errors[status] -= 1
                body = json.dumps({"message": f"stub {status}"}).encode()
                return status, {}, body
        return None

    def _files_payload(self):
        return {name: {"filename": name,
                       "sha": gist_store._sha_text(body),
                       "raw_url": f"https://gist/raw/{name}"}
                for name, body in self.files.items()}

    def patch_count(self):
        return sum(1 for method, _ in self.requests if method == "PATCH")

    def get_count(self):
        return sum(1 for method, _ in self.requests if method == "GET")


class _Ctx:
    def __init__(self):
        self.cfg = {"gist": {"enabled": True, "gist_id": "g1",
                             "token_env": "GITHUB_GIST_TOKEN",
                             "canonical_file": "mihomo-alive.yaml",
                             "status_file": "probe-status.json",
                             "history_file": "probe-history.jsonl"}}
        self.logs = []

    def log(self, level, message):
        self.logs.append((level, message))


def _run(transport, ctx=None, **kw):
    ctx = ctx or _Ctx()
    with mock.patch.dict(os.environ, {"GITHUB_GIST_TOKEN": "ghp_" + "A" * 30},
                         clear=False):
        result = gist_store.publish(ctx.cfg, CANONICAL, META, log=ctx.log,
                                    transport=transport, backoff=0, **kw)
    return ctx, result


class NoChangeTest(unittest.TestCase):
    def test_identical_payload_produces_zero_patch(self):
        transport = StubTransport()
        ctx, first = _run(transport, ctx=_Ctx(), status_payload={"round_id": 7},
                          history_text="{}\n")
        self.assertEqual(first["status"], "published")
        patches_after_first = transport.patch_count()
        ctx2, second = _run(transport, ctx=_Ctx(),
                            status_payload={"round_id": 7},
                            history_text="{}\n")
        self.assertEqual(second["status"], "no_change")
        self.assertEqual(transport.patch_count(), patches_after_first)

    def test_no_change_result_has_no_revision_bump(self):
        transport = StubTransport()
        _run(transport, ctx=_Ctx(), status_payload={}, history_text="")
        before = transport.revision
        ctx, result = _run(transport, ctx=_Ctx(), status_payload={},
                           history_text="")
        self.assertEqual(result["status"], "no_change")
        self.assertEqual(transport.revision, before)

    def test_status_only_change_is_zero_patch(self):
        # 指导 §8：幂等门禁钉在 canonical 文件上。status 里的 round_id /
        # updated_at 每轮都不同，但它们是随行文件，绝不允许单独制造 revision
        # -- 否则"内容不变零 PATCH"的门禁名存实亡。
        transport = StubTransport()
        _run(transport, ctx=_Ctx(), status_payload={"round_id": 7},
             history_text="")
        patches_after_first = transport.patch_count()
        ctx, result = _run(transport, ctx=_Ctx(), status_payload={"round_id": 8},
                           history_text="")
        self.assertEqual(result["status"], "no_change")
        self.assertEqual(transport.patch_count(), patches_after_first)


class FixedFilesTest(unittest.TestCase):
    def test_exactly_four_files(self):
        transport = StubTransport()
        _run(transport, ctx=_Ctx(), status_payload={}, history_text="")
        self.assertEqual(set(transport.files),
                         {"mihomo-alive.yaml", "probe-status.json",
                          "probe-history.jsonl", "README.md"})

    def test_existing_extra_file_left_alone(self):
        transport = StubTransport(files={"probe-hk3-a.yaml": "stale"})
        _run(transport, ctx=_Ctx(), status_payload={}, history_text="")
        # 禁止扩散：额外文件既不更新也不依赖；发布只写固定四文件。
        self.assertEqual(transport.files.get("probe-hk3-a.yaml"), "stale")
        self.assertEqual(len(transport.files), 5)

    def test_only_changed_files_patched(self):
        transport = StubTransport()
        ctx = _Ctx()
        _run(transport, ctx=ctx, status_payload={}, history_text="")
        after_first = dict(transport.files)
        # 只改 canonical 正文：digest/count 不变（README 与 status 只依赖它们），
        # 所以 PATCH 只能包含 canonical 一个文件。
        transport.requests = []
        with mock.patch.dict(os.environ,
                             {"GITHUB_GIST_TOKEN": "ghp_" + "A" * 30},
                             clear=False):
            result = gist_store.publish(
                ctx.cfg, b"proxies:\n- name: x\n  type: ss\n", META,
                log=ctx.log, transport=transport, backoff=0,
                status_payload={}, history_text="")
        self.assertEqual(result["status"], "published")
        self.assertEqual(result["changed"], ["mihomo-alive.yaml"])


class FirstPublishTest(unittest.TestCase):
    def test_empty_gist_creates_all_four(self):
        transport = StubTransport(files={})
        ctx, result = _run(transport, ctx=_Ctx(), status_payload={}, history_text="")
        self.assertEqual(result["status"], "published")
        self.assertEqual(transport.patch_count(), 1)
        self.assertEqual(len(transport.files), 4)


class RetryTest(unittest.TestCase):
    def test_429_backs_off_then_succeeds(self):
        transport = StubTransport(errors={429: 2})
        ctx, result = _run(transport, ctx=_Ctx(), status_payload={}, history_text="")
        self.assertEqual(result["status"], "published")
        # 2 个 429 + 1 GET + 1 PATCH
        self.assertEqual(transport.get_count(), 3)

    def test_500_on_read_never_reaches_patch(self):
        transport = StubTransport(errors={500: 3})
        ctx, result = _run(transport, ctx=_Ctx(), status_payload={}, history_text="")
        self.assertEqual(result["status"], "publish_failed")
        self.assertEqual(transport.patch_count(), 0)
        self.assertTrue(result["error_code"].startswith("GIST_HTTP_"))

    def test_401_terminal_no_retry(self):
        transport = StubTransport(errors={401: 1})
        ctx, result = _run(transport, ctx=_Ctx(), status_payload={}, history_text="")
        self.assertEqual(result["status"], "publish_failed")
        self.assertEqual(result["error_code"], "GIST_HTTP_401")
        self.assertEqual(transport.get_count(), 1)

    def test_403_terminal_no_retry(self):
        transport = StubTransport(errors={403: 1})
        _ctx, result = _run(transport, ctx=_Ctx(), status_payload={}, history_text="")
        self.assertEqual(result["error_code"], "GIST_HTTP_403")

    def test_404_terminal_no_retry(self):
        transport = StubTransport(errors={404: 1})
        _ctx, result = _run(transport, ctx=_Ctx(), status_payload={}, history_text="")
        self.assertEqual(result["error_code"], "GIST_HTTP_404")

    def test_failure_does_not_touch_local_artifact(self):
        # 失败返回 publish_failed，stub 状态（旧产物）保持不变。
        transport = StubTransport(files={
            "mihomo-alive.yaml": "old",
            "README.md": "old",
            "probe-status.json": "old",
            "probe-history.jsonl": "old",
        }, errors={500: 3})
        ctx, result = _run(transport, ctx=_Ctx(), status_payload={}, history_text="")
        self.assertEqual(result["status"], "publish_failed")
        self.assertEqual(transport.files["mihomo-alive.yaml"], "old")
        self.assertEqual(len(transport.files), 4)


class TokenTest(unittest.TestCase):
    def test_missing_token_is_config_error(self):
        ctx = _Ctx()
        env = {k: v for k, v in os.environ.items()
               if k not in ("GITHUB_GIST_TOKEN", "GITHUB_TOKEN_FILE")}
        with mock.patch.dict(os.environ, env, clear=True):
            result = gist_store.publish(ctx.cfg, CANONICAL, META, log=ctx.log,
                                        transport=StubTransport(), backoff=0)
        self.assertEqual(result["status"], "publish_failed")
        self.assertEqual(result["error_code"], "GIST_CONFIG_ERROR")

    def test_token_from_file_fallback(self):
        token_file = iso._state["root"] / "token.txt"
        token_file.write_text("ghp_" + "B" * 30, encoding="utf-8")
        transport = StubTransport()
        ctx = _Ctx()
        env = dict(os.environ)
        env.pop("GITHUB_GIST_TOKEN", None)
        env["GITHUB_TOKEN_FILE"] = str(token_file)
        with mock.patch.dict(os.environ, env, clear=True):
            result = gist_store.publish(ctx.cfg, CANONICAL, META, log=ctx.log,
                                        transport=transport, backoff=0,
                                        status_payload={}, history_text="")
        self.assertEqual(result["status"], "published")

    def test_logs_mask_pat(self):
        transport = StubTransport(errors={401: 1})
        ctx = _Ctx()
        pat = "github_pat_" + "C" * 30
        env = dict(os.environ)
        env["GITHUB_GIST_TOKEN"] = pat
        env.pop("GITHUB_TOKEN_FILE", None)
        with mock.patch.dict(os.environ, env, clear=True):
            gist_store.publish(ctx.cfg, CANONICAL, META, log=ctx.log,
                               transport=transport, backoff=0,
                               status_payload={}, history_text="")
        joined = "\n".join(message for _, message in ctx.logs)
        self.assertNotIn(pat, joined)
        self.assertIn("github_pat_***", joined)

    def test_error_payload_masked(self):
        transport = StubTransport(errors={403: 1})
        ctx = _Ctx()
        pat = "ghp_" + "D" * 30
        env = dict(os.environ)
        env["GITHUB_GIST_TOKEN"] = pat
        env.pop("GITHUB_TOKEN_FILE", None)
        with mock.patch.dict(os.environ, env, clear=True):
            result = gist_store.publish(ctx.cfg, CANONICAL, META, log=ctx.log,
                                        transport=transport, backoff=0,
                                        status_payload={}, history_text="")
        self.assertNotIn(pat, json.dumps(result))
        self.assertNotIn(pat, "\n".join(m for _, m in ctx.logs))


class HistoryTest(unittest.TestCase):
    def test_history_truncated_to_100(self):
        rows = [{"id": i, "started_at": f"2026-01-01T00:{i:02d}:00",
                 "finished_at": None, "trigger": "manual", "total": 3,
                 "ok": 2, "failed": 1, "dropped": 0, "restored": 0,
                 "suspect": 0} for i in range(250)]

        class _Db:
            @staticmethod
            def query(sql, params=()):
                assert params[0] == 100
                return rows[:params[0]]

        text = gist_store.build_history(_Db)
        lines = [ln for ln in text.splitlines() if ln.strip()]
        self.assertEqual(len(lines), 100)
        # 保留最近 100 轮：最新一行 round_id=99（rows 已按 id DESC 给出前 100 条）。
        self.assertEqual(json.loads(lines[-1])["round_id"], 99)
        self.assertEqual(json.loads(lines[0])["round_id"], 0)
        for line in lines:
            record = json.loads(line)
            self.assertNotIn("proxy", record)
            self.assertNotIn("uuid", record)

    def test_history_text_passed_through_truncated(self):
        lines = "\n".join(json.dumps({"round_id": i}) for i in range(150))
        truncated = gist_store._truncate_history(lines)
        kept = [ln for ln in truncated.splitlines() if ln.strip()]
        self.assertEqual(len(kept), 100)
        self.assertEqual(json.loads(kept[-1])["round_id"], 149)


class ReadmeTest(unittest.TestCase):
    def test_readme_warns_secret_not_private(self):
        body = gist_store.build_readme(_Ctx().cfg, {"updated_at": "x"},
                                       {"count": 3, "digest": "d"})
        self.assertIn("secret 不等于私密", body)
        self.assertIn("mihomo-alive.yaml", body)
        self.assertIn("Sub-Store", body)


class EngineGistWiringTest(unittest.TestCase):
    """engine._publish_gist 接线（P4）：meta 透传、失败不改账本、关闭不触发。"""

    def setUp(self):
        self.logs = []

    def _log(self, level, message):
        self.logs.append((level, message))

    def _cfg(self, enabled=True):
        return {"gist": {"enabled": enabled, "gist_id": "g1",
                         "token_env": "GITHUB_GIST_TOKEN"}}

    @staticmethod
    def _canonical():
        return CANONICAL, dict(META)

    def test_meta_digest_reaches_the_publisher(self):
        # 回归钉：_write_canonical_export 产出的 (content, meta) 必须成对走到
        # gist_store.publish —— digest 丢了，status/README 与幂等门禁全瞎。
        captured = {}

        def fake_publish(cfg, content, meta, log=None, status_payload=None,
                         history_text=None, **kw):
            captured["content"], captured["meta"] = content, meta
            captured["status_payload"] = status_payload
            return {"status": "published", "revision": "r1",
                    "digest": (meta or {}).get("digest"),
                    "content_sha256": "x", "changed": ["mihomo-alive.yaml"]}

        with mock.patch.object(gist_store, "publish", fake_publish):
            note = engine._publish_gist(self._cfg(), 7, self._canonical(),
                                        10, 9, False, None, self._log)
        self.assertIsNone(note)
        self.assertEqual(captured["content"], CANONICAL)
        self.assertEqual(captured["meta"]["digest"], META["digest"])
        self.assertEqual(captured["status_payload"]["digest"], META["digest"])

    def test_publish_failure_returns_failure_note(self):
        def fake_publish(cfg, *args, **kw):
            return {"status": "publish_failed", "error_code": "GIST_HTTP_500",
                    "message": "GitHub 返回 500"}

        with mock.patch.object(gist_store, "publish", fake_publish):
            note = engine._publish_gist(self._cfg(), 7, self._canonical(),
                                        10, 9, False, None, self._log)
        # 指导 §8：API 失败不得影响账本结算，UI 显示「本轮完成，发布失败」。
        self.assertIn("发布失败", note)

    def test_disabled_gist_never_calls_publish(self):
        def fail_publish(*args, **kw):
            raise AssertionError("gist disabled: publish must not be called")

        with mock.patch.object(gist_store, "publish", fail_publish):
            note = engine._publish_gist(self._cfg(enabled=False), 7,
                                        self._canonical(), 10, 9, False,
                                        "kept", self._log)
        self.assertEqual(note, "kept")


if __name__ == "__main__":
    unittest.main()
