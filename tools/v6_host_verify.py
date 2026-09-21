#!/usr/bin/env python3
"""Verify the real fix: host networking gives the probe container IPv6 egress.

The host has working IPv6; Docker bridges here do not (default bridge has no
IPv6 at all, and the pre-existing healthcheck-v6 bridge hands out a ULA with no
NAT66 route to the internet). This runs the kernel with --network host and
tests both an IPv6-only node and an IPv4 node as a control.
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
TD = "/tmp/v6host"
RUNNER = "v6host-core"
PORT = 19297
SECRET = "hosttest"
TEST_URL = "http://connectivitycheck.platform.hicloud.com/generate_204"

sys.path.insert(0, BASE)
from mihomo_test import config as cfgmod  # noqa: E402
from mihomo_test.store import Client  # noqa: E402


def classify(server):
    """Return 'v6only', 'v4only', or None for a node's server host."""
    if ":" in server and "." not in server.split(":")[0]:
        return "v6only"
    try:
        families = {i[0] for i in socket.getaddrinfo(server, None)}
    except socket.gaierror:
        return None
    has6, has4 = socket.AF_INET6 in families, socket.AF_INET in families
    if has6 and not has4:
        return "v6only"
    if has4:
        return "v4only"
    return None


def collect():
    cfg = cfgmod.load()
    client = Client(cfg["substore"]["backend"])
    groups = {"v6only": [], "v4only": []}
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
            kind = classify(server)
            if kind:
                groups[kind].append(proxy)
    return groups


def build(path, proxies):
    lines = [
        "mixed-port: 0", "allow-lan: false", "mode: rule", "log-level: warning",
        "ipv6: true", "unified-delay: true",
        f"external-controller: 127.0.0.1:{PORT}", f'secret: "{SECRET}"',
        "dns:", "  enable: true", "  ipv6: true",
        "  enhanced-mode: fake-ip", "  fake-ip-range: 198.18.0.1/16",
        "  nameserver:", "    - 223.5.5.5", "    - 1.1.1.1",
        "proxies:",
    ]
    names = []
    for i, proxy in enumerate(proxies):
        p = {k: v for k, v in proxy.items()
             if k not in ("dialer-proxy", "interface-name", "routing-mark")}
        p["name"] = f"n{i}"
        names.append((p["name"], str(proxy.get("server")), proxy.get("type")))
        lines.append("  - " + json.dumps(p, ensure_ascii=False))
    lines += ["proxy-groups:", '  - name: "P"', "    type: select", "    proxies:"]
    for n, _s, _t in names:
        lines.append("      - " + json.dumps(n))
    lines += ["rules:", "  - MATCH,P", ""]
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))
    return names


def test(names, label):
    ok = 0
    for name, server, proto in names:
        query = urllib.parse.urlencode({"timeout": 6000, "url": TEST_URL, "expected": "204"})
        req = urllib.request.Request(
            f"http://127.0.0.1:{PORT}/proxies/{name}/delay?{query}",
            headers={"Authorization": f"Bearer {SECRET}"})
        try:
            with urllib.request.urlopen(req, timeout=16) as resp:
                delay = json.load(resp).get("delay")
                ok += 1
                print(f"   OK   {proto:9s} {server[:42]:44s} {delay}ms")
        except urllib.error.HTTPError as exc:
            body = json.loads(exc.read().decode("utf-8", "replace") or "{}")
            print(f"   fail {proto:9s} {server[:42]:44s} HTTP{exc.code} "
                  f"{str(body.get('message'))[:34]}")
        except Exception as exc:
            print(f"   fail {proto:9s} {server[:42]:44s} {str(exc)[:34]}")
    return ok


def main():
    os.makedirs(TD, exist_ok=True)
    groups = collect()
    v6 = groups["v6only"][:6]
    v4 = groups["v4only"][:3]
    print(f"IPv6-only: {len(groups['v6only'])}   IPv4: {len(groups['v4only'])}")
    if not v6:
        print("no IPv6-only nodes found")
        return 1

    path = os.path.join(TD, "cfg.yaml")
    names = build(path, v6 + v4)

    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    run = subprocess.run([
        "docker", "run", "-d", "--name", RUNNER, "--network", "host",
        "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
        "--pids-limit", "512", "--memory", "384m",
        "-v", f"{path}:/root/.config/mihomo/config.yaml:ro",
        "metacubex/mihomo:latest", "-d", "/root/.config/mihomo",
        "-f", "/root/.config/mihomo/config.yaml",
    ], capture_output=True, text=True)
    if run.returncode != 0:
        print("docker run failed:", run.stderr[-300:])
        return 1

    net = subprocess.run(
        ["docker", "exec", RUNNER, "sh", "-c",
         "(wget -q -O- -T 6 'http://[2606:4700:4700::1111]/' >/dev/null 2>&1 "
         "&& echo 'IPv6 egress OK' || echo 'IPv6 egress FAILED')"],
        capture_output=True, text=True)
    print("container (host net) ipv6:", net.stdout.strip())

    ready = False
    for _ in range(25):
        try:
            req = urllib.request.Request(f"http://127.0.0.1:{PORT}/version",
                                         headers={"Authorization": f"Bearer {SECRET}"})
            with urllib.request.urlopen(req, timeout=3) as resp:
                if resp.status == 200:
                    ready = True
                    break
        except Exception:
            pass
        time.sleep(1.5)
    if not ready:
        print("core not ready:")
        print(subprocess.run(["docker", "logs", "--tail", "20", RUNNER],
                             capture_output=True, text=True).stdout[-700:])
        subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
        return 1

    print(f"\n=== results with --network host ({len(v6)} IPv6-only + {len(v4)} IPv4) ===")
    ok = test(names, "host")
    print(f"\nalive {ok}/{len(names)}")
    print(f"  IPv6-only: {ok - min(ok, len(v4))} alive (of {len(v6)})")
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())