"""Back up the ledger and repair the two rows corrupted by the TZ bug.

Rounds 55/56 were closed by `_abandon_round` using a stale state file, which
stamped them with a UTC-derived finish time while started_at was container-local
(CST). The two clocks are 8h apart, so finish < start.

Repair: set finished_at to started_at + 0 (they were aborted before any work
completed, and duration_s is already 0), preserving the note. This keeps the
rows honest -- the round started and was abandoned -- instead of inventing a
duration we never measured.

Run with --apply to write; default is a dry run.
"""
import shutil
import sqlite3
import sys
import time

DB = "/srv/mihomo-test/data/state.db"
APPLY = "--apply" in sys.argv


def main():
    conn = sqlite3.connect(DB)
    conn.row_factory = sqlite3.Row
    bad = list(conn.execute(
        "SELECT id, started_at, finished_at, note FROM rounds "
        "WHERE finished_at IS NOT NULL AND finished_at < started_at"))
    if not bad:
        print("no inverted rows; nothing to repair")
        return 0

    print(f"found {len(bad)} inverted row(s):")
    for r in bad:
        print(f"  id={r['id']} start={r['started_at']} finish={r['finished_at']} "
              f"note={r['note']!r}")

    if not APPLY:
        print("\ndry run. re-run with --apply to write the repair.")
        return 0

    stamp = time.strftime("%Y%m%d-%H%M%S")
    backup = f"{DB}.pre-tzfix-{stamp}"
    shutil.copy2(DB, backup)
    print(f"\nbacked up to {backup}")

    with conn:
        for r in bad:
            conn.execute("UPDATE rounds SET finished_at=? WHERE id=?",
                         (r["started_at"], r["id"]))
            print(f"  repaired id={r['id']} -> finished_at={r['started_at']}")
    conn.close()

    conn = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    left = conn.execute("SELECT COUNT(*) FROM rounds WHERE finished_at IS NOT NULL "
                        "AND finished_at < started_at").fetchone()[0]
    conn.close()
    print(f"\nremaining inverted rows: {left}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
