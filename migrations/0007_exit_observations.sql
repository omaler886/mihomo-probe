-- Persist real exit identities independently of node entry/server addresses.
ALTER TABLE results ADD COLUMN exit_ip TEXT;
ALTER TABLE results ADD COLUMN exit_asn TEXT;
ALTER TABLE results ADD COLUMN verification_error TEXT;
ALTER TABLE results ADD COLUMN verification_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN exit_ip TEXT;
ALTER TABLE nodes ADD COLUMN exit_asn TEXT;
CREATE TABLE IF NOT EXISTS exit_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  round_id INTEGER NOT NULL,
  source TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  endpoint_ip TEXT,
  exit_ip TEXT NOT NULL,
  country TEXT,
  asn TEXT,
  observed_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_exit_history_node
  ON exit_history(source, fingerprint, id DESC);
