#!/usr/bin/env python3
"""Ablation: which missing field killed the edgetunnel front in my first test?

Four variants of the same node, differing only in whether ech-opts and the
x-padding-* family are present. Each runs in an isolated kernel and gets one
delay test. This pinpoints the field the share-link parser must preserve.
"""
import json
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

sys.path.insert(0, "/srv/mihomo-test")
TD = "/tmp/ablation"
RUNNER = "ablation-core"
PORT = 19294
SECRET = "ablate"

BASE = {"type": "vless", "name": "front", "server": "edge.example.net", "port": 443,
        "uuid": "00000000-0000-4000-8000-000000000002", "udp": True, "tls": True,
        "client-fingerprint": "chrome", "skip-cert-verify": False,
        "packet-encoding": "xudp", "encryption": "none",
        "servername": "front.example.com", "network": "xhttp"}
ECH = {"enable": True, "_dns": "https://dns.alidns.com/dns-query",
       "query-server-name": "cloudflare-ech.com"}
XHTTP_FULL = {"host": "front.example.com", "path": "/", "mode": "stream-one",
              "x-padding-obfs-mode": True, "x-padding-key": "_000000",
              "x-padding-header": "abcdef",
              "x-padding-placement": "queryInHeader", "x-padding-method": "tokenish"}
XHTTP_PLAIN = {"host": "front.example.com", "path": "/", "mode": "stream-one"}


def variant(with_ech, with_padding):
    proxy = dict(BASE)
    if with_ech:
        proxy["ech-opts"] = ECH
    proxy["xhttp-opts"] = dict(XHTTP_FULL if with_padding else XHTTP_PLAIN)
    label = "ech" if with_ech else "-"
    label += "+padding" if with_padding else "+无padding"
    return label, proxy


def run(label, proxy):
    import os
    os.makedirs(TD, exist_ok=True)
    path = f"{TD}/cfg.yaml"
    doc = {"mixed-port": 0, "mode": "rule", "log-level": "warning",
           "external-controller": f"127.0.0.1:{PORT}", "secret": SECRET,
           "dns": {"enable": True, "ipv6": True, "nameserver": ["1.1.1.1"]},
           "proxies": [proxy]}
    with open(path, "w", encoding="utf-8") as fh:
        import yaml
        yaml.safe_dump(doc, fh, allow_unicode=True, sort_keys=False)
    proc = subprocess.run(
        ["docker", "run", "--rm", "-v", f"{path}:/root/.config/mihomo/config.yaml:ro",
         "metacubex/mihomo:latest", "-t", "-d", "/root/.config/mihomo",
         "-f", "/root/.config/mihomo/config.yaml"], capture_output=True, text=True)
    if proc.returncode != 0:
        return "配置被拒绝"
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    subprocess.run(["docker", "run", "-d", "--name", RUNNER, "--network", "host",
                    "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
                    "-v", f"{path}:/root/.config/mihomo/config.yaml:ro",
                    "metacubex/mihomo:latest", "-d", "/root/.config/mihomo",
                    "-f", "/root/.config/mihomo/config.yaml"],
                   capture_output=True, text=True)
    ready = False
    for _ in range(20):
        try:
            req = urllib.request.Request(f"http://127.0.0.1:{PORT}/version",
                                         headers={"Authorization": f"Bearer {SECRET}"})
            with urllib.request.urlopen(req, timeout=3) as resp:
                if resp.status == 200:
                    ready = True
                    break
        except Exception:
            pass
        time.sleep(1.2)
    if not ready:
        return "内核未就绪"
    query = urllib.parse.urlencode(
        {"timeout": 8000, "url": "http://connectivitycheck.platform.hicloud.com/generate_204",
         "expected": "204"})
    try:
        req = urllib.request.Request(
            f"http://127.0.0.1:{PORT}/proxies/front/delay?{query}",
            headers={"Authorization": f"Bearer {SECRET}"})
        with urllib.request.urlopen(req, timeout=16) as resp:
            result = f"OK {json.load(resp).get('delay')}ms"
    except urllib.error.HTTPError as exc:
        result = f"HTTP {exc.code}"
    except Exception as exc:
        result = f"{type(exc).__name__}"
    time.sleep(0.8)
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    return result


def main():
    print("=== 消融：同一个节点，只差字段 ===")
    for with_ech, with_padding in ((True, True), (True, False), (False, True), (False, False)):
        label, proxy = variant(with_ech, with_padding)
        print(f"  ech={'有' if with_ech else '无'} padding={'有' if with_padding else '无':2s} -> ", end="")
        print(run(label, proxy))
        time.sleep(0.5)
    return 0


if __name__ == "__main__":
    sys.exit(main())