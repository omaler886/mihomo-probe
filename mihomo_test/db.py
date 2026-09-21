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
  duration_s REAL
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
  detail TEXT
);
CREATE INDEX IF NOT EXISTS idx_results_round ON results(round_id);
CREATE INDEX IF NOT EXISTS idx_results_node ON results(source, fingerprint);
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
            _conn = sqlite3.connect(DB_PATH, check_same_thread=False)
            _conn.row_factory = sqlite3.Row
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


def _migrate(conn):
    """Drop bookkeeping written before nodes were keyed by fingerprint."""
    tables = {row[0] for row in conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table'")}
    if "nodes" not in tables:
        return
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


def start_round(trigger):
    cur = execute("INSERT INTO rounds(started_at, trigger) VALUES(?,?)", (now(), trigger))
    return cur.lastrowid


def finish_round(round_id, **fields):
    cols = ", ".join(f"{k}=?" for k in fields)
    execute(f"UPDATE rounds SET finished_at=?, {cols} WHERE id=?",
            (now(), *fields.values(), round_id))


def record_result(round_id, source, fingerprint, display, verdict, delay_ms,
                  reason, country, attempts, detail):
    execute(
        "INSERT INTO results(round_id, source, fingerprint, display, verdict, delay_ms,"
        " reason, country, attempts, detail) VALUES(?,?,?,?,?,?,?,?,?,?)",
        (round_id, source, fingerprint, display, verdict, delay_ms, reason, country,
         attempts, detail),
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
    """Drop bookkeeping for nodes that vanished upstream."""
    if not keep_fingerprints:
        cur = execute("DELETE FROM nodes WHERE source=?", (source,))
        return cur.rowcount
    placeholders = ",".join("?" for _ in keep_fingerprints)
    cur = execute(
        f"DELETE FROM nodes WHERE source=? AND fingerprint NOT IN ({placeholders})",
        (source, *keep_fingerprints),
    )
    return cur.rowcount


def ip_geo_get(ips):
    """Return {ip: countryCode} for the cached subset of ips."""
    ips = [i for i in ips if i]
    if not ips:
        return {}
    placeholders = ",".join("?" for _ in ips)
    return {row["ip"]: row["country"]
            for row in query(f"SELECT ip, country FROM ip_geo WHERE ip IN ({placeholders})",
                             tuple(ips))}


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
    sql = ("SELECT source, fingerprint, display AS name, proto, server, country,"
           " status, consec_fail, last_delay_ms, last_reason, last_ok, total_ok,"
           " total_fail, first_seen, last_seen, ip_alive, ip_total FROM nodes")
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
