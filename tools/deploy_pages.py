"""Deploy the built front end (`dist/`) to Cloudflare Pages.

Run `tools/build_web.py` first; this only publishes what is already on disk.

    python tools/deploy_pages.py --name quiet-harbor --dist dist
    python tools/deploy_pages.py --random-name --dist dist     # pick a free one

Credentials come from the environment, falling back to the account file used by
the other CF tooling on this machine:

    CF_ACCOUNT_ID   (required)
    CF_API_TOKEN    a scoped token -- must include Cloudflare Pages: Edit
    CF_API_KEY      + CF_API_EMAIL   a Global API Key instead

The scoped token this account currently holds does *not* include Pages: Edit
(it is `Zone:DNS:Edit` plus `Zone:Read`), so a deploy needs the Global Key. That
is not a fallback to reach for casually: it is the account-wide credential, and
the project is not to be shared outside this machine.

Wrangler does the actual upload rather than a hand-rolled multipart request. It
batches the files itself and is the supported path; the raw direct-upload API
expects a `manifest` keyed by a specific content hash, and getting that subtly
wrong produces a deployment that looks fine until a file 404s.
"""
import argparse
import json
import os
import random
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
API = "https://api.cloudflare.com/client/v4"

DEFAULT_CREDS = Path(r"D:\ChatGPT\cf优选\cf-dns-route\config.json")
DEFAULT_GLOBAL_KEY = Path(r"D:\ChatGPT\cf优选\cf-dns-route\config.json.globalkey.bak")

# Two words from here read as a name rather than as a hash, and the pages.dev
# subdomain has to be globally unique -- hence the numeric tail when a pair is
# already taken.
WORDS_A = ("quiet amber velvet hollow copper silent marble lantern tundra "
           "cobalt ivory cedar drifting golden nimbus olive pewter russet "
           "sable thistle umber willow").split()
WORDS_B = ("harbor anchor compass orchard beacon ferry granite junction kiln "
           "ledger meadow nucleus outcrop pasture quarry ridge summit terrace "
           "upland vault workshop yard").split()

OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def api(path, headers, method="GET", body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(API + path, data=data, headers=headers,
                                method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with OPENER.open(req, timeout=60) as resp:
            return resp.status, json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", "replace")
        try:
            return exc.code, json.loads(raw)
        except ValueError:
            return exc.code, {"raw": raw[:300]}
    except Exception as exc:
        return None, {"transport_error": f"{type(exc).__name__}: {exc}"}


def load_credentials(creds_path):
    """Return (account_id, headers, env-for-wrangler). Secrets are never printed."""
    account = os.environ.get("CF_ACCOUNT_ID", "").strip()
    token = os.environ.get("CF_API_TOKEN", "").strip()
    key = os.environ.get("CF_API_KEY", "").strip()
    email = os.environ.get("CF_API_EMAIL", "").strip()

    if not account and creds_path.exists():
        stored = json.loads(creds_path.read_text(encoding="utf-8"))
        account = str(stored.get("account_id") or "").strip()
        token = token or str(stored.get("api_token") or "").strip()
    if not account:
        raise SystemExit("缺少 CF_ACCOUNT_ID（或凭证文件里的 account_id）")

    env = {"CF_ACCOUNT_ID": account}
    if key and email:
        headers = {"X-Auth-Email": email, "X-Auth-Key": key}
        env.update({"CLOUDFLARE_API_KEY": key, "CLOUDFLARE_EMAIL": email})
    elif token:
        headers = {"Authorization": f"Bearer {token}"}
        env["CLOUDFLARE_API_TOKEN"] = token
    else:
        raise SystemExit("缺少 CF_API_TOKEN 或 CF_API_KEY + CF_API_EMAIL")
    env["CLOUDFLARE_ACCOUNT_ID"] = account
    return account, headers, env


def list_projects(account, headers):
    status, payload = api(f"/accounts/{account}/pages/projects", headers)
    if status != 200:
        raise SystemExit(f"列出 Pages 项目失败: HTTP {status} {json.dumps(payload)[:300]}")
    return payload.get("result", [])


def ensure_project(account, headers, name, branch="main"):
    """Create the project if it is not there yet; return its record."""
    status, payload = api(f"/accounts/{account}/pages/projects/{name}", headers)
    if status == 200:
        print(f"  项目已存在: {name}  {payload['result'].get('subdomain')}")
        return payload["result"]
    status, payload = api(f"/accounts/{account}/pages/projects", headers,
                          method="POST", body={"name": name,
                                               "production_branch": branch})
    if status in (200, 201):
        print(f"  已创建项目: {name}  {payload['result'].get('subdomain')}")
        return payload["result"]
    raise SystemExit(f"创建项目失败: HTTP {status} {json.dumps(payload)[:300]}")


def pick_free_name(account, headers, attempts=12):
    taken = {p["name"] for p in list_projects(account, headers)}
    for _ in range(attempts):
        candidate = f"{random.choice(WORDS_A)}-{random.choice(WORDS_B)}"
        if candidate in taken:
            candidate = f"{candidate}-{random.randint(100, 999)}"
        if candidate not in taken:
            return candidate
    raise SystemExit("连续取不到空闲项目名，请手动指定 --name")


def deploy(dist, name, env, message="mihomo-test dashboard"):
    """Publish `dist` with wrangler. Returns (ok, combined output)."""
    dist = Path(dist).resolve()
    if not dist.is_dir():
        raise SystemExit(f"目录不存在: {dist}（先跑 tools/build_web.py）")
    # `npx` on Windows is a .cmd shim, which subprocess needs shell=False to find
    # by full path; on POSIX it is a plain executable.
    npx = "npx.cmd" if os.name == "nt" else "npx"
    cmd = [npx, "--yes", "wrangler@4", "pages", "deploy", str(dist),
           "--project-name", name, "--branch", "main",
           "--commit-dirty=true", "--commit-message", message]
    merged = dict(os.environ)
    merged.update(env)
    proc = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8",
                          errors="replace", env=merged, timeout=900)
    return proc.returncode == 0, (proc.stdout or "") + (proc.stderr or "")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", help="Pages 项目名（留空配合 --random-name）")
    parser.add_argument("--random-name", action="store_true",
                        help="随机取一个空闲项目名")
    parser.add_argument("--dist", default=str(ROOT / "dist"))
    parser.add_argument("--creds", default=str(DEFAULT_CREDS))
    parser.add_argument("--message", default="mihomo-test dashboard")
    args = parser.parse_args()

    account, headers, env = load_credentials(Path(args.creds))
    print(f"账号 {account[:8]}…  凭据 {'Global Key' if 'CLOUDFLARE_API_KEY' in env else 'scoped token'}")

    name = args.name
    if args.random_name or not name:
        name = pick_free_name(account, headers)
        print(f"随机项目名: {name}")

    project = ensure_project(account, headers, name)
    print(f"部署 {args.dist} …")
    ok, output = deploy(args.dist, name, env, args.message)
    tail = "\n".join(output.strip().splitlines()[-25:])
    print(tail)
    if not ok:
        return 1

    match = re.search(r"https://[a-z0-9.-]+\.pages\.dev", output)
    print("=" * 60)
    print("部署成功")
    print("  项目名 :", name)
    print("  子域   :", project.get("subdomain"))
    if match:
        print("  本次 URL:", match.group(0))
    print("  跨域白名单里要填的前端来源: https://" + str(project.get("subdomain") or f"{name}.pages.dev"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
