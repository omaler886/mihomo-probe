-- Keep per-address observations attributable after the in-memory round is gone.
-- These nullable additions preserve all Python inserts and existing observations.
ALTER TABLE results ADD COLUMN endpoint_ip TEXT;
ALTER TABLE results ADD COLUMN original_server TEXT;
ALTER TABLE results ADD COLUMN protocol TEXT;
ALTER TABLE results ADD COLUMN dns_incomplete INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_results_endpoint
  ON results(source, fingerprint, endpoint_ip, round_id DESC);
