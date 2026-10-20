-- 0009_quality_samples: G07 quality sampling.
--
-- One row per sampled node per round: the bytes actually transferred and the
-- wall clock they took. Python never recorded throughput (it only measured
-- delay), so this table is a Rust extension and stays out of the way of every
-- Python-owned table: additive, no ALTER, nothing dropped.
CREATE TABLE IF NOT EXISTS quality_samples (
    round_id    INTEGER NOT NULL,
    source      TEXT    NOT NULL,
    fingerprint TEXT    NOT NULL,
    ts          TEXT    NOT NULL,
    bytes       INTEGER NOT NULL,
    elapsed_ms  REAL    NOT NULL,
    PRIMARY KEY (round_id, source, fingerprint)
);

-- The panel reads a node's newest samples, so the index leads with the node.
CREATE INDEX IF NOT EXISTS idx_quality_samples_node
    ON quality_samples (source, fingerprint, round_id DESC);