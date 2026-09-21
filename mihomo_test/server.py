"""HTTP API + dashboard, plus the interval scheduler."""
import calendar
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

BUSY = threading.Event()
_next_run = {"at": None}


class _State:
    cfg = None


def auth_ok(handler, cfg):
    token = cfg["auth"]["token"]
    if not token:
        return True
    query = urllib.parse.parse_qs(urllib.parse.urlparse(handler.path).query)
    if query.get("token", [""])[0] == token:
        return True
    if handler.headers.get("X-Auth-Token") == token:
        return True
    if handler.headers.get("Authorization") == f"Bearer {token}":
        return True
    return False


def exports_summary(cfg):
    """Only enabled sources are published, so only they belong in this list.

    A disabled source keeps its ledger for cheap re-enabling, but its export
    file is removed -- listing it here would advertise a stale snapshot.
    """
    out = []
    for source in cfg["sources"]:
        if not source.get("enabled", True):
            continue
        key = source["key"]
        meta = engine.export_meta(key) or {}
        out.append({"key": key, "url": engine.export_url(cfg, key),
                    "count": meta.get("count", 0)})
    return out


def status_payload(cfg):
    return {
        "stats": db.stats(),
        "last_round": db.last_round(),
        "busy": BUSY.is_set(),
        "next_run": _next_run["at"],
        "exports": exports_summary(cfg),
        "config": cfg,
    }


def run_in_background(cfg, trigger="manual", only_source=None):
    """Start a round on a worker thread; raise if one is already running."""
    if BUSY.is_set():
        raise engine.Busy("a round is already running")
    BUSY.set()

    def work():
        try:
            engine.run_round(cfg, trigger=trigger, only_source=only_source)
        except engine.Busy:
            # Normal when the scheduler fires while a round is still running.
            # Logged rather than swallowed because a *permanently* held lock
            # looks exactly like this from here, and silence is what let that
            # failure mode go unnoticed.
            db.log("info", "已有一轮在运行，跳过本次调度")
        except Exception as exc:  # keep the service alive across round failures
            db.log("error", f"本轮异常: {type(exc).__name__}: {exc}")
        finally:
            BUSY.clear()

    thread = threading.Thread(target=work, name="round", daemon=True)
    thread.start()
    return thread


class Handler(BaseHTTPRequestHandler):
    server_version = "mihomo-test"
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        pass

    # -- helpers ---------------------------------------------------------
    def _send(self, code, body, ctype="application/json; charset=utf-8"):
        if isinstance(body, (dict, list)):
            body = json.dumps(body, ensure_ascii=False, default=str)
        raw = body.encode("utf-8") if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Cache-Control", "no-store")
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
    def do_GET(self):
        cfg = _State.cfg
        path = self._path()
        try:
            if path in ("/healthz", "/api/health"):
                return self._send(200, {"ok": True})
            if not auth_ok(self, cfg):
                return self._send(401, {"error": "unauthorized",
                                        "hint": "append ?token=<your token>"})
            if path in ("/", "/ui"):
                return self._send(200, ui.render(cfg["ui"]["title"], cfg["auth"]["token"]),
                                  "text/html; charset=utf-8")
            if path == "/api/status":
                return self._send(200, status_payload(cfg))
            if path == "/api/nodes":
                nodes = db.list_nodes()
                for source in {n["source"] for n in nodes}:
                    trends = db.recent_trends(source)
                    for node in nodes:
                        if node["source"] == source:
                            node["trend"] = trends.get(node["fingerprint"], [])
                return self._send(200, {"nodes": nodes})
            if path == "/api/logs":
                return self._send(200, {"events": db.recent_events(200)})
            if path == "/api/rounds":
                return self._send(200, {"rounds": db.last_rounds(30)})
            if path == "/api/substore-resources":
                client = Client(cfg["substore"]["backend"])
                try:
                    available, errors = client.list_resources()
                except StoreError as exc:
                    return self._send(502, {"error": f"读取 Sub-Store 失败: {exc}",
                                            "available": [], "configured": cfg["sources"]})
                return self._send(200, {"available": available, "errors": errors,
                                        "configured": cfg["sources"]})
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
            if not auth_ok(self, cfg):
                return self._send(401, {"error": "unauthorized"})
            body = self._json_body()
            if path == "/api/run":
                try:
                    run_in_background(cfg, trigger=body.get("trigger", "manual"),
                                      only_source=body.get("source"))
                except engine.Busy:
                    return self._send(409, {"error": "a round is already running"})
                return self._send(200, {"started": True})
            if path == "/api/push":
                client = Client(cfg["substore"]["backend"])
                keys = [s["key"] for s in cfg["sources"]]
                result = engine.push_exports(cfg, client, keys)
                return self._send(200, {"message": "；".join(result), "detail": result})
            if path == "/api/alert-test":
                return self._send(200, {"message": "；".join(notifier.test_channels(cfg))})
            if path == "/api/link":
                client = Client(cfg["substore"]["backend"])
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
                sync = []
                if "sources" in clean and cfg["publish"].get("enabled", True):
                    # keep Sub-Store in step with the selection immediately:
                    # otherwise a removed source lingers as a dangling member
                    # until somebody remembers to press the sync button.
                    try:
                        sync = engine.link_substore(new_cfg, Client(cfg["substore"]["backend"]))
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
                if time.time() >= due and not BUSY.is_set():
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
