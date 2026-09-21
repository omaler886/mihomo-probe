"""Entry point: run the dashboard, or execute a single round from the CLI."""
import argparse
import signal
import sys
import threading

from . import config as cfgmod
from . import db
from . import engine
from . import server


def main(argv=None):
    parser = argparse.ArgumentParser(prog="mihomo_test", description=__doc__)
    parser.add_argument("command", nargs="?", default="serve",
                        choices=["serve", "round", "push", "status"])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8088)
    parser.add_argument("--source", default=None, help="only test this source key")
    parser.add_argument("--trigger", default="cli")
    parser.add_argument("--no-schedule", action="store_true")
    args = parser.parse_args(argv)

    cfg = cfgmod.load()

    if args.command == "round":
        try:
            summary = engine.run_round(cfg, trigger=args.trigger, only_source=args.source)
        except engine.Busy as exc:
            print(f"跳过: {exc}")
            return 2
        print(summary)
        return 0 if summary.get("alive") else 1

    if args.command == "push":
        from .store import Client

        store = Client(cfg["substore"]["backend"])
        keys = [s["key"] for s in cfg["sources"]]
        for line in engine.push_exports(cfg, store, keys):
            print(line)
        return 0

    if args.command == "status":
        print("stats:", db.stats())
        print("last round:", db.last_round())
        return 0

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