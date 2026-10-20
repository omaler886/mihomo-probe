-- 0006: explicit publication authority, additive over the existing ledger.
-- Do not infer an active export from MAX(id): legacy rows may be staged or
-- invalid. They remain available for inspection and explicit validated import.
CREATE TABLE IF NOT EXISTS active_exports (
  key TEXT PRIMARY KEY NOT NULL,
  snapshot_id INTEGER NOT NULL REFERENCES export_snapshots(id),
  activated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_active_exports_snapshot
  ON active_exports(snapshot_id);
-- Non-unique so preexisting duplicate legacy rows remain untouched.
CREATE INDEX IF NOT EXISTS idx_export_snapshots_content
  ON export_snapshots(key, sha256, id DESC);

-- The connection's foreign_keys pragma may be disabled by a legacy reader.
-- Enforce key ownership independently, without rewriting old snapshot data.
CREATE TRIGGER IF NOT EXISTS active_exports_insert_owner
BEFORE INSERT ON active_exports
WHEN NOT EXISTS (
  SELECT 1 FROM export_snapshots WHERE id = NEW.snapshot_id AND key = NEW.key
)
BEGIN
  SELECT RAISE(ABORT, 'export snapshot ownership mismatch');
END;

CREATE TRIGGER IF NOT EXISTS active_exports_update_owner
BEFORE UPDATE ON active_exports
WHEN NOT EXISTS (
  SELECT 1 FROM export_snapshots WHERE id = NEW.snapshot_id AND key = NEW.key
)
BEGIN
  SELECT RAISE(ABORT, 'export snapshot ownership mismatch');
END;

CREATE TRIGGER IF NOT EXISTS active_exports_prevent_snapshot_delete
BEFORE DELETE ON export_snapshots
WHEN EXISTS (SELECT 1 FROM active_exports WHERE snapshot_id = OLD.id)
BEGIN
  SELECT RAISE(ABORT, 'active export snapshot cannot be deleted');
END;

CREATE TRIGGER IF NOT EXISTS active_exports_prevent_snapshot_update
BEFORE UPDATE ON export_snapshots
WHEN EXISTS (SELECT 1 FROM active_exports WHERE snapshot_id = OLD.id)
BEGIN
  SELECT RAISE(ABORT, 'active export snapshot is immutable');
END;
