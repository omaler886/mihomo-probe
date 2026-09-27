"""Entry point: run the dashboard, or execute a single round from the CLI."""
import argparse
import signal
import sys
import threading

from . import config as cfgmod
from . import db
from . import engine
from . import ipmap as ipmap_mod
from . import server


def _ipmap_log(level, message):
    """ipmap 的日志走 stdout,和面板的事件日志分家:这是一次性运维输出。"""
    print(f"[{level}] {message}")


def main(argv=None):
    parser = argparse.ArgumentParser(prog="mihomo_test", description=__doc__)
    parser.add_argument("command", nargs="?", default="serve",
                        choices=["serve", "round", "push", "status", "ipmap"])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8088)
    parser.add_argument("--source", default=None,
                        help="round: only test this source key; "
                             "ipmap: Sub-Store 资源名(配合 --source-kind)")
    parser.add_argument("--mode", default=None, choices=["direct", "chain"],
                        help="manual split: direct skips chains, chain tests only "
                             "the chained entries; omitted runs everything")
    parser.add_argument("--trigger", default="cli")
    parser.add_argument("--no-schedule", action="store_true")

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
    parser.add_argument("--api-port", type=int, default=ipmap_mod.DEFAULT_API_PORT,
                        help="ipmap: 独立内核 API 端口")
    parser.add_argument("--base-port", type=int, default=ipmap_mod.DEFAULT_BASE_PORT,
                        help="ipmap: 车道端口起点")
    parser.add_argument("--mixed-port", type=int, default=ipmap_mod.DEFAULT_MIXED_PORT,
                        help="ipmap: 独立内核 mixed-port")
    parser.add_argument("--key", default=None, help="ipmap: 输出文件名(data/ipmap/<key>.json|.md)")
    parser.add_argument("--out", default=None, help="ipmap: JSON 输出路径(覆盖 --key)")
    parser.add_argument("--push-sub", default=None,
                        help="ipmap: 把带实测标注的节点 upsert 进 Sub-Store 本地订阅")
    parser.add_argument("--keep-core", action="store_true",
                        help="ipmap: 探测后保留 mihomo-ipmap 容器(排查用)")
    args = parser.parse_args(argv)

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