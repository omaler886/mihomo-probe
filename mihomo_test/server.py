"""HTTP API + dashboard, plus the interval scheduler."""
import calendar
import hmac
import json
import os
import re
import threading
import time
import urllib.parse
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from . import config as cfgmod
from . import db
from . import engine
from . import gist_store
from . import notifier
from . import probe_dispatch
from . import substore_bridge
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
MAX_JSON_BODY_BYTES = 2 * 1024 * 1024
BODY_TIMEOUT_SECONDS = 15


class RequestBodyError(ValueError):
    """A rejected request body, with an HTTP status safe to return to clients."""

    def __init__(self, status, message):
        """Store a status and public message for the request handler."""
        super().__init__(message)
        self.status = status


def _reject_json_constant(_value):
    """Reject non-JSON numeric constants instead of persisting NaN/Infinity."""
    raise ValueError("non-finite JSON number")


def _publish_scoped(path):
    """Paths a publish token may read: exports (prefix) + the probe node snapshot.

    `/api/export/` is prefix-matched because the key may contain any legal
    character; `/api/probe/nodes` is matched exactly, so a trailing path such
    as `/api/probe/nodes/anything` cannot slip into the read-only scope.
    """
    return path.startswith("/api/export/") or path == "/api/probe/nodes"


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
    """Every (token, channel) the request presents; channel is query|header.

    The channel matters since query-token compat marking: a token in the URL
    ends up in access logs, browser history and referral headers, so it is
    legacy -- header forms are what new integrations should use. Which channel
    authenticated is reported by `auth_kind`.
    """
    query = urllib.parse.parse_qs(urllib.parse.urlparse(handler.path).query)
    out = []
    if query.get("token", [""])[0]:
        out.append((query["token"][0], "query"))
    if handler.headers.get("X-Auth-Token"):
        out.append((handler.headers.get("X-Auth-Token"), "header"))
    auth = handler.headers.get("Authorization") or ""
    if auth.startswith("Bearer "):
        out.append((auth[len("Bearer "):], "header"))
    return out


def auth_kind(handler, cfg, path=None):
    """(channel, scope) when the request carries an accepted token, else None.

    scope is "admin" or "publish" -- the publish scope only exists on the
    paths `_publish_scoped` names, exactly as `auth_ok` allowed before. The
    first matching (token, channel) pair wins, header order after query order
    as `_presented_tokens` lists them.
    """
    accepted = [("admin", str(cfg.get("auth", {}).get("token") or ""))]
    if path and _publish_scoped(path):
        accepted.append(("publish", str(cfg.get("publish", {}).get("token") or "")))
    for presented, channel in _presented_tokens(handler):
        for scope, token in accepted:
            if _matches(presented, token):
                return channel, scope
    return None


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
    not carry the admin credential. The probe node snapshot
    (`GET /api/probe/nodes`) shares that publish scope: it is the metadata-only
    readout Sub-Store consumes alongside the exports (ARCHITECTURE §5).
    """
    return auth_kind(handler, cfg, path) is not None


_query_token_admin_warned = False


def _note_query_token_admin(kind):
    """Once per process: a query token just granted *admin* scope.

    Query tokens are compat: they land in access logs and browser history, so
    every authenticated-by-URL admin request is a leak waiting for a log to
    happen. The publish scope keeps its query token on purpose -- export URLs
    pasted into Sub-Store are the documented integration and carry only the
    read-only credential. Warn once, not per poll: /api/status is fetched every
    five seconds and one line per poll would be noise, not signal.
    """
    global _query_token_admin_warned
    if kind != ("query", "admin") or _query_token_admin_warned:
        return
    _query_token_admin_warned = True
    db.log("warn", "检测到 query token 认证（?token=），已按兼容模式放行；"
                   "请改用 X-Auth-Token 或 Authorization: Bearer 头。"
                   "/api/export/* 等只读发布面的 query token 不受影响")


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


def _mask_backend(url):
    """Return the backend's HTTP origin with any credential-bearing parts masked."""
    try:
        parsed = urllib.parse.urlsplit(str(url or ""))
        host, port = parsed.hostname, parsed.port
    except ValueError:
        return cfgmod.SECRET_MASK
    if parsed.scheme not in ("http", "https") or not host:
        return cfgmod.SECRET_MASK
    authority = f"[{host}]" if ":" in host else host
    if port is not None:
        authority += f":{port}"
    has_secret = (parsed.username is not None or parsed.password is not None
                  or parsed.path not in ("", "/") or parsed.query or parsed.fragment)
    path = "/" + cfgmod.SECRET_MASK if has_secret else parsed.path
    return urllib.parse.urlunsplit((parsed.scheme, authority, path, "", ""))


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
    a five-second poll buys nothing.

    The alert credentials and the Sub-Store backend URL are masked for the
    reason S-12 was upgraded to P0: the Telegram bot token and the webhook URL
    are live senders, and the backend path is an auth bypass on another system.
    The form *does* read the alert fields back -- so every masked field is in
    config.MASKABLE_PATHS, and `validate_patch` drops the mask on save instead
    of writing it over the real value.
    """
    out = json.loads(json.dumps(cfg, default=str))
    for path in ("auth.token", "publish.token",
                 "alert.telegram.token", "alert.webhook.url"):
        parts = path.split(".")
        block = out
        for part in parts[:-1]:
            block = block.get(part) if isinstance(block, dict) else None
        if isinstance(block, dict) and block.get(parts[-1]):
            block[parts[-1]] = cfgmod.SECRET_MASK
    backend = out.get("substore")
    if isinstance(backend, dict) and backend.get("backend"):
        backend["backend"] = _mask_backend(backend["backend"])
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


# ------------------------------------------------------------- HK3 v3 表面
# 指导 §10 API 契约。/api/v1/* 与既有 /api/* 并存：旧面板和旧集成不动，新控
# 制台（P6）只读新面。写接口一律：鉴权 + Content-Type: application/json +
# 请求体上限（复用 _body_length）+ 审计 ID + 稳定错误码；token、Gist token、
# 节点凭据不进响应与日志（指导 §10/§13）。

V1_PREFIX = "/api/v1/"

# 探测点「在线」的心跳窗口。probes[].heartbeat 由 agent 定期发送；超过这个
# 窗口没有心跳即视为离线（Web 总览的 在线数/启用数，指导 §9.1）。180s 容忍
# 一次分钟级心跳抖动，又不会把一个半夜挂掉的探测点显示成在线。
PROBE_ONLINE_WINDOW_S = 180

# 节点页筛选用的状态白名单：与 /api/probe/nodes 的边缘拒绝同一风格，一个
# 拼错的状态返回 400 而不是一份「看起来是没有这种节点」的空 200。
V1_NODE_STATUSES = ("alive", "pending", "dead", "unknown", "excluded")

# 写接口的稳定错误码 -> HTTP 状态。前端只认 error 码，不认自然语言。
V1_ERROR_STATUS = {
    "BAD_PAYLOAD": 400,
    "UNSUPPORTED_MEDIA_TYPE": 415,
    "ROUND_MISMATCH": 400,
    "UNKNOWN_PROBE": 404,
    "ROUND_STALE": 409,
    "DIGEST_MISMATCH": 409,
    "RESULT_LATE": 409,
    "NOT_IMPLEMENTED": 501,
    "PUSH_CONFIRM_REQUIRED": 501,
}


def _probe_bearer(handler):
    """The Bearer token the request presents, or "" (指导 §10：probe token 走
    Authorization: Bearer，与 admin 的 X-Auth-Token/query 是两套通道)。"""
    auth = handler.headers.get("Authorization") or ""
    if auth.startswith("Bearer "):
        return auth[len("Bearer "):].strip()
    return ""


def probe_auth(handler, cfg, probe_id):
    """(ok, error_code) for a probe-token request.

    Two separate credential systems on purpose (指导 §13.1): the admin token
    drives the host docker socket and must never authenticate a probe route,
    and a probe token must never reach an admin route. The comparison is
    constant-time (`_matches`), and the token itself is only ever read from
    the env var `probes[].token_env` names -- never from the request, the
    config file or the response.
    """
    entry = probe_dispatch.probe_entry(cfg, probe_id)
    if entry is None:
        return False, "UNKNOWN_PROBE"
    token = probe_dispatch.probe_token(entry)
    if not token:
        # A registered probe whose env var is unset is an operator mistake,
        # not an auth failure: answering 401 would read as "wrong token".
        return False, "PROBE_TOKEN_UNSET"
    presented = _probe_bearer(handler)
    if presented and _matches(presented, token):
        return True, ""
    return False, "unauthorized"


def _v1_error(code, message=None, audit_id=None):
    body = {"error": code}
    if message:
        body["message"] = message
    if audit_id:
        body["audit_id"] = audit_id
    return V1_ERROR_STATUS.get(code, 400), body


def _parse_db_stamp(stamp):
    """A stored db timestamp as a UTC epoch; 0 when unusable.

    Shares server._parse's formats because it reads the same column family:
    db.now() writes UTC, so timegm (not mktime) is what keeps a heartbeat
    from being read back shifted by the container's TZ.
    """
    return _parse(stamp)


def _probe_view(probe_id, entry, row, now):
    """One probe for /api/v1/probes and the dashboard's counts.

    Config and ledger are merged rather than one shadowing the other: a probe
    registered in config.json but never heartbeated has no ledger row yet and
    must still appear (as offline), and a ledger row whose config entry was
    removed must still be visible to whoever is about to re-add it.
    """
    enabled = (bool(entry.get("enabled", True)) if entry is not None
               else bool((row or {}).get("enabled", 1)))
    last_seen = (row or {}).get("last_seen_at")
    online = False
    if last_seen:
        seen_at = _parse_db_stamp(str(last_seen))
        online = bool(seen_at) and (now - seen_at) <= PROBE_ONLINE_WINDOW_S
    return {
        "probe_id": probe_id,
        "name": (entry or {}).get("name") or (row or {}).get("name") or probe_id,
        "enabled": enabled,
        "online": online,
        "last_seen_at": last_seen,
        "version": (row or {}).get("version"),
        "state": (row or {}).get("state") or "unknown",
        "endpoint": (entry or {}).get("endpoint"),
        "token_env": (entry or {}).get("token_env"),
        "tags": (entry or {}).get("tags") or [],
    }


def v1_probes(cfg):
    rows = {}
    for row in db.query("SELECT * FROM probes"):
        rows[row["probe_id"]] = row
    now = time.time()
    out, seen = [], set()
    for entry in (cfg or {}).get("probes") or []:
        if not isinstance(entry, dict):
            continue
        probe_id = str(entry.get("id") or "")
        if not probe_id or probe_id in seen:
            continue
        seen.add(probe_id)
        out.append(_probe_view(probe_id, entry, rows.get(probe_id), now))
    for probe_id, row in rows.items():
        if probe_id not in seen:
            out.append(_probe_view(probe_id, None, row, now))
    out.sort(key=lambda item: item["probe_id"])
    return out


def _canonical_paths():
    """磁盘上 canonical 产物与其 meta 的位置（与 engine.EXPORT_DIR 同源）。"""
    base = cfgmod.DATA / "exports"
    return (base / f"{engine.CANONICAL_KEY}.yaml",
            base / f"{engine.CANONICAL_KEY}.meta.json")


def canonical_artifact():
    """`(content_bytes, meta_dict)` of the canonical export, or None.

    The artifact from the last stable round is the ONLY thing the manual
    publish endpoint may send (指导 §8 去重保证): no re-render, no rebuild per
    request -- the file on disk IS the subscription body every consumer gets.
    """
    yaml_path, meta_path = _canonical_paths()
    try:
        content = yaml_path.read_bytes()
    except OSError:
        return None
    try:
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        meta = {}
    if not isinstance(meta, dict):
        meta = {}
    return content, meta


def canonical_meta_digest():
    """Digest recorded beside the artifact; '' when there is none yet."""
    _yaml_path, meta_path = _canonical_paths()
    try:
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return ""
    if not isinstance(meta, dict):
        return ""
    return str(meta.get("digest") or "")


def _gist_publication_state():
    """Dashboard 的 gist 块：最近一次真实发布的时间 / revision / 一致性。

    Only `published` / `no_change` rows count as a publication timeline entry:
    rejected or failed attempts are audit events, not a publish time.
    """
    state = {"last_published_at": None, "revision": None,
             "consistent_with_stable_round": None}
    try:
        rows = db.list_publish_audit(50)
    except Exception:  # noqa: BLE001 - table may predate v3
        return state
    latest = next((r for r in rows if r["kind"] == "gist"
                   and r["status"] in ("published", "no_change")), None)
    if latest is None:
        return state
    digest = canonical_meta_digest()
    state["last_published_at"] = latest["created_at"]
    state["revision"] = latest["revision"]
    state["consistent_with_stable_round"] = bool(
        digest and str(latest["digest"] or "") == digest)
    return state


def v1_dashboard(cfg):
    """总览（指导 §9.1）：当前轮、节点五态、探测点在线数、发布状态。

    The gist block is live since P4 (fed by publish_audit); substore stays
    null-shaped until P5 lands, so the console can render both from day one.
    """
    last = db.last_round() or {}
    stats = db.stats()
    probes = v1_probes(cfg)
    enabled = [p for p in probes if p["enabled"]]
    online = [p for p in probes if p["online"]]
    if not last:
        round_state = "idle"
    elif not last.get("finished_at"):
        round_state = "running"
    elif last.get("suspect"):
        round_state = "suspect"
    elif (last.get("ok") or 0) > 0:
        round_state = "ok"
    else:
        round_state = "failed"
    return {
        "round": {
            "id": last.get("id"),
            "state": round_state,
            "started_at": last.get("started_at"),
            "finished_at": last.get("finished_at"),
            "trigger": last.get("trigger"),
            "mode": last.get("mode"),
            "total": last.get("total") or 0,
            "ok": last.get("ok") or 0,
            "failed": last.get("failed") or 0,
            "dropped": last.get("dropped") or 0,
            "restored": last.get("restored") or 0,
            "suspect": last.get("suspect") or 0,
            "duration_s": last.get("duration_s"),
        },
        "nodes": stats,
        "probes": {"total": len(probes), "enabled": len(enabled),
                   "online": len(online)},
        "busy": BUSY.locked(),
        # P4 已落地 gist（数据源 publish_audit）；substore 仍等 P5。
        "gist": _gist_publication_state(),
        "substore": {"last_pushed_at": None, "last_result": None,
                     "target": (cfg.get("substore") or {}).get("target_name")},
    }


def _v1_limit(raw, default=20, maximum=100):
    value = (raw or "").strip()
    if not value:
        return default
    try:
        number = int(value)
    except ValueError:
        return None
    if number < 1 or number > maximum:
        return None
    return number


def v1_round_detail(round_id):
    """One round: its row, the per-node results, the probe observations."""
    try:
        rid = int(str(round_id).strip())
    except (TypeError, ValueError):
        return None
    row = db.one("SELECT * FROM rounds WHERE id=?", (rid,))
    if row is None:
        return None
    results = db.query(
        "SELECT source, fingerprint, display, verdict, delay_ms, reason,"
        " country, category FROM results WHERE round_id=?"
        " ORDER BY source, fingerprint", (rid,))
    return {"round": row, "results": results,
            "observations": db.get_observations(rid)}


def v1_nodes(cfg, params):
    """节点页投影（指导 §9.2）：搜索/状态/协议/来源筛选 + 分页。

    Reuses db.list_nodes -- the same projection /api/nodes already serves, so
    the two surfaces cannot drift. That projection carries the node's address
    but no credential: UUIDs and passwords live in the proxy dicts, which no
    nodes-table read can reach (指导 §10：默认不返回敏感凭据).
    """
    def one(name, default=""):
        return (params.get(name, [default])[0] or default).strip()

    query = one("q").lower()
    status = one("status")
    proto = one("proto").lower()
    source = one("source")
    if status and status not in V1_NODE_STATUSES:
        return None, _v1_error("BAD_PAYLOAD",
                               f"status 必须是 {'/'.join(V1_NODE_STATUSES)} 之一")
    try:
        page = max(1, int(one("page", "1") or 1))
        page_size = min(200, max(1, int(one("page_size", "50") or 50)))
    except ValueError:
        return None, _v1_error("BAD_PAYLOAD", "page/page_size 必须是整数")
    rows = db.list_nodes(source=source or None, status=status or None)
    if proto:
        rows = [r for r in rows if (r.get("proto") or "").lower() == proto]
    if query:
        rows = [r for r in rows
                if query in (r.get("name") or "").lower()
                or query in (r.get("fingerprint") or "").lower()
                or query in (r.get("server") or "").lower()]
    total = len(rows)
    start = (page - 1) * page_size
    return {"nodes": rows[start:start + page_size], "total": total,
            "page": page, "page_size": page_size}, None


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

    try:
        thread = threading.Thread(target=work, name="round", daemon=True)
        thread.start()
    except Exception:
        # No worker owns the lock until start succeeds.
        BUSY.release()
        raise
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
        if self.close_connection:
            self.send_header("Connection", "close")
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

    def _body_length(self):
        """Return a bounded content length, rejecting ambiguous HTTP framing."""
        if self.headers.get_all("Transfer-Encoding"):
            raise RequestBodyError(400, "Transfer-Encoding is not supported")
        lengths = self.headers.get_all("Content-Length", [])
        if not lengths:
            return 0
        if len(lengths) != 1:
            raise RequestBodyError(400, "multiple Content-Length headers")
        value = lengths[0].strip()
        if not value.isascii() or not value.isdigit():
            raise RequestBodyError(400, "invalid Content-Length")
        value = value.lstrip("0") or "0"
        if len(value) > len(str(MAX_JSON_BODY_BYTES)) or int(value) > MAX_JSON_BODY_BYTES:
            raise RequestBodyError(413, "request body too large")
        return int(value)

    def _read_body(self, length):
        """Read exactly length bytes within one deadline; raise on timeout or EOF."""
        previous_timeout = self.connection.gettimeout()
        deadline = time.monotonic() + BODY_TIMEOUT_SECONDS
        raw = bytearray()
        try:
            while len(raw) < length:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RequestBodyError(408, "request body timed out")
                self.connection.settimeout(remaining)
                chunk = self.rfile.read1(min(65536, length - len(raw)))
                if not chunk:
                    raise RequestBodyError(400, "incomplete request body")
                raw.extend(chunk)
        except TimeoutError as exc:
            raise RequestBodyError(408, "request body timed out") from exc
        finally:
            self.connection.settimeout(previous_timeout)
        return raw

    def _json_body(self):
        """Return a JSON object; empty commands remain valid, malformed ones fail."""
        length = self._body_length()
        if length == 0:
            return {}
        raw = self._read_body(length)
        try:
            body = json.loads(raw.decode("utf-8"), parse_constant=_reject_json_constant)
        except (ValueError, RecursionError) as exc:
            raise RequestBodyError(400, "invalid JSON body") from exc
        if not isinstance(body, dict):
            raise RequestBodyError(400, "JSON body must be an object")
        return body

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
            kind = auth_kind(self, cfg, path)
            if kind is None:
                return self._send(401, {"error": "unauthorized",
                                        "hint": "send header X-Auth-Token: <token> "
                                                "(Authorization: Bearer also works; "
                                                "query ?token= is legacy compat)"})
            _note_query_token_admin(kind)
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
            if path == "/api/probe/nodes":
                # Read-only ledger snapshot for the official Sub-Store Script
                # Operator (substore_bridge/probe_filter.script.js). The field
                # whitelist and the response envelope live in
                # substore_bridge.py so this endpoint and its tests cannot
                # drift apart -- and no config value is read here at all.
                query = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
                source = (query.get("source", [""])[0] or "").strip() or None
                status = (query.get("status", [""])[0] or "").strip() or None
                if status is not None and status not in substore_bridge.PROBE_NODE_STATUSES:
                    # Edge rejection, the `/api/run` mode whitelist style: an
                    # unknown status would otherwise come back as an empty
                    # 200 and read as "no such nodes" instead of a typo.
                    return self._send(400, {"error":
                        "status 必须是 "
                        f"{'/'.join(substore_bridge.PROBE_NODE_STATUSES)}"
                        f" 之一，收到 {status!r}"})
                # An unknown `source` is not an error: for a read-only publish
                # surface an empty answer is the correct answer, and it keeps
                # the semantics of cfg["sources"] off this endpoint.
                return self._send(200, substore_bridge.probe_nodes_snapshot(
                    db.list_nodes(source=source, status=status)))
            if path == "/api/logs":
                return self._send(200, {"events": db.recent_events(200)})
            if path == "/api/rounds":
                return self._send(200, {"rounds": db.last_rounds(30)})
            if path.startswith(V1_PREFIX):
                return self._v1_get(cfg, path)
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

    def _v1_get(self, cfg, path):
        """GET /api/v1/* (指导 §10)。全部 admin 面：调用方已过 auth_kind。

        探测器自己的两个写口（heartbeat/result）不在这里——它们用 probe
        token，在 do_POST 里分流，避免两套凭证在同一条全局鉴权里缠在一起。
        """
        query = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
        if path == "/api/v1/dashboard":
            return self._send(200, v1_dashboard(cfg))
        if path == "/api/v1/rounds":
            limit = _v1_limit(query.get("limit", [""])[0])
            if limit is None:
                return self._send(400, {"error": "BAD_PAYLOAD",
                                        "message": "limit 必须是 1..100 的整数"})
            return self._send(200, {"rounds": db.last_rounds(limit)})
        if path.startswith("/api/v1/rounds/"):
            detail = v1_round_detail(path[len("/api/v1/rounds/"):])
            if detail is None:
                return self._send(404, {"error": "ROUND_NOT_FOUND"})
            return self._send(200, detail)
        if path == "/api/v1/nodes":
            payload, error = v1_nodes(cfg, query)
            if error is not None:
                return self._send(error[0], error[1])
            return self._send(200, payload)
        if path == "/api/v1/probes":
            return self._send(200, {"probes": v1_probes(cfg)})
        return self._send(404, {"error": "not found", "path": path})

    # -- /api/v1 写接口（指导 §10）--------------------------------------
    #
    # 探测点路径用 probe token（probes[].token_env 指向的环境变量，Bearer，
    # 常量时间比较）；Gist / Sub-Store 占位用 admin token。两套凭证互不通：
    # probe token 调 admin 面、admin token 调 probe 面，都只得到一个 401。
    _V1_HEARTBEAT_RE = re.compile(
        r"^/api/v1/probes/([a-z0-9][a-z0-9_-]{1,31})/heartbeat$")
    _V1_RESULT_RE = re.compile(
        r"^/api/v1/probes/([a-z0-9][a-z0-9_-]{1,31})/jobs/([^/]+)/result$")

    def _v1_json_body(self):
        """A JSON object, with the Content-Type gate every write route needs.

        The body limit itself is `_body_length`'s (2 MiB) -- one implementation,
        so the v1 surface cannot drift from the framing tests that pin it.
        """
        ctype = (self.headers.get("Content-Type") or "").split(";")[0]
        if ctype.strip().lower() != "application/json":
            raise RequestBodyError(415, "Content-Type 必须是 application/json")
        return self._json_body()

    def _v1_post(self, cfg, path):
        match = self._V1_HEARTBEAT_RE.match(path)
        if match:
            return self._v1_heartbeat(cfg, match.group(1))
        match = self._V1_RESULT_RE.match(path)
        if match:
            return self._v1_result(cfg, match.group(1), match.group(2))
        if path == "/api/v1/gist/publish":
            return self._v1_gist_publish(cfg, path)
        if path in ("/api/v1/substore/preview", "/api/v1/substore/confirm-token",
                    "/api/v1/substore/push"):
            return self._v1_substore_placeholder(cfg, path, path.rsplit("/", 1)[-1])
        return self._send(404, {"error": "not found", "path": path})

    def _v1_deny_probe(self, code):
        """401 for a wrong/absent token; 503 when the env var itself is unset."""
        if code == "PROBE_TOKEN_UNSET":
            return self._send(503, {"error": code,
                                    "message": "探测点 token 环境变量未设置"})
        self.close_connection = True
        return self._send(401, {"error": code or "unauthorized"})

    def _v1_admin(self, cfg, path):
        kind = auth_kind(self, cfg, path)
        if kind is None or kind[1] != "admin":
            self.close_connection = True
            self._send(401, {"error": "unauthorized"})
            return False
        _note_query_token_admin(kind)
        return True

    def _v1_heartbeat(self, cfg, probe_id):
        entry = probe_dispatch.probe_entry(cfg, probe_id)
        if entry is None:
            # Unknown before any token work: this path can never authenticate,
            # and answering 401 would send an operator hunting a token problem.
            audit_id = uuid.uuid4().hex
            db.log("warn", f"heartbeat 拒绝 audit={audit_id} "
                           f"probe={probe_id}: UNKNOWN_PROBE")
            return self._send(404, {"error": "UNKNOWN_PROBE",
                                    "audit_id": audit_id})
        ok, code = probe_auth(self, cfg, probe_id)
        if not ok:
            return self._v1_deny_probe(code)
        body = self._v1_json_body()
        version, state = body.get("version"), body.get("state")
        if version is not None and (not isinstance(version, str)
                                    or len(version) > 64):
            return self._send(400, {"error": "BAD_PAYLOAD",
                                    "message": "version 必须是 ≤64 字符"})
        if state is not None and (not isinstance(state, str)
                                  or len(state) > 32):
            return self._send(400, {"error": "BAD_PAYLOAD",
                                    "message": "state 必须是 ≤32 字符"})
        if db.touch_probe_seen(probe_id, version=version, state=state) == 0:
            # Registered in config but never synced into the ledger: the
            # heartbeat itself is the registration event, so the probe's first
            # beat is not answered with an error nobody can act on.
            db.upsert_probe(probe_id, str(entry.get("name") or probe_id),
                            enabled=bool(entry.get("enabled", True)))
            db.touch_probe_seen(probe_id, version=version, state=state)
            db.log("info", f"探测点 {probe_id} 首次心跳，已按配置注册")
        return self._send(200, {"ok": True, "probe_id": probe_id})

    def _v1_result(self, cfg, probe_id, round_id):
        entry = probe_dispatch.probe_entry(cfg, probe_id)
        if entry is None:
            audit_id = uuid.uuid4().hex
            db.log("warn", f"探测点结果拒绝 audit={audit_id} "
                           f"probe={probe_id}: UNKNOWN_PROBE")
            return self._send(404, {"error": "UNKNOWN_PROBE",
                                    "audit_id": audit_id})
        ok, code = probe_auth(self, cfg, probe_id)
        if not ok:
            return self._v1_deny_probe(code)
        body = self._v1_json_body()
        audit_id = uuid.uuid4().hex
        if str(body.get("round_id")) != str(round_id):
            # The path names the round the Controller dispatched; a body that
            # disagrees is a client bug, and recording it under the body's
            # round would make the audit line lie about which job it answers.
            db.log("warn", f"探测点结果拒绝 audit={audit_id} probe={probe_id} "
                           f"round={round_id}: ROUND_MISMATCH")
            return self._send(400, {"error": "ROUND_MISMATCH",
                                    "audit_id": audit_id})
        code = probe_dispatch.verify_result(body, cfg)
        if code is None:
            recorded = len(body.get("results") or [])
            db.log("info", f"探测点结果已接收 audit={audit_id} "
                           f"probe={probe_id} round={round_id} nodes={recorded}")
            return self._send(200, {"ok": True, "recorded": recorded,
                                    "audit_id": audit_id})
        status, payload = _v1_error(code, audit_id=audit_id)
        db.log("warn", f"探测点结果被拒绝 audit={audit_id} probe={probe_id} "
                       f"round={round_id} code={code}")
        return self._send(status, payload)

    def _v1_gist_publish(self, cfg, path):
        """P4：手动触发一次 canonical 产物发布到固定四文件 Gist（指导 §8/§10）。

        与轮内自动发布共用 gist_store 的幂等规则（canonical 未变化 → 零
        PATCH）。token 只从 `gist.token_env` 环境变量读（gist_store 内部），
        响应与日志只出现脱敏后的错误码与 digest；每次调用都落一条
        publish_audit，包括被拒绝的。
        """
        if not self._v1_admin(cfg, path):
            return None
        self._v1_json_body()
        audit_id = uuid.uuid4().hex
        gist = cfg.get("gist") or {}
        gist_id = str(gist.get("gist_id") or "").strip()

        def _audit(status, summary=None):
            db.record_publish_audit(audit_id, "gist", gist_id or "gist",
                                    None, "", "admin", status, summary)

        if not gist.get("enabled", True):
            _audit("rejected_disabled", {"phase": "P4"})
            db.log("warn", f"Gist 发布拒绝 audit={audit_id}: GIST_DISABLED")
            return self._send(409, {"error": "GIST_DISABLED",
                                    "message": "gist.enabled 为 false",
                                    "audit_id": audit_id})
        artifact = canonical_artifact()
        if artifact is None:
            _audit("no_artifact", {"phase": "P4"})
            db.log("warn", f"Gist 发布拒绝 audit={audit_id}: NO_CANONICAL_ARTIFACT")
            return self._send(409, {"error": "NO_CANONICAL_ARTIFACT",
                                    "message": "尚无 canonical 导出，先完成一轮测活",
                                    "audit_id": audit_id})
        content, meta = artifact
        last = dict(db.last_round() or {})
        status_payload = {
            "round_id": last.get("id"),
            "total": last.get("total") or 0,
            "alive": last.get("ok") or 0,
            "suspect": bool(last.get("suspect")),
            "digest": str(meta.get("digest") or ""),
            "updated_at": db.now(),
        }
        try:
            history = gist_store.build_history(db)
        except Exception:  # noqa: BLE001 - history is best-effort
            history = ""
        result = gist_store.publish(
            cfg, content, meta,
            log=lambda level, message: db.log(
                level, f"[gist] {gist_store.mask(message)}"),
            status_payload=status_payload, history_text=history)
        status_word = str(result.get("status") or "publish_failed")
        db.record_publish_audit(
            audit_id, "gist", gist_id or "gist", result.get("revision"),
            str(result.get("digest") or ""), "admin", status_word,
            {"error_code": result.get("error_code"),
             "changed": result.get("changed")})
        if status_word == "publish_failed":
            error_code = str(result.get("error_code") or "")
            http = 409 if error_code == "GIST_CONFIG_ERROR" else 502
            db.log("warn", f"Gist 发布失败 audit={audit_id} code={error_code}")
            return self._send(http, {"error": "GIST_PUBLISH_FAILED",
                                     "error_code": error_code,
                                     "message": str(result.get("message") or ""),
                                     "audit_id": audit_id})
        db.log("info", f"Gist 手动发布 audit={audit_id} status={status_word} "
                       f"digest={result.get('digest')}")
        return self._send(200, {"ok": True, "status": status_word,
                                "digest": result.get("digest"),
                                "revision": result.get("revision"),
                                "changed": result.get("changed") or [],
                                "audit_id": audit_id})

    def _v1_substore_placeholder(self, cfg, path, action):
        """P5 占位：人工推送三件套（预览/确认 token/推送）全部待落地。

        现在就是 501 而不是静默成功，是因为「自动推送」正是本轮要删除的
        行为（指导 §0）：一个假装成功的占位比一个响亮的未实现更危险。
        """
        if not self._v1_admin(cfg, path):
            return None
        self._v1_json_body()
        audit_id = uuid.uuid4().hex
        db.record_publish_audit(
            audit_id, "substore",
            str((cfg.get("substore") or {}).get("target_name") or ""),
            None, "", "admin", "not_implemented",
            {"phase": "P5", "action": action})
        db.log("info", f"Sub-Store {action} 占位 audit={audit_id}（P5 落地）")
        return self._send(501, {"error": "PUSH_CONFIRM_REQUIRED",
                                "message": "P5 落地", "audit_id": audit_id})

    def do_POST(self):
        cfg = _State.cfg
        path = self._path()
        try:
            if path.startswith(V1_PREFIX):
                return self._v1_post(cfg, path)
            kind = auth_kind(self, cfg, path)
            if kind is None:
                self.close_connection = True
                return self._send(401, {"error": "unauthorized"})
            _note_query_token_admin(kind)
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
                # Workstreams/18 B5: the legacy create/refresh writeback is
                # gated behind BOTH switches (both default false), so a
                # default deployment never writes to Sub-Store from here --
                # F-001's "no writeback by default". Migration is the Rust
                # scan/apply/rollback surface (read-only scan, update-only
                # apply, no delete at all).
                if not engine.legacy_substore_write_enabled(cfg):
                    message = ("legacy Sub-Store 联动写入已停用"
                               "（push_to_substore + allow_legacy_substore_write "
                               "未同时开启）；迁移请用 probe-cli migrate scan"
                               "（只读扫描，绝不创建/删除）")
                    db.log("warn", "legacy Sub-Store 联动写入未开启，/api/link 已拒绝写入")
                    return self._send(200, {"message": message, "detail": [message]})
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
                        # Same double switch as /api/link above: without both
                        # flags the save stays write-free by default; the
                        # operator migrates via the Rust scan/apply surface.
                        if not engine.legacy_substore_write_enabled(new_cfg):
                            sync = []
                            db.log("warn", "legacy Sub-Store 联动写入未开启，保存未触发联动同步")
                        else:
                            try:
                                sync = engine.link_substore(new_cfg,
                                                            store_client(cfg))
                            except StoreError as exc:
                                sync = [f"联动同步失败: {exc}"]
                return self._send(200, {"saved": True, "config": redacted_config(new_cfg),
                                        "sources": new_cfg.get("sources", []),
                                        "link": sync, "notes": notes})
            if path == "/api/reload":
                db.log("info", "配置已重新加载")
                _State.cfg = cfgmod.load()
                return self._send(200, {"reloaded": True})
            return self._send(404, {"error": "not found", "path": path})
        except RequestBodyError as exc:
            # Unread or ambiguously framed bytes cannot become the next request.
            self.close_connection = True
            return self._send(exc.status, {"error": str(exc)})
        except Exception as exc:
            return self._send(500, {"error": f"{type(exc).__name__}: {exc}"})


def serve(cfg, host="127.0.0.1", port=8088):
    _State.cfg = cfg
    httpd = ThreadingHTTPServer((host, port), Handler)
    httpd.daemon_threads = True
    return httpd


def scheduler_loop(stop_event, poll_seconds=20):
    """Poll until stop_event is set, launching due rounds at the configured interval."""
    first_due = None
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
                first_due = None
            else:
                interval = max(1, int(schedule.get("interval_minutes", 30))) * 60
                last = db.last_round()
                last_ts = _parse(last["started_at"]) if last else 0
                now = time.time()
                if last_ts:
                    due = last_ts + interval
                    first_due = None
                else:
                    # Keep the startup deadline stable across empty-ledger polls.
                    if first_due is None:
                        first_due = now + 5
                    due = first_due
                _next_run["at"] = time.strftime("%H:%M:%S", time.localtime(due))
                if now >= due and not BUSY.locked():
                    run_in_background(cfg, trigger="schedule")
                    first_due = now + interval
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
