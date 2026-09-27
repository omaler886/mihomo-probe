"""Build the static front end for a CDN deployment.

The front end lives in `mihomo_test/web/` and the backend serves those files
verbatim for a same-origin deployment. A CDN deployment needs exactly one thing
different: the bootstrap block, which must point the page at the backend's
origin instead of at whatever host served it.

That is the whole build. There is no bundler, no transpiler and no dependency:
the sources are the artifacts, so `dist/` is byte-identical to `web/` except for
`index.html` (the bootstrap) and the added `_headers`.

    python tools/build_web.py --api-base https://probe.example --out dist

Two things this must never do:

* Bake a token into the output. `dist/` is served from a public bucket; a token
  there is a published credential. The bootstrap carries `null` and the page
  reads the token from the URL or from localStorage instead.
* Copy anything it was not asked to. `web/` may hold build inputs (`_headers`)
  or editor leftovers; an explicit allowlist keeps them out of the upload.
"""
import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from mihomo_test import ui  # noqa: E402

# What gets published. `index.html` is rendered, the rest are copied verbatim.
COPY_ASSETS = ("app.css", "app.js", "theme.js")

DEFAULT_TITLE = "mihomo 测活中心"
DEFAULT_API_BASE = "https://probe.example.com"

# A base carrying a query, a fragment or the word "token" is either a mistake or
# an attempt to point the page -- and therefore the admin credential -- somewhere
# else. Escaping already stops it from injecting code; this stops it from being
# a *valid* but wrong destination.
SUSPICIOUS_BASE = re.compile(r"[?#]|token", re.I)

# The shape `config.load()` mints for both tokens. A 32-hex run in a published
# bundle is not proof of a leak, but it is never expected either, and the whole
# point of this check is to fail loudly rather than ship a credential.
TOKEN_SHAPE = re.compile(r"\b[0-9a-f]{32}\b")


def validate_api_base(api_base):
    """Return a normalised api base, or raise on a suspicious one."""
    text = str(api_base or "").strip()
    if text and not text.startswith("https://"):
        raise ValueError(f"api-base 必须以 https:// 开头（收到 {text!r}）")
    if SUSPICIOUS_BASE.search(text):
        raise ValueError(f"api-base 含可疑字符（? / # / token）：{text!r}")
    return text.rstrip("/")


def validate_output(files):
    """Nothing in the bundle may carry a credential. Returns a list of problems.

    Two checks, because a token has two ways to leak: as the JSON field the page
    reads, and as the raw string wherever it was pasted. The first is the one
    that matters -- a page that *reads* a token out of a public file works
    perfectly, silently, which is exactly why nobody would notice.
    """
    problems = []
    page = files["index.html"].decode("utf-8")
    if '"token": null' not in page:
        problems.append('index.html 的 bootstrap 里没有 "token": null')
    for name, blob in sorted(files.items()):
        hit = TOKEN_SHAPE.search(blob.decode("utf-8", "replace"))
        if hit:
            problems.append(
                f"{name} 里出现 32 位十六进制串（形如令牌）: {hit.group(0)[:8]}…")
    return problems


def headers_file(api_base):
    """Cloudflare Pages `_headers` for the published site.

    The CSP matters more here than on the same-origin page: the token lives in
    this origin's localStorage, so a script injected into this page could read
    it out and post it anywhere. `connect-src` therefore names the one backend
    origin rather than `*`.

    Everything is `no-cache` (revalidate) rather than a long `max-age`. The
    asset filenames carry no content hash, so a long cache means a fresh
    `index.html` can be paired with a stale `app.js` -- and the page's whole
    contract is that `app.js` reads its configuration out of `index.html`'s
    bootstrap block. Version skew between the two breaks the panel silently.
    Correctness first; the edge still answers the revalidation.
    """
    origin = api_base.rstrip("/")
    csp = (
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; "
        f"img-src 'self' data:; connect-src 'self' {origin}; "
        "base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    )
    lines = [
        "/*",
        "  X-Content-Type-Options: nosniff",
        "  X-Frame-Options: DENY",
        "  Referrer-Policy: no-referrer",
        f"  Content-Security-Policy: {csp}",
        "  Cache-Control: no-cache",
        "",
    ]
    return "\n".join(lines)


def build(api_base, out_dir, title=DEFAULT_TITLE):
    """Write the publishable tree into `out_dir`.

    Two things it deliberately does not do.

    It does not wipe the directory first: a recursive delete of a build output
    is indistinguishable from a recursive delete of anything else to a
    filesystem guard, and on this machine it is refused outright. It overwrites
    the files it owns and unlinks leftovers one at a time instead.

    It does not write anything before validating. The first version wrote
    `dist/` and *then* checked for a baked-in token -- so a rejected build left
    `dist/index.html` on disk, and a deploy run afterwards would have published
    exactly the file the check existed to stop.
    """
    api_base = validate_api_base(api_base)
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)

    # Rendered index.html: same escaping path as the served page, so the two
    # cannot drift. `token=None` is the point -- see the module docstring.
    payload = {"title": title, "token": None, "apiBase": api_base}
    html = ui.index_template().replace("__BOOTSTRAP__", ui.bootstrap_json(payload))

    files = {"index.html": html.encode("utf-8")}
    for name in COPY_ASSETS:
        files[name] = (ui.WEB_DIR / name).read_bytes()
    files["_headers"] = headers_file(api_base).encode("utf-8")

    problems = validate_output(files)
    if problems:
        # Remove the previous build's page too: leaving it there is how a
        # "failed" build gets deployed by the next command in the chain.
        stale = out / "index.html"
        if stale.exists():
            stale.unlink()
        raise SystemExit("拒绝构建（产物可能带凭据）:\n  " + "\n  ".join(problems))

    for name, blob in files.items():
        (out / name).write_bytes(blob)

    # A manifest with hashes, so a deploy can be checked after the fact without
    # trusting that the upload sent what was built.
    manifest = {}
    for name, blob in sorted(files.items()):
        manifest[name] = {"bytes": len(blob),
                          "sha256": hashlib.sha256(blob).hexdigest()}
    (out / "manifest.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8")

    # Leftovers from an older build would be uploaded and published. Only files
    # directly in `out` are considered -- never a recursive walk.
    keep = set(files) | {"manifest.json"}
    dropped = []
    for entry in out.iterdir():
        if entry.is_file() and entry.name not in keep:
            entry.unlink()
            dropped.append(entry.name)
    if dropped:
        print("清理旧产物:", ", ".join(sorted(dropped)))

    return out, manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--api-base", default=DEFAULT_API_BASE,
                        help="后端 origin，写进 bootstrap 的 apiBase")
    parser.add_argument("--out", default=str(ROOT / "dist"))
    parser.add_argument("--title", default=DEFAULT_TITLE)
    args = parser.parse_args()

    try:
        out, manifest = build(args.api_base, args.out, args.title)
    except ValueError as exc:
        print(f"!! {exc}")
        return 2

    print(f"构建完成: {out}")
    print(f"后端 origin: {args.api_base}")
    total = 0
    for name in sorted(manifest):
        info = manifest[name]
        total += info["bytes"]
        print(f"  {name:14s} {info['bytes']:>7d} B  {info['sha256'][:16]}…")
    print(f"  {'合计':14s} {total:>7d} B")
    print("已校验：bootstrap 里 token 为 null，产物中无 32 位十六进制串")
    return 0


if __name__ == "__main__":
    sys.exit(main())
