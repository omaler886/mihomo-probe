-- 0008: give `results` the `category` column 0001 already indexes.
--
-- 0001 creates `results` WITHOUT `category` and two statements later builds
-- `idx_results_round_category ON results(round_id, category)`. A fresh database
-- therefore dies at "no such column: category" -- what a brand-new deployment
-- and the in-memory migration test hit. Every deployed ledger so far escaped it
-- because the Python service (db.py) created the table with that column.
--
-- 0001 is append-only: migrations.rs is explicit that an applied migration is
-- never edited and corrections arrive as new files. So the repair is here.
--
-- A plain `ALTER TABLE ADD COLUMN` would fail on a Python-built ledger where the
-- column already exists, and SQLite cannot branch DDL. The table is rebuilt
-- from an explicit definition carrying `category`, with every old row copied
-- across unchanged.
--
-- The definition must include EVERY column the migration set has added by the
-- time this runs -- 0005's endpoint observations and 0007's exit identity.
-- Rebuilding from 0001's eleven columns alone silently dropped eight columns
-- that insert_results writes, and the whole round's ledger commit failed with
-- "table results has no column named endpoint_ip". Keep this list in sync with
-- 0001 + 0005 + 0007; migrations 0009+ that add columns to `results` still
-- apply normally after this one.
CREATE TABLE IF NOT EXISTS results_0008 (
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
  category TEXT,
  endpoint_ip TEXT,
  original_server TEXT,
  protocol TEXT,
  dns_incomplete INTEGER NOT NULL DEFAULT 0,
  exit_ip TEXT,
  exit_asn TEXT,
  verification_error TEXT,
  verification_bytes INTEGER NOT NULL DEFAULT 0
);
INSERT INTO results_0008 (
  round_id, source, fingerprint, display, verdict, delay_ms, reason, country,
  attempts, detail, category, endpoint_ip, original_server, protocol,
  dns_incomplete, exit_ip, exit_asn, verification_error, verification_bytes)
  SELECT round_id, source, fingerprint, display, verdict, delay_ms, reason, country,
         attempts, detail, category, endpoint_ip, original_server, protocol,
         dns_incomplete, exit_ip, exit_asn, verification_error, verification_bytes
    FROM results;
DROP TABLE IF EXISTS results;
ALTER TABLE results_0008 RENAME TO results;
CREATE INDEX IF NOT EXISTS idx_results_round ON results(round_id);
CREATE INDEX IF NOT EXISTS idx_results_node ON results(source, fingerprint);
CREATE INDEX IF NOT EXISTS idx_results_source_round ON results(source, round_id DESC);
CREATE INDEX IF NOT EXISTS idx_results_round_category ON results(round_id, category);
CREATE INDEX IF NOT EXISTS idx_results_endpoint
  ON results(source, fingerprint, endpoint_ip, round_id DESC);
