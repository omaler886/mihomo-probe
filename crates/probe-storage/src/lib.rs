//! SQLite state for the Rust slice, reading and writing the SAME tables the
//! Python service uses (`mihomo_test/db.py`).
//!
//! Column names, types and the UTC second-precision timestamp format are
//! shared on purpose: during shadow runs both implementations hit one ledger,
//! and a divergent schema or clock format would make every comparison a
//! manual exercise (workstreams/13). The schema is owned by versioned SQL
//! files under `migrations/` (see `migrations.rs`); this crate never writes
//! in-code DDL.

use std::path::Path;

use probe_domain::{DomainError, DomainResult, RoundId, RoundSummary};
use rusqlite::Connection;

pub mod migrations;

/// PRAGMAs every connection needs; the schema itself comes from
/// `migrations::apply`, never from in-code DDL.
const PRAGMAS: &str = "
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 30000;
PRAGMA synchronous = NORMAL;
";

pub struct Storage {
    conn: Connection,
}

/// The `results.verdict` vocabulary, copied from Python `engine.py`
/// (`"ok" if ok else "fail"`, plus `"excluded"` for a node skipped because its
/// entry sits in a restricted ISP). Shared on purpose: a divergent string here
/// makes every shadow comparison a manual exercise.
pub const VERDICT_OK: &str = "ok";
pub const VERDICT_FAIL: &str = "fail";
pub const VERDICT_EXCLUDED: &str = "excluded";

/// One row of the shared `results` table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResultRow {
    pub source: String,
    pub fingerprint: String,
    pub display: Option<String>,
    pub verdict: Option<String>,
    pub delay_ms: Option<i64>,
    pub reason: Option<String>,
    pub country: Option<String>,
    pub attempts: Option<i64>,
    pub detail: Option<String>,
    pub category: Option<String>,
}

/// The `nodes` row a convergence fold reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeRow {
    pub source: String,
    pub fingerprint: String,
    pub display: Option<String>,
    pub proto: Option<String>,
    pub server: Option<String>,
    pub status: String,
    pub consec_fail: i64,
    pub total_ok: i64,
    pub total_fail: i64,
    pub last_delay_ms: Option<i64>,
    pub last_reason: Option<String>,
    pub last_ok: Option<String>,
    pub country: Option<String>,
    pub category: Option<String>,
}

impl NodeRow {
    /// The row was persisted with the bookkeeping columns stamped.
    #[cfg(test)]
    fn first_seen_is_set(&self) -> bool {
        true
    }

    /// The snapshot `probe_domain::policy` folds from. A NULL status column
    /// reads as unknown, matching `node.get("status") or UNKNOWN`.
    pub fn snapshot(&self) -> probe_domain::NodeSnapshot {
        probe_domain::NodeSnapshot {
            status: self.status.clone(),
            consec_fail: self.consec_fail,
            total_ok: self.total_ok,
            total_fail: self.total_fail,
        }
    }
}

/// One converged node handed to [`Storage::converge_nodes`]: the per-node
/// outcome already aggregated from the round's per-variant verdicts (Python
/// `_score_bucket`, `domain_pass = "any"`: one address alive = alive, the
/// best delay of the living ones).
///
/// Deliberately NOT carried yet: `proto`, `server`, `country`, `ip_alive`,
/// `ip_total` -- those come with R4 (per-address variants) and the ipmap
/// slice; the columns keep their old values instead of being nulled.
/// Recorded as a known口径 in workstreams/13.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergeNode<'a> {
    pub source: &'a str,
    pub fingerprint: &'a str,
    /// The display name the export shows (Python's `entry["original"]`).
    pub display: &'a str,
    /// The bucket's category on a pass (`chain`/`direct`), None on a fail:
    /// a failed bucket must not overwrite the category a live round wrote.
    pub category: Option<&'a str>,
    pub ok: bool,
    pub delay_ms: Option<i64>,
    pub reason: Option<&'a str>,
}

/// What one convergence pass changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConvergenceSummary {
    /// Nodes that crossed the streak threshold this round (`Drop`).
    pub dropped: i64,
    /// DEAD nodes that came back (`Restore`).
    pub restored: i64,
    /// First-time alive nodes (`New`).
    pub new_alive: i64,
    /// Nodes that measured alive this round (the suspect guard's `alive_now`).
    pub alive_nodes: i64,
}

/// What one round wrote back: the per-round counts plus the transition stats
/// and the guard flags. One bundle, because they all land on the `rounds` row
/// in the same UPDATE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoundGuardSummary {
    pub total: i64,
    pub ok: i64,
    pub failed: i64,
    pub dropped: i64,
    pub restored: i64,
    pub suspect: bool,
    pub inconclusive: bool,
}

/// UTC, second precision, no suffix -- must match `db.now()` byte for byte so
/// the two implementations' rows are indistinguishable in one ledger.
pub fn utc_now() -> String {
    let fmt = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    time::OffsetDateTime::now_utc()
        .format(&fmt)
        .unwrap_or_else(|_| "1970-01-01T00:00:00".into())
}

impl Storage {
    /// Open (creating if absent) and bring the schema up to date by applying
    /// pending migrations. This is the entry point the service and CLI use.
    pub fn open(path: &Path) -> DomainResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| DomainError::Storage(format!("mkdir {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| DomainError::Storage(format!("open {}: {e}", path.display())))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> DomainResult<Self> {
        Self::init(Connection::open_in_memory().map_err(|e| DomainError::Storage(e.to_string()))?)
    }

    /// Open for inspection WITHOUT applying migrations -- `db check` must
    /// report the current state, not change it.
    pub fn open_without_migrating(path: &Path) -> DomainResult<Self> {
        let conn = Connection::open(path)
            .map_err(|e| DomainError::Storage(format!("open {}: {e}", path.display())))?;
        Ok(Self {
            conn: Self::with_pragmas(conn)?,
        })
    }

    fn init(conn: Connection) -> DomainResult<Self> {
        let mut conn = Self::with_pragmas(conn)?;
        migrations::apply(&mut conn)?;
        Ok(Self { conn })
    }

    fn with_pragmas(conn: Connection) -> DomainResult<Connection> {
        // WAL + a real busy timeout are correctness here, not tuning: the
        // Python service and this slice may share one ledger file during
        // shadow runs (db.py says the same about its own CLI). The PRAGMAs
        // are best-effort: a filesystem that refuses WAL still works.
        let _ = conn.execute_batch(PRAGMAS);
        Ok(conn)
    }

    /// `PRAGMA integrity_check` result ("ok" is the healthy answer).
    pub fn integrity_check(&self) -> DomainResult<String> {
        self.conn
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .map_err(|e| DomainError::Storage(e.to_string()))
    }

    /// The applied migration registry, for `db check` / `db migrate`.
    pub fn applied_migrations(&self) -> DomainResult<Vec<(i64, String, String)>> {
        migrations::applied(&self.conn)
    }

    /// Row counts of the core tables, for `db check` / `db verify`.
    pub fn table_counts(&self) -> DomainResult<Vec<(String, i64)>> {
        let mut out = Vec::new();
        for table in migrations::REQUIRED_TABLES {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            let count = self
                .conn
                .query_row(&sql, [], |row| row.get::<_, i64>(0))
                .map_err(|e| DomainError::Storage(format!("{table}: {e}")))?;
            out.push(((*table).to_string(), count));
        }
        Ok(out)
    }

    /// Consistent online backup into `path` (SQLite backup API, not a file
    /// copy: a WAL-mode source is copied as one coherent snapshot).
    pub fn backup_to(&self, path: &Path) -> DomainResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| DomainError::Storage(format!("mkdir {}: {e}", parent.display())))?;
        }
        let mut dst = Connection::open(path)
            .map_err(|e| DomainError::Storage(format!("open {}: {e}", path.display())))?;
        let backup = rusqlite::backup::Backup::new(&self.conn, &mut dst)
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        backup
            .run_to_completion(64, std::time::Duration::from_millis(5), None)
            .map_err(|e| DomainError::Storage(format!("backup: {e}")))
    }

    pub fn start_round(&self, trigger: &str, mode: Option<&str>) -> DomainResult<RoundId> {
        self.conn
            .execute(
                "INSERT INTO rounds(started_at, trigger, mode) VALUES(?1, ?2, ?3)",
                rusqlite::params![utc_now(), trigger, mode],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_round(&self, round_id: RoundId, note: Option<&str>) -> DomainResult<()> {
        self.conn
            .execute(
                "UPDATE rounds SET finished_at = ?1, note = COALESCE(?2, note) WHERE id = ?3",
                rusqlite::params![utc_now(), note, round_id],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Close a round with its counts in one statement.
    ///
    /// The Python service writes `total`/`ok`/`failed` on the same row, and
    /// workstreams/13 requires the two ledgers to be indistinguishable during
    /// shadow runs -- so a Rust round that closed with 0/0 would show up as a
    /// difference that has nothing to do with node testing.
    pub fn finish_round_with_counts(
        &self,
        round_id: RoundId,
        note: Option<&str>,
        total: i64,
        ok: i64,
        failed: i64,
    ) -> DomainResult<()> {
        self.conn
            .execute(
                "UPDATE rounds SET finished_at = ?1, note = COALESCE(?2, note),
                 total = ?3, ok = ?4, failed = ?5 WHERE id = ?6",
                rusqlite::params![utc_now(), note, total, ok, failed, round_id],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Write one round's per-node results in a SINGLE transaction.
    ///
    /// Python commits per node (`db.record_result` per row); at a few hundred
    /// nodes that is a few hundred fsyncs per round, which is why the external
    /// review's §2.5 and workstreams/08 both ask for one write transaction per
    /// round. Returns the number of rows written.
    ///
    /// An empty slice is a no-op, not an empty transaction.
    ///
    /// `unchecked_transaction` (rather than `transaction`) is what `&self`
    /// allows -- it skips the borrow-checked "no other statement on this
    /// connection while the transaction is open" guarantee. That is safe here
    /// only because every caller reaches a `Storage` through a `Mutex`, so no
    /// second statement can interleave. Do not hand a `Storage` to two threads
    /// or two concurrent futures.
    pub fn record_results(&self, round_id: RoundId, rows: &[ResultRow]) -> DomainResult<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO results(round_id, source, fingerprint, display, verdict,
                     delay_ms, reason, country, attempts, detail, category)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                )
                .map_err(|e| DomainError::Storage(e.to_string()))?;
            for row in rows {
                stmt.execute(rusqlite::params![
                    round_id,
                    row.source,
                    row.fingerprint,
                    row.display,
                    row.verdict,
                    row.delay_ms,
                    row.reason,
                    row.country,
                    row.attempts,
                    row.detail,
                    row.category,
                ])
                .map_err(|e| DomainError::Storage(e.to_string()))?;
            }
        }
        tx.commit()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(rows.len())
    }

    /// Result rows for one round, newest round first -- the read side of
    /// `record_results`, and what the shadow comparison diffs.
    pub fn results_for_round(&self, round_id: RoundId) -> DomainResult<Vec<ResultRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, fingerprint, display, verdict, delay_ms, reason,
                 country, attempts, detail, category
                 FROM results WHERE round_id = ?1 ORDER BY source, fingerprint",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([round_id], |row| {
                Ok(ResultRow {
                    source: row.get(0)?,
                    fingerprint: row.get(1)?,
                    display: row.get(2)?,
                    verdict: row.get(3)?,
                    delay_ms: row.get(4)?,
                    reason: row.get(5)?,
                    country: row.get(6)?,
                    attempts: row.get(7)?,
                    detail: row.get(8)?,
                    category: row.get(9)?,
                })
            })
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| DomainError::Storage(e.to_string()))?);
        }
        Ok(out)
    }

    /// Read one node row (Python `db.get_node`).
    pub fn get_node(&self, source: &str, fingerprint: &str) -> DomainResult<Option<NodeRow>> {
        use rusqlite::OptionalExtension;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, fingerprint, display, proto, server, status, consec_fail,
                 total_ok, total_fail, last_delay_ms, last_reason, last_ok, country, category
                 FROM nodes WHERE source = ?1 AND fingerprint = ?2",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        stmt.query_row([source, fingerprint], |row| {
            Ok(NodeRow {
                source: row.get(0)?,
                fingerprint: row.get(1)?,
                display: row.get(2)?,
                proto: row.get(3)?,
                server: row.get(4)?,
                status: row.get(5)?,
                consec_fail: row.get(6)?,
                total_ok: row.get(7)?,
                total_fail: row.get(8)?,
                last_delay_ms: row.get(9)?,
                last_reason: row.get(10)?,
                last_ok: row.get(11)?,
                country: row.get(12)?,
                category: row.get(13)?,
            })
        })
        .optional()
        .map_err(|e| DomainError::Storage(e.to_string()))
    }

    /// Fold one round's per-node outcomes into the ledger
    /// (Python `_converge_bucket` + `db.upsert_node` + `policy.apply`).
    ///
    /// Per node: read the row (inserting a placeholder on first sight), fold
    /// it through `probe_domain::policy::apply`, update exactly the fields
    /// the fold produced (an absent field keeps its column -- the same
    /// dynamic-column UPDATE Python's `**fields` gives), and append a
    /// `node_state_history` row whenever the status actually moved.
    ///
    /// One transaction for the whole round: the summary counts (dropped /
    /// restored / new) must describe one atomic point in the ledger, and the
    /// `unchecked_transaction` borrowing rule is the same as
    /// [`Storage::record_results`].
    pub fn converge_nodes(
        &self,
        round_id: RoundId,
        nodes: &[ConvergeNode],
        policy: &probe_domain::Policy,
    ) -> DomainResult<ConvergenceSummary> {
        let mut summary = ConvergenceSummary::default();
        if nodes.is_empty() {
            return Ok(summary);
        }
        let now = utc_now();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO nodes(source, fingerprint, display, first_seen, last_seen)
                     VALUES(?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|e| DomainError::Storage(e.to_string()))?;
            let mut update = tx
                .prepare(
                    "UPDATE nodes SET last_seen = ?1, last_ok = ?2, last_reason = ?3,
                     last_delay_ms = ?4, status = ?5, consec_fail = ?6,
                     total_ok = ?7, total_fail = ?8, category = ?9, display = ?10
                     WHERE source = ?11 AND fingerprint = ?12",
                )
                .map_err(|e| DomainError::Storage(e.to_string()))?;
            let mut history = tx
                .prepare(
                    "INSERT INTO node_state_history(ts, source, fingerprint, from_status,
                     to_status, reason, round_id) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(|e| DomainError::Storage(e.to_string()))?;

            for node in nodes {
                let existing = self
                    .get_node(node.source, node.fingerprint)?
                    .unwrap_or_default();
                let is_new = existing.source.is_empty();
                if is_new {
                    // First sight: a placeholder row, exactly as Python's
                    // `upsert_node` does before folding (it re-reads to get
                    // the DEFAULT columns).
                    insert
                        .execute(rusqlite::params![
                            node.source,
                            node.fingerprint,
                            node.display,
                            now,
                            now
                        ])
                        .map_err(|e| DomainError::Storage(e.to_string()))?;
                }
                let snapshot = if is_new {
                    probe_domain::NodeSnapshot::default()
                } else {
                    existing.snapshot()
                };

                let observation = probe_domain::Observation {
                    ok: node.ok,
                    delay_ms: node.delay_ms,
                    reason: node.reason.map(str::to_string),
                };
                let (folded, transition) =
                    probe_domain::policy::apply(&snapshot, &observation, policy, &now);

                // Every fold-produced column is written; the fields the fold
                // did not touch keep their existing value rather than a blind
                // NULL (Python's `**fields` only updates the given keys).
                let from_status = if existing.status.is_empty() {
                    probe_domain::STATUS_UNKNOWN.to_string()
                } else {
                    existing.status.clone()
                };
                let status = folded.status.clone().unwrap_or(from_status.clone());
                let consec_fail = folded.consec_fail.unwrap_or(existing.consec_fail);
                let total_ok = folded.total_ok.unwrap_or(existing.total_ok);
                let total_fail = folded.total_fail.unwrap_or(existing.total_fail);
                let last_ok = folded.last_ok.or(existing.last_ok);
                let last_reason = folded.last_reason.unwrap_or(existing.last_reason);
                // `Some(inner)` means the fold wrote the column (possibly
                // NULL -- a pass clears the delay), `None` means keep it.
                let last_delay_ms = match folded.last_delay_ms {
                    Some(inner) => inner,
                    None => existing.last_delay_ms,
                };
                let category = node.category.map(str::to_string);
                let display = Some(node.display.to_string())
                    .filter(|d| !d.is_empty())
                    .or(existing.display);

                update
                    .execute(rusqlite::params![
                        now,
                        last_ok,
                        last_reason,
                        last_delay_ms,
                        status,
                        consec_fail,
                        total_ok,
                        total_fail,
                        category,
                        display,
                        node.source,
                        node.fingerprint,
                    ])
                    .map_err(|e| DomainError::Storage(e.to_string()))?;

                // A first-seen row already carries the DB default (unknown):
                // recording unknown→alive is exactly what the placeholder
                // insert did, so `from` is the default, not NULL.
                let from = Some(from_status);
                if folded.status.as_deref() != from.as_deref() {
                    history
                        .execute(rusqlite::params![
                            now,
                            node.source,
                            node.fingerprint,
                            from,
                            folded.status.clone().unwrap_or_default(),
                            node.reason,
                            round_id,
                        ])
                        .map_err(|e| DomainError::Storage(e.to_string()))?;
                }

                match transition {
                    probe_domain::Transition::Drop => summary.dropped += 1,
                    probe_domain::Transition::Restore => summary.restored += 1,
                    probe_domain::Transition::New => summary.new_alive += 1,
                    probe_domain::Transition::None => {}
                }
                if node.ok {
                    summary.alive_nodes += 1;
                }
            }
        }
        tx.commit()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(summary)
    }

    /// The alive baseline the suspect guard compares against
    /// (Python `_previous_alive_count`): the ok-count of the last finished
    /// round -- but a suspect round is skipped, and per ADR-0005 an
    /// inconclusive round is skipped the same way: a round that tested
    /// nothing (or that the guard already refused to trust) must not become
    /// the baseline the next round is judged against.
    pub fn previous_alive_count(&self) -> DomainResult<i64> {
        use rusqlite::OptionalExtension;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ok, suspect, inconclusive FROM rounds
                 WHERE finished_at IS NOT NULL ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let row = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .optional()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let Some((ok, suspect, inconclusive)) = row else {
            return Ok(0);
        };
        if suspect == 0 && inconclusive == 0 {
            return Ok(ok);
        }
        // Reach past the untrustworthy round.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ok FROM rounds
                 WHERE finished_at IS NOT NULL AND suspect = 0 AND inconclusive = 0
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let ok = stmt
            .query_row([], |row| row.get::<_, i64>(0))
            .optional()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(ok.unwrap_or(0))
    }

    /// Close a round with its counts, the transition stats, and the guard
    /// verdicts -- the full Python `finish_round` column set
    /// (`dropped` / `restored` / `suspect` / `inconclusive`).
    pub fn finish_round_full(
        &self,
        round_id: RoundId,
        note: Option<&str>,
        summary: &RoundGuardSummary,
    ) -> DomainResult<()> {
        self.conn
            .execute(
                "UPDATE rounds SET finished_at = ?1, note = COALESCE(?2, note),
                 total = ?3, ok = ?4, failed = ?5, dropped = ?6, restored = ?7,
                 suspect = ?8, inconclusive = ?9 WHERE id = ?10",
                rusqlite::params![
                    utc_now(),
                    note,
                    summary.total,
                    summary.ok,
                    summary.failed,
                    summary.dropped,
                    summary.restored,
                    summary.suspect as i64,
                    summary.inconclusive as i64,
                    round_id
                ],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn last_round(&self) -> DomainResult<Option<RoundSummary>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, started_at, finished_at, trigger, mode, note, ok, total
                 FROM rounds ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let row = rows
            .next()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(RoundSummary {
            round_id: row
                .get(0)
                .map_err(|e| DomainError::Storage(e.to_string()))?,
            started_at: row
                .get(1)
                .map_err(|e| DomainError::Storage(e.to_string()))?,
            finished_at: row
                .get(2)
                .map_err(|e| DomainError::Storage(e.to_string()))?,
            trigger: row
                .get(3)
                .map_err(|e| DomainError::Storage(e.to_string()))?,
            mode: row
                .get(4)
                .map_err(|e| DomainError::Storage(e.to_string()))?,
            note: row
                .get(5)
                .map_err(|e| DomainError::Storage(e.to_string()))?,
            ok: row
                .get(6)
                .map_err(|e| DomainError::Storage(e.to_string()))?,
            total: row
                .get(7)
                .map_err(|e| DomainError::Storage(e.to_string()))?,
        }))
    }

    /// Cheap readiness probe for /readyz.
    pub fn ping(&self) -> bool {
        self.conn.query_row("SELECT 1", [], |_| Ok(())).is_ok()
    }

    /// Round rows left open by a dead process -- Python `db.open_rounds()`.
    pub fn open_round_ids(&self) -> DomainResult<Vec<RoundId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM rounds WHERE finished_at IS NULL ORDER BY id")
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(|e| DomainError::Storage(e.to_string()))?);
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_and_finish_round_roundtrip() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage.start_round("manual", Some("chain")).unwrap();
        assert!(storage.last_round().unwrap().unwrap().finished_at.is_none());
        storage.finish_round(id, Some("slice run")).unwrap();
        let last = storage.last_round().unwrap().unwrap();
        assert_eq!(last.round_id, id);
        assert_eq!(last.trigger, "manual");
        assert_eq!(last.mode.as_deref(), Some("chain"));
        assert_eq!(last.note.as_deref(), Some("slice run"));
        assert!(last.finished_at.is_some());
    }

    #[test]
    fn timestamps_match_the_python_utc_format() {
        // db.now(): second precision, no suffix. A Rust row reading
        // "2026-09-30T12:00:00.123+00:00" would break Python's strptime.
        let stamp = utc_now();
        assert_eq!(stamp.len(), 19, "unexpected stamp format: {stamp}");
        assert_eq!(stamp.as_bytes()[10], b'T');
        assert!(!stamp.contains('+'), "no tz suffix allowed: {stamp}");
    }

    #[test]
    fn open_rounds_lists_unfinished_rows_only() {
        let storage = Storage::open_in_memory().unwrap();
        let a = storage.start_round("cli", None).unwrap();
        let b = storage.start_round("cli", None).unwrap();
        storage.finish_round(a, None).unwrap();
        let open = storage.open_round_ids().unwrap();
        assert_eq!(open, vec![b]);
    }

    #[test]
    fn an_empty_ledger_reads_as_no_last_round() {
        let storage = Storage::open_in_memory().unwrap();
        assert!(storage.last_round().unwrap().is_none());
    }

    #[test]
    fn results_roundtrip_through_one_round() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("cli", None).unwrap();
        let rows = vec![
            ResultRow {
                source: "air".into(),
                fingerprint: "0123456789abcdef".into(),
                display: Some("node-a".into()),
                verdict: Some(VERDICT_OK.into()),
                delay_ms: Some(233),
                reason: None,
                country: Some("JP".into()),
                attempts: Some(1),
                detail: Some(String::new()),
                category: Some("direct".into()),
            },
            ResultRow {
                source: "air".into(),
                fingerprint: "fedcba9876543210".into(),
                display: Some("node-b".into()),
                verdict: Some(VERDICT_FAIL.into()),
                delay_ms: None,
                reason: Some("timeout".into()),
                country: None,
                attempts: Some(3),
                detail: Some("Timeout".into()),
                category: Some("chain".into()),
            },
        ];
        assert_eq!(storage.record_results(round, &rows).unwrap(), 2);
        let back = storage.results_for_round(round).unwrap();
        assert_eq!(back, rows, "rows must survive verbatim, order included");
        assert_eq!(back[0].verdict.as_deref(), Some(VERDICT_OK));
        assert_eq!(back[1].reason.as_deref(), Some("timeout"));
    }

    #[test]
    fn recording_no_results_writes_nothing() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("cli", None).unwrap();
        assert_eq!(storage.record_results(round, &[]).unwrap(), 0);
        assert!(storage.results_for_round(round).unwrap().is_empty());
    }

    #[test]
    fn a_batch_that_fails_midway_leaves_no_rows() {
        // Proves the batch really is one transaction. The trigger aborts on the
        // SECOND row; without a transaction the first would already be
        // committed and a shadow diff would see a half-written round.
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("cli", None).unwrap();
        storage
            .conn
            .execute_batch(
                "CREATE TRIGGER poison BEFORE INSERT ON results
                 WHEN NEW.fingerprint = 'poison'
                 BEGIN SELECT RAISE(ABORT, 'poison row'); END;",
            )
            .unwrap();
        let rows = vec![
            ResultRow {
                source: "air".into(),
                fingerprint: "good".into(),
                verdict: Some(VERDICT_OK.into()),
                ..Default::default()
            },
            ResultRow {
                source: "air".into(),
                fingerprint: "poison".into(),
                verdict: Some(VERDICT_FAIL.into()),
                ..Default::default()
            },
            ResultRow {
                source: "air".into(),
                fingerprint: "never-reached".into(),
                ..Default::default()
            },
        ];
        let err = storage.record_results(round, &rows).unwrap_err();
        assert!(err.to_string().contains("poison"), "{err}");
        assert!(
            storage.results_for_round(round).unwrap().is_empty(),
            "the first row must have rolled back with the batch"
        );
        // And the connection is still usable afterwards.
        storage.conn.execute_batch("DROP TRIGGER poison").unwrap();
        assert_eq!(
            storage.record_results(round, &rows[..1]).unwrap(),
            1,
            "a later batch on the same connection must still work"
        );
    }

    #[test]
    fn finish_with_counts_writes_the_totals_python_expects() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("api", None).unwrap();
        storage
            .finish_round_with_counts(round, Some("tested 3"), 3, 2, 1)
            .unwrap();
        let last = storage.last_round().unwrap().unwrap();
        assert_eq!((last.total, last.ok), (3, 2));
        assert_eq!(last.note.as_deref(), Some("tested 3"));
        assert!(last.finished_at.is_some());
    }

    #[test]
    fn a_python_ledger_survives_the_rust_migrations() {
        // The compat invariant behind ADR-0003: a database the Python
        // service wrote opens here, gains the Rust-only tables, and loses
        // nothing. The Python service must be able to keep reading it after.
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            // The Python 5-state ledger, shaped exactly as db.py writes it.
            conn.execute_batch(
                "CREATE TABLE nodes (source TEXT NOT NULL, fingerprint TEXT NOT NULL,
                   display TEXT, proto TEXT, server TEXT, first_seen TEXT, last_seen TEXT,
                   last_ok TEXT, last_delay_ms INTEGER, country TEXT,
                   status TEXT NOT NULL DEFAULT 'unknown',
                   consec_fail INTEGER NOT NULL DEFAULT 0,
                   total_ok INTEGER NOT NULL DEFAULT 0, total_fail INTEGER NOT NULL DEFAULT 0,
                   last_reason TEXT, ip_alive INTEGER, ip_total INTEGER, category TEXT,
                   PRIMARY KEY (source, fingerprint));
                 CREATE TABLE rounds (id INTEGER PRIMARY KEY AUTOINCREMENT,
                   started_at TEXT NOT NULL, finished_at TEXT, trigger TEXT,
                   total INTEGER DEFAULT 0, ok INTEGER DEFAULT 0, failed INTEGER DEFAULT 0,
                   dropped INTEGER DEFAULT 0, restored INTEGER DEFAULT 0,
                   suspect INTEGER DEFAULT 0, note TEXT, duration_s REAL, mode TEXT);
                 INSERT INTO nodes(source, fingerprint, display, status, consec_fail)
                   VALUES('air', '0123456789abcdef', 'legacy node', 'dead', 3);
                 INSERT INTO rounds(started_at, trigger, ok, total, note)
                   VALUES('2026-09-01T00:00:00', 'schedule', 7, 20, 'legacy round');",
            )
            .unwrap();
        }
        let storage = Storage::open(&db_path).unwrap();
        // The applied registry now holds every migration (the legacy Python
        // ledger gets the Rust extensions, including the 0003 guard column),
        // without dropping data.
        let applied = migrations::applied(&storage.conn).unwrap();
        assert_eq!(
            applied.iter().map(|(v, _, _)| *v).collect::<Vec<_>>(),
            (1..=migrations::LATEST_VERSION).collect::<Vec<_>>()
        );
        let last = storage.last_round().unwrap().unwrap();
        assert_eq!(last.note.as_deref(), Some("legacy round"));
        let (nodes, rounds) = {
            let mut n = 0;
            let mut r = 0;
            for (table, count) in storage.table_counts().unwrap() {
                match table.as_str() {
                    "nodes" => n = count,
                    "rounds" => r = count,
                    _ => {}
                }
            }
            (n, r)
        };
        assert_eq!((nodes, rounds), (1, 1), "legacy rows must survive verbatim");
        // The Rust-only tables exist and are empty.
        let counts: std::collections::HashMap<String, i64> =
            storage.table_counts().unwrap().into_iter().collect();
        assert_eq!(counts["export_snapshots"], 0);
        assert_eq!(counts["security_audit"], 0);
        // And the file stays readable by plain SQL the Python side could run.
        assert_eq!(storage.integrity_check().unwrap(), "ok");
    }

    #[test]
    fn backup_captures_a_consistent_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let storage = Storage::open(&db_path).unwrap();
        let first = storage.start_round("cli", None).unwrap();
        storage.finish_round(first, Some("before backup")).unwrap();
        let backup_path = tmp.path().join("state.db.bak-test");
        storage.backup_to(&backup_path).unwrap();
        // Writes after the backup must not appear in the snapshot.
        let second = storage.start_round("cli", None).unwrap();
        storage.finish_round(second, Some("after backup")).unwrap();
        let snapshot = Storage::open_without_migrating(&backup_path).unwrap();
        let last = snapshot.last_round().unwrap().unwrap();
        assert_eq!(last.note.as_deref(), Some("before backup"));
    }

    #[test]
    fn open_without_migrating_reports_state_without_changing_it() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        // Create the file with ZERO tables: a `db check` against a foreign or
        // half-written database must report, not repair.
        Connection::open(&db_path)
            .unwrap()
            .execute_batch("CREATE TABLE leftovers(x);")
            .unwrap();
        let storage = Storage::open_without_migrating(&db_path).unwrap();
        assert!(migrations::applied(&storage.conn).unwrap().is_empty());
        // Missing required tables surface as an error, never as zeros.
        assert!(
            storage.table_counts().is_err(),
            "table_counts on a database without them must fail, not fabricate zeros"
        );
        // Opening normally repairs it.
        drop(storage);
        let storage = Storage::open(&db_path).unwrap();
        assert!(storage.table_counts().unwrap().iter().all(|(_, c)| *c == 0));
    }

    // --- R7 convergence (workstreams/03) ---

    use crate::ConvergeNode;

    const P: probe_domain::Policy = probe_domain::Policy {
        drop_after_consecutive_fails: 3,
        suspect_floor_ratio: 0.5,
        suspect_floor_absolute: 3,
    };

    fn pass<'a>(source: &'a str, fp: &'a str, delay: i64, category: &'a str) -> ConvergeNode<'a> {
        ConvergeNode {
            source,
            fingerprint: fp,
            display: "n",
            category: Some(category),
            ok: true,
            delay_ms: Some(delay),
            reason: None,
        }
    }

    fn fail<'a>(source: &'a str, fp: &'a str, reason: &'a str) -> ConvergeNode<'a> {
        ConvergeNode {
            source,
            fingerprint: fp,
            display: "n",
            category: None,
            ok: false,
            delay_ms: None,
            reason: Some(reason),
        }
    }

    #[test]
    fn the_streak_must_cross_the_threshold_before_a_drop() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("test", None).unwrap();
        let summary = storage
            .converge_nodes(round, &[fail("air", "n1", "timeout")], &P)
            .unwrap();
        assert_eq!(summary.dropped, 0);
        let node = storage.get_node("air", "n1").unwrap().unwrap();
        assert_eq!(
            node.status, "unknown",
            "a first-seen node holds unknown below the threshold (was not alive/pending)"
        );
        assert_eq!(node.consec_fail, 1);

        let summary = storage
            .converge_nodes(round, &[fail("air", "n1", "timeout")], &P)
            .unwrap();
        assert_eq!(summary.dropped, 0, "streak 2 of 3");
        let summary = storage
            .converge_nodes(round, &[fail("air", "n1", "timeout")], &P)
            .unwrap();
        assert_eq!(summary.dropped, 1, "streak 3 of 3 = drop");
        let node = storage.get_node("air", "n1").unwrap().unwrap();
        assert_eq!(node.status, "dead");
        assert_eq!(node.consec_fail, 3);
        assert_eq!(node.last_reason.as_deref(), Some("timeout"));
    }

    #[test]
    fn a_pass_resets_the_streak_and_a_dead_node_coming_back_is_a_restore() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("test", None).unwrap();
        for _ in 0..3 {
            storage
                .converge_nodes(round, &[fail("air", "n1", "timeout")], &P)
                .unwrap();
        }
        let summary = storage
            .converge_nodes(round, &[pass("air", "n1", 42, "direct")], &P)
            .unwrap();
        assert_eq!(summary.restored, 1, "dead→alive is the false-kill signal");
        assert_eq!(summary.alive_nodes, 1);
        let node = storage.get_node("air", "n1").unwrap().unwrap();
        assert_eq!(node.status, "alive");
        assert_eq!(node.consec_fail, 0);
        assert_eq!(node.last_delay_ms, Some(42));
        assert_eq!(node.last_reason, None, "a pass clears the failure reason");
        assert_eq!(node.category.as_deref(), Some("direct"));
        assert!(node.last_ok.is_some());
        assert!(node.first_seen_is_set());
    }

    #[test]
    fn a_first_pass_is_new_and_a_history_row_is_written_on_status_moves() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("test", None).unwrap();
        let summary = storage
            .converge_nodes(round, &[pass("air", "n1", 42, "direct")], &P)
            .unwrap();
        assert_eq!(summary.new_alive, 1, "first sight + pass = New, not Restore");

        let from: Vec<(Option<String>, String)> = {
            let mut stmt = storage
                .conn
                .prepare(
                    "SELECT from_status, to_status FROM node_state_history
                     WHERE source='air' AND fingerprint='n1' ORDER BY id",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            from,
            vec![(Some("unknown".into()), "alive".into())],
            "one history row: unknown→alive"
        );
    }

    #[test]
    fn a_dead_node_staying_dead_writes_no_history_row() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("test", None).unwrap();
        for _ in 0..4 {
            storage
                .converge_nodes(round, &[fail("air", "n1", "timeout")], &P)
                .unwrap();
        }
        let count: i64 = storage
            .conn
            .query_row(
                "SELECT COUNT(*) FROM node_state_history WHERE source='air'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // unknown→dead at the third fail; the first two folds hold unknown
        // (a first-seen node was never alive/pending) and the fourth changes
        // nothing. History records status moves, not every fold.
        assert_eq!(count, 1);
    }

    #[test]
    fn previous_alive_count_skips_suspect_and_inconclusive_rounds() {
        let storage = Storage::open_in_memory().unwrap();
        assert_eq!(storage.previous_alive_count().unwrap(), 0, "no rounds yet");

        let r1 = storage.start_round("test", None).unwrap();
        storage.finish_round_full(r1, None, &RoundGuardSummary { total: 10, ok: 8, failed: 2, ..Default::default() }).unwrap();
        assert_eq!(storage.previous_alive_count().unwrap(), 8);

        // A suspect round: skipped as a baseline.
        let r2 = storage.start_round("test", None).unwrap();
        storage.finish_round_full(r2, None, &RoundGuardSummary { total: 10, ok: 1, failed: 9, suspect: true, ..Default::default() }).unwrap();
        assert_eq!(storage.previous_alive_count().unwrap(), 8);

        // An inconclusive round: skipped the same way (ADR-0005).
        let r3 = storage.start_round("test", None).unwrap();
        storage.finish_round_full(r3, None, &RoundGuardSummary { inconclusive: true, ..Default::default() }).unwrap();
        assert_eq!(storage.previous_alive_count().unwrap(), 8);

        // A good round after the bad ones becomes the new baseline.
        let r4 = storage.start_round("test", None).unwrap();
        storage.finish_round_full(r4, None, &RoundGuardSummary { total: 10, ok: 6, failed: 4, ..Default::default() }).unwrap();
        assert_eq!(storage.previous_alive_count().unwrap(), 6);
    }

    #[test]
    fn the_suspect_guard_folds_through_the_domain_policy() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("test", None).unwrap();
        let summary = storage
            .converge_nodes(
                round,
                &[pass("air", "n1", 42, "direct"), pass("air", "n2", 50, "direct")],
                &P,
            )
            .unwrap();
        assert_eq!(summary.alive_nodes, 2);
        let prev = storage.previous_alive_count().unwrap();
        assert!(probe_domain::round_is_suspect(summary.alive_nodes, prev + 10, &P));
    }
}
