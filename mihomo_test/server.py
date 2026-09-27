"""HTTP API + dashboard, plus the interval scheduler."""
import calendar
import hmac
import json
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from . import config as cfgmod
from . import db
from . import engine
from . import notifier
from . import ui
from .store import Client, StoreError

# Serialises rounds. A Lock rather than an Event: `Event.is_set()` followed by
# `set()` is two steps, and two concurrent POST /api/run could both pass the
# check. The loser then raised Busy out of `engine.run_round` and its
# `finally: BUSY.clear()` released the winner's hold -- after which the panel
# reported "not running", the scheduler started a fresh thread every poll
# (each one logging "已有一轮在运行"), and a manual run answered
# `{"started": true}` while doing nothing at all.
BUSY = threading.Lock()
_next_run = {"at": None}

# Headers applied to every response. The dashboard carries the admin token, so
# an XSS here is not "defaced UI", it is credential theft -- and the token also
# drives the host docker socket. `frame-ancestors 'none'` is the CSP equivalent
# of X-Frame-Options and is what modern browsers honour.
#
# `script-src 'self'` with no 'unsafe-inline' is possible now that the front end
# lives in `web/*.js` and the bootstrap block is a `<script type="application/
# json">` (data, not code, so it is exempt from script-src). It was
# 'unsafe-inline' while the whole dashboard was one inline <script> with inline
# `onclick=` handlers; keeping it would have left the CDN build -- where the
# token sits in localStorage -- one injected attribute away from leaking it.
#
# `style-src` keeps 'unsafe-inline' on purpose: a `style="..."` attribute cannot
# execute, and the panels set widths/colours inline (the stacked ratio bar, the
# per-category accent dots). Tightening it would mean generating a class per
# value for no security gain.
SECURITY_HEADERS = {
    "X-Content-Type-Options": "nosniff",
    "X-Frame-Options": "DENY",
    "Referrer-Policy": "no-referrer",
    "Content-Security-Policy": (
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; "
        "img-src 'self' data:; connect-src 'self'; base-uri 'none'; "
        "form-action 'none'; frame-ancestors 'none'"
    ),
}

# What a cross-origin front end is allowed to send. The token travels in
# `X-Auth-Token` rather than a cookie, so no `Allow-Credentials` is needed --
# and omitting it means a browser will not attach cookies to a request that a
# hostile page managed to make.
CORS_ALLOW_HEADERS = "X-Auth-Token, Content-Type, Authorization"
CORS_ALLOW_METHODS = "GET, POST, OPTIONS"


def cors_origin(cfg, handler):
    """The Origin to echo back, or '' when this request is not an allowed one.

    Echoing the specific origin rather than `*` is deliberate: `*` cannot be
    combined with credentials, and echoing keeps the allowlist meaningful --
    only a front end the operator named in `server.cors_origins` gets to read
    the response at all. A request with no `Origin` (curl, Sub-Store, a
    same-origin fetch) is not a cross-origin request and needs no header.

    The **allowlist entry** is returned, not the request's own value. The two
    are equal by construction, so the header is identical -- but it means the
    bytes we echo come from a source we control rather than from the caller,
    which keeps "the allowlist decides what may be echoed" true by inspection
    instead of by reasoning about string equality.
    """
    origin = (handler.headers.get("Origin") or "").strip()
    if not origin:
        return ""
    wanted = origin.rstrip("/")
    for item in ((cfg or {}).get("server", {}).get("cors_origins") or []):
        entry = str(item).strip().rstrip("/")
        if entry and entry == wanted:
            return entry
    return ""



class _State:
    cfg = None


def _matches(candidate, expected):
    """Constant-time token comparison; False unless both are non-empty strings.

    Compared as UTF-8 bytes rather than str: `hmac.compare_digest` raises
    TypeError on a str containing non-ASCII, so a hand-set token with a Chinese
    character in it would have turned every request into a 500 instead of
    authenticating.
    """
    if not isinstance(candidate, str) or not isinstance(expected, str):
        return False
    if not candidate or not expected:
        return False
    return hmac.compare_digest(candidate.encode("utf-8"), expected.encode("utf-8"))


def _presented_tokens(handler):
    query = urllib.parse.parse_qs(urllib.parse.urlparse(handler.path).query)
    out = [query.get("token", [""])[0], handler.headers.get("X-Auth-Token") or ""]
    auth = handler.headers.get("Authorization") or ""
    if auth.startswith("Bearer "):
        out.append(auth[len("Bearer "):])
    return out


def auth_ok(handler, cfg, path=None):
    """True when the request carries a token this endpoint accepts.

    An empty admin token denies rather than allows. It used to allow, on the
    theory that no token configured means auth is off -- but "no token
    configured" is also what an unreadable config.json produced, so a full disk
    silently published the dashboard, every node's credentials via
    /api/export/*.yaml, and POST /api/config. `config.load()` now mints a token
    on every path, and this makes the failure loud if it ever does not.

    The export endpoints additionally accept the read-only `publish.token`:
    those URLs are handed to Sub-Store and rendered into the page, so they must
    not carry the admin credential.
    """
    accepted = [str(cfg.get("auth", {}).get("token") or "")]
    if path and path.startswith("/api/export/"):
        accepted.append(str(cfg.get("publish", {}).get("token") or ""))
    presented = _presented_tokens(handler)
    return any(_matches(c, t) for t in accepted for c in presented)


def store_client(cfg):
    """The Sub-Store client for the configured backend.

    Every call site read `cfg["substore"]["backend"]` with a direct subscript,
    so a config without the `substore` key -- which `config.load()` never
    produces, but a hand-built config.json or a trimmed test fixture can --
    turned every sources save, the resources dropdown and the sync button into
    a 500 (KeyError) instead of a store error. Falling back to the config
    default keeps that shape working; a genuinely unreachable backend still
    fails later, inside the StoreError handling the callers already have.
    """
    return Client((cfg.get("substore") or {}).get("backend")
                  or cfgmod.DEFAULTS["substore"]["backend"])


def exports_summary(cfg):
    """Only sources that actually own an export file belong in this list.

    That means enabled *and* not muted (`export: false`). A disabled source
    keeps its ledger for cheap re-enabling, but its export file is removed, and
    a muted one never had one -- listing either would advertise a URL that
    answers 404 or serves a snapshot nothing maintains.
    """
    out = []
    for source in cfg["sources"]:
        if not source.get("enabled", True) or not source.get("export", True):
            continue
        key = source["key"]
        meta = engine.export_meta(key) or {}
        out.append({"key": key, "url": engine.export_url(cfg, key),
                    "count": meta.get("count", 0)})
    return out


def redacted_config(cfg):
    """A copy of cfg with the credentials masked.

    `/api/status` is polled by the dashboard every 5 seconds and its payload is
    one screenshot or one "look at this" forward away from being public. The
    admin token is the credential that also reaches the host docker socket, so
    it does not belong in it -- and the dashboard does not need it here, because
    `ui.render` already embeds the token it was loaded with.

    `publish.token` is masked for a narrower reason: it is not the same class of
    secret (it only reads `/api/export/*`, and it is handed to Sub-Store on
    purpose), but the settings form does **not** read it back, so exposing it in
    a five-second poll buys nothing. Everything else is left in place because
    the form *does* read those back for editing; masking them would silently
    overwrite them with the mask on save.
    """
    out = json.loads(json.dumps(cfg, default=str))
    for section, key in (("auth", "token"), ("publish", "token")):
        block = out.get(section)
        if isinstance(block, dict) and block.get(key):
            block[key] = "***"
    return out


def status_payload(cfg):
    busy = BUSY.locked()
    return {
        "stats": db.stats(),
        "last_round": db.last_round(),
        "busy": busy,
        # Which kind of round is in flight, so the dashboard's header can say
        # 直连测活 / 链式测活 instead of a bare "正在测试". None while idle, and
        # also None for a scheduler round, which is a full chain-aware round
        # and is reported as such in `mode_label`.
        "busy_mode": engine.current_mode() if busy else None,
        "next_run": _next_run["at"],
        "exports": exports_summary(cfg),
        "config": redacted_config(cfg),
    }


def run_in_background(cfg, trigger="manual", only_source=None, mode=None):
    """Start a round on a worker thread; raise if one is already running.

    `mode` is forwarded to `engine.run_round` so the dashboard's 直连测活 /
    链式测活 buttons can scope a manual round without touching the scheduler,
    which always runs a complete round (mode=None).
    """
    if not BUSY.acquire(blocking=False):
        raise engine.Busy("a round is already running")

    def work():
        try:
            engine.run_round(cfg, trigger=trigger, only_source=only_source,
                             mode=mode)
        except engine.Busy:
            # Normal when the scheduler fires while a round is still running.
            # Logged rather than swallowed because a *permanently* held lock
            # looks exactly like this from here, and silence is what let that
            # failure mode go unnoticed.
            db.log("info", "已有一轮在运行，跳过本次调度")
        except Exception as exc:  # keep the service alive across round failures
            db.log("error", f"本轮异常: {type(exc).__name__}: {exc}")
        finally:
            # Released only by the caller that acquired it, because acquisition
            # is what raised for everyone else.
            BUSY.release()

    thread = threading.Thread(target=work, name="round", daemon=True)
    thread.start()
    return thread


class Handler(BaseHTTPRequestHandler):
    server_version = "mihomo-test"
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        pass

    # -- helpers ---------------------------------------------------------
    def _send(self, code, body, ctype="application/json; charset=utf-8",
              cache="no-store"):
        if isinstance(body, (dict, list)):
            body = json.dumps(body, ensure_ascii=False, default=str)
        raw = body.encode("utf-8") if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Cache-Control", cache)
        # Unconditionally, not only when echoing an origin: the response body
        # *does* depend on the Origin header, so a shared cache that ignored
        # `Vary` could hand one origin's answer to another.
        self.send_header("Vary", "Origin")
        origin = cors_origin(_State.cfg, self)
        if origin:
            self.send_header("Access-Control-Allow-Origin", origin)
        for name, value in SECURITY_HEADERS.items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)

    def _json_body(self):
        length = int(self.headers.get("Content-Length") or 0)
        if not length:
            return {}
        try:
            return json.loads(self.rfile.read(length).decode("utf-8"))
        except ValueError:
            return {}

    def _path(self):
        parsed = urllib.parse.urlparse(self.path)
        return urllib.parse.unquote(parsed.path)

    # -- routing ---------------------------------------------------------
    def do_OPTIONS(self):
        """CORS preflight. Deliberately unauthenticated.

        A preflight is sent by the browser before the real request and carries
        no custom headers, so it *cannot* present a token. Requiring one here
        would make every cross-origin call fail before it was ever made -- and
        the failure looks like "the API is down", not like an auth problem.
        """
        self.send_response(204)
        self.send_header("Content-Length", "0")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Vary", "Origin")
        origin = cors_origin(_State.cfg, self)
        if origin:
            self.send_header("Access-Control-Allow-Origin", origin)
            self.send_header("Access-Control-Allow-Methods", CORS_ALLOW_METHODS)
            self.send_header("Access-Control-Allow-Headers", CORS_ALLOW_HEADERS)
            self.send_header("Access-Control-Max-Age", "600")
        for name, value in SECURITY_HEADERS.items():
            self.send_header(name, value)
        self.end_headers()

    def do_GET(self):
        cfg = _State.cfg
        path = self._path()
        try:
            if path in ("/healthz", "/api/health"):
                return self._send(200, {"ok": True})
            # Static front-end assets, served before the token check.
            #
            # This is not a hole: these files are the same ones a public CDN
            # serves, they hold no secret, and the browser fetches `<link>` and
            # `<script src>` without any custom header -- gating them would mean
            # the panel never renders its styles or its script. The *shell*
            # (`/`, below) still requires the token, because that is what
            # carries it.
            asset = path.lstrip("/")
            if asset in ui.ASSET_TYPES:
                return self._send(200, ui.asset_bytes(asset), ui.ASSET_TYPES[asset],
                                  cache=ui.ASSET_CACHE)
            if not auth_ok(self, cfg, path):
                return self._send(401, {"error": "unauthorized",
                                        "hint": "append ?token=<your token>"})
            if path in ("/", "/ui"):
                return self._send(200, ui.render(cfg["ui"]["title"], cfg["auth"]["token"]),
                                  "text/html; charset=utf-8")
            if path == "/api/status":
                return self._send(200, status_payload(cfg))
            if path == "/api/stats":
                # Per-category breakdown. `round_id` is optional and defaults to
                # the newest round; the dashboard passes it explicitly once the
                # operator picks an older round from the 轮次 list, so this
                # endpoint stays a pure reader with no hidden "current" state.
                query = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
                raw_round = (query.get("round_id", [""])[0] or "").strip()
                round_id = None
                if raw_round:
                    try:
                        round_id = int(raw_round)
                    except ValueError:
                        # A bogus round_id should not 500; falling back to the
                        # newest round is what the caller asked for anyway when
                        # the parameter is absent, and it keeps the panel usable
                        # while somebody is poking at the URL bar.
                        round_id = None
                return self._send(200, db.stats_by_category(round_id))
            if path == "/api/nodes":
                nodes = db.list_nodes()
                # One query for every source, then a single pass over the nodes.
                # The previous shape called `recent_trends` once per source and
                # rescanned the whole node list for each of those calls, on a
                # response the dashboard fetches every 5 seconds.
                trends = db.recent_trends_all()
                for node in nodes:
                    node["trend"] = trends.get(node["source"], {}).get(node["fingerprint"], [])
                return self._send(200, {"nodes": nodes})
            if path == "/api/logs":
                return self._send(200, {"events": db.recent_events(200)})
            if path == "/api/rounds":
                return self._send(200, {"rounds": db.last_rounds(30)})
            if path == "/api/substore-resources":
                client = store_client(cfg)
                try:
                    available, errors = client.list_resources()
                except StoreError as exc:
                    return self._send(502, {"error": f"读取 Sub-Store 失败: {exc}",
                                            "available": [], "configured": cfg["sources"]})
                return self._send(200, {"available": available, "errors": errors,
                                        "configured": cfg["sources"]})
            if path == "/api/substore-nodes":
                # The node names of one resource, for the front-pool picker.
                # Fetched on demand rather than shipped alongside
                # /api/substore-resources: a collection can render hundreds of
                # nodes, and the panel only needs them while the picker is open.
                query = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
                kind = (query.get("kind", ["sub"])[0] or "").strip()
                name = (query.get("name", [""])[0] or "").strip()
                if kind not in cfgmod.SOURCE_KINDS:
                    kind = "sub"
                if not name:
                    return self._send(400, {"error": "缺少 name 参数", "nodes": []})
                try:
                    proxies = store_client(cfg).fetch_source(kind, name)
                except StoreError as exc:
                    return self._send(502, {"error": f"读取 {kind}/{name} 失败: {exc}",
                                            "nodes": []})
                nodes = [{"name": str(p.get("name") or ""),
                          "type": str(p.get("type") or ""),
                          "server": str(p.get("server") or "")}
                         for p in proxies]
                return self._send(200, {"nodes": nodes, "kind": kind, "name": name})
            if path.startswith("/api/export/"):
                name = path[len("/api/export/"):]
                key = name[:-5] if name.endswith(".yaml") else name
                content = engine.read_export(key)
                if content is None:
                    return self._send(404, f"# no export for {key} yet\n", "text/yaml; charset=utf-8")
                return self._send(200, content, "text/yaml; charset=utf-8")
            return self._send(404, {"error": "not found", "path": path})
        except Exception as exc:
            return self._send(500, {"error": f"{type(exc).__name__}: {exc}"})

    def do_POST(self):
        cfg = _State.cfg
        path = self._path()
        try:
            if not auth_ok(self, cfg, path):
                return self._send(401, {"error": "unauthorized"})
            body = self._json_body()
            if path == "/api/run":
                # Whitelist, the same two values argparse allows for `--mode`.
                # The engine only ever *compares* `mode` against "direct" and
                # "chain", so anything else is stored verbatim in `rounds.mode`
                # and then quietly runs a full round -- a body saying
                # {"mode": "Direct"} would report itself as a direct round while
                # dialing chains, and the header would fall back to a label that
                # reads "测试". Refuse it at the edge instead.
                mode = body.get("mode")
                if mode is not None and mode not in ("direct", "chain"):
                    return self._send(400, {"error":
                                            f"mode 必须是 direct 或 chain，收到 {mode!r}"})
                if mode == "chain" and engine.chain_block(cfg) is None:
                    # `chain_block` is the exact gate the round itself applies
                    # (enabled AND a front pool named, by resource or by pasted
                    # text), so refusing here cannot diverge from what the round
                    # would have done: a round that is byte-for-byte a 直连测活.
                    # The 链式测活 button used to start that round and only
                    # explain itself in a log line -- three minutes and a full
                    # dial of every node after the click. Fail at the edge with
                    # the fix instead.
                    why = ("chain.enabled 为 false" if not cfg.get("chain", {}).get("enabled")
                           else "前置来源为空，也没有粘贴手动前置")
                    return self._send(400, {"error":
                        f"链式未生效（{why}），这轮不会比直连测多测任何东西；"
                        "请先在「设置」里开启链式，并选一个前置来源或粘贴前置节点"})
                source = body.get("source")
                if source is not None and not isinstance(source, str):
                    return self._send(400, {"error": "source 必须是来源 key"})
                try:
                    run_in_background(cfg, trigger=body.get("trigger", "manual"),
                                      only_source=source,
                                      mode=mode)
                except engine.Busy:
                    return self._send(409, {"error": "a round is already running"})
                return self._send(200, {"started": True})
            if path == "/api/push":
                if not cfg["publish"].get("enabled", True):
                    # Exports are written only while publishing is on, so the
                    # files this push would read are the snapshot from whenever
                    # it was last enabled. Upserting them would hand Sub-Store
                    # subscriptions that serve nodes no round maintains any
                    # more -- and `push_exports` reports success for that,
                    # which reads as a working integration built on stale data.
                    return self._send(400, {"error":
                        "publish.enabled 为 false，导出已停用，没有可推送的内容"
                        "（推送只会写入停用前留下的旧文件）。先开启发布再推送。"})
                client = store_client(cfg)
                # Enabled sources only: a disabled one is not published, so
                # pushing it -- or reporting it as "no export yet" -- is wrong
                # on both counts. See `engine.publish_keys`.
                result = engine.push_exports(cfg, client, engine.publish_keys(cfg))
                return self._send(200, {"message": engine.push_summary(result),
                                        "detail": result})
            if path == "/api/alert-test":
                return self._send(200, {"message": "；".join(notifier.test_channels(cfg))})
            if path == "/api/link":
                client = store_client(cfg)
                messages = engine.link_substore(cfg, client)
                return self._send(200, {"message": "；".join(messages), "detail": messages})
            if path == "/api/config":
                try:
                    # Validate before merging: `update()` on its own would accept
                    # any key at all, including ones no code reads.
                    clean, notes = cfgmod.validate_patch(body)
                    new_cfg = cfgmod.update(clean)
                except ValueError as exc:
                    return self._send(400, {"error": f"配置无效: {exc}"})
                _State.cfg = new_cfg
                db.log("info", "配置已更新")
                for note in notes:
                    db.log("warn", f"配置写入已调整: {note}")
                # A cleared paste leaves the materialised `…-front-manual` sub
                # behind, and its content is embedded at write time -- so it
                # would go on offering fronts the operator removed, and show up
                # in the resource list as something nobody can explain. Cleaning
                # it up here rather than in the round covers the case where
                # chaining is switched off in the same save, which never reaches
                # `collect_fronts` again.
                if not cfgmod.front_text(new_cfg.get("chain") or {}):
                    try:
                        engine.drop_manual_front_sub(new_cfg, store_client(new_cfg))
                    except StoreError as exc:
                        db.log("warn", f"清理手动前置订阅失败: {exc}")
                sync = []
                if "sources" in clean and cfg["publish"].get("enabled", True):
                    # keep Sub-Store in step with the selection immediately:
                    # otherwise a removed source lingers as a dangling member
                    # until somebody remembers to press the sync button.
                    #
                    # Only when the link-relevant projection moved, though. The
                    # measurement toggles reach here as sources patches too,
                    # and re-running `link_substore` for one is a full sub
                    # listing plus one upsert per source of latency -- paid on
                    # every 直连/链式/relay checkbox click, for an outcome that
                    # is byte-identical and reported as sync noise besides.
                    if engine.link_signature(cfg) != engine.link_signature(new_cfg):
                        try:
                            sync = engine.link_substore(new_cfg,
                                                        store_client(cfg))
                        except StoreError as exc:
                            sync = [f"联动同步失败: {exc}"]
                return self._send(200, {"saved": True, "config": new_cfg,
                                        "sources": new_cfg.get("sources", []),
                                        "link": sync, "notes": notes})
            if path == "/api/reload":
                db.log("info", "配置已重新加载")
                _State.cfg = cfgmod.load()
                return self._send(200, {"reloaded": True})
            return self._send(404, {"error": "not found", "path": path})
        except Exception as exc:
            return self._send(500, {"error": f"{type(exc).__name__}: {exc}"})


def serve(cfg, host="127.0.0.1", port=8088):
    _State.cfg = cfg
    httpd = ThreadingHTTPServer((host, port), Handler)
    httpd.daemon_threads = True
    return httpd


def scheduler_loop(stop_event, poll_seconds=20):
    """Trigger rounds on the configured interval."""
    while not stop_event.is_set():
        cfg = _State.cfg
        try:
            # Cheap (one indexed SELECT) and it is what keeps the "no round is
            # left unfinished forever" invariant true between restarts -- a
            # round orphaned by the CLI, or by a restart that happened before
            # the budget elapsed, is closed here rather than never.
            engine.reap_orphan_rounds(cfg)
            schedule = cfg["schedule"]
            if not schedule.get("enabled", True):
                _next_run["at"] = None
            else:
                interval = max(1, int(schedule.get("interval_minutes", 30))) * 60
                last = db.last_round()
                last_ts = _parse(last["started_at"]) if last else 0
                due = (last_ts or time.time()) + interval
                if last_ts == 0:
                    due = time.time() + 5
                _next_run["at"] = time.strftime("%H:%M:%S", time.localtime(due))
                if time.time() >= due and not BUSY.locked():
                    run_in_background(cfg, trigger="schedule")
        except Exception as exc:
            db.log("error", f"调度器异常: {type(exc).__name__}: {exc}")
        stop_event.wait(poll_seconds)


def _parse(stamp):
    """Parse a stored timestamp as UTC; return 0 when it is unusable.

    Must match db.now(), which writes UTC. Using mktime here would read every
    stored value back shifted by the container's TZ offset, so the schedule
    would think a round that just ran was hours overdue (or hours away).
    """
    for fmt in ("%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"):
        try:
            return calendar.timegm(time.strptime(stamp, fmt))
        except (TypeError, ValueError):
            continue
    return 0
