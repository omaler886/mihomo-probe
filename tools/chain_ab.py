#!/usr/bin/env python3
"""A/B: the same node measured direct and chained, on a private root.

Answers "why does a node that passes a direct test fail a chained one" with
measurements instead of theory. Every picked node is entered twice, under two
ledger sources, so the two verdicts cannot merge (same fingerprint, different
(source, fp) bucket). Nothing here touches /srv/mihomo-test.
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from collections import Counter, defaultdict

ROOT = "/tmp/chainab"
os.environ["MIHOMO_TEST_ROOT"] = ROOT
os.environ["MIHOMO_TEST_HOST_ROOT"] = ROOT
sys.path.insert(0, "/srv/mihomo-test")

from mihomo_test import config as cfgmod      # noqa: E402
from mihomo_test import core as coremod       # noqa: E402
from mihomo_test import engine                # noqa: E402
from mihomo_test.store import Client          # noqa: E402

RUNNER = "chainab-core"
PORT = 19324
SECRET = ["chainab"]
DSRC, CSRC = "AB-direct", "AB-chain"
PLACEHOLDER = ("剩余流量", "到期", "过期", "官网", "订阅", "重置", "倍率", "建议",
               "公告", "客服", "流量", "Traffic", "Expire", "防失联", "网址")
WANT = [("hysteria2", 12), ("vless", 14), ("ss", 8), ("vmess", 4),
        ("trojan", 4), ("anytls", 3), ("tuic", 1)]
SOURCES = [("gammasub", "sub"), ("deltasub", "sub"),
           ("legacy-sub-c", "sub"), ("betasub", "sub")]


def log(level, message):
    print(f"  [{level}] {message}", flush=True)


def api(path, method="GET", payload=None):
    body = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(f"http://127.0.0.1:{PORT}{path}", data=body,
                                 method=method)
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
    print("内核未就绪:\n", (logs.stdout + logs.stderr)[-1200:])
    return False


def build_cfg(backend):
    return {
        "substore": {"backend": backend},
        "core": {"api": f"http://127.0.0.1:{PORT}", "lanes": 8, "base_port": 19330,
                 "container": RUNNER, "mixed_port": 19334,
                 "container_config_path": "/root/.config/mihomo/config.yaml"},
        "sources": [{"key": key, "kind": kind, "name": key, "label": key,
                     "enabled": True} for key, kind in SOURCES],
        "chain": {"enabled": True, "max_fronts": 1,
                  "front_source": {"kind": "sub", "name": "CM-CF"}},
        "test": {"targets": ["http://connectivitycheck.platform.hicloud.com/generate_204",
                             "https://cp.cloudflare.com/generate_204"],
                 "expected_status": "204", "timeout_ms": 6000,
                 "timeout_ms_retry": 9000, "max_attempts": 2, "retry_pause_s": 0.3,
                 "concurrency": 16},
        "dns": {"views": {"cn": {"resolver": "https://doh.pub/dns-query",
                                 "ecs": "114.114.114.0/24", "ecs_prefix": 24},
                          "overseas": {"resolver": "https://cloudflare-dns.com/dns-query",
                                       "ecs": "8.8.8.8/24", "ecs_prefix": 24}},
                "timeout_s": 8, "cache_hours": 0},
        "verify": {"enabled": False, "entry_check": False},
        "policy": {"drop_after_consecutive_fails": 3,
                   "suspect_floor_ratio": 0.5, "suspect_floor_absolute": 3},
        "publish": {"enabled": False},
        "watchdog": {"round_timeout_minutes": 20},
        "alert": {"enabled": False},
    }


def real_proxy(proxy):
    name = str(proxy.get("name") or "")
    if any(bad in name for bad in PLACEHOLDER):
        return False
    if not (proxy.get("uuid") or proxy.get("password")):
        return False
    return bool(str(proxy.get("server") or "").strip())


def main():
    os.makedirs(f"{ROOT}/core", exist_ok=True)
    os.makedirs(f"{ROOT}/data", exist_ok=True)
    prod = json.load(open("/srv/mihomo-test/data/config.json", encoding="utf-8"))
    backend = prod["substore"]["backend"]
    cfg = build_cfg(backend)
    store = Client(backend)

    fronts = engine.collect_fronts(cfg, store, log)
    front_names = [f["proxy"]["name"] for f in fronts]
    print(f"前置池: {len(fronts)} 条 -> {[f['name'] for f in fronts]}")

    # --- pick nodes: quota per protocol, across the ordinary (non-chain) sources
    quota = dict(WANT)
    picked, seen = [], set()
    for key, kind in SOURCES:
        entries, errors = engine.collect_entries(
            store, [{"key": key, "kind": kind, "name": key, "enabled": True}])
        for msg in errors:
            print("   拉取失败:", msg)
        for entry in entries:
            proto = str(entry["proxy"].get("type") or "")
            if quota.get(proto, 0) <= 0 or not real_proxy(entry["proxy"]):
                continue
            server = str(entry["proxy"].get("server"))
            if (proto, server) in seen:
                continue
            seen.add((proto, server))
            quota[proto] -= 1
            picked.append({"name": entry["name"], "proxy": entry["proxy"],
                           "type": proto, "group": key,
                           "network": str(entry["proxy"].get("network") or "")})

    # --- plus the live production chain source (all of it), measured both ways
    air_entries, errors = engine.collect_entries(
        store, [{"key": "air", "kind": "collection", "name": "air", "enabled": True}])
    for msg in errors:
        print("   拉取失败:", msg)
    air_n = 0
    for entry in air_entries:
        if not real_proxy(entry["proxy"]):
            continue
        air_n += 1
        picked.append({"name": entry["name"], "proxy": entry["proxy"],
                       "type": str(entry["proxy"].get("type") or ""),
                       "group": "air(生产链式源)",
                       "network": str(entry["proxy"].get("network") or "")})

    print(f"\n挑出 {len(picked)} 条节点（普通来源 {len(picked) - air_n} + air {air_n}）")
    print("  协议分布:", dict(Counter(p["type"] for p in picked)))
    print("  来源分布:", dict(Counter(p["group"] for p in picked)))

    # --- build two variants per node
    direct_entries, chain_entries, meta = [], [], {}
    for i, item in enumerate(picked):
        plain = {k: v for k, v in item["proxy"].items()
                 if k not in ("dialer-proxy", "dialer_proxy")}
        fp = engine._orig_fp(plain)
        meta[fp] = item
        direct_entries.append({"source": DSRC, "name": item["name"], "proxy": plain,
                               "index": i, "fp": fp, "category": engine.CAT_DIRECT})
        chain_entries.append({"source": CSRC, "name": item["name"],
                              "proxy": {**plain, "dialer-proxy": "__ab__"},
                              "index": i, "fp": fp, "category": engine.CAT_CHAIN})

    all_entries = direct_entries + chain_entries
    test_entries, excluded = engine.classify_and_expand(all_entries + fronts, cfg, log)
    test_entries, expanded = engine.expand_chains(test_entries, front_names)
    print(f"链式展开: {expanded} 条 -> {expanded * len(front_names)} 变体；"
          f"内核配置共 {len(test_entries)} 条；入口排除 {len(excluded)}")

    secret = cfgmod.core_secret()
    SECRET[0] = secret
    proxies, mapping, dropped = coremod.make_testable(
        test_entries, cfg["core"], secret, log=log,
        strip_ech=bool(cfg["verify"].get("strip_ech")),
        keep_dialer=front_names)
    for item in dropped:
        print(f"  剔除 {item['name']}: {item['why']}")

    kept = [p for p in proxies if "dialer-proxy" in p]
    print(f"内核配置: 保留 dialer-proxy 的 proxy {len(kept)} 条")
    cfg_path = cfgmod.CORE_DIR / "config.yaml"
    text = cfg_path.read_text(encoding="utf-8")
    cfg_path.write_text(text.replace("log-level: warning", "log-level: debug"),
                        encoding="utf-8")

    if not start_kernel(cfg_path):
        return 1

    core = coremod.Core(cfg["core"], secret)
    by_name = {m["mihomo"]: m for m in mapping}
    results, chain_failed, live = engine._test_phases(
        core, mapping, cfg["test"], 16, None, bool(fronts), log)
    print(f"\n前置存活 {live}/{len(fronts)}；front_dead 判失败 {len(chain_failed)}")

    # --- pair up
    def verdict_for(source, fp):
        outs = [o for n, o in results.items()
                if by_name.get(n, {}).get("source") == source
                and by_name.get(n, {}).get("fp") == fp]
        if not outs:
            return ("untested", None)
        ok = [o for o in outs if o["reason"] is None]
        if ok:
            return ("ok", min(o["delay_ms"] for o in ok))
        return (outs[0]["reason"], None)

    rows = []
    for fp, item in meta.items():
        dv, dms = verdict_for(DSRC, fp)
        cv, cms = verdict_for(CSRC, fp)
        rows.append({"name": item["name"], "type": item["type"], "group": item["group"],
                     "network": item["network"], "direct": dv, "dms": dms,
                     "chain": cv, "cms": cms})

    print("\n" + "=" * 100)
    print(f"{'节点':42s} {'协议':10s} {'来源':16s} {'直连':14s} {'链式':14s}")
    print("=" * 100)
    for r in sorted(rows, key=lambda x: (x["group"], x["type"], x["name"])):
        d = f"OK {r['dms']}ms" if r["direct"] == "ok" else f"FAIL {r['direct']}"
        c = f"OK {r['cms']}ms" if r["chain"] == "ok" else f"FAIL {r['chain']}"
        print(f"{r['name'][:42]:42s} {r['type']:10s} {r['group'][:16]:16s} "
              f"{d:14s} {c:14s}")

    print("\n" + "=" * 100)
    print("交叉表（行=直连，列=链式）")
    cross = Counter((r["direct"], r["chain"]) for r in rows)
    for (d, c), n in sorted(cross.items(), key=lambda x: -x[1]):
        dd = "OK" if d == "ok" else d
        cc = "OK" if c == "ok" else c
        print(f"  直连={dd:12s} 链式={cc:16s} : {n}")
    print()
    print("按协议：直连通过 / 链式通过 / 总数")
    byproto = defaultdict(lambda: [0, 0, 0])
    for r in rows:
        b = byproto[r["type"]]
        b[2] += 1
        if r["direct"] == "ok":
            b[0] += 1
        if r["chain"] == "ok":
            b[1] += 1
    for proto, (d, c, n) in sorted(byproto.items(), key=lambda x: -x[1][2]):
        print(f"  {proto:12s} 直连 {d:3d}/{n:3d}   链式 {c:3d}/{n:3d}")
    print()
    print("按来源：直连通过 / 链式通过 / 总数")
    bygrp = defaultdict(lambda: [0, 0, 0])
    for r in rows:
        b = bygrp[r["group"]]
        b[2] += 1
        if r["direct"] == "ok":
            b[0] += 1
        if r["chain"] == "ok":
            b[1] += 1
    for grp, (d, c, n) in sorted(bygrp.items(), key=lambda x: -x[1][2]):
        print(f"  {grp:20s} 直连 {d:3d}/{n:3d}   链式 {c:3d}/{n:3d}")

    print("\n--- 直连通 / 链式不通（按协议）---")
    bad = [r for r in rows if r["direct"] == "ok" and r["chain"] != "ok"]
    print("  数量:", len(bad), " 协议:", dict(Counter(r["type"] for r in bad)),
          " 原因:", dict(Counter(r["chain"] for r in bad)))
    for r in bad[:40]:
        print(f"    {r['name'][:40]:40s} {r['type']:10s} 直连 {r['dms']}ms -> 链式 {r['chain']}")

    print("\n--- 直连不通 / 链式通 ---")
    rev = [r for r in rows if r["direct"] != "ok" and r["chain"] == "ok"]
    print("  数量:", len(rev))
    for r in rev[:40]:
        print(f"    {r['name'][:40]:40s} {r['type']:10s} 直连 {r['direct']} -> 链式 {r['cms']}ms")

    errs = subprocess.run(["docker", "logs", "--tail", "300", RUNNER],
                          capture_output=True, text=True)
    lines = [ln.split("msg=")[-1][:170] for ln in (errs.stdout + errs.stderr).splitlines()
             if any(k in ln.lower() for k in ("dialer", "xhttp", "quic", "udp",
                                              "handshake", "refused", "reset", "eof"))
             and "[DNS] resolve" not in ln]
    print("\n内核日志（相关行，末 25）:")
    for ln in lines[-25:]:
        print("   ", ln)

    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    with open(f"{ROOT}/result.json", "w", encoding="utf-8") as fh:
        json.dump(rows, fh, ensure_ascii=False, indent=1)
    print("\n原始结果已写入", f"{ROOT}/result.json")
    return 0


if __name__ == "__main__":
    sys.exit(main())
