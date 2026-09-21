#!/usr/bin/env python3
"""Verify the fix: IPv6-only nodes pass once the probe container has IPv6.

Runs the kernel in a container attached to an IPv6-enabled bridge, with IPv6
enabled in the config, and tests the same nodes that score 0% on the default
Docker bridge. Read-only with respect to the production stack.
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
TD = "/tmp/v6fix"
RUNNER = "v6fix-core"
NETWORK = os.environ.get("V6_NETWORK", "healthcheck-v6")
PORT = 19293
SECRET = "fixtest"
TEST_URL = "http://connectivitycheck.platform.hicloud.com/generate_204"

sys.path.insert(0, BASE)
from mihomo_test import config as cfgmod  # noqa: E402
from mihomo_test.store import Client  # noqa: E402


def collect():
    """Return (ipv6_only, ipv4ish) node lists from the enabled sources."""
    cfg = cfgmod.load()
    client = Client(cfg["substore"]["backend"])
    v6, v4 = [], []
    for source in cfg["sources"]:
        if not source.get("enabled"):
            continue
        try:
            proxies = client.fetch_source(source["kind"], source["name"])
        except Exception as exc:
            print(f"  skip {source['key']}: {str(exc)[:60]}")
            continue
        for proxy in proxies:
            server = str(proxy.get("server") or "")
            if not server:
                continue
            # A node is IPv6-only when its host resolves to AAAA but not A;
            # a bare literal like [::1] counts too.
            if ":" in server and "." not in server.split(":")[0]:
                v6.append(proxy)
                continue
            try:
                infos = socket.getaddrinfo(server, None)
            except socket.gaierror:
                continue
            families = {i[0] for i in infos}
            if socket.AF_INET6 in families and socket.AF_INET not in families:
                v6.append(proxy)
            else:
                v4.append(proxy)
    return v6, v4


def build(path, proxies, ipv6):
    lines = [
        "mixed-port: 0", "allow-lan: false", "mode: rule", "log-level: warning",
        f"ipv6: {str(ipv6).lower()}", "unified-delay: true",
        "external-controller: 0.0.0.0:9090", f'secret: "{SECRET}"',
        "dns:", "  enable: true", f"  ipv6: {str(ipv6).lower()}",
        "  enhanced-mode: fake-ip", "  fake-ip-range: 198.18.0.1/16",
        "  nameserver:", "    - 223.5.5.5", "    - 1.1.1.1",
        "proxies:",
    ]
    names = []
    for i, proxy in enumerate(proxies):
        p = {k: v for k, v in proxy.items()
             if k not in ("dialer-proxy", "interface-name", "routing-mark")}
        p["name"] = f"n{i}"
        names.append((p["name"], str(proxy.get("server"))))
        lines.append("  - " + json.dumps(p, ensure_ascii=False))
    lines += ["proxy-groups:", '  - name: "P"', "    type: select", "    proxies:"]
    for n, _s in names:
        lines.append("      - " + json.dumps(n))
    lines += ["rules:", "  - MATCH,P", ""]
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))
    return names


def run(label, proxies, ipv6, network):
    path = os.path.join(TD, f"{label}.yaml")
    names = build(path, proxies, ipv6)
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    cmd = [
        "docker", "run", "-d", "--name", RUNNER,
        "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
        "--pids-limit", "512", "--memory", "384m",
        "-p", f"127.0.0.1:{PORT}:9090",
        "-v", f"{path}:/root/.config/mihomo/config.yaml:ro",
    ]
    if network:
        cmd += ["--network", network]
    cmd += ["metacubex/mihomo:latest", "-d", "/root/.config/mihomo",
            "-f", "/root/.config/mihomo/config.yaml"]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        print(f"  docker run failed: {proc.stderr[-250:]}")
        return None

    ready = False
    for _ in range(25):
        try:
            req = urllib.request.Request(
                f"http://127.0.0.1:{PORT}/version",
                headers={"Authorization": f"Bearer {SECRET}"})
            with urllib.request.urlopen(req, timeout=3) as resp:
                if resp.status == 200:
                    ready = True
                    break
        except Exception:
            pass
        time.sleep(1.5)
    if not ready:
        print("  core not ready")
        subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
        return None

    ok = 0
    samples = []
    for name, server in names:
        query = urllib.parse.urlencode({"timeout": 6000, "url": TEST_URL, "expected": "204"})
        req = urllib.request.Request(
            f"http://127.0.0.1:{PORT}/proxies/{name}/delay?{query}",
            headers={"Authorization": f"Bearer {SECRET}"})
        try:
            with urllib.request.urlopen(req, timeout=16) as resp:
                delay = json.load(resp).get("delay")
                ok += 1
                samples.append(f"{server[:38]} -> {delay}ms")
        except urllib.error.HTTPError as exc:
            detail = json.loads(exc.read().decode("utf-8", "replace") or "{}")
            samples.append(f"{server[:38]} -> HTTP{exc.code} {str(detail.get('message'))[:40]}")
        except Exception as exc:
            samples.append(f"{server[:38]} -> {str(exc)[:50]}")
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    return {"alive": ok, "total": len(names), "samples": samples}


def main():
    os.makedirs(TD, exist_ok=True)
    v6, v4 = collect()
    print(f"IPv6-only candidates: {len(v6)}   IPv4: {len(v4)}")
    if not v6:
        print("no IPv6-only nodes found")
        return 1

    v6_sample = v6[:8]
    report = {}
    for v6flag in (False, True):
        label = f"v6-{v6flag}"
        net = NETWORK if v6flag else None
        result = run(label, v6_sample, v6flag, net)
        report[label] = result
        if result:
            print(f"\n=== config ipv6={v6flag}, network={net or 'default bridge'} ===")
            print(f"  alive {result['alive']}/{result['total']}")
            for line in result["samples"][:6]:
                print("   ", line)

    print("\n=== verdict ===")
    off = report.get("v6-False", {}) or {}
    on = report.get("v6-True", {}) or {}
    print(f"  default bridge (no IPv6)      : {off.get('alive')}/{off.get('total')} alive")
    print(f"  {NETWORK} + ipv6:true : {on.get('alive')}/{on.get('total')} alive")
    return 0


if __name__ == "__main__":
    sys.exit(main())