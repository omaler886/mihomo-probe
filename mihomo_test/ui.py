"""Static front-end shell for the dashboard.

The dashboard used to be one 1200-line Python string with the CSS and JS inlined
and the admin token interpolated into the page. That made the UI impossible to
deploy anywhere except this process, and it forced `script-src 'unsafe-inline'`
into the CSP because everything -- including a generated `onclick=` -- lived
inline.

Now the front end is real static files under `web/`, and this module is only a
shell: it reads `index.html` and substitutes one JSON bootstrap block.

    web/index.html   shell, `__BOOTSTRAP__` placeholder, no inline script
    web/app.css      styles (light/dark via `data-theme` on <html>)
    web/app.js       application logic
    web/theme.js     pre-paint theme resolution (must stay synchronous)

Serving the same files from the CDN build is the whole point of the split: the
only thing that differs is the bootstrap payload (an absolute `apiBase`, and no
token -- a static file on a public CDN must never carry a credential).

Two contracts to preserve when editing `index.html`:

* The bootstrap is a `<script type="application/json">` block, **not** an
  executable inline script. Browsers treat a non-JS `type` as data, so it is
  exempt from `script-src`, which lets the CSP stay at `script-src 'self'`.
* Everything the front end needs from the server arrives in that one block, so
  the HTML itself is byte-identical between the same-origin and CDN builds.
"""
import json
from pathlib import Path

WEB_DIR = Path(__file__).resolve().parent / "web"

INDEX = "index.html"

# Only these are served. A generic static handler over `web/` would also expose
# `_headers` and anything a future build drops in there.
ASSET_TYPES = {
    "app.css": "text/css; charset=utf-8",
    "app.js": "application/javascript; charset=utf-8",
    "theme.js": "application/javascript; charset=utf-8",
}

# `no-cache` (revalidate) rather than `no-store`: these files are not secret,
# and re-reading a few KB on each load is cheaper than shipping a stale UI after
# a deploy. The CDN build sets its own, longer, cache headers.
ASSET_CACHE = "no-cache"


def asset_text(name):
    """Raw text of a static asset. Raises OSError when it is missing."""
    return (WEB_DIR / name).read_text(encoding="utf-8")


def asset_bytes(name):
    return (WEB_DIR / name).read_bytes()


def index_template():
    """`index.html` with the `__BOOTSTRAP__` placeholder still in place."""
    return asset_text(INDEX)


def bootstrap_json(payload):
    """Serialise the bootstrap block for a `<script type="application/json">`.

    Two escapes matter, and both are about not being able to terminate the
    surrounding element:

    * `<` is written as `\\u003c` unconditionally. A `<` can only ever occur
      inside a JSON string, and leaving one raw would let a hostile value end
      the script element with `</script>` -- or open the HTML tokenizer's
      "script data escaped" state with `<!--`, after which the real `</script>`
      no longer closes the block. Escaping `<` removes both at once.
    * U+2028/U+2029 are escaped too. They are legal inside a JSON string and
      `JSON.parse` accepts them, but they are line terminators to a *JavaScript*
      parser -- so escaping them costs nothing and means this payload stays
      valid if it is ever read as a JS literal instead of as data.

    `ensure_ascii=False` keeps Chinese titles readable in the served page (and
    keeps the page's own assertions about the title meaningful); the two escapes
    above are what actually make it safe, not the ASCII folding.
    """
    raw = json.dumps(payload, ensure_ascii=False, sort_keys=True)
    return (raw.replace("<", "\\u003c")
               .replace("\u2028", "\\u2028")
               .replace("\u2029", "\\u2029"))


def render(title, token, api_base=""):
    """The dashboard shell with its bootstrap block filled in.

    `api_base` is empty for the same-origin deployment (the front end then calls
    `/api/...` on whatever host served it) and an absolute origin for a front
    end hosted elsewhere -- which is what the CDN build bakes in.
    """
    payload = {
        "title": str(title),
        "token": str(token),
        "apiBase": str(api_base or ""),
    }
    return index_template().replace("__BOOTSTRAP__", bootstrap_json(payload))
