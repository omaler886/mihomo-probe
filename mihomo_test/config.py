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
import shutil
import threading
import time
import urllib.parse
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

# HK3 schema v3（指导 §3）。probes[].id 同时是 API 路径段和数据库主键，所以
# 限制为小写 slug；token 只允许用环境变量名引用，明文写进 config.json 会被
# /api/status 带出去、并复制进每一份文件备份。
PROBE_ID_RE = re.compile(r'^[a-z0-9][a-z0-9_-]{1,31}$')
ENV_NAME_RE = re.compile(r'^[A-Z][A-Z0-9_]*$')
PROBE_PLAINTEXT_TOKEN_KEYS = ("token", "secret", "api_key", "password")
# probes[] 条目的合法键（指导 §3）。白名单外的键一律拒绝：一个不在名单里的
# 敏感键名（authorization/apikey/...）会绕过明文字段拦截并随 /api/status
# 泄漏到前端。
PROBE_ENTRY_KEYS = frozenset({
    "id", "name", "enabled", "endpoint", "token_env", "weight", "tags",
})


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

    `export` defaults to true and is the only field that lets a source be
    tested without being published. It exists for sources worth *watching* --
    a flaky free subscription that occasionally yields one working node -- which
    should still be measured and shown on the dashboard, but must not hand that
    single node to every client. `enabled=False` cannot express this: it stops
    the testing too, and demotes the ledger to `unknown`.

    `relay` defaults to false and marks a source whose nodes are used as a
    *transit hop* rather than as an exit. It only feeds the dashboard's
    per-category statistics; it deliberately does not change what gets tested or
    published, so switching it can never silently drop a source's export.

    This function rebuilds each entry from a whitelist, so any key not named
    here is silently dropped on every `load()`/`update()`. That is the reason
    `relay` must be written out explicitly rather than left to survive by
    accident -- a field this function forgets is a field the panel saves
    successfully and then loses on the next boot.
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
            "export": entry.get("export") is not False,
            # `is True` rather than truthiness: a hand-edited "yes"/1 should read
            # as "not marked" instead of silently classifying a whole source as
            # transit and skewing every per-category number on the dashboard.
            "relay": entry.get("relay") is True,
            # Two independent measurement switches, both defaulting to on.
            #
            # They only change anything for a node carrying `dialer-proxy`:
            # a plain node is measured the same way either way (there is no
            # dialer to keep or strip), so leaving both on does not double the
            # round. A chained node is measured direct when `direct` is on
            # (dialer stripped -- it is then judged as its own server, which is
            # what "direct" means) and once per front when `chain` is on.
            #
            # `is not False` rather than truthiness, matching `enabled`/`export`:
            # a config written before this field existed reads as "on", so the
            # upgrade cannot silently stop measuring one of the two ways.
            "direct": entry.get("direct") is not False,
            "chain": entry.get("chain") is not False,
        })
    return out


# The front pool accepts pasted text as well as a Sub-Store resource, and that
# text goes straight into config.json. A cap keeps a stray paste (or a config
# written by something other than the panel) from turning into a multi-megabyte
# JSON blob that every `load()` then parses on the request path.
MAX_FRONT_TEXT = 262144
# Same reasoning for the pick list: it is names, not data.
MAX_FRONT_PICK = 500


def normalize_chain(block):
    """Repair the `chain` block: front_source, pasted text, and the pick list.

    Mirrors `normalize_sources`: an absent or malformed block becomes the
    disabled default rather than raising, because this runs on every load and a
    hand-edited config.json should not be able to stop the service from booting.

    Three ways to name the front pool, and they compose rather than exclude each
    other, because the pool is capped by `max_fronts` and the operator's intent
    is "use these, in this order":

    * `front_source` -- a Sub-Store resource, optionally narrowed by
    * `front_pick`   -- the display names to keep from it (empty means "all"),
    * `front_text`   -- share links or a base64 subscription body, pasted.

    `front_pick` is deduplicated with order preserved: `collect_fronts` walks it
    against the resource's own order, but a duplicate in the list would make the
    panel report a different count than the pool actually holds.
    """
    out = dict(block) if isinstance(block, dict) else {}
    ref = out.get("front_source")
    ref = dict(ref) if isinstance(ref, dict) else {}
    if ref.get("kind") not in SOURCE_KINDS:
        ref["kind"] = "sub"
    ref["name"] = str(ref.get("name") or "").strip()
    out["front_source"] = ref
    out["enabled"] = out.get("enabled") is True

    text = out.get("front_text")
    out["front_text"] = (text if isinstance(text, str) else "")[:MAX_FRONT_TEXT]

    picked, seen = [], set()
    raw_pick = out.get("front_pick")
    for item in (raw_pick if isinstance(raw_pick, list) else []):
        name = str(item or "").strip()
        if not name or name in seen:
            continue
        seen.add(name)
        picked.append(name)
        if len(picked) >= MAX_FRONT_PICK:
            break
    out["front_pick"] = picked
    return out


def front_source_name(block):
    """The Sub-Store resource named as the front pool; '' when unset."""
    return str(((block or {}).get("front_source") or {}).get("name") or "").strip()


def front_text(block):
    """The pasted front list, stripped; '' when unset."""
    return str((block or {}).get("front_text") or "").strip()


DEFAULTS = {
    # HK3 轻量化 schema 版本（指导 §3）。v2 及更早的 config.json 没有这个键，
    # load() 据此触发一次性迁移 migrate_config_v2_to_v3。
    "schema_version": 3,
    "substore": {
        # Sub-Store's backend URL can carry a secret path (this deployment's
        # does). It is deliberately NOT the default here: the default must stay
        # credential-free so nothing sensitive can be committed. Deployments
        # that need one set SUBSTORE_BACKEND in .env -- and once config.json
        # exists its stored value wins over this anyway.
        "backend": os.environ.get("SUBSTORE_BACKEND", "http://127.0.0.1:3000"),
        # 唯一 canonical 订阅对象名：Sub-Store 侧只更新这一个预绑定对象，不再
        # 按来源/探测点/轮次分叉出重复订阅（指导 §0/§8 去重保证）。
        "target_name": "mihomo-probe-alive",
        # 人工推送硬门禁（指导 §3）：Sub-Store 只允许用户在 Web 预览并二次确认
        # 后推送，scheduler / 轮次 hook / Gist publish 不得间接调用。取值不是
        # true 一律拒绝启动，见 validate_config_v3。
        "manual_push_only": True,
    },
    "core": {
        # Deliberately not 19090/19094: the legacy /srv/mihomo-health stack
        # binds 19090 on its own 30-minute cron, and squatting on that port
        # makes its container fail to start and leaves it probing our core.
        "api": "http://127.0.0.1:19190",
        # The kernel's HTTP inbound. This key deliberately lived only in the
        # deployed config.json until 2026-09-30 -- DEFAULTS had none and
        # `core.build_config` read it with a direct subscript -- so a fresh
        # install died on its first round with a bare KeyError before any node
        # was tested (ARCHITECTURE §mixed_port 陷阱). 19194 is the value the
        # deployment docs standardise on; MIHOMO_TEST_MIXED_PORT overrides.
        "mixed_port": int(os.environ.get("MIHOMO_TEST_MIXED_PORT", "19194")),
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
        # HTTPS targets first, the plain-HTTP one last. `engine.test_one`
        # re-partitions them anyway (an https:// target is tried before any
        # non-https one, whatever this order says) and only an HTTPS pass can
        # mark a node alive -- a bare 204 over http:// proves reachability, not
        # usability, and once published an HTTPS-broken node fails in real
        # use. The listed order is what the docs and the panel show.
        "targets": [
            "https://www.gstatic.com/generate_204",
            "https://cp.cloudflare.com/generate_204",
            "http://connectivitycheck.platform.hicloud.com/generate_204",
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
        # 链式真实拉流校验：延迟 204 只证明链路应答，不证明能跑真实 TLS 会话
        # （2026-09-28 家宽视角对比实测：延迟通过的节点真实拉流 TLS 握手全断，
        # 客户端的 gstatic health-check 有同样的盲区）。对判活的链式节点和前置
        # 经车道各拉一次真实页面；任何完成的 HTTP 响应都算通过，拨号错误/超时/
        # TLS 重置判 payload_fail（前置则连坐其链式变体为 front_dead）。
        "chain_payload": {
            "enabled": True,
            "url": "https://www.google.com/",
            "timeout_s": 15,
        },
    },
    # 链式代理测活。
    #
    # 上游订阅里带 `dialer-proxy` 的节点只有经前置才可能可用，当直连测会得到
    # 假活（内核确实拨得通那个地址，只是客户端不会那样拨）。开启后这类节点不再
    # 被当直连测：先测前置，再经「活着的前置」测链式；前置全死则该节点判失败，
    # 原因记为 `front_dead`，而不是一个它根本没拨过的 `timeout`。
    #
    # 前置池来自一个 Sub-Store 资源而不是写死的 URI：前置换 IP / 换域名 / 换
    # 协议时跟着 Sub-Store 自动更新，配置里不用动。实测这个前置必须带
    # `xhttp-opts.x-padding-*` 才拨得通（剥掉就 504），而 Sub-Store 渲染的
    # ClashMeta 会完整带上，所以直接引用即可。ECH 可以剥（实测剥了照样通），
    # 于是 `verify.strip_ech` 不需要为它开特例。
    "chain": {
        "enabled": False,
        "front_source": {"kind": "sub", "name": ""},
        # 从前置来源里挑出来的节点名；空 = 该来源全部节点。
        "front_pick": [],
        # 手动粘贴的前置：分享链接（`vless://…` 每行一条）或 base64 订阅内容。
        # 交给 Sub-Store 解析（见 engine.manual_fronts），本服务不自己写解析器。
        "front_text": "",
        # 前置池上限。链式节点的测试量是「节点数 × 前置数 × 地址数」，前置池
        # 是乘数里最容易被无意放大的那个（一个 CF 前置订阅能轻松列出几十条）。
        "max_fronts": 8,
        # 全节点链式（客户端路径）模式：上游不带 dialer-proxy 的普通节点也经前置池
        # 测链式变体，且不发直连孪生——账本判活口径=客户端真实口径。动因：消费端
        # （air 集合）会把所有节点强制挂到 CDN 前置后面，而直连测通≠过链能通
        # （2026-09-29 家宽对比：155 个直连判活节点 24 个过链即死）。开启后直连
        # 信息从账本退位；首轮存活数可能下跌触发护栏，属预期。需先开启链式。
        "test_plain_nodes": False,
    },
    # 多探测点统一测活参数（指导 §3）。engine 在 v3 仍读既有 policy/test 键，
    # 这里是由它们派生出的单一事实来源，v4 再收口（plan §4.1）。
    "probe_policy": {
        "interval_minutes": 30,
        "timeout_ms": 5000,
        "concurrency": 20,
        "dead_after_consecutive_failures": 3,
        "round_guard_ratio": 0.5,
        "round_guard_min_alive": 3,
    },
    # 探测点注册表。空数组 = 本地单探测点兼容模式（隐含的 "local" 探测点），
    # 节点结论与改造前一致（指导 §4.3、§16 灰度第 1 步）。
    "probes": [],
    # 多探测点结果聚合默认值（指导 §4.3）：任一探测点成功即活；全部失败且
    # 达到最小完成探测点数才判败；不足则 inconclusive，不推进 consec_fail。
    "aggregation": {
        "strategy": "any_alive",
        "minimum_completed_probes": 1,
        "late_result_seconds": 90,
    },
    # 节点重命名（指导 §6）：发生在聚合与死亡收敛之后、生成 canonical export
    # 之前；账本身份始终用连接参数指纹，改名只影响显示名。
    "rename": {
        "enabled": True,
        "template": "{country} | {display_name}",
        "collision_suffix": " #{n}",
        "preserve_original_name": True,
    },
    # Gist 发布中间层（指导 §8）：一个 Gist 只维护固定文件，禁止按探测点 /
    # 轮次 / 来源无限新建。token 仅从 token_env 指定的环境变量读取，日志全
    # 脱敏。
    "gist": {
        "enabled": True,
        "gist_id": "",
        "token_env": "GITHUB_GIST_TOKEN",
        "visibility": "secret",
        "canonical_file": "mihomo-alive.yaml",
        "status_file": "probe-status.json",
        "history_file": "probe-history.jsonl",
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
        # Migration-window escape hatch (workstreams/18). The refactor's rule
        # is that a round must never CREATE a Sub-Store object: every repeated
        # run of the old pipeline forked one logical subscription into many
        # products. `push_to_substore` only decides whether the legacy write
        # path runs at all; this flag decides whether it may still run, and
        # even then it can only UPDATE a pre-bound canonical sub. Env
        # `ALLOW_LEGACY_SUBSTORE_WRITE=1` opens it without editing this file,
        # which is what a rollback needs; `=0` closes it again during an
        # incident.
        "allow_legacy_substore_write": False,
        "prefix": "probe",
        "hostname": os.environ.get("MIHOMO_TEST_HOSTNAME", "probe.example.com"),
        "add_region_tag": True,
        # Read-only credential for the export endpoints, kept separate from
        # `auth.token` on purpose. The export URL is pasted into Sub-Store and
        # rendered into the dashboard, so it leaks by design -- and until this
        # existed it leaked the *admin* token, which on this deployment is also
        # the ability to drive the host's docker daemon. Generated on first
        # load when empty; see `load()`.
        "token": "",
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
    # 允许跨域调用本 API 的前端来源（也就是部署在 CDN 上那份静态前端）。
    #
    # 默认空数组 = 只有同源页面能用。这不是"保守"，是唯一安全的默认：同源部署
    # 根本不需要 CORS，而一个默认放行的白名单等于把面板连同它的令牌接口交给
    # 任何一个网站。要用 CDN 前端就显式填上那个来源。
    #
    # 比对是**精确匹配 origin**（含协议与端口，忽略结尾斜杠），不做通配 ——
    # 通配子域意味着任何一个 `evil.example.com` 都能读到带令牌的响应。
    "server": {"cors_origins": []},
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

# Paths a config patch may not touch, whatever it says.
#
# `core.container` is the one that matters: the app holds a mounted
# /var/run/docker.sock and passes this value straight to `docker restart` /
# `docker start` / `docker logs` (see core.Core). Leaving it writable through
# POST /api/config turns any token leak into "stop an arbitrary container on
# the host", which is a different order of problem from "someone can read my
# node list". Deployments set it with MIHOMO_TEST_CORE_CONTAINER instead --
# that is read into DEFAULTS, so it still works, it just is not remotely
# editable any more.
IMMUTABLE_PATHS = ("core.container",)

# The mask server.redacted_config writes for these paths in the /api/status
# payload. A patch carrying the mask back means "unchanged": validate_patch
# drops the key instead of writing it, so a settings save that round-trips the
# redacted payload cannot overwrite a real credential with the mask. Without
# this, masking the polling payload would have corrupted the values on the
# first settings save (the form reads the redacted copy back).
MASKABLE_PATHS = ("auth.token", "publish.token", "alert.telegram.token",
                  "alert.webhook.url", "substore.backend")
SECRET_MASK = "***"

# Keys discarded by the most recent load(), for the round to report.
_last_dropped = []

# What the one-shot v2 -> v3 migration did on the most recent load(), for the
# round to report (指导 §5 wants the operator told that chain came back
# closed). Consume-once, same reasoning as take_dropped_keys.
_last_migration = []


def take_migration_report():
    """Return and clear the migration notes from the last load()."""
    out = list(_last_migration)
    del _last_migration[:]
    return out


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


def _ensure_tokens(cfg):
    """Guarantee a usable admin token and a read-only publish token.

    Called on *every* load, not just on the file-absent path. The original
    version generated a token only when config.json did not exist, so a file
    that existed but could not be parsed (`except (OSError, ValueError):
    stored = {}`) fell through to DEFAULTS with `auth.token` empty -- and
    `server.auth_ok` treats an empty token as "no auth configured" and lets
    everything through. On this deployment that is an unauthenticated public
    dashboard (the tunnel is the only ingress) that also hands out
    `/api/export/*.yaml`, i.e. every live node's credentials, and accepts
    POST /api/config. A truncated file after a full disk or an interrupted
    write is enough to get there.

    Returns True when something was generated, so the caller can persist it.
    """
    changed = False
    for path in ("auth.token", "publish.token"):
        current = str(_get_path(cfg, path) or "").strip()
        if len(current) >= MIN_TOKEN_LEN:
            continue
        _set_path(cfg, path, secrets.token_hex(16))
        changed = True
    return changed


def validate_config_v3(cfg):
    """The one schema gate for HK3 v3: startup, save and pre-test (指导 §3).

    Raises ValueError with an actionable message on the first violation, so a
    refused boot/file says what to change. Returns cfg with `chain.enabled`
    forced to a real bool, so callers can chain `validate_config_v3(load())`.

    The config layer only *validates*: it never touches node streaks. The
    death threshold is copied across so the engine keeps converging at exactly
    the value the operator already had.
    """
    if not isinstance(cfg, dict):
        raise ValueError("配置必须是 JSON 对象")

    # substore.manual_push_only：人工推送硬门禁。缺失同样拒绝——一个没有表态
    # 的配置文件不能在升级后被解释成"允许自动推送"。
    substore = cfg.get("substore")
    if not isinstance(substore, dict) or substore.get("manual_push_only") is not True:
        raise ValueError(
            "substore.manual_push_only 必须为 true：Sub-Store 只允许在 Web 界面"
            "预览并二次确认后人工推送（指导 §3）。请把该键改回 true，或删除它"
            "以取默认值。")

    # probes：本地单探测点兼容模式是空列表，本身合法。
    probes = cfg.get("probes", [])
    if not isinstance(probes, list):
        raise ValueError("probes 必须是数组（空数组 = 本地单探测点兼容模式）")
    seen = set()
    for index, entry in enumerate(probes):
        if not isinstance(entry, dict):
            raise ValueError(f"probes[{index}] 必须是对象")
        # 键白名单（指导 §3 的探测点形状）。只拦四个已知敏感键名不够：
        # 运维手写 `authorization`/`apikey`/`access_key` 会过闸并随
        # /api/status 到前端。白名单 fail-closed——未列出的键一律拒绝，
        # 与 normalize_sources 的白名单重建同范式。
        unknown = sorted(set(entry) - PROBE_ENTRY_KEYS)
        if unknown:
            raise ValueError(
                f"probes[{index}] 含未知键 {unknown}：只允许 "
                f"{sorted(PROBE_ENTRY_KEYS)}（token 只能经 token_env 引用"
                "环境变量，指导 §3）")
        probe_id = entry.get("id")
        if not isinstance(probe_id, str) or not PROBE_ID_RE.match(probe_id):
            raise ValueError(
                f"probes[{index}].id 非法: {probe_id!r}"
                "（需匹配 ^[a-z0-9][a-z0-9_-]{1,31}$）")
        if probe_id in seen:
            raise ValueError(
                f"probes[].id 重复: {probe_id}（同一探测点只能注册一次）")
        seen.add(probe_id)
        for bad in PROBE_PLAINTEXT_TOKEN_KEYS:
            if bad in entry:
                raise ValueError(
                    f"probes[{index}] 不允许包含明文 {bad}：token 只能通过"
                    " token_env 引用环境变量（指导 §3）")
        token_env = entry.get("token_env")
        if token_env and (not isinstance(token_env, str)
                          or not ENV_NAME_RE.match(token_env)):
            raise ValueError(
                f"probes[{index}].token_env 必须是环境变量名"
                f"（^[A-Z][A-Z0-9_]*$）: {token_env!r}")

    # 同一条规则管 Gist token：配置里只放环境变量名。
    gist = cfg.get("gist")
    if isinstance(gist, dict):
        token_env = gist.get("token_env")
        if token_env and (not isinstance(token_env, str)
                          or not ENV_NAME_RE.match(token_env)):
            raise ValueError(
                f"gist.token_env 必须是环境变量名（^[A-Z][A-Z0-9_]*$）:"
                f" {token_env!r}")

    # dead_after_consecutive_failures：缺失时从既有 policy 阈值派生，绝不在此
    # 重置任何节点的 streak（配置层只校验不清账，指导 §3/§7.8）。
    policy_block = cfg.get("policy") if isinstance(cfg.get("policy"), dict) else {}
    probe_policy = cfg.get("probe_policy")
    if not isinstance(probe_policy, dict):
        probe_policy = {}
        cfg["probe_policy"] = probe_policy
    if probe_policy.get("dead_after_consecutive_failures") is None:
        legacy = policy_block.get("drop_after_consecutive_fails")
        if isinstance(legacy, int) and not isinstance(legacy, bool) and legacy >= 1:
            probe_policy["dead_after_consecutive_failures"] = legacy
        else:
            probe_policy["dead_after_consecutive_failures"] = \
                DEFAULTS["probe_policy"]["dead_after_consecutive_failures"]
    threshold = probe_policy["dead_after_consecutive_failures"]
    if not isinstance(threshold, int) or isinstance(threshold, bool) or threshold < 1:
        raise ValueError("probe_policy.dead_after_consecutive_failures 必须为正整数")

    # chain.enabled 缺失强制 false（指导 §3/§5）：与 normalize_chain 的 `is
    # True` 语义一致，只有显式 true 才算开启。
    chain = cfg.get("chain")
    if not isinstance(chain, dict):
        chain = {}
        cfg["chain"] = chain
    chain["enabled"] = chain.get("enabled") is True
    return cfg


def _v3_template():
    """The v3-only slice of DEFAULTS: the base a v2 config is migrated onto."""
    return {
        "schema_version": 3,
        "probe_policy": copy.deepcopy(DEFAULTS["probe_policy"]),
        "probes": copy.deepcopy(DEFAULTS["probes"]),
        "aggregation": copy.deepcopy(DEFAULTS["aggregation"]),
        "rename": copy.deepcopy(DEFAULTS["rename"]),
        "gist": copy.deepcopy(DEFAULTS["gist"]),
        "substore": {
            "target_name": DEFAULTS["substore"]["target_name"],
            "manual_push_only": DEFAULTS["substore"]["manual_push_only"],
        },
    }


def migrate_config_v2_to_v3(cfg):
    """One-shot v2 -> v3 migration (指导 §3/§15 Phase 2).

    Returns (new_cfg, report). Pure: backup and persistence are the caller's
    job (load() does both). An explicitly written value always wins over a
    derived default; only missing keys are filled, so re-running it on an
    already-migrated config is a no-op. Node streaks are untouched -- the
    death threshold is copied from the policy value the engine already uses.
    """
    old = copy.deepcopy(cfg) if isinstance(cfg, dict) else {}
    report = []
    new = _deep_merge(_v3_template(), old)
    new["schema_version"] = 3
    if old.get("schema_version") != 3:
        report.append(f"schema_version: {old.get('schema_version', '缺失')} -> 3")

    # probe_policy 从既有 test/policy/schedule 值派生；已显式写过的
    # probe_policy 键优先（半 v3 配置不被回退）。
    test_cfg = old.get("test") if isinstance(old.get("test"), dict) else {}
    policy_cfg = old.get("policy") if isinstance(old.get("policy"), dict) else {}
    schedule_cfg = old.get("schedule") if isinstance(old.get("schedule"), dict) else {}
    old_pp = old.get("probe_policy") if isinstance(old.get("probe_policy"), dict) else {}
    probe_policy = dict(DEFAULTS["probe_policy"])
    derived = {
        "interval_minutes": schedule_cfg.get("interval_minutes"),
        "timeout_ms": test_cfg.get("timeout_ms"),
        "concurrency": test_cfg.get("concurrency"),
        "dead_after_consecutive_failures": policy_cfg.get("drop_after_consecutive_fails"),
        "round_guard_ratio": policy_cfg.get("suspect_floor_ratio"),
        "round_guard_min_alive": policy_cfg.get("suspect_floor_absolute"),
    }
    legacy_paths = {
        "interval_minutes": "schedule.interval_minutes",
        "timeout_ms": "test.timeout_ms",
        "concurrency": "test.concurrency",
        "dead_after_consecutive_failures": "policy.drop_after_consecutive_fails",
        "round_guard_ratio": "policy.suspect_floor_ratio",
        "round_guard_min_alive": "policy.suspect_floor_absolute",
    }
    for key, value in derived.items():
        if key in old_pp or value is None:
            continue
        probe_policy[key] = value
        report.append(f"probe_policy.{key} <- {legacy_paths[key]}（{value}）")
    probe_policy.update(old_pp)
    new["probe_policy"] = probe_policy

    # chain.enabled 缺失 -> false（指导 §5）：升级旧配置即使曾配过前置，也先
    # 迁移为关闭，并在报告里提示用户手动开启。已经是 false 的配置再跑一遍
    # 迁移不产生新报告——迁移必须可重复执行且安静。
    chain = new.get("chain")
    if not isinstance(chain, dict):
        chain = {}
        new["chain"] = chain
    if chain.get("enabled") is not True:
        if "enabled" not in chain:
            report.append("chain.enabled 缺失，已置 false：如需链式测活请在面板"
                          "手动开启（指导 §5）")
            chain["enabled"] = False
        elif chain["enabled"] is not False:
            report.append(f"chain.enabled 取值 {chain['enabled']!r} 不是 true，"
                          "已归一为 false")
            chain["enabled"] = False

    # substore 补 target_name / manual_push_only。
    substore = new.get("substore")
    if not isinstance(substore, dict):
        substore = {}
        new["substore"] = substore
    if "target_name" not in substore:
        substore["target_name"] = DEFAULTS["substore"]["target_name"]
        report.append(f"substore.target_name 缺失，已补 {substore['target_name']!r}")
    if substore.get("manual_push_only") is not True:
        substore["manual_push_only"] = True
        report.append("substore.manual_push_only 缺失或非 true，已置 true"
                      "（指导 §3 人工推送硬门禁）")

    for key in ("probes", "aggregation", "rename", "gist"):
        if key not in old:
            report.append(f"{key} 段缺失，已补 v3 默认值")
    return new, report


def _backup_config_v3_once():
    """Snapshot config.json as config.json.bak-v3-<UTC> before the first v3
    migration. Returns the backup path, or None when there is nothing to do.

    Once only: an existing .bak-v3-* means an earlier run already kept the
    pre-migration copy, and copying again would overwrite that snapshot with
    the already-migrated file. Best effort -- a backup we cannot take must not
    block startup.
    """
    try:
        if not CONFIG_PATH.exists():
            return None
        if list(CONFIG_PATH.parent.glob(CONFIG_PATH.name + ".bak-v3-*")):
            return None
        stamp = time.strftime("%Y%m%dT%H%M%S", time.gmtime())
        backup = CONFIG_PATH.with_name(f"{CONFIG_PATH.name}.bak-v3-{stamp}")
        shutil.copy2(CONFIG_PATH, backup)
        return backup
    except OSError:
        return None


def load():
    """Read config.json merged over defaults; write it back when absent."""
    DATA.mkdir(parents=True, exist_ok=True)
    with _lock:
        if not CONFIG_PATH.exists():
            cfg = copy.deepcopy(DEFAULTS)
            cfg["sources"] = normalize_sources(cfg["sources"])
            cfg["chain"] = normalize_chain(cfg.get("chain"))
            _ensure_tokens(cfg)
            validate_config_v3(cfg)
            _write(cfg)
            return cfg
        try:
            stored = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            stored = {}
        stored, dropped = prune_dead(stored)
        _last_dropped[:] = dropped
        # One-shot v2 -> v3 migration (指导 §3): schema_version absent or below
        # 3 means the file predates the unique-config model. Back the file up
        # first (once), then migrate. Node streaks are not touched here -- the
        # death threshold is carried across so convergence continues unchanged.
        migrated = False
        version = stored.get("schema_version")
        if not isinstance(version, int) or isinstance(version, bool) or version < 3:
            _backup_config_v3_once()
            stored, migration_report = migrate_config_v2_to_v3(stored)
            _last_migration[:] = migration_report
            migrated = True
        cfg = _deep_merge(DEFAULTS, stored)
        # Defensive: a hand-edited or older file may hold keys that would be
        # unsafe as file names.
        cfg["sources"] = normalize_sources(cfg.get("sources"))
        cfg["chain"] = normalize_chain(cfg.get("chain"))
        # The legacy Sub-Store write switch is also an environment variable,
        # so a rollback never has to edit this file under pressure and an
        # incident can close the path again with `=0`. A blank value leaves
        # the stored decision alone. See DEFAULTS["publish"].
        env_legacy = os.environ.get("ALLOW_LEGACY_SUBSTORE_WRITE", "").strip().lower()
        if env_legacy:
            cfg["publish"]["allow_legacy_substore_write"] = env_legacy in ("1", "true", "yes", "on")
        tokens_minted = _ensure_tokens(cfg)
        # The one schema gate, run last so it sees the fully merged config
        # (指导 §3): a failure here refuses startup rather than booting on a
        # config the rules forbid.
        validate_config_v3(cfg)
        # Persist when the file was pruned, when a token had to be minted, or
        # when a migration ran: a token that only lives in this process would
        # change on every restart, and a migrated file left un-migrated on disk
        # would re-migrate (and re-report) on every boot.
        if migrated or dropped or tokens_minted:
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
    """Raise when a source would consume this system's own published output.

    Only an *enabled* entry is a problem. Checking every entry regardless made
    the check unsatisfiable in the one case it matters: a config that lists
    `collection/<prefix>` with the box unticked was rejected on every save, so
    the operator could not tick it back off -- the only way out of the state
    the error message tells them to get out of. A disabled entry is inert:
    `engine.export_keys` and the round both walk `enabled` sources only.
    """
    prefix = str(prefix or "").strip()
    if not prefix:
        return
    for entry in sources or []:
        if (entry.get("kind") == "collection"
                and entry.get("enabled") is True
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
    # Multiplies every chained node's test count, so an extreme value here is a
    # round that never finishes rather than a wrong answer.
    "chain.max_fronts": (1, 64),
    # These two were absent, and `notifier.send` does `int(cooldown_minutes)`
    # without a guard: a non-numeric value raised inside `_maybe_alert`, which
    # runs *after* the round has already been recorded and published, so the
    # exception travelled up to `run_round`'s handler and stamped a successful
    # round as "aborted".
    "alert.cooldown_minutes": (1, 10080),
    "alert.alive_floor": (0, 100000),
    # The kernel's inbound is a network listener: a typo like 191940 or a
    # negative value must clamp, not wedge `build_config`.
    "core.mixed_port": (1024, 65535),
}

# `auth_ok` now *denies* on a falsy token (it used to pass, which turned an
# empty value into "auth disabled"), so this is a second line of defence: a
# token short enough to guess is no better than none.
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


def _pop_path(cfg, path):
    """Remove one dotted path, tolerating absent sections."""
    parts = path.split(".")
    node = cfg
    for part in parts[:-1]:
        if not isinstance(node, dict) or part not in node:
            return
        node = node[part]
    if isinstance(node, dict):
        node.pop(parts[-1], None)


def _is_secret_mask(path, value):
    """Return whether a submitted value is an unchanged credential placeholder."""
    if value == SECRET_MASK:
        return True
    if path != "substore.backend" or not isinstance(value, str):
        return False
    try:
        parsed = urllib.parse.urlsplit(value)
        return (parsed.scheme in ("http", "https") and bool(parsed.hostname)
                and parsed.path == "/" + SECRET_MASK and not parsed.query
                and not parsed.fragment and parsed.username is None)
    except ValueError:
        return False


def validate_patch(patch):
    """Return (clean_patch, notes) for a config patch coming from the UI.

    Only the keys in DEAD_KEYS are dropped, plus IMMUTABLE_PATHS, plus
    out-of-range numbers clamped and too-short tokens refused. This
    deliberately does *not* enforce "must be in DEFAULTS": a deployment may
    legitimately carry keys the code reads but DEFAULTS never declared
    (`core.mixed_port` is one), and rejecting those would break a working
    install. See the DEAD_KEYS comment.

    Out-of-range numbers are clamped rather than rejected: an extreme value is
    usually a typo, and failing the whole save would throw away the user's other
    edits. Notes are returned so the caller can report what it changed.
    """
    clean, dropped = prune_dead(patch)
    notes = []
    if dropped:
        notes.append("忽略已废弃配置项: " + ", ".join(sorted(dropped)))
    for path in IMMUTABLE_PATHS:
        head, _, leaf = path.partition(".")
        section = clean.get(head)
        if isinstance(section, dict) and leaf in section:
            section.pop(leaf)
            if not section:
                del clean[head]
            notes.append(f"忽略不可远程修改的配置项: {path}"
                         "（改用环境变量 MIHOMO_TEST_CORE_CONTAINER）")
    for path in MASKABLE_PATHS:
        # The settings form round-trips the redacted /api/status payload; a
        # mask arriving here is the form saying "this field was not edited",
        # so the stored value must survive the save untouched.
        if _is_secret_mask(path, _get_path(clean, path)):
            _pop_path(clean, path)
            notes.append(f"{path} 为掩码占位，已按未改动处理")
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
    # An empty target list is not a valid setting: `engine.test_one` raises
    # ValueError on it, so every scheduled round afterwards would die in the
    # worker pool and be recorded as an exception, with nothing in the panel
    # saying why. Refuse the value instead of accepting a config that cannot run.
    targets = _get_path(clean, "test.targets")
    if targets is not None and not [t for t in targets if str(t).strip()]:
        _set_path(clean, "test.targets", list(DEFAULTS["test"]["targets"]))
        notes.append("test.targets 不能为空，已还原为默认测试目标")
    # `chain.front_text` is refused rather than truncated when it is absurdly
    # long. Truncating a base64 body yields something that still looks like a
    # subscription and parses into garbage nodes, so the failure would surface
    # as "the front pool is empty" with nothing pointing at the paste.
    text = _get_path(clean, "chain.front_text")
    if isinstance(text, str) and len(text) > MAX_FRONT_TEXT:
        clean["chain"].pop("front_text", None)
        if not clean["chain"]:
            del clean["chain"]
        notes.append(f"chain.front_text 超过 {MAX_FRONT_TEXT} 字符，已忽略（原值保留）")
    for path in ("auth.token", "publish.token"):
        value = _get_path(clean, path)
        if value is None:
            continue
        if len(str(value or "").strip()) < MIN_TOKEN_LEN:
            del clean[path.split(".")[0]][path.split(".")[1]]
            notes.append(f"{path} 过短（<{MIN_TOKEN_LEN}）已忽略："
                         "空 token 会关闭全部鉴权")
    return clean, notes


def update(patch):
    """Merge a validated patch into the stored config; return the new config."""
    with _lock:
        cfg = _deep_merge(load(), patch)
        if "sources" in (patch or {}):
            cfg["sources"] = normalize_sources(cfg.get("sources"))
            reject_self_reference(cfg["sources"], cfg.get("publish", {}).get("prefix"))
        if "chain" in (patch or {}):
            cfg["chain"] = normalize_chain(cfg.get("chain"))
        # The same schema gate the startup path runs (指导 §3): a patch that
        # would store a forbidden config is refused here instead of being
        # written and only failing on the next boot.
        validate_config_v3(cfg)
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
