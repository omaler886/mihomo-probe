#!/usr/bin/env python3
"""A/B test: does dns.ipv6=false kill IPv6-only nodes?

Builds two throwaway mihomo configs that differ only in dns.ipv6, runs each in
an isolated container on its own port, and tests the same nodes through the
kernel's own delay API. Nothing here touches the production core or its config.
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
TD = "/tmp/v6test"
TEST_URL = "http://connectivitycheck.platform.hicloud.com/generate_204"
RUNNER = "v6test-core"

sys.path.insert(0, BASE)
from mihomo_test import config as cfgmod  # noqa: E402
from mihomo_test.store import Client  # noqa: E402


def candidates():
    """Collect the IPv6-ish nodes currently being tested."""
    cfg = cfgmod.load()
    client = Client(cfg["substore"]["backend"])
    out = []
    for source in cfg["sources"]:
        if not source.get("enabled"):
            continue
        try:
            proxies = client.fetch_source(source["kind"], source["name"])
        except Exception as exc:
            print(f"  skip {source['key']}: {str(exc)[:60]}")
            continue
        for proxy in proxies:
            if proxy.get("type") == "trojan" or "v6" in str(proxy.get("server", "")).lower():
                out.append(proxy)
    return out


def build_config(path, proxies, ipv6, secret, port):
    lines = [
        "mixed-port: 0", "allow-lan: false", "mode: rule", "log-level: warning",
        f"ipv6: {str(ipv6).lower()}", "unified-delay: true",
        "external-controller: 0.0.0.0:9090", f'secret: "{secret}"',
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
        names.append(p["name"])
        lines.append("  - " + json.dumps(p, ensure_ascii=False))
    lines += ["proxy-groups:", '  - name: "P"', "    type: select", "    proxies:"]
    for n in names:
        lines.append("      - " + json.dumps(n))
    lines += ["rules:", "  - MATCH,P", ""]
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))
    return names


def start(name, config_path, port, secret):
    subprocess.run(["docker", "rm", "-f", name], capture_output=True, text=True)
    proc = subprocess.run([
        "docker", "run", "-d", "--name", name,
        "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
        "--pids-limit", "512", "--memory", "384m",
        "-p", f"127.0.0.1:{port}:9090",
        "-v", f"{config_path}:/root/.config/mihomo/config.yaml:ro",
        "metacubex/mihomo:latest",
        "-d", "/root/.config/mihomo", "-f", "/root/.config/mihomo/config.yaml",
    ], capture_output=True, text=True)
    if proc.returncode != 0:
        print("  docker run failed:", proc.stderr[-300:])
        return False
    for _ in range(25):
        try:
            req = urllib.request.Request(
                f"http://127.0.0.1:{port}/version",
                headers={"Authorization": f"Bearer {secret}"})
            with urllib.request.urlopen(req, timeout=3) as resp:
                if resp.status == 200:
                    return True
        except Exception:
            pass
        time.sleep(1.5)
    print("  core did not become ready; logs:")
    print(subprocess.run(["docker", "logs", "--tail", "15", name],
                         capture_output=True, text=True).stdout[-600:])
    return False


def test_all(port, names, secret):
    results = {}
    for n in names:
        query = urllib.parse.urlencode({"timeout": 5000, "url": TEST_URL, "expected": "204"})
        url = f"http://127.0.0.1:{port}/proxies/{n}/delay?{query}"
        req = urllib.request.Request(url, headers={"Authorization": f"Bearer {secret}"})
        try:
            with urllib.request.urlopen(req, timeout=14) as resp:
                results[n] = ("ok", json.load(resp).get("delay"))
        except urllib.error.HTTPError as exc:
            body = exc.read().decode("utf-8", "replace")[:120]
            results[n] = (f"HTTP{exc.code}", body)
        except Exception as exc:
            results[n] = ("ERR", str(exc)[:100])
    return results


def main():
    os.makedirs(TD, exist_ok=True)
    secret = "testsecret"
    proxies = candidates()
    print(f"candidates: {len(proxies)}")
    if not proxies:
        print("nothing to test")
        return 1

    report = {}
    for ipv6 in (False, True):
        label = f"ipv6={ipv6}"
        path = os.path.join(TD, f"cfg-{ipv6}.yaml")
        port = 19290 if not ipv6 else 19291
        names = build_config(path, proxies, ipv6, secret, port)

        ok, err = subprocess.run(
            ["docker", "run", "--rm", "-v", f"{path}:/root/.config/mihomo/config.yaml:ro",
             "metacubex/mihomo:latest", "-t", "-d", "/root/.config/mihomo",
             "-f", "/root/.config/mihomo/config.yaml"],
            capture_output=True, text=True).returncode == 0, None
        print(f"\n=== {label} === config accepted: {ok}")

        if not start(RUNNER, path, port, secret):
            report[label] = {"error": "core failed to start"}
            continue
        res = test_all(port, names, secret)
        alive = sum(1 for v in res.values() if v[0] == "ok")
        report[label] = {"alive": alive, "total": len(names),
                         "sample": {k: v for k, v in list(res.items())[:6]}}
        print(f"  alive {alive}/{len(names)}")
        for n, v in list(res.items())[:6]:
            print(f"    {n}: {v[0]} {str(v[1])[:70]}")
        subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)

    print("\n=== verdict ===")
    a = report.get("ipv6=False", {}).get("alive")
    b = report.get("ipv6=True", {}).get("alive")
    print(json.dumps(report, ensure_ascii=False, indent=1)[:1200])
    if a is not None and b is not None:
        print(f"\ndns.ipv6=false -> {a} alive; dns.ipv6=true -> {b} alive")
    return 0


if __name__ == "__main__":
    sys.exit(main())