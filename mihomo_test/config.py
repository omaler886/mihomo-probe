"""Configuration for mihomo-test.

Settings live in one JSON file so the Web UI can edit them. Secrets (the
Sub-Store backend URL and the UI token) default to env vars so a redeploy
does not have to rewrite the file.
"""
import copy
import json
import os
import re
import secrets
import threading
from pathlib import Path

# ROOT is where the app looks for its own files; HOST_ROOT is where those same
# files live on the docker host. They are the same path in a non-container
# deploy. When the app runs in a container they must differ, because the app
# asks the host docker daemon to bind-mount project paths for the kernel's
# own `mihomo -t` config check -- a container path would not resolve there.
ROOT = Path(os.environ.get("MIHOMO_TEST_ROOT", "/srv/mihomo-test"))
HOST_ROOT = Path(os.environ.get("MIHOMO_TEST_HOST_ROOT", str(ROOT)))
DATA = ROOT / "data"
CORE_DIR = ROOT / "core"
CONFIG_PATH = DATA / "config.json"
CORE_SECRET_PATH = DATA / "core.secret"
TUNNEL_TOKEN_PATH = DATA / "tunnel.token"

_lock = threading.RLock()

# A source key becomes both a file name (data/exports/<key>.yaml) and a URL
# segment, so path separators and dot-only names have to be refused.
_UNSAFE_KEY = re.compile(r'[/\\:*?"<>|\x00-\x1f]')
KEY_MAX = 48
SOURCE_KINDS = ("collection", "sub")


def safe_key(value, fallback="src"):
    """Derive a filesystem- and URL-safe key from a resource name."""
    text = _UNSAFE_KEY.sub("", str(value or ""))
    text = re.sub(r"\s+", "-", text.strip()).strip(".")
    text = text[:KEY_MAX]
    if not text or text in (".", ".."):
        text = fallback
    return text


def validate_key(key):
    """Return key if it is usable as a file name and URL segment."""
    if not isinstance(key, str) or not key:
        raise ValueError("key 不能为空")
    if key != key.strip():
        raise ValueError(f"key 首尾不能有空白: {key!r}")
    if key in (".", "..") or key.startswith("."):
        raise ValueError(f"key 不能以点开头: {key!r}")
    if _UNSAFE_KEY.search(key):
        raise ValueError(f"key 含非法字符 (/ \\ : * ? \" < > |): {key!r}")
    if len(key) > KEY_MAX:
        raise ValueError(f"key 过长（上限 {KEY_MAX}）: {key!r}")
    return key


def normalize_sources(sources):
    """Repair a sources list: validate names, default kinds, unique safe keys.

    An explicitly supplied key is validated strictly so the user gets a clear
    error; an absent one is derived leniently from the resource name.
    """
    if not isinstance(sources, list):
        return []
    out, used = [], set()
    for entry in sources:
        if not isinstance(entry, dict):
            continue
        name = str(entry.get("name") or "").strip()
        if not name:
            continue
        kind = entry.get("kind") if entry.get("kind") in SOURCE_KINDS else "collection"
        raw_key = str(entry.get("key") or "").strip()
        key = validate_key(raw_key) if raw_key else safe_key(name, fallback="src")
        base, suffix = key, 2
        while key in used:
            key = f"{base}-{suffix}"
            suffix += 1
        used.add(key)
        out.append({
            "key": key,
            "kind": kind,
            "name": name,
            "label": str(entry.get("label") or name),
            "enabled": entry.get("enabled") is not False,
        })
    return out


DEFAULTS = {
    "substore": {
        # Sub-Store's backend URL can carry a secret path (this deployment's
        # does). It is deliberately NOT the default here: the default must stay
        # credential-free so nothing sensitive can be committed. Deployments
        # that need one set SUBSTORE_BACKEND in .env -- and once config.json
        # exists its stored value wins over this anyway.
        "backend": os.environ.get("SUBSTORE_BACKEND", "http://127.0.0.1:3000"),
    },
    "core": {
        # Deliberately not 19090/19094: the legacy /srv/mihomo-health stack
        # binds 19090 on its own 30-minute cron, and squatting on that port
        # makes its container fail to start and leaves it probing our core.
        "api": "http://127.0.0.1:19190",
        # Parallel egress-verification lanes: one select group and one loopback
        # inbound each, on base_port..base_port+lanes-1. The lane groups are
        # named "__LANE<i>__" by core.lane_group(); the single probe group this
        # replaced is gone, so nothing configures its name any more.
        "lanes": 8,
        "base_port": 19200,
        "container": os.environ.get("MIHOMO_TEST_CORE_CONTAINER", "mihomo-probe"),
        # The in-container path only. The host-side path is derived from
        # CORE_DIR where it is needed (see core.build_config), so a separate
        # key for it would be a switch that changes nothing.
        "container_config_path": "/root/.config/mihomo/config.yaml",
    },
    "sources": [
        {"key": "air", "kind": "collection", "name": "air", "label": "air", "enabled": True},
    ],
    "test": {
        "targets": [
            "http://connectivitycheck.platform.hicloud.com/generate_204",
            "https://www.gstatic.com/generate_204",
            "https://cp.cloudflare.com/generate_204",
        ],
        "expected_status": "204",
        "timeout_ms": 5000,
        "timeout_ms_retry": 9000,
        "concurrency": 20,
        "max_attempts": 3,
        "retry_pause_s": 0.3,
    },
    "dns": {
        # Resolve every domain from two vantages so both address sets get
        # tested: geo-DNS hands CN and overseas resolvers different answers,
        # and probing only the VPS's own view misses one of them entirely.
        "views": {
            "cn": {"resolver": "https://doh.pub/dns-query",
                   "ecs": "114.114.114.0/24", "ecs_prefix": 24},
            "overseas": {"resolver": "https://cloudflare-dns.com/dns-query",
                         "ecs": "8.8.8.8/24", "ecs_prefix": 24},
        },
        "timeout_s": 8,
        "cache_hours": 6,
    },
    "verify": {
        "enabled": True,
        "entry_check": True,
        "exclude_entry_countries": ["CN"],
        # any = 域名下任一可测 IP 活即算活; all = 全部可测 IP 都活才算活
        "domain_pass": "any",
        # 测试时剥离 ech-opts：mihomo 的 ECH 处理对 CF 前置节点会偶发 404
        # 且延迟翻倍（实测 4 轮 1 次 404），剥掉之后测的是节点本身而不是
        # 内核的 ECH 实现。代价：只能证明「非 ECH 路径」可用。
        "strip_ech": True,
        "trace_url": "https://www.cloudflare.com/cdn-cgi/trace",
        "exclude_countries": ["CN"],
        "max_nodes": 0,
        "timeout_s": 15,
    },
    "policy": {
        "drop_after_consecutive_fails": 3,
        "suspect_floor_ratio": 0.5,
        "suspect_floor_absolute": 3,
    },
    "schedule": {"interval_minutes": 30, "enabled": True},
    "publish": {
        "enabled": True,
        "push_to_substore": False,
        "prefix": "probe",
        "hostname": os.environ.get("MIHOMO_TEST_HOSTNAME", "probe.example.com"),
        "add_region_tag": True,
    },
    "watchdog": {"round_timeout_minutes": 20},
    "alert": {
        "enabled": False,
        "telegram": {"enabled": False, "token": "", "chat_id": ""},
        "webhook": {"enabled": False, "url": ""},
        "cooldown_minutes": 240,
        "alive_floor": 0,
    },
    "auth": {"token": os.environ.get("MIHOMO_TEST_TOKEN", "")},
    "ui": {"title": "mihomo 测活中心"},
}


def _deep_merge(base, override):
    """Return base updated by override, recursing into mappings."""
    out = copy.deepcopy(base)
    for key, value in (override or {}).items():
        if isinstance(value, dict) and isinstance(out.get(key), dict):
            out[key] = _deep_merge(out[key], value)
        else:
            out[key] = value
    return out


# Keys with no reader anywhere, removed from DEFAULTS so they stop being
# offered. Deliberately an explicit list rather than "everything not in
# DEFAULTS".
#
# DEFAULTS is NOT the schema, and treating it as one broke a live round on
# 2026-09-20: the deployed config.json carried `core.mixed_port`,
# `core.service` and `core.compose_file`, none of which are in DEFAULTS, and
# `core.build_config` reads `core_cfg["mixed_port"]` with a direct subscript --
# so pruning "unknown" keys raised KeyError and aborted round 99. A key can be
# read by code and absent from DEFAULTS; that is the normal shape of a
# deployment-specific override.
DEAD_KEYS = ("core.probe_group", "core.config_path")

# Keys discarded by the most recent load(), for the round to report.
_last_dropped = []


def take_dropped_keys():
    """Return and clear the keys the last load() discarded.

    Consume-once on purpose. The round loop reports these every round, but
    `load()` happens rarely -- the server caches its config in `_State.cfg` and
    the scheduler reuses that object. Returning the same list forever made every
    round re-announce keys the file had already been cleaned of, which reads as
    "your config still has dead switches" long after it does not.
    """
    out = list(_last_dropped)
    del _last_dropped[:]
    return out


def prune_dead(stored):
    """Drop the keys in DEAD_KEYS; return (kept, dropped).

    Only those exact paths: see the comment on DEAD_KEYS for why a
    DEFAULTS-based whitelist is not safe here.
    """
    kept, dropped = {}, []
    for key, value in (stored or {}).items():
        if not isinstance(value, dict):
            kept[key] = value
            continue
        sub = dict(value)
        for path in DEAD_KEYS:
            head, _, leaf = path.partition(".")
            if head == key and leaf in sub:
                sub.pop(leaf)
                dropped.append(path)
        if sub:
            kept[key] = sub
        elif value:
            # nothing left to merge; leaving `{"core": {}}` behind would be a
            # no-op that reads like a real setting
            pass
        else:
            kept[key] = value
    return kept, dropped


def load():
    """Read config.json merged over defaults; write it back when absent."""
    DATA.mkdir(parents=True, exist_ok=True)
    with _lock:
        if not CONFIG_PATH.exists():
            cfg = copy.deepcopy(DEFAULTS)
            if not cfg["auth"]["token"]:
                cfg["auth"]["token"] = secrets.token_hex(16)
            cfg["sources"] = normalize_sources(cfg["sources"])
            _write(cfg)
            return cfg
        try:
            stored = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            stored = {}
        stored, dropped = prune_dead(stored)
        _last_dropped[:] = dropped
        cfg = _deep_merge(DEFAULTS, stored)
        # Defensive: a hand-edited or older file may hold keys that would be
        # unsafe as file names.
        cfg["sources"] = normalize_sources(cfg.get("sources"))
        if dropped:
            # Persist the pruned form so the dead keys stop being rewritten by
            # every later save; the file converges on the schema.
            try:
                _write(cfg)
            except OSError:
                pass
        return cfg


def _write(cfg):
    tmp = CONFIG_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")
    tmp.replace(CONFIG_PATH)


def save(cfg):
    """Persist cfg atomically; return the value written."""
    DATA.mkdir(parents=True, exist_ok=True)
    with _lock:
        _write(cfg)
    return cfg


def reject_self_reference(sources, prefix):
    """Raise when a source would consume this system's own published output."""
    prefix = str(prefix or "").strip()
    if not prefix:
        return
    for entry in sources or []:
        if (entry.get("kind") == "collection"
                and str(entry.get("name") or "").strip() == prefix):
            raise ValueError(
                f"数据源 {entry.get('key')} 指向本系统自己的输出集合 {prefix}，"
                "会造成自我循环（自己的输出被当成输入再测一遍）。请在面板里取消勾选它。")


# Numeric settings that can wedge the service when given an extreme value, with
# the range they are clamped to. `schedule.interval_minutes` is the worst of
# them: a non-numeric value raised inside the scheduler, where the exception was
# swallowed, so the scheduler simply stopped firing and nothing said so.
NUMERIC_BOUNDS = {
    "schedule.interval_minutes": (1, 1440),
    "watchdog.round_timeout_minutes": (1, 180),
    "test.concurrency": (1, 200),
    "test.timeout_ms": (100, 60000),
    "test.timeout_ms_retry": (100, 60000),
    "test.max_attempts": (1, 10),
    "policy.drop_after_consecutive_fails": (1, 20),
}

# `auth_ok` passes on a falsy token, so an empty one turns every auth check off.
MIN_TOKEN_LEN = 16


def _get_path(cfg, path):
    node = cfg
    for part in path.split("."):
        if not isinstance(node, dict) or part not in node:
            return None
        node = node[part]
    return node


def _set_path(cfg, path, value):
    parts = path.split(".")
    node = cfg
    for part in parts[:-1]:
        node = node.setdefault(part, {})
    node[parts[-1]] = value


def validate_patch(patch):
    """Return (clean_patch, notes) for a config patch coming from the UI.

    Only the keys in DEAD_KEYS are dropped, plus out-of-range numbers clamped
    and a too-short `auth.token` refused. This deliberately does *not* enforce
    "must be in DEFAULTS": a deployment may legitimately carry keys the code
    reads but DEFAULTS never declared (`core.mixed_port` is one), and rejecting
    those would break a working install. See the DEAD_KEYS comment.

    Out-of-range numbers are clamped rather than rejected: an extreme value is
    usually a typo, and failing the whole save would throw away the user's other
    edits. Notes are returned so the caller can report what it changed.
    """
    clean, dropped = prune_dead(patch)
    notes = []
    if dropped:
        notes.append("忽略已废弃配置项: " + ", ".join(sorted(dropped)))
    for path, (low, high) in NUMERIC_BOUNDS.items():
        value = _get_path(clean, path)
        if value is None:
            continue
        try:
            number = int(value)
        except (TypeError, ValueError):
            _set_path(clean, path, _get_path(DEFAULTS, path))
            notes.append(f"{path} 不是数字，已还原为默认值")
            continue
        clamped = max(low, min(high, number))
        if clamped != number:
            _set_path(clean, path, clamped)
            notes.append(f"{path} 超出 [{low}, {high}]，已夹取为 {clamped}")
    if _get_path(clean, "auth.token") is not None \
            and len(str(_get_path(clean, "auth.token") or "")) < MIN_TOKEN_LEN:
        del clean["auth"]["token"]
        notes.append(f"auth.token 过短（<{MIN_TOKEN_LEN}）已忽略："
                     "空 token 会关闭全部鉴权")
    return clean, notes


def update(patch):
    """Merge a validated patch into the stored config; return the new config."""
    with _lock:
        cfg = _deep_merge(load(), patch)
        if "sources" in (patch or {}):
            cfg["sources"] = normalize_sources(cfg.get("sources"))
            reject_self_reference(cfg["sources"], cfg.get("publish", {}).get("prefix"))
        return save(cfg)


def core_secret():
    """Return the mihomo API secret, generating it on first use."""
    with _lock:
        if not CORE_SECRET_PATH.exists():
            CORE_SECRET_PATH.write_text(secrets.token_hex(16), encoding="utf-8")
            CORE_SECRET_PATH.chmod(0o600)
        return CORE_SECRET_PATH.read_text(encoding="utf-8").strip()


def tunnel_token():
    """Return the Cloudflare tunnel token, or an empty string."""
    try:
        return TUNNEL_TOKEN_PATH.read_text(encoding="utf-8").strip()
    except OSError:
        return os.environ.get("TUNNEL_TOKEN", "").strip()
