#!/usr/bin/env python3
"""Get the kernel's actual reason for the IPv6-only node failures.

Runs mihomo with log-level: debug in an isolated container, triggers one delay
test, and prints the kernel's own log lines. Also checks whether an
IPv6-enabled Docker network actually provides IPv6 egress.
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = "/srv/mihomo-test"
TD = "/tmp/v6dbg"
RUNNER = "v6dbg-core"
NETWORK = os.environ.get("V6_NETWORK", "healthcheck-v6")
PORT = 19295
SECRET = "dbgtest"
TEST_URL = "http://connectivitycheck.platform.hicloud.com/generate_204"

sys.path.insert(0, BASE)
from mihomo_test import config as cfgmod  # noqa: E402
from mihomo_test.store import Client  # noqa: E402


def find_v6_node():
    """Return the first IPv6-only proxy found in the enabled sources."""
    import socket

    cfg = cfgmod.load()
    client = Client(cfg["substore"]["backend"])
    for source in cfg["sources"]:
        if not source.get("enabled"):
            continue
        try:
            proxies = client.fetch_source(source["kind"], source["name"])
        except Exception:
            continue
        for proxy in proxies:
            server = str(proxy.get("server") or "")
            if ":" in server and "." not in server.split(":")[0]:
                return proxy
            try:
                families = {i[0] for i in socket.getaddrinfo(server, None)}
            except socket.gaierror:
                continue
            if socket.AF_INET6 in families and socket.AF_INET not in families:
                return proxy
    return None


def build(path, proxy):
    p = {k: v for k, v in proxy.items()
         if k not in ("dialer-proxy", "interface-name", "routing-mark")}
    p["name"] = "target"
    lines = [
        "mixed-port: 0", "allow-lan: false", "mode: rule", "log-level: debug",
        "ipv6: true", "unified-delay: true",
        "external-controller: 0.0.0.0:9090", f'secret: "{SECRET}"',
        "dns:", "  enable: true", "  ipv6: true",
        "  enhanced-mode: fake-ip", "  fake-ip-range: 198.18.0.1/16",
        "  nameserver:", "    - 223.5.5.5", "    - 1.1.1.1",
        "proxies:", "  - " + json.dumps(p, ensure_ascii=False),
        "proxy-groups:", '  - name: "P"', "    type: select", "    proxies:",
        '      - "target"',
        "rules:", "  - MATCH,P", "",
    ]
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))
    return p


def main():
    os.makedirs(TD, exist_ok=True)
    node = find_v6_node()
    if not node:
        print("no IPv6-only node found in the enabled sources")
        return 1
    summary = {k: node.get(k) for k in ("name", "type", "server", "port",
                                        "sni", "skip-cert-verify", "udp")}
    print("target node:", json.dumps(summary, ensure_ascii=False))
    print("all keys:", sorted(node.keys()))

    path = os.path.join(TD, "cfg.yaml")
    build(path, node)

    for label, network in (("default bridge", None), (NETWORK, NETWORK)):
        subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
        cmd = ["docker", "run", "-d", "--name", RUNNER,
               "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
               "--pids-limit", "512", "--memory", "384m",
               "-p", f"127.0.0.1:{PORT}:9090",
               "-v", f"{path}:/root/.config/mihomo/config.yaml:ro"]
        if network:
            cmd += ["--network", network]
        cmd += ["metacubex/mihomo:latest", "-d", "/root/.config/mihomo",
                "-f", "/root/.config/mihomo/config.yaml"]
        run = subprocess.run(cmd, capture_output=True, text=True)
        print(f"\n########## {label} ##########")
        if run.returncode != 0:
            print("docker run failed:", run.stderr[-250:])
            continue

        # how does the container's own IPv6 look?
        net = subprocess.run(
            ["docker", "exec", RUNNER, "sh", "-c",
             "cat /proc/net/if_inet6 2>/dev/null | head -3; echo '--- route ---'; "
             "ip -6 route 2>/dev/null | head -5; echo '--- egress ---'; "
             "(wget -q -O- -T 6 'http://[2606:4700:4700::1111]/' >/dev/null 2>&1 "
             "&& echo 'IPv6 egress OK' || echo 'IPv6 egress FAILED')"],
            capture_output=True, text=True)
        print("container ipv6:", net.stdout.strip()[:400])

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
            print("core not ready; logs:")
            print(subprocess.run(["docker", "logs", "--tail", "20", RUNNER],
                                 capture_output=True, text=True).stdout[-800:])
            continue

        query = urllib.parse.urlencode({"timeout": 6000, "url": TEST_URL, "expected": "204"})
        req = urllib.request.Request(
            f"http://127.0.0.1:{PORT}/proxies/target/delay?{query}",
            headers={"Authorization": f"Bearer {SECRET}"})
        try:
            with urllib.request.urlopen(req, timeout=18) as resp:
                print("delay result:", resp.read().decode()[:120])
        except urllib.error.HTTPError as exc:
            print("delay HTTP", exc.code, exc.read().decode("utf-8", "replace")[:140])
        except Exception as exc:
            print("delay error:", str(exc)[:140])

        time.sleep(1.5)
        logs = subprocess.run(["docker", "logs", "--tail", "60", RUNNER],
                              capture_output=True, text=True)
        interesting = [ln for ln in (logs.stdout + logs.stderr).splitlines()
                       if any(k in ln.lower() for k in
                              ("error", "warn", "fail", "dial", "resolve", "handshake",
                               "timeout", "refused", "unreachable", "target"))]
        print("kernel log (filtered):")
        for line in interesting[-18:]:
            print("   ", line[:200])
        subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())