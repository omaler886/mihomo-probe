"""人工 Sub-Store 推送（指导 §0.6 / §9.4 / §13.5 / §10）。

P5 之后 Sub-Store 的**唯一**写路径是：

    预览（只读 diff）→ confirm-token（60s 一次性，绑定 target+digest+节点数）
    → push（校验并消费 token → update-only 更新目标对象）→ publish_audit

全部人工触发、全部落审计。自动回灌（轮末 push、Gist 发布联动）已从 engine
删除——本模块是仅存的写入口，且 update-only：目标对象不存在一律报错，绝不新建。

稳定错误码（指导 §10「稳定错误码」的扩展，HTTP 映射见 server._v1_error）：

* PUSH_CONFIRM_REQUIRED   —— 未带/未知 confirm_token
* CONFIRM_TOKEN_EXPIRED   —— 超过 60s 有效期
* CONFIRM_TOKEN_MISMATCH  —— target/digest/节点数与签发时不一致（内容已变）
* CONFIRM_TOKEN_REUSED    —— token 已被消费（防重放）
* SUBSTORE_TARGET_MISSING —— 目标对象不存在（拒绝新建）
* BAD_CONTENT             —— canonical 内容不是合法的 ClashMeta proxies YAML

token 注册表在进程内存中：60s TTL 下重启丢token只是让操作员重按一次预览，
不构成安全问题；审计在 DB，跨重启可查。样例输出只含 name/type —— server/
uuid/password 等 §13.7 敏感字段不进预览、不进日志。
"""
import secrets
import threading
import time

import yaml

from . import core as coremod
from .store import NotFound, StoreError

TOKEN_TTL_S = 60
SAMPLE_LIMIT = 20

_tokens = {}
_lock = threading.Lock()


class PublishError(Exception):
    """A push/preview must not proceed; carries the stable §10 error code."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.message = message


# ---- confirm token（一次性，绑定 target+digest+节点数，防重放）-----------

def issue_confirm_token(target, digest, node_count, ttl_s=TOKEN_TTL_S,
                        now=None):
    """Sign a 60s one-shot token bound to (target, digest, node_count)."""
    token = secrets.token_urlsafe(32)
    issued_at = time.time() if now is None else now()
    with _lock:
        # 一切旧 token（尤其同 target 的）立刻作废：操作员重新预览即重新签发，
        # 不允许两张并行有效的确认单指向同一目标。
        for key, entry in list(_tokens.items()):
            if entry["target"] == target:
                entry["used"] = True
        _tokens[token] = {
            "target": target, "digest": digest, "node_count": node_count,
            "expires_at": issued_at + ttl_s, "used": False,
        }
    return {"token": token, "expires_in": ttl_s, "target": target,
            "digest": digest, "node_count": node_count}


def consume_confirm_token(token, target, digest, node_count, now=None):
    """Consume one token, or raise PublishError with the stable code."""
    moment = time.time() if now is None else now()
    with _lock:
        entry = _tokens.get(str(token or ""))
        if entry is None:
            raise PublishError(
                "PUSH_CONFIRM_REQUIRED",
                "缺少有效的确认 token，请先 POST /api/v1/substore/confirm-token")
        if entry["used"]:
            raise PublishError("CONFIRM_TOKEN_REUSED",
                               "确认 token 已被使用（防重放）")
        if moment >= entry["expires_at"]:
            entry["used"] = True
            raise PublishError("CONFIRM_TOKEN_EXPIRED",
                               "确认 token 已过期（60s），请重新预览签发")
        if (entry["target"], entry["digest"],
                entry["node_count"]) != (target, digest, node_count):
            raise PublishError(
                "CONFIRM_TOKEN_MISMATCH",
                "目标/digest/节点数与签发时不一致，canonical 已变化，请重新预览")
        entry["used"] = True
    return True


def reset_tokens():
    """Test hook: drop every outstanding token."""
    with _lock:
        _tokens.clear()


# ---- preview -------------------------------------------------------------

def parse_proxies(content_text):
    """ClashMeta YAML -> proxy dicts; anything else is BAD_CONTENT."""
    try:
        data = yaml.safe_load(content_text or "")
    except yaml.YAMLError as exc:
        raise PublishError("BAD_CONTENT", f"YAML 解析失败: {exc}") from exc
    proxies = data.get("proxies") if isinstance(data, dict) else None
    if not isinstance(proxies, list):
        raise PublishError("BAD_CONTENT", "内容缺少 proxies 数组")
    return [p for p in proxies if isinstance(p, dict)]


def _by_fingerprint(proxies):
    out = {}
    for proxy in proxies:
        out.setdefault(coremod.fingerprint_proxy(proxy), proxy)
    return out


def _sample(proxy):
    """样例只带展示字段——§13.7 的连接凭据永不进预览/日志。"""
    return {"name": proxy.get("name"), "type": proxy.get("type")}


def diff_preview(current, canonical):
    """What the push would change, identity = 连接参数指纹。

    Renamed = 同指纹不同显示名——改名绝不能呈现成删除+新增对。样例各取前
    20 条（§9.4）。
    """
    current_by_fp = _by_fingerprint(current or [])
    canonical_by_fp = _by_fingerprint(canonical or [])
    added = [fp for fp in canonical_by_fp if fp not in current_by_fp]
    removed = [fp for fp in current_by_fp if fp not in canonical_by_fp]
    renamed = [{"from": current_by_fp[fp].get("name"),
                "to": canonical_by_fp[fp].get("name")}
               for fp in canonical_by_fp
               if fp in current_by_fp
               and current_by_fp[fp].get("name") != canonical_by_fp[fp].get("name")]
    return {
        "added": len(added),
        "removed": len(removed),
        "renamed": len(renamed),
        "unchanged": len(canonical_by_fp) - len(added) - len(renamed),
        "current_count": len(current_by_fp),
        "canonical_count": len(canonical_by_fp),
        "samples": {
            "added": [_sample(canonical_by_fp[fp]) for fp in added[:SAMPLE_LIMIT]],
            "removed": [_sample(current_by_fp[fp]) for fp in removed[:SAMPLE_LIMIT]],
            "renamed": renamed[:SAMPLE_LIMIT],
        },
    }


# ---- push ----------------------------------------------------------------

def target_name(cfg):
    return str((cfg.get("substore") or {}).get("target_name")
               or "mihomo-probe-alive")


def push_canonical(store, cfg, content_text, count, log=None):
    """update-only 把 canonical 内容写进 target_name；绝不新建对象。

    Returns a result dict `{status, target, count, action}`; raises
    PublishError(SUBSTORE_TARGET_MISSING / SUBSTORE_ERROR) on refusal —
    the caller records the audit row either way.
    """
    log = log or (lambda *args: None)
    name = target_name(cfg)
    payload = {
        "name": name,
        "displayName": f"mihomo-probe alive ({count})",
        "source": "local",
        "url": "",
        "content": content_text,
        "mergeSources": "",
        "ignoreFailedRemoteSub": "quiet",
        "passThroughUA": False,
        "process": [],
    }
    try:
        action = store.update_only("sub", name, payload)
    except NotFound:
        raise PublishError(
            "SUBSTORE_TARGET_MISSING",
            f"Sub-Store 对象 {name} 不存在；本服务绝不新建，请先在 Sub-Store 绑定") from None
    except StoreError as exc:
        raise PublishError("SUBSTORE_ERROR", f"Sub-Store 更新失败: {exc}") from exc
    log("info", f"Sub-Store 人工推送 {name} {action}，{count} 节点")
    return {"status": "pushed", "target": name, "count": count,
            "action": action}
