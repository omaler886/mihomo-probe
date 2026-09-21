#!/usr/bin/env python3
"""CLI wrapper: point Sub-Store at the exporter for the selected sources.

The dashboard does the same thing via POST /api/link; this script exists for
use over SSH.

Usage:  python3 link_substore.py            # create/refresh
        python3 link_substore.py --remove   # undo (only our own objects)
"""
import argparse
import sys
import urllib.parse
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mihomo_test import config as cfgmod  # noqa: E402
from mihomo_test import engine  # noqa: E402
from mihomo_test.store import Client, StoreError  # noqa: E402


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--remove", action="store_true")
    args = parser.parse_args()

    cfg = cfgmod.load()
    client = Client(cfg["substore"]["backend"])
    prefix = cfg["publish"].get("prefix", "probe")

    if args.remove:
        keys = [s["key"] for s in cfg["sources"]]
        for key in keys:
            try:
                client._request("DELETE", "/api/sub/" + urllib.parse.quote(f"{prefix}-{key}", safe=""))
                print(f"deleted sub {prefix}-{key}")
            except StoreError as exc:
                print(f"sub {prefix}-{key}: {exc}")
        try:
            client._request("DELETE", "/api/collection/" + urllib.parse.quote(prefix, safe=""))
            print(f"deleted collection {prefix}")
        except StoreError as exc:
            print(f"collection {prefix}: {exc}")
        return 0

    for line in engine.link_substore(cfg, client, log=lambda _lvl, _msg: None):
        print(line)
    print()
    print(f"客户端引用: /download/collection/{prefix}?target=ClashMeta")
    return 0


if __name__ == "__main__":
    sys.exit(main())
