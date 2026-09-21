"""Read-only ad-hoc queries against the node-liveness ledger.

Companion to ``tools/loop_snapshot.py``. The snapshot answers "where do we
stand"; this answers "show me the evidence for node X / round Y / reason Z".
It exists so a read-only investigator can pull raw rows with one command
instead of composing a throwaway script each time.

Live (read-only, nothing is written):

    ssh -o BatchMode=yes vps 'docker exec -i mihomo-test python3 -' \\
        < tools/loop_query.py history myFreeNodeChat 40

Local (against a copy of the project's data/):

    MIHOMO_TEST_ROOT=/path/to/project python3 tools/loop_query.py stats

Commands
--------
    stats                       ledger totals + round/result ranges
    rounds [N]                  last N rounds (default 8)
    node <substr>               current node rows matching display/fingerprint
    history <substr> [N]        per-round verdict/reason/detail + signature count
    round <id>                  every result row of one round
    reasons [N]                 verdict x reason counts over the last N rounds
    reason-by-round [N]         reason mix per round (is a failure round-wide?)
    events [N] [level]          recent events, optionally filtered by level
    dupes                       servers shared by several fingerprints
    rows <table> [filter...]    filtered rows; filters are col=val / col!=val /
                                col~substr / col:ge:val / col:le:val /
                                col:gt:val / col:lt:val
                                (structured so no shell quoting is needed)
    group <table> <col> [filter...]   COUNT(*) grouped by a column
    sql "<SELECT ...>"          one read-only statement (guarded)
    sql64 <base64>              the same, base64-encoded -- use this for any
                                statement containing spaces, `(`, `*` or a
                                quote, because `sql` has to survive two shells

**Never write ``>`` or ``<`` in a filter.** The filter travels as a bare argv
word through ssh and then through a second shell, and both read ``>`` as a
redirection: ``rows nodes consec_fail>=40`` silently creates a file named
``=40`` and swallows the output. Use ``consec_fail:ge:40`` instead.

Examples (no quoting hazards, unlike --sql):

    loop_query.py rows nodes source=618
    loop_query.py rows nodes status=dead --order consec_fail
    loop_query.py rows nodes consec_fail:ge:40
    loop_query.py group nodes status
    loop_query.py group results reason round_id:ge:61
    loop_query.py group nodes server source=618

Columns available (see mihomo_test/db.py SCHEMA):

  nodes    source fingerprint display proto server first_seen last_seen last_ok
           last_delay_ms country status consec_fail total_ok total_fail
           last_reason ip_alive ip_total
  rounds   id started_at finished_at trigger total ok failed dropped restored
           suspect note duration_s
  results  round_id source fingerprint display verdict delay_ms reason country
           attempts detail
  events   id ts level message
  ip_geo   ip country isp checked_at
"""
import os
import sqlite3
import sys
from collections import Counter, defaultdict

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from mihomo_test import config  # noqa: E402

# Why not ``from mihomo_test import db``?  db.connect() runs
# ``executescript(SCHEMA)`` -- every CREATE TABLE IF NOT EXISTS is still a
# write statement, so merely *opening* the ledger through db.py takes a write
# lock.  While the scheduler is mid-round (it writes results continuously) that
# turns a diagnostic read into a lock fight: queries hang for minutes and then
# return nothing.  Opening the file read-only in a URI connection never
# requests a write lock, so an investigator can read the ledger while a round
# is in flight -- which is exactly when the evidence matters most.
_RO = None


def _conn_ro():
    global _RO
    if _RO is None:
        path = (config.DATA / "state.db").as_posix()
        _RO = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=120)
        _RO.row_factory = sqlite3.Row
    return _RO


def query(sql, params=()):
    return [dict(row) for row in _conn_ro().execute(sql, params).fetchall()]


def one(sql, params=()):
    rows = query(sql, params)
    return rows[0] if rows else None


def stats():
    row = one(
        "SELECT COUNT(*) AS total,"
        " SUM(status='alive') AS alive,"
        " SUM(status='dead') AS dead,"
        " SUM(status='pending') AS pending,"
        " SUM(status='unknown') AS unknown,"
        " SUM(status='excluded') AS excluded"
        " FROM nodes"
    )
    return {k: (v or 0) for k, v in (row or {}).items()}


def last_rounds(limit=20):
    return query("SELECT * FROM rounds ORDER BY id DESC LIMIT ?", (limit,))

BANNED = {"insert", "update", "delete", "drop", "alter", "attach", "pragma",
          "vacuum", "replace", "create", "reindex", "trigger"}


def _h(title):
    print(f"\n=== {title} ===")


def _trunc(value, width):
    text = "" if value is None else str(value)
    return text.replace("\n", " | ")[:width]


def _words(text):
    out, cur = [], []
    for ch in text:
        if ch.isalnum() or ch == "_":
            cur.append(ch)
        else:
            if cur:
                out.append("".join(cur))
                cur = []
    if cur:
        out.append("".join(cur))
    return out


def guard(sql):
    """Accept exactly one read-only statement, or raise ValueError."""
    body = sql.strip().rstrip(";").strip()
    if not body:
        raise ValueError("empty SQL")
    if ";" in body:
        raise ValueError("only a single statement is allowed")
    lowered = body.lower()
    if not (lowered.startswith("select") or lowered.startswith("with")):
        raise ValueError("only SELECT / WITH statements are allowed")
    for token in _words(lowered):
        if token in BANNED:
            raise ValueError(f"forbidden keyword: {token}")
    return body


def _dump(rows, limit):
    if not rows:
        print("(no rows)")
        return
    cols = list(rows[0].keys())
    print("\t".join(cols))
    for row in rows[:limit]:
        print("\t".join(_trunc(row[c], 90) for c in cols))
    if len(rows) > limit:
        print(f"... {len(rows) - limit} more rows (raise --limit)")


def cmd_stats(args):
    print(stats())
    counts = one("SELECT (SELECT COUNT(*) FROM rounds) AS rounds,"
                    " (SELECT COUNT(*) FROM results) AS results,"
                    " (SELECT COUNT(*) FROM events) AS events,"
                    " (SELECT COUNT(*) FROM ip_geo) AS ip_geo")
    print(counts)
    print("results round range:",
          one("SELECT MIN(round_id) AS lo, MAX(round_id) AS hi FROM results"))


def cmd_rounds(args):
    # accept a bare count too, the way `events` does -- `rounds 4` used to be
    # silently ignored and always print 8
    n = args.limit or next((int(t) for t in args.args if t.isdigit()), 8)
    for r in last_rounds(n):
        print(f"#{r['id']:<4} {r['started_at']} {str(r.get('trigger')):<9} "
              f"total={r.get('total')} ok={r.get('ok')} failed={r.get('failed')} "
              f"dropped={r.get('dropped')} restored={r.get('restored')} "
              f"suspect={r.get('suspect')} dur={r.get('duration_s')}")


def cmd_node(args):
    needle = args.limit and args.args[0]
    rows = query(
        "SELECT source, substr(fingerprint,1,10) AS fp, display, status, consec_fail,"
        " total_ok, total_fail, last_reason, country, last_delay_ms, last_ok,"
        " ip_alive, ip_total FROM nodes WHERE display LIKE ? OR fingerprint LIKE ?"
        " ORDER BY source, display",
        (f"%{needle}%", f"%{needle}%"),
    )
    _dump(rows, args.limit or 60)


def cmd_history(args):
    """Per-round evidence for matching nodes.

    This is what a batch-death review needs: whether the failure signature is
    byte-identical every round (deterministic death) or drifts (a kill rule is
    misfiring, or the target side is flaky).
    """
    needle = args.args[0]
    span = int(args.args[1]) if len(args.args) > 1 else 20
    nodes = query(
        "SELECT source, fingerprint, display, COUNT(*) AS n FROM results"
        " WHERE display LIKE ? GROUP BY source, fingerprint, display"
        " ORDER BY n DESC LIMIT ?",
        (f"%{needle}%", args.limit or 60),
    )
    if not nodes:
        print("(no node matches)")
        return
    hi = one("SELECT MAX(round_id) AS hi FROM results")["hi"]
    lo = max(0, hi - span)
    for n in nodes:
        print(f"\n--- {n['source']}  {n['display']}  (rows total={n['n']})")
        rows = query(
            "SELECT round_id, verdict, reason, delay_ms, attempts, country, detail"
            " FROM results WHERE source=? AND fingerprint=? AND round_id>=?"
            " ORDER BY round_id",
            (n["source"], n["fingerprint"], lo),
        )
        for r in rows:
            print(f"  #{r['round_id']:<4} {str(r['verdict']):<10} "
                  f"{str(r['reason']):<16} delay={str(r['delay_ms']):<7} "
                  f"att={r['attempts']} cc={r['country']} {_trunc(r['detail'], 60)}")
        sigs = Counter(f"{r['verdict']}|{r['reason']}|{_trunc(r['detail'], 40)}"
                       for r in rows)
        print(f"  rounds shown={len(rows)}  distinct signatures={len(sigs)}")
        for sig, count in sigs.most_common(5):
            print(f"    x{count:<3} {sig}")


def cmd_round(args):
    round_id = int(args.args[0])
    rows = query(
        "SELECT source, display, verdict, reason, delay_ms, attempts, country, detail"
        " FROM results WHERE round_id=? ORDER BY verdict, source, display",
        (round_id,),
    )
    print(f"round {round_id}: {len(rows)} result rows")
    _dump(rows, args.limit or 60)


def cmd_reasons(args):
    span = args.limit or 20
    hi = one("SELECT MAX(round_id) AS hi FROM results")["hi"]
    lo = max(0, hi - span)
    rows = query(
        "SELECT verdict, reason, COUNT(*) AS n FROM results WHERE round_id>=?"
        " GROUP BY verdict, reason ORDER BY n DESC",
        (lo,),
    )
    print(f"rounds {lo}..{hi}")
    for r in rows:
        print(f"  {r['n']:>6}  {str(r['verdict']):<10} {r['reason']}")


def cmd_reason_by_round(args):
    span = args.limit or 12
    hi = one("SELECT MAX(round_id) AS hi FROM results")["hi"]
    lo = max(0, hi - span)
    rows = query(
        "SELECT round_id, reason, COUNT(*) AS n FROM results WHERE round_id>=?"
        " GROUP BY round_id, reason ORDER BY round_id, n DESC",
        (lo,),
    )
    buckets = defaultdict(list)
    for r in rows:
        buckets[r["round_id"]].append(f"{r['reason']}x{r['n']}")
    for round_id in sorted(buckets):
        print(f"  #{round_id:<4} " + "  ".join(buckets[round_id][:8]))


def cmd_events(args):
    """Recent events. First positional arg may be a count or a level name.

    `events 12` used to be read as a *level* named "12", so it silently matched
    nothing -- the docstring promised a count and the code wanted a level.
    Accepting either spelling removes the trap.
    """
    n = args.limit or 20
    level = None
    for token in args.args:
        if token.isdigit():
            n = int(token)
        else:
            level = token
    sql = "SELECT ts, level, message FROM events"
    params = ()
    if level:
        sql += " WHERE level=?"
        params = (level,)
    sql += " ORDER BY id DESC LIMIT ?"
    for e in query(sql, params + (n,)):
        print(f"  {e['ts']} {e['level']:<5} {_trunc(e['message'], 150)}")


def cmd_dupes(args):
    """A *server* shared by two fingerprints is suspicious; two display names
    sharing one fingerprint is expected (db.py folds them by design)."""
    rows = query(
        "SELECT server, COUNT(DISTINCT fingerprint) AS fps,"
        " GROUP_CONCAT(DISTINCT source) AS sources,"
        " GROUP_CONCAT(DISTINCT display) AS names"
        " FROM nodes WHERE server IS NOT NULL AND server<>''"
        " GROUP BY server HAVING fps>1 ORDER BY fps DESC LIMIT ?",
        (args.limit or 40,),
    )
    _dump(rows, args.limit or 40)


def cmd_sql(args):
    _dump(query(guard(args.args[0])), args.limit or 60)


def cmd_sql64(args):
    """Like ``sql``, but the statement arrives base64-encoded.

    ``sql "SELECT ..."`` is a trap for anything non-trivial: the statement is a
    bare argv word that ssh concatenates into a remote command line, so the
    remote shell splits it on spaces and chokes on ``(`` -- `COUNT(*)` alone is
    enough to break it. Base64 has no shell metacharacters at all, so it
    survives both shells intact.
    """
    import base64

    try:
        statement = base64.b64decode(args.args[0]).decode("utf-8")
    except Exception as exc:  # noqa: BLE001 - any decode failure is the same to us
        raise ValueError(f"bad base64: {exc}") from None
    print(f"decoded: {statement}")
    _dump(query(guard(statement)), args.limit or 60)


# --- structured filters -------------------------------------------------
# ``--sql`` is the escape hatch, but passing SQL through ssh means fighting two
# layers of shell quoting (and a bare ``*`` would glob). These two commands take
# the filter as separate argv words and rebuild a parameterised statement, so an
# investigator never has to quote anything.

TABLES = {
    "nodes": ["source", "fingerprint", "display", "proto", "server", "first_seen",
              "last_seen", "last_ok", "last_delay_ms", "country", "status",
              "consec_fail", "total_ok", "total_fail", "last_reason", "ip_alive",
              "ip_total"],
    "rounds": ["id", "started_at", "finished_at", "trigger", "total", "ok",
               "failed", "dropped", "restored", "suspect", "note", "duration_s"],
    "results": ["round_id", "source", "fingerprint", "display", "verdict",
                "delay_ms", "reason", "country", "attempts", "detail"],
    "events": ["id", "ts", "level", "message"],
    "ip_geo": ["ip", "country", "isp", "checked_at"],
}

# longest first so ">=" is matched before ">"
OPS = [(">=", ">="), ("<=", "<="), ("!=", "<>"), ("~", "LIKE"),
       ("=", "="), (">", ">"), ("<", "<")]

# Shell-safe operator spellings.
#
# ``col>=40`` looks fine and is a trap: the filter is passed as a bare argv
# word, and BOTH the local shell and the remote shell the ssh command lands in
# treat ``>`` as a redirection. The result is that a file named ``=40`` is
# created in the current directory, the output is silently swallowed into it,
# and the query never runs. That is exactly how a "the ledger query returned
# nothing" mystery starts. ``:op:val`` uses no shell metacharacter at all, so
# it survives both shells; prefer it for anything other than equality.
WORD_OPS = {"eq": "=", "ne": "<>", "like": "LIKE",
            "ge": ">=", "le": "<=", "gt": ">", "lt": "<"}


def _coerce(value):
    try:
        return int(value)
    except ValueError:
        pass
    try:
        return float(value)
    except ValueError:
        return value


def _split_filter(token):
    """Return (column, sql_operator, value) for one filter word."""
    if token.count(":") >= 2:
        col, _, rest = token.partition(":")
        op_name, _, val = rest.partition(":")
        if op_name in WORD_OPS:
            return col, WORD_OPS[op_name], val
    for symbol, sqlop in OPS:
        head, found, tail = token.partition(symbol)
        if found:
            return head, sqlop, tail
    return None, None, None


def parse_filters(table, tokens):
    cols = TABLES[table]
    where, params = [], []
    for token in tokens:
        col, op, val = _split_filter(token)
        if col is None:
            raise ValueError(
                f"bad filter {token!r}; use col=val / col~substr / "
                f"col:ge:val / col:le:val / col:gt:val / col:lt:val "
                f"(avoid > and < -- the shell eats them)")
        if col not in cols:
            raise ValueError(f"unknown column {col!r} for table {table}; "
                             f"known: {', '.join(cols)}")
        if op == "LIKE":
            val = f"%{val}%"
        where.append(f"{col} {op} ?")
        params.append(_coerce(val))
    return where, params


def cmd_rows(args):
    table = args.args[0]
    if table not in TABLES:
        raise ValueError(f"unknown table {table!r}; known: {', '.join(TABLES)}")
    where, params = parse_filters(table, args.args[1:])
    sql = f"SELECT * FROM {table}"
    if where:
        sql += " WHERE " + " AND ".join(where)
    if args.order:
        if args.order not in TABLES[table]:
            raise ValueError(f"cannot order by {args.order!r}")
        sql += f" ORDER BY {args.order}"
    sql += " LIMIT ?"
    print(f"{table} WHERE {' AND '.join(where) if where else '(all)'}"
          f"  params={params}")
    _dump(query(sql, tuple(params) + (args.limit or 60,)), args.limit or 60)


def cmd_group(args):
    table = args.args[0]
    if table not in TABLES:
        raise ValueError(f"unknown table {table!r}; known: {', '.join(TABLES)}")
    if len(args.args) < 2:
        raise ValueError("group needs a column: group <table> <col> [filter...]")
    col = args.args[1]
    if col not in TABLES[table]:
        raise ValueError(f"unknown column {col!r} for table {table}")
    where, params = parse_filters(table, args.args[2:])
    sql = f"SELECT {col}, COUNT(*) AS n FROM {table}"
    if where:
        sql += " WHERE " + " AND ".join(where)
    sql += f" GROUP BY {col} ORDER BY n DESC LIMIT ?"
    print(f"{table} GROUP BY {col} WHERE {' AND '.join(where) if where else '(all)'}"
          f"  params={params}")
    _dump(query(sql, tuple(params) + (args.limit or 60,)), args.limit or 60)


COMMANDS = {
    "stats": cmd_stats,
    "rounds": cmd_rounds,
    "node": cmd_node,
    "history": cmd_history,
    "round": cmd_round,
    "reasons": cmd_reasons,
    "reason-by-round": cmd_reason_by_round,
    "events": cmd_events,
    "dupes": cmd_dupes,
    "rows": cmd_rows,
    "group": cmd_group,
    "sql": cmd_sql,
    "sql64": cmd_sql64,
}

USAGE = "usage: loop_query.py <command> [args...] [--limit N]\ncommands: " + \
        ", ".join(sorted(COMMANDS))


class Args:
    def __init__(self, command, args, limit, order=None):
        self.command = command
        self.args = args
        self.limit = limit
        self.order = order


def main(argv=None):
    raw = list(sys.argv[1:] if argv is None else argv)
    limit = None
    order = None
    rest = []
    i = 0
    while i < len(raw):
        if raw[i] == "--limit" and i + 1 < len(raw):
            limit = int(raw[i + 1])
            i += 2
            continue
        if raw[i].startswith("--limit="):
            limit = int(raw[i].split("=", 1)[1])
            i += 1
            continue
        if raw[i] == "--order" and i + 1 < len(raw):
            order = raw[i + 1]
            i += 2
            continue
        if raw[i].startswith("--order="):
            order = raw[i].split("=", 1)[1]
            i += 1
            continue
        rest.append(raw[i])
        i += 1

    if not rest or rest[0] in ("-h", "--help", "help"):
        print(__doc__)
        print(USAGE)
        return 2
    command = rest[0].lstrip("-")
    if command not in COMMANDS:
        print(f"unknown command: {rest[0]}\n{USAGE}")
        return 2

    _h("root")
    print(os.environ.get("MIHOMO_TEST_ROOT", "<unset>"))
    try:
        COMMANDS[command](Args(command, rest[1:], limit, order))
    except (ValueError, IndexError) as exc:
        print(f"ERROR: {exc}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
