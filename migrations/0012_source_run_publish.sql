-- 0012: per-run publish bookkeeping (workstreams/18 B2, 《指导》§3.4).
--
-- The remote publish leg (Gist / composite) records its per-source outcome on
-- the run slot that produced the publication: `publish_status` is the short
-- vocabulary the GUI and the run detail read, `error_code` the publisher's
-- frozen error spelling (`gist_auth`, `rate_limited`, ...) or the sanitized
-- gist-side note a composite publish carries.
--
-- Additive like every migration since 0002: two nullable columns via
-- ALTER TABLE -- no row is rewritten, the Python ledger ignores them, and a
-- NULL means "this run predates the remote leg" (the local-only pipeline
-- writes nothing here).
ALTER TABLE source_runs ADD COLUMN publish_status TEXT;
ALTER TABLE source_runs ADD COLUMN error_code TEXT;

-- The reads the run detail page makes: a source's history newest-first, now
-- including its publish outcome. Re-declared as a plain index (the original
-- partial indexes from 0011 are untouched).
CREATE INDEX IF NOT EXISTS idx_source_runs_publish_status
    ON source_runs(source_id, publish_status);
