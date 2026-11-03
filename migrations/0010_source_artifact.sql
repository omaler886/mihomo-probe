-- 0010: permanent source identity and the one-artifact-per-source registry
-- (workstreams/18, 《mihomo-probe 精简重构指导》§2.1/§5).
--
-- Before this migration a "source" existed only as a TEXT column whose value
-- was the configured key: rename the key and every historical row, every
-- export snapshot and every download URL silently became a different source.
-- These two tables give each logical source an immutable `source_id` and pin
-- exactly one artifact row to it, so a rename, a re-run or a scheduler restart
-- can only ever update the same artifact instead of minting a new one.
--
-- Additive over the Python ledger, like every migration since 0002: nothing is
-- altered, dropped or rewritten. The backfill below is INSERT OR IGNORE, so a
-- ledger that already carries rows (or is migrated twice through a restored
-- backup) keeps its first-assigned ids.
CREATE TABLE IF NOT EXISTS sources (
    source_id   TEXT PRIMARY KEY NOT NULL,
    -- The configured key is the natural key: `SourceSpec.key` is already
    -- de-duplicated with a -2/-3 suffix at config load, and it is what the
    -- ledger's `source` columns and the export keys carry.
    key         TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    -- 'collection' | 'sub' | 'unknown'. Backfilled rows read 'unknown' until
    -- the service re-syncs them from the live config on its next start.
    input_type  TEXT NOT NULL,
    input_ref   TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- One artifact per source, forever: `ux_artifacts_source_id` is the hard form
-- of "a subscription can never grow a second export product". `gist_file` is
-- unique too, because a Gist is a single flat namespace -- two sources sharing
-- a file name would silently overwrite each other every round.
CREATE TABLE IF NOT EXISTS artifacts (
    artifact_id           TEXT PRIMARY KEY NOT NULL,
    source_id             TEXT NOT NULL UNIQUE REFERENCES sources(source_id),
    profile_id            TEXT NOT NULL DEFAULT 'default',
    gist_id               TEXT,
    gist_file             TEXT NOT NULL UNIQUE,
    stable_url            TEXT,
    last_content_sha256   TEXT,
    last_success_run_id   INTEGER,
    last_published_at     TEXT
);

CREATE INDEX IF NOT EXISTS idx_artifacts_gist_id ON artifacts(gist_id);

-- Backfill: every key that already owns ledger rows or an export snapshot
-- becomes a source. The id is a random v4 UUID generated in SQL so no runtime
-- dependency is needed for a one-time data migration:
--   * byte 6 high nibble forced to '4' (version 4),
--   * byte 8 high bits forced to the 10xx variant range ('8'..'b').
-- `randomblob` is per-row, so each key gets its own id, and UNIQUE(key) plus
-- OR IGNORE makes a second apply a no-op.
INSERT OR IGNORE INTO sources
    (source_id, key, display_name, input_type, input_ref, enabled, created_at, updated_at)
SELECT
    substr(h, 1, 8) || '-' || substr(h, 9, 4) || '-4' || substr(h, 14, 3) || '-'
    || substr('89ab', (unicode(substr(h, 17, 1)) % 4) + 1, 1) || substr(h, 18, 3)
    || '-' || substr(h, 21, 12),
    key,
    key,
    'unknown',
    key,
    1,
    strftime('%Y-%m-%dT%H:%M:%S', 'now'),
    strftime('%Y-%m-%dT%H:%M:%S', 'now')
FROM (
    SELECT DISTINCT key, lower(hex(randomblob(16))) AS h FROM export_snapshots
    UNION
    SELECT DISTINCT source AS key, lower(hex(randomblob(16))) AS h FROM nodes
);

-- And one artifact per backfilled source: the file name follows the key, which
-- is unique, and the id embeds the source id so the pair is recognisable in
-- logs without a join. `last_content_sha256` starts NULL: the first publish
-- after the upgrade is a real change, which is the safe default.
INSERT OR IGNORE INTO artifacts (artifact_id, source_id, profile_id, gist_file)
SELECT 'artifact:' || source_id, source_id, 'default', key || '.yaml'
FROM sources;
