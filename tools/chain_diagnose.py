#!/usr/bin/env python3
"""Direct vs chained-through-a-front, for the four pasted VLESS targets.

The user reports: direct works, chained through the cdn front does not. This
builds one kernel config containing the fronts (the pasted edgetunnel xhttp
node plus the CDN collection), the targets direct, and one chained variant per
front x target, then delay-tests everything and captures the kernel's own dial
errors -- the log line says which hop broke, where "it doesn't work" does not.

Runs an isolated kernel on host networking (port 19296); production untouched.
"""
import json
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = "/srv/mihomo-test"
TD = "/tmp/chaintest"
RUNNER = "chaintest-core"
PORT = 19296
SECRET = "chaintest"
TEST_URL = "http://connectivitycheck.platform.hicloud.com/generate_204"

sys.path.insert(0, BASE)
from mihomo_test.store import Client  # noqa: E402

TARGET_URIS = [
    "vless://00000000-0000-4000-8000-000000000003@198.51.100.20:8443?encryption=none&security=tls&sni=ee.example.net&fp=chrome&type=tcp&headerType=none#EE-IR02",
    "vless://00000000-0000-4000-8000-000000000001@jp3.example.net:443?encryption=none&flow=xtls-rprx-vision&security=reality&sni=www.bandainamcoent.co.jp&fp=chrome&pbk=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA&sid=0123456789abcd&type=tcp&headerType=none#JP02",
    "vless://00000000-0000-4000-8000-000000000001@jp1.example.net:443?encryption=none&flow=xtls-rprx-vision&security=reality&sni=www.capcom.co.jp&fp=chrome&pbk=BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB&sid=0123456789&type=tcp&headerType=none#JP04",
    "vless://00000000-0000-4000-8000-000000000001@jp2.example.net:443?encryption=none&flow=xtls-rprx-vision&security=reality&sni=www.capcom.co.jp&fp=chrome&pbk=CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC&sid=9876543210&type=tcp&headerType=none#JP05",
]
FRONT_URI = ("vless://00000000-0000-4000-8000-000000000002@edge.example.net:443"
             "?encryption=none&security=tls&sni=front.example.com&fp=chrome"
             "&ech=cloudflare-ech.com%2Bhttps%3A%2F%2Fdns.alidns.com%2Fdns-query"
             "&type=xhttp&host=front.example.com&path=%2F&mode=stream-one"
             "&extra=%7B%22xPaddingObfsMode%22%3Atrue%2C%22xPaddingMethod%22%3A"
             "%22tokenish%22%2C%22xPaddingPlacement%22%3A%22queryInHeader%22%2C"
             "%22xPaddingHeader%22%3A%22abcdef%22%2C%22xPaddingKey%22%3A%22_000000%22%7D"
             "#edgetunnel")


def parse_vless(uri, name):
    """vless share link -> mihomo proxy dict (best effort for Xray extras)."""
    rest = uri[len("vless://"):]
    frag = ""
    if "#" in rest:
        rest, frag = rest.split("#", 1)
    query = ""
    if "?" in rest:
        rest, query = rest.split("?", 1)
    uuid, hostport = rest.rsplit("@", 1)
    host, port = hostport.rsplit(":", 1)
    q = {k: v[0] for k, v in urllib.parse.parse_qs(query).items()}
    proxy = {
        "name": name, "type": "vless", "server": host, "port": int(port),
        "uuid": uuid, "udp": True,
    }
    security = (q.get("security") or "none").lower()
    if security in ("tls", "reality"):
        proxy["tls"] = True
        if q.get("sni"):
            proxy["servername"] = q["sni"]
        if q.get("fp"):
            proxy["client-fingerprint"] = q["fp"]
    if security == "reality":
        reality = {"public-key": q.get("pbk", "")}
        if q.get("sid"):
            reality["short-id"] = q["sid"]
        proxy["reality-opts"] = reality
    if q.get("flow"):
        proxy["flow"] = q["flow"]
    network = (q.get("type") or "tcp").lower()
    if network in ("ws", "grpc", "h2", "xhttp"):
        proxy["network"] = network
    if q.get("host"):
        host_header = q["host"]
    if network == "xhttp":
        mode = (q.get("mode") or "auto").lower()
        opts = {"path": q.get("path", "/"), "host": q.get("host", "")}
        if mode in ("stream-one", "stream-up", "packet-up", "auto"):
            opts["mode"] = mode
        proxy["xhttp-opts"] = opts
    if q.get("ech"):
        # Xray fetches the ECHConfigList through this DoH URL; mihomo has no
        # equivalent for a config-list URL, so it is dropped (TLS then exposes
        # the SNI, which for a CF-fronted own-domain node is not fatal).
        proxy["_dropped_ech"] = q["ech"]
    return proxy, q


def yamlize(proxies):
    import json as _json
    return "\n".join("  - " + _json.dumps(p, ensure_ascii=False) for p in proxies)


def build(path, proxies, fronts):
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
    for f in fronts:
        lines.append(f'      - "{f}"')
    lines += ["listeners:",
              '  - name: "exitlane"', "    type: mixed", "    port: 19297",
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
        message = json.loads(body).get("message", "")[:60]
    except ValueError:
        message = body[:60]
    return f"HTTP{status}", message


def kernel_errors(runner, limit=14):
    logs = subprocess.run(["docker", "logs", "--tail", "80", runner],
                          capture_output=True, text=True)
    lines = (logs.stdout + logs.stderr).splitlines()
    out = []
    for line in lines:
        low = line.lower()
        if any(k in low for k in ("error", "dial", "refused", "timeout", "reset",
                                  "handshake", "unreachable", "xhttp", "reality")):
            if "level=debug" in line and "[DNS]" in line:
                continue
            out.append(line.split("msg=")[-1][:150])
    return out[-limit:]


def main():
    import os
    os.makedirs(TD, exist_ok=True)
    import yaml

    # fronts: the pasted xhttp node, then the CDN collection members
    front_proxy, front_q = parse_vless(FRONT_URI, "FRONT-edgetunnel")
    fronts = [("FRONT-edgetunnel", front_proxy)]
    try:
        cfg = json.loads(open("/srv/mihomo-test/data/config.json", encoding="utf-8").read())
        client = Client(cfg["substore"]["backend"])
        for proxy in client.fetch_proxies("CDN"):
            name = f"CDN-{len(fronts)}"
            fronts.append((name, proxy))
    except Exception as exc:
        print(f"  (CDN collection unavailable: {str(exc)[:80]})")

    targets = []
    for i, uri in enumerate(TARGET_URIS, 1):
        proxy, q = parse_vless(uri, f"T{i}")
        targets.append((f"T{i}", proxy, uri.split("#")[-1]))

    proxies = []
    for name, proxy in fronts:
        p = dict(proxy, name=name)
        if "_dropped_ech" in p:
            p.pop("_dropped_ech")
        proxies.append(p)
    for name, proxy, _f in targets:
        proxies.append(dict(proxy, name=name))
    chain_names = []
    for fi, (fname, _fp) in enumerate(fronts):
        for ti, (tname, tproxy, _f) in enumerate(targets):
            cname = f"F{fi}x{tname}"
            chain_names.append((cname, fname, tname))
            proxies.append(dict(tproxy, name=cname, dialer_proxy=fname))

    # the xhttp front: try stream-one, then degrade until the kernel accepts
    variants = [("stream-one", dict(front_proxy)), ("stream-up", dict(front_proxy))]
    accepted = None
    for mode, variant in variants:
        opts = dict(variant.get("xhttp-opts") or {}, mode=mode)
        trial = [dict(p, name=p["name"]) for p in proxies]
        for p in trial:
            if p["name"] == "FRONT-edgetunnel":
                p["xhttp-opts"] = opts
        path = build(f"{TD}/cfg-{mode}.yaml", trial, [f[0] for f in fronts])
        proc = subprocess.run(
            ["docker", "run", "--rm", "-v", f"{path}:/root/.config/mihomo/config.yaml:ro",
             "metacubex/mihomo:latest", "-t", "-d", "/root/.config/mihomo",
             "-f", "/root/.config/mihomo/config.yaml"],
            capture_output=True, text=True)
        ok = proc.returncode == 0
        print(f"xhttp mode={mode}: config {'accepted' if ok else 'REJECTED'}")
        if not ok:
            print("   ", (proc.stderr or proc.stdout).strip().splitlines()[-1][:160])
        if ok and accepted is None:
            accepted = (mode, trial, path)
    if accepted is None:
        print("kernel rejects every xhttp variant -> that alone explains a dead chain")
        return 1

    mode, proxies, path = accepted
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
        print("core not ready; logs:")
        print(subprocess.run(["docker", "logs", "--tail", "25", RUNNER],
                             capture_output=True, text=True).stdout[-900:])
        return 1

    def exit_ip(front):
        api("/proxies/EXIT", "PUT", {"name": front})
        time.sleep(0.6)
        handler = urllib.request.ProxyHandler(
            {"http": "http://127.0.0.1:19297", "https": "http://127.0.0.1:19297"})
        opener = urllib.request.build_opener(handler)
        try:
            with opener.open("https://www.cloudflare.com/cdn-cgi/trace", timeout=15) as r:
                for line in r.read().decode("utf-8", "replace").splitlines():
                    if line.startswith("ip="):
                        return line[3:]
        except Exception as exc:
            return f"ERR:{str(exc)[:40]}"
        return "?"

    print(f"\n=== front health (xhttp mode={mode}) ===")
    front_exits = {}
    for name, _p in fronts:
        verdict, value = delay(name)
        if name.startswith("FRONT") or True:
            front_exits[name] = exit_ip(name) if verdict == "OK" else "-"
        print(f"  {name:22s} {verdict:8s} {str(value):>6s}   exit={front_exits.get(name, '-')}")
        time.sleep(0.4)

    print("\n=== targets: direct vs chained ===")
    print(f"  {'front':22s} " + " ".join(f"{t:10s}" for t, _p, _f in targets))
    matrix = {}
    for fi, (fname, _fp) in enumerate(fronts):
        row = []
        for ti, (tname, _tp, _f) in enumerate(targets):
            cname = f"F{fi}x{tname}"
            verdict, value = delay(cname)
            matrix[(fname, tname)] = (verdict, value)
            row.append(f"{verdict}/{value}"[:10] if verdict == "OK"
                       else f"{verdict} {str(value)[:14]}")
        print(f"  {fname:22s} " + " ".join(f"{c:>10s}" for c in row))
        time.sleep(0.4)
    # direct row
    row = []
    for tname, _tp, _f in targets:
        verdict, value = delay(tname)
        matrix[("DIRECT", tname)] = (verdict, value)
        row.append(f"{verdict}/{value}"[:10] if verdict == "OK"
                   else f"{verdict} {str(value)[:14]}")
    print(f"  {'DIRECT (无前置)':22s} " + " ".join(f"{c:>10s}" for c in row))

    print("\n=== kernel dial errors during chained tests ===")
    for line in kernel_errors(RUNNER):
        print("   ", line)
    subprocess.run(["docker", "rm", "-f", RUNNER], capture_output=True, text=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())