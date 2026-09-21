#!/usr/bin/env python3
"""Chain test with the user's exact edgetunnel proxy dicts as the front.

The previous attempt failed because I rebuilt the node from its share link and
dropped ech-opts plus the whole x-padding-* family. This run uses the dicts
verbatim: if the chain works now, the earlier failure was my incomplete node,
not the front.
"""
import json
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = "/srv/mihomo-test"
TD = "/tmp/chain2"
RUNNER = "chain2-core"
PORT = 19298
SECRET = "chain2"
TEST_URL = "http://connectivitycheck.platform.hicloud.com/generate_204"

sys.path.insert(0, BASE)

EDGETUNNEL = {"type": "vless", "name": "edgetunnel", "server": "edge.example.net",
              "port": 443, "uuid": "00000000-0000-4000-8000-000000000002",
              "udp": True, "tls": True, "client-fingerprint": "chrome",
              "skip-cert-verify": False,
              "ech-opts": {"enable": True,
                           "_dns": "https://dns.alidns.com/dns-query",
                           "query-server-name": "cloudflare-ech.com"},
              "packet-encoding": "xudp", "network": "xhttp",
              "xhttp-opts": {"host": "front.example.com", "path": "/",
                             "mode": "stream-one", "x-padding-obfs-mode": True,
                             "x-padding-key": "_000000", "x-padding-header": "abcdef",
                             "x-padding-placement": "queryInHeader",
                             "x-padding-method": "tokenish"},
              "encryption": "none", "servername": "front.example.com"}
EDGETUNNEL_RUD = dict(EDGETUNNEL, name="edgetunnel-RUD",
                      xhttp_opts=None) if False else None
EDGETUNNEL_RUD = dict(EDGETUNNEL)
EDGETUNNEL_RUD["name"] = "edgetunnel-RUD"
EDGETUNNEL_RUD["xhttp-opts"] = dict(EDGETUNNEL["xhttp-opts"],
                                    path="/files/about/acg")

TARGETS = [
    {"type": "vless", "name": "T1-EE", "server": "198.51.100.20", "port": 8443,
     "uuid": "00000000-0000-4000-8000-000000000003", "udp": True, "tls": True,
     "client-fingerprint": "chrome", "servername": "ee.example.net",
     "encryption": "none", "network": "tcp"},
    {"type": "vless", "name": "T2-JP02", "server": "jp3.example.net",
     "port": 443, "uuid": "00000000-0000-4000-8000-000000000001", "udp": True,
     "tls": True, "client-fingerprint": "chrome",
     "servername": "www.bandainamcoent.co.jp", "flow": "xtls-rprx-vision",
     "reality-opts": {"public-key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                      "short-id": "0123456789abcd"},
     "encryption": "none", "network": "tcp"},
    {"type": "vless", "name": "T3-JP04", "server": "jp1.example.net",
     "port": 443, "uuid": "00000000-0000-4000-8000-000000000001", "udp": True,
     "tls": True, "client-fingerprint": "chrome",
     "servername": "www.capcom.co.jp", "flow": "xtls-rprx-vision",
     "reality-opts": {"public-key": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
                      "short-id": "0123456789"},
     "encryption": "none", "network": "tcp"},
    {"type": "vless", "name": "T4-JP05", "server": "jp2.example.net",
     "port": 443, "uuid": "00000000-0000-4000-8000-000000000001", "udp": True,
     "tls": True, "client-fingerprint": "chrome",
     "servername": "www.capcom.co.jp", "flow": "xtls-rprx-vision",
     "reality-opts": {"public-key": "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
                      "short-id": "9876543210"},
     "encryption": "none", "network": "tcp"},
]
FRONTS = [EDGETUNNEL, EDGETUNNEL_RUD]


def yamlize(proxies):
    import json as _json
    return "\n".join("  - " + _json.dumps(p, ensure_ascii=False) for p in proxies)


def build(path, proxies):
    lines = [
        "mixed-port: 0", "allow-lan: false", "bind-address: 127.0.0.1",
        "mode: rule", "log-level: debug", "ipv6: true", "unified-delay: true",
        f"external-controller: 127.0.0.1:{PORT}", f'secret: "{SECRET}"',
        "dns:", "  enable: true", "  ipv6: true", "  enhanced-mode: fake-ip",
        "  fake-ip-range: 198.18.0.1/16", "  nameserver:", "    - 1.1.1.1",
        "proxies:", yamlize(proxies),
        "proxy-groups:",
        '  - name: "EXIT"', "    type: select", "    proxies:",
    ]
    for f in FRONTS:
        lines.append(f'      - "{f["name"]}"')
    lines += ["listeners:",
              '  - name: "exitlane"', "    type: mixed", "    port: 19299",
              "    listen: 127.0.0.1",
              "rules:", "  - IN-NAME,exitlane,EXIT", "  - MATCH,DIRECT", ""]
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))
    return path


def api(path, method="GET", payload=None):
    body = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(f"http://127.0.0.1:{PORT}{path}", data=body,
                                 method=method)
    req.add_header("Authorization", f"Bearer {SECRET}")
    if body:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=25) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")


def delay(name, timeout_ms=8000):
    query = urllib.parse.urlencode({"timeout": timeout_ms, "url": TEST_URL,
                                    "expected": "204"})
    status, body = api(f"/proxies/{urllib.parse.quote(name, safe='')}/delay?{query}")
    if status == 200:
        return "OK", json.loads(body).get("delay")
    try:
        return f"HTTP{status}", json.loads(body).get("message", "")[:40]
    except ValueError:
        return f"HTTP{status}", body[:40]


def kernel_errors(limit=12):
    logs = subprocess.run(["docker", "logs", "--tail", "120", RUNNER],
                          capture_output=True, text=True)
    out = []
    for line in (logs.stdout + logs.stderr).splitlines():
        low = line.lower()
        if any(k in low for k in ("error", "dial", "refused", "timeout", "reset",
                                  "handshake", "unreachable", "padding", "ech",
                                  "xhttp", "reality")):
            if "[DNS] resolve" in line:
                continue
            out.append(line.split("msg=")[-1][:160])
    return out[-limit:]


def exit_ip(front):
    api("/proxies/EXIT", "PUT", {"name": front})
    time.sleep(0.6)
    handler = urllib.request.ProxyHandler(
        {"http": "http://127.0.0.1:19299", "https": "http://127.0.0.1:19299"})
    opener = urllib.request.build_opener(handler)
    try:
        with opener.open("https://www.cloudflare.com/cdn-cgi/trace", timeout=15) as r:
            for line in r.read().decode("utf-8", "replace").splitlines():
                if line.startswith("ip="):
                    return line[3:]
    except Exception as exc:
        return f"ERR:{str(exc)[:40]}"
    return "?"


def main():
    import os
    os.makedirs(TD, exist_ok=True)

    proxies = [dict(f) for f in FRONTS] + [dict(t) for t in TARGETS]
    chain_names = []
    for front in FRONTS:
        for target in TARGETS:
            cname = f"X-{front['name']}-{target['name']}"
            chain_names.append((cname, front["name"], target["name"]))
            proxies.append(dict(target, name=cname, dialer_proxy=front["name"]))

    path = build(f"{TD}/cfg.yaml", proxies)
    proc = subprocess.run(
        ["docker", "run", "--rm", "-v", f"{path}:/root/.config/mihomo/config.yaml:ro",
         "metacubex/mihomo:latest", "-t", "-d", "/root/.config/mihomo",
         "-f", "/root/.config/mihomo/config.yaml"],
        capture_output=True, text=True)
    print(f"=== 配置校验（含 ech-opts + x-padding）: "
          f"{'通过' if proc.returncode == 0 else '拒绝'} ===")
    if proc.returncode != 0:
        print((proc.stderr or proc.stdout).strip()[-400:])
        return 1

    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    subprocess.run(["docker", "run", "-d", "--name", RUNNER, "--network", "host",
                    "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
                    "--pids-limit", "512", "--memory", "384m",
                    "-v", f"{path}:/root/.config/mihomo/config.yaml:ro",
                    "metacubex/mihomo:latest", "-d", "/root/.config/mihomo",
                    "-f", "/root/.config/mihomo/config.yaml"],
                   capture_output=True, text=True)
    for _ in range(25):
        try:
            if api("/version")[0] == 200:
                break
        except Exception:
            pass
        time.sleep(1.5)
    else:
        print("内核未就绪; 日志:")
        print(subprocess.run(["docker", "logs", "--tail", "25", RUNNER],
                             capture_output=True, text=True).stdout[-900:])
        return 1

    print("\n=== 前置本体（你给的原始配置，含 ECH + xPadding） ===")
    front_exits = {}
    for front in FRONTS:
        name = front["name"]
        verdict, value = delay(name)
        front_exits[name] = exit_ip(name) if verdict == "OK" else "-"
        print(f"  {name:18s} {verdict:8s} {str(value):>6s}   exit={front_exits[name]}")
        time.sleep(0.4)

    print("\n=== 直连 vs 链式（经 edgetunnel 前置） ===")
    print(f"  {'路径':28s} " + " ".join(f"{t['name']:12s}" for t in TARGETS))
    for front in FRONTS:
        row = []
        for target in TARGETS:
            cname = f"X-{front['name']}-{target['name']}"
            verdict, value = delay(cname)
            row.append(f"{verdict}/{value}"[:12] if verdict == "OK"
                       else f"{verdict} {str(value)[:10]}")
            time.sleep(0.3)
        print(f"  经 {front['name']:12s} " + " ".join(f"{c:>12s}" for c in row))
    row = []
    for target in TARGETS:
        verdict, value = delay(target["name"])
        row.append(f"{verdict}/{value}"[:12] if verdict == "OK"
                   else f"{verdict} {str(value)[:10]}")
        time.sleep(0.3)
    print(f"  {'直连（无前置）':18s} " + " ".join(f"{c:>12s}" for c in row))

    print("\n=== 内核拨号日志（链式期间的报错/REALITY 详情） ===")
    for line in kernel_errors():
        print("   ", line)
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())