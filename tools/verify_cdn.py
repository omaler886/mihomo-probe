"""Verify a deployed CDN front end, from outside Cloudflare.

Checks the four things that silently break a static deploy: the page is served,
the assets are served with the right content type, the security headers from
`_headers` actually landed, and the bootstrap block names the backend.

    python tools/verify_cdn.py https://cobalt-anchor.pages.dev --api-base https://probe.example

Three traps this handles, all of which produce a *false* failure if ignored
(they are documented in the `cf-pages-upstream-sync` skill):

* A local `HTTPS_PROXY` makes urllib fetch through it and return a short,
  `CF-Ray`-less body that reads like the site is down. Empty proxy map.
* Claiming `Accept-Encoding: br` without a brotli decoder yields binary garbage
  that looks like corruption. Only gzip/identity are requested.
* `dict(headers)` is case-sensitive, so `"CF-Ray" in headers` misses.
"""
import argparse
import gzip
import json
import re
import sys
import urllib.error
import urllib.request

OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))

UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 " \
     "(KHTML, like Gecko) Chrome/124.0 Safari/537.36"

ASSETS = {
    "/": ("text/html", True),
    "/app.css": ("text/css", False),
    "/app.js": ("javascript", False),
    "/theme.js": ("javascript", False),
}


def fetch(url):
    req = urllib.request.Request(url, headers={
        "User-Agent": UA,
        "Accept-Encoding": "gzip, identity",
        "Cache-Control": "no-cache",
    })
    try:
        with OPENER.open(req, timeout=45) as resp:
            raw = resp.read()
            if (resp.headers.get("Content-Encoding") or "").lower() == "gzip":
                raw = gzip.decompress(raw)
            headers = {k.lower(): v for k, v in resp.headers.items()}
            return resp.status, raw, headers
    except urllib.error.HTTPError as exc:
        headers = {k.lower(): v for k, v in exc.headers.items()}
        return exc.code, exc.read(), headers
    except Exception as exc:
        return None, f"{type(exc).__name__}: {exc}".encode(), {}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("--api-base", default="")
    args = parser.parse_args()
    base = args.base.rstrip("/")

    failures = []

    def check(name, ok, detail=""):
        print(("  PASS  " if ok else "  FAIL  ") + name + (f"  {detail}" if detail else ""))
        if not ok:
            failures.append(name)

    page = ""
    for path, (ctype, is_html) in ASSETS.items():
        status, raw, headers = fetch(base + path)
        body = raw.decode("utf-8", "replace")
        label = path if path != "/" else "/ (index.html)"
        check(f"{label} 200", status == 200, f"HTTP {status}")
        check(f"{label} content-type", ctype in (headers.get("content-type") or ""),
              headers.get("content-type", "—"))
        check(f"{label} 经过 Cloudflare 边缘",
              bool(headers.get("cf-ray")), headers.get("cf-ray", "无 CF-Ray"))
        if is_html:
            page = body
        else:
            check(f"{label} 非空", len(raw) > 200, f"{len(raw)} B")

    # `_headers` is applied by the edge, not baked into the file, so this is the
    # only way to know it took effect.
    _, _, page_headers = fetch(base + "/")
    csp = page_headers.get("content-security-policy", "")
    check("CSP 已生效（来自 _headers）", bool(csp), csp[:80] or "无")
    check("CSP 禁止内联脚本", "script-src 'self'" in csp and
          "script-src 'unsafe-inline'" not in csp, csp[:120])
    check("CSP frame-ancestors 'none'", "frame-ancestors 'none'" in csp)
    check("X-Content-Type-Options: nosniff",
          page_headers.get("x-content-type-options") == "nosniff")
    if args.api_base:
        check("CSP connect-src 指向后端",
              args.api_base.rstrip("/") in csp, csp[:160])

    match = re.search(
        r'<script type="application/json" id="bootstrap">(.*?)</script>',
        page, re.S)
    check("bootstrap 块存在", match is not None)
    if match:
        try:
            boot = json.loads(match.group(1))
        except ValueError as exc:
            boot = None
            check("bootstrap 是合法 JSON", False, str(exc))
        if boot is not None:
            check("bootstrap 不含令牌（CDN 上不得有凭据）",
                  boot.get("token") is None, repr(boot.get("token"))[:40])
            if args.api_base:
                check("bootstrap apiBase 指向后端",
                      boot.get("apiBase") == args.api_base.rstrip("/"),
                      repr(boot.get("apiBase")))
            check("bootstrap 带标题", bool(boot.get("title")), repr(boot.get("title")))

    check("页面引用三个静态资源",
          all(f'"{n}"' in page for n in ("app.css", "app.js", "theme.js")))
    check("页面没有内联脚本", "<script>" not in page and "onclick=" not in page)

    print("=" * 62)
    if failures:
        print(f"FAILED ({len(failures)}): " + ", ".join(failures))
        return 1
    print("CDN 前端验证通过 ✓")
    return 0


if __name__ == "__main__":
    sys.exit(main())
