-- 0002: Rust-side ledger extensions (additive only).
--
-- New tables the Rust pipeline writes as batches land; nothing Python writes
-- is altered. Per ADR-0003 (workstreams/08) the `sources` table and an
-- `observations` rename are deferred to the R10 shadow decision -- during the
-- compat phase config.json stays the source registry and `results` stays the
-- shared per-round table, so the Python reader keeps working untouched.

-- One row per node state transition, appended by the Rust convergence pass
-- (R7). The nodes row keeps the latest state; this is the history the panel's
-- "why did this die" question needs.
CREATE TABLE IF NOT EXISTS node_state_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  source TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  from_status TEXT,
  to_status TEXT NOT NULL,
  reason TEXT,
  round_id INTEGER
);
CREATE INDEX IF NOT EXISTS idx_node_state_history_node
  ON node_state_history(source, fingerprint, id DESC);

-- One row per published export write (R8): what changed, and enough identity
-- to materialise the previous version again. Content itself lives in
-- export_snapshots, capped per key.
CREATE TABLE IF NOT EXISTS export_snapshots (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  key TEXT NOT NULL,
  created_at TEXT NOT NULL,
  node_count INTEGER NOT NULL,
  sha256 TEXT NOT NULL,
  content TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_export_snapshots_key
  ON export_snapshots(key, id DESC);

-- Every privileged config write (POST /api/config and its Rust equivalent):
-- who, what paths moved, which guards fired. No values -- masked summaries
-- only, mirroring the /api/status redaction rules.
CREATE TABLE IF NOT EXISTS config_audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  actor TEXT NOT NULL DEFAULT 'panel',
  changed_paths TEXT NOT NULL,
  notes TEXT
);

-- Security events: token generation, secret-scan hits, guardrail trips,
-- export rollbacks. Append-only.
CREATE TABLE IF NOT EXISTS security_audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  kind TEXT NOT NULL,
  detail TEXT
);
