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


def load():
    """Read config.json merged over defaults; write it back when absent."""
    DATA.mkdir(parents=True, exist_ok=True)
    with _lock:
        if not CONFIG_PATH.exists():
            cfg = copy.deepcopy(DEFAULTS)
            cfg["sources"] = normalize_sources(cfg["sources"])
            cfg["chain"] = normalize_chain(cfg.get("chain"))
            _ensure_tokens(cfg)
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
        cfg["chain"] = normalize_chain(cfg.get("chain"))
        # Persist when the file was pruned *or* when a token had to be minted:
        # a token that only lives in this process would change on every
        # restart, breaking every URL that was handed out.
        if dropped or _ensure_tokens(cfg):
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
        if _get_path(clean, path) == SECRET_MASK:
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
