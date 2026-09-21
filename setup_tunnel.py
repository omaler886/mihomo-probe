#!/usr/bin/env python3
"""Create (or reuse) the Cloudflare Tunnel that exposes the dashboard.

Uses the Cloudflare credentials acme.sh already stores on the host, so no
browser login is needed. Idempotent: re-running only repairs what drifted.

Usage:  sudo python3 setup_tunnel.py [--hostname probe.example.com] [--port 8088]
"""
import argparse
import base64
import json
import os
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path

TUNNEL_NAME = "mihomo-test"
ACME_CONF = "/root/.acme.sh/account.conf"
API = "https://api.cloudflare.com/client/v4"


def credentials():
    """Read the Cloudflare key/email pair out of acme.sh's account file."""
    try:
        text = Path(ACME_CONF).read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        sys.exit(f"cannot read {ACME_CONF}: {exc}")
    def grab(name):
        match = re.search(name + r"=.(.+?).\s*$", text, re.M)
        return match.group(1) if match else None
    key = grab("SAVED_CF_Key") or grab("SAVED_CF_Token")
    email = grab("SAVED_CF_Email")
    if not key:
        sys.exit("no Cloudflare credential found in account.conf")
    return key, email


def make_client(key, email):
    def call(path, method="GET", payload=None):
        body = json.dumps(payload).encode() if payload is not None else None
        req = urllib.request.Request(API + path, data=body, method=method)
        if email:
            req.add_header("X-Auth-Email", email)
            req.add_header("X-Auth-Key", key)
        else:
            req.add_header("Authorization", "Bearer " + key)
        req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=45) as resp:
                return json.load(resp)
        except urllib.error.HTTPError as exc:
            raw = exc.read().decode("utf-8", "replace")
            try:
                return json.loads(raw)
            except ValueError:
                return {"success": False, "errors": [{"message": raw[:300], "code": exc.code}]}
    return call


def ensure_tunnel(call, account_id):
    listing = call(f"/accounts/{account_id}/cfd_tunnel?is_deleted=false&per_page=100")
    if not listing.get("success"):
        sys.exit(f"cannot list tunnels: {json.dumps(listing.get('errors'))[:300]}")
    for item in listing["result"]:
        if item["name"] == TUNNEL_NAME:
            return item["id"], False
    secret = base64.b64encode(os.urandom(32)).decode()
    created = call(f"/accounts/{account_id}/cfd_tunnel", "POST",
                   {"name": TUNNEL_NAME, "tunnel_secret": secret, "config_src": "cloudflare"})
    if not created.get("success"):
        sys.exit(f"cannot create tunnel: {json.dumps(created.get('errors'))[:300]}")
    return created["result"]["id"], True


def ensure_ingress(call, account_id, tunnel_id, hostname, port):
    payload = {
        "config": {
            "ingress": [
                {"hostname": hostname, "service": f"http://127.0.0.1:{port}"},
                {"service": "http_status:404"},
            ]
        }
    }
    result = call(f"/accounts/{account_id}/cfd_tunnel/{tunnel_id}/configurations", "PUT", payload)
    return result.get("success", False), result


def ensure_dns(call, zone_id, hostname, tunnel_id):
    target = f"{tunnel_id}.cfargotunnel.com"
    existing = call(f"/zones/{zone_id}/dns_records?type=CNAME&name={hostname}")
    if existing.get("success") and existing["result"]:
        record = existing["result"][0]
        if record["content"] == target and record.get("proxied"):
            return "unchanged"
        call(f"/zones/{zone_id}/dns_records/{record['id']}", "PUT",
             {"type": "CNAME", "name": hostname, "content": target, "proxied": True, "ttl": 1})
        return "updated"
    created = call(f"/zones/{zone_id}/dns_records", "POST",
                   {"type": "CNAME", "name": hostname, "content": target, "proxied": True, "ttl": 1})
    if not created.get("success"):
        return "failed: " + json.dumps(created.get("errors"))[:200]
    return "created"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--hostname", default="probe.example.com")
    parser.add_argument("--port", type=int, default=8088)
    parser.add_argument("--root", default="/srv/mihomo-test")
    parser.add_argument("--zone", default="example.com")
    args = parser.parse_args()

    key, email = credentials()
    call = make_client(key, email)

    accounts = call("/accounts?per_page=50")
    if not accounts.get("success") or not accounts["result"]:
        sys.exit("cannot read the Cloudflare account list")
    account_id = accounts["result"][0]["id"]

    zones = call(f"/zones?name={args.zone}")
    if not zones.get("success") or not zones["result"]:
        sys.exit(f"zone {args.zone} is not accessible with this credential")
    zone_id = zones["result"][0]["id"]

    tunnel_id, created = ensure_tunnel(call, account_id)
    print(f"tunnel {TUNNEL_NAME}: {'created' if created else 'reused'} ({tunnel_id[:8]}...)")

    ok, result = ensure_ingress(call, account_id, tunnel_id, args.hostname, args.port)
    print("ingress:", "ok" if ok else json.dumps(result.get("errors"))[:200])

    print("dns:", ensure_dns(call, zone_id, args.hostname, tunnel_id))

    token = call(f"/accounts/{account_id}/cfd_tunnel/{tunnel_id}/token")
    if not token.get("success"):
        sys.exit(f"cannot fetch the tunnel token: {json.dumps(token.get('errors'))[:200]}")
    path = Path(args.root) / "data" / "tunnel.token"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(token["result"], encoding="utf-8")
    path.chmod(0o600)
    print(f"token written to {path}")
    print(f"\nURL: https://{args.hostname}/")


if __name__ == "__main__":
    main()
