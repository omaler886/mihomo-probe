"""Read-only work generator for the autonomous loop.

Answers "what should the next iteration do?" without a human supplying a goal.
It scans the repository and (when a ledger is reachable) the live deployment,
then prints a ranked backlog of concrete, individually verifiable candidates --
each with a stable ID so the loop can dedupe work it already finished.

    python3 tools/loop_scan.py                 # print the backlog
    python3 tools/loop_scan.py --write         # also write .local/loop/BACKLOG.md
    MIHOMO_TEST_ROOT=/srv/mihomo-test python3 tools/loop_scan.py --live

    # against the live deployment (read-only)
    ssh -o BatchMode=yes vps 'docker exec -i mihomo-test python3 -' < tools/loop_scan.py

Categories, and what each one is for:

    LIVE    anomalies in the liveness ledger (batch deaths, undecided nodes,
            suspect rounds, alert-worthy events) -- the highest-value work,
            because it is the product's actual verdict that is in question.
    VERDICT does the failure-classification vocabulary agree with itself
            (produced reasons vs tested reasons vs documented reasons)?
    CONFIG  every declared config key is actually read somewhere?
    SILENT  exception handlers that swallow errors with no log line.
    TESTS   duplicate test names (silent no-op tests) and uncovered branches.
    DOC     README commands/identifiers that no longer exist in the code.
    SIZE    oversized functions (refactor candidates, lowest priority).

Never mutates anything and never talks to the network.
"""
import argparse
import ast
import json
import os
import re
import subprocess
import sys
from collections import Counter, defaultdict
from pathlib import Path

# Windows consoles default to a legacy code page; make the report printable
# regardless of where the scanner runs.
try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except (AttributeError, OSError):  # pragma: no cover
    pass


def _find_root():
    """Locate the project directory.

    `python3 -` (the way this script is often run inside the container) has no
    usable __file__, so falling back on it silently points at the wrong tree --
    which once made every config key look unread. Trust, in order: the app's own
    MIHOMO_TEST_ROOT, the script location, the working directory.
    """
    candidates = []
    if os.environ.get("MIHOMO_TEST_ROOT"):
        candidates.append(Path(os.environ["MIHOMO_TEST_ROOT"]))
    try:
        candidates.append(Path(__file__).resolve().parent.parent)
    except NameError:  # pragma: no cover - stdin execution
        pass
    candidates.append(Path.cwd())
    candidates.append(Path("/srv/mihomo-test"))
    for cand in candidates:
        if (cand / "mihomo_test" / "engine.py").exists():
            return cand
    return candidates[0]


ROOT = _find_root()
sys.path.insert(0, str(ROOT))

MAX_FUNC_LINES = 80
DEAD_BATCH_MIN = 5          # same source+reason, this many nodes = worth reviewing
LONG_STREAK_MIN = 6         # rounds of consecutive failure = "batch" territory

SEV_ORDER = {"HIGH": 0, "MED": 1, "LOW": 2}


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------

def _read(path):
    try:
        return Path(path).read_text(encoding="utf-8")
    except OSError:
        return ""


def pkg_files():
    return sorted((ROOT / "mihomo_test").glob("*.py"))
def test_files():
    return sorted((ROOT / "tests").glob("test_*.py"))


def pkg_text(include_tests=False):
    """Concatenated source of the package (and optionally the tests)."""
    paths = pkg_files() + (test_files() if include_tests else [])
    return "\n".join(_read(p) for p in paths)


def finding(cat, sev, tid, title, evidence, experiment):
    return {"cat": cat, "sev": sev, "id": tid, "title": title,
            "evidence": evidence, "experiment": experiment}


# --------------------------------------------------------------------------
# VERDICT -- reason vocabulary consistency
# --------------------------------------------------------------------------

# Reasons the pipeline can attach to a node. Collected from the classifier
# itself (see produced_reasons) rather than hardcoded, so a new branch in
# _reason_from shows up here automatically.
def produced_reasons():
    """The failure vocabulary the pipeline can actually attach to a node.

    Scoped to the classifier and to explicit ledger writes -- a bare "return a
    string" anywhere in the package also matches action names ("reloaded",
    "restarted"), which are not verdicts and would be pure noise here.
    """
    reasons = set()
    core_path = ROOT / "mihomo_test" / "core.py"
    try:
        tree = ast.parse(_read(core_path))
    except SyntaxError:
        tree = None
    for node in ast.walk(tree) if tree else []:
        if isinstance(node, ast.FunctionDef) and node.name == "_reason_from":
            for sub in ast.walk(node):
                if not isinstance(sub, ast.Return) or sub.value is None:
                    continue
                value = sub.value
                first = value.elts[0] if isinstance(value, ast.Tuple) and value.elts else value
                if isinstance(first, ast.Constant):
                    reasons.add(str(first.value))
                elif isinstance(first, ast.JoinedStr):
                    reasons.add("http_<status>")
    engine = _read(ROOT / "mihomo_test" / "engine.py")
    reasons |= set(re.findall(r'return None, "([a-z_]+)"', engine))
    reasons |= set(re.findall(r'last_reason="([a-z_]+)"', engine))
    # TERMINAL_REASONS deliberately excluded: it lists values the classifier
    # must treat as final, including defensive ones core never returns. Those
    # are checked separately by scan_verdicts.
    return {r for r in reasons if r not in ("ok", "unknown", "probe")}


def reason_is_asserted(reason, tests):
    """Is this reason string asserted somewhere in the test suite?

    A template reason like ``http_<status>`` can never appear literally in a
    test -- the code builds it with an f-string -- so a literal search always
    missed it and the scan reported it as untested forever, no matter how many
    concrete cases were covered. For a template, accept a concrete
    instantiation instead (``http_418``).
    """
    if "<" in reason and ">" in reason:
        head = reason.split("<", 1)[0]
        return bool(re.search(rf'["\']{re.escape(head)}\d+["\']', tests))
    return f'"{reason}"' in tests or f"'{reason}'" in tests


def scan_verdicts():
    out = []
    reasons = produced_reasons()
    code = pkg_text()
    tests = "\n".join(_read(p) for p in test_files())
    readme = _read(ROOT / "README.md")

    untested = sorted(r for r in reasons if not reason_is_asserted(r, tests))
    if untested:
        out.append(finding(
            "VERDICT", "MED", "VERDICT-untested-reasons",
            f"{len(untested)} 个失败分类没有任何单测断言",
            f"reasons={untested}",
            "给每个未覆盖的分类各写一条离线用例（构造内核响应体 → 断言 reason），"
            "并反证：临时改掉分类逻辑，确认用例真的失败。"))

    # A reason the code can never produce but the docs promise (or vice versa).
    documented = set(re.findall(r"`([a-z]+(?:_[a-z]+)+)`", readme))
    ghost = sorted(t for t in documented if t not in code and t not in tests)
    if ghost:
        out.append(finding(
            "VERDICT", "MED", "VERDICT-doc-ghost-identifiers",
            f"README 提到的 {len(ghost)} 个标识符在代码里不存在",
            f"ghost={ghost}",
            "逐个核对：要么是文档残留（删/改文档），要么是漏实现的机制（补代码 + 用例）。"))

    terminal = re.findall(r"TERMINAL_REASONS\s*=\s*\{([^}]*)\}", code)
    if terminal:
        names = set(re.findall(r'"([a-z_]+)"', terminal[0]))
        never = sorted(n for n in names if f'"{n}"' not in _read(ROOT / "mihomo_test" / "core.py"))
        if never:
            out.append(finding(
                "VERDICT", "LOW", "VERDICT-terminal-never-produced",
                f"TERMINAL_REASONS 里有 {len(never)} 个值内核永不返回",
                f"terminal-only={never}（core._reason_from 不产出它们）",
                "确认是防御性冗余（保留并注释）还是分类改名后的残留（删掉）。"))
    return out


# --------------------------------------------------------------------------
# CONFIG -- declared keys that nothing reads
# --------------------------------------------------------------------------

def _leaf_paths(node, prefix=""):
    if isinstance(node, dict):
        for key, value in node.items():
            yield from _leaf_paths(value, f"{prefix}{key}.")
    else:
        yield prefix[:-1]


def scan_config_keys():
    """A declared key that no code path reads is either dead config or a
    switch wired into the UI but never honoured (this class of bug shipped
    twice: verify.entry_check and verify.domain_pass)."""
    try:
        from mihomo_test import config
    except Exception as exc:  # pragma: no cover - import must not break the scan
        return [finding("CONFIG", "LOW", "CONFIG-import-failed",
                        "无法导入 config 模块", str(exc), "先让包可导入。")]
    code = "\n".join(_read(p) for p in pkg_files() if p.name != "config.py")
    code += "\n" + "\n".join(_read(p) for p in (ROOT / "tools").glob("*.py"))
    out = []
    dead = []
    for path in _leaf_paths(config.DEFAULTS):
        leaf = path.rsplit(".", 1)[-1]
        if not re.search(rf'["\']{re.escape(leaf)}["\']', code):
            dead.append(path)
    # Nothing is excluded by name any more.
    #
    # This used to skip `backend` / `config_path` / `container_config_path` on
    # the grounds that env-var and path keys are "legitimate config even when
    # only referenced in config.py itself". Matching on the *leaf* name could
    # not tell that apart from a genuinely unread key, so it silently hid
    # `core.config_path` -- a key with no reader anywhere, which the scan is
    # supposed to surface. The keys are now either read (and so not reported)
    # or removed from DEFAULTS, and the exclusion is gone with them.
    if dead:
        out.append(finding(
            "CONFIG", "MED", "CONFIG-dead-keys",
            f"{len(dead)} 个配置项没有任何代码读取（疑似死开关）",
            "keys=" + ", ".join(sorted(dead)),
            "逐个确认：能改行为的接线（改代码 + 单测），改不了行为的从 DEFAULTS 与面板移除。"))
    return out


# --------------------------------------------------------------------------
# SILENT -- swallowed exceptions
# --------------------------------------------------------------------------

def scan_silent_except():
    """Find handlers that swallow an error outright: no log line, no re-raise,
    no fallback return. Those are the ones that turn a broken pipeline into a
    silent no-op -- the failure mode that let the legacy crons spin for weeks."""
    out = []
    offenders = []
    for path in pkg_files():
        try:
            tree = ast.parse(_read(path))
        except SyntaxError:
            continue
        for node in ast.walk(tree):
            if not isinstance(node, ast.ExceptHandler):
                continue
            # `pass` parses as ast.Pass, not ast.Expr, so filtering only on
            # "Expr with a constant" let every `except ...: pass` through and
            # the check never fired -- the exact case this finding is named
            # after. A docstring or bare constant counts as a no-op too.
            meaningful = [
                s for s in node.body
                if not isinstance(s, ast.Pass)
                and not (isinstance(s, ast.Expr) and isinstance(s.value, ast.Constant))
            ]
            if not meaningful:
                offenders.append(f"{path.name}:{node.lineno}")
    if offenders:
        out.append(finding(
            "SILENT", "MED", "SILENT-swallowed-except",
            f"{len(offenders)} 处 except 仅 `pass`（错误被完全吞掉，既不记日志也不上抛）",
            "at=" + ", ".join(offenders[:15]) + (" ..." if len(offenders) > 15 else ""),
            "对每一处判断：这里吞掉异常是否合理？不合理的补一行 log 并加用例证明会记日志。"
            "（本项目的历史教训：旧管线的静默失败让整条链路空转数周无人发现。）"))

    # Weaker signal: a handler that returns a fallback without logging and
    # without re-raising. Often deliberate, but when it hides a real fault the
    # caller cannot tell "no data" from "broken".
    fallbacks = []
    for path in pkg_files():
        try:
            tree = ast.parse(_read(path))
        except SyntaxError:
            continue
        for node in ast.walk(tree):
            if not isinstance(node, ast.ExceptHandler):
                continue
            logs = any(
                isinstance(s, ast.Expr) and isinstance(s.value, ast.Call)
                and getattr(s.value.func, "attr", "") in ("log", "warning", "error", "exception")
                for s in node.body)
            reraises = any(isinstance(s, ast.Raise) for s in node.body)
            returns = any(isinstance(s, ast.Return) for s in node.body)
            if returns and not logs and not reraises:
                fallbacks.append(f"{path.name}:{node.lineno}")
    if fallbacks:
        out.append(finding(
            "SILENT", "LOW", "SILENT-unlogged-fallback",
            f"{len(fallbacks)} 处 except 不记日志就返回兑底值",
            "at=" + ", ".join(fallbacks[:12]) + (" ..." if len(fallbacks) > 12 else ""),
            "抽查最危险的几处（涉及账本写入、发布、锁释放的）：调用方能否区分"
            "「没数据」与「出错了」？不能就补日志或用例钉住行为。"))
    return out


# --------------------------------------------------------------------------
# TESTS -- duplicate names and uncovered reasons
# --------------------------------------------------------------------------

def scan_tests():
    out = []
    dupes, totals = [], Counter()
    per_class = defaultdict(set)
    for path in test_files():
        try:
            tree = ast.parse(_read(path))
        except SyntaxError:
            continue
        for node in ast.walk(tree):
            if not isinstance(node, ast.ClassDef):
                continue
            for sub in node.body:
                if isinstance(sub, (ast.FunctionDef, ast.AsyncFunctionDef)) \
                        and sub.name.startswith("test"):
                    totals[f"{path.name}::{node.name}"] += 1
                    if sub.name in per_class[(path.name, node.name)]:
                        dupes.append(f"{path.name}::{node.name}.{sub.name}:{sub.lineno}")
                    per_class[(path.name, node.name)].add(sub.name)
    if dupes:
        out.append(finding(
            "TESTS", "HIGH", "TESTS-duplicate-names",
            f"{len(dupes)} 个重名测试（Python 只保留最后一个，前面的永不执行）",
            "at=" + ", ".join(dupes[:8]),
            "重命名或合并；并反证：删掉其中一个后跑该文件，确认测试数确实变化。"))
    biggest = totals.most_common(3)
    if biggest and biggest[0][1] < 10:
        out.append(finding(
            "TESTS", "LOW", "TESTS-thin-classes",
            "最大测试类也偏小，可能有大片未覆盖分支",
            ", ".join(f"{k}={v}" for k, v in biggest),
            "用 python3 -m trace 或人工过一遍 engine._score_bucket / _converge_bucket 的分支，"
            "挑一条没有用例的路径补上。"))
    return out


# --------------------------------------------------------------------------
# DOC -- README drift
# --------------------------------------------------------------------------

def scan_docs():
    out = []
    readme = _read(ROOT / "README.md")
    if not readme:
        return out

    main = _read(ROOT / "mihomo_test" / "__main__.py")
    choices = re.search(r"choices=\[([^\]]*)\]", main)
    known = set(re.findall(r'"([a-z-]+)"', choices.group(1))) if choices else set()
    claimed = set(re.findall(r"-m\s+mihomo_test\s+([a-z][a-z-]*)", readme))
    missing = sorted(claimed - known)
    if missing:
        out.append(finding(
            "DOC", "MED", "DOC-missing-cli",
            f"README 写了 {len(missing)} 个不存在的子命令",
            f"missing={missing}, actual={sorted(known)}",
            "改 README 或补子命令；两边对齐后跑一次 `-m mihomo_test <cmd> --help` 验证。"))

    compose = _read(ROOT / "docker-compose.yml")
    services = set(re.findall(r"^\s{2}([a-z][a-z0-9-]*):", compose, re.M))
    named = set(re.findall(r"`([a-z][a-z0-9-]*(?:-probe|-test))`", readme))
    ghost = sorted(named - services)
    if ghost:
        out.append(finding(
            "DOC", "LOW", "DOC-ghost-services",
            f"README 提到 {len(ghost)} 个 compose 里不存在的服务",
            f"ghost={ghost}, actual={sorted(services)}",
            "对齐文档与实际 compose 服务名。"))
    return out


# --------------------------------------------------------------------------
# SIZE -- refactor candidates
# --------------------------------------------------------------------------

def scan_sizes():
    out = []
    long_funcs = []
    for path in pkg_files():
        text = _read(path)
        try:
            tree = ast.parse(text)
        except SyntaxError:
            continue
        for node in ast.walk(tree):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                span = (node.end_lineno or node.lineno) - node.lineno
                if span > MAX_FUNC_LINES:
                    long_funcs.append(f"{path.name}:{node.lineno} {node.name} ({span} 行)")
    if long_funcs:
        out.append(finding(
            "SIZE", "LOW", "SIZE-long-functions",
            f"{len(long_funcs)} 个函数超过 {MAX_FUNC_LINES} 行",
            "at=" + ", ".join(long_funcs[:8]),
            "只在有测试保护的模块里拆分；每拆一步跑一次全量单测，保持行为不变。"))
    return out


# --------------------------------------------------------------------------
# LIVE -- anomalies in the liveness ledger
# --------------------------------------------------------------------------

# Sent over ssh to read the live ledger without needing the project tree (the
# app image deliberately ships only mihomo_test/ and tests/). One JSON line on
# stdout, marked so ssh's own noise can be ignored. SQL stays on single lines:
# this is pasted into a heredoc-free `python3 -`, and the project has been
# bitten before by shell/quoting mangling of inline Python.
_LEDGER_DUMP = (
    "import json\n"
    "from mihomo_test import db\n"
    "payload = {\n"
    "    'stats': db.stats(),\n"
    "    'nodes': db.query('SELECT source, display, status, last_reason, consec_fail, country FROM nodes'),\n"
    "    'rounds': db.last_rounds(20),\n"
    "    'events': db.recent_events(200),\n"
    "}\n"
    "print('@@LEDGER@@' + json.dumps(payload, ensure_ascii=False))\n"
)
_LEDGER_MARK = "@@LEDGER@@"


def _ledger_local():
    """Read the ledger belonging to *this* checkout.

    MIHOMO_TEST_ROOT is pinned first: config.py falls back to the absolute
    path /srv/mihomo-test, which on Windows resolves to a different tree
    (D:\\srv\\mihomo-test) that another session may be using -- reading that
    one would silently report a foreign, empty ledger as production.
    """
    os.environ["MIHOMO_TEST_ROOT"] = str(ROOT)
    from mihomo_test import db
    return {"stats": db.stats(),
            "nodes": db.query("SELECT source, display, status, last_reason,"
                              " consec_fail, country FROM nodes"),
            "rounds": db.last_rounds(20),
            "events": db.recent_events(200)}


def _ledger_is_real(ledger):
    """A ledger with no nodes and no rounds is not a deployment."""
    return bool(ledger and (ledger.get("nodes") or ledger.get("rounds")))


def _ledger_ssh(host, container, timeout=90):
    cmd = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host,
           f"docker exec -i {container} python3 -"]
    proc = subprocess.run(cmd, input=_LEDGER_DUMP, capture_output=True, text=True,
                          encoding="utf-8", errors="replace", timeout=timeout)
    if proc.returncode != 0:
        raise RuntimeError((proc.stderr or "ssh failed").strip()[:200])
    for line in reversed(proc.stdout.splitlines()):
        if line.startswith(_LEDGER_MARK):
            return json.loads(line[len(_LEDGER_MARK):])
    raise RuntimeError("ssh 返回里没有账本载荷")


def scan_live(source="auto", ssh_host="vps", container="mihomo-test"):
    """Anomalies in the liveness ledger. Read-only, both paths.

    source: auto (ssh then local), ssh, local, off.
    """
    ledger, err = None, None
    attempts = {"auto": [("ssh", _ledger_ssh), ("local", _ledger_local)],
                "ssh": [("ssh", _ledger_ssh)],
                "local": [("local", _ledger_local)]}.get(source, [])
    for kind, fetch in attempts:
        try:
            got = fetch(ssh_host, container) if kind == "ssh" else fetch()
        except Exception as exc:
            err = f"{kind}: {type(exc).__name__}: {exc}"
            continue
        if not _ledger_is_real(got):
            err = f"{kind}: 账本为空（不是部署）"
            continue
        ledger = got
        break
    if ledger is None:
        return [finding("LIVE", "LOW", "LIVE-no-ledger",
                        "取不到线上账本（离线模式）",
                        (err or "ledger source=off")[:200],
                        "用 --live 自动走 ssh 拉 vps；或设 MIHOMO_TEST_ROOT 到部署目录。")]
    stats = ledger["stats"]
    nodes = ledger["nodes"]
    rounds = ledger["rounds"]
    events = ledger["events"]
    out = []
    dead = [n for n in nodes if n["status"] == "dead"]
    pending = [n for n in nodes if n["status"] == "pending"]

    # 1. batch deaths: many nodes, same source, same reason, long streaks
    batches = Counter((n["source"], n["last_reason"]) for n in dead
                      if (n["consec_fail"] or 0) >= LONG_STREAK_MIN)
    worst = [(k, v) for k, v in batches.items() if v >= DEAD_BATCH_MIN]
    if worst:
        detail = "; ".join(f"{src}/{reason} ×{count}" for (src, reason), count in
                           sorted(worst, key=lambda kv: -kv[1]))
        out.append(finding(
            "LIVE", "HIGH", "LIVE-batch-death",
            f"{sum(c for _, c in worst)} 个节点成批判死（同源同因，连败 ≥{LONG_STREAK_MIN} 轮）",
            detail,
            "只读复核：挑该批里 2 个节点，取其近 20 轮 results 记录，"
            "确认签名是否每轮完全一致。一致=真死（整批下线）；不一致=误杀，"
            "找出与单点失败不同的机制（入口过滤/出口验证/指纹折叠）。"))

    if pending:
        grouped = Counter((n["source"], n["last_reason"]) for n in pending)
        out.append(finding(
            "LIVE", "HIGH", "LIVE-undecided-nodes",
            f"{len(pending)} 个节点尚未定性（连续失败 1–2 轮）",
            "; ".join(f"{s}/{r} ×{c}" for (s, r), c in grouped.most_common()),
            "逐个复核并给出结论（alive 或 dead 的依据），不允许用「观察中」收尾；"
            "同一个节点在下一次扫描里再次出现即视为未进展。"))

    excluded = [n for n in nodes if n["status"] == "excluded"]
    if excluded:
        out.append(finding(
            "LIVE", "MED", "LIVE-excluded-nodes",
            f"{len(excluded)} 个节点因入口在受限 ISP 被整轮跳过",
            "; ".join(f"{n['source']}/{n['display'][:28]}" for n in excluded[:6]),
            "核对：入口 IP 的归属国是否真的在 exclude_entry_countries 里；"
            "对多宿主节点，确认是否还有非 CN 路径可测（保守策略不该误排除）。"))

    suspect = [r for r in rounds if r.get("suspect")]
    if suspect:
        out.append(finding(
            "LIVE", "HIGH", "LIVE-suspect-rounds",
            f"近 {len(rounds)} 轮里有 {len(suspect)} 轮触发护栏（结果未发布）",
            ", ".join(f"#{r['id']} ok={r['ok']}/{r['total']}" for r in suspect[:6]),
            "护栏意味着整轮性事故。查该轮的事件日志与出口验证计数，"
            "判断是测试目标侧抖动还是内核侧问题；必要时固化成一禁用例。"))

    durs = [r.get("duration_s") for r in rounds if r.get("duration_s")]
    if durs:
        out.append(finding(
            "LIVE", "LOW", "LIVE-duration-trend",
            "轮次耗时与预算的余量",
            f"近 {len(durs)} 轮 {min(durs):.0f}–{max(durs):.0f}s；"
            f"最近一轮 {durs[0]:.0f}s",
            "若最小值接近预算（默认 1200s）就要查看门狗在哪个阶段收手；"
            "否则本项仅作趋势记录，不投入迭代。"))

    noisy = [e for e in events if e["level"] not in ("info", "debug")]
    if noisy:
        out.append(finding(
            "LIVE", "MED", "LIVE-warn-events",
            f"最近有 {len(noisy)} 条 warn/error 事件",
            "; ".join(f"{e['ts']} {e['message'][:60]}" for e in noisy[:5]),
            "逐条归因：可复现的固化成一个用例；一次性的记进 JOURNAL 不再追。"))

    flips = sum((r.get("restored") or 0) for r in rounds)
    out.append(finding(
        "LIVE", "LOW", "LIVE-restore-rate",
        "判死的翻转率（误杀的下界证据）",
        f"近 {len(rounds)} 轮共恢复 {flips} 次；stats={json.dumps(stats, ensure_ascii=False)}",
        "恢复次数多说明判死阈值偏激；为 0 说明收敛稳定。记录数字即可，"
        "除非同时出现成批判死。"))
    return out


# --------------------------------------------------------------------------
# report
# --------------------------------------------------------------------------

def collect(live=False, ssh_host="vps", container="mihomo-test"):
    items = []
    items += scan_verdicts()
    items += scan_config_keys()
    items += scan_silent_except()
    items += scan_tests()
    items += scan_docs()
    items += scan_sizes()
    if live:
        items += scan_live("auto", ssh_host, container)
    items.sort(key=lambda f: (SEV_ORDER.get(f["sev"], 9), f["cat"], f["id"]))
    return items


def render(items, live=False):
    lines = ["# BACKLOG（自动生成，只读扫描）", "",
             f"- generator: `tools/loop_scan.py`{' --live' if live else ''}",
             f"- candidates: {len(items)}",
             "- ID 是稳定的：已在 STATE 标 ✓ 的 ID 直接跳过，不要重复做。", "",
             "| 严重度 | 类别 | ID | 事项 | 证据 | 建议实验 |",
             "|---|---|---|---|---|---|"]
    for f in items:
        lines.append(f"| {f['sev']} | {f['cat']} | `{f['id']}` | {f['title']} "
                     f"| {f['evidence']} | {f['experiment']} |")
    lines += ["", "## 怎么用", "",
              "1. 每迭代取**第一条未被标记的** HIGH；没有 HIGH 才取 MED，最后才 LOW。",
              "2. 在 STATE.md 的「已完成 ID」列表里登记 ID，防止重复。",
              "3. 需要授权才能做的，登记到 `.local/loop/NEEDS-DECISION.md` 后跳到下一条。",
              "4. 每 6 个迭代用 `--live` 重扫一次：新异常会自动冒到前面。", ""]
    return "\n".join(lines)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--live", action="store_true", help="also scan the liveness ledger")
    ap.add_argument("--ssh-host", default="vps", help="host holding the live deployment")
    ap.add_argument("--container", default="mihomo-test", help="app container name")
    ap.add_argument("--write", action="store_true",
                    help="write .local/loop/BACKLOG.md instead of printing")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    args = ap.parse_args(argv)

    items = collect(live=args.live, ssh_host=args.ssh_host, container=args.container)
    if args.json:
        print(json.dumps(items, ensure_ascii=False, indent=2))
        return 0
    text = render(items, live=args.live)
    if args.write:
        dest = ROOT / ".local" / "loop" / "BACKLOG.md"
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(text, encoding="utf-8")
        print(f"wrote {dest} ({len(items)} candidates)")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())