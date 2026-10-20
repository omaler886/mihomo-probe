-- 0004: Persistent DNS answers, separate from Python's domain_views cache.
--
-- The engine hashes domain/type/view/resolver/ECS into an opaque SHA-256 key.
-- No resolver URLs or credentials belong in this table's keys. Payloads are
-- caller-serialized JSON for either positive or negative answers.
-- Timestamps are UTC Unix seconds in the shared 1970..9999 range.

CREATE TABLE IF NOT EXISTS dns_cache (
  key TEXT PRIMARY KEY NOT NULL
    CHECK (typeof(key) = 'text'
      AND length(CAST(key AS BLOB)) = 64
      AND length(key) = 64
      AND key NOT GLOB '*[^0-9A-Fa-f]*'),
  value_json TEXT NOT NULL
    CHECK (typeof(value_json) = 'text'
      AND length(CAST(value_json AS BLOB)) BETWEEN 1 AND 1048576
      AND instr(value_json, char(0)) = 0
      AND json_valid(value_json)),
  stored_at INTEGER NOT NULL
    CHECK (typeof(stored_at) = 'integer'
      AND stored_at BETWEEN 0 AND 253402300799),
  expires_at INTEGER NOT NULL
    CHECK (typeof(expires_at) = 'integer'
      AND expires_at > stored_at
      AND expires_at <= 253402300799)
);
CREATE INDEX IF NOT EXISTS idx_dns_cache_expires_at ON dns_cache(expires_at);
