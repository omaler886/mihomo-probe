"""One testing round: fetch sources, test through the kernel, converge, publish."""
import ipaddress
import collections
import json
import os
import re
import socket
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
from .store import Client, StoreError

ROUND_STATE = cfgmod.DATA / "round.state.json"


class RoundTimeout(RuntimeError):
    """A round exceeded its budget; the lock is released so the schedule lives."""


def _write_state(phase, round_id=None):
    """Record what a round is doing, so a stall is visible from outside.

    `ts` is UTC to match db.now(); `epoch` is the same instant as a number,
    used for liveness checks. The two must agree -- they are the same moment
    expressed twice, and a mismatch is what made a crashed round's leftover
    state look like it belonged to a different one.
    """
    try:
        cfgmod.DATA.mkdir(parents=True, exist_ok=True)
        tmp = ROUND_STATE.with_suffix(".json.tmp")
        tmp.write_text(json.dumps({
            "phase": phase, "round_id": round_id, "pid": os.getpid(),
            "ts": time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()),
            "epoch": time.time(),
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
    _write_state(phase, round_id)
    if deadline is not None and time.monotonic() > deadline:
        raise RoundTimeout(f"round exceeded its budget at phase {phase}")

EXPORT_DIR = cfgmod.DATA / "exports"
# A region tag we previously prepended, so re-tagging stays idempotent.
_LEADING_TAG = re.compile(r"^\[[A-Z]{2}(?:-[A-Za-z0-9]+)?\]\s+")

# Deterministic failures: testing again with the same parameters cannot
# change the answer, so the retry budget is not spent on them.
#
# `controller_error` is deliberately absent. It means our own kernel API could
# not be reached, which is transient and says nothing about the node, so it is
# worth spending a retry on.
TERMINAL_REASONS = {"bad_request", "bad_delay", "bad_response", "unreachable"}

_round_lock = threading.Lock()

# The round this process currently has open. `_abandon_round` used to recover
# the id solely from round.state.json, so a crash on a machine where that file
# could not be written left the ledger row open forever. This is the in-process
# record that does not depend on the filesystem.
_CURRENT_ROUND_ID = None


def _set_current_round(round_id):
    global _CURRENT_ROUND_ID
    _CURRENT_ROUND_ID = round_id


class Busy(RuntimeError):
    pass


def test_one(core, entry, test_cfg):
    """Test one node, retrying only where a retry can plausibly help."""
    urls = test_cfg["targets"]
    if not urls:
        raise ValueError("no test targets configured")
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
    while attempts < max_attempts:
        url = urls[attempts % len(urls)]
        attempts += 1
        delay, reason, detail = core.delay(entry["mihomo"], url, timeout, expected)
        if reason is None:
            return {"delay_ms": delay, "reason": None, "detail": "",
                    "attempts": attempts, "url": url}
        if reason in TERMINAL_REASONS:
            break
        if reason == "timeout":
            # A timeout is the one failure a bigger budget can overturn.
            timeout = long_timeout
        if attempts < max_attempts:
            time.sleep(pause)
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


def resolve_servers(entries):
    """Map every distinct server host to its resolved addresses."""
    hosts = {}
    for entry in entries:
        server = str(entry["proxy"].get("server") or "").strip()
        if not server or server in hosts:
            continue
        if _is_literal_ip(server):
            hosts[server] = [server]
            continue
        try:
            infos = socket.getaddrinfo(server, None)
            hosts[server] = sorted({i[4][0] for i in infos})
        except socket.gaierror:
            hosts[server] = []
    return hosts


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
    """
    verify_cfg = cfg.get("verify", {})
    if not verify_cfg.get("entry_check", True):
        return list(entries), []
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
            # neither vantage resolved it: let the kernel try its own path
            test_entries += group
            continue
        testable = [ip for ip in candidates
                    if (countries.get(ip) or "").upper() not in banned]
        if not testable:
            for entry in group:
                excluded.append(dict(entry, fp=_orig_fp(entry["proxy"])))
            continue
        for entry in group:
            orig_fp = _orig_fp(entry["proxy"])
            for ip in testable:
                variant = dict(entry["proxy"])
                variant["server"] = ip
                test_entries.append({**entry, "proxy": variant, "fp": orig_fp,
                                     "orig_proxy": entry["proxy"], "test_ip": ip})
    return test_entries, excluded


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
    """Fingerprint of the original (domain-form) node, matching the ledger."""
    return coremod.fingerprint_proxy(
        {k: v for k, v in proxy.items() if k not in coremod.DROP_FIELDS})


def collect_entries(store, sources):
    """Fetch every enabled source and flatten it into testable entries."""
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
        for proxy in proxies:
            name = str(proxy.get("name") or f"node-{index}")
            entries.append({"source": source["key"], "name": name, "proxy": proxy, "index": index})
            index += 1
    return entries, errors


def run_round(cfg, trigger="manual", only_source=None, log=db.log):
    """Execute one full round; returns a summary dict.

    Guarded by both an in-process lock and a file lock: the CLI and the
    service are separate processes, and letting them overlap tests the same
    nodes twice, which advances the convergence counter twice in one round.
    """
    if not _round_lock.acquire(blocking=False):
        raise Busy("a round is already running")
    handle = None
    try:
        handle = _acquire_file_lock()
        return _run_round(cfg, trigger, only_source, log)
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


def _run_round(cfg, trigger, only_source, log):
    started = time.time()
    deadline = time.monotonic() + _budget(cfg)
    round_id = db.start_round(trigger)
    _set_current_round(round_id)
    _write_state("start", round_id)
    log("info", f"第 {round_id} 轮开始 (trigger={trigger})")
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
    test_entries, excluded_entries = classify_and_expand(entries, cfg, log)
    if excluded_entries:
        log("info", f"{len(excluded_entries)} 个节点入口在受限 ISP 上，跳过测试")
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
        strip_ech=bool(cfg.get("verify", {}).get("strip_ech")))
    for item in dropped:
        log("warn", f"剔除不可用配置的节点 {item['name']}: {item['why']}")
    log("info", f"内核配置就绪: {len(proxies)} 节点, 剔除 {len(dropped)}")
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
    results = _test_all(core, mapping, test_cfg, concurrency)

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
                                 unverified=unverified, over_limit=over_limit)
    configured = [s.get("key") for s in cfg["sources"] if s.get("key")]
    removed = db.delete_sources_not_in(configured)
    if removed:
        log("info", f"清理已取消勾选来源的 {removed} 条节点记录")
    # A disabled source keeps its rows on purpose, but its verdicts stop being
    # maintained. Demote them so the panel's `alive` count only claims what is
    # still being tested -- 25 of vps's 315 `alive` were a disabled source's
    # snapshot frozen 21 hours earlier.
    demoted = db.demote_disabled_sources([s["key"] for s in sources])
    if demoted:
        log("info", f"{demoted} 条记录属于已停用来源，状态降级为 unknown（历史保留）")
    stale = cleanup_exports(cfg)
    if stale:
        log("info", f"清理不再启用的来源导出文件: {', '.join(stale)}")
    elapsed = round(time.time() - started, 1)
    db.finish_round(round_id, total=len(mapping), ok=summary["alive"],
                    failed=summary["total"] - summary["alive"], dropped=summary["dropped"],
                    restored=summary["restored"], suspect=1 if summary["suspect"] else 0,
                    note=summary["note"], duration_s=elapsed)
    log("info", f"第 {round_id} 轮结束: 活 {summary['alive']}/{summary['total']}, "
                f"降级 {summary['dropped']}, 恢复 {summary['restored']}, "
                f"新入 {summary.get('new_alive', 0)}, 耗时 {elapsed}s"
                + ("  ⚠️ 本轮可疑，未发布" if summary["suspect"] else ""))
    summary["round_id"] = round_id
    summary["duration_s"] = elapsed
    try:
        ROUND_STATE.unlink()
    except OSError:
        pass
    _maybe_alert(cfg, summary, log)
    return summary



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
    if rid and round_id is None and not _state_belongs_to(rid):
        log("warn", f"round.state.json 指向第 {rid} 轮，但该轮已结束；"
                    "视为陈旧残留，不据此关闭任何轮次")
        rid = None
    if rid:
        try:
            db.finish_round(rid, note=f"aborted: {why}"[:200], duration_s=0)
        except (ValueError, TypeError):
            pass
    notifier.send(cfg, alert_key, title, why, level="error", log=log)


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


def _test_all(core, mapping, test_cfg, concurrency):
    from concurrent.futures import ThreadPoolExecutor

    results = {}

    def run(entry):
        return entry["mihomo"], test_one(core, entry, test_cfg)

    with ThreadPoolExecutor(max_workers=max(1, concurrency)) as pool:
        for name, outcome in pool.map(run, mapping):
            results[name] = outcome
    return results


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

    Returns ("drop" | "restore" | None, [alive primary names]).
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
    db.upsert_node(source, fingerprint, display, **fields)

    db.record_result(round_id, source, fingerprint, display,
                     "ok" if ok else "fail", delay, reason, country,
                     max(o["attempts"] for o in bucket["outcomes"]), detail[:200])
    return transition, ([primary] if ok else [])


def _record_excluded_nodes(round_id, excluded_entries):
    """Write ledger rows for nodes skipped by the entry-IP filter.

    Excluded nodes are never tested, so they are absent from `by_name` -- and
    without recording them here the prune in `_prune_removed_nodes` would wipe
    their records every single round.
    """
    by_source = collections.defaultdict(list)
    fps = collections.defaultdict(set)
    for entry in excluded_entries or []:
        by_source[entry["source"]].append(entry)
    for source, entries in by_source.items():
        for entry in entries:
            # Same identity rule as tested nodes: fingerprint the stripped
            # proxy, so a node flipping between testable and excluded keeps
            # one ledger entry instead of growing a duplicate.
            stripped = {k: v for k, v in entry["proxy"].items()
                        if k not in coremod.DROP_FIELDS}
            fingerprint = coremod.fingerprint_proxy(stripped)
            fps[source].add(fingerprint)
            if db.get_node(source, fingerprint) is None:
                db.upsert_node(source, fingerprint, entry["name"])
            db.upsert_node(
                source, fingerprint, entry["name"],
                status=policy.EXCLUDED, last_reason="entry_cn",
                proto=entry["proxy"].get("type"), server=entry["proxy"].get("server"))
            db.record_result(round_id, source, fingerprint, entry["name"],
                             "excluded", None, "entry_cn", None, 0, "")
    return by_source, fps


def _prune_removed_nodes(by_name, excluded_by_source, excluded_fps, log):
    """Drop ledger rows for nodes upstream no longer lists.

    Without this an unlisted node lingers as `unknown` forever and inflates the
    panel's node count.
    """
    all_sources = set(entry["source"] for entry in by_name.values()) | set(excluded_by_source)
    for source in all_sources:
        seen = sorted({entry["fp"] for entry in by_name.values() if entry["source"] == source}
                      | excluded_fps.get(source, set()))
        gone = db.delete_nodes_not_in(source, seen)
        if gone:
            log("info", f"{source}: 上游已移除 {gone} 个节点，清理记录")


def _publish_sources(cfg, store, sources, alive_by_source, proxy_by_name, countries, log):
    """Write each source's export file, and optionally upsert into Sub-Store."""
    if not cfg["publish"].get("enabled", True):
        return False
    for source in sources:
        key = source["key"]
        _write_export(cfg, key, alive_by_source.get(key, []), proxy_by_name, countries)
    if cfg["publish"].get("push_to_substore"):
        push_exports(cfg, store, [s["key"] for s in sources], log)
    return True


def _apply_and_publish(cfg, round_id, store, by_name, proxies, results, countries, sources, log,
                       excluded_entries=None, unverified=None, over_limit=None):
    """Converge every tested node, reconcile the ledger, then publish.

    Thin orchestrator: the per-node state machine lives in `_converge_bucket`,
    untestable-entry bookkeeping in `_record_excluded_nodes`, ledger hygiene in
    `_prune_removed_nodes`, and the export write in `_publish_sources`. The
    guardrail decision stays here because it gates publishing and depends on the
    totals only this function has.
    """
    excluded = {c.upper() for c in verify_cfg_excludes(cfg)}
    domain_pass = str(cfg.get("verify", {}).get("domain_pass", "any")).lower()
    proxy_by_name = _index_original_proxies(by_name, proxies)
    alive_by_source = {}
    dropped = restored = new_alive = 0
    previous_alive = _previous_alive_count()

    for bucket in group_by_fingerprint(results, by_name).values():
        transition, alive_names = _converge_bucket(
            cfg, round_id, bucket, proxy_by_name, countries, excluded, domain_pass, log)
        if transition == "drop":
            dropped += 1
        elif transition == "restore":
            restored += 1
        elif transition == "new":
            new_alive += 1
        if alive_names:
            alive_by_source.setdefault(bucket["entry"]["source"], []).extend(alive_names)

    excluded_by_source, excluded_fps = _record_excluded_nodes(round_id, excluded_entries)
    _prune_removed_nodes(by_name, excluded_by_source, excluded_fps, log)

    total = len(by_name)
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
        if not _publish_sources(cfg, store, sources, alive_by_source,
                                proxy_by_name, countries, log):
            note = note or "publish disabled"

    return {"total": total, "alive": alive, "dropped": dropped, "restored": restored,
            "new_alive": new_alive, "suspect": suspect, "note": note,
            "countries": countries}


def verify_cfg_excludes(cfg):
    return cfg.get("verify", {}).get("exclude_countries", []) or []


def _previous_alive_count():
    row = db.one("SELECT ok, suspect, finished_at FROM rounds WHERE finished_at IS NOT NULL "
                 "ORDER BY id DESC LIMIT 2")
    if row and row.get("suspect"):
        row = db.one("SELECT ok FROM rounds WHERE finished_at IS NOT NULL AND suspect=0 "
                     "ORDER BY id DESC LIMIT 1")
    return int(row["ok"]) if row else 0


def _export_proxies(names, proxy_by_name, countries, cfg):
    """Build the published list, collapsing repeats of the same endpoint.

    Upstream lists sometimes carry one server under two names (an "IPv6"
    alias with byte-identical connection fields). Publishing both would hand
    the client the same node twice.
    """
    out, seen = [], {}
    tag = cfg["publish"].get("add_region_tag", True)
    for name in sorted(names, key=lambda n: (proxy_by_name[n].get("type") or "", n)):
        proxy = dict(proxy_by_name[name])
        fingerprint = coremod.fingerprint_proxy(proxy)
        if fingerprint in seen:
            continue
        seen[fingerprint] = name
        country = (countries.get(name) or {}).get("country")
        if tag and country:
            # Trust the measured exit over whatever tag the name already had.
            proxy["name"] = f"[{country}] {_LEADING_TAG.sub('', str(proxy['name']))}"
        out.append(proxy)
    return out


def _write_export(cfg, key, names, proxy_by_name, countries):
    EXPORT_DIR.mkdir(parents=True, exist_ok=True)
    proxies = _export_proxies(names, proxy_by_name, countries, cfg)
    path = EXPORT_DIR / f"{key}.yaml"
    tmp = path.with_suffix(".yaml.tmp")
    tmp.write_text(
        yaml.safe_dump({"proxies": proxies}, allow_unicode=True, sort_keys=False),
        encoding="utf-8",
    )
    tmp.replace(path)
    (EXPORT_DIR / f"{key}.meta.json").write_text(
        json.dumps({"count": len(proxies), "updated_at": db.now()}, ensure_ascii=False),
        encoding="utf-8",
    )
    return path


def push_exports(cfg, store, keys, log=None):
    """Upsert the exported lists into Sub-Store as local subscriptions.

    Every write is an upsert. The previous pipelines only ever PATCHed a name
    they assumed still existed, so one rename upstream turned the whole
    write-back into a silent no-op that still exited 0.
    """
    log = log or db.log
    prefix = cfg["publish"].get("prefix", "probe")
    results = []
    for key in keys:
        content = read_export(key)
        if content is None:
            results.append(f"{key}: 还没有输出文件，先跑一轮")
            continue
        meta = export_meta(key) or {}
        # Distinct from the pull-mode name so the two integration styles do not
        # overwrite each other.
        sub_name = f"{prefix}-{key}-local"
        payload = {
            "name": sub_name,
            "displayName": f"{key} 测活 ({meta.get('count', 0)})",
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
            results.append(f"{sub_name}: {action} ({meta.get('count', 0)} 节点)")
            log("info", f"Sub-Store 订阅 {sub_name} {action}，{meta.get('count', 0)} 节点")
        except StoreError as exc:
            results.append(f"{sub_name}: 失败 {exc}")
            log("error", f"写入 Sub-Store 失败 {sub_name}: {exc}")
    return results


def export_url(cfg, key):
    """The pull URL to paste into Sub-Store or a client."""
    token = cfg["auth"]["token"]
    return f"/api/export/{key}.yaml?token={token}"


QUICK_SETTING = {
    "type": "Quick Setting Operator",
    "args": {"useless": "DISABLED", "udp": "DEFAULT", "scert": "DEFAULT",
             "tfo": "DEFAULT", "vmess aead": "DEFAULT", "reuse": "DEFAULT",
             "block-quic": "DEFAULT", "ecn": "DEFAULT", "ip-version": "DEFAULT"},
}



def cleanup_exports(cfg):
    """Remove export files whose source is gone *or* currently disabled.

    A disabled source keeps its convergence history in the database so it can
    be re-enabled cheaply, but its export must not keep serving a snapshot that
    stopped updating when it was switched off.
    """
    keys = {s.get("key") for s in cfg.get("sources", [])
            if s.get("key") and s.get("enabled", True)}
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
def link_substore(cfg, store, log=None):
    """Point Sub-Store at the exporter for every enabled source.

    Creates/refreshes one remote sub per source plus the bundle collection, and
    removes the remote subs this function previously created for sources that
    are no longer selected. Only objects that point back at our own host are
    ever pruned, so the push-mode `-local` subs are left alone.
    """
    log = log or db.log
    prefix = cfg["publish"].get("prefix", "probe")
    host = cfg["publish"].get("hostname", "")
    token = cfg["auth"]["token"]
    if not host:
        return ["publish.hostname 未配置，无法生成远程订阅地址"]

    sources = [s for s in cfg["sources"] if s.get("enabled", True)]
    members, messages = [], []
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

    if members:
        collection = {
            "name": prefix,
            "displayName": "测活聚合",
            "remark": "mihomo-test 真实内核测活结果（自动生成）",
            "mergeSources": "",
            "ignoreFailedRemoteSub": "quiet",
            "passThroughUA": False,
            "subscriptions": members,
            "process": [QUICK_SETTING],
        }
        try:
            action = store.upsert("collection", prefix, collection)
            messages.append(f"{prefix}: {action} ({len(members)} 成员)")
        except StoreError as exc:
            messages.append(f"{prefix}: 失败 {exc}")
    else:
        messages.append(f"{prefix}: 所有来源都暂无存活节点，未更新聚合集合")
        log("warn", "所有来源都无存活节点，未更新 Sub-Store 聚合集合")

    try:
        existing = store.get_json("/api/subs") or []
    except StoreError:
        existing = []
    for item in existing:
        name = item.get("name") or ""
        if name in members or not name.startswith(prefix + "-"):
            continue
        # Only our own link objects: remote, and pointing at this exporter.
        if item.get("source") != "remote" or host not in str(item.get("url") or ""):
            continue
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
