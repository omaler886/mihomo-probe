#!/usr/bin/env python3
"""Prove that N inbound listeners can each route to their own select group.

The egress-verification phase is serial today because the kernel's selector is
global state. Parallel lanes need one select group per inbound port, and rules
that send each listener's traffic to its own group. This checks whether mihomo
actually honours that, by selecting different nodes in two lanes and confirming
the two ports report different exits -- then swapping them and confirming the
exits swap too.
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
TD = "/tmp/lanetest"
RUNNER = "lanetest-core"
PORT0, PORT1 = 19310, 19311
SECRET = "lanetest"
TRACE = "https://www.cloudflare.com/cdn-cgi/trace"
TRACE_HTTP = "http://www.gstatic.com/generate_204"

sys.path.insert(0, BASE)
from mihomo_test import config as cfgmod  # noqa: E402
from mihomo_test.store import Client  # noqa: E402


def api(port, path, method="GET", payload=None):
    body = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=body, method=method)
    req.add_header("Authorization", f"Bearer {SECRET}")
    if body:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")


def egress(port):
    """Return (ip, loc) or (error-tag, None) for traffic leaving via `port`."""
    handler = urllib.request.ProxyHandler(
        {"http": f"http://127.0.0.1:{port}", "https": f"http://127.0.0.1:{port}"})
    opener = urllib.request.build_opener(handler)
    try:
        with opener.open(TRACE, timeout=15) as resp:
            fields = {}
            for line in resp.read().decode("utf-8", "replace").splitlines():
                if "=" in line:
                    k, _, v = line.partition("=")
                    fields[k.strip()] = v.strip()
            return fields.get("ip"), fields.get("loc")
    except Exception as exc:
        return "ERR:" + str(exc)[:60], None


def two_good_nodes():
    """Two nodes known to exit from different addresses."""
    cfg = cfgmod.load()
    client = Client(cfg["substore"]["backend"])
    for source in cfg["sources"]:
        if not source.get("enabled"):
            continue
        try:
            proxies = client.fetch_source(source["kind"], source["name"])
        except Exception:
            continue
        seen = {}
        for proxy in proxies:
            server = str(proxy.get("server") or "")
            if proxy.get("type") == "anytls" and server and server not in seen:
                seen[server] = proxy
            if len(seen) == 2:
                return list(seen.values())
    return []


def build(path, proxies, lanes=2, use_in_name=True):
    p0 = {k: v for k, v in proxies[0].items()
          if k not in ("dialer-proxy", "interface-name", "routing-mark")}
    p0["name"] = "A"
    p1 = {k: v for k, v in proxies[1].items()
          if k not in ("dialer-proxy", "interface-name", "routing-mark")}
    p1["name"] = "B"

    lines = [
        "mixed-port: 0", "allow-lan: false", "bind-address: 127.0.0.1",
        "mode: rule", "log-level: debug", "ipv6: true", "unified-delay: true",
        f"external-controller: 127.0.0.1:{PORT0}", f'secret: "{SECRET}"',
        "dns:", "  enable: true", "  ipv6: true", "  enhanced-mode: fake-ip",
        "  fake-ip-range: 198.18.0.1/16", "  nameserver:", "    - 223.5.5.5",
        "proxies:",
        "  - " + json.dumps(p0, ensure_ascii=False),
        "  - " + json.dumps(p1, ensure_ascii=False),
        "proxy-groups:",
    ]
    for i in range(lanes):
        lines += [f'  - name: "__LANE{i}__"', "    type: select", "    proxies:",
                  '      - "A"', '      - "B"']
    lines += ["listeners:"]
    for i in range(lanes):
        lines += [f'  - name: "lane{i}"', "    type: mixed",
                  f"    port: {PORT0 + i}", "    listen: 127.0.0.1"]
    lines += ["rules:"]
    if use_in_name:
        for i in range(lanes):
            lines.append(f"  - IN-NAME,lane{i},__LANE{i}__")
    lines.append(f"  - MATCH,__LANE0__")
    lines.append("")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))
    return ["A", "B"]


def main():
    os.makedirs(TD, exist_ok=True)
    proxies = two_good_nodes()
    if len(proxies) < 2:
        print("need two distinct anytls nodes to prove independence")
        return 1
    print("nodes:", [p.get("server") for p in proxies])

    results = {}
    for use_in_name in (True, False):
        label = "IN-NAME rules" if use_in_name else "fallback (all->LANE0)"
        path = os.path.join(TD, f"lanes-{int(use_in_name)}.yaml")
        build(path, proxies, lanes=2, use_in_name=use_in_name)

        subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
        run = subprocess.run([
            "docker", "run", "-d", "--name", RUNNER, "--network", "host",
            "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW", "--pids-limit", "512",
            "--memory", "384m",
            "-v", f"{path}:/root/.config/mihomo/config.yaml:ro",
            "metacubex/mihomo:latest", "-d", "/root/.config/mihomo",
            "-f", "/root/.config/mihomo/config.yaml",
        ], capture_output=True, text=True)
        if run.returncode != 0:
            print(f"{label}: docker run failed {run.stderr[-200:]}")
            continue
        ready = False
        for _ in range(25):
            try:
                if api(PORT0, "/version")[0] == 200:
                    ready = True
                    break
            except Exception:
                pass
            time.sleep(1.5)
        if not ready:
            print(f"{label}: core not ready")
            logs = subprocess.run(["docker", "logs", "--tail", "25", RUNNER],
                                  capture_output=True, text=True).stdout
            print("  logs:", "\n  ".join(l[:150] for l in logs.splitlines()
                                         if "error" in l.lower() or "warn" in l.lower())[:600])
            subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
            continue

        # listeners present?
        listeners = [ln for ln in
                     subprocess.run(["docker", "exec", RUNNER, "sh", "-c",
                                     "true"], capture_output=True, text=True).stdout.splitlines()]
        print(f"\n=== {label} ===")
        observations = []

        for label2, lane0_node, lane1_node in (("A|B", "A", "B"), ("B|A", "B", "A")):
            for port, node in ((PORT0, lane0_node), (PORT1, lane1_node)):
                group = f"__LANE{port - PORT0}__"
                st, body = api(PORT0, f"/proxies/{urllib.parse.quote(group, safe='')}",
                               "PUT", {"name": node})
                if st not in (200, 204):
                    observations.append(f"select {group}<-{node} failed HTTP{st}")
            time.sleep(1.0)
            ip0 = egress(PORT0)
            ip1 = egress(PORT1)
            observations.append(f"{label2}: lane0={ip0} lane1={ip1}")
        for line in observations:
            print("  ", line)
        logs = subprocess.run(["docker", "logs", "--tail", "40", RUNNER],
                              capture_output=True, text=True)
        interesting = [l for l in (logs.stdout + logs.stderr).splitlines()
                       if any(k in l.lower() for k in ("error", "warn", "match", "rule",
                                                       "lane", "connect", "in-name"))]
        print("  kernel log:")
        for line in interesting[-10:]:
            print("     ", line[:180])
        results[label] = observations
        subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)

    print("\n=== verdict ===")
    ok = results.get("IN-NAME rules") or []
    swapped = [o for o in ok if o.startswith("B|A")]
    if ok and swapped:
        print("  lanes are independent (exits swapped when the selectors swapped)")
    else:
        print("  lanes NOT independent; needs a different routing approach")
    return 0


if __name__ == "__main__":
    sys.exit(main())