"""Read-only triage snapshot for the node-liveness loop.

One cheap command that answers "where does the liveness ledger stand right now"
without composing SQL by hand -- meant to be the first call of every loop
iteration, both locally and against the live deployment:

    # live (vps, inside the app container)
    ssh -o BatchMode=yes vps 'docker exec -i mihomo-test python3 -' < tools/loop_snapshot.py

    # locally against a copy of the project's data/
    MIHOMO_TEST_ROOT=/path/to/project python3 tools/loop_snapshot.py

Prints nothing that mutates state, and never touches the network. Sections:

    stats            node totals by status
    rounds           last N rounds with ok/failed/dropped/suspect and duration
    dead             dead nodes grouped by last_reason, then the streaks
    pending          undecided nodes (the false-kill suspects)
    excluded         nodes skipped as untestable from here
    restored flip    nodes that came back alive in the recent window
    events           recent non-progress log lines (warnings/errors first)
"""
import argparse
import os
import sys
from collections import Counter

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import db  # noqa: E402


def _h(title):
    print(f"\n=== {title} ===")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--rounds", type=int, default=6)
    ap.add_argument("--events", type=int, default=15)
    ap.add_argument("--top", type=int, default=12, help="rows per detail section")
    args = ap.parse_args(argv)

    _h("root")
    print(os.environ.get("MIHOMO_TEST_ROOT", "<unset>"))

    _h("stats")
    print(db.stats())

    _h(f"rounds (last {args.rounds})")
    rounds = db.last_rounds(args.rounds)
    for r in rounds:
        flag = "SUSPECT" if r.get("suspect") else "ok"
        dur = r.get("duration_s")
        print(f"#{r['id']:<4} {r['started_at']}  {str(r.get('trigger')):<9} "
              f"total={r.get('total')} ok={r.get('ok')} failed={r.get('failed')} "
              f"dropped={r.get('dropped')} restored={r.get('restored')} "
              f"dur={dur}s {flag} {r.get('note') or ''}")

    nodes = db.query("SELECT * FROM nodes")

    _h("dead by last_reason")
    dead = [n for n in nodes if n["status"] == "dead"]
    for reason, count in Counter(n["last_reason"] for n in dead).most_common():
        print(f"{count:>4}  {reason}")
    print(f"---- worst streaks (>= {3} = judged dead)")
    for n in sorted(dead, key=lambda n: -(n["consec_fail"] or 0))[:args.top]:
        print(f"  {n['consec_fail']:>3} fails  {n['source']:<10} {n['display'][:44]:<44} "
              f"{n['last_reason']}")

    _h("pending (undecided -- false-kill suspects)")
    pending = [n for n in nodes if n["status"] == "pending"]
    for n in sorted(pending, key=lambda n: -(n["consec_fail"] or 0))[:args.top]:
        print(f"  {n['consec_fail']:>3} fails  {n['source']:<10} {n['display'][:44]:<44} "
              f"{n['last_reason']}")

    _h("excluded (untestable from this vantage point)")
    for n in nodes:
        if n["status"] == "excluded":
            print(f"  {n['source']:<10} {n['display'][:44]:<44} {n['last_reason']}")

    _h("came back alive in the recent window (flip = false-kill evidence)")
    print("  restored per round:", [r.get("restored") for r in rounds])

    _h(f"events (last {args.events}, newest first)")
    noisy = ("出口验证", "延迟测试", "拉取")
    shown = 0
    for e in db.recent_events(400):
        msg = e["message"]
        if any(msg.startswith(p) for p in noisy):
            continue
        print(f"  {e['ts']} {e['level']:<5} {msg[:130]}")
        shown += 1
        if shown >= args.events:
            break
    return 0


if __name__ == "__main__":
    sys.exit(main())