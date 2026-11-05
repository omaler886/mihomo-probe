"""Entry point: run the dashboard, or execute a single round from the CLI.

Two roles share this codebase (workstreams/20 裁决 5): the Controller
(`serve`, the existing panel + scheduler) and the Probe Agent (`agent`, a new
mode). The agent is the 指导 §2 architecture's leaf: it accepts Jobs over
HTTP, measures them through its own mihomo kernel, posts ProbeResults back,
and touches nothing else -- no Sub-Store, no Gist, no business config. That
separation is what keeps "one node identity ledger" true when several probes
report into one Controller.
"""
import argparse
import hmac
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from . import config as cfgmod
from . import core as coremod
from . import db
from . import engine
from . import ipmap as ipmap_mod
from . import probe_dispatch
from . import server

# --------------------------------------------------------------- Probe Agent
# 端口与既有角色错开：Controller 内核 19190/19194/19200、ipmap 19191/19494/
# 19300（ipmap.py 的默认值）。同一台机器上兼容模式跑 Controller + agent 时不
# 会互相抢端口。
AGENT_DEFAULT_PORT = 8090
AGENT_CONTAINER = os.environ.get("MIHOMO_TEST_AGENT_CONTAINER", "mihomo-agent")
AGENT_IMAGE = ipmap_mod.IMAGE
AGENT_CORE_SUBDIR = "core-agent"
AGENT_DEFAULT_API_PORT = 19192
AGENT_DEFAULT_MIXED_PORT = 19196
AGENT_DEFAULT_BASE_PORT = 19320
# 一个 Job 的全部节点序列化后通常几百 KB；8 MiB 留足余量，又仍然是个上限。
AGENT_MAX_BODY_BYTES = 8 * 1024 * 1024
# 默认延迟目标与 probe_policy.timeout_ms 默认值对齐；agent 不读业务配置，
# 这两个值由部署方用命令行/环境变量与 Controller 的 probe_policy 对齐。
AGENT_TEST_URL = "https://www.gstatic.com/generate_204"
AGENT_TIMEOUT_MS = 5000
AGENT_CONCURRENCY = 20


class _AgentBodyError(ValueError):
    """A rejected agent request body, with the status safe to return."""

    def __init__(self, status, message):
        super().__init__(message)
        self.status = status


class AgentOptions:
    """Everything the agent needs, none of it from the business config."""

    def __init__(self, probe_id, token, controller, host="127.0.0.1",
                 port=AGENT_DEFAULT_PORT, timeout_ms=AGENT_TIMEOUT_MS,
                 concurrency=AGENT_CONCURRENCY, test_url=AGENT_TEST_URL,
                 container=AGENT_CONTAINER,
                 api_port=AGENT_DEFAULT_API_PORT,
                 mixed_port=AGENT_DEFAULT_MIXED_PORT,
                 base_port=AGENT_DEFAULT_BASE_PORT):
        self.probe_id = probe_id
        self.token = token
        self.controller = controller
        self.host = host
        self.port = port
        self.timeout_ms = timeout_ms
        self.concurrency = concurrency
        self.test_url = test_url
        self.container = container
        self.api_port = api_port
        self.mixed_port = mixed_port
        self.base_port = base_port

    def core_cfg(self):
        return {
            "api": f"http://127.0.0.1:{self.api_port}",
            "mixed_port": self.mixed_port,
            "lanes": 1,
            "base_port": self.base_port,
            "container": self.container,
            "container_config_path": "/root/.config/mihomo/config.yaml",
        }


def _agent_log(level, message):
    """Agent ops output on stdout; never node data (指导 §4.1/§13.7)."""
    print(f"[agent:{level}] {message}", flush=True)


def _host_path(path):
    """Translate a container-visible path to the host path docker needs.

    Same problem core.config_test solves with HOST_ROOT: the bind-mount is
    resolved by the host daemon, which cannot see a path that only exists
    inside this container.
    """
    text = str(path)
    root, host_root = str(cfgmod.ROOT), str(cfgmod.HOST_ROOT)
    if host_root != root and text.startswith(root):
        return host_root + text[len(root):]
    return text


def _sweep_agent_core(root):
    """Remove leftover per-job kernel config dirs from a crashed run.

    A job dir holds the node credentials its kernel config was built from, so
    a crash mid-job must not leave one behind indefinitely (指导 §4.1: 完成后
    立即清理临时文件). Sweeping at startup bounds the exposure to one crash.
    """
    removed = 0
    for child in root.glob("job-*"):
        shutil.rmtree(child, ignore_errors=True)
        removed += 1
    if removed:
        _agent_log("warn", f"已清理上次残留的内核配置目录 {removed} 个")


def _start_kernel(core_cfg, secret, host_dir, log):
    """Run a one-shot mihomo kernel for this job (host network, v6 has routes)."""
    cli = coremod.docker_cli()
    if cli is None:
        raise RuntimeError("docker 不可用：探测点需要一台能跑 mihomo 内核的机器")
    subprocess.run([cli, "rm", "-f", core_cfg["container"]],
                   capture_output=True, timeout=60)
    cmd = [
        cli, "run", "-d",
        "--name", core_cfg["container"],
        "--network", "host",
        "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
        "--security-opt", "no-new-privileges:true",
        "--memory", "384m",
        "-v", f"{host_dir}:/root/.config/mihomo",
        AGENT_IMAGE,
        "-d", "/root/.config/mihomo",
        "-f", "/root/.config/mihomo/config.yaml",
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    if proc.returncode != 0:
        raise RuntimeError("探测点内核启动失败: "
                           + ((proc.stderr or proc.stdout) or "")[-300:])
    kernel = coremod.Core(core_cfg, secret)
    if not kernel.wait_ready(timeout_s=45):
        raise RuntimeError("探测点内核未就绪: " + kernel.logs(tail=30)[-400:])
    log("info", f"内核已启动（{core_cfg['container']}，API {core_cfg['api']}）")
    return kernel


def _stop_kernel(container, log):
    cli = coremod.docker_cli()
    if cli is None:
        return
    subprocess.run([cli, "rm", "-f", container], capture_output=True, timeout=60)
    log("info", f"内核容器已移除（{container}）")


def _execute_job(job, opts, log):
    """Measure one Job and post the ProbeResult back (指导 §4.1/§4.2).

    The kernel config is built from the Job's own node list with the names the
    Controller assigned (`mihomo_name`), written into a per-job directory and
    deleted as soon as the measurement ends -- node credentials never reach a
    long-lived file, the ledger, Gist or a log. `egress_country` stays null:
    exit verification is the Controller's job, done once per node across all
    probes, not once per probe.
    """
    round_id = job.get("round_id")
    probe_id = opts.probe_id
    nodes = [n for n in (job.get("nodes") or []) if isinstance(n, dict)]
    started_at = probe_dispatch.iso_now()
    cfgmod.DATA.mkdir(parents=True, exist_ok=True)
    secret = cfgmod.core_secret()
    core_root = cfgmod.ROOT / AGENT_CORE_SUBDIR
    core_root.mkdir(parents=True, exist_ok=True)
    core_dir = Path(tempfile.mkdtemp(prefix=f"job-{round_id}-", dir=core_root))
    core_cfg = opts.core_cfg()
    entries = []
    for index, node in enumerate(nodes):
        proxy = dict(node.get("proxy") or {})
        # The kernel config must use the name the Controller assigned: the Job
        # is the single source of naming for this round (指导 §4.1), and a
        # proxy that carried its own upstream name would measure one node
        # under a name nobody else in the round knows.
        name = str(node.get("mihomo_name") or f"__P_{index:06d}")
        proxy["name"] = name
        entries.append({
            "source": probe_id,
            "name": name,
            "proxy": proxy,
            "orig_proxy": dict(proxy),
            "index": index,
            "fp": node.get("fingerprint"),
        })
    results = []
    try:
        proxies, mapping, dropped = coremod.prepare(entries)
        coremod.build_config(proxies, core_cfg, secret, out_dir=core_dir)
        log("info", f"轮次 {round_id}：{len(proxies)} 节点进入内核配置"
                    f"（剔除 {len(dropped)}）")
        by_name = {entry["name"]: entry for entry in entries}
        for item in dropped:
            entry = by_name.get(item.get("name")) or {}
            results.append({
                "fingerprint": entry.get("fp") or item.get("name"),
                "alive": False,
                "delay_ms": None,
                "reason": f"dropped: {item.get('why')}",
                "egress_country": None,
            })
        if proxies:
            kernel = _start_kernel(core_cfg, secret, _host_path(core_dir), log)
            try:
                def measure(item):
                    return item, kernel.delay(item["mihomo"], opts.test_url,
                                              opts.timeout_ms)
                workers = max(1, int(opts.concurrency))
                if workers == 1:
                    pairs = [measure(item) for item in mapping]
                else:
                    with ThreadPoolExecutor(max_workers=workers) as pool:
                        pairs = list(pool.map(measure, mapping))
            finally:
                _stop_kernel(opts.container, log)
            for item, (delay_ms, reason, _detail) in pairs:
                results.append({
                    "fingerprint": item.get("fp") or item["mihomo"],
                    "alive": reason is None,
                    "delay_ms": delay_ms if reason is None else None,
                    "reason": reason,
                    "egress_country": None,
                })
    finally:
        shutil.rmtree(core_dir, ignore_errors=True)
    payload = {
        "round_id": round_id,
        "probe_id": probe_id,
        "config_digest": job.get("config_digest"),
        "started_at": started_at,
        "finished_at": probe_dispatch.iso_now(),
        "results": results,
    }
    _post_result(payload, opts, log)


def _post_result(payload, opts, log):
    """POST the ProbeResult to the Controller (指导 §4.2 回传路由)。"""
    url = (opts.controller.rstrip("/")
           + f"/api/v1/probes/{opts.probe_id}/jobs/{payload['round_id']}/result")
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    req = urllib.request.Request(url, data=body, method="POST", headers={
        "Content-Type": "application/json",
        "Authorization": f"Bearer {opts.token}",
    })
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            resp.read()
            code = getattr(resp, "status", 200)
        log("info", f"轮次 {payload['round_id']} 结果已回传"
                    f"（HTTP {code}，{len(payload['results'])} 节点）")
    except urllib.error.HTTPError as exc:
        detail = ""
        try:
            detail = exc.read().decode("utf-8", "replace")[:200]
        except Exception:
            pass
        # The Controller's rejection reasons (ROUND_STALE / DIGEST_MISMATCH /
        # RESULT_LATE) are the operator's next step, so they are logged whole.
        log("error", f"结果回传被拒绝（HTTP {exc.code}）: {detail}")
    except Exception as exc:
        log("error", f"结果回传失败（{type(exc).__name__}: {exc}）")


class _AgentHandler(BaseHTTPRequestHandler):
    """The probe agent's whole surface: POST /jobs and GET /healthz (指导 §13.2).

    Deliberately not the Controller's Handler: this one has no config, no
    ledger, no Sub-Store client and no admin token -- nothing an attacker
    could reach through it beyond "run the job you were handed".
    """
    server_version = "mihomo-probe-agent"
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        _agent_log("http", fmt % args)

    def _send_json(self, code, payload):
        raw = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Cache-Control", "no-store")
        if self.close_connection:
            self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(raw)

    def _body_length(self):
        if self.headers.get_all("Transfer-Encoding"):
            raise _AgentBodyError(400, "Transfer-Encoding is not supported")
        lengths = self.headers.get_all("Content-Length", [])
        if not lengths:
            return 0
        if len(lengths) != 1:
            raise _AgentBodyError(400, "multiple Content-Length headers")
        value = lengths[0].strip()
        if not value.isascii() or not value.isdigit():
            raise _AgentBodyError(400, "invalid Content-Length")
        if int(value) > AGENT_MAX_BODY_BYTES:
            raise _AgentBodyError(413, "request body too large")
        return int(value)

    def _read_body(self, length):
        raw = bytearray()
        while len(raw) < length:
            chunk = self.rfile.read1(min(65536, length - len(raw)))
            if not chunk:
                raise _AgentBodyError(400, "incomplete request body")
            raw.extend(chunk)
        return bytes(raw)

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path == "/healthz":
            return self._send_json(200, {"id": self.server.opts.probe_id,
                                         "ok": True})
        return self._send_json(404, {"error": "not found"})

    def do_POST(self):
        opts = self.server.opts
        if self.path.split("?", 1)[0] != "/jobs":
            return self._send_json(404, {"error": "not found"})
        # Token first, body second: an unauthenticated caller never gets its
        # bytes read at all.
        presented = _bearer(self.headers.get("Authorization") or "")
        if not presented or not _same(presented, opts.token):
            self.close_connection = True
            return self._send_json(401, {"error": "unauthorized"})
        try:
            length = self._body_length()
            raw = self._read_body(length)
        except _AgentBodyError as exc:
            self.close_connection = True
            return self._send_json(exc.status, {"error": str(exc)})
        ts = (self.headers.get(probe_dispatch.TIMESTAMP_HEADER) or "").strip()
        signature = (self.headers.get(probe_dispatch.SIGNATURE_HEADER)
                     or "").strip()
        if not ts.isdigit():
            return self._send_json(403, {"error": "SIGNATURE_STALE",
                                         "message": "缺少合法时间戳"})
        if abs(time.time() - int(ts)) > probe_dispatch.SIGNATURE_WINDOW_S:
            return self._send_json(403, {"error": "SIGNATURE_STALE",
                                         "message": "签名时间窗已过（5 分钟）"})
        if not probe_dispatch.verify_signature(opts.token, raw, ts, signature):
            return self._send_json(403, {"error": "SIGNATURE_INVALID"})
        try:
            job = json.loads(raw.decode("utf-8"))
        except ValueError:
            return self._send_json(400, {"error": "BAD_PAYLOAD",
                                         "message": "invalid JSON body"})
        if not isinstance(job, dict) or not job.get("round_id"):
            return self._send_json(400, {"error": "BAD_PAYLOAD",
                                         "message": "Job 缺少 round_id"})
        if not self.server.begin_job():
            # One kernel, one job at a time: a second concurrent Job would
            # restart the kernel under the first one's feet.
            return self._send_json(409, {"error": "BUSY",
                                         "message": "探测点正在执行上一轮任务"})
        threading.Thread(target=self.server.run_job, args=(job,),
                         daemon=True).start()
        return self._send_json(202, {"accepted": True})


def _bearer(header):
    return header[len("Bearer "):].strip() if header.startswith("Bearer ") else ""


def _same(left, right):
    return hmac.compare_digest(left.encode("utf-8"), right.encode("utf-8"))


class _AgentServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, handler, opts):
        super().__init__(address, handler)
        self.opts = opts
        self._busy = threading.Lock()

    def begin_job(self):
        return self._busy.acquire(blocking=False)

    def end_job(self):
        self._busy.release()

    def run_job(self, job):
        try:
            _execute_job(job, self.opts, _agent_log)
        except Exception as exc:  # a failed job must not kill the agent
            _agent_log("error", f"任务执行失败: {type(exc).__name__}: {exc}")
        finally:
            self.end_job()


def agent_serve(opts, host="127.0.0.1"):
    """Bind the agent's HTTP surface; port 0 picks a free one (tests)."""
    return _AgentServer((host, opts.port), _AgentHandler, opts)


def run_agent(args):
    """The `agent` subcommand: serve until signalled."""
    probe_id = str(args.id or "")
    if not cfgmod.PROBE_ID_RE.match(probe_id):
        print(f"agent 启动失败：--id {probe_id!r} 非法"
              "（需匹配 ^[a-z0-9][a-z0-9_-]{1,31}$，与 Controller 配置"
              " probes[].id 一致）", file=sys.stderr)
        return 2
    token = os.environ.get(str(args.token_env or ""), "")
    if not token:
        print(f"agent 启动失败：环境变量 {args.token_env} 未设置"
              "（探测点 token 只从环境变量读，不进命令行参数）", file=sys.stderr)
        return 2
    opts = AgentOptions(
        probe_id=probe_id, token=token, controller=args.controller,
        host=args.host, port=args.port, timeout_ms=args.timeout_ms,
        concurrency=args.concurrency, test_url=args.test_url,
        container=args.core_container, api_port=args.api_port,
        mixed_port=args.mixed_port, base_port=args.base_port)
    core_root = cfgmod.ROOT / AGENT_CORE_SUBDIR
    core_root.mkdir(parents=True, exist_ok=True)
    _sweep_agent_core(core_root)
    httpd = agent_serve(opts, args.host)
    stop = threading.Event()

    def shutdown(*_):
        stop.set()
        threading.Thread(target=httpd.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)
    _agent_log("info", f"探测点 {opts.probe_id} 监听 "
                       f"http://{args.host}:{httpd.server_address[1]}，"
                       f"Controller {opts.controller}")
    try:
        httpd.serve_forever()
    finally:
        stop.set()
        httpd.server_close()
    return 0


def _ipmap_log(level, message):
    """ipmap 的日志走 stdout,和面板的事件日志分家:这是一次性运维输出。"""
    print(f"[{level}] {message}")


def main(argv=None):
    parser = argparse.ArgumentParser(prog="mihomo_test", description=__doc__)
    parser.add_argument("command", nargs="?", default="serve",
                        choices=["serve", "round", "push", "status", "ipmap",
                                 "agent"])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=None,
                        help="serve 默认 8088；agent 默认 8090")
    parser.add_argument("--source", default=None,
                        help="round: only test this source key; "
                             "ipmap: Sub-Store 资源名(配合 --source-kind)")
    parser.add_argument("--mode", default=None, choices=["direct", "chain"],
                        help="manual split: direct skips chains, chain tests only "
                             "the chained entries; omitted runs everything")
    parser.add_argument("--trigger", default="cli")
    parser.add_argument("--no-schedule", action="store_true")

    # agent 专属参数（指导 §15 Phase 3 / plan 裁决 5）。探测点不读业务配置：
    # 测量参数在这里显式给出，由部署方与 Controller 的 probe_policy 对齐。
    parser.add_argument("--id", default=None,
                        help="agent: 探测点 id（需与 Controller probes[].id 一致）")
    parser.add_argument("--token-env", default=None,
                        help="agent: 存放探测点 token 的环境变量名"
                             "（对应 Controller probes[].token_env）")
    parser.add_argument("--controller", default="http://127.0.0.1:8088",
                        help="agent: Controller 基址")
    parser.add_argument("--timeout-ms", type=int, default=AGENT_TIMEOUT_MS,
                        help="agent: 单节点延迟测试超时（对齐 probe_policy.timeout_ms）")
    parser.add_argument("--concurrency", type=int, default=AGENT_CONCURRENCY,
                        help="agent: 并发测量数")
    parser.add_argument("--test-url", default=AGENT_TEST_URL,
                        help="agent: 延迟测试目标（HTTPS 优先）")
    parser.add_argument("--core-container", default=AGENT_CONTAINER,
                        help="agent: 一次性内核容器名")
    parser.add_argument("--api-port", type=int, default=AGENT_DEFAULT_API_PORT,
                        help="agent: 内核 external-controller 端口")
    parser.add_argument("--mixed-port", type=int,
                        default=AGENT_DEFAULT_MIXED_PORT,
                        help="agent: 内核 mixed-port")
    parser.add_argument("--base-port", type=int,
                        default=AGENT_DEFAULT_BASE_PORT,
                        help="agent: 内核车道端口起点")

    # ipmap 专属参数。节点来源三选一(--url / --file / --source),其余都有
    # 能用的默认值;见 mihomo_test/ipmap.py 的模块 docstring。
    parser.add_argument("--url", default=None, help="ipmap: 订阅地址(Clash YAML)")
    parser.add_argument("--file", default=None, help="ipmap: 本地 Clash YAML 路径")
    parser.add_argument("--source-kind", default="sub", choices=["sub", "collection"],
                        help="ipmap: --source 指向的资源类型")
    parser.add_argument("--family", default="all", choices=["all", "v4", "v6"],
                        help="ipmap: 只测写明为该族的入口(按 server 字面量)")
    parser.add_argument("--lanes", type=int, default=4, help="ipmap: 并发车道数")
    parser.add_argument("--limit", type=int, default=0, help="ipmap: 只测前 N 个(0=全部)")
    parser.add_argument("--timeout-s", type=int, default=12, help="ipmap: 单次回显超时")
    parser.add_argument("--key", default=None, help="ipmap: 输出文件名(data/ipmap/<key>.json|.md)")
    parser.add_argument("--out", default=None, help="ipmap: JSON 输出路径(覆盖 --key)")
    parser.add_argument("--push-sub", default=None,
                        help="ipmap: 把带实测标注的节点 upsert 进 Sub-Store 本地订阅")
    parser.add_argument("--keep-core", action="store_true",
                        help="ipmap: 探测后保留 mihomo-ipmap 容器(排查用)")
    args = parser.parse_args(argv)

    # agent 先于配置加载分支：探测点不读业务配置（指导 §2/§4.1），也就不该
    # 被 config.json 的 schema 校验拦住，更不该在启动时碰 data/ 里的账本。
    if args.command == "agent":
        if not args.id or not args.token_env:
            parser.error("agent 需要 --id 与 --token-env")
        if args.port is None:
            args.port = AGENT_DEFAULT_PORT
        return run_agent(args)

    if args.port is None:
        args.port = 8088

    cfg = cfgmod.load()

    if args.command == "ipmap":
        try:
            ipmap_mod.run(args, cfg, _ipmap_log)
        except ipmap_mod.ImapError as exc:
            print(f"ipmap 失败: {exc}", file=sys.stderr)
            return 2
        return 0

    if args.command == "round":
        try:
            summary = engine.run_round(cfg, trigger=args.trigger,
                                       only_source=args.source, mode=args.mode)
        except engine.Busy as exc:
            print(f"跳过: {exc}")
            return 2
        print(summary)
        return 0 if summary.get("alive") else 1

    if args.command == "push":
        from .store import Client

        store = Client(cfg["substore"]["backend"])
        # Same enabled-only rule as the panel's push button.
        records = engine.push_exports(cfg, store, engine.publish_keys(cfg))
        for line in engine.push_report(records):
            print(line)
        return 0

    if args.command == "status":
        print("stats:", db.stats())
        print("last round:", db.last_round())
        return 0

    # A round cannot outlive the process that was running it, so anything the
    # ledger still shows as open belongs to a previous life -- a container
    # restart during a round. Close it before the scheduler starts another one,
    # or the hole stays forever (the watchdog timer died with that process).
    engine.reap_orphan_rounds(cfg)

    httpd = server.serve(cfg, args.host, args.port)
    stop = threading.Event()

    def shutdown(*_):
        stop.set()
        threading.Thread(target=httpd.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)

    if not args.no_schedule:
        threading.Thread(target=server.scheduler_loop, args=(stop,), daemon=True,
                         name="scheduler").start()
    db.log("info", f"服务启动于 http://{args.host}:{args.port}")
    try:
        httpd.serve_forever()
    finally:
        stop.set()
        httpd.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
