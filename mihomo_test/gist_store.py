"""GitHub Gist 作为唯一发布中间层（指导 §8 / §13）。

一个 Gist 只维护四个固定文件，禁止按探测点 / 轮次 / 来源新建：

* `mihomo-alive.yaml`   —— 唯一可订阅的 canonical 节点列表（来自 canonical_export）
* `probe-status.json`   —— 当前轮 + 各探测点健康概况（无节点凭据）
* `probe-history.jsonl` —— 最近 100 轮摘要，超出滚动截断（无凭据）
* `README.md`           —— 更新时间、格式说明、secret 非私密警告、手动推送方式

发布规则：

* GET 读当前 Gist → **以 canonical 文件的 sha 为准**做幂等门禁：canonical 内容
  未变化 → NoChange，零 PATCH、零 revision（指导 §8「内容 digest 未变化时不
  提交新 revision」；status/history/README 是随行文件，只在 canonical 变化的
  同一个 PATCH 里一起更新——否则带 updated_at 的 status 会让每个 round 都
  产生一个新 revision，幂等性就名存实亡）。变化则 PATCH 只含变化的文件。
* 首次 Gist 为空（四文件都不存在）时一次 PATCH 建齐四文件（create-in-place，
  不 POST 新 Gist；Gist id 已在 cfg）。
* 429/5xx 有界退避重试；401/403/404 终态不重试。
* API 失败返回 `{"status": "publish_failed", "error_code": ...}`，**不得影响账本
  结算**（UI 显示「本轮完成，发布失败」）。
* token 仅从 `cfg["gist"]["token_env"]` 指定的环境变量读（可回退
  `GITHUB_TOKEN_FILE` 指向的文件），所有日志经 `mask()` 脱敏。
* `publish(..., transport=...)` 可注入 transport，测试不触网。
"""
import hashlib
import json
import os
import re
import stat
import time
import urllib.error
import urllib.request

API = "https://api.github.com"
HISTORY_LIMIT = 100
# 退避上限（秒）。测试里可用 `backoff=` 收紧，真实调用最长等 8s。
MAX_BACKOFF_S = 8.0
# 终态 HTTP 状态：凭据 / 权限 / 对象不存在，重试不可能改变结果。
TERMINAL = frozenset({401, 403, 404, 422})
# 瞬时故障：有界退避后重试。
RETRYABLE = frozenset({429, 500, 502, 503, 504})


def mask(s):
    """任何输出前统一脱敏。

    覆盖：GitHub PAT、Bearer/Authorization、查询参数里的 token/key/password/
    access_token/secret/api_key、32/40 位十六进制串、UUIDv4。
    """
    s = str(s)
    s = re.sub(r"(ghp_|gho_|github_pat_)[A-Za-z0-9_]{20,}", r"\1***", s)
    s = re.sub(r"(?i)\b(bearer\s+)[A-Za-z0-9._\-]{16,}", r"\1***", s)
    s = re.sub(r"(?i)\b(authorization\s*[:=]\s*)(basic|bearer)\s+\S+", r"\1\2 ***", s)
    s = re.sub(r"([?&](?:token|key|password|access_token|secret|api_key|auth)=)"
               r"[^&\s]+", r"\1***", s)
    s = re.sub(r"\b[0-9a-f]{32,40}\b", lambda m: m.group(0)[:6] + "***", s)
    s = re.sub(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b",
               lambda m: m.group(0)[:8] + "-****-****-****-************", s)
    return s


def read_token(cfg):
    """Read the Gist token from `token_env`, falling back to GITHUB_TOKEN_FILE.

    Returns `(token, source)`; `(None, reason)` when nothing usable is set.
    """
    gist = cfg.get("gist") or {}
    env_name = str(gist.get("token_env") or "GITHUB_GIST_TOKEN").strip()
    token = os.environ.get(env_name, "").strip()
    if token:
        return token, env_name
    token_file = os.environ.get("GITHUB_TOKEN_FILE", "").strip()
    if token_file:
        path = os.path.expanduser(token_file)
        if not os.path.exists(path):
            return None, f"GITHUB_TOKEN_FILE 不存在: {path}"
        try:
            mode = stat.S_IMODE(os.stat(path).st_mode)
            if mode & 0o077:
                # 权限过宽只告警，不阻断：很多 Windows / 容器挂载拿不到 0600。
                os.environ.setdefault("GIST_TOKEN_FILE_PERM_WARN", oct(mode))
        except OSError:
            pass
        with open(path, "r", encoding="utf-8") as handle:
            token = handle.read().strip()
        if token:
            return token, f"file:{mask(token_file)}"
        return None, "GITHUB_TOKEN_FILE 为空"
    return None, f"未设置 {env_name} 或 GITHUB_TOKEN_FILE"


def gh(method, path, body=None, token=None, retries=3, backoff=MAX_BACKOFF_S,
       transport=None):
    """GitHub API call with bounded backoff; returns (status, headers, payload).

    `transport(method, url, headers, data_bytes) -> (status, headers, payload)`
    is the injection point tests use to stub 429/500/401 without touching the
    network: it owns one request's HTTP semantics and hands back a final answer
    for that request (it never retries on its own and never raises for an HTTP
    status). The retry / backoff / terminal-status rules below apply to a
    transport answer exactly as they do to a real `urllib` one. `None` selects
    the real path.
    """
    url = path if str(path).startswith("http") else API + path
    headers = {"User-Agent": "mihomo-test/1",
               "Accept": "application/vnd.github+json",
               "X-GitHub-Api-Version": "2022-11-28"}
    if token:
        headers["Authorization"] = "Bearer " + token
    data = json.dumps(body).encode("utf-8") if body is not None else None
    if data:
        headers["Content-Type"] = "application/json"
    last = None
    attempts = retries + 1
    for attempt in range(attempts):
        if transport is not None:
            status, resp_headers, payload = transport(method, url, headers, data)
            resp_headers = dict(resp_headers or {})
            payload = _decode(payload)
            if status in TERMINAL:
                return status, resp_headers, payload
            if status in RETRYABLE and attempt < attempts - 1:
                last = (status, resp_headers, payload)
                time.sleep(min(backoff, 2 ** attempt))
                continue
            return status, resp_headers, payload
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=25) as resp:
                raw = resp.read()
                return resp.status, dict(resp.headers), _decode(raw)
        except urllib.error.HTTPError as exc:
            raw = exc.read(1024)
            status = exc.code
            payload = _decode(raw) or {"error": mask(raw)}
            if status in TERMINAL:
                return status, {}, payload
            if status in RETRYABLE and attempt < attempts - 1:
                last = (status, {}, payload)
                time.sleep(min(backoff, 2 ** attempt))
                continue
            return status, {}, payload
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            payload = {"error": mask(exc)}
            if attempt < attempts - 1:
                last = (0, {}, payload)
                time.sleep(min(backoff, 2 ** attempt))
                continue
            return last
    return last


def _decode(raw):
    if not raw:
        return {}
    if isinstance(raw, (dict, list)):
        return raw
    try:
        return json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        return {}


def file_names(cfg):
    gist = cfg.get("gist") or {}
    return {
        "canonical": str(gist.get("canonical_file") or "mihomo-alive.yaml"),
        "status": str(gist.get("status_file") or "probe-status.json"),
        "history": str(gist.get("history_file") or "probe-history.jsonl"),
        "readme": "README.md",
    }


def build_readme(cfg, status_payload, meta):
    """README.md 正文：更新时间、格式说明、secret 警告、手动推送方式。"""
    gist = cfg.get("gist") or {}
    names = file_names(cfg)
    updated = (status_payload or {}).get("updated_at") or "unknown"
    visibility = str(gist.get("visibility") or "secret")
    count = (meta or {}).get("count", 0)
    lines = [
        "# mihomo-probe 测活订阅",
        "",
        f"- 更新时间（UTC）：{updated}",
        f"- 节点数：{count}",
        f"- digest：`{(meta or {}).get('digest') or ''}`",
        "",
        "## 文件",
        "",
        f"- `{names['canonical']}`：唯一可订阅的 canonical 节点列表（Clash/mihomo "
        "YAML）。每轮只更新这一个文件，节点顺序固定，不改连接参数。",
        f"- `{names['status']}`：当前轮与各探测点健康概况。不含任何节点凭据。",
        f"- `{names['history']}`：最近 "
        f"{HISTORY_LIMIT} 轮摘要（一行一 JSON，超出滚动截断）。不含节点凭据。",
        "",
        "## 安全提示",
        "",
        f"这个 Gist 是 **{visibility}**，但 **secret 不等于私密**：任何拿到 URL 的"
        "人都能读。订阅 YAML 含节点凭据（server / uuid / password / reality 等），"
        "请把 URL 当成凭据对待。若不能接受凭据经 URL 暴露，请改用私有 Git 仓库或"
        "自托管对象存储。",
        "",
        "## 手动推送到 Sub-Store",
        "",
        "本仓库**不会**自动写入 Sub-Store。要消费这份结果，请在 Sub-Store 里添加"
        f" `{names['canonical']}` 的 raw URL 作为远程订阅，或使用 Web 控制台"
        "「发布中心 → 确认推送到 Sub-Store」（需二次确认 + 一次性 token）。",
    ]
    return "\n".join(lines) + "\n"


def build_history(db, limit=HISTORY_LIMIT):
    """最近 `limit` 轮摘要，一行一 JSON；仅计数与发布状态，无凭据。

    `db` is the ledger module (mihomo_test.db); injected so tests can hand in a
    stub without touching the real file.
    """
    lines = []
    try:
        rows = db.query("SELECT * FROM rounds ORDER BY id DESC LIMIT ?", (limit,))
    except Exception:  # noqa: BLE001 - schema may predate `rounds`
        return ""
    for row in rows:
        row = dict(row)
        lines.append(json.dumps({
            "round_id": row.get("id"),
            "started_at": row.get("started_at"),
            "finished_at": row.get("finished_at"),
            "trigger": row.get("trigger"),
            "total": row.get("total") or 0,
            "alive": row.get("ok") or 0,
            "dead": row.get("failed") or 0,
            "dropped": row.get("dropped") or 0,
            "restored": row.get("restored") or 0,
            "suspect": bool(row.get("suspect")),
            "publish": "skipped" if row.get("suspect") else "ok",
        }, ensure_ascii=False))
    return "\n".join(lines) + ("\n" if lines else "")


def _truncate_history(text, limit=HISTORY_LIMIT):
    if not text:
        return ""
    lines = [ln for ln in text.splitlines() if ln.strip()]
    return "\n".join(lines[-limit:]) + ("\n" if lines else "")


class GistError(Exception):
    """Raised internally when the API answers in a way we cannot publish past."""


def publish(cfg, canonical_bytes, meta, log=None, status_payload=None,
            history_text=None, transport=None, db=None, backoff=MAX_BACKOFF_S):
    """Publish the four fixed files; idempotent on content digest.

    Returns one of:
        {"status": "published", "revision", "digest", "content_sha256", "changed"}
        {"status": "no_change", "digest", "content_sha256", "revision"}
        {"status": "publish_failed", "error_code", "message"}
    """
    log = log or (lambda level, message: None)
    gist = cfg.get("gist") or {}
    gist_id = str(gist.get("gist_id") or "").strip()
    names = file_names(cfg)
    digest = (meta or {}).get("digest") or ""
    content_sha = (meta or {}).get("content_sha256") or _sha(canonical_bytes)

    token, source = read_token(cfg)
    if not token:
        message = f"Gist token 未配置：{mask(source)}"
        log("warn", message)
        return {"status": "publish_failed", "error_code": "GIST_CONFIG_ERROR",
                "message": message}

    if not gist_id:
        message = "gist.gist_id 未配置"
        log("warn", message)
        return {"status": "publish_failed", "error_code": "GIST_CONFIG_ERROR",
                "message": message}

    # ---- read current state
    status, _headers, payload = gh("GET", f"/gists/{gist_id}", token=token,
                                   retries=2, backoff=backoff, transport=transport)
    if status in (401, 403):
        log("warn", f"Gist 读取被拒（{status}），audit token={mask(token)}")
        return {"status": "publish_failed", "error_code": f"GIST_HTTP_{status}",
                "message": f"GitHub 返回 {status}（凭据或权限）"}
    if status == 404:
        log("warn", f"Gist {mask(gist_id)} 不存在或无权限")
        return {"status": "publish_failed", "error_code": "GIST_HTTP_404",
                "message": "Gist 不存在或 token 无权限"}
    if status != 200:
        log("warn", f"Gist 读取失败 status={status}")
        return {"status": "publish_failed", "error_code": f"GIST_HTTP_{status}",
                "message": f"GitHub 返回 {status}"}

    files = (payload or {}).get("files") or {}
    remote = _remote_shas(files)
    revision = str((payload or {}).get("history_url") or "").rstrip("/").split("/")[-1] \
        or None

    # ---- compose the four fixed files
    status_json = status_payload if status_payload is not None else {}
    if not isinstance(status_json, dict):
        status_json = {}
    status_json.setdefault("digest", digest)
    status_json.setdefault("updated_at", _now())
    status_body = json.dumps(status_json, ensure_ascii=False, sort_keys=True,
                             indent=2) + "\n"
    history_body = _truncate_history(history_text if history_text is not None
                                     else (build_history(db) if db else ""))
    readme_body = build_readme(cfg, status_json, meta)
    wanted = {
        names["canonical"]: canonical_bytes.decode("utf-8"),
        names["status"]: status_body,
        names["history"]: history_body,
        names["readme"]: readme_body,
    }
    wanted_sha = {name: _sha_text(body) for name, body in wanted.items()}

    if _sha_eq(remote.get(names["canonical"]), wanted_sha[names["canonical"]]):
        # 指导 §8：canonical（节点内容）未变化 → 不提交新 revision。status/
        # history/README 即便有新内容也作为随行文件等待，最新一轮状态由
        # /api/v1/dashboard 实时给出；Gist 的一个 revision = 一次真实节点集变化。
        log("info", f"Gist 内容未变化 digest={digest}，不提交新 revision")
        return {"status": "no_change", "digest": digest,
                "content_sha256": content_sha, "revision": revision}

    changed = {name: body for name, body in wanted.items()
               if not _sha_eq(remote.get(name), wanted_sha[name])}

    # ---- PATCH only the changed files
    patch = {"files": {name: {"content": body} for name, body in changed.items()}}
    status, _headers, resp = gh("PATCH", f"/gists/{gist_id}", body=patch,
                                token=token, retries=3, backoff=backoff,
                                transport=transport)
    if status != 200:
        log("warn", f"Gist 发布失败 status={status} files={sorted(changed)}")
        return {"status": "publish_failed", "error_code": f"GIST_HTTP_{status}",
                "message": f"GitHub 返回 {status}"}
    new_revision = str((resp or {}).get("history_url") or "").rstrip("/").split("/")[-1] \
        or revision
    log("info", f"Gist 已发布 revision={new_revision} digest={digest} "
                f"changed={sorted(changed)}")
    return {"status": "published", "revision": new_revision, "digest": digest,
            "content_sha256": content_sha, "changed": sorted(changed)}


def _sha(content_bytes):
    return hashlib.sha256(content_bytes).hexdigest()


def _sha_text(text):
    """Git blob sha1 of a file body -- the form GitHub's API reports as `sha`.

    Exposed for tests: a transport stub builds its `files[name].sha` from this
    so a genuine no-change round compares equal without any real network call.
    """
    body = text.encode("utf-8")
    header = f"blob {len(body)}\0".encode("ascii")
    return "sha1:" + hashlib.sha1(header + body).hexdigest()


def _sha_eq(remote, wanted):
    """Compare a GitHub-reported file sha with our computed one.

    GitHub's `sha` is the git blob hash (sha1 of `blob <len>\\0<content>`), so a
    test stub that reports `sha1:<hex>` or `sha256:<hex>` must match on the same
    scheme. A bare hex is compared as-is, which is what a hand-built fixture
    uses. Cross-scheme never matches -- that is the safe direction: an unknown
    remote value means "changed", and a PATCH is idempotent anyway.
    """
    remote, wanted = str(remote), str(wanted)
    if ":" in remote and ":" in wanted:
        return remote.split(":", 1) == wanted.split(":", 1)
    return remote == wanted


def _remote_shas(files):
    """{filename: sha} for the files GitHub currently stores."""
    out = {}
    for name, entry in files.items():
        if isinstance(entry, dict):
            sha = entry.get("sha") or entry.get("raw_url")
            if sha:
                out[str(name)] = str(sha)
    return out


def _now():
    import time as _time
    return _time.strftime("%Y-%m-%dT%H:%M:%S", _time.gmtime())
