-- 0003: R7 round guard (workstreams/03, ADR-0005).
--
-- ADR-0005 ruling: `inconclusive` is a ROUND-level mark, not a sixth node
-- status. The five node statuses (unknown/alive/pending/dead/excluded) stay
-- exactly as they are; what changes is that a round whose kernel was
-- unreachable (or whose config never got generated) is recorded as
-- inconclusive=1, making explicit what the runner already does implicitly:
-- such a round tests nothing, advances no streak, and must never serve as
-- the alive baseline for the suspect guard (`previous_alive_count` skips it
-- the same way it skips suspect rounds).
--
-- Python compatibility (shadow phase, shared database): the Python service
-- never reads this column, and its INSERTs rely on the DEFAULT 0, so rows it
-- writes are unchanged. When Python reads a Rust-written inconclusive round
-- it sees "a finished round with ok=0" -- which feeds `_previous_alive_count`
-- conservatively: the guard then protects the previous publication exactly
-- as intended. No Python-side change is required.

ALTER TABLE rounds ADD COLUMN inconclusive INTEGER NOT NULL DEFAULT 0;
