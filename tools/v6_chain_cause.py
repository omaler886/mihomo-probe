#!/usr/bin/env python3
"""Decide WHY chained IPv6-only nodes fail.

Two hypotheses:
  A. the front has no IPv6 egress, so it cannot reach an AAAA-only target
  B. mihomo's `dialer-proxy` does not forward IPv6 targets at all

Test 1  front -> an IPv6-only HTTP endpoint (front's own v6 egress)
Test 2  v6-only target chained through an IPv4-ish front   (production shape)
Test 3  v6-only target chained through a *v6-capable* front (a v6-only node)
Test 4  v6-only target as its own server (control: must pass, vps has v6)

Read-only wrt the production stack: private dir, its own container + port.
"""
import json
import os
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = "/srv/mihomo-test"
TD = "/tmp/v6chain"
RUNNER = "v6chain-core"
PORT = 19299
SECRET = "v6chain"
# AAAA-only, plain HTTP so a 301 is a clean "reached it" signal.
V6_URL = "http://[2606:4700:4700::1111]/"
V6_EXPECTED = "301"
TEST_URL = "http://connectivitycheck.platform.hicloud.com/generate_204"

sys.path.insert(0, BASE)
from mihomo_test import config as cfgmod  # noqa: E402
from mihomo_test.store import Client  # noqa: E402


def fam(server):
    """'v6only' / 'v4only' / 'dual' / None for a server host."""
    if ":" in server and "." not in server.split(":")[0]:
        return "v6only"
    try:
        fams = {i[0] for i in socket.getaddrinfo(server, None)}
    except socket.gaierror:
        return None
    h6, h4 = socket.AF_INET6 in fams, socket.AF_INET in fams
    if h6 and h4:
        return "dual"
    if h6:
        return "v6only"
    if h4:
        return "v4only"
    return None


def collect():
    cfg = cfgmod.load()
    client = Client(cfg["substore"]["backend"])
    chained_v6, plain_v6, fronts = [], [], []
    for source in cfg["sources"]:
        if not source.get("enabled"):
            continue
        try:
            proxies = client.fetch_source(source["kind"], source["name"])
        except Exception:
            continue
        for proxy in proxies:
            server = str(proxy.get("server") or "")
            if not server:
                continue
            kind = fam(server)
            has_dialer = "dialer-proxy" in proxy or "dialer_proxy" in proxy
            if kind == "v6only":
                (chained_v6 if has_dialer else plain_v6).append(
                    (source["key"], proxy, server))
    try:
        fronts = client.fetch_source("sub", "air-local")
    except Exception as exc:
        print("air-local unavailable:", str(exc)[:80])
    return chained_v6, plain_v6, fronts


def strip(proxy, name):
    p = {k: v for k, v in proxy.items()
         if k not in ("dialer-proxy", "dialer_proxy", "interface-name", "routing-mark")}
    p["name"] = name
    return p


def build(path, proxies):
    lines = [
        "mixed-port: 0", "allow-lan: true", "bind-address: 127.0.0.1",
        "mode: rule", "log-level: debug", "ipv6: true", "unified-delay: true",
        "external-controller: 127.0.0.1:%d" % PORT, 'secret: "%s"' % SECRET,
        "dns:", "  enable: true", "  ipv6: true", "  enhanced-mode: fake-ip",
        "  fake-ip-range: 198.18.0.1/16",
        "  nameserver:", "    - 223.5.5.5", "    - 1.1.1.1",
        "proxies:",
    ]
    for p in proxies:
        lines.append("  - " + json.dumps(p, ensure_ascii=False))
    lines += ["proxy-groups:", '  - name: "EXIT"', "    type: select", "    proxies:"]
    for p in proxies:
        lines.append("      - " + json.dumps(p["name"], ensure_ascii=False))
    lines += ["listeners:",
              '  - name: "lane"', "    type: mixed", "    port: 19300",
              "    listen: 127.0.0.1",
              "rules:", "  - IN-NAME,lane,EXIT", "  - MATCH,DIRECT", ""]
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))


def api(path, method="GET", payload=None, timeout=30):
    body = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request("http://127.0.0.1:%d%s" % (PORT, path),
                                 data=body, method=method)
    req.add_header("Authorization", "Bearer " + SECRET)
    if body:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")


def delay(name, url=TEST_URL, expected="204", timeout_ms=8000):
    query = urllib.parse.urlencode({"timeout": timeout_ms, "url": url,
                                    "expected": expected})
    status, body = api("/proxies/%s/delay?%s" % (urllib.parse.quote(name, safe=""), query))
    if status == 200:
        return "OK", json.loads(body).get("delay")
    try:
        return "HTTP%d" % status, json.loads(body).get("message", "")[:70]
    except ValueError:
        return "HTTP%d" % status, body[:70]


def main():
    os.makedirs(TD, exist_ok=True)
    chained_v6, plain_v6, fronts = collect()
    print("chain-capable v6-only nodes :", len(chained_v6))
    print("plain v6-only nodes         :", len(plain_v6))
    print("air-local fronts            :", len(fronts))
    for key, p, s in chained_v6[:8]:
        print("   v6 target", key, p.get("name"), "->", s, p.get("type"))
    for key, p, s in plain_v6[:5]:
        print("   v6 plain ", key, p.get("name"), "->", s, p.get("type"))

    if not chained_v6:
        print("no chained v6-only node to test")
        return 1

    proxies = []
    # fronts from the production pool (first 2)
    front_names = []
    for i, p in enumerate(fronts[:2]):
        name = "F%d" % i
        proxies.append(strip(p, name))
        front_names.append(name)
    # a v6-capable front: a plain v6-only node
    v6front = None
    if plain_v6:
        v6front = "FV"
        proxies.append(strip(plain_v6[0][1], v6front))
    # targets: first 3 chained v6-only nodes, as their own server (dialer stripped)
    targets = []
    for i, (key, p, server) in enumerate(chained_v6[:3]):
        tname = "T%d" % i
        proxies.append(strip(p, tname))
        targets.append((tname, key, p.get("name"), server))
        if v6front:
            proxies.append(dict(strip(p, "C%d" % i), **{"dialer-proxy": v6front}))
        for fname in front_names:
            proxies.append(dict(strip(p, "P%d%s" % (i, fname)), **{"dialer-proxy": fname}))

    path = os.path.join(TD, "cfg.yaml")
    build(path, proxies)
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    subprocess.run(
        ["docker", "run", "-d", "--name", RUNNER, "--network", "host",
         "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
         "--pids-limit", "512", "--memory", "384m",
         "-v", "%s:/root/.config/mihomo/config.yaml:ro" % path,
         "metacubex/mihomo:latest", "-d", "/root/.config/mihomo",
         "-f", "/root/.config/mihomo/config.yaml"],
        capture_output=True, text=True)
    for _ in range(30):
        try:
            if api("/version", timeout=3)[0] == 200:
                break
        except Exception:
            pass
        time.sleep(1.5)
    else:
        print("core not ready")
        print(subprocess.run(["docker", "logs", "--tail", "30", RUNNER],
                             capture_output=True, text=True).stdout[-1200:])
        return 1

    print("\n=== test 1: does the front reach an IPv6-only endpoint? ===")
    for name in front_names + ([v6front] if v6front else []):
        v, d = delay(name, V6_URL, V6_EXPECTED)
        v4, d4 = delay(name)
        print("  %-4s  v4-test=%-10s %-8s   v6-test=%-10s %s" %
              (name, v4, str(d4)[:10], v, str(d)[:44]))

    print("\n=== tests 2/3/4: v6-only target, direct vs chained ===")
    for tname, key, display, server in targets:
        v, d = delay(tname)
        print("  T=%s (%s) %s" % (tname, display, server))
        print("      direct          : %-10s %s" % (v, str(d)[:60]))
        for fname in front_names:
            cv, cd = delay("P%s%s" % (tname[1:], fname))
            print("      via front %-4s  : %-10s %s" % (fname, cv, str(cd)[:60]))
        if v6front:
            cv, cd = delay("C%s" % tname[1:])
            print("      via v6 front    : %-10s %s" % (cv, str(cd)[:60]))

    print("\n=== kernel log (chained failures) ===")
    logs = subprocess.run(["docker", "logs", "--tail", "400", RUNNER],
                          capture_output=True, text=True)
    keep = []
    for line in (logs.stdout + logs.stderr).splitlines():
        low = line.lower()
        if "[DNS]" in line:
            continue
        if any(k in low for k in ("error", "fail", "dial", "refused", "timeout",
                                  "reset", "unreachable", "no route", "network")):
            keep.append(line.split("msg=")[-1][:190])
    for line in keep[-30:]:
        print("   ", line)
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
