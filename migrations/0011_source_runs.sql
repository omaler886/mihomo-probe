-- 0011: per-source run slots (workstreams/18, 《指导》§7).
--
-- A measuring round is global -- one round walks every enabled source -- but
-- the API hands out *per-source* triggers (the GUI's per-card 立即测活). The
-- rule that must survive an operator clicking twice, or a client retrying a
-- request it believes was lost, is: at most ONE in-flight run per source, and
-- the same Idempotency-Key must resolve to the same run.
--
-- This table is that mutex. A trigger CLAIMS a slot first; the slot becomes
-- ACTIVE once the round actually starts, and it is CLOSED by the round task
-- (or by the scheduler's reaper after a crash). `round_id` is NULL while a
-- claim has no round yet, so the claim itself needs no round to exist.
--
-- Additive over the Python ledger like every migration since 0002.
CREATE TABLE IF NOT EXISTS source_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id       TEXT NOT NULL REFERENCES sources(source_id),
    round_id        INTEGER REFERENCES rounds(id),
    -- The claim's origin: 'api-manual' | 'cli-round' | 'schedule'.
    trigger         TEXT NOT NULL,
    status          TEXT NOT NULL
                    CHECK (status IN ('claimed', 'active', 'completed', 'failed',
                                      'cancelled', 'abandoned')),
    idempotency_key TEXT,
    claimed_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- The mutex: at most one un-closed slot per source, enforced by the database
-- rather than by the caller's discipline. A CLOSED slot is history and does
-- not participate.
CREATE UNIQUE INDEX IF NOT EXISTS ux_source_runs_one_open
    ON source_runs(source_id)
    WHERE status IN ('claimed', 'active');

-- An idempotency key resolves to exactly one slot for as long as the caller
-- may retry it. NULL keys (the normal scheduled case) stay outside the index:
-- SQLite treats each NULL as distinct, which is what we want.
CREATE UNIQUE INDEX IF NOT EXISTS ux_source_runs_idempotency
    ON source_runs(idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- The two reads the API and the GUI make: a source's open slot, and its run
-- history newest-first.
CREATE INDEX IF NOT EXISTS idx_source_runs_source_closed
    ON source_runs(source_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_source_runs_round
    ON source_runs(round_id)
    WHERE round_id IS NOT NULL;
