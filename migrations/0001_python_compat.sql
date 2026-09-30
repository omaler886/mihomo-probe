-- 0001: Python-compat base schema.
--
-- Byte-compatible with mihomo_test/db.py's SCHEMA block: a ledger written by
-- the Python service must open here unchanged, and a ledger created here must
-- be openable by the Python service. Table and index definitions are copied
-- from db.py (IF NOT EXISTS everywhere, so a database Python already made
-- simply skips them). Renames or replacements (observations vs results,
-- sources table) are deliberately deferred to the shadow-comparison decision
-- (ADR-0003 in workstreams/08) -- the Python reader must keep working while
-- both implementations share one file.

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
  category TEXT
);
CREATE INDEX IF NOT EXISTS idx_results_round ON results(round_id);
CREATE INDEX IF NOT EXISTS idx_results_node ON results(source, fingerprint);
CREATE INDEX IF NOT EXISTS idx_results_source_round ON results(source, round_id DESC);
CREATE INDEX IF NOT EXISTS idx_nodes_source ON nodes(source);
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
