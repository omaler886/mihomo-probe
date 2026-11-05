"""HK3 多探测点：Controller 侧 Job 分发与 ProbeResult 校验（指导 §4.1/§4.2/§10/§13）。

角色分工（指导 §2 目标架构）：

- Controller（engine 轮次）把轻量 Job POST 给每个启用探测点：
  `dispatch_round`。
- 探测点把 ProbeResult POST 回 Controller：
  `verify_result` 校验 round/probe/digest/deadline 四项，合法才幂等落
  `probe_observations`；旧轮次、未知探测点、digest 不符、超时结果只写
  `events` 审计，绝不推进死亡计数（指导 §4.2/§4.3）。
- 节点凭据只随 Job 走 HTTPS（loopback 例外供本地/测试），请求带每探测点
  独立 token + HMAC-SHA256 签名与 5 分钟时间窗（指导 §13.1）。

契约（与 P3a engine 侧，brief 约定，分歧以 brief 为准）：

- `engine._dispatch_probe_jobs(cfg, round_id, jobs, log)` 内部
  `from . import probe_dispatch; return probe_dispatch.dispatch_round(...)`。
- Job: {"round_id","probe_id","config_digest","deadline"(ISO8601),
  "chain_enabled":bool,"nodes":[{"fingerprint","mihomo_name","proxy":dict}]}。
- ProbeResult: {"round_id","probe_id","config_digest","started_at",
  "finished_at","results":[{"fingerprint","alive","delay_ms","reason",
  "egress_country"}]}。
- `engine.config_digest(cfg)` 由 P3a 提供；本模块的 `config_digest` 优先调用
  它，未就绪时用同一算法的本地实现（与 engine.config_digest 一致，P3a 对齐）。

日志纪律（指导 §13.7）：token、请求体（含节点凭据）一律不进日志与 events；
只记录探测点 id、轮次、节点数与错误类型。
"""
import hashlib
import hmac
import json
import os
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone

from . import config as cfgmod
from . import db

# 指导 §13.1：HMAC 签名 + 5 分钟时间窗。
SIGNATURE_HEADER = "X-Probe-Signature"
TIMESTAMP_HEADER = "X-Probe-Timestamp"
SIGNATURE_WINDOW_S = 300

# 分发超时：一次 POST 最多等这么久。探测点超时/离线按「该探测点观测缺失」
# 处理，由聚合层兜底（指导 §4.3），不影响 Controller 其它探测点。
PROBE_TIMEOUT_S = 15

# 含节点凭据的 Job 只允许 HTTPS；loopback 是本地联调与测试的唯一例外。
LOOPBACK_HOSTS = {"127.0.0.1", "localhost", "::1"}

# config_digest 的算法与 engine.config_digest 逐字一致（P3a 对齐）：只取探测
# 结论依赖的六个 section，缺省的 section 不参与（engine.DIGEST_SECTIONS 同规则）。
# 两份实现若哈希不同的键集，会互相拒绝对方的结果——所以回退实现是照抄，不是另起
# 一套更宽的。

# 每 Job 的 deadline 登记表：Controller 重启后仍需知道「这轮本来什么时候
# 截止」，否则过期结果无从判定。只存 probe_id/round_id/deadline，无凭据。
DEADLINE_KEEP_S = 24 * 3600
DEADLINE_MAX_ENTRIES = 4096


# ------------------------------------------------------------------ 时间工具


def iso_now():
    """UTC ISO8601（秒级，Z 后缀），与指导 §4.1/§4.2 的报文形状一致。"""
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_iso8601(text):
    """Parse an ISO8601 stamp into an aware UTC datetime; None when unusable.

    Naive stamps are read as UTC: every producer here (db.now, iso_now) writes
    UTC, so guessing local time would shift a deadline by the container's TZ
    offset -- the same class of bug db.now's docstring records.
    """
    if not isinstance(text, str) or not text.strip():
        return None
    raw = text.strip()
    if raw.endswith(("Z", "z")):
        raw = raw[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(raw)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


# ------------------------------------------------------------ config_digest


def _engine_config_digest():
    """engine.config_digest when P3a has landed it, else None.

    Imported lazily: engine imports this module inside `_dispatch_probe_jobs`,
    and a module-level import here would turn that into a cycle the moment the
    two files grow a top-level dependency on each other.
    """
    try:
        from . import engine as _engine
    except Exception:  # pragma: no cover - engine is always importable here
        return None
    fn = getattr(_engine, "config_digest", None)
    return fn if callable(fn) else None


def config_digest(cfg):
    """Stable digest of the unique logical config a round was built from.

    Prefers `engine.config_digest` (P3a): the Job's digest is computed there,
    so preferring it keeps dispatch and verification on one algorithm even if
    the two implementations ever drift. The local fallback below is the same
    algorithm, verbatim (与 engine.config_digest 一致，P3a 对齐): the six
    sections a probe's verdict depends on, canonical JSON, sha256,
    "sha256:" prefixed. Two implementations that hash different key sets
    would refuse each other's results, so the fallback copies the section
    list rather than inventing a wider one.
    """
    fn = _engine_config_digest()
    if fn is not None:
        try:
            return fn(cfg or {})
        except Exception:
            pass
    sections = {}
    for key in ("probes", "aggregation", "chain", "test", "policy", "verify"):
        block = (cfg or {}).get(key)
        if block is not None:
            sections[key] = block
    blob = json.dumps(sections, sort_keys=True, ensure_ascii=False,
                      separators=(",", ":"), default=str)
    return "sha256:" + hashlib.sha256(blob.encode("utf-8")).hexdigest()


# ------------------------------------------------------------------ HMAC 工具


def sign_request(secret, body_bytes, ts):
    """Hex HMAC-SHA256 over `ts + "\\n" + body` keyed by the probe token.

    Pure function: the Controller signs every Job and the Probe Agent verifies
    it with the same code, so the two can never disagree about what is signed.
    The timestamp is inside the signed material, so a captured request cannot
    be replayed outside the 5-minute window (指导 §13.1).
    """
    if isinstance(body_bytes, str):
        body_bytes = body_bytes.encode("utf-8")
    mac = hmac.new(str(secret).encode("utf-8"),
                   str(ts).encode("utf-8") + b"\n" + body_bytes,
                   hashlib.sha256)
    return mac.hexdigest()


def verify_signature(secret, body_bytes, ts, signature,
                     window_s=SIGNATURE_WINDOW_S, now=None):
    """True when the signature is valid AND the timestamp is inside the window.

    Both failures answer False -- a caller that wants to tell "expired" from
    "forged" checks the timestamp itself, which is public data anyway.
    """
    if not isinstance(signature, str) or not signature.strip():
        return False
    try:
        ts_value = int(str(ts).strip())
    except (TypeError, ValueError):
        return False
    now = int(time.time()) if now is None else int(now)
    if abs(now - ts_value) > int(window_s):
        return False
    expected = sign_request(secret, body_bytes, ts_value)
    return hmac.compare_digest(expected, signature.strip().lower())


# ------------------------------------------------------------ deadline 登记表

_deadline_lock = threading.Lock()
_deadline_cache = None


def _deadlines_path():
    return cfgmod.DATA / "probe_deadlines.json"


def _load_deadlines():
    global _deadline_cache
    if _deadline_cache is not None:
        return _deadline_cache
    data = {}
    try:
        raw = json.loads(_deadlines_path().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        raw = None
    if isinstance(raw, dict):
        for key, value in raw.items():
            if isinstance(value, dict) and isinstance(value.get("deadline"), str):
                data[key] = {"deadline": value["deadline"],
                             "at": float(value.get("at") or 0.0)}
    _deadline_cache = data
    return data


def _save_deadlines(data):
    """Persist the table; a store that cannot be written must not fail a round.

    Losing it costs lateness detection after a restart (the newest-round and
    settled-round gates still reject stale rounds), which is strictly better
    than refusing to dispatch a Job because a JSON file was unwritable.
    """
    path = _deadlines_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
        tmp.replace(path)
    except OSError:
        pass


def note_deadline(probe_id, round_id, deadline):
    """Record the Job's deadline so a late result can be refused (指导 §4.2)."""
    if not deadline:
        return
    key = f"{probe_id}|{round_id}"
    now = time.time()
    with _deadline_lock:
        data = _load_deadlines()
        data[key] = {"deadline": str(deadline), "at": now}
        if len(data) > DEADLINE_MAX_ENTRIES:
            cutoff = now - DEADLINE_KEEP_S
            for stale in [k for k, v in data.items()
                          if v.get("at", 0.0) < cutoff]:
                del data[stale]
            while len(data) > DEADLINE_MAX_ENTRIES:
                data.pop(next(iter(data)))
        _save_deadlines(data)


def deadline_for(probe_id, round_id):
    """The registered deadline for one (probe, round), or None."""
    with _deadline_lock:
        entry = _load_deadlines().get(f"{probe_id}|{round_id}")
        return entry.get("deadline") if entry else None


def reset_deadlines():
    """Drop the in-process table and its file (test hook)."""
    global _deadline_cache
    with _deadline_lock:
        _deadline_cache = {}
        try:
            _deadlines_path().unlink()
        except OSError:
            pass


# ------------------------------------------------------------------ 探测点


def probe_entry(cfg, probe_id):
    """The probes[] entry for one id, or None (enabled or not)."""
    for entry in (cfg or {}).get("probes") or []:
        if isinstance(entry, dict) and str(entry.get("id") or "") == probe_id:
            return entry
    return None


def probe_token(entry):
    """The probe's token, read from the env var its token_env names.

    Empty when unset: callers treat that as "cannot dispatch / cannot
    authenticate" rather than falling back to some shared secret (指导 §3:
    token 只允许引用环境变量名).
    """
    env_name = str((entry or {}).get("token_env") or "")
    if not env_name:
        return ""
    return os.environ.get(env_name, "") or ""


def _endpoint_allowed(endpoint):
    """HTTPS, or loopback HTTP for local wiring and the test suite (指导 §13.1)."""
    try:
        parsed = urllib.parse.urlsplit(str(endpoint or ""))
    except ValueError:
        return False
    if parsed.scheme == "https":
        return True
    if parsed.scheme == "http" and (parsed.hostname or "").lower() in LOOPBACK_HOSTS:
        return True
    return False


# ------------------------------------------------------------ Job 分发（§4.1）


def dispatch_round(cfg, round_id, jobs, log):
    """POST every Job to its probe; return {probe_id: "ok"|"err"}.

    Never raises: one unreachable probe must not take the round down with it.
    A probe that times out, refuses the connection or answers 5xx simply has
    no observations this round -- "未完成", which the aggregation layer treats
    as missing evidence, never as a node failure (指导 §4.3). The deadline is
    registered before the POST so a result whose reply was lost still gets the
    lateness gate.
    """
    summary = {}
    enabled = {}
    for entry in (cfg or {}).get("probes") or []:
        if isinstance(entry, dict) and entry.get("enabled", True):
            enabled[str(entry.get("id") or "")] = entry
    for job in jobs or []:
        if not isinstance(job, dict):
            summary["unknown"] = "err"
            log("warn", "Job 分发跳过：任务不是对象")
            continue
        probe_id = str(job.get("probe_id") or "")
        entry = enabled.get(probe_id)
        if entry is None:
            summary[probe_id or "unknown"] = "err"
            log("warn", f"Job 分发跳过：探测点 {probe_id or '?'} 未注册或已禁用")
            continue
        endpoint = str(entry.get("endpoint") or "").strip()
        if not endpoint:
            summary[probe_id] = "err"
            log("warn", f"Job 分发跳过：探测点 {probe_id} 未配置 endpoint")
            continue
        token = probe_token(entry)
        if not token:
            summary[probe_id] = "err"
            log("warn", f"Job 分发跳过：探测点 {probe_id} 的 token 环境变量 "
                        f"{entry.get('token_env')} 未设置")
            continue
        if not _endpoint_allowed(endpoint):
            summary[probe_id] = "err"
            log("warn", f"Job 分发拒绝：探测点 {probe_id} 的 endpoint 不是 "
                        "HTTPS（指导 §13.1：含节点凭据的 Job 不得明文传输；"
                        "loopback 除外）")
            continue
        body = json.dumps(job, ensure_ascii=False,
                          separators=(",", ":")).encode("utf-8")
        ts = str(int(time.time()))
        headers = {
            "Content-Type": "application/json",
            "Authorization": f"Bearer {token}",
            TIMESTAMP_HEADER: ts,
            SIGNATURE_HEADER: sign_request(token, body, ts),
        }
        note_deadline(probe_id, round_id, job.get("deadline"))
        url = endpoint.rstrip("/") + "/jobs"
        try:
            req = urllib.request.Request(url, data=body, headers=headers,
                                         method="POST")
            with urllib.request.urlopen(req, timeout=PROBE_TIMEOUT_S) as resp:
                resp.read()
                code = getattr(resp, "status", 200)
            if 200 <= code < 300:
                summary[probe_id] = "ok"
                log("info", f"Job 已分发：探测点 {probe_id}，轮次 {round_id}，"
                            f"{len(job.get('nodes') or [])} 节点")
            else:
                summary[probe_id] = "err"
                log("warn", f"Job 分发失败：探测点 {probe_id} 返回 HTTP {code}")
        except urllib.error.HTTPError as exc:
            summary[probe_id] = "err"
            log("warn", f"Job 分发失败：探测点 {probe_id} 返回 HTTP {exc.code}")
        except Exception as exc:
            # Offline, DNS, TLS, timeout: the probe is simply absent this
            # round. The type name is safe to log; the message can echo the
            # URL (which carries no credential) but never the token or body.
            summary[probe_id] = "err"
            log("warn", f"Job 分发失败：探测点 {probe_id} 无响应"
                        f"（{type(exc).__name__}）")
    return summary


# ------------------------------------------------------- ProbeResult 校验（§4.2）


def _reject(code, why, round_id, probe_id):
    """Audit-only rejection: an events line, no observation, no streak."""
    db.log("warn", f"探测点结果被拒绝[{code}] round={round_id} probe={probe_id}: {why}")
    return code


def _round_row(round_id):
    try:
        return db.one("SELECT * FROM rounds WHERE id=?", (round_id,))
    except Exception:
        # A round_id that is not id-shaped (dict, "r_...", ...) cannot name a
        # round; sqlite would raise on the binding.
        return None


def _probe_known(probe_id, cfg):
    if probe_entry(cfg, probe_id) is not None:
        return True
    try:
        return db.one("SELECT probe_id FROM probes WHERE probe_id=?",
                      (probe_id,)) is not None
    except Exception:
        return False


def _clean_result_item(item):
    """One ProbeResult entry in validated form, or None when malformed."""
    if not isinstance(item, dict):
        return None
    fingerprint = item.get("fingerprint")
    if not isinstance(fingerprint, str) or not fingerprint:
        return None
    alive = item.get("alive")
    if not isinstance(alive, bool):
        return None
    delay_ms = item.get("delay_ms")
    if delay_ms is not None and (not isinstance(delay_ms, int)
                                 or isinstance(delay_ms, bool) or delay_ms < 0):
        return None
    reason = item.get("reason")
    if reason is not None and not isinstance(reason, str):
        return None
    country = item.get("egress_country")
    if country is not None and (not isinstance(country, str) or len(country) > 8):
        return None
    return {"fingerprint": fingerprint, "alive": alive, "delay_ms": delay_ms,
            "reason": reason, "egress_country": country}


def _payload_digest(payload):
    blob = json.dumps(payload, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, default=str)
    return "sha256:" + hashlib.sha256(blob.encode("utf-8")).hexdigest()


def _deadline_for(probe_id, round_id):
    """The Job's registered deadline, or None when the Controller cannot know it.

    Only the dispatch-side registration is authoritative: it survives a
    Controller restart (the table is persisted next to the ledger), and when
    even that is missing there is no honest deadline to enforce -- the
    newest-round and settled-round gates above are what still keep an old
    round's result out of the ledger.
    """
    registered = deadline_for(probe_id, round_id)
    if registered:
        parsed = parse_iso8601(registered)
        if parsed is not None:
            return parsed
    return None


def verify_result(payload, cfg, expected_digest=None):
    """Validate one ProbeResult; record it, or return a stable error code.

    Returns None when accepted (observations written idempotently and the
    probe touched), else one of BAD_PAYLOAD / ROUND_STALE / UNKNOWN_PROBE /
    DIGEST_MISMATCH / RESULT_LATE. Every rejection is audit-only (指导 §4.2):
    an events line, never an observation, never a death-count change.
    """
    if not isinstance(payload, dict):
        return _reject("BAD_PAYLOAD", "payload 不是对象", None, None)
    round_id = payload.get("round_id")
    probe_id = payload.get("probe_id")
    digest = payload.get("config_digest")
    results = payload.get("results")
    if not isinstance(probe_id, str) or not probe_id:
        return _reject("BAD_PAYLOAD", "probe_id 缺失或不是字符串", round_id, None)
    if not isinstance(digest, str) or not digest:
        return _reject("BAD_PAYLOAD", "config_digest 缺失", round_id, probe_id)
    if not isinstance(results, list):
        return _reject("BAD_PAYLOAD", "results 缺失或不是数组", round_id, probe_id)
    items = []
    for index, item in enumerate(results):
        clean = _clean_result_item(item)
        if clean is None:
            return _reject("BAD_PAYLOAD", f"results[{index}] 形状非法",
                           round_id, probe_id)
        items.append(clean)

    # 1) 轮次：必须存在、未结算、且仍是最新一轮（旧轮次只入审计，指导 §4.2）。
    row = _round_row(round_id)
    if row is None:
        return _reject("ROUND_STALE", "轮次不存在", round_id, probe_id)
    if row.get("finished_at"):
        return _reject("ROUND_STALE", "轮次已结算", round_id, probe_id)
    newest = db.one("SELECT MAX(id) AS m FROM rounds") or {}
    try:
        if int(round_id) < int(newest.get("m") or 0):
            return _reject("ROUND_STALE", "已被更新的轮次取代", round_id, probe_id)
    except (TypeError, ValueError):
        return _reject("ROUND_STALE", "round_id 不是轮次 id", round_id, probe_id)

    # 2) 探测点：必须已知（配置注册或账本里心跳过）。
    if not _probe_known(probe_id, cfg):
        return _reject("UNKNOWN_PROBE", "探测点未注册", round_id, probe_id)

    # 3) digest：必须与轮次所依据的唯一配置一致。
    expected = expected_digest or config_digest(cfg or {})
    if digest != expected:
        return _reject("DIGEST_MISMATCH", "config_digest 与当前配置不一致",
                       round_id, probe_id)

    # 4) deadline：超时回传只入审计（指导 §4.2/§14 集成场景）。
    deadline = _deadline_for(probe_id, round_id)
    if deadline is not None:
        if datetime.now(timezone.utc) > deadline:
            return _reject("RESULT_LATE", "超过本轮 deadline", round_id, probe_id)
        finished = parse_iso8601(payload.get("finished_at"))
        if finished is not None and finished > deadline:
            return _reject("RESULT_LATE", "finished_at 晚于 deadline",
                           round_id, probe_id)

    # 合法：幂等落观测（PK round+probe+fingerprint，重复上报覆盖）+ 心跳。
    payload_digest = _payload_digest(payload)
    for item in items:
        db.record_observation(round_id, probe_id, item["fingerprint"],
                              item["alive"], item["delay_ms"], item["reason"],
                              item["egress_country"], payload_digest)
    db.touch_probe_seen(probe_id)
    return None
