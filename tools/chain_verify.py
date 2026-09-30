#!/usr/bin/env python3
"""Verify the chain path against a real mihomo, on vps, without touching prod.

The unit tests cover each piece against mocks; the parts that only exist at the
seams cannot be mocked honestly. Four of them matter here:

  1. whether the kernel *accepts* a config whose proxies carry `dialer-proxy`
     pointing at a reserved `__FRONT<n>__` name. A wrong field name is silently
     ignored by mihomo, and the chain would then be dialled direct and pass for
     entirely the wrong reason -- so this checks the config text, not just the
     exit code.
  2. whether a chained variant really goes through the front, proved by
     dialling through a front that cannot reach anything.
  3. whether the two-phase split reaches the right verdict when no front works,
     and whether that verdict lands in the ledger as a real failure.
  4. what chaining actually buys: the same node tested direct and chained, side
     by side.

Scenario A: the real front pool (Sub-Store `cm-xhttp`, i.e. the edgetunnel the
user pasted) with real nodes borrowed from a real source, each tested twice --
once direct and once with a `dialer-proxy` hung on it the way upstream would.

Scenario B: the same nodes with a front pool that cannot work (TEST-NET-1), to
watch the `front_dead` verdict land in the ledger through the real code path.

Runs on a private MIHOMO_TEST_ROOT and its own kernel container and port, so the
production stack is not written to, restarted, or dialled through.
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = "/tmp/chainverify"
os.environ["MIHOMO_TEST_ROOT"] = ROOT
os.environ["MIHOMO_TEST_HOST_ROOT"] = ROOT

# Deliberately NOT /srv/mihomo-test: this imports the code under test from a
# staging copy, so the verification cannot be confused by a half-uploaded
# production tree and cannot affect the running service.
SRC = os.environ.get("MIHOMO_TEST_SRC", "/tmp/chainverify-src")
sys.path.insert(0, SRC)

from mihomo_test import config as cfgmod          # noqa: E402
from mihomo_test import core as coremod           # noqa: E402
from mihomo_test import engine                    # noqa: E402
from mihomo_test.store import Client              # noqa: E402

RUNNER = "chainverify-core"
PORT = 19302
SECRET = ["chainverify"]        # replaced with the real one once the root is set
TARGET_SOURCE = os.environ.get("CHAIN_VERIFY_SOURCE", "demo-source")
CHAIN_SOURCE = "chain-test"
DIRECT_SOURCE = "direct-test"
# Names upstream uses for placeholder rows that are not proxies at all. The
# first entries of most airport lists are these, so a naive `entries[:3]` tests
# nothing but the placeholders.
PLACEHOLDER = ("剩余流量", "到期", "过期", "官网", "订阅", "重置", "倍率",
               "建议", "公告", "客服", "专线节点", "Traffic", "Expire")
# TCP-carried protocols only. The front here is an xhttp tunnel, i.e. TCP: a
# QUIC-based outbound (hysteria2 / tuic) cannot ride it, so chaining one fails
# for a reason that has nothing to do with the feature -- and the first run of
# this script picked exactly those, which read as "the chain is broken".
TCP_PROTOCOLS = ("vless", "vmess", "trojan", "ss", "ssr", "socks5", "http")
DEAD_FRONT = {
    "type": "vless", "name": "dead-front", "server": "192.0.2.1", "port": 443,
    "uuid": "00000000-0000-4000-8000-000000000000", "udp": True, "tls": True,
    "client-fingerprint": "chrome", "encryption": "none", "network": "tcp",
    "servername": "example.com",
}


class _Store:
    """The real Sub-Store client, with named resources overridden.

    Scenario B needs a front pool that cannot work while the target source stays
    real, so the override is keyed by resource name. Monkeypatching the client
    instance instead replaced *every* fetch -- the target source then handed back
    the dead front, and the run silently tested nothing.
    """

    def __init__(self, backend, override=None):
        self._client = Client(backend)
        self._override = dict(override or {})

    def fetch_source(self, kind, name, target="ClashMeta"):
        if name in self._override:
            return [dict(p) for p in self._override[name]]
        return self._client.fetch_source(kind, name, target)


def log(level, message):
    print(f"  [{level}] {message}", flush=True)


def api(path, method="GET", payload=None):
    body = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(f"http://127.0.0.1:{PORT}{path}", data=body,
                                method=method)
    # Must be the secret the generated config was written with, or the kernel
    # answers 401 and the readiness loop spins until it times out.
    req.add_header("Authorization", f"Bearer {SECRET[0]}")
    if body:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")


def start_kernel(config_path):
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    subprocess.run(
        ["docker", "run", "-d", "--name", RUNNER, "--network", "host",
         "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
         "--pids-limit", "512", "--memory", "384m",
         "-v", f"{config_path}:/root/.config/mihomo/config.yaml:ro",
         "metacubex/mihomo:latest", "-d", "/root/.config/mihomo",
         "-f", "/root/.config/mihomo/config.yaml"],
        capture_output=True, text=True)
    for _ in range(25):
        try:
            if api("/version")[0] == 200:
                return True
        except urllib.error.URLError:
            pass
        time.sleep(1.5)
    logs = subprocess.run(["docker", "logs", "--tail", "30", RUNNER],
                          capture_output=True, text=True)
    print("内核未就绪:\n", (logs.stdout + logs.stderr)[-800:])
    return False


def kernel_errors(limit=14):
    logs = subprocess.run(["docker", "logs", "--tail", "400", RUNNER],
                          capture_output=True, text=True)
    out = []
    for line in (logs.stdout + logs.stderr).splitlines():
        low = line.lower()
        if any(k in low for k in ("dialer", "unknown field", "error", "refused",
                                  "no such", "not found", "dial", "xhttp",
                                  "handshake", "reset", "eof")):
            if "[DNS] resolve" in line:
                continue
            out.append(line.split("msg=")[-1][:170])
    return out[-limit:]


def build_cfg(front_name):
    backend = json.loads(
        open("/srv/mihomo-test/data/config.json", encoding="utf-8").read()
    )["substore"]["backend"]
    return {
        "substore": {"backend": backend},
        "core": {"api": f"http://127.0.0.1:{PORT}", "lanes": 4, "base_port": 19310,
                 "container": RUNNER, "mixed_port": 19314,
                 "container_config_path": "/root/.config/mihomo/config.yaml"},
        "sources": [{"key": TARGET_SOURCE, "kind": "sub", "name": TARGET_SOURCE,
                     "label": TARGET_SOURCE, "enabled": True}],
        "chain": {"enabled": True, "max_fronts": 4,
                  "front_source": {"kind": "sub", "name": front_name}},
        "test": {"targets": ["http://connectivitycheck.platform.hicloud.com/generate_204",
                             "https://cp.cloudflare.com/generate_204"],
                 "expected_status": "204", "timeout_ms": 6000,
                 "timeout_ms_retry": 9000, "max_attempts": 2, "retry_pause_s": 0.3,
                 "concurrency": 12},
        "dns": {"views": {"cn": {"resolver": "https://doh.pub/dns-query",
                                 "ecs": "114.114.114.0/24", "ecs_prefix": 24},
                          "overseas": {"resolver": "https://cloudflare-dns.com/dns-query",
                                       "ecs": "8.8.8.8/24", "ecs_prefix": 24}},
                "timeout_s": 8, "cache_hours": 0},
        "verify": {"enabled": False, "entry_check": True,
                   "exclude_entry_countries": ["CN"],
                   "exclude_countries": ["CN"]},
        "policy": {"drop_after_consecutive_fails": 3,
                   "suspect_floor_ratio": 0.5, "suspect_floor_absolute": 3},
        "publish": {"enabled": False},
        "watchdog": {"round_timeout_minutes": 10},
        "alert": {"enabled": False},
    }


def pick_nodes(entries, count):
    """Real, TCP-carried proxies only: no placeholder rows, no QUIC."""
    out, skipped = [], []
    for entry in entries:
        proxy = entry["proxy"]
        name = str(proxy.get("name") or "")
        if any(bad in name for bad in PLACEHOLDER):
            continue
        if not (proxy.get("uuid") or proxy.get("password")):
            continue
        if str(proxy.get("type") or "") not in TCP_PROTOCOLS:
            skipped.append(f"{name[:24]}({proxy.get('type')})")
            continue
        out.append(entry)
        if len(out) >= count:
            break
    if skipped:
        print(f"  （跳过非 TCP 协议节点 {len(skipped)} 条：{', '.join(skipped[:4])}…）")
    return out


def _verdict(outcome):
    if outcome is None:
        return "未测"
    if outcome["reason"] is None:
        return f"OK {outcome['delay_ms']}ms"
    return f"FAIL {outcome['reason']}"


def scenario(label, front_name, fake_fronts=None, chain_n=3):
    print(f"\n{'=' * 74}\n{label}\n{'=' * 74}", flush=True)
    cfg = build_cfg(front_name)
    store = _Store(cfg["substore"]["backend"],
                   {front_name: fake_fronts} if fake_fronts else None)

    fronts = engine.collect_fronts(cfg, store, log)
    front_names = [f["proxy"]["name"] for f in fronts]

    entries, errors = engine.collect_entries(store, cfg["sources"])
    for message in errors:
        print("   拉取失败:", message)
    picked = pick_nodes(entries, chain_n)

    # Each node goes in twice, under two ledger sources, so the two verdicts
    # cannot merge: the fingerprint is the same (it is the same node) but the
    # bucket key is (source, fp).
    chain_entries, direct_entries = [], []
    for entry in picked:
        proxy = dict(entry["proxy"])
        chain_entries.append({**entry, "source": CHAIN_SOURCE,
                              "proxy": {**proxy, "dialer-proxy": "hk_b"}})
        direct_entries.append({**entry, "source": DIRECT_SOURCE, "proxy": proxy})
    rest = [e for e in entries if e not in picked]
    all_entries = chain_entries + direct_entries + rest
    print(f"  源 {TARGET_SOURCE}: {len(entries)} 条；挑出 {len(picked)} 条真节点做"
          f"「直连 vs 链式」；前置 {len(fronts)} 条")

    test_entries, _excluded = engine.classify_and_expand(all_entries + fronts, cfg, log)
    test_entries, expanded = engine.expand_chains(test_entries, front_names)
    print(f"  链式展开: {expanded} 个节点 -> {expanded * len(front_names)} 条变体；"
          f"内核配置共 {len(test_entries)} 条")

    secret = cfgmod.core_secret()
    SECRET[0] = secret
    proxies, mapping, dropped = coremod.make_testable(
        test_entries, cfg["core"], secret, log=log,
        strip_ech=bool(cfg["verify"].get("strip_ech")),
        keep_dialer=front_names)
    for item in dropped:
        print(f"  剔除 {item['name']}: {item['why']}")

    kept = [p for p in proxies if "dialer-proxy" in p]
    cfg_text = (cfgmod.CORE_DIR / "config.yaml").read_text(encoding="utf-8")
    marker = '"dialer-proxy"'
    print(f"  内核配置: 保留 {marker} 的 proxy {len(kept)} 条"
          f"（示例 {kept[0]['name'][:24]} -> {kept[0]['dialer-proxy']}）" if kept
          else f"  !! 内核配置里没有 {marker}")
    print(f"  配置文本含 {marker}: {marker in cfg_text}"
          f"；前端名进内核: {engine.FRONT_NAME_PREFIX in cfg_text}")

    # `build_config` writes `log-level: warning`, which is right for production
    # and useless for diagnosing a dial that fails. Bump it for this run only --
    # a chain failure with no kernel line is unattributable, and `kernel_error`
    # (mihomo's generic 503) says nothing about which hop broke.
    cfg_path = cfgmod.CORE_DIR / "config.yaml"
    cfg_path.write_text(cfg_text.replace("log-level: warning", "log-level: debug"),
                        encoding="utf-8")
    for entry in picked:
        print(f"    · {entry['name'][:26]:26s} type={entry['proxy'].get('type')}"
              f" network={entry['proxy'].get('network')}")

    if not start_kernel(cfg_path):
        return None

    core = coremod.Core(cfg["core"], secret)
    by_name = {m["mihomo"]: m for m in mapping}
    results, chain_failed, live = engine._test_phases(
        core, mapping, cfg["test"], 12, None, bool(fronts), log)

    print("\n  --- 前置 ---")
    for m in mapping:
        if m.get("role") == "front":
            print(f"   {m['original'][:30]:30s} {m['mihomo']:11s} "
                  f"{_verdict(results.get(m['mihomo']))}")

    print("\n  --- 直连 vs 链式（同一个节点，同一个指纹） ---")
    print(f"   {'节点':30s} {'直连':16s} {'链式':28s}")
    rows = []
    for entry in picked:
        direct = next((o for n, o in results.items()
                       if by_name[n]["source"] == DIRECT_SOURCE
                       and by_name[n]["fp"] == entry["fp"]), None)
        chain_outs = [(n, o) for n, o in results.items()
                      if by_name[n]["source"] == CHAIN_SOURCE
                      and by_name[n]["fp"] == entry["fp"]]
        if chain_outs:
            ok = [o for _n, o in chain_outs if o["reason"] is None]
            chain_text = (f"OK {min(o['delay_ms'] for o in ok)}ms "
                          f"({len(ok)}/{len(chain_outs)} 条前置带得动)") if ok \
                else f"FAIL {chain_outs[0][1]['reason']}"
        else:
            chain_text = "未测（前置不可用）"
        rows.append((entry["name"][:30], _verdict(direct), chain_text))
        print(f"   {rows[-1][0]:30s} {rows[-1][1]:16s} {rows[-1][2]:28s}")

    print(f"\n  前置存活 {live}/{len(fronts)}；无可用前置被直接判失败的节点 "
          f"{len(chain_failed)}")
    if chain_failed:
        round_id = engine.db.start_round("verify")
        _bs, _fps = engine._record_chain_failures(cfg, round_id, chain_failed, log)
        node = engine.db.get_node(chain_failed[0]["source"], chain_failed[0]["fp"])
        print(f"  账本: {node['display'][:26]} status={node['status']} "
              f"consec_fail={node['consec_fail']} last_reason={node['last_reason']}")

    errs = kernel_errors()
    print(f"\n  内核日志（dialer / 错误）: {errs if errs else '(无相关)'}")
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    return {"fronts": len(fronts), "live": live, "rows": rows,
            "chain_failed": len(chain_failed)}


def main():
    os.makedirs(f"{ROOT}/core", exist_ok=True)
    os.makedirs(f"{ROOT}/data", exist_ok=True)
    print(f"导入代码来自: {SRC}\nROOT={ROOT}  CORE_DIR={cfgmod.CORE_DIR}")

    a = scenario("场景 A：真实前置池（Sub-Store cm-xhttp）", "cm-xhttp", chain_n=8)
    b = scenario("场景 B：前置池不可用（TEST-NET-1）", "cm-xhttp",
                 fake_fronts=[dict(DEAD_FRONT)], chain_n=2)

    print(f"\n{'=' * 74}\n结论\n{'=' * 74}")
    if a:
        print(f"  场景 A: 前置 {a['live']}/{a['fronts']} 活")
        for name, direct, chain in a["rows"]:
            print(f"    {name[:30]:30s} 直连 {direct:16s} 链式 {chain}")
    if b:
        print(f"  场景 B: 前置 {b['live']}/{b['fronts']} 活，"
              f"被直接判失败的链式节点 {b['chain_failed']}（应为节点数）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
