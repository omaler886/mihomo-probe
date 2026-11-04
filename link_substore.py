#!/usr/bin/env python3
"""Migration-era helper, DEMOTED (workstreams/18 B5, F-058): plan-only.

This script USED to create/refresh the `probe-<key>` Sub-Store objects (and
`--remove` DELETEd them). Both semantics are gone: the migration lives in the
Rust surface now -- a read-only `scan`, an update-only `apply` (dry run
unless `--execute`) and a `rollback`, none of which can ever create or
delete a Sub-Store object. An unattended agent produces a plan and an audit
trail, never a deletion.

This helper only prints the exact command to run. It reads no config, holds
no token, and performs no network request.

Usage:
    python3 link_substore.py --scan     # print the migration scan command
"""
import argparse
import os
import sys


def deployment_root():
    """The root `probe-cli` itself resolves to (same env, same default).

    A string, not a Path: the default is the POSIX deployment path and must
    print verbatim on any host.
    """
    root = os.environ.get("MIHOMO_TEST_ROOT", "").strip()
    return root if root else "/srv/mihomo-test"


def main():
    # Piped output is machine consumption: pin UTF-8 so the printed command
    # and notices survive any host codepage. A real terminal keeps the
    # locale encoding.
    if not sys.stdout.isatty():
        sys.stdout.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(
        description="print the Sub-Store duplicate-migration command "
                    "(this script no longer writes to Sub-Store)")
    parser.add_argument(
        "--scan", action="store_true",
        help="print the exact `probe-cli migrate scan` command")
    args = parser.parse_args()

    if not args.scan:
        print(__doc__.strip())
        print()
        print("唯一入口: python3 link_substore.py --scan")
        return 0

    root = deployment_root()
    plan = f"{root}/migrations/plan.json"
    print("此脚本已降级：不再创建/刷新，也不再删除任何 Sub-Store 对象。")
    print("用 Rust 迁移面执行只读扫描（绝不写入、绝不删除；本命令也未发起任何请求）：")
    print()
    print(f"  probe-cli --root {root} migrate scan --report {plan}")
    print()
    print("审阅 plan.json（URL 已脱敏）后再决定是否执行 apply——默认 dry run：")
    print(f"  probe-cli --root {root} migrate apply --plan {plan}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
