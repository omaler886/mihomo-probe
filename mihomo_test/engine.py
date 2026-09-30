"""One testing round: fetch sources, test through the kernel, converge, publish."""
import calendar
import ipaddress
import collections
import hashlib
import json
import os
import re
import threading
import time
import urllib.parse
import urllib.request

import yaml

from . import config as cfgmod
from . import core as coremod
from . import doh as dohmod
from . import db
from . import notifier
from . import policy
from .store import Client, NotFound, StoreError

ROUND_STATE = cfgmod.DATA / "round.state.json"


class RoundTimeout(RuntimeError):
    """A round exceeded its budget; the lock is released so the schedule lives."""


def _write_state(phase, round_id=None, mode=None):
    """Record what a round is doing, for a human looking at the filesystem.

    Nothing in the program reads anything here except `round_id` (see
    `_abandon_round`) and `mode` (see `server.current_mode`, which shows the
    running round's 直连/链式 on the dashboard). `phase`, `pid`, `ts` and
    `epoch` are for an operator running `cat data/round.state.json` while a
    round looks stuck -- which is the whole reason the file exists.

    An earlier version of this docstring claimed `epoch` was used for liveness
    checks and that `ts`/`epoch` disagreeing was the root cause of a stale file
    being misattributed to the wrong round. That is no longer true and has not
    been for a while: ownership is decided by `_state_belongs_to()`, which asks
    the database whether the round is still open. Do not "fix" a ts/epoch
    mismatch -- it would be changing a field nobody reads.

    `mode` is persisted rather than held only on the in-process runner because
    the dashboard is served by the same process but a *stuck* round is
    inspected via this file. Keeping one copy means the two can never disagree.
    """
    try:
        cfgmod.DATA.mkdir(parents=True, exist_ok=True)
        tmp = ROUND_STATE.with_suffix(".json.tmp")
        tmp.write_text(json.dumps({
            "phase": phase, "round_id": round_id, "pid": os.getpid(),
            "ts": time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()),
            "epoch": time.time(), "mode": mode,
        }, ensure_ascii=False), encoding="utf-8")
        tmp.replace(ROUND_STATE)
    except OSError as exc:
        # Reported rather than swallowed. This file is how a crashed round
        # recovers its own round id, so a silent write failure is what produces
        # a ghost row: `finished_at` stays NULL forever, and
        # `_previous_alive_count` skips unfinished rows, which quietly moves the
        # guardrail's baseline to a much older round. `_CURRENT_ROUND_ID` now
        # covers the same crash in-process, but an operator still needs to know
        # the file is not being written.
        try:
            db.log("warn", f"轮次状态文件写入失败（{exc}）")
        except Exception:  # noqa: BLE001 - already on a failure path; a logging
            pass            # failure must not turn a write problem into a crash


def _budget(cfg):
    try:
        return max(1, int(cfg.get("watchdog", {}).get("round_timeout_minutes", 20))) * 60
    except (TypeError, ValueError):
        return 20 * 60


def _checkpoint(deadline, phase, round_id=None):
    """Raise when the round has overrun its budget."""
    # `_CURRENT_ROUND_MODE` rather than a parameter: there are four call sites
    # and the mode is a property of the round, not of the phase. Threading it
    # through each one by hand is how a call site gets missed and the dashboard
    # flips back to "unknown mode" halfway through a round -- the file is
    # rewritten at every phase, so a single missed argument is visible.
    _write_state(phase, round_id, _CURRENT_ROUND_MODE)
    if deadline is not None and time.monotonic() > deadline:
        raise RoundTimeout(f"round exceeded its budget at phase {phase}")

EXPORT_DIR = cfgmod.DATA / "exports"
# Everything we may have prepended on an earlier round, so re-tagging is
# idempotent: any number of leading `[XX]` tags, in any order, with or without
# a flag emoji between them.
#
# The original pattern matched exactly one `[XX]` and required the name to
# start with `[`, so a name that had already been through the flag operator
# (`🇭🇰 [HK] Foo`) did not match at all -- the tag was appended instead of
# replaced. Inside a source that feeds itself (a collection whose members
# include our own export) that leaks one tag per round: the live `air`
# collection had `[HK] 🇭🇰 [HK]  [HK]  HK-Kwu...` after three passes.
#
# A flag is two regional-indicator code points; `\s*` (not `\s+`) so
# `[HK][HK] Foo` collapses too.
_LEADING_TAG = re.compile(
    r"^(?:"
    r"\[[A-Z]{2}(?:-[A-Za-z0-9]+)?\]\s*"       # a region tag: [HK], [US-CA]
    r"|[\U0001F1E6-\U0001F1FF]{2}\s*"          # a flag emoji: 🇭🇰
    r")+"
)

# Deterministic failures: testing again with the same parameters cannot
# change the answer, so the retry budget is not spent on them.
#
# `controller_error` is deliberately absent. It means our own kernel API could
# not be reached, which is transient and says nothing about the node, so it is
# worth spending a retry on.
TERMINAL_REASONS = {"bad_request", "bad_delay", "bad_response", "unreachable"}

# Chain support. A node that carries `dialer-proxy` is only reachable through
# the front that field names, so testing it direct answers a question nobody
# asked: the kernel really can dial that address from this box, and the client
# really will not.
DIALER_FIELD = "dialer-proxy"
# The ledger source the front pool is filed under. It is not a user source, so
# it never gets an export file, but it does get ledger rows -- otherwise the
# front's own health would be invisible and its streak would reset every round.
FRONT_SOURCE_KEY = "__front__"

# A ledger fingerprint is exactly 16 hex chars -- tests/test_live.py asserts it,
# and that length check is how the old name-keyed scheme is detected. The direct
# twin of a chained node therefore gets a *derived* fingerprint
# (`core.variant_fingerprint`), not a decorated one. A real one would not work:
# `fingerprint_proxy` ignores `dialer-proxy`, so the stripped proxy hashes to the
# very same value as the chained one and the two measurements would silently
# collapse into a single ledger row.
# Kernel-facing name prefix for a front. Reserved so a chained variant's
# `dialer-proxy` cannot accidentally resolve to one of the user's own nodes.
FRONT_NAME_PREFIX = "__FRONT"
# Recorded instead of the `timeout` a real dial would produce, because the
# chain was never dialled -- the front was already known dead. The verdict is
# still a failure: a chain whose front carries nothing is not usable, and that
# is the requested rule. Only the reason differs, so the panel blames the front
# rather than the node.
FRONT_DEAD_REASON = "front_dead"

# The front pool can be filled from three places (see `collect_fronts`), and the
# pasted one is materialised into a local Sub-Store subscription so that
# Sub-Store parses it. This suffix names that subscription.
MANUAL_FRONT_SUFFIX = "-front-manual"
# Pasted text -> the digest last written to Sub-Store, per subscription name.
# Keyed by name and not a single slot so that two different prefixes (two
# deployments sharing one Sub-Store) cannot think each other's write is theirs.
# It is a cache, not state: losing it costs one idempotent upsert.
_MANUAL_FRONT_SYNCED = {}
# Names whose stale subscription has already been removed in this process, so
# the "text is empty" path does not issue a DELETE every single round.
_MANUAL_FRONT_CLEANED = set()

# The three kinds of node the dashboard reports separately. They are derived
# per node, not per source, because one source can yield all three: `air`
# supplies the chained nodes whose `dialer-proxy` points at the front pool.
#
# `CAT_RELAY` is the transit hop itself -- a node whose only job is to carry
# someone else's traffic. It is decided by `relay` on the source, and by
# `chain.front_source`, because the front pool is a transit hop whether or not
# anyone remembered to mark its source.
#
# A node can only be in one category. The precedence is relay > chain > direct:
# a front that also carries `dialer-proxy` is still a front, because that is the
# role it plays in the chain this system measures.
CAT_DIRECT = "direct"
CAT_RELAY = "relay"
CAT_CHAIN = "chain"
CATEGORIES = (CAT_DIRECT, CAT_RELAY, CAT_CHAIN)
CATEGORY_LABELS = {CAT_DIRECT: "直连节点", CAT_RELAY: "中转节点", CAT_CHAIN: "链式代理"}

_round_lock = threading.Lock()

# The round this process currently has open. `_abandon_round` used to recover
# the id solely from round.state.json, so a crash on a machine where that file
# could not be written left the ledger row open forever. This is the in-process
# record that does not depend on the filesystem.
_CURRENT_ROUND_ID = None

# The mode of the round in flight, for the same reason as the id above: the
# dashboard asks "what is running right now" and the answer must not depend on
# re-reading a file that a crash may have left behind. Cleared alongside the id
# and only ever set under the round lock, so it belongs to exactly one round.
_CURRENT_ROUND_MODE = None


def _set_current_round(round_id, mode=None):
    global _CURRENT_ROUND_ID, _CURRENT_ROUND_MODE
    _CURRENT_ROUND_ID = round_id
    _CURRENT_ROUND_MODE = mode


def current_mode():
    """The mode of the round in flight, or None when nothing is running.

    Public because `server.status_payload` needs it and reaching into the
    private global from another module is how the two get out of step.
    """
    return _CURRENT_ROUND_MODE


class Busy(RuntimeError):
    pass


def test_one(core, entry, test_cfg, deadline=None):
    """Test one node, retrying only where a retry can plausibly help.

    `deadline` is a `time.monotonic()` instant. It is checked before every
    attempt, not only between phases: `_test_all` is by far the longest stage
    of a round (worst case ~51s per node at max_attempts=3 with the retry
    timeout), so a checkpoint that only runs after the stage returns cannot
    interrupt it. With concurrency 20 and 500 address variants that stage alone
    can exceed the whole 20-minute budget and keep going, while the scheduler
    is blocked by the round lock and the panel reports "正在测试" the entire
    time. Overrun is now bounded by a single attempt.

    Only an HTTPS target can produce an "alive" verdict. The default target
    list once opened with a plain-HTTP connectivity endpoint, and its rotation
    meant the first success short-circuited the loop: a node could be published
    on the strength of an `http://` 204 alone, then fail every HTTPS site in
    real use (measured on the live CDN-front chains on 2026-09-28: 8 of 16
    alive chained nodes had never been HTTPS-verified). The plain-HTTP target
    is still dialled -- it is the only CN-reachable one, and its outcome is
    diagnostic -- but its success merely notes "HTTP 通" and the loop keeps
    hunting for an HTTPS pass; if none arrives the node fails on its last
    HTTPS outcome.

    HTTPS targets are also tried first, whatever order the config lists them
    in: a plain-HTTP target can never rescue an HTTPS failure, so spending an
    attempt on it before the HTTPS ones are exhausted would waste the very
    retry the HTTPS targets needed.
    """
    urls = test_cfg["targets"]
    if not urls:
        raise ValueError("no test targets configured")
    # Stable partition: every https:// target before every non-https one.
    urls = ([u for u in urls if str(u).startswith("https://")]
            + [u for u in urls if not str(u).startswith("https://")])
    https_required = str(urls[0]).startswith("https://")
    expected = str(test_cfg.get("expected_status", "204"))
    max_attempts = max(1, int(test_cfg.get("max_attempts", 3)))
    base_timeout = int(test_cfg.get("timeout_ms", 5000))
    long_timeout = int(test_cfg.get("timeout_ms_retry", base_timeout))
    pause = float(test_cfg.get("retry_pause_s", 0.3))

    timeout = base_timeout
    attempts = 0
    delay = None
    reason = "unknown"
    detail = ""
    url = urls[0]
    http_pass = None      # (delay, url): a plain-HTTP success, not a verdict
    last_https = None     # (reason, detail) of the latest failed HTTPS attempt
    last_fail = None      # ultimate fallback when nothing HTTPS was attempted
    while attempts < max_attempts:
        if deadline is not None and time.monotonic() > deadline:
            raise RoundTimeout(f"round exceeded its budget while testing {entry['mihomo']}")
        url = urls[attempts % len(urls)]
        attempts += 1
        delay, reason, detail = core.delay(entry["mihomo"], url, timeout, expected)
        if reason is None:
            if not https_required or url.startswith("https://"):
                return {"delay_ms": delay, "reason": None, "detail": "",
                        "attempts": attempts, "url": url}
            http_pass = (delay, url)
        else:
            last_fail = (reason, detail)
            if not https_required or url.startswith("https://"):
                last_https = (reason, detail)
            if reason in TERMINAL_REASONS:
                break
            if reason == "timeout":
                # A timeout is the one failure a bigger budget can overturn.
                timeout = long_timeout
        if attempts < max_attempts:
            time.sleep(pause)
    if last_https is None:
        # Only reachable when the config has no https:// target at all -- then
        # `https_required` is False and the loop returned above -- or every
        # attempt failed before any HTTPS target was dialled.
        reason, detail = last_fail or (reason, detail)
    elif http_pass is not None:
        reason, detail = last_https
        detail = (f"{detail}；plain-HTTP 探测点 {http_pass[1]} 通（{http_pass[0]}ms），"
                  "但 HTTPS 未通过，不判活").strip("；")
    else:
        reason, detail = last_https
    return {"delay_ms": delay, "reason": reason, "detail": detail,
            "attempts": attempts, "url": url}



def _drop_self_references(cfg, sources, log):
    """Refuse to test our own published collection.

    Enabling the output collection as an input makes the pipeline consume its
    own result: every surviving node is tested twice per round and appears
    twice on the dashboard, and a node that dies stays dead through both paths.
    """
    prefix = str(cfg.get("publish", {}).get("prefix") or "").strip()
    if not prefix:
        return sources
    kept, dropped = [], []
    for source in sources:
        if source.get("kind") == "collection" and str(source.get("name")) == prefix:
            dropped.append(source)
        else:
            kept.append(source)
    for source in dropped:
        log("warn", f"数据源 {source.get('key')} 指向本系统自己的输出集合 {prefix}，已跳过（自我循环）")
    return kept


def _is_literal_ip(server):
    try:
        ipaddress.ip_address(server)
        return True
    except ValueError:
        return False


def lookup_countries(ips, log):
    """Batch country lookup, backed by a persistent sqlite cache.

    ip-api.com's batch endpoint takes 100 IPs a call; caching means each IP is
    paid for once ever, so a 300-node list costs a handful of requests on the
    first round and zero afterwards.

    Call this once per round with every candidate address, not once per server:
    the request count is what the endpoint rate-limits, and the caller has
    already grouped the addresses for us.
    """
    cached = db.ip_geo_get(ips)
    todo = [ip for ip in ips if ip not in cached]
    for start in range(0, len(todo), 90):
        chunk = todo[start:start + 90]
        rows, error = _fetch_country_batch(chunk)
        if rows is None:
            # Report every address still undecided, not just this chunk: a
            # failure stops the sequence, so the chunk size understates what
            # went unclassified -- which is the number an operator needs in
            # order to judge how much of the round lost its entry filter.
            log("warn", f"入口 IP 归属查询失败（{error}），"
                        f"本轮跳过入口过滤，{len(todo) - start} 个地址未定性")
            return cached
        # the batch endpoint echoes the address back in the `query` field
        normalised = [{"ip": row.get("query"), "country": row.get("countryCode"),
                       "isp": row.get("isp")} for row in rows if row.get("query")]
        db.ip_geo_put(normalised)
        for row in normalised:
            cached[row["ip"]] = row.get("country")
    return cached


BATCH_URL = "http://ip-api.com/batch?fields=query,countryCode,isp"
# One retry, short pause. A 429 is a rate-limit answer rather than a permanent
# failure, so it is worth asking once more -- but only once: the caller has
# already stopped filtering this round either way, and hammering the endpoint
# is what turns a transient limit into a recurring one.
BATCH_RETRY_PAUSE_S = 2.0


def _fetch_country_batch(chunk, attempts=2):
    """POST one chunk to ip-api; return (rows, None) or (None, error text).

    Returning a value rather than raising is deliberate -- classification is
    skipped for the round instead of wrongly excluding nodes on missing data.
    Reporting is left to the caller, which is the only place that knows how
    many addresses the failure actually stranded.
    """
    body = json.dumps(chunk, separators=(",", ":")).encode("utf-8")
    attempts = max(1, attempts)
    last = None
    for attempt in range(attempts):
        req = urllib.request.Request(
            BATCH_URL, data=body, headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=45) as resp:
                return json.load(resp), None
        except Exception as exc:  # noqa: BLE001 - urllib raises many types
            last = exc
            if attempt + 1 < attempts:
                time.sleep(BATCH_RETRY_PAUSE_S)
    return None, str(last)[:80]


def classify_and_expand(entries, cfg, log):
    """Resolve domains from both vantages and build per-address tests.

    Geo-DNS hands CN and overseas resolvers different address sets, and
    round-robin may return several per query -- testing only the kernel's own
    resolution samples one address and calls the node dead when a client would
    have gotten a live one. Every distinct address becomes its own test here;
    the ledger and the export keep the original domain form.

    Addresses on a restricted ISP (CN by default) are skipped rather than
    counted as dead: from this vantage point they cannot be tested at all, and
    a domain whose every address is restricted is excluded outright.

    Entries marked `no_expand` (the front pool) are passed through untouched.
    A front is a dialer, not a destination: what matters is whether the kernel
    can open a connection *through* it, and mihomo resolves the front's own
    domain when it dials. Splitting it per address would also make front
    liveness ambiguous, because a chained variant names exactly one of those
    addresses as its dialer -- so one dead address of a healthy front would
    read as a dead front.
    """
    keep = [entry for entry in entries if entry.get("no_expand")]
    entries = [entry for entry in entries if not entry.get("no_expand")]
    verify_cfg = cfg.get("verify", {})
    if not verify_cfg.get("entry_check", True):
        return keep + list(entries), []
    banned = {c.upper() for c in verify_cfg.get("exclude_entry_countries", ["CN"]) or []}
    dns_cfg = cfg.get("dns", {})
    try:
        cache_s = max(0, int(dns_cfg.get("cache_hours", 6) or 0)) * 3600
    except (TypeError, ValueError):
        cache_s = 6 * 3600

    groups = collections.defaultdict(list)
    for entry in entries:
        server = str(entry["proxy"].get("server") or "").strip()
        groups[server or "?"].append(entry)

    # Resolve every server first, then look all the addresses up in one batch.
    #
    # The lookup used to sit inside this loop, so it ran once per server per
    # round: hundreds of small requests at a rate-limited endpoint. ip-api
    # answers that with HTTP 429, and a 429 here does not fail loudly -- it
    # silently switches entry filtering off for the whole round. Hoisting the
    # lookup makes the cost one batched sequence per round no matter how many
    # servers the subscriptions list.
    resolved, pending_ips = [], []
    for server, group in groups.items():
        candidates = _resolve_candidates(server, dns_cfg, cache_s, log)
        resolved.append((group, candidates))
        for ip in candidates:
            if ip not in pending_ips:
                pending_ips.append(ip)

    countries = lookup_countries(pending_ips, log) if pending_ips else {}

    test_entries, excluded = [], []
    for group, candidates in resolved:
        if not candidates:
            # Neither vantage resolved it: let the kernel try its own path.
            # `group` already carries the fp computed in `collect_entries`, so
            # these nodes keep the same ledger identity as resolved ones.
            test_entries += group
            continue
        testable = [ip for ip in candidates
                    if (countries.get(ip) or "").upper() not in banned]
        if not testable:
            for entry in group:
                excluded.append(entry)
            continue
        for entry in group:
            orig_fp = entry["fp"]
            for ip in testable:
                variant = dict(entry["proxy"])
                variant["server"] = ip
                test_entries.append({**entry, "proxy": variant, "fp": orig_fp,
                                     "orig_proxy": entry["proxy"], "test_ip": ip})
    return keep + test_entries, excluded


def _resolve_candidates(server, dns_cfg, cache_s, log):
    """Every distinct address a server name resolves to, from both vantages.

    A literal address is its own single candidate. For a hostname the answer
    comes from the two-view DNS cache when fresh, otherwise from a fresh
    resolve whose result is cached only when at least one view answered --
    caching an all-empty result would pin an unresolvable name for the whole
    cache window.
    """
    if _is_literal_ip(server):
        return [server]
    views = db.domain_views_get(server, cache_s) if cache_s else None
    if views is None:
        try:
            views = dohmod.resolve_views(server, dns_cfg.get("views"),
                                         int(dns_cfg.get("timeout_s", 8) or 8))
        except Exception as exc:
            log("warn", f"{server}: 视角解析异常（{str(exc)[:60]}）")
            views = {}
        if any(views.values()):
            db.domain_views_put(server, views)
    candidates, seen = [], set()
    for label in sorted(views):
        for ip in views.get(label) or []:
            if ip not in seen:
                seen.add(ip)
                candidates.append(ip)
    return candidates


def _orig_fp(proxy):
    """Fingerprint of the original (domain-form) node, matching the ledger.

    This is the ONLY place a ledger identity is derived. Everything downstream
    -- `classify_and_expand`, `core.prepare`, `_record_excluded_nodes` -- must
    reuse the `fp` carried on the entry rather than recomputing one, because
    recomputation happens on a *transformed* proxy.

    That distinction is not cosmetic. `core.prepare` strips `dialer-proxy` and
    friends, optionally strips `ech-opts`, and coerces `port` to an int before
    it used to fall back to `fingerprint_proxy(proxy)`. So a node with
    `port: "443"` or with `ech-opts` present got a DIFFERENT fingerprint on any
    round where the domain resolved from neither DNS vantage (the
    `test_entries += group` path, which carries no fp) -- and
    `_prune_removed_nodes` treats the fingerprints seen this round as the
    whitelist, so every round like that DELETED the node's ledger row,
    resetting `consec_fail` to zero. With DoH flapping, a node that should
    converge to "dead after 3 consecutive failures" never gets past 1.

    The same mismatch also changed what got published: the fallback path set
    `orig_proxy` to the already-stripped proxy, so a chained node lost its
    `dialer-proxy` and was exported as an unusable config.
    """
    return coremod.fingerprint_proxy(
        {k: v for k, v in proxy.items() if k not in coremod.DROP_FIELDS})


def classify_category(proxy, source_relay=False):
    """Which of the three kinds of node this is: direct, relay, or chain.

    Derived from the node itself plus one flag about its source, so it needs no
    extra state to survive a round. The precedence matters: a node that is both
    a marked transit hop and carries `dialer-proxy` is a *relay*, because that
    is the role it plays in the chain being measured -- reporting it as a chain
    would double-count it and make the chain total disagree with the front pool.

    `dialer-proxy` is checked in both spellings because mihomo normalises `_`
    to `-` and upstream subscriptions use both.
    """
    if source_relay:
        return CAT_RELAY
    if DIALER_FIELD in proxy or "dialer_proxy" in proxy:
        return CAT_CHAIN
    return CAT_DIRECT


def collect_entries(store, sources):
    """Fetch every enabled source and flatten it into testable entries.

    Each entry is tagged with its `category` here, while the source's `relay`
    flag is still in hand. `core.prepare` only receives proxies and entries, so
    a category computed later would have lost the source-level signal and would
    have to re-derive it from the node alone -- which cannot tell a marked
    transit hop from an ordinary node.
    """
    entries, errors = [], []
    index = 0
    for source in sources:
        if not source.get("enabled", True):
            continue
        kind = source.get("kind", "collection")
        try:
            proxies = store.fetch_source(kind, source["name"])
        except (StoreError, yaml.YAMLError) as exc:
            errors.append(f"{source['key']}: {exc}")
            continue
        relay_source = source.get("relay") is True
        for proxy in proxies:
            name = str(proxy.get("name") or f"node-{index}")
            entries.append({"source": source["key"], "name": name, "proxy": proxy,
                            "index": index, "fp": _orig_fp(proxy),
                            "category": classify_category(proxy, relay_source)})
            index += 1
    return entries, errors


def chain_block(cfg):
    """The `chain` settings when chaining is on and a front pool is named.

    Returning None for the half-configured cases is deliberate: "enabled but no
    front source" would otherwise make every chained node fail with
    `front_dead` because of a missing config field, which reads as a network
    problem. `run_round` reports the misconfiguration instead.

    A pool counts as named when *either* input is set -- a Sub-Store resource
    or pasted text -- because they are two ways to fill the same pool, not two
    features. Requiring the resource would make a manual-only pool read as
    "chaining is off" and silently turn 链式测活 into a direct round.
    """
    block = cfg.get("chain") or {}
    if block.get("enabled") is not True:
        return None
    if not cfgmod.front_source_name(block) and not cfgmod.front_text(block):
        return None
    return block


def manual_front_sub_name(cfg):
    """The Sub-Store subscription the pasted front list is materialised into."""
    prefix = str((cfg.get("publish") or {}).get("prefix") or "probe")
    return f"{prefix}{MANUAL_FRONT_SUFFIX}"


def manual_fronts(cfg, store, log):
    """Parse the pasted front list into proxies, through Sub-Store.

    The pasted text is either share links (`vless://…`, one per line) or a
    base64 subscription body, and Sub-Store already parses every dialect of
    both. Writing our own link parser would be a second and worse
    implementation of a job that is one HTTP call away -- and the one that
    would rot, because the dialects change and Sub-Store's does not.

    So the text is upserted as a local subscription and read back as ClashMeta.
    The upsert is skipped when the text is unchanged since the last one, so a
    steady pool costs no writes; the cache is per process and losing it only
    costs one idempotent upsert.

    An empty text deletes the subscription instead of leaving it behind: its
    content is embedded at write time, so a leftover copy would go on offering
    fronts the operator removed, and it would show up in the panel's resource
    list as something nobody can explain.
    """
    block = chain_block(cfg) or {}
    text = cfgmod.front_text(block)
    name = manual_front_sub_name(cfg)
    if not text:
        _drop_manual_front_sub(store, name, log)
        return []
    digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
    if _MANUAL_FRONT_SYNCED.get(name) != digest:
        payload = {
            "name": name,
            "displayName": "手动前置（测活中心）",
            "source": "local",
            "url": "",
            "content": text,
            "mergeSources": "",
            "ignoreFailedRemoteSub": "quiet",
            "passThroughUA": False,
            "process": [],
        }
        try:
            action = store.upsert("sub", name, payload)
        except StoreError as exc:
            log("error", f"手动前置写入 Sub-Store 失败（{name}）: {exc}")
            return []
        _MANUAL_FRONT_SYNCED[name] = digest
        _MANUAL_FRONT_CLEANED.discard(name)
        log("info", f"手动前置已写入 Sub-Store 订阅 {name}（{action}）")
    try:
        return store.fetch_sub_proxies(name)
    except (StoreError, yaml.YAMLError) as exc:
        log("error", f"手动前置订阅 {name} 解析失败: {exc}")
        return []


def _drop_manual_front_sub(store, name, log):
    """Remove the materialised manual pool once the pasted text is cleared."""
    if name in _MANUAL_FRONT_CLEANED:
        return
    _MANUAL_FRONT_CLEANED.add(name)
    if _MANUAL_FRONT_SYNCED.pop(name, None) is None:
        # Never written by this process. It may still be a leftover from a
        # previous one, so ask Sub-Store to drop it anyway -- a 404 means there
        # was nothing to clean, which is the common case.
        pass
    try:
        store.delete("sub", name)
        log("info", f"手动前置已清空，删除 Sub-Store 订阅 {name}")
    except NotFound:
        pass
    except StoreError as exc:
        log("warn", f"清理手动前置订阅 {name} 失败: {exc}")


def drop_manual_front_sub(cfg, store, log=None):
    """Remove the materialised manual pool; the config-save path calls this.

    `manual_fronts` does the same thing on its own path, but a save that clears
    the paste *and* switches chaining off never reaches it again -- and the
    subscription's content is embedded at write time, so leaving it would keep
    offering fronts the operator removed.
    """
    _drop_manual_front_sub(store, manual_front_sub_name(cfg), log or db.log)


def collect_fronts(cfg, store, log):
    """Fetch the front pool: the nodes a chained node dials through.

    The pool comes from a Sub-Store resource rather than a URI baked into the
    config, so a front that changes address or protocol is picked up by
    Sub-Store and needs no edit here.

    Three inputs fill it, in this order, and `max_fronts` caps the result:

    1. `front_text`  -- pasted share links or base64, parsed by Sub-Store.
    2. `front_pick`  -- names selected out of the resource, when set.
    3. `front_source` -- the whole resource.

    Pasted entries come first because they are the operator's explicit,
    just-typed choice; a resource is the standing default. When `front_pick` is
    set the resource is *narrowed* rather than supplemented: the operator said
    which nodes to use, and silently keeping the rest would make the pool
    depend on an order they cannot see.

    Each front is one entry, whatever it resolves to -- see
    `classify_and_expand` for why a front is not expanded per address.
    """
    block = chain_block(cfg)
    # Before the early return: clearing the paste *and* the front source in one
    # edit makes `chain_block` None, and a `-front-manual` sub left behind would
    # keep serving fronts the operator removed (its content is embedded).
    if not cfgmod.front_text(block or {}):
        _drop_manual_front_sub(store, manual_front_sub_name(cfg), log)
    if block is None:
        return []
    ref = block["front_source"] or {}
    kind = ref.get("kind") if ref.get("kind") in cfgmod.SOURCE_KINDS else "sub"
    name = cfgmod.front_source_name(block)
    picked = [str(x) for x in (block.get("front_pick") or [])]

    resource = []
    if name:
        try:
            resource = store.fetch_source(kind, name)
        except (StoreError, yaml.YAMLError) as exc:
            log("error", f"前置来源 {kind}/{name} 拉取失败: {exc}")
            resource = []
        if picked:
            wanted = set(picked)
            kept = [p for p in resource if str(p.get("name") or "") in wanted]
            missing = len(wanted) - len({str(p.get("name") or "") for p in kept})
            log("info", f"前置来源 {kind}/{name} 按名单取 {len(kept)}/{len(resource)} 条"
                        + (f"（{missing} 个名单节点不在该来源里）" if missing else ""))
            resource = kept
    elif picked:
        log("warn", "chain.front_pick 有名单但没有前置来源，已忽略")

    manual = manual_fronts(cfg, store, log)
    proxies = manual + resource
    try:
        cap = max(1, min(64, int(block.get("max_fronts", 8) or 8)))
    except (TypeError, ValueError):
        cap = 8
    fronts, dropped = [], 0
    for i, proxy in enumerate(proxies):
        if len(fronts) >= cap:
            dropped += 1
            continue
        display = str(proxy.get("name") or f"front-{i}")
        # The kernel-facing name is reserved and position-derived: a chained
        # variant's `dialer-proxy` must resolve to the front and never to a
        # user's node that happens to share its display name.
        kernel_name = f"{FRONT_NAME_PREFIX}{len(fronts)}__"
        fronts.append({
            "source": FRONT_SOURCE_KEY,
            "name": display,
            "proxy": {**proxy, "name": kernel_name},
            "index": i,
            "fp": _orig_fp(proxy),
            "orig_proxy": dict(proxy),
            "role": "front",
            # The pool is a transit hop by definition, regardless of whether its
            # source was marked `relay`. Deciding it here rather than from the
            # source means the category stays right even when the same resource
            # is also listed as an ordinary source.
            "category": CAT_RELAY,
            "no_expand": True,
        })
    parts = []
    if manual:
        parts.append(f"手动 {len(manual)} 条")
    if name:
        parts.append(f"{kind}/{name}{'（按名单）' if picked else ''}")
    origin = " + ".join(parts) or "（未配置）"
    if dropped:
        log("warn", f"前置池 {origin} 有 {dropped} 条超出 chain.max_fronts={cap}，已忽略")
    if not fronts:
        log("error", f"前置池 {origin} 没有可用前置，链式节点本轮全部判失败")
    else:
        log("info", f"前置池 {len(fronts)} 条（{origin}）："
                    + "、".join(f["name"] for f in fronts[:6])
                    + ("…" if len(fronts) > 6 else ""))
    return fronts


def _measure_flags(source_key, source_flags):
    """The (direct, chain) measurement pair for one source.

    `source_flags is None` means "no per-source policy this round": the caller
    keeps the pre-existing behaviour, which is chain-only.

    Both-off falls back to direct rather than testing nothing: a node this
    round never measures is a node `_prune_removed_nodes` deletes from the
    ledger, so an operator un-ticking both boxes would silently lose the
    source's history instead of just changing how it is measured.
    """
    if not source_flags:
        # No policy at all: keep the historical behaviour (chain-only), so a
        # caller that never learned about these switches is unaffected. A 直连
        # 测活 round reaches `expand_chains` with an empty front pool and
        # returns before this matters.
        return False, True
    direct, chain = source_flags.get(source_key, (True, True))
    if not direct and not chain:
        direct = True
    return bool(direct), bool(chain)


def expand_chains(entries, front_names, source_flags=None, fail_without_front=False,
                  plain_too=False):
    """Give every chained node one test variant per front.

    `source_flags` maps a source key to its (direct, chain) switches; see
    `_measure_flags`. A source with `chain` off does not get front variants at
    all -- its dialer is stripped and it is measured as its own server. A
    source with `direct` on *in addition to* chain gets a stripped variant
    alongside the chained ones, and that variant is keyed by the fingerprint of
    its stripped self: both variants describe the same node, so without a
    distinct identity the ledger would fold them into one row and the two
    measurements would overwrite each other.

    A node carrying `dialer-proxy` is tested once per front, and every variant
    keeps the node's fingerprint, so the existing per-fingerprint aggregation
    decides it without knowing anything about chains: one front carrying the
    traffic is enough, and every front failing fails the node. That is the rule
    we want -- a dead front fails its chain rather than excusing it.

    Runs after `classify_and_expand`, so a chained node is already one entry per
    resolved address and the expansion multiplies fronts on top of that.

    An empty pool means three different things to three callers, and only one
    of them is a failure:

    * `source_flags is None` -- a 直连测活 round. Every node is measured as its
      own server on purpose, so the entries pass through untouched and the
      kernel strips the dialer.
    * `fail_without_front is False` -- chaining is off or half-configured for
      this round. Falling through to a direct measurement is the documented
      behaviour (the round logs a warning saying so).
    * `fail_without_front is True` -- this round promised to measure chains and
      the pool came back empty. The chained node must then be *failed*, never
      measured direct: stripping the dialer is exactly what `core.prepare`
      documents as "reports a node as alive on a path its owner never uses".
      It is emitted with `role="chain"` and `front=None` so `_test_phases`
      routes it into `chain_failed` without dialling it.

    `plain_too` extends all of the above to nodes the upstream ships *without*
    a `dialer-proxy`: each gets front variants (carrying the pool's dialer, so
    the export can publish the chained form) and -- deliberately -- no direct
    twin, because the mode exists to make the ledger's verdict the *client's*
    verdict. See the in-loop comment for the measurement that motivated it.
    """
    if not front_names and not (source_flags is not None and fail_without_front):
        return entries, 0
    out, chained = [], 0
    for entry in entries:
        proxy = entry["proxy"]
        plain = DIALER_FIELD not in proxy
        if entry.get("role") == "front":
            out.append(entry)
            continue
        if plain and not plain_too:
            out.append(entry)
            continue
        # A node from a relay-flagged source is a *front*, not a passenger: it
        # is already the hop a chain dials through. Expanding it would give it a
        # `dialer-proxy` pointing at another front, which is not a topology this
        # deployment has -- and because the ledger folds by fingerprint, the
        # variant's `chain` category would overwrite the relay classification
        # `classify_category` already committed to, so the 分类统计 panel would
        # report the front pool as chain traffic and the two totals would stop
        # reconciling. Leave it exactly as the classifier found it.
        if entry.get("category") == CAT_RELAY:
            out.append(entry)
            continue
        direct, chain = _measure_flags(entry["source"], source_flags)
        if plain:
            # Client-path mode (`plain_too`): a node the upstream ships without
            # a dialer is measured exactly the way the consuming client uses it
            # -- dialled through the front pool. Measured on 2026-09-29, 24 of
            # 155 probe-alive nodes were dead through exactly that path while
            # their direct dial from the vantage succeeded, because the client
            # (the `air` collection) forces every node behind a CDN front the
            # probe's direct dial never traverses. No direct twin is emitted on
            # purpose: the ledger verdict for this node IS the client's verdict,
            # so a node that only answers as its own server does not get
            # rescued into the export by its direct twin and shipped wearing a
            # chain that failed. A source with its `chain` switch off keeps the
            # old direct-only measurement.
            if not chain:
                out.append(entry)
                continue
            direct = False
        if chain:
            chained += 1
            if not front_names:
                # Chain round, empty pool: no dial, no verdict from a dial --
                # `_test_phases` fails it with `front_dead` instead. The dialer
                # stays on the proxy on purpose: `prepare` strips whatever is
                # not in `keep_dialer`, and here that is everything, so the
                # kernel never sees a dangling reference.
                out.append({**entry, "role": "chain", "front": None,
                            "category": CAT_CHAIN})
            for front in front_names:
                variant = dict(proxy)
                variant[DIALER_FIELD] = front
                # The variant ends up chained, whatever the original node was
                # classified as: once a dialer is attached, the node is reachable
                # only through it, and that is what `chain` means here. The
                # fingerprint is deliberately *not* changed, so all variants still
                # fold into one ledger row -- and that row's category must therefore
                # be the chain one, which is what this sets.
                out.append({**entry, "proxy": variant, "role": "chain",
                            "front": front, "category": CAT_CHAIN})
        if direct:
            # Measured as its own server: the dialer is the whole difference
            # between the two, so dropping it is what "直连测活" means. `prepare`
            # would strip it anyway (the value is kept only when it names a real
            # front), but doing it here keeps `category` honest for the
            # 分类统计 panel, which reads the entry rather than the config.
            stripped = {k: v for k, v in proxy.items() if k != DIALER_FIELD}
            extra = {}
            if chain:
                # Both ways are measured, so the two rows must not share a
                # fingerprint -- that is what keeps the two measurements from
                # overwriting each other in the ledger. It has to be a *derived*
                # one: `fingerprint_proxy` ignores `dialer-proxy`, so hashing the
                # stripped proxy yields the chained node's own fingerprint and
                # the two variants collapse into one row. `variant_fingerprint`
                # also keeps it 16 hex, which the ledger asserts.
                #
                # The base is the entry's fp, never a re-hash of `stripped`.
                # `classify_and_expand` has already run, so `proxy["server"]`
                # is a *resolved address* here (see that function's docstring),
                # and `_orig_fp(stripped)` would hash the address instead of the
                # node: the twin's identity would then change every time the
                # domain resolved to a new address set, `_prune_removed_nodes`
                # would drop the old row, and `consec_fail` would reset to zero
                # -- the same incident `_orig_fp`'s own docstring records, this
                # time arriving from the other side. `entry["fp"]` is the domain
                # form the ledger and the export already speak, so it is stable
                # across addresses and across DNS flapping.
                #
                # Only when both are on: with `chain` off there is a single
                # variant, and re-keying it would orphan every ledger row the
                # source already has (the old fingerprint stops appearing, so
                # `_prune_removed_nodes` deletes the history).
                extra["fp"] = coremod.variant_fingerprint(
                    entry.get("fp") or _orig_fp(stripped), "direct")
            out.append({**entry, "proxy": stripped, "role": "direct",
                        "category": CAT_DIRECT, **extra})
    return out, chained


def round_uses_chains(cfg, mode=None):
    """Whether this round should dial chained nodes through the front pool.

    The dashboard's manual 直连测活 button passes ``mode="direct"``: the round
    then skips the front pool entirely, so `core.prepare` strips `dialer-proxy`
    and every node is measured as its own server (a chained node that is only
    reachable through its front reads as 假活 -- that is the requested "direct"
    measurement). Every other mode (the 链式测活 button's ``"chain"`` and the
    scheduler's ``None``) chains exactly as configured.

    Both modes still test *every* enabled source: a round that skipped nodes
    would let `_prune_removed_nodes` delete their ledger rows and
    `_publish_sources` truncate their export files. Only the front dialling
    differs.
    """
    if mode == "direct":
        return False
    return chain_block(cfg) is not None


def run_round(cfg, trigger="manual", only_source=None, mode=None, log=db.log):
    """Execute one full round; returns a summary dict.

    Guarded by both an in-process lock and a file lock: the CLI and the
    service are separate processes, and letting them overlap tests the same
    nodes twice, which advances the convergence counter twice in one round.

    `mode` is ``None`` for a complete chain-aware round (what the scheduler
    runs), ``"direct"`` for 直连测活 (chaining off), or ``"chain"`` for
    链式测活. See `round_uses_chains`.
    """
    if not _round_lock.acquire(blocking=False):
        raise Busy("a round is already running")
    handle = None
    try:
        handle = _acquire_file_lock()
        return _run_round(cfg, trigger, only_source, log, mode)
    except RoundTimeout as exc:
        log("error", str(exc))
        _abandon_round(cfg, round_id=_CURRENT_ROUND_ID, why=str(exc), log=log,
                       alert_key="round_timeout", title="轮次超时，已中止并释放调度锁")
        return {"error": "round timeout", "detail": str(exc)}
    except Exception as exc:
        log("error", f"本轮异常: {type(exc).__name__}: {exc}")
        _abandon_round(cfg, round_id=_CURRENT_ROUND_ID,
                       why=f"{type(exc).__name__}: {exc}", log=log,
                       alert_key="round_failed", title="轮次执行异常")
        raise
    finally:
        # Release the in-process lock unconditionally.
        #
        # The file-lock release runs first, and if it raised, the in-process
        # lock stayed held for the lifetime of the process: every later round
        # then failed with Busy, `server.run_in_background` swallowed that
        # silently, and the service stopped testing anything while still
        # looking perfectly idle. Nesting the releases makes the in-process
        # lock impossible to strand.
        try:
            if handle is not None:
                _release_file_lock(handle)
        finally:
            _round_lock.release()
            _set_current_round(None)
        try:
            ROUND_STATE.unlink()
        except OSError:
            pass


def _acquire_file_lock():
    try:
        import fcntl
    except ImportError:  # non-POSIX dev machine; the service is Linux
        return None
    cfgmod.DATA.mkdir(parents=True, exist_ok=True)
    handle = open(cfgmod.DATA / "round.lock", "w")
    try:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        handle.close()
        raise Busy("another round is already running in a different process") from None
    return handle


def _release_file_lock(handle):
    """Best-effort unlock. Must never raise: it runs inside a `finally`.

    An exception escaping here used to skip the caller's in-process lock
    release, which wedged the scheduler permanently -- see the `finally` in
    `run_round`. Failures are therefore caught, but still reported: a silent
    swallow here would hide an fd leak or a lock that never went away.
    """
    try:
        import fcntl

        fcntl.flock(handle, fcntl.LOCK_UN)
    except ImportError:
        pass  # non-POSIX dev machine; the service is Linux
    except OSError as exc:
        db.log("warn", f"释放轮次文件锁失败（{exc}），依赖进程内锁兜底")
    try:
        handle.close()
    except OSError as exc:
        db.log("warn", f"关闭轮次锁文件失败（{exc}）")


def _reconcile_ledger(cfg, sources, fronts, chain_configured, log):
    """Drop ledger rows for sources that are gone, then demote disabled ones.

    Split out of `_run_round` so the front-pool rule can be tested without
    booting a kernel. That rule is subtle enough to have already produced one
    real outage: it must keep the front pool while chaining is *configured*,
    not merely while fronts were collected, because a 直连测活 round collects
    none -- so keying it off `fronts` alone would delete the whole pool on
    every direct round and a later chain round would start from no history.

    (The extraction is also what makes the `chain_on` NameError testable. That
    name was deleted by the manual-mode refactor while this code still read it,
    so every round aborted at teardown with ok=0 and published nothing.)
    """
    configured = [s.get("key") for s in cfg["sources"] if s.get("key")]
    keep_fronts = bool(fronts) or chain_configured
    if keep_fronts:
        configured.append(FRONT_SOURCE_KEY)
    removed = db.delete_sources_not_in(configured)
    if removed:
        log("info", f"清理已取消勾选来源的 {removed} 条节点记录")
    # A disabled source keeps its rows on purpose, but its verdicts stop being
    # maintained. Demote them so the panel's `alive` count only claims what is
    # still being tested -- 25 of vps's 315 `alive` were a disabled source's
    # snapshot frozen 21 hours earlier.
    demoted = db.demote_disabled_sources(
        [s["key"] for s in sources] + ([FRONT_SOURCE_KEY] if keep_fronts else []))
    if demoted:
        log("info", f"{demoted} 条记录属于已停用来源，状态降级为 unknown（历史保留）")
    return keep_fronts


def _run_round(cfg, trigger, only_source, log, mode=None):
    started = time.time()
    deadline = time.monotonic() + _budget(cfg)
    round_id = db.start_round(trigger, mode)
    _set_current_round(round_id, mode)
    _write_state("start", round_id, mode)
    log("info", f"第 {round_id} 轮开始 (trigger={trigger}"
        + (f", mode={mode}" if mode else "") + ")")
    for path in cfgmod.take_dropped_keys():
        # Reported, not swallowed: a key that survives in config.json while no
        # code reads it is a switch the user believes in and cannot use.
        log("warn", f"配置项 {path} 不在 schema 里，已忽略并从 config.json 移除")

    sources = [s for s in cfg["sources"] if s.get("enabled", True)]
    if only_source:
        sources = [s for s in sources if s["key"] == only_source]
    sources = _drop_self_references(cfg, sources, log)
    if not sources:
        db.finish_round(round_id, note="no enabled sources", duration_s=0)
        return {"round_id": round_id, "error": "no enabled sources"}
    _checkpoint(deadline, "fetch", round_id)

    store = Client(cfg["substore"]["backend"])
    entries, errors = collect_entries(store, sources)
    for message in errors:
        log("error", f"拉取失败 {message}")

    # Fronts are fetched before classification so they are tested like any other
    # node; `classify_and_expand` passes them through untouched (see there).
    # A 直连测活 round (mode="direct") skips the pool on purpose, so every node
    # is measured as its own server -- see `round_uses_chains`.
    # Which of the three shapes this round takes. `round_uses_chains` is the
    # single source of truth, and it is deliberately *not* the same question as
    # "is chaining configured": `chain_block` returns None for the
    # half-configured cases (enabled with no front source), so a 链式测活 click
    # with chaining off lands in the `else` branch below. The log has to say
    # what actually happened rather than repeat the button's name -- an earlier
    # version announced "本轮按直连测" from this branch while running the
    # chain-shaped path, which is exactly the kind of note that makes an
    # operator trust the wrong column.
    chained_round = round_uses_chains(cfg, mode)
    chain_configured = chain_block(cfg) is not None
    if chained_round:
        fronts = collect_fronts(cfg, store, log)
        front_names = [f["proxy"]["name"] for f in fronts]
        has_chained_nodes = any(DIALER_FIELD in e["proxy"] for e in entries)
        if fronts and not has_chained_nodes:
            log("warn", f"前置池有 {len(fronts)} 条，但没有任何数据源提供带 {DIALER_FIELD} "
                        "的节点，链式测活本轮无事可做")
        elif has_chained_nodes and not fronts:
            log("error", f"有节点带 {DIALER_FIELD}，但前置池为空，这些节点本轮全部判失败"
                         f"（原因 {FRONT_DEAD_REASON}，未拨号；检查 chain.front_source "
                         "指向的来源、chain.front_pick 名单，以及 chain.front_text "
                         "粘贴的链接是否还能解析出节点）")
    else:
        fronts, front_names = [], []
        has_chained_nodes = any(DIALER_FIELD in e["proxy"] for e in entries)
        if mode == "chain":
            # 链式测活 was clicked but `chain_block` said no: chaining is off,
            # or it is on with an empty front source. Either way the round is
            # byte-for-byte a 直连测活 round, and the operator needs to know
            # that the button did not do what its label promised.
            why = ("chain.enabled 为 false" if not cfg.get("chain", {}).get("enabled")
                   else "前置来源为空，也没有粘贴手动前置")
            log("warn", f"链式测活：链式未生效（{why}），本轮等同直连测；"
                        "请先在「设置」里开启链式，并选一个前置来源或粘贴前置节点")
        if has_chained_nodes:
            # A warning, not a quiet note: a chained node that is only reachable
            # through its front fails a direct test, so this round can drop it
            # from the published export until the next chain round restores it.
            # The operator should know that before reading the node table.
            log("warn", "本轮关闭链式，带 dialer-proxy 的节点按直连判定；"
                        "仅经前置可达的节点可能被判失败并从导出移除，"
                        "直到下一轮链式测活恢复")
        elif chain_configured:
            log("info", "本轮关闭链式，节点全部按直连测")

    test_entries, excluded_entries = classify_and_expand(entries + fronts, cfg, log)
    if excluded_entries:
        log("info", f"{len(excluded_entries)} 个节点入口在受限 ISP 上，跳过测试")
    # Per-source measurement policy. A 直连测活 round passes None on purpose:
    # that button means "measure everything direct", so it must not be re-split
    # by the per-source switches. The scheduler and 链式测活 honour them.
    source_flags = (None if mode == "direct"
                    else {s["key"]: (s.get("direct", True), s.get("chain", True))
                          for s in sources})
    direct_only = sorted(k for k, (d, c) in (source_flags or {}).items() if d and not c)
    if direct_only:
        log("info", "仅按直连测的来源：" + "、".join(direct_only))
    # Client-path mode: plain nodes (no upstream dialer) are measured through
    # the front pool too, because that is the path the consuming client forces
    # on them. Inert without chaining -- say so rather than silently ignoring
    # the switch.
    plain_too = bool((cfg.get("chain") or {}).get("test_plain_nodes"))
    if plain_too and not chain_configured:
        log("warn", "chain.test_plain_nodes 已开启但链式未生效，普通节点本轮仍按直连测")
    # `fail_without_front` is set only when this round *is* a chain round. An
    # empty pool there is a failure to report (`front_dead`), not an
    # invitation to measure the chained nodes direct: the kernel would strip
    # the dialer and report nodes alive on a path their owner never uses (the
    # hazard `core.prepare`'s docstring spells out, and the reason the
    # empty-pool error above promises these nodes fail). A 直连测活 round and
    # a round with chaining off reach this call with an empty pool on purpose,
    # so they keep the fall-through-to-direct behaviour they log before here.
    test_entries, chained = expand_chains(test_entries, front_names, source_flags,
                                          fail_without_front=chained_round,
                                          plain_too=plain_too)
    if chained:
        if front_names:
            log("info", f"{chained} 个节点展开为链式变体，每个前置一条 "
                        f"（共 {chained * len(front_names)} 条）"
                        + ("，含全节点链式模式" if plain_too else ""))
        else:
            log("warn", f"{chained} 个链式节点没有可用前置，本轮不拨号，"
                        f"全部按 {FRONT_DEAD_REASON} 判失败")
    entries = test_entries
    if not entries:
        db.finish_round(round_id, note="no nodes fetched", duration_s=round(time.time() - started, 1))
        log("error", "本轮没拉到任何节点")
        notifier.send(cfg, "no_nodes", "本轮没有拉到任何节点",
                      "所有来源都拉取失败，输出保持上一轮。"
                      + ("；".join(errors) if errors else ""),
                      level="error", log=log)
        return {"round_id": round_id, "error": "no nodes fetched", "errors": errors}

    secret = cfgmod.core_secret()
    core_cfg = cfg["core"]
    proxies, mapping, dropped = coremod.make_testable(
        entries, core_cfg, secret, log=log,
        strip_ech=bool(cfg.get("verify", {}).get("strip_ech")),
        keep_dialer=front_names)
    for item in dropped:
        log("warn", f"剔除不可用配置的节点 {item['name']}: {item['why']}")
    log("info", f"内核配置就绪: {len(proxies)} 节点, 剔除 {len(dropped)}")
    # Map the pruned names back to their entries. `make_testable` reports
    # `{name, why}` because that is all `prepare` knows -- it sees proxies, not
    # entries -- but the ledger keys on (source, fp), so the caller has to
    # reattach both. Names are not unique upstream, hence every match, not one.
    #
    # Silently ignoring this list is what let a kernel-rejected node keep the
    # previous round's `alive` status and get exported as a YAML the kernel
    # then refused to load.
    dropped_names = {item["name"] for item in dropped}
    rejected_entries = ([e for e in entries if e["name"] in dropped_names]
                        if dropped_names else [])
    if not proxies:
        db.finish_round(round_id, note="every node was rejected by the kernel",
                        duration_s=round(time.time() - started, 1))
        return {"round_id": round_id, "error": "no testable nodes"}

    _checkpoint(deadline, "build", round_id)
    core = coremod.Core(core_cfg, secret)
    action = core.start_and_load(log=log)
    log("info", f"内核 {action}，开始真实测试")

    by_name = {entry["mihomo"]: entry for entry in mapping}
    test_cfg = cfg["test"]
    concurrency = int(test_cfg.get("concurrency", 20))
    # Two passes over one loaded config, not two kernel loads: the fronts are
    # tested first, and the chains are then restricted to the fronts that
    # actually carried traffic. A chain through a dead front cannot succeed, so
    # dialling it would spend a timeout per front to re-learn the front's own
    # result. The variants still have to exist in the config from the start,
    # because that is what the kernel was loaded with.
    #
    # The split is keyed on the *entries*, not on `fronts`: a chain round whose
    # pool came back empty still has chain-role orphans in `mapping`, and the
    # split is the only thing that turns them into `chain_failed` (recorded,
    # streak advanced, pruned safely) rather than leaving them in no result at
    # all.
    phased = bool(fronts) or any(e.get("role") == "chain" for e in entries)
    results, chain_failed, live_fronts = _test_phases(
        core, mapping, test_cfg, concurrency, deadline, phased, log)
    if (fronts or chain_failed) and not live_fronts:
        # Every chained node just failed for one reason, and that reason is not
        # the nodes. Without this the panel would show a mass failure with
        # `front_dead` buried in the reason column of a few hundred rows.
        # Two shapes reach here: a pool that was fetched and carried nothing,
        # and a pool that never arrived. Both stop chaining dead, and the
        # message has to say which -- "前置池 0 条全部失败" tells an operator
        # nothing about whether to inspect `chain.front_source` or the
        # Sub-Store backend behind it.
        #
        # Wrapped like `_maybe_alert` below: an alert is best-effort, and this
        # one runs *before* `finish_round`, so a raise here would stamp a round
        # that actually completed as aborted and rewrite its finish time.
        pool = (f"前置池 {len(fronts)} 条全部失败" if fronts
                else "前置池为空（chain.front_source 拉取失败或没有可用节点）")
        try:
            notifier.send(cfg, "front_dead", "前置全部不通，链式测活停摆",
                          f"{pool}，{len(chain_failed)} 个链式节点"
                          "本轮判失败（原因 front_dead，未拨号）。"
                          "先确认 chain.front_source 里的前置是否还有效。",
                          level="error", log=log)
        except Exception as exc:  # noqa: BLE001 - an alert must never fail a round
            log("error", f"告警发送失败（不影响本轮结果）: {type(exc).__name__}: {exc}")

    _checkpoint(deadline, "delay-test", round_id)
    # Real-payload verification before the survivor list is built: a node that
    # answered the 204 but cannot carry a real TLS session must not be
    # egress-verified or published as alive.
    _verify_chain_payload(core, core_cfg, mapping, results, cfg, log, deadline=deadline)

    survivors = [name for name, outcome in results.items() if outcome["reason"] is None]
    log("info", f"存活 {len(survivors)}/{len(mapping)}，开始出口验证")

    verify_cfg = cfg.get("verify", {})
    countries, unverified, over_limit = {}, set(), []
    _checkpoint(deadline, "delay-test", round_id)
    if verify_cfg.get("enabled", True) and survivors:
        countries, unverified, over_limit = _verify_egress(
            core, core_cfg, survivors, verify_cfg, log, deadline=deadline)
    else:
        log("info", "出口验证已关闭，跳过")

    _checkpoint(deadline, "publish", round_id)
    summary = _apply_and_publish(cfg, round_id, store, by_name, proxies, results,
                                 countries, sources, log,
                                 excluded_entries=excluded_entries,
                                 unverified=unverified, over_limit=over_limit,
                                 chain_failed_entries=chain_failed,
                                 rejected_entries=rejected_entries,
                                 fronts=fronts)
    _reconcile_ledger(cfg, sources, fronts, chain_configured, log)
    stale = cleanup_exports(cfg)
    if stale:
        log("info", f"清理不再启用的来源导出文件: {', '.join(stale)}")
    elapsed = round(time.time() - started, 1)
    # `total`/`failed` must use the same unit as `ok`. `ok` counts endpoints
    # (one per fingerprint, however many addresses it resolved to), so passing
    # `len(mapping)` here -- one row per address variant -- reported a node that
    # resolves to 3 addresses and is alive as "1 of 3, 2 failed". The guardrail
    # reads `ok` only, so it was never affected; the panel and the history table
    # were simply wrong, and in the direction that looks like mass death.
    db.finish_round(round_id, total=summary["total"], ok=summary["alive"],
                    failed=summary["total"] - summary["alive"], dropped=summary["dropped"],
                    restored=summary["restored"], suspect=1 if summary["suspect"] else 0,
                    note=summary["note"], duration_s=elapsed)
    log("info", f"第 {round_id} 轮结束: 活 {summary['alive']}/{summary['total']}, "
                f"降级 {summary['dropped']}, 恢复 {summary['restored']}, "
                f"新入 {summary.get('new_alive', 0)}, 耗时 {elapsed}s"
                + ("  ⚠️ 本轮可疑，未发布" if summary["suspect"] else ""))
    summary["round_id"] = round_id
    summary["duration_s"] = elapsed
    # The round has been recorded and published at this point. Alerts are
    # best-effort and must not be able to change that verdict: `notifier.send`
    # does `int(cooldown_minutes)` and writes a state file, so a bad config
    # value or a full disk raised here, travelled up to `run_round`, and stamped
    # a successful round "aborted" with its finish time rewritten.
    try:
        _maybe_alert(cfg, summary, log)
    except Exception as exc:  # noqa: BLE001 - an alert must never fail a round
        log("error", f"告警发送失败（不影响本轮结果）: {type(exc).__name__}: {exc}")
    try:
        db.trim_results(cfg)
    except Exception as exc:  # noqa: BLE001 - housekeeping, never load-bearing
        log("warn", f"清理历史 results 失败: {type(exc).__name__}: {exc}")
    return summary



def _stored_epoch(stamp):
    """Epoch seconds for a stored timestamp, which is always UTC. 0 if unusable."""
    for fmt in ("%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"):
        try:
            return calendar.timegm(time.strptime(str(stamp), fmt))
        except (TypeError, ValueError):
            continue
    return 0


def reap_orphan_rounds(cfg, log=None):
    """Close rounds left open by a process that died mid-round.

    `_abandon_round` only runs *in* the process, so a container restart during
    a round -- and `docker compose up -d --build`, the documented way to deploy
    a code change, is exactly that -- left the ledger row open forever. Nothing
    else ever closed it: the watchdog's deadline is a monotonic timer that died
    with the process, and the scheduler only ever starts new rounds. On vps
    that left round 172 unfinished, which fails
    `test_live.test_no_round_is_left_unfinished_forever` for good.

    Only rounds older than the watchdog budget are reaped. A round that is
    genuinely running takes ~3 minutes, so it is younger than that; and the CLI
    (`python3 -m mihomo_test round`) may legitimately run one alongside the
    service, because the two share the ledger but not the in-process BUSY lock.
    """
    log = log or db.log
    cutoff = _budget(cfg)
    now = time.time()
    closed = []
    for row in db.open_rounds():
        started = _stored_epoch(row.get("started_at"))
        if started and now - started > cutoff:
            closed.append(int(row["id"]))
    for round_id in closed:
        try:
            db.finish_round(round_id, duration_s=0,
                            note="orphaned: 进程在轮次中途退出（重建/被杀），本轮无结果")
        except (ValueError, TypeError):
            continue
    if closed:
        log("warn", "回收 %d 个未闭合的残留轮次: %s"
            % (len(closed), ", ".join(f"第{i}轮" for i in closed)))
    return closed


def _abandon_round(cfg, round_id, why, log, alert_key, title):
    """Close out a round that died mid-flight.

    An unclosed row stays "in progress" forever and the ledger keeps a hole,
    so the round id is recovered from the state file the checkpoint writes.

    The state file is only trusted when it plausibly belongs to the round that
    actually died: a leftover file from an earlier crash can name a round that
    has since finished, and closing *that* one stamps a healthy round with a
    failure note and an out-of-order finish time. On vps this produced a
    finished_at eight hours before started_at for rounds 55/56.
    """
    state = {}
    try:
        state = json.loads(ROUND_STATE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        pass
    rid = round_id or state.get("round_id")
    if rid:
        try:
            rid = int(rid)
        except (ValueError, TypeError):
            rid = None
    # Ownership is checked even for an explicitly supplied round id. The guard
    # used to run only on the state-file path (`round_id is None`), so the
    # in-process caller passed `_CURRENT_ROUND_ID` and skipped it -- which is
    # how a failure *after* `finish_round` (an alert, say) rewrote a completed
    # round's note to "aborted: ..." and moved its finished_at.
    if rid and not _state_belongs_to(rid):
        log("warn", f"第 {rid} 轮已经结束，不再按中止处理（忽略本次 {why}）")
        rid = None
    if rid:
        try:
            db.finish_round(rid, note=f"aborted: {why}"[:200], duration_s=0)
        except (ValueError, TypeError):
            pass
    try:
        notifier.send(cfg, alert_key, title, why, level="error", log=log)
    except Exception as exc:  # noqa: BLE001 - this runs inside an except block
        log("error", f"中止告警发送失败: {type(exc).__name__}: {exc}")


def _state_belongs_to(round_id):
    """True when the recorded round is still open (i.e. plausibly the victim)."""
    row = db.one("SELECT finished_at FROM rounds WHERE id=?", (round_id,))
    return row is not None and not row.get("finished_at")

def _maybe_alert(cfg, summary, log):
    """Fire the alerts a finished round can raise; each has its own cooldown."""
    alerts = cfg.get("alert", {})
    if not alerts.get("enabled", False):
        return
    if summary.get("suspect"):
        notifier.send(cfg, "suspect_round", "护栏触发：本轮结果可疑，未发布",
                      f"存活 {summary.get('alive')} 低于上轮安全下限，"
                      "已保留上一轮输出。先确认本机网络与测试目标是否正常。",
                      level="error", log=log)
    try:
        floor = int(alerts.get("alive_floor", 0) or 0)
    except (TypeError, ValueError):
        floor = 0
    alive = summary.get("alive")
    if floor > 0 and isinstance(alive, int) and alive < floor:
        notifier.send(cfg, "alive_low", f"存活节点低于下限（{alive} < {floor}）",
                      f"本轮存活 {alive}，配置下限 {floor}。", level="warn", log=log)


def _test_all(core, mapping, test_cfg, concurrency, deadline=None):
    """Test every prepared proxy; raise RoundTimeout once the budget is gone.

    `pool.map` propagates the first exception, and the executor's shutdown
    waits for the rest -- but every remaining task re-checks `deadline` before
    its first attempt and returns immediately, so waiting is bounded by the
    tasks already in flight (one attempt each), not by the whole queue.
    """
    from concurrent.futures import ThreadPoolExecutor

    results = {}

    def run(entry):
        return entry["mihomo"], test_one(core, entry, test_cfg, deadline=deadline)

    with ThreadPoolExecutor(max_workers=max(1, concurrency)) as pool:
        for name, outcome in pool.map(run, mapping):
            results[name] = outcome
    return results


def _test_phases(core, mapping, test_cfg, concurrency, deadline, chained, log):
    """Test fronts first, then chains -- but only through fronts that worked.

    Returns (results, chain_failed_entries, live_fronts). `chain_failed_entries`
    holds one entry per chained node that had no live front left, deduplicated
    by fingerprint, for `_record_chain_failures` to fail without dialling.

    The phase split is the whole point of the feature: a chain is only a
    statement about the front if the front carries traffic, so the front's
    verdict has to exist before the chain's can mean anything. With chaining off
    this is a single pass over `mapping` and behaves exactly as before.

    The split is on `role`, not on `category`. `role` says what a node *does*
    in this round, which is what the ordering needs; `category` says what kind
    of node it is, which is what the statistics need. They agree on the
    interesting cases and differ on purpose: a relay that is not in the pool is
    tested in pass one like any direct node, while its numbers are still
    reported under 中转节点.
    """
    if not chained:
        return _test_all(core, mapping, test_cfg, concurrency, deadline=deadline), [], 0

    first = [m for m in mapping if m.get("role") != "chain"]
    second = [m for m in mapping if m.get("role") == "chain"]
    results = _test_all(core, first, test_cfg, concurrency, deadline=deadline)

    fronts = [m for m in first if m.get("role") == "front"]
    live = {m["mihomo"] for m in fronts if results[m["mihomo"]]["reason"] is None}
    if fronts:
        log("info", f"前置存活 {len(live)}/{len(fronts)}"
                    + ("（前置全死，链式节点将全部判失败）" if not live else ""))
    # The category census is logged here because this is the one point in the
    # round where every tested node is in hand at once. Without it a
    # misclassification (a source marked `relay` by accident) would surface only
    # as a strange number on the dashboard, with nothing in the log to explain
    # it. Chain nodes are counted by fingerprint: the variants are the same node
    # dialled through different fronts, so counting rows would multiply the
    # total by the pool size.
    if first or second:
        direct_n = sum(1 for m in first if m.get("category") == CAT_DIRECT)
        relay_n = sum(1 for m in first if m.get("category") == CAT_RELAY)
        chain_nodes = len({(m["source"], m["fp"]) for m in second})
        log("info", f"分类统计口径: 直连 {direct_n}, 中转 {relay_n}, "
                    f"链式 {chain_nodes} 个节点（{len(second)} 条变体）")
    runnable = [m for m in second if m["front"] in live]
    if runnable:
        results.update(_test_all(core, runnable, test_cfg, concurrency, deadline=deadline))

    # Only nodes with nothing left to try are failed outright. A node with one
    # live front and one dead one is tested through the live one, and the
    # per-fingerprint aggregation already handles that correctly.
    tried = {(m["source"], m["fp"]) for m in runnable}
    chain_failed, seen = [], set()
    for m in second:
        key = (m["source"], m["fp"])
        if key in tried or key in seen:
            continue
        seen.add(key)
        chain_failed.append(m)
    if chain_failed:
        log("warn", f"{len(chain_failed)} 个链式节点没有可用前置，判失败（未拨号）")
    return results, chain_failed, len(live)


def _verify_egress(core, core_cfg, survivors, verify_cfg, log, deadline=None):
    """Route real traffic through each survivor and read its exit country.

    The kernel's selector is global state, so a single group makes this phase
    serial -- ~1.9s per live node, which at a few hundred nodes is most of the
    round. Each lane is its own select group with its own inbound port, and an
    IN-NAME rule pins that listener to that group, so lanes run concurrently
    and independently (verified: swapping two lanes' selections swaps their
    reported exits).
    """
    from concurrent.futures import ThreadPoolExecutor

    trace_url = verify_cfg.get("trace_url")
    timeout_s = int(verify_cfg.get("timeout_s", 15))
    lanes = coremod.lane_count(core_cfg)
    ports = coremod.lane_ports(core_cfg, lanes)
    limit = int(verify_cfg.get("max_nodes", 0) or 0)
    queue = survivors[:limit] if limit else survivors
    if not queue:
        return {}, set(), []

    buckets = {i: queue[i::lanes] for i in range(lanes)}
    out, lock, progress = {}, threading.Lock(), [0]

    def run_lane(index):
        lane_result = {}
        group, port = coremod.lane_group(index), ports[index]
        for name in buckets.get(index, []):
            if deadline is not None and time.monotonic() > deadline:
                break
            try:
                core.select(group, name)
            except coremod.CoreError as exc:
                lane_result[name] = {"country": None, "error": str(exc)[:120]}
                continue
            fields, error = core.egress(port, trace_url, timeout_s)
            if error:
                lane_result[name] = {"country": None, "error": error}
            else:
                lane_result[name] = {"country": fields.get("loc"), "ip": fields.get("ip"),
                                     "colo": fields.get("colo"), "error": None}
            with lock:
                progress[0] += 1
                done = progress[0]
                if done % 25 == 0 or done == len(queue):
                    log("info", f"出口验证 {done}/{len(queue)}")
        return lane_result

    with ThreadPoolExecutor(max_workers=lanes) as pool:
        for lane_result in pool.map(run_lane, range(lanes)):
            out.update(lane_result)
    # Anything queued but absent from the results was never checked: its lane hit
    # the deadline first. This is a backstop rather than the main protection --
    # when the deadline is what caused it, the checkpoint that runs immediately
    # after this call raises and the round is abandoned before publishing (pinned
    # by RoundBudgetTest). It matters if that checkpoint ever moves.
    unverified = {name for name in queue if name not in out}
    # `verify.max_nodes` truncates the queue outright. Those nodes are published
    # without any exit check, which is the setting's documented trade-off -- but
    # it used to happen with no record at all, so an operator could not tell a
    # verified round from a truncated one.
    over_limit = [name for name in survivors if name not in queue]
    return out, unverified, over_limit


def _verify_chain_payload(core, core_cfg, mapping, results, cfg, log, deadline=None):
    """Real-fetch verification for alive fronts and chained nodes.

    A 204 delay test proves the chain dialled and answered -- it does not
    prove the path carries a real TLS session. The home-vantage comparison
    (2026-09-28) caught a chained node that passed the delay check and then
    failed every real fetch with a TLS reset, and the client's own gstatic
    health-check shares that blind spot, so the panel kept showing it alive
    while it was unusable. One real page pull per node through the lanes
    catches the class.

    Any completed HTTP response passes (`core.fetch`); dial errors, timeouts
    and TLS resets fail:

    * a front that fails the pull fails with `front_dead` -- and every chained
      variant dialling through it fails with the same reason, so the panel
      blames the front rather than the nodes (the rule FRONT_DEAD_REASON's
      own docstring states);
    * a chained node whose front is fine fails with `payload_fail` -- a
      non-terminal reason, so the convergence policy treats an intermittent
      node exactly like any other flapper instead of executing it on one
      strike.

    One variant per fingerprint is pulled: the per-fingerprint fold already
    rules "any front carried it", so testing one variant per node is the same
    statement at payload granularity, and the pool is small by construction.
    Best-effort at the deadline: entries never reached keep their delay
    verdict, and the checkpoint after this call handles overrun.

    Config: `verify.chain_payload` -- `{enabled, url, timeout_s}`; disabled
    with `enabled: false`.
    """
    from concurrent.futures import ThreadPoolExecutor

    vc = (cfg.get("verify") or {}).get("chain_payload") or {}
    if vc.get("enabled") is False:
        return
    url = str(vc.get("url") or "https://www.google.com/")
    timeout_s = int(vc.get("timeout_s", 15))
    # `mapping` holds every prepared entry, but `results` only the dialled
    # ones: chain_failed variants (no live front) and other never-dialled
    # entries have no outcome to verify, so `results.get` is load-bearing --
    # indexing straight into `results` KeyError'd exactly there.
    fronts = [m for m in mapping
              if m.get("role") == "front"
              and (results.get(m["mihomo"]) or {}).get("reason") is None]
    chains, seen = [], set()
    for m in mapping:
        outcome = results.get(m["mihomo"])
        if (m.get("role") == "chain" and outcome is not None
                and outcome["reason"] is None
                and (m["source"], m["fp"]) not in seen):
            seen.add((m["source"], m["fp"]))
            chains.append(m)
    if not fronts and not chains:
        return

    lanes = coremod.lane_count(core_cfg)
    ports = coremod.lane_ports(core_cfg, lanes)
    queue = fronts + chains
    failures, lock, progress = {}, threading.Lock(), [0]

    def run_lane(index):
        lane_out = {}
        group, port = coremod.lane_group(index), ports[index]
        for m in queue[index::lanes]:
            if deadline is not None and time.monotonic() > deadline:
                break
            try:
                core.select(group, m["mihomo"])
            except coremod.CoreError as exc:
                lane_out[m["mihomo"]] = str(exc)[:120]
                continue
            _status, _nbytes, error = core.fetch(port, url, timeout_s)
            if error:
                lane_out[m["mihomo"]] = error
            with lock:
                progress[0] += 1
        return lane_out

    with ThreadPoolExecutor(max_workers=lanes) as pool:
        for lane_out in pool.map(run_lane, range(lanes)):
            failures.update(lane_out)

    if not failures:
        log("info", f"链式真实拉流校验: {len(queue)}/{len(queue)} 通过")
        return
    dead_fronts = {m["mihomo"] for m in fronts if m["mihomo"] in failures}
    n_front = n_node = 0
    for m in queue:
        error = failures.get(m["mihomo"])
        if error is None:
            continue
        if m.get("role") == "front":
            results[m["mihomo"]] = {**results[m["mihomo"]],
                                    "reason": FRONT_DEAD_REASON,
                                    "detail": f"真实拉流失败: {error}"}
            n_front += 1
        elif m.get("front") in dead_fronts:
            results[m["mihomo"]] = {**results[m["mihomo"]],
                                    "reason": FRONT_DEAD_REASON,
                                    "detail": f"前置真实拉流失败（延迟已过，不怪节点）: {error}"}
        else:
            results[m["mihomo"]] = {**results[m["mihomo"]],
                                    "reason": "payload_fail",
                                    "detail": f"延迟通过但真实拉流失败: {error}"}
            n_node += 1
    log("warn", f"链式真实拉流校验: {len(queue) - n_front - n_node}/{len(queue)} 通过，"
                f"{n_front} 个前置、{n_node} 个链式节点判失败")


def group_by_fingerprint(results, by_name):
    """Fold aliases of one endpoint into a single bucket.

    Upstream sometimes lists the same server under two names, and those names
    share a fingerprint. Evaluating them separately would advance the failure
    streak twice per round and drop the node a round early.
    """
    grouped = {}
    for name, outcome in results.items():
        entry = by_name[name]
        bucket = grouped.setdefault(
            (entry["source"], entry["fp"]),
            {"entry": entry, "names": [], "outcomes": []},
        )
        bucket["names"].append(name)
        bucket["outcomes"].append(outcome)
    return grouped


def _resolve_exit(countries, names, excluded):
    """Return (country, failure_reason) from the verified exits of a node's aliases."""
    if not countries:
        return None, None
    verified = [countries[n] for n in names if n in countries]
    if not verified:
        return None, None
    good = [v for v in verified if not v.get("error")]
    if not good:
        return None, "verify_failed"
    country = good[0].get("country")
    if country and country.upper() in excluded:
        return country, f"exit_{country.upper()}"
    return country, None


def _index_original_proxies(by_name, proxies):
    """Map display name -> the *original-domain* proxy for publishing.

    The kernel tests per-address variants (`host__1`, `host__2`, ...); the
    ledger and the export speak the original domain form, so publish-time
    lookups must go through `orig_proxy`.
    """
    return {name: m.get("orig_proxy") or next(
        (p for p in proxies if p["name"] == name), {"name": name})
        for name, m in by_name.items()}


def _score_bucket(bucket, domain_pass):
    """Reduce one fingerprint's per-address outcomes to (ok, delay, reason, detail).

    `ip_alive > 0` is the default: a domain that resolves to several addresses
    is alive as long as one of them answers. `domain_pass == "all"` tightens
    that to every address having answered.
    """
    outcomes = bucket["outcomes"]
    ip_total = len(outcomes)
    ip_alive = sum(1 for o in outcomes if o["reason"] is None)
    ok = ip_alive > 0
    if domain_pass == "all" and ip_total > 1:
        ok = ip_alive == ip_total
    if ok:
        delays = [o["delay_ms"] for o in outcomes
                  if o["reason"] is None and o["delay_ms"] is not None]
        return True, (min(delays) if delays else None), None, "", ip_alive, ip_total
    # Failed. Report the first *failing* address rather than outcomes[0]: under
    # domain_pass=all a dead node can still open the list with a healthy
    # address, and indexing [0] would then record reason=None -- a dead node
    # with no reason at all, which the panel renders as a blank explanation.
    first = next((o for o in outcomes if o["reason"] is not None), outcomes[0])
    return False, None, first["reason"], first.get("detail") or "", ip_alive, ip_total


def _converge_bucket(cfg, round_id, bucket, proxy_by_name, countries, excluded,
                     domain_pass, log):
    """Converge every alias of one endpoint and write the round's result rows.

    Returns ("drop" | "restore" | None, [alive primary names], alive category).
    The category is the bucket's own -- `chain` or `direct` -- and is None on
    a failed bucket: the export uses it to publish the form that actually
    passed (a chained node whose chain failed the round but whose direct twin
    passed must go out direct, not as a chain the round just disproved).
    """
    entry = bucket["entry"]
    source, fingerprint = entry["source"], entry["fp"]
    display = entry["original"]

    ok, delay, reason, detail, ip_alive, ip_total = _score_bucket(bucket, domain_pass)
    country, exit_failure = _resolve_exit(countries, bucket["names"], excluded)
    if ok and exit_failure:
        # The node answers the probe but egresses somewhere we refuse to
        # publish, so it does not count as alive this round.
        ok, reason = False, exit_failure

    node = db.get_node(source, fingerprint)
    if node is None:
        db.upsert_node(source, fingerprint, display)
        node = db.get_node(source, fingerprint)

    fields, transition = policy.apply(node, ok, delay, reason, cfg["policy"])
    if country:
        fields["country"] = country
    if ip_total:
        fields.update({"ip_alive": ip_alive, "ip_total": ip_total})
    primary = bucket["names"][0]
    fields.update({"proto": proxy_by_name[primary].get("type"),
                   "server": proxy_by_name[primary].get("server")})
    # Re-stamped every round rather than only on insert: a node moves between
    # categories when its source is re-marked as a relay, and a stale category
    # would keep feeding the old bucket forever.
    category = entry.get("category") or CAT_DIRECT
    fields["category"] = category
    db.upsert_node(source, fingerprint, display, **fields)

    db.record_result(round_id, source, fingerprint, display,
                     "ok" if ok else "fail", delay, reason, country,
                     max(o["attempts"] for o in bucket["outcomes"]), detail[:200],
                     category=category)
    return transition, ([primary] if ok else []), (category if ok else None)


REJECTED_REASON = "kernel_rejected"


def _record_excluded_nodes(round_id, excluded_entries):
    """Write ledger rows for nodes skipped by the entry-IP filter.

    Excluded nodes are never tested, so they are absent from `by_name` -- and
    without recording them here the prune in `_prune_removed_nodes` would wipe
    their records every single round.

    `consec_fail` is reset to 0, which is the whole point of `EXCLUDED`: the
    classification means "not testable from here", so a streak must not survive
    it. This path bypasses `policy.apply` -- the only other place a streak is
    cleared -- so without the explicit reset a node that reached
    `consec_fail=2` and then had its entry IP reclassified as CN kept the 2 and
    resumed from there, i.e. one more round would kill a node that had never
    actually failed. `policy.py` states this invariant in a comment and
    `test_live` asserts it against the live ledger.
    """
    by_source = collections.defaultdict(list)
    fps = collections.defaultdict(set)
    for entry in excluded_entries or []:
        by_source[entry["source"]].append(entry)
    for source, entries in by_source.items():
        for entry in entries:
            # Same identity rule as tested nodes: the fingerprint computed once
            # in `collect_entries`, so a node flipping between testable and
            # excluded keeps one ledger entry instead of growing a duplicate.
            fingerprint = entry.get("fp") or _orig_fp(entry["proxy"])
            fps[source].add(fingerprint)
            db.upsert_node(
                source, fingerprint, entry["name"],
                status=policy.EXCLUDED, last_reason="entry_cn", consec_fail=0,
                proto=entry["proxy"].get("type"), server=entry["proxy"].get("server"),
                category=entry.get("category") or CAT_DIRECT)
            db.record_result(round_id, source, fingerprint, entry["name"],
                             "excluded", None, "entry_cn", None, 0, "",
                             category=entry.get("category") or CAT_DIRECT)
    return by_source, fps


def _record_rejected_nodes(cfg, round_id, entries, log):
    """Fail nodes the kernel refused to load, and keep them out of the export.

    `core.make_testable` prunes a node whose proxy the kernel rejects, logs
    "剔除不可用配置的节点", and returns the surviving list -- but the pruning
    never reached the ledger. The node therefore kept whatever status the
    previous round left on it, so a node that was `alive` last round stayed
    `alive`, was counted in the export, and the published YAML carried a proxy
    the kernel refuses:

        proxy 147: invalid REALITY short ID
        configuration file test failed

    That is the same class of defect as a dangling `dialer-proxy`: the round
    tested fine, and the export was unloadable. Measured live, one `<源>`
    node (`short-id: <sid>片段`) was published as alive this way.

    The verdict is a real `fail` -- an entry the kernel will not accept is not
    usable, so it must converge out of the export the normal way, through
    `policy.apply` and its consecutive-failure streak. Recording it here also
    keeps `_prune_removed_nodes` from deleting the row: these entries are
    absent from `by_name` (nothing was dialled).

    Returns (by_source, fps) in the same shape as `_record_excluded_nodes`.
    """
    by_source = collections.defaultdict(list)
    fps = collections.defaultdict(set)
    for entry in entries or []:
        source, fingerprint = entry["source"], entry.get("fp") or _orig_fp(entry["proxy"])
        if fingerprint in fps[source]:
            continue
        fps[source].add(fingerprint)
        by_source[source].append(entry)
        display = entry["name"]
        node = db.get_node(source, fingerprint)
        if node is None:
            db.upsert_node(source, fingerprint, display)
            node = db.get_node(source, fingerprint)
        fields, _ = policy.apply(node, False, None, REJECTED_REASON, cfg["policy"])
        fields.update({"proto": entry["proxy"].get("type"),
                       "server": entry["proxy"].get("server")})
        # Carry the category through: a rejected node still belongs to whichever
        # bucket it was collected into, and `record_result` would otherwise
        # write NULL -- which `stats_by_category` reads as `direct`. A rejected
        # chained node would then be counted against 直连 in both units.
        cat = entry.get("category") or CAT_DIRECT
        db.upsert_node(source, fingerprint, display, category=cat, **fields)
        db.record_result(round_id, source, fingerprint, display, "fail", None,
                         REJECTED_REASON, None, 0, "内核拒绝该节点配置", category=cat)
    if by_source:
        log("warn", f"{sum(len(v) for v in by_source.values())} 个节点被内核拒绝，"
                    f"已判失败且不再导出（{REJECTED_REASON}）")
    return by_source, fps


def _record_chain_failures(cfg, round_id, entries, log):
    """Fail chained nodes that had no live front, without dialling them.

    The round tests the fronts first and only then the chains, so by the time
    this runs it already knows which fronts carry traffic. A chained node whose
    every front is dead cannot succeed through any of them, so dialling it would
    burn a timeout per front to learn what the front's own result already says.

    The verdict is `fail`, which is the requested rule: a chain whose front
    carries nothing is not usable. Only the recorded reason differs from a real
    dial -- `front_dead` instead of `timeout` -- so the dashboard blames the
    front instead of reporting a node that was never tried as timing out. The
    streak advances through `policy.apply` exactly as a dial failure would.

    Returns (by_source, fps) so `_prune_removed_nodes` keeps these rows: they
    are absent from `by_name` (nothing was dialled), and the complement-based
    prune would otherwise delete them and reset the very streak being advanced.
    """
    by_source = collections.defaultdict(list)
    fps = collections.defaultdict(set)
    for entry in entries or []:
        source, fingerprint = entry["source"], entry["fp"]
        if fingerprint in fps[source]:
            continue
        fps[source].add(fingerprint)
        by_source[source].append(entry)
        display = entry["original"]
        node = db.get_node(source, fingerprint)
        if node is None:
            db.upsert_node(source, fingerprint, display)
            node = db.get_node(source, fingerprint)
        fields, _ = policy.apply(node, False, None, FRONT_DEAD_REASON, cfg["policy"])
        fields.update({"proto": entry.get("proto"), "server": entry.get("server")})
        # Always `chain`: this path only ever sees chained entries (they are the
        # ones with a front to be dead). Leaving it out wrote NULL, which the
        # stats read as `direct` -- so every chain killed by a dead front pool
        # silently inflated 直连's failure count.
        db.upsert_node(source, fingerprint, display, category=CAT_CHAIN, **fields)
        db.record_result(round_id, source, fingerprint, display, "fail", None,
                         FRONT_DEAD_REASON, None, 0, "前置全部不通，链式未测",
                         category=CAT_CHAIN)
        log("warn", f"{display}: 前置全部不通，链式判失败（{FRONT_DEAD_REASON}）")
    return by_source, fps


def _prune_removed_nodes(by_name, excluded_by_source, excluded_fps, log, chain_failed=None,
                         rejected=None):
    """Drop ledger rows for nodes upstream no longer lists.

    Without this an unlisted node lingers as `unknown` forever and inflates the
    panel's node count.

    `chain_failed` and `rejected` map source -> fingerprints that were failed
    without being dialled. They have to count as seen here, or the prune would
    treat them as vanished upstream and delete the streak it just advanced.
    """
    chain_failed = chain_failed or {}
    rejected = rejected or {}
    all_sources = (set(entry["source"] for entry in by_name.values())
                   | set(excluded_by_source) | set(chain_failed) | set(rejected))
    for source in all_sources:
        seen = sorted({entry["fp"] for entry in by_name.values() if entry["source"] == source}
                      | excluded_fps.get(source, set())
                      | chain_failed.get(source, set())
                      | rejected.get(source, set()))
        gone = db.delete_nodes_not_in(source, seen)
        if gone:
            log("info", f"{source}: 上游已移除 {gone} 个节点，清理记录")


def export_keys(cfg):
    """The keys that get an export file: enabled *and* marked for export.

    `export: false` is how a source is tested and shown but never published --
    see `config.normalize_sources`. Everything downstream that decides "does
    this source own a file" must ask this, not `enabled` alone, or a muted
    source would keep a stale YAML on disk for `link_substore` to pick up.
    """
    return [s["key"] for s in cfg.get("sources", [])
            if s.get("key") and s.get("enabled", True) and s.get("export", True)]


def _front_state_path():
    return EXPORT_DIR / "fronts.json"


def _write_front_state(chain_fronts, log):
    """Record the round's live front proxies for the exports of rounds that
    test no fronts.

    A 直连测活 round (or a round with chaining switched off) still publishes
    chained nodes in their configured form, and those dialers need fronts to
    resolve against. The last chain round's live pool is the best available
    answer, so it is kept here; an empty pool is written just as deliberately,
    so a pool that died is not resurrected by stale state.
    """
    try:
        EXPORT_DIR.mkdir(parents=True, exist_ok=True)
        payload = {"updated_at": db.now(), "fronts": list(chain_fronts)}
        path = _front_state_path()
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
        tmp.replace(path)
    except OSError as exc:
        log("warn", f"前置留档写入失败（不影响本轮导出）: {type(exc).__name__}: {exc}")


def _read_front_state(log=None):
    """The last recorded live front proxies, or [] when there is none."""
    try:
        payload = json.loads(_front_state_path().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    fronts = payload.get("fronts") if isinstance(payload, dict) else None
    if not isinstance(fronts, list):
        return []
    return [f for f in fronts if isinstance(f, dict) and f.get("name")]


def _publish_sources(cfg, store, sources, alive_by_source, proxy_by_name, countries, log,
                     chain_fronts=None, alive_forms=None):
    """Write each source's export file, and optionally upsert into Sub-Store.

    `chain_fronts` is the round's live front proxies: `[]` when the round
    tested fronts and none survived, `None` when this round tested no fronts
    at all -- then the last recorded pool from `_write_front_state` stands in.
    `alive_forms` maps source -> display -> which measured form ("chain" or
    "direct") earned the node its alive verdict this round.
    """
    if not cfg["publish"].get("enabled", True):
        return False
    if chain_fronts is None:
        chain_fronts = _read_front_state(log)
    else:
        _write_front_state(chain_fronts, log)
    keys = set(export_keys(cfg))
    for source in sources:
        key = source["key"]
        if key not in keys:
            continue
        _write_export(cfg, key, alive_by_source.get(key, []), proxy_by_name, countries,
                      chain_fronts=chain_fronts, alive_forms=(alive_forms or {}).get(key))
    if cfg["publish"].get("push_to_substore"):
        push_exports(cfg, store, [s["key"] for s in sources if s["key"] in keys], log)
    return True


def _apply_and_publish(cfg, round_id, store, by_name, proxies, results, countries, sources, log,
                       excluded_entries=None, unverified=None, over_limit=None,
                       chain_failed_entries=None, rejected_entries=None, fronts=None):
    """Converge every tested node, reconcile the ledger, then publish.

    Thin orchestrator: the per-node state machine lives in `_converge_bucket`,
    untestable-entry bookkeeping in `_record_excluded_nodes`, ledger hygiene in
    `_prune_removed_nodes`, and the export write in `_publish_sources`. The
    guardrail decision stays here because it gates publishing and depends on the
    totals only this function has.

    `fronts` is the round's front pool (from `collect_fronts`); the ones whose
    delay test passed are what the chained exports publish as their dialer
    targets. A front the round could not carry traffic through must not be
    published -- that is exactly how a panel-alive chain breaks in a client.
    """
    excluded = {c.upper() for c in verify_cfg_excludes(cfg)}
    domain_pass = str(cfg.get("verify", {}).get("domain_pass", "any")).lower()
    proxy_by_name = _index_original_proxies(by_name, proxies)
    alive_by_source = {}
    alive_forms = {}
    dropped = restored = new_alive = 0
    previous_alive = _previous_alive_count()

    for bucket in group_by_fingerprint(results, by_name).values():
        transition, alive_names, alive_cat = _converge_bucket(
            cfg, round_id, bucket, proxy_by_name, countries, excluded, domain_pass, log)
        if transition == "drop":
            dropped += 1
        elif transition == "restore":
            restored += 1
        elif transition == "new":
            new_alive += 1
        if alive_names:
            src = bucket["entry"]["source"]
            alive_by_source.setdefault(src, []).extend(alive_names)
            # The form that gets published follows the measurement: a chain
            # verdict outranks the direct twin (the export speaks the source's
            # configured form), but a node whose chain failed while its direct
            # twin passed must go out direct -- shipping the chained form would
            # publish a dialer the round just disproved.
            forms = alive_forms.setdefault(src, {})
            display = bucket["entry"]["original"]
            if alive_cat == CAT_CHAIN:
                forms[display] = "chain"
            else:
                forms.setdefault(display, "direct")

    excluded_by_source, excluded_fps = _record_excluded_nodes(round_id, excluded_entries)
    _, chain_fps = _record_chain_failures(cfg, round_id, chain_failed_entries, log)
    rejected_by_source, rejected_fps = _record_rejected_nodes(
        cfg, round_id, rejected_entries, log)
    _prune_removed_nodes(by_name, excluded_by_source, excluded_fps, log,
                         chain_failed=chain_fps, rejected=rejected_fps)

    # A rejected node is failed above, but its name may still be sitting in a
    # bucket that was tested in an *earlier* round and is not re-tested here.
    # Publishing is what reads `alive_by_source`, so the guard has to be on the
    # publish path, not only on the ledger write.
    rejected_names = {entry["name"] for entry in (rejected_entries or [])}
    if rejected_names:
        for source in list(alive_by_source):
            alive_by_source[source] = [
                n for n in alive_by_source[source] if n not in rejected_names]

    # The fronts are tested nodes with ledger rows of their own, so they belong
    # in the total -- otherwise the panel's headline count would not match the
    # node table, which lists them. They are still absent from `sources`, so
    # `_publish_sources` never writes an export for them. Chain-failed nodes are
    # in neither `by_name` nor `sources`, so they are added explicitly.
    tested = {(entry["source"], entry["fp"]) for entry in by_name.values()}
    for source, fps in chain_fps.items():
        tested |= {(source, fp) for fp in fps}
    total = len(tested)
    alive = sum(len(v) for v in alive_by_source.values())
    suspect = policy.round_is_suspect(alive, previous_alive, cfg["policy"])
    note = None
    if suspect:
        note = f"alive {alive} < floor from previous {previous_alive}; not published"
        log("warn", f"护栏触发: 存活 {alive} 低于上轮 {previous_alive} 的安全下限，保留上轮结果")

    unverified = set(unverified or ())
    if unverified and excluded:
        # A node queued for egress verification but never checked is not
        # "verified clean". With a country filter configured, publishing it can
        # send the user's traffic out through the very country that filter
        # refuses -- and afterwards nothing in the ledger would say so, because
        # both `results.detail` and `results.country` stay empty. Refuse to
        # publish this round rather than silently downgrading the check to
        # nothing. (The nodes themselves keep their verdict: `_resolve_exit`
        # deliberately leaves an unverified node alone rather than killing it.)
        suspect = True
        note = f"{len(unverified)} 个节点未完成出口验证，本轮不发布"
        log("warn", f"出口验证未完成: {len(unverified)} 个节点未验证，"
                    f"已配置出口国别过滤 {sorted(excluded)}，保留上轮结果")
    elif unverified:
        log("warn", f"出口验证未完成: {len(unverified)} 个节点未验证"
                    f"（未配置出口国别过滤，按未验证发布）")

    if over_limit:
        # `verify.max_nodes` truncates verification on purpose, but a round that
        # skipped it must be distinguishable from one that completed it.
        log("warn", f"verify.max_nodes 截断：{len(over_limit)} 个节点本轮未做出口验证，"
                    "已按未验证发布")

    # A suspect round keeps the previous exports on disk -- that is the whole
    # point of the guardrail, so publishing is skipped rather than overwritten.
    if not suspect:
        live_fronts = [f["orig_proxy"] for f in (fronts or [])
                       if (results.get(f["proxy"]["name"]) or {}).get("reason") is None]
        if not _publish_sources(cfg, store, sources, alive_by_source,
                                proxy_by_name, countries, log,
                                chain_fronts=live_fronts, alive_forms=alive_forms):
            note = note or "publish disabled"

    return {"total": total, "alive": alive, "dropped": dropped, "restored": restored,
            "new_alive": new_alive, "suspect": suspect, "note": note,
            "countries": countries}


def verify_cfg_excludes(cfg):
    return cfg.get("verify", {}).get("exclude_countries", []) or []


def _previous_alive_count():
    """Alive count of the last completed round, skipping a suspect one.

    `LIMIT 1` is deliberate and used to read `LIMIT 2`, which suggested a
    comparison of two rounds that never happened: `db.one` returns rows[0] and
    the second row was never read. The fallback query below is what actually
    reaches past a suspect round.
    """
    row = db.one("SELECT ok, suspect, finished_at FROM rounds WHERE finished_at IS NOT NULL "
                 "ORDER BY id DESC LIMIT 1")
    if row and row.get("suspect"):
        row = db.one("SELECT ok FROM rounds WHERE finished_at IS NOT NULL AND suspect=0 "
                     "ORDER BY id DESC LIMIT 1")
    return int(row["ok"]) if row else 0


# Display tag for the front nodes published inside a chained export. Reserved
# so a front can never collide with a tested node's region-tagged name, and so
# an operator can see at a glance which entries are transit hops.
FRONT_EXPORT_TAG = "[前置] "
# Select-group name when `chain.front_source.name` itself would shadow a node.
FRONT_GROUP_FALLBACK = "chain-front"


def _resolve_chain_dialers(cfg, out, chain_fronts):
    """Make every published chained node's `dialer-proxy` resolve in the file.

    A chained node reaches a client as `dialer-proxy: <front>`, and that name
    must resolve *inside the published file* -- the probe's own kernel config
    is invisible to a client. Two regression layers led here:

    * The upstream form dangles outright. A real client kernel (v1.19.29)
      rejects the whole file:
      `proxy [[GB] GB-09 · SS] dialer-proxy [cdn] not found`.
    * The first rewrite fixed loadability but not topology: it emitted a
      select group named `chain.front_source.name` whose members were the
      export's own chained nodes -- every selection dialled itself. And the
      front proxies were never published at all, so no client could resolve a
      working front even by hand. Worse, Sub-Store drops `proxy-groups` when
      re-rendering a subscription for download (measured on this deployment:
      the exported group exists on disk, the downloaded sub has none), so a
      group-based rewrite does not survive the pipeline clients pull through.

    So the fronts themselves are published: `chain_fronts` -- the round's live
    front proxies -- enter the export under `[前置] ` names, and a dangling
    dialer points at them. A single-front pool points *directly at the front
    proxy*, which is the one form that survives Sub-Store; a multi-front pool
    needs the group for failover, and direct-file consumers get it, while
    Sub-Store consumers of a multi-front pool must keep the group themselves
    (or narrow the pool to one front).

    Dialers that already name a published proxy are left alone: the upstream
    author's own topology may be meaningful, and the client resolves it.

    With no live fronts available (`chain_fronts` empty -- every front failed
    this round, a direct round with no front history, or chaining switched
    off) there is nothing a client could resolve, so the chained nodes are
    dropped from the export instead of published broken: one dangling dialer
    makes a client kernel reject the whole file, taking every working direct
    node down with it.

    Returns (proxies, groups).
    """
    published = {p["name"] for p in out}
    dangling = [p for p in out
                if p.get(DIALER_FIELD) and p[DIALER_FIELD] not in published]
    if not dangling:
        return out, []
    if not chain_fronts or chain_block(cfg) is None:
        # Unpublishable as chained: drop exactly the dangling nodes and keep
        # every node whose dialer the file already resolves.
        drop = {id(p) for p in dangling}
        return [p for p in out if id(p) not in drop], []

    fronts, taken = [], set(published)
    for proxy in chain_fronts:
        proxy = {k: v for k, v in proxy.items() if k not in coremod.DROP_FIELDS}
        base = f"{FRONT_EXPORT_TAG}{str(proxy.get('name') or '').strip() or 'front'}"
        name, suffix = base, 2
        while name in taken:
            name = f"{base} #{suffix}"
            suffix += 1
        taken.add(name)
        fronts.append({**proxy, "name": name})
    if len(fronts) == 1:
        # One front needs no indirection, and a plain proxy reference is the
        # only rewrite that survives Sub-Store's group-stripping download.
        target, groups = fronts[0]["name"], []
    else:
        ref = (chain_block(cfg).get("front_source") or {})
        target = str(ref.get("name") or "").strip()
        if not target or target in taken:
            # Never shadow a node: mihomo keys proxies and groups in one
            # namespace, and a colliding group name points the dialer nowhere.
            target = FRONT_GROUP_FALLBACK
        if target in taken:
            suffix = 2
            while target in taken:
                target = f"{FRONT_GROUP_FALLBACK} #{suffix}"
                suffix += 1
        taken.add(target)
        groups = [{"name": target, "type": "select",
                   "proxies": [f["name"] for f in fronts]}]
    for p in dangling:
        p[DIALER_FIELD] = target
    return out + fronts, groups


def _export_proxies(names, proxy_by_name, countries, cfg, key, chain_fronts=(),
                    alive_forms=None):
    """Build the published list, collapsing repeats of the same endpoint.

    Upstream lists sometimes carry one server under two names (an "IPv6"
    alias with byte-identical connection fields). Publishing both would hand
    the client the same node twice.

    A second kind of repeat needs its own collapse: with both measurement
    switches on, one node is tested twice -- once chained, once with its dialer
    stripped -- and the two variants share a display name while differing in
    `fingerprint_proxy` (the dialer is a connection field, so the fingerprints
    are genuinely different). The fingerprint pass below cannot see that, and
    the client would receive two nodes called the same thing. The export speaks
    for the source's *configured* form, so the chained variant wins: the direct
    one exists to tell the panel whether the node also works on its own.

    With one exception, measured on 2026-09-28: a chained node whose chain
    failed the round (payload_fail -- the 204 answered but a real page pull
    hit a TLS reset) while its direct twin passed still landed in the export
    as the chained form, i.e. published wearing the exact dialer the round
    had just disproved. So the form follows the measurement: `alive_forms`
    (source -> display -> "chain" | "direct", see `_apply_and_publish`) says
    which form earned the alive verdict, and only a chain verdict publishes
    the chained shape. No form info at all keeps the old chained-wins rule.

    `chain_fronts` carries the round's live front proxies (see
    `_publish_sources`); the chained dialers and the front publishing are
    resolved by `_resolve_chain_dialers`.
    """
    forms = alive_forms or {}
    best = {}
    for name in names:
        proxy = proxy_by_name[name]
        display = str(proxy.get("name") or "")
        cur = best.get(display)
        if cur is None:
            best[display] = (name, proxy)
            continue
        chained = bool(proxy.get(DIALER_FIELD))
        cur_chained = bool(cur[1].get(DIALER_FIELD))
        if chained != cur_chained and chained == (forms.get(display) != "direct"):
            best[display] = (name, proxy)
    out, seen = [], {}
    tag = cfg["publish"].get("add_region_tag", True)
    for display, (name, proxy) in sorted(
            best.items(), key=lambda kv: (kv[1][1].get("type") or "", kv[0])):
        proxy = dict(proxy)
        fingerprint = coremod.fingerprint_proxy(proxy)
        if fingerprint in seen:
            continue
        seen[fingerprint] = name
        country = (countries.get(name) or {}).get("country")
        if tag and country:
            # Trust the measured exit over whatever tag the name already had.
            proxy["name"] = f"[{country}] {_LEADING_TAG.sub('', str(proxy['name']))}"
        out.append(proxy)
    return _resolve_chain_dialers(cfg, out, chain_fronts)


# Strings the YAML 1.1 resolver reads as something other than text. A client
# loading our export runs Go's yaml.v3, which still applies the 1.1 core
# schema, so an unquoted scalar like `123456e2` arrives as the float 473277000
# rather than the short ID the server actually speaks. PyYAML, which writes the
# file, uses the 1.2 schema and sees a plain string -- so it emits no quotes and
# the two ends disagree.
#
# Measured on the live kernel (v1.19.29), one node, only `short-id` varying:
#     short-id: 123456e2     -> exit 1  "invalid REALITY short ID"
#     short-id: '123456e2'   -> exit 0
#     short-id: deadbeef00     -> exit 0   (no `e`, never looks numeric)
# and across the 426 exported nodes exactly this one value was affected.
#
# The probe never notices, because its own kernel config is written as inline
# JSON -- `{"short-id": "123456e2"}` is quoted there and always parsed as text.
# Only the export goes through YAML, so only clients saw the breakage.
_YAML_11_SCALAR = re.compile(
    r"""^(?:
        [+-]?[0-9][0-9_]*(?:\.[0-9_]*)?[eE][+-]?[0-9]+   # 123456e2, 1.5e3
      | [+-]?(?:0[bB][01_]+|0[oO]?[0-7_]+|0[xX][0-9a-fA-F_]+)  # 0x1f, 0755
      | [+-]?(?:\.[0-9_]+|[0-9][0-9_]*(?:\.[0-9_]*)?)  # 123, 1.5
        (?::[0-5]?[0-9])+                               # 1:30 (sexagesimal)
      | ~|null|Null|NULL
      | true|True|TRUE|false|False|FALSE
      | y|Y|yes|Yes|YES|n|N|no|No|NO|on|On|ON|off|Off|OFF
    )$""",
    re.VERBOSE,
)


class _ExportDumper(yaml.SafeDumper):
    """A dumper that keeps YAML-1.1-lookalike strings quoted.

    Only the strings are touched; ints, floats and bools are emitted exactly
    as `SafeDumper` would.
    """


def _represent_str(dumper, value):
    style = "'" if _YAML_11_SCALAR.match(value) else None
    return dumper.represent_scalar("tag:yaml.org,2002:str", value, style=style)


_ExportDumper.add_representer(str, _represent_str)


def _write_export(cfg, key, names, proxy_by_name, countries, chain_fronts=(),
                  alive_forms=None):
    EXPORT_DIR.mkdir(parents=True, exist_ok=True)
    proxies, groups = _export_proxies(names, proxy_by_name, countries, cfg, key,
                                      chain_fronts=chain_fronts,
                                      alive_forms=alive_forms)
    payload = {"proxies": proxies}
    if groups:
        payload["proxy-groups"] = groups
    path = EXPORT_DIR / f"{key}.yaml"
    tmp = path.with_suffix(".yaml.tmp")
    tmp.write_text(
        yaml.dump(payload, Dumper=_ExportDumper, allow_unicode=True,
                  sort_keys=False, default_flow_style=False, width=4096),
        encoding="utf-8",
    )
    tmp.replace(path)
    (EXPORT_DIR / f"{key}.meta.json").write_text(
        json.dumps({"count": len(proxies), "updated_at": db.now()}, ensure_ascii=False),
        encoding="utf-8",
    )
    return path


def publish_keys(cfg):
    """The keys a push may touch: enabled sources, nothing else.

    A disabled source keeps its ledger so it can be re-enabled cheaply, but it
    is not published -- so it must not be pushed, and must not be *reported*
    either. Enumerating `[s["key"] for s in cfg["sources"]]` sent the sources
    the operator had just switched off straight into `push_exports`, where the
    missing export file turned into "还没有输出文件，先跑一轮": a line that reads
    like a failure for something deliberately turned off. Worse, in the window
    between disabling a source and the next round's `cleanup_exports` its stale
    YAML is still on disk, so the push resurrected it in Sub-Store -- and
    `link_substore` never prunes `-local` subs, so nothing would have undone it.

    `link_substore` and `exports_summary` already filtered on `enabled`; this
    is the one path that did not.

    A muted source (`export: false`) is skipped for the same reason as a
    disabled one: it owns no file, so pushing it reports a missing export as a
    failure the operator cannot act on.
    """
    keys = []
    for source in cfg.get("sources", []):
        if not source.get("enabled", True) or not source.get("export", True):
            continue
        key = str(source.get("key") or "").strip()
        if key:
            keys.append(key)
    return keys


def _push_record(key, name, ok, level, text, count=0):
    return {"key": key, "name": name, "ok": ok, "level": level,
            "text": text, "count": count}


def push_exports(cfg, store, keys, log=None):
    """Upsert the exported lists into Sub-Store as local subscriptions.

    Every write is an upsert. The previous pipelines only ever PATCHed a name
    they assumed still existed, so one rename upstream turned the whole
    write-back into a silent no-op that still exited 0.

    Returns one record per key (`{key, name, ok, level, text, count}`) rather
    than a sentence per key, so the dashboard can render each outcome as its
    own row instead of re-parsing a string. `push_report` keeps the old
    one-line-per-key text for the CLI.
    """
    log = log or db.log
    prefix = cfg["publish"].get("prefix", "probe")
    results = []
    for key in keys:
        content = read_export(key)
        if content is None:
            results.append(_push_record(
                key, key, ok=False, level="warn",
                text=f"{key}: 还没有输出文件，先跑一轮"))
            continue
        meta = export_meta(key) or {}
        count = meta.get("count", 0)
        # Distinct from the pull-mode name so the two integration styles do not
        # overwrite each other.
        sub_name = f"{prefix}-{key}-local"
        payload = {
            "name": sub_name,
            "displayName": f"{key} 测活 ({count})",
            "source": "local",
            "url": "",
            "content": content,
            "mergeSources": "",
            "ignoreFailedRemoteSub": "quiet",
            "passThroughUA": False,
            "process": [],
        }
        try:
            action = store.upsert("sub", sub_name, payload)
            results.append(_push_record(
                key, sub_name, ok=True, level="ok", count=count,
                text=f"{sub_name}: {action} ({count} 节点)"))
            log("info", f"Sub-Store 订阅 {sub_name} {action}，{count} 节点")
        except StoreError as exc:
            results.append(_push_record(
                key, sub_name, ok=False, level="error", count=count,
                text=f"{sub_name}: 失败 {exc}"))
            log("error", f"写入 Sub-Store 失败 {sub_name}: {exc}")

    _prune_local_subs(cfg, store, keys, results, log)
    return results


def _prune_local_subs(cfg, store, keys, results, log):
    """Delete the `-local` subs of sources this push no longer publishes.

    `push_exports` only ever upserts the keys it is handed, so a source that
    stops being published keeps its last `-local` subscription forever -- and
    since the content is embedded at push time, that copy goes on serving the
    nodes it had when it was retired. A muted source is the sharp case: its
    export file is deleted, so the stale sub resolves to zero nodes and
    Sub-Store answers HTTP 500 for it, which reads as a broken deployment
    rather than a deliberate mute.

    Scoped to `source == "local"` with our own prefix. A remote sub of the same
    name is `link_substore`'s to manage, and anything pointing elsewhere is not
    ours to remove.
    """
    prefix = cfg["publish"].get("prefix", "probe")
    # Two inputs, deliberately: `keys` is what *this* push wrote, and
    # `publish_keys(cfg)` is what may still legitimately exist. Pruning on
    # `keys` alone deleted every other source's `-local` subscription during a
    # single-source round (`POST /api/run {"source": ...}` or `--source` on the
    # CLI), because that path hands `push_exports` exactly one key -- an
    # unrelated source lost its published list because the operator tested one
    # of them. Retired sources are still removed: they are absent from
    # `publish_keys`.
    keep = {f"{prefix}-{key}-local" for key in keys} | {
        f"{prefix}-{key}-local" for key in publish_keys(cfg)}
    try:
        existing = store.get_json("/api/subs") or []
    except StoreError:
        return
    for item in existing:
        name = str(item.get("name") or "")
        if not name.endswith("-local") or not name.startswith(prefix + "-"):
            continue
        if name in keep or item.get("source") != "local":
            continue
        try:
            store._request("DELETE", "/api/sub/" + urllib.parse.quote(name, safe=""))
            results.append(_push_record(
                name[len(prefix) + 1:-len("-local")], name, ok=True, level="ok",
                text=f"{name}: 已移除（来源不再推送）"))
            log("info", f"Sub-Store 移除过期本地订阅 {name}")
        except StoreError as exc:
            results.append(_push_record(
                name[len(prefix) + 1:-len("-local")], name, ok=False,
                level="error", text=f"{name}: 移除失败 {exc}"))


def push_report(results):
    """One plain text line per push record, in the order pushed."""
    return [record["text"] for record in results]


def push_summary(results):
    """A one-line headline for the push result notice."""
    if not results:
        return "没有已启用的来源，未推送任何内容"
    done = [r for r in results if r["ok"]]
    failed = [r for r in results if not r["ok"]]
    head = f"已推送 {len(done)} 个订阅" if done else "没有订阅被推送"
    if failed:
        head += f"，{len(failed)} 个未推送"
    return head


def export_token(cfg):
    """The credential an export URL carries: read-only, never the admin one.

    Falls back to `auth.token` only when no publish token exists, which cannot
    happen through `config.load()` (it mints one) and keeps a hand-built cfg in
    a test or a one-off script working.
    """
    return (str(cfg.get("publish", {}).get("token") or "").strip()
            or str(cfg.get("auth", {}).get("token") or ""))


def export_url(cfg, key):
    """The pull URL to paste into Sub-Store or a client.

    `quote` on the key: source keys may be non-ASCII (the UI allows Chinese
    names), and an unencoded path segment produces a URL that some clients and
    proxies reject. `link_substore` already quoted; this did not, so the two
    callers disagreed about the same key.
    """
    return f"/api/export/{urllib.parse.quote(key)}.yaml?token={export_token(cfg)}"


QUICK_SETTING = {
    "type": "Quick Setting Operator",
    "args": {"useless": "DISABLED", "udp": "DEFAULT", "scert": "DEFAULT",
             "tfo": "DEFAULT", "vmess aead": "DEFAULT", "reuse": "DEFAULT",
             "block-quic": "DEFAULT", "ecn": "DEFAULT", "ip-version": "DEFAULT"},
}



def cleanup_exports(cfg):
    """Remove export files whose source is gone, disabled, or muted.

    A disabled source keeps its convergence history in the database so it can
    be re-enabled cheaply, but its export must not keep serving a snapshot that
    stopped updating when it was switched off. A source with `export: false` is
    still tested, yet it owns no file either -- same reasoning, and without this
    the last file written before it was muted would sit there forever.
    """
    keys = set(export_keys(cfg))
    removed = []
    try:
        existing = list(EXPORT_DIR.glob("*.yaml"))
    except OSError:
        return removed
    for path in existing:
        key = path.name[:-5]
        if key not in keys:
            try:
                path.unlink()
                meta = EXPORT_DIR / f"{key}.meta.json"
                if meta.exists():
                    meta.unlink()
                removed.append(key)
            except OSError:
                continue
    return removed


def _collection_payload(prefix, subscriptions):
    """The aggregate collection body; `subscriptions` may be empty on purpose."""
    return {
        "name": prefix,
        "displayName": "测活聚合",
        "remark": "mihomo-test 真实内核测活结果（自动生成）",
        "mergeSources": "",
        "ignoreFailedRemoteSub": "quiet",
        "passThroughUA": False,
        "subscriptions": list(subscriptions),
        "process": [QUICK_SETTING],
    }


def link_signature(cfg):
    """Everything `link_substore`'s output depends on, as a comparable value.

    The server re-runs `link_substore` after every sources save so a removed
    source cannot linger in Sub-Store. But a save that only flips a measurement
    switch (`relay`, `direct`, `chain`) reaches it as a sources patch too, and
    re-running the sync then is one upsert per source plus a full sub listing
    for a byte-identical outcome -- the round is what reads those fields, and
    the round never talks to Sub-Store through this path.

    Only the fields `link_substore` reads are projected: `key`, `kind`, `name`,
    `label`, `enabled`, `export`, plus `publish.prefix` and `publish.hostname`.
    The projections mirror `normalize_sources`' own defaulting (`kind` falls
    back to collection, `label` to the name, `key` to the name) so a config
    that never went through `load()` -- a `serve()` caller's hand-built dict --
    still compares equal to its normalized form. The mirroring errs in the safe
    direction: it can only make the signatures agree for sources the sync
    would treat identically, never skip a sync for a source it would not.

    Export *content* changes every round, but this signature is only ever
    compared between the config before and after one save -- a round running in
    between publishes content on its own and never consults the comparison.
    """
    pub = cfg.get("publish") or {}
    sources = tuple(sorted(
        (str(s.get("key") or s.get("name") or ""),
         str(s.get("kind") if s.get("kind") in cfgmod.SOURCE_KINDS
             else "collection"),
         str(s.get("name") or ""),
         str(s.get("label") or s.get("name") or ""),
         bool(s.get("enabled", True)), bool(s.get("export", True)))
        for s in cfg.get("sources") or []))
    return (str(pub.get("prefix") or ""), str(pub.get("hostname") or ""), sources)


def link_substore(cfg, store, log=None):
    """Point Sub-Store at the exporter for every enabled source.

    Creates/refreshes one remote sub per source plus the bundle collection, and
    removes the remote subs this function previously created for sources that
    are no longer selected. Only objects that point back at our own host are
    ever pruned, so the push-mode `-local` subs are left alone.

    Two rules keep the cleanup from being destructive. A prune keys on what
    *should* exist (`expected`), not on which writes happened to succeed, so a
    transient 5xx cannot delete a healthy object; and the collection is always
    brought in line with whatever is about to be deleted, so it is never left
    naming subs that no longer exist.
    """
    log = log or db.log
    prefix = cfg["publish"].get("prefix", "probe")
    host = cfg["publish"].get("hostname", "")
    # The read-only token, not the admin one. This URL is POSTed to a Sub-Store
    # backend whose address is itself a config value, so using `auth.token` here
    # meant "point substore.backend at a host you control, press 同步联动, and
    # you are handed the admin credential permanently" -- which on this
    # deployment also reaches the host docker socket.
    token = export_token(cfg)
    if not host:
        return ["publish.hostname 未配置，无法生成远程订阅地址"]

    # Muted sources are tested but own no file, so there is nothing to link --
    # and Sub-Store answers HTTP 500 for a subscription that resolves to zero
    # nodes, which is exactly what their URL would return.
    sources = [s for s in cfg["sources"]
               if s.get("enabled", True) and s.get("export", True)]
    members, messages = [], []
    # What *should* exist in Sub-Store, decided before any write is attempted.
    # The prune below keys on this and never on `members`, because `members`
    # only holds the writes that happened to succeed: one 5xx from the backend
    # left a perfectly healthy object out of `members`, the prune read that as
    # "no longer needed", and DELETEd a subscription that was fine -- a
    # recoverable error turned into data loss.
    expected = set()
    for source in sources:
        key = str(source.get("key") or "").strip()
        if not key:
            messages.append(f"{source.get('name')}: 缺少 key，跳过")
            continue
        # Sub-Store answers HTTP 500 for any subscription that resolves to zero
        # nodes, so a source with nothing alive must not be linked at all.
        meta = export_meta(key) or {}
        count = int(meta.get("count") or 0)
        if read_export(key) is None or count == 0:
            messages.append(f"{key}: 暂无存活节点，跳过联动")
            log("warn", f"{key} 暂无存活节点，未联动 Sub-Store")
            continue
        name = f"{prefix}-{key}"
        expected.add(name)
        url = (f"https://{host}/api/export/{urllib.parse.quote(key)}.yaml"
               f"?token={token}")
        payload = {
            "name": name,
            "displayName": f"{source.get('label') or source['name']} 测活(真实内核)",
            "source": "remote",
            "url": url,
            "content": "",
            "mergeSources": "",
            "ignoreFailedRemoteSub": "quiet",
            "passThroughUA": False,
            "process": [QUICK_SETTING],
        }
        try:
            action = store.upsert("sub", name, payload)
        except StoreError as exc:
            messages.append(f"{name}: 失败 {exc}")
            continue
        messages.append(f"{name}: {action}")
        members.append(name)
        log("info", f"Sub-Store 联动 {name} {action}")

    # Read the live object list once, before deciding anything: both the
    # collection's membership and the prune below are answered from it.
    try:
        existing = store.get_json("/api/subs") or []
    except StoreError:
        existing = []
    existing_names = {str(item.get("name") or "") for item in existing}

    # Every link object of ours that no configured source wants any more.
    # Keyed on `expected` (what should exist), never on `members` (what
    # happened to succeed) -- see the note where `expected` is built.
    retired = []
    for item in existing:
        name = str(item.get("name") or "")
        if name in expected or not name.startswith(prefix + "-"):
            continue
        # Only our own link objects: remote, and pointing at this exporter.
        if item.get("source") != "remote" or host not in str(item.get("url") or ""):
            continue
        retired.append(name)

    # The collection may only name objects that are actually there: what this
    # run just wrote, plus the ones a failed write left untouched but that are
    # already in Sub-Store.
    wanted = list(members) + sorted(
        n for n in expected - set(members) if n in existing_names)

    if wanted:
        try:
            action = store.upsert("collection", prefix,
                                  _collection_payload(prefix, wanted))
            messages.append(f"{prefix}: {action} ({len(wanted)} 成员)")
        except StoreError as exc:
            messages.append(f"{prefix}: 失败 {exc}")
    elif retired:
        # Nothing wanted, and this run is about to delete objects the existing
        # collection almost certainly names. An empty collection is the only
        # shape that still renders: one pointing at deleted subs is the HTTP
        # 500 that the zero-node skip exists to prevent. Doing this in the same
        # breath as the prune is the whole point -- previously the collection
        # was left alone while its members were deleted under it.
        try:
            action = store.upsert("collection", prefix, _collection_payload(prefix, []))
            messages.append(f"{prefix}: 无可用来源，聚合集合已清空 ({action})")
        except StoreError as exc:
            messages.append(f"{prefix}: 清空聚合集合失败 {exc}")
    elif expected:
        # Every write failed, but the objects already out there are still the
        # right ones (name and URL are deterministic) and nothing is being
        # deleted: leave the collection exactly as it is rather than acting on
        # a transient backend error.
        messages.append(f"{prefix}: 联动写入全部失败，保留现有聚合集合")
        log("error", "Sub-Store 联动写入全部失败，未改动聚合集合")
    else:
        # Nothing to link and nothing to delete. Say which reason applied: the
        # old wording blamed 「暂无存活节点」 even when no source was enabled
        # with 导出 on, which sent the operator looking at the wrong thing.
        reason = ("没有启用且开启导出的来源" if not sources
                  else "没有可联动的来源（暂无存活节点或缺少 key）")
        messages.append(f"{prefix}: {reason}，聚合集合未更新")
        log("warn", f"{reason}，未更新 Sub-Store 聚合集合")

    for name in retired:
        try:
            store._request("DELETE", "/api/sub/" + urllib.parse.quote(name, safe=""))
            messages.append(f"{name}: 已移除（来源不再启用）")
            log("info", f"Sub-Store 联动移除 {name}")
        except StoreError as exc:
            messages.append(f"{name}: 移除失败 {exc}")
    return messages


def read_export(key):
    # Keys are user-supplied now, so resolve inside the export directory only.
    try:
        safe = cfgmod.validate_key(key)
    except ValueError:
        return None
    path = EXPORT_DIR / f"{safe}.yaml"
    if path.parent != EXPORT_DIR:
        return None
    try:
        return path.read_text(encoding="utf-8")
    except OSError:
        return None


def export_meta(key):
    try:
        safe = cfgmod.validate_key(key)
    except ValueError:
        return None
    path = EXPORT_DIR / f"{safe}.meta.json"
    if path.parent != EXPORT_DIR:
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
