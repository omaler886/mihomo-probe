#!/usr/bin/env python3
"""Focus on the pasted edgetunnel front: is it usable from vps at all?

The full chain matrix already showed every CDN-collection front works while the
pasted xhttp front times out for all four targets. This isolates the front
itself: kernel delay, raw TCP/TLS reachability, DNS view, and the kernel's own
error line -- to separate "mihomo cannot speak this node's dialect" from "the
node is unreachable from here".
"""
import json
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, "/srv/mihomo-test")
TD = "/tmp/frontdbg"
RUNNER = "frontdbg-core"
PORT = 19299
SECRET = "frontdbg"

FRONT_URI = ("vless://00000000-0000-4000-8000-000000000002@edge.example.net:443"
             "?encryption=none&security=tls&sni=front.example.com&fp=chrome"
             "&ech=cloudflare-ech.com%2Bhttps%3A%2F%2Fdns.alidns.com%2Fdns-query"
             "&type=xhttp&host=front.example.com&path=%2F&mode=stream-one"
             "&extra=%7B%22xPaddingObfsMode%22%3Atrue%2C%22xPaddingMethod%22%3A"
             "%22tokenish%22%2C%22xPaddingPlacement%22%3A%22queryInHeader%22%2C"
             "%22xPaddingHeader%22%3A%22abcdef%22%2C%22xPaddingKey%22%3A%22_000000%22%7D"
             "#edgetunnel")


def parse_front():
    import urllib.parse
    rest = FRONT_URI[len("vless://"):]
    rest, frag = rest.split("#", 1)
    rest, query = rest.split("?", 1)
    uuid, hostport = rest.rsplit("@", 1)
    host, port = hostport.rsplit(":", 1)
    q = {k: v[0] for k, v in urllib.parse.parse_qs(query).items()}
    variants = []
    for mode in ("stream-one", "stream-up", "auto"):
        variants.append({
            "name": f"front-{mode}", "type": "vless", "server": host,
            "port": int(port), "uuid": uuid, "udp": True, "tls": True,
            "servername": q.get("sni"), "client-fingerprint": q.get("fp"),
            "network": "xhttp",
            "xhttp-opts": {"mode": mode, "path": q.get("path", "/"),
                           "host": q.get("host", "")},
        })
    return host, port, q, variants


def main():
    import os
    os.makedirs(TD, exist_ok=True)
    import yaml

    host, port, q, variants = parse_front()
    print(f"front: {host}:{port}  sni={q.get('sni')}  transport=xhttp/{q.get('mode')}")

    print("\n=== DNS 视角 ===")
    sys.path.insert(0, "/srv/mihomo-test")
    from mihomo_test import doh
    for label, resolver, ecs in (
            ("vps 本机解析", None, None),
            ("CN doh.pub + ECS{114}", "https://doh.pub/dns-query", "114.114.114.114"),
            ("海外 cloudflare + ECS{8.8.8.8}", "https://cloudflare-dns.com/dns-query", "8.8.8.8")):
        try:
            if resolver is None:
                import socket
                ips = sorted({i[4][0] for i in socket.getaddrinfo(host, None)})
            else:
                ips = doh.query(host, doh.TYPE_A, resolver, ecs) + \
                    doh.query(host, doh.TYPE_AAAA, resolver, ecs)
            print(f"  {label:32s} -> {ips[:4]}")
        except Exception as exc:
            print(f"  {label:32s} -> 失败 {str(exc)[:50]}")

    print("\n=== TCP / TLS 可达性（宿主机直连） ===")
    import socket as sock
    try:
        with sock.create_connection((host, int(port)), timeout=8) as s:
            print(f"  TCP connect: OK (peer={s.getpeername()})")
    except Exception as exc:
        print(f"  TCP connect: 失败 {exc}")

    print("\n=== 内核对三种 xhttp mode 的态度 ===")
    for variant in variants:
        path = f"{TD}/{variant['xhttp-opts']['mode']}.yaml"
        doc = {"mixed-port": 0, "mode": "rule", "log-level": "debug",
               "external-controller": f"127.0.0.1:{PORT}", "secret": SECRET,
               "dns": {"enable": True, "ipv6": True,
                       "nameserver": ["1.1.1.1"]},
               "proxies": [variant]}
        with open(path, "w", encoding="utf-8") as fh:
            yaml.safe_dump(doc, fh, allow_unicode=True, sort_keys=False)
        proc = subprocess.run(
            ["docker", "run", "--rm",
             "-v", f"{path}:/root/.config/mihomo/config.yaml:ro",
             "metacubex/mihomo:latest", "-t", "-d", "/root/.config/mihomo",
             "-f", "/root/.config/mihomo/config.yaml"],
            capture_output=True, text=True)
        tail = (proc.stderr or proc.stdout).strip().splitlines()
        tail = tail[-1][:150] if tail else ""
        print(f"  mode={variant['xhttp-opts']['mode']:12s} config "
              f"{'接受' if proc.returncode == 0 else '拒绝: ' + tail}")

    # live test with the first accepted mode
    print("\n=== 实测（逐 mode 起内核测延迟并抓内核报错） ===")
    for variant in variants:
        mode = variant["xhttp-opts"]["mode"]
        path = f"{TD}/live-{mode}.yaml"
        doc = {"mixed-port": 0, "mode": "rule", "log-level": "debug",
               "external-controller": f"127.0.0.1:{PORT}", "secret": SECRET,
               "dns": {"enable": True, "ipv6": True, "nameserver": ["1.1.1.1"]},
               "proxies": [variant]}
        with open(path, "w", encoding="utf-8") as fh:
            yaml.safe_dump(doc, fh, allow_unicode=True, sort_keys=False)
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
            print(f"  mode={mode}: 内核未就绪")
            continue
        name = variant["name"]
        query = (f"timeout=8000&url=http://connectivitycheck.platform.hicloud.com"
                 f"/generate_204&expected=204")
        try:
            req = urllib.request.Request(
                f"http://127.0.0.1:{PORT}/proxies/{name}/delay?{query}",
                headers={"Authorization": f"Bearer {SECRET}"})
            with urllib.request.urlopen(req, timeout=16) as resp:
                print(f"  mode={mode:12s} delay = {json.load(resp).get('delay')}ms  ✓")
        except Exception as exc:
            print(f"  mode={mode:12s} delay = 失败 ({str(exc)[:60]})")
        time.sleep(1.0)
        logs = subprocess.run(["docker", "logs", "--tail", "60", RUNNER],
                              capture_output=True, text=True)
        interesting = [l.split("msg=")[-1][:130] for l in (logs.stdout + logs.stderr).splitlines()
                       if any(k in l.lower() for k in ("error", "warn", "dial", "xhttp",
                                                       "403", "404", "refused", "reset",
                                                       "handshake"))]
        print("   kernel:", "\n           ".join(interesting[-6:]) or "(无相关错误)")
        subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())