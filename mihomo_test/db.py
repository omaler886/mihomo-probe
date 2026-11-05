"""SQLite state for mihomo-test.

Two things need to survive between rounds: each node's consecutive-failure
counter (the convergence signal) and the per-round history the UI charts.

Nodes are keyed by a fingerprint of their connection parameters rather than
their display name. Upstream lists contain duplicate names, and a name-keyed
table silently merges two distinct nodes into one record -- which makes one
node's failures cancel out another's successes.
"""
import calendar
import json
import shutil
import sqlite3
import threading
import time

from . import config

DB_PATH = config.DATA / "state.db"
_lock = threading.RLock()
_conn = None

SCHEMA = """
CREATE TABLE IF NOT EXISTS nodes (
  source TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  display TEXT,
  proto TEXT,
  server TEXT,
  first_seen TEXT,
  last_seen TEXT,
  last_ok TEXT,
  last_delay_ms INTEGER,
  country TEXT,
  status TEXT NOT NULL DEFAULT 'unknown',
  consec_fail INTEGER NOT NULL DEFAULT 0,
  total_ok INTEGER NOT NULL DEFAULT 0,
  total_fail INTEGER NOT NULL DEFAULT 0,
  last_reason TEXT,
  ip_alive INTEGER,
  ip_total INTEGER,
  -- direct / relay / chain. Kept on the node row so the dashboard can group a
  -- source's own nodes by kind, and re-stamped every round so re-marking a
  -- source as a relay moves its nodes instead of leaving them in the old bucket.
  category TEXT,
  PRIMARY KEY (source, fingerprint)
);
CREATE TABLE IF NOT EXISTS rounds (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  trigger TEXT,
  total INTEGER DEFAULT 0,
  ok INTEGER DEFAULT 0,
  failed INTEGER DEFAULT 0,
  dropped INTEGER DEFAULT 0,
  restored INTEGER DEFAULT 0,
  suspect INTEGER DEFAULT 0,
  note TEXT,
  duration_s REAL,
  mode TEXT
);
CREATE TABLE IF NOT EXISTS results (
  round_id INTEGER NOT NULL,
  source TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  display TEXT,
  verdict TEXT,
  delay_ms INTEGER,
  reason TEXT,
  country TEXT,
  attempts INTEGER,
  detail TEXT,
  -- The per-round copy of the node's category. `nodes.category` only holds the
  -- latest value, so the round-over-round history needed to answer "did the
  -- chain pool get worse this week" has to live here, alongside the verdict it
  -- belongs to.
  category TEXT
);
CREATE INDEX IF NOT EXISTS idx_results_round ON results(round_id);
CREATE INDEX IF NOT EXISTS idx_results_node ON results(source, fingerprint);
-- `recent_trends` filters on source and orders by round_id. Without a
-- (source, round_id) index that is a full scan plus a sort of the whole table,
-- on every /api/nodes poll (the dashboard polls every 5 seconds) -- and the
-- table has no retention of its own.
CREATE INDEX IF NOT EXISTS idx_results_source_round ON results(source, round_id DESC);
CREATE INDEX IF NOT EXISTS idx_nodes_source ON nodes(source);
-- The per-category breakdown groups one round's rows by category, so the index
-- has to lead with round_id: `idx_results_source_round` cannot serve it.
CREATE INDEX IF NOT EXISTS idx_results_round_category ON results(round_id, category);
CREATE INDEX IF NOT EXISTS idx_rounds_finished ON rounds(finished_at, id DESC);
CREATE TABLE IF NOT EXISTS ip_geo (
  ip TEXT PRIMARY KEY,
  country TEXT,
  isp TEXT,
  checked_at TEXT
);
CREATE TABLE IF NOT EXISTS domain_views (
  domain TEXT PRIMARY KEY,
  views TEXT,
  checked_at TEXT
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  level TEXT NOT NULL,
  message TEXT NOT NULL
);
"""

# HK3 schema v3 追加表（指导 §12，SQL 逐字采用）。追加式：旧版可以忽略这三张
# 表，回滚不需要 down migration；既有 nodes/rounds/results 一律不重建、不清空
# （指导 §7.8：升级不得清空 nodes、consec_fail、最后成功时间或历史轮次）。
V3_SCHEMA = """
CREATE TABLE IF NOT EXISTS probes (
  probe_id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  last_seen_at TEXT,
  version TEXT,
  state TEXT NOT NULL DEFAULT 'unknown'
);
CREATE TABLE IF NOT EXISTS probe_observations (
  round_id TEXT NOT NULL,
  probe_id TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  alive INTEGER,
  delay_ms INTEGER,
  reason TEXT,
  egress_country TEXT,
  received_at TEXT NOT NULL,
  payload_digest TEXT NOT NULL,
  PRIMARY KEY (round_id, probe_id, fingerprint)
);
CREATE TABLE IF NOT EXISTS publish_audit (
  audit_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  target TEXT NOT NULL,
  revision TEXT,
  digest TEXT NOT NULL,
  actor TEXT NOT NULL,
  status TEXT NOT NULL,
  summary_json TEXT,
  created_at TEXT NOT NULL
);
"""


def now():
    """Timestamp for every stored column: UTC, always.

    Deliberately UTC rather than container-local. The app container runs with
    TZ=Asia/Shanghai while the host is UTC, so a localtime stamp means two
    processes in the same system can write values 8 hours apart into one
    column -- which is exactly how rounds 55/56 on vps ended up with a
    finished_at eight hours *before* their started_at. Storing UTC removes
    the class of bug rather than the instance.

    Format is unchanged (second precision, no suffix) so existing rows and
    the UI keep parsing it; only the clock behind it is now unambiguous.
    """
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())


def to_epoch(stamp):
    """Parse a stored timestamp as UTC; return 0 when it is unusable."""
    try:
        return calendar.timegm(time.strptime(stamp, "%Y-%m-%dT%H:%M:%S"))
    except (TypeError, ValueError):
        return 0


def connect():
    global _conn
    with _lock:
        if _conn is None:
            DB_PATH.parent.mkdir(parents=True, exist_ok=True)
            # `timeout` and WAL are not tuning, they are correctness. The app
            # and the CLI are separate processes and `run_round` says so
            # explicitly: while a CLI round writes results, the service is still
            # writing events through its own connection. With the default
            # rollback journal and a 5s busy timeout that is
            # "OperationalError: database is locked" -- and `db.log` is called
            # from failure paths, so losing that write loses the only record of
            # the original problem. WAL lets readers and one writer coexist.
            _conn = sqlite3.connect(DB_PATH, check_same_thread=False, timeout=30)
            _conn.row_factory = sqlite3.Row
            try:
                _conn.execute("PRAGMA journal_mode=WAL")
                _conn.execute("PRAGMA busy_timeout=30000")
                _conn.execute("PRAGMA synchronous=NORMAL")
            except sqlite3.Error:
                # An older build or a filesystem that refuses WAL still works,
                # just with the old contention behaviour.
                pass
            # Migrate before creating indexes: an index on a column added by
            # migration cannot be built against the older table.
            _migrate(_conn)
            _conn.executescript(SCHEMA)
            _conn.commit()
        return _conn


def _backup(path):
    """Copy the ledger aside before a destructive migration.

    The pre-fingerprint migration drops tables. If anything after the DROP
    fails, that convergence history is unrecoverable -- and it is the only
    place the failure streaks live, so losing it silently resets every node
    to 'unknown'. Best effort: a backup we cannot take must not block startup.
    """
    try:
        stamp = time.strftime("%Y%m%d-%H%M%S")
        shutil.copy2(path, path.with_name(f"{path.name}.bak-{stamp}"))
    except OSError:
        pass


def _add_v3_tables(conn, tables):
    """Create the HK3 v3 tables idempotently; back the ledger up first (指导 §12).

    The snapshot is taken once, the first time this build adds the v3 tables
    to a file that already exists on disk. The rule is deliberately
    file-level rather than schema-level: a brand-new database is one page (the
    WAL header write alone makes the file non-empty), so "has tables" would
    skip the very upgrade case this exists for, while a 4KB copy of an empty
    ledger costs nothing and keeps the rule auditable in one line. An existing
    `.bak-v3-*` means an earlier run already kept the pre-v3 snapshot. Best
    effort -- a backup we cannot take must not block startup, same rule as
    `_backup`.
    """
    if all(name in tables for name in
           ("probes", "probe_observations", "publish_audit")):
        return
    try:
        if (DB_PATH.exists() and DB_PATH.stat().st_size > 0
                and not list(DB_PATH.parent.glob(DB_PATH.name + ".bak-v3-*"))):
            # Fold the WAL back into the main file first, so the copy is a
            # complete ledger rather than a main file missing recent commits.
            try:
                conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            except sqlite3.Error:
                pass
            stamp = time.strftime("%Y%m%dT%H%M%S", time.gmtime())
            shutil.copy2(DB_PATH,
                         DB_PATH.with_name(f"{DB_PATH.name}.bak-v3-{stamp}"))
    except OSError:
        pass
    conn.executescript(V3_SCHEMA)
    conn.commit()


def _migrate(conn):
    """In-place column additions, then the one drop-everything migration."""
    tables = {row[0] for row in conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table'")}
    # HK3 v3 追加表在任何 early return 之前建：新库同样需要这三张表，且
    # CREATE TABLE IF NOT EXISTS + 上面的存在性检查保证幂等。
    _add_v3_tables(conn, tables)
    if "nodes" not in tables:
        return
    _add_round_mode(conn, tables)
    _add_category_columns(conn, tables)
    columns = {row[1] for row in conn.execute("PRAGMA table_info(nodes)")}
    if "fingerprint" in columns:
        # in-place addition for nodes written before per-IP verdicts existed
        if "ip_alive" not in columns:
            conn.execute("ALTER TABLE nodes ADD COLUMN ip_alive INTEGER")
            conn.execute("ALTER TABLE nodes ADD COLUMN ip_total INTEGER")
            conn.commit()
        return
    _backup(DB_PATH)
    # One transaction for both DROPs: a half-applied migration (nodes gone,
    # results left) would break the rebuild of the indexes this schema needs.
    with conn:
        for table in ("results", "nodes"):
            if table in tables:
                conn.execute(f"DROP TABLE {table}")


def _add_category_columns(conn, tables):
    """Add `category` to `nodes` and `results` on a pre-category database.

    Existing rows are back-filled to `direct` rather than left NULL. A NULL
    would not be a harmless "unknown": the per-category breakdown groups on this
    column, so a NULL bucket shows up on the dashboard as a fourth category with
    no name next to it. `direct` is the honest default -- before this feature
    existed, a node with a `dialer-proxy` was tested direct, which is exactly
    what the column now records.

    Called before the fingerprint branch and independent of it, for the same
    reason `_add_round_mode` is: the rounds table and both of these columns have
    to exist whether or not the drop-everything rebuild runs.
    """
    for table in ("nodes", "results"):
        if table not in tables:
            continue
        cols = {row[1] for row in conn.execute(f"PRAGMA table_info({table})")}
        if "category" not in cols:
            conn.execute(f"ALTER TABLE {table} ADD COLUMN category TEXT")
            conn.execute(f"UPDATE {table} SET category='direct' WHERE category IS NULL")
            conn.commit()
    if "results" in tables:
        conn.execute("CREATE INDEX IF NOT EXISTS idx_results_round_category "
                     "ON results(round_id, category)")
        conn.commit()


def _add_round_mode(conn, tables):
    """Add `rounds.mode` to a database written before manual round modes existed.

    Purely additive, and deliberately independent of the fingerprint check
    below: the rounds table predates nodes-as-fingerprints and is never
    dropped, so the column has to be added on both branches. `ALTER TABLE ADD
    COLUMN` cannot fail on a database that already has it -- it raises
    OperationalError -- so the PRAGMA check is what makes this idempotent.
    """
    if "rounds" not in tables:
        return
    cols = {row[1] for row in conn.execute("PRAGMA table_info(rounds)")}
    if "mode" not in cols:
        conn.execute("ALTER TABLE rounds ADD COLUMN mode TEXT")
        conn.commit()


def execute(sql, params=()):
    with _lock:
        conn = connect()
        cur = conn.execute(sql, params)
        conn.commit()
        return cur


def query(sql, params=()):
    with _lock:
        return [dict(row) for row in connect().execute(sql, params).fetchall()]


def one(sql, params=()):
    rows = query(sql, params)
    return rows[0] if rows else None


def log(level, message):
    execute("INSERT INTO events(ts, level, message) VALUES(?,?,?)", (now(), level, message))
    execute(
        "DELETE FROM events WHERE id NOT IN "
        "(SELECT id FROM events ORDER BY id DESC LIMIT 2000)"
    )


def start_round(trigger, mode=None):
    """Open a round row. `mode` mirrors what the engine ran: None (scheduler),
    "direct" (直连测活) or "chain" (链式测活)."""
    cur = execute("INSERT INTO rounds(started_at, trigger, mode) VALUES(?,?,?)",
                  (now(), trigger, mode))
    return cur.lastrowid


def finish_round(round_id, **fields):
    cols = ", ".join(f"{k}=?" for k in fields)
    execute(f"UPDATE rounds SET finished_at=?, {cols} WHERE id=?",
            (now(), *fields.values(), round_id))


def record_result(round_id, source, fingerprint, display, verdict, delay_ms,
                  reason, country, attempts, detail, category=None):
    """Append one node's verdict for this round.

    `category` is optional so the pre-category call shapes keep working; a NULL
    here is grouped under `direct` by `stats_by_category` rather than surfacing
    as an unnamed fourth bucket.
    """
    execute(
        "INSERT INTO results(round_id, source, fingerprint, display, verdict, delay_ms,"
        " reason, country, attempts, detail, category) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
        (round_id, source, fingerprint, display, verdict, delay_ms, reason, country,
         attempts, detail, category),
    )


def get_node(source, fingerprint):
    return one("SELECT * FROM nodes WHERE source=? AND fingerprint=?", (source, fingerprint))


def upsert_node(source, fingerprint, display=None, **fields):
    stamp = now()
    if not get_node(source, fingerprint):
        execute(
            "INSERT INTO nodes(source, fingerprint, display, first_seen, last_seen)"
            " VALUES(?,?,?,?,?)",
            (source, fingerprint, display, stamp, stamp),
        )
    if display:
        fields = dict(fields, display=display)
    if not fields:
        execute("UPDATE nodes SET last_seen=? WHERE source=? AND fingerprint=?",
                (stamp, source, fingerprint))
        return
    cols = ", ".join(f"{k}=?" for k in fields)
    execute(
        f"UPDATE nodes SET last_seen=?, {cols} WHERE source=? AND fingerprint=?",
        (stamp, *fields.values(), source, fingerprint),
    )


def delete_nodes_not_in(source, keep_fingerprints):
    """Drop bookkeeping for nodes that vanished upstream.

    Deleting the complement cannot be chunked, so the keep-list is inverted in
    Python and the rows are removed in chunks -- same reason as `ip_geo_get`:
    this list is every node the round tested, which is well past 999.
    """
    if not keep_fingerprints:
        cur = execute("DELETE FROM nodes WHERE source=?", (source,))
        return cur.rowcount
    keep = set(keep_fingerprints)
    victims = [row["fingerprint"] for row in
               query("SELECT fingerprint FROM nodes WHERE source=?", (source,))
               if row["fingerprint"] not in keep]
    removed = 0
    for chunk in _chunked(victims):
        placeholders = ",".join("?" for _ in chunk)
        removed += execute(
            f"DELETE FROM nodes WHERE source=? AND fingerprint IN ({placeholders})",
            (source, *chunk),
        ).rowcount
    return removed


# SQLite's default SQLITE_MAX_VARIABLE_NUMBER is 999 (32766 on 3.32+), and a
# round hands `ip_geo_get` every candidate address of every source. At 300
# nodes resolving to 2-4 addresses each that is 600-1200 placeholders, so on an
# older build the query raised "too many SQL variables" -- inside
# `classify_and_expand`, i.e. the whole round failed, for a reason that looks
# nothing like its cause.
SQL_VAR_CHUNK = 400


def _chunked(items, size=SQL_VAR_CHUNK):
    for start in range(0, len(items), size):
        yield items[start:start + size]


def ip_geo_get(ips):
    """Return {ip: countryCode} for the cached subset of ips."""
    ips = [i for i in dict.fromkeys(i for i in ips if i)]
    if not ips:
        return {}
    out = {}
    for chunk in _chunked(ips):
        placeholders = ",".join("?" for _ in chunk)
        out.update({
            row["ip"]: row["country"]
            for row in query(f"SELECT ip, country FROM ip_geo WHERE ip IN ({placeholders})",
                             tuple(chunk))
        })
    return out


def ip_geo_put(rows):
    """Cache batch country lookups; checked_at only notes when it was seen."""
    for row in rows:
        execute(
            "INSERT INTO ip_geo(ip, country, isp, checked_at) VALUES(?,?,?,?) "
            "ON CONFLICT(ip) DO UPDATE SET country=excluded.country, "
            "isp=excluded.isp, checked_at=excluded.checked_at",
            (row["ip"], row.get("country"), row.get("isp"), now()),
        )


def domain_views_get(domain, max_age_s):
    """Return the cached {view: [ip]} for a domain if it is still fresh."""
    row = one("SELECT views, checked_at FROM domain_views WHERE domain=?", (domain,))
    if not row:
        return None
    # Parsed as UTC to match db.now(), which wrote it. mktime would shift the
    # age by the TZ offset and could keep an expired entry alive (or drop a
    # fresh one) depending on which side of UTC the container sits.
    epoch = to_epoch(row["checked_at"])
    if not epoch:
        return None
    age = time.time() - epoch
    # A stamp in the future is not "infinitely fresh", it is unusable: the row
    # was written by a build that stored local time, or the clock stepped back.
    # `age > max_age_s` alone never fires for a negative age, so such an entry
    # was pinned forever -- and because the DNS views decide whether a node's
    # entry is on a restricted ISP, a one-off resolution could exclude a node
    # permanently. Re-resolve instead of trusting it.
    if age < 0 or age > max_age_s:
        return None
    try:
        return json.loads(row["views"])
    except ValueError:
        return None


def domain_views_put(domain, views):
    execute("INSERT INTO domain_views(domain, views, checked_at) VALUES(?,?,?) "
            "ON CONFLICT(domain) DO UPDATE SET views=excluded.views, "
            "checked_at=excluded.checked_at",
            (domain, json.dumps(views, ensure_ascii=False), now()))


def delete_sources_not_in(keys):
    """Drop bookkeeping for sources the user deselected.

    Without this, a deselected source's nodes linger in the dashboard as
    permanently 'unknown' and inflate the totals.
    """
    keys = [k for k in keys if k]
    if not keys:
        return 0
    placeholders = ",".join("?" for _ in keys)
    cur = execute(f"DELETE FROM nodes WHERE source NOT IN ({placeholders})", tuple(keys))
    return cur.rowcount


def demote_disabled_sources(enabled_keys):
    """Mark rows of configured-but-disabled sources as unknown; return the count.

    A disabled source deliberately keeps its history so re-enabling is cheap
    (see `engine.cleanup_exports`), and nothing prunes it. But its last verdict
    is no longer maintained, and leaving those rows as `alive` puts nodes into
    the headline count that nothing has tested for hours: on vps, 25 of the 315
    `alive` were a disabled source's snapshot frozen 21 hours earlier. That is a
    claim the system cannot back up.

    Demoting to `unknown` removes the claim while keeping `consec_fail`,
    `total_ok` and `total_fail` intact, so a re-enabled source still restarts
    cheaply. `excluded` is left alone -- it is a classification, not a verdict.
    """
    keys = [k for k in enabled_keys if k]
    if not keys:
        return 0
    placeholders = ",".join("?" for _ in keys)
    cur = execute(
        f"UPDATE nodes SET status='unknown' WHERE source NOT IN ({placeholders})"
        f" AND status NOT IN ('unknown', 'excluded')",
        tuple(keys),
    )
    return cur.rowcount


def list_nodes(source=None, status=None):
    # `category` has to be selected explicitly: every consumer reads it as
    # `n.category || "direct"`, so omitting it here does not blank the column --
    # it silently relabels every 中转 and 链式 node as 直连 in the table, the
    # filter and the badge, while `/api/stats` (which queries the column
    # directly) keeps reporting the correct split.
    sql = ("SELECT source, fingerprint, display AS name, proto, server, country,"
           " status, consec_fail, last_delay_ms, last_reason, last_ok, total_ok,"
           " total_fail, first_seen, last_seen, ip_alive, ip_total, category FROM nodes")
    where, params = [], []
    if source:
        where.append("source=?")
        params.append(source)
    if status:
        where.append("status=?")
        params.append(status)
    if where:
        sql += " WHERE " + " AND ".join(where)
    sql += " ORDER BY source, (status='dead'), (status='unknown'), last_delay_ms IS NULL, last_delay_ms"
    return query(sql, params)


def recent_trends(source, limit_per_node=12):
    """Return {fingerprint: [verdict, ...]} newest-first for the UI sparkline."""
    rows = query(
        "SELECT fingerprint, verdict FROM results WHERE source=? "
        "ORDER BY round_id DESC LIMIT 8000",
        (source,),
    )
    trends = {}
    for row in rows:
        bucket = trends.setdefault(row["fingerprint"], [])
        if len(bucket) < limit_per_node:
            bucket.append(row["verdict"])
    return trends


def recent_trends_all(limit_per_node=12, per_source_rows=8000):
    """Return {source: {fingerprint: [verdict, ...]}} in one pass.

    `/api/nodes` used to call `recent_trends` once per distinct source and then
    loop over every node for each of those calls -- O(sources x nodes) Python
    work, on a payload the dashboard re-fetches every 5 seconds. The per-source
    row cap is preserved by taking the newest `per_source_rows` rounds' worth
    for each source with a window function instead of a global LIMIT.
    """
    rows = query(
        "SELECT source, fingerprint, verdict FROM ("
        "  SELECT source, fingerprint, verdict,"
        "         ROW_NUMBER() OVER (PARTITION BY source ORDER BY round_id DESC) AS rn"
        "  FROM results"
        ") WHERE rn <= ?",
        (per_source_rows,),
    )
    out = {}
    for row in rows:
        bucket = out.setdefault(row["source"], {}).setdefault(row["fingerprint"], [])
        if len(bucket) < limit_per_node:
            bucket.append(row["verdict"])
    return out


# How many completed rounds of per-node history to keep. `results` has one row
# per node per round, so at 2000 nodes and a 30-minute interval it grows by
# roughly 3M rows a month with nothing pruning it -- in a container capped at
# 256MB. `events` was already trimmed; this is the table that actually grows.
KEEP_RESULT_ROUNDS = 500


def trim_results(cfg=None, keep_rounds=KEEP_RESULT_ROUNDS):
    """Delete `results` rows older than the newest `keep_rounds` rounds.

    Returns the number of rows removed. Rounds themselves and the `nodes`
    ledger are never touched: those are small and carry the convergence state.
    """
    row = one("SELECT MIN(id) AS cutoff FROM ("
              "  SELECT id FROM rounds ORDER BY id DESC LIMIT ?)", (int(keep_rounds),))
    cutoff = (row or {}).get("cutoff")
    if not cutoff:
        return 0
    cur = execute("DELETE FROM results WHERE round_id < ?", (cutoff,))
    return cur.rowcount


def open_rounds():
    """Rounds that were started and never closed, oldest first."""
    return query("SELECT id, started_at, trigger FROM rounds "
                 "WHERE finished_at IS NULL ORDER BY id")


def last_rounds(limit=20):
    return query("SELECT * FROM rounds ORDER BY id DESC LIMIT ?", (limit,))


def last_round():
    return one("SELECT * FROM rounds ORDER BY id DESC LIMIT 1")


def recent_events(limit=200):
    return query("SELECT * FROM events ORDER BY id DESC LIMIT ?", (limit,))


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


# The two units a per-category total can be counted in, and they are not the
# same number. Measured live: 348 nodes in the ledger against 504 dialled rows
# in one round, because a node resolving to two addresses is one ledger row and
# two test rows, and a chained node is one ledger row and one row per front.
#
# Reporting only one of them is what makes the dashboard look broken: the node
# table (ledger) and the round summary (rows) would show different totals for
# the same category, and neither would be wrong. Both are returned, labelled.
CATEGORY_BUCKETS = ("direct", "relay", "chain")


def stats_by_category(round_id=None):
    """Per-category counts in both units, for the dashboard.

    `nodes` answers "how many 直连节点 do I have"; `tested` answers "how much
    work did 直连 cost this round". A node whose `category` is NULL -- written
    before this column existed, or by a caller that did not pass one -- is
    grouped under `direct`, which is what those nodes were in fact tested as.

    `round_id` defaults to the most recent round that has results, not to
    `last_round()`: a round in flight has a `rounds` row but no `results` yet,
    and defaulting to it would blank the panel for the minute a round runs.

    Delay statistics come only from `alive` rows, since a failed verdict stores
    the delay of the attempt that failed, which is a timeout constant rather
    than a measurement.
    """
    if round_id is None:
        row = one("SELECT MAX(round_id) AS rid FROM results")
        round_id = (row or {}).get("rid")

    out = {
        cat: {
            "category": cat,
            "nodes": {"total": 0, "alive": 0, "dead": 0, "pending": 0,
                      "unknown": 0, "excluded": 0},
            # Every key the populated path can set has to exist here too. The
            # dashboard reads `tested.skipped` unconditionally when it builds a
            # category card, and a missing key renders as "undefined" -- or
            # throws, if it lands in arithmetic. An empty category is the
            # *normal* state for relay/chain until a relay source is configured,
            # so this is the shape the panel sees first.
            "tested": {"total": 0, "ok": 0, "fail": 0, "skipped": 0},
            "delay_ms": {"median": None, "avg": None, "min": None, "max": None},
            "reasons": [],
            "top_countries": [],
        }
        for cat in CATEGORY_BUCKETS
    }

    for row in query(
        "SELECT COALESCE(category, 'direct') AS cat,"
        " COUNT(*) AS total,"
        " SUM(status='alive') AS alive,"
        " SUM(status='dead') AS dead,"
        " SUM(status='pending') AS pending,"
        " SUM(status='unknown') AS unknown,"
        " SUM(status='excluded') AS excluded"
        " FROM nodes GROUP BY cat"
    ):
        bucket = out.get(row["cat"])
        if bucket is None:
            continue
        bucket["nodes"] = {k: (row[k] or 0) for k in
                           ("total", "alive", "dead", "pending", "unknown", "excluded")}

    if round_id is None:
        return {"round_id": None, "categories": out}
    results = query(
        "SELECT COALESCE(category, 'direct') AS cat,"
        " COUNT(*) AS total,"
        " SUM(verdict='ok') AS ok,"
        " SUM(verdict='fail') AS fail,"
        " SUM(verdict='excluded') AS skipped,"
        " AVG(CASE WHEN verdict='ok' AND delay_ms IS NOT NULL THEN delay_ms END) AS avg_ms,"
        " MIN(CASE WHEN verdict='ok' AND delay_ms IS NOT NULL THEN delay_ms END) AS min_ms,"
        " MAX(CASE WHEN verdict='ok' AND delay_ms IS NOT NULL THEN delay_ms END) AS max_ms"
        " FROM results WHERE round_id=? GROUP BY cat",
        (round_id,),
    )
    for row in results:
        bucket = out.get(row["cat"])
        if bucket is None:
            continue
        bucket["tested"] = {
            "total": row["total"] or 0,
            "ok": row["ok"] or 0,
            "fail": row["fail"] or 0,
            # `excluded` is neither a pass nor a failure: the node was never
            # dialled. Counted separately so ok+fail+skipped adds up to total.
            "skipped": row["skipped"] or 0,
        }
        if row["avg_ms"] is not None:
            bucket["delay_ms"] = {
                "avg": round(float(row["avg_ms"]), 1),
                "min": row["min_ms"],
                "max": row["max_ms"],
                "median": _median_delay(round_id, row["cat"]),
            }

    for row in query(
        "SELECT COALESCE(category, 'direct') AS cat, COALESCE(reason, 'ok') AS reason,"
        " COUNT(*) AS n FROM results WHERE round_id=?"
        " AND verdict<>'ok' GROUP BY cat, reason ORDER BY n DESC",
        (round_id,),
    ):
        bucket = out.get(row["cat"])
        if bucket is not None and len(bucket["reasons"]) < 6:
            bucket["reasons"].append({"reason": row["reason"], "count": row["n"]})

    for row in query(
        "SELECT COALESCE(category, 'direct') AS cat, COALESCE(country, '—') AS country,"
        " COUNT(*) AS n FROM results WHERE round_id=? AND verdict='ok'"
        " GROUP BY cat, country ORDER BY n DESC",
        (round_id,),
    ):
        bucket = out.get(row["cat"])
        if bucket is not None and len(bucket["top_countries"]) < 6:
            bucket["top_countries"].append({"country": row["country"], "count": row["n"]})

    return {"round_id": round_id, "categories": out}


def _median_delay(round_id, category):
    """Median of one category's successful delays this round.

    Computed in SQL with an offset rather than by pulling every delay into
    Python: a category can hold several hundred rows and this runs on the
    dashboard's poll.
    """
    row = one(
        "SELECT COUNT(*) AS n FROM results WHERE round_id=?"
        " AND COALESCE(category,'direct')=? AND verdict='ok' AND delay_ms IS NOT NULL",
        (round_id, category),
    )
    count = (row or {}).get("n") or 0
    if not count:
        return None
    row = one(
        "SELECT delay_ms FROM results WHERE round_id=?"
        " AND COALESCE(category,'direct')=? AND verdict='ok' AND delay_ms IS NOT NULL"
        " ORDER BY delay_ms LIMIT 1 OFFSET ?",
        (round_id, category, count // 2),
    )
    return (row or {}).get("delay_ms")


# --------------------------------------------------------------- HK3 v3 表面
# probes / probe_observations / publish_audit 的最小访问函数（指导 §12）。
# nodes 仍是聚合后的唯一状态账本，这些表只做多探测点观察与发布审计，不参与
# 死亡计数推进。


def upsert_probe(probe_id, name, enabled=True, last_seen_at=None,
                 version=None, state=None):
    """Register or refresh a probe row.

    `version` / `state` / `last_seen_at` default to None = "leave unchanged"
    on an existing row, so re-syncing the config cannot reset a probe's
    online state or blank its reported version. A brand-new row is stamped
    with now() and 'unknown' unless told otherwise. UPDATE-first rather than
    INSERT .. ON CONFLICT: the "only touch what was passed" rule is the whole
    point, and a COALESCE over the excluded row cannot express it.
    """
    sets, params = ["name=?", "enabled=?"], [name, 1 if enabled else 0]
    if last_seen_at is not None:
        sets.append("last_seen_at=?")
        params.append(last_seen_at)
    if version is not None:
        sets.append("version=?")
        params.append(version)
    if state is not None:
        sets.append("state=?")
        params.append(state)
    params.append(probe_id)
    if execute(f"UPDATE probes SET {', '.join(sets)} WHERE probe_id=?",
               tuple(params)).rowcount:
        return
    execute(
        "INSERT INTO probes(probe_id, name, enabled, last_seen_at, version, state)"
        " VALUES(?,?,?,?,?,COALESCE(?, 'unknown'))",
        (probe_id, name, 1 if enabled else 0, last_seen_at or now(),
         version, state),
    )


def touch_probe_seen(probe_id, version=None, state=None):
    """Heartbeat: stamp last_seen_at, optionally recording version/state.

    Returns the number of rows updated: 0 means the probe is not registered,
    which callers treat as "unknown probe" -- audit only, never a node
    failure (指导 §4.2).
    """
    sets, params = ["last_seen_at=?"], [now()]
    if version is not None:
        sets.append("version=?")
        params.append(version)
    if state is not None:
        sets.append("state=?")
        params.append(state)
    params.append(probe_id)
    return execute(
        f"UPDATE probes SET {', '.join(sets)} WHERE probe_id=?", tuple(params),
    ).rowcount


def record_observation(round_id, probe_id, fingerprint, alive, delay_ms=None,
                       reason=None, egress_country=None, payload_digest=""):
    """Idempotently store one probe's verdict for one node this round.

    PK (round_id, probe_id, fingerprint) plus INSERT OR REPLACE: a re-reported
    result overwrites instead of duplicating (指导 §2：重复上报必须幂等覆盖
    或拒绝)。
    """
    execute(
        "INSERT OR REPLACE INTO probe_observations"
        "(round_id, probe_id, fingerprint, alive, delay_ms, reason,"
        " egress_country, received_at, payload_digest)"
        " VALUES(?,?,?,?,?,?,?,?,?)",
        (round_id, probe_id, fingerprint,
         None if alive is None else (1 if alive else 0),
         delay_ms, reason, egress_country, now(), payload_digest or ""),
    )


def get_observations(round_id):
    """Every probe's observation for one round, probe then fingerprint order."""
    return query(
        "SELECT round_id, probe_id, fingerprint, alive, delay_ms, reason,"
        " egress_country, received_at, payload_digest FROM probe_observations"
        " WHERE round_id=? ORDER BY probe_id, fingerprint",
        (round_id,),
    )


def record_publish_audit(audit_id, kind, target, revision, digest, actor,
                         status, summary_json=None):
    """Append one publish audit row (指导 §12 publish_audit 表).

    `summary_json` accepts a dict/list (serialized here) or a ready string.
    """
    if summary_json is not None and not isinstance(summary_json, str):
        summary_json = json.dumps(summary_json, ensure_ascii=False)
    execute(
        "INSERT OR REPLACE INTO publish_audit"
        "(audit_id, kind, target, revision, digest, actor, status,"
        " summary_json, created_at) VALUES(?,?,?,?,?,?,?,?,?)",
        (audit_id, kind, target, revision, digest, actor, status,
         summary_json, now()),
    )


def list_publish_audit(limit=50):
    """Newest-first publish audit rows for the 发布中心."""
    return query(
        "SELECT audit_id, kind, target, revision, digest, actor, status,"
        " summary_json, created_at FROM publish_audit"
        " ORDER BY created_at DESC, audit_id DESC LIMIT ?",
        (int(limit),),
    )
