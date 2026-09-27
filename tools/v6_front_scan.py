#!/usr/bin/env python3
"""Which nodes in a source can actually reach the IPv6 internet?

Loads every node of a Sub-Store source into one throwaway kernel (host
networking) and delay-tests each against an AAAA-only endpoint. A node that
answers can serve as a *v6-capable front* for IPv6-only chained targets.

Usage: v6_front_scan.py [source-name] [kind]      (default: air-local sub)
"""
import concurrent.futures as futures
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = "/srv/mihomo-test"
TD = "/tmp/v6scan"
RUNNER = "v6scan-core"
PORT = 19301
SECRET = "v6scan"
V6_URL = "http://[2606:4700:4700::1111]/"
V6_EXPECTED = "301"
V4_URL = "http://connectivitycheck.platform.hicloud.com/generate_204"
V4_EXPECTED = "204"

sys.path.insert(0, BASE)
from mihomo_test import config as cfgmod  # noqa: E402
from mihomo_test.store import Client  # noqa: E402


def fetch(kind, name):
    cfg = cfgmod.load()
    client = Client(cfg["substore"]["backend"])
    return client.fetch_source(kind, name)


def build(path, proxies):
    lines = [
        "mixed-port: 0", "allow-lan: true", "bind-address: 127.0.0.1",
        "mode: rule", "log-level: warning", "ipv6: true", "unified-delay: true",
        "external-controller: 127.0.0.1:%d" % PORT, 'secret: "%s"' % SECRET,
        "dns:", "  enable: true", "  ipv6: true", "  enhanced-mode: fake-ip",
        "  fake-ip-range: 198.18.0.1/16",
        "  nameserver:", "    - 223.5.5.5", "    - 1.1.1.1",
        "proxies:",
    ]
    for p in proxies:
        lines.append("  - " + json.dumps(p, ensure_ascii=False))
    lines += ["proxy-groups:", '  - name: "G"', "    type: select", "    proxies:"]
    for p in proxies:
        lines.append("      - " + json.dumps(p["name"], ensure_ascii=False))
    lines += ["rules:", "  - MATCH,G", ""]
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))


def api(path, timeout=30):
    req = urllib.request.Request("http://127.0.0.1:%d%s" % (PORT, path))
    req.add_header("Authorization", "Bearer " + SECRET)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")


def delay(name, url, expected, timeout_ms=8000):
    q = urllib.parse.urlencode({"timeout": timeout_ms, "url": url, "expected": expected})
    st, body = api("/proxies/%s/delay?%s" % (urllib.parse.quote(name, safe=""), q))
    return st == 200


def specs():
    """Sources to scan: argv entries shaped `kind/name`, default air-local."""
    if len(sys.argv) > 1:
        out = []
        for arg in sys.argv[1:]:
            kind, _, name = arg.partition("/")
            out.append((kind or "sub", name or kind))
        return out
    return [("sub", "air-local")]


def main():
    os.makedirs(TD, exist_ok=True)
    named = []
    for kind, name in specs():
        try:
            proxies = fetch(kind, name)
        except Exception as exc:
            print("skip %s/%s: %s" % (kind, name, str(exc)[:70]))
            continue
        print("nodes in %s/%s: %d" % (kind, name, len(proxies)))
        for p in proxies:
            q = {k: v for k, v in p.items()
                 if k not in ("dialer-proxy", "dialer_proxy", "interface-name",
                              "routing-mark")}
            q["name"] = "n%d" % len(named)
            named.append((q["name"], "%s|%s" % (name, str(p.get("name"))),
                          str(p.get("type")), str(p.get("server")), q))
    if not named:
        print("nothing to scan")
        return 1
    path = os.path.join(TD, "cfg.yaml")
    build(path, [n[4] for n in named])
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    run = subprocess.run(
        ["docker", "run", "-d", "--name", RUNNER, "--network", "host",
         "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
         "--pids-limit", "512", "--memory", "384m",
         "-v", "%s:/root/.config/mihomo/config.yaml:ro" % path,
         "metacubex/mihomo:latest", "-d", "/root/.config/mihomo",
         "-f", "/root/.config/mihomo/config.yaml"],
        capture_output=True, text=True)
    if run.returncode != 0:
        print("docker run failed:", run.stderr[-300:])
        return 1
    for _ in range(30):
        try:
            if api("/version", timeout=3)[0] == 200:
                break
        except Exception:
            pass
        time.sleep(1.5)
    else:
        print("core not ready")
        return 1

    def probe(item):
        kn, _disp, _t, _s, _p = item
        return kn, delay(kn, V4_URL, V4_EXPECTED), delay(kn, V6_URL, V6_EXPECTED)

    v4ok, v6ok = [], []
    with futures.ThreadPoolExecutor(max_workers=16) as pool:
        for kn, a, b in pool.map(probe, named):
            if a:
                v4ok.append(kn)
            if b:
                v6ok.append(kn)
    by_kernel = {n[0]: n for n in named}
    print("\nv4 reachable: %d/%d" % (len(v4ok), len(named)))
    print("v6 reachable: %d/%d" % (len(v6ok), len(named)))
    print("\n--- v6-capable nodes ---")
    for kn in v6ok:
        _kn, disp, t, s, _p = by_kernel[kn]
        print("  %-40s %-10s %s" % (disp[:40], t, s))
    with open(os.path.join(TD, "result.json"), "w", encoding="utf-8") as fh:
        json.dump({"v4": v4ok, "v6": v6ok,
                   "nodes": [{"kernel": n[0], "display": n[1], "type": n[2],
                              "server": n[3]} for n in named]}, fh, ensure_ascii=False)
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
