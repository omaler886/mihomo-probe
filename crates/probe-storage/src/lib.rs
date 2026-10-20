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

pub mod cache;
pub mod migrations;
pub mod publication;

pub use cache::{DnsCacheRecord, GeoCacheRecord};

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

/// Upper bound for every list read in this crate, mirroring
/// `publication::MAX_EXPORT_PAGE`. A caller that asks for more is refused
/// rather than silently clamped: the API layer decides what to tell its client.
pub const MAX_READ_LIMIT: usize = 100;

/// Headline aggregates for the Prometheus surface: counts only, so no label
/// can grow with the ledger's cardinality.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    /// Rounds closed with a verdict (`finished_at IS NOT NULL`).
    pub rounds_finished: i64,
    /// Finished rounds the alive-floor guard flagged.
    pub rounds_suspect: i64,
    /// Finished rounds marked inconclusive (partial coverage or a failed commit).
    pub rounds_inconclusive: i64,
    /// Finished rounds that recorded a wall-clock duration.
    pub rounds_with_duration: i64,
    /// `(status, rows)` pairs, ordered by status.
    pub nodes_by_status: Vec<(String, i64)>,
    /// Keys with an active export snapshot.
    pub active_exports: i64,
}

/// One operational event: the Rust counterpart of Python's `events` table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventRow {
    pub id: i64,
    pub ts: String,
    pub level: String,
    pub message: String,
}

/// Longest message an event keeps: a ledger row is not a transcript.
pub const MAX_EVENT_CHARS: usize = 500;
/// Rows the event log keeps, newest first (Python `db.log` prunes to 2000).
pub const MAX_EVENT_ROWS: i64 = 2000;

/// One throughput sample (G07): bytes actually moved and how long it took.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QualitySampleRow {
    pub source: String,
    pub fingerprint: String,
    pub ts: String,
    pub bytes: i64,
    pub elapsed_ms: f64,
}

/// One row of the per-source aggregate the status document reports.
///
/// Counts only: no node name, no address, nothing that could identify a
/// probe. One row per source that owns at least one node row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceSummaryRow {
    pub source: String,
    pub total: i64,
    pub alive: i64,
    pub dead: i64,
}

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
    pub endpoint_ip: Option<String>,
    pub original_server: Option<String>,
    pub protocol: Option<String>,
    pub dns_incomplete: bool,
    pub exit_ip: Option<String>,
    pub exit_asn: Option<String>,
    pub verification_error: Option<String>,
    pub verification_bytes: i64,
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
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub ip_alive: Option<i64>,
    pub ip_total: Option<i64>,
    pub exit_ip: Option<String>,
    pub exit_asn: Option<String>,
}

impl NodeRow {
    /// The row was persisted with the bookkeeping columns stamped.
    #[cfg(test)]
    fn first_seen_is_set(&self) -> bool {
        self.first_seen.is_some()
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

/// One `node_state_history` row (migration 0002): a recorded status move.
///
/// Only moves are stored -- a fold that leaves the status unchanged writes
/// nothing -- so a node's trail is short and every row is a real transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeHistoryRow {
    pub id: i64,
    pub ts: String,
    pub source: String,
    pub fingerprint: String,
    /// The status the node moved away from; NULL for rows written before the
    /// transition source was recorded.
    pub from_status: Option<String>,
    pub to_status: String,
    pub reason: Option<String>,
    /// The round that observed the move; NULL when no round was involved.
    pub round_id: Option<i64>,
}

/// One converged node handed to [`Storage::converge_nodes`]: the per-node
/// outcome already aggregated from the round's per-variant verdicts (Python
/// `_score_bucket`, `domain_pass = "any"`: one address alive = alive, the
/// best delay of the living ones).
///
/// Metadata is the original domain form, not a temporary kernel alias/address.
/// Missing optional metadata retains the previously stored value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    pub is_excluded: bool,
    pub proto: Option<&'a str>,
    pub server: Option<&'a str>,
    pub ip_alive: Option<i64>,
    pub ip_total: Option<i64>,
    pub country: Option<&'a str>,
    pub exit_ip: Option<&'a str>,
    pub exit_asn: Option<&'a str>,
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

/// One `rounds` row as the read surface exposes it.
///
/// The counters and the two guard flags are coalesced on read: `inconclusive`
/// arrived with migration 0003, every other counter is only `DEFAULT 0`, and a
/// ledger written before those columns existed can still hold NULL there. Only
/// the genuinely optional columns are `Option`; a missing count reads as 0 and
/// a missing flag as false, which is what the Python reader assumes too.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundRow {
    pub round_id: RoundId,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub trigger: Option<String>,
    pub mode: Option<String>,
    pub note: Option<String>,
    pub total: i64,
    pub ok: i64,
    pub failed: i64,
    pub dropped: i64,
    pub restored: i64,
    pub suspect: bool,
    pub inconclusive: bool,
    pub duration_s: Option<f64>,
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
    /// IMMEDIATE (not DEFERRED): these paths read before writing (`get_node`
    /// in convergence, the shadow co-writer's rows in results), and a
    /// DEFERRED transaction that upgrades its WAL read snapshot to a write
    /// fails **immediately** with SQLITE_BUSY_SNAPSHOT when another process
    /// -- the Python service this ledger is shared with by design -- commits
    /// in between, bypassing `busy_timeout` and discarding the whole round's
    /// measurements. IMMEDIATE takes the write lock up front, so the timeout
    /// applies. (`&self` + `new_unchecked` is what the caller's `Mutex`
    /// makes safe: no second statement can interleave on this connection.)
    pub fn record_results(&self, round_id: RoundId, rows: &[ResultRow]) -> DomainResult<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let tx = self.immediate_transaction()?;
        let written = self.insert_results(round_id, rows)?;
        tx.commit()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(written)
    }

    /// Insert result rows inside the caller's transaction; return the inserted count.
    fn insert_results(&self, round_id: RoundId, rows: &[ResultRow]) -> DomainResult<usize> {
        {
            let mut stmt = self
                .conn
                .prepare(
                    "INSERT INTO results(round_id, source, fingerprint, display, verdict,
                     delay_ms, reason, country, attempts, detail, category,
                     endpoint_ip, original_server, protocol, dns_incomplete,
                     exit_ip, exit_asn, verification_error, verification_bytes)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
                )
                .map_err(|e| DomainError::Storage(e.to_string()))?;
            let mut history = self
                .conn
                .prepare(
                    "INSERT INTO exit_history(round_id, source, fingerprint, endpoint_ip,
                 exit_ip, country, asn, observed_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                )
                .map_err(|error| DomainError::Storage(error.to_string()))?;
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
                    row.endpoint_ip,
                    row.original_server,
                    row.protocol,
                    row.dns_incomplete,
                    row.exit_ip,
                    row.exit_asn,
                    row.verification_error,
                    row.verification_bytes,
                ])
                .map_err(|e| DomainError::Storage(e.to_string()))?;
                if let Some(exit_ip) = &row.exit_ip {
                    history
                        .execute(rusqlite::params![
                            round_id,
                            row.source,
                            row.fingerprint,
                            row.endpoint_ip,
                            exit_ip,
                            row.country,
                            row.exit_asn,
                            utc_now(),
                        ])
                        .map_err(|error| DomainError::Storage(error.to_string()))?;
                }
            }
        }
        Ok(rows.len())
    }

    /// Commit raw results and their per-node fold together, returning write statistics.
    ///
    /// A failure in either table (or transition history) rolls back the entire batch.
    /// Inputs must describe the same completed round; cancellation is checked before
    /// entering this synchronous transaction, which is the round's commit boundary.
    pub fn commit_measurements(
        &self,
        round_id: RoundId,
        rows: &[ResultRow],
        nodes: &[ConvergeNode],
        policy: &probe_domain::Policy,
    ) -> DomainResult<(usize, ConvergenceSummary)> {
        if rows.is_empty() && nodes.is_empty() {
            return Ok((0, ConvergenceSummary::default()));
        }
        // IMMEDIATE for the same reason as `record_results`: the convergence
        // pass reads (`get_node`) before it writes, and the Python co-writer
        // can commit in between, which would fail a DEFERRED upgrade with
        // SQLITE_BUSY_SNAPSHOT and throw away a fully-measured round.
        let tx = self.immediate_transaction()?;
        let summary = self.apply_convergence(round_id, nodes, policy)?;
        let written = self.insert_results(round_id, rows)?;
        tx.commit()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok((written, summary))
    }

    /// Result rows for one round, newest round first -- the read side of
    /// `record_results`, and what the shadow comparison diffs.
    pub fn results_for_round(&self, round_id: RoundId) -> DomainResult<Vec<ResultRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, fingerprint, display, verdict, delay_ms, reason,
                 country, attempts, detail, category, endpoint_ip, original_server,
                 protocol, dns_incomplete, exit_ip, exit_asn, verification_error, verification_bytes
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
                    endpoint_ip: row.get(10)?,
                    original_server: row.get(11)?,
                    protocol: row.get(12)?,
                    dns_incomplete: row.get(13)?,
                    exit_ip: row.get(14)?,
                    exit_asn: row.get(15)?,
                    verification_error: row.get(16)?,
                    verification_bytes: row.get(17)?,
                })
            })
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| DomainError::Storage(e.to_string()))?);
        }
        Ok(out)
    }

    /// Record this round's throughput samples, skipping exact duplicates.
    ///
    /// The primary key is (round_id, source, fingerprint), so a retried write
    /// is a no-op rather than an error -- the round may only sample each node
    /// once, and re-running the same round id must not multiply rows.
    pub fn record_quality_samples(
        &self,
        round_id: RoundId,
        rows: &[QualitySampleRow],
    ) -> DomainResult<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let tx = self.immediate_transaction()?;
        let mut written = 0;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR IGNORE INTO quality_samples
                     (round_id, source, fingerprint, ts, bytes, elapsed_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(|e| DomainError::Storage(e.to_string()))?;
            // The timestamp is the write time, not the caller's: a sample is
            // recorded when it is recorded, and letting the caller supply it
            // would only invite a stale one.
            let ts = utc_now();
            for row in rows {
                written += stmt
                    .execute(rusqlite::params![
                        round_id,
                        row.source,
                        row.fingerprint,
                        ts,
                        row.bytes,
                        row.elapsed_ms
                    ])
                    .map_err(|e| DomainError::Storage(e.to_string()))?;
            }
        }
        tx.commit()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(written)
    }

    /// Append one operational event, keeping the newest [`MAX_EVENT_ROWS`]
    /// rows -- the same prune Python `db.log` performs.
    ///
    /// The message is truncated on a character boundary: an event carries a
    /// summary, not a transcript.
    pub fn record_event(&self, level: &str, message: &str) -> DomainResult<()> {
        let level: String = level.chars().take(16).collect();
        let message: String = message.chars().take(MAX_EVENT_CHARS).collect();
        self.conn
            .execute(
                "INSERT INTO events(ts, level, message) VALUES(?1, ?2, ?3)",
                rusqlite::params![utc_now(), level, message],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        self.conn
            .execute(
                "DELETE FROM events WHERE id NOT IN (SELECT id FROM events ORDER BY id DESC LIMIT ?1)",
                rusqlite::params![MAX_EVENT_ROWS],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Newest events first, at most `limit`.
    pub fn recent_events(&self, limit: usize) -> DomainResult<Vec<EventRow>> {
        validate_read_limit(limit)?;
        let mut statement = self
            .conn
            .prepare("SELECT id, ts, level, message FROM events ORDER BY id DESC LIMIT ?1")
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![limit as i64], |row| {
                Ok(EventRow {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    level: row.get(2)?,
                    message: row.get(3)?,
                })
            })
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    /// Every node's stored status, keyed by (source, fingerprint).
    ///
    /// One unpaged query: the round only needs to know which nodes have never
    /// been measured so it can run those first.
    pub fn node_statuses(
        &self,
    ) -> DomainResult<std::collections::HashMap<(String, String), String>> {
        let mut statement = self
            .conn
            .prepare("SELECT source, fingerprint, status FROM nodes")
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let mut statuses = std::collections::HashMap::new();
        for row in rows {
            let (source, fingerprint, status) =
                row.map_err(|e| DomainError::Storage(e.to_string()))?;
            statuses.insert(
                (source, fingerprint),
                status.unwrap_or_else(|| "unknown".into()),
            );
        }
        Ok(statuses)
    }

    /// Newest throughput samples for one node, newest first.
    pub fn recent_quality(
        &self,
        source: &str,
        fingerprint: &str,
        limit: usize,
    ) -> DomainResult<Vec<QualitySampleRow>> {
        validate_read_limit(limit)?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, fingerprint, ts, bytes, elapsed_ms FROM quality_samples
                  WHERE source = ?1 AND fingerprint = ?2
                  ORDER BY round_id DESC LIMIT ?3",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![source, fingerprint, limit as i64],
                |row| {
                    Ok(QualitySampleRow {
                        source: row.get(0)?,
                        fingerprint: row.get(1)?,
                        ts: row.get(2)?,
                        bytes: row.get(3)?,
                        elapsed_ms: row.get(4)?,
                    })
                },
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    /// Newest result rows for one node, newest first: the window a quality
    /// summary is computed from.
    ///
    /// `limit` must be 1..=MAX_READ_LIMIT, like every other read page. An
    /// unknown node is an empty window, not an error -- the node row itself
    /// is what decides whether the node exists.
    pub fn recent_results(
        &self,
        source: &str,
        fingerprint: &str,
        limit: usize,
    ) -> DomainResult<Vec<ResultRow>> {
        validate_read_limit(limit)?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, fingerprint, display, verdict, delay_ms, reason,
                 country, attempts, detail, category, endpoint_ip, original_server,
                 protocol, dns_incomplete, exit_ip, exit_asn, verification_error, verification_bytes
                 FROM results WHERE source = ?1 AND fingerprint = ?2
                 ORDER BY round_id DESC LIMIT ?3",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![source, fingerprint, limit as i64],
                |row| {
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
                        endpoint_ip: row.get(10)?,
                        original_server: row.get(11)?,
                        protocol: row.get(12)?,
                        dns_incomplete: row.get(13)?,
                        exit_ip: row.get(14)?,
                        exit_asn: row.get(15)?,
                        verification_error: row.get(16)?,
                        verification_bytes: row.get(17)?,
                    })
                },
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    /// Read one node row (Python `db.get_node`).
    pub fn get_node(&self, source: &str, fingerprint: &str) -> DomainResult<Option<NodeRow>> {
        use rusqlite::OptionalExtension;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, fingerprint, display, proto, server, status, consec_fail,
                 total_ok, total_fail, last_delay_ms, last_reason, last_ok, country, category,
                 first_seen, last_seen, ip_alive, ip_total, exit_ip, exit_asn
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
                first_seen: row.get(14)?,
                last_seen: row.get(15)?,
                ip_alive: row.get(16)?,
                ip_total: row.get(17)?,
                exit_ip: row.get(18)?,
                exit_asn: row.get(19)?,
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
    /// restored / new) must describe one atomic point in the ledger. IMMEDIATE
    /// for the same reason as [`Storage::commit_measurements`].
    pub fn converge_nodes(
        &self,
        round_id: RoundId,
        nodes: &[ConvergeNode],
        policy: &probe_domain::Policy,
    ) -> DomainResult<ConvergenceSummary> {
        if nodes.is_empty() {
            return Ok(ConvergenceSummary::default());
        }
        let tx = self.immediate_transaction()?;
        let summary = self.apply_convergence(round_id, nodes, policy)?;
        tx.commit()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(summary)
    }

    /// An IMMEDIATE write transaction on this connection: takes the write
    /// lock up front so the connection's `busy_timeout` (not the
    /// snapshot-upgrade dead end) governs contention with the co-writer.
    fn immediate_transaction(&self) -> DomainResult<rusqlite::Transaction<'_>> {
        rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| DomainError::Storage(e.to_string()))
    }

    /// Apply node observations/history in the caller's transaction; return transitions.
    fn apply_convergence(
        &self,
        round_id: RoundId,
        nodes: &[ConvergeNode],
        policy: &probe_domain::Policy,
    ) -> DomainResult<ConvergenceSummary> {
        let mut summary = ConvergenceSummary::default();
        let now = utc_now();
        {
            let mut insert = self
                .conn
                .prepare(
                    "INSERT INTO nodes(source, fingerprint, display, first_seen, last_seen)
                     VALUES(?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|e| DomainError::Storage(e.to_string()))?;
            let mut update = self
                .conn
                .prepare(
                    "UPDATE nodes SET last_seen = ?1, last_ok = ?2, last_reason = ?3,
                     last_delay_ms = ?4, status = ?5, consec_fail = ?6,
                     total_ok = ?7, total_fail = ?8, category = ?9, display = ?10,
                     proto = COALESCE(?13, proto), server = COALESCE(?14, server),
                     ip_alive = COALESCE(?15, ip_alive), ip_total = COALESCE(?16, ip_total),
                     country = COALESCE(?17, country), exit_ip = COALESCE(?18, exit_ip),
                     exit_asn = COALESCE(?19, exit_asn)
                     WHERE source = ?11 AND fingerprint = ?12",
                )
                .map_err(|e| DomainError::Storage(e.to_string()))?;
            let mut history = self
                .conn
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
                    is_excluded: node.is_excluded,
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
                let category = node.category.map(str::to_string).or(existing.category);
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
                        node.proto,
                        node.server,
                        node.ip_alive,
                        node.ip_total,
                        node.country,
                        node.exit_ip,
                        node.exit_asn,
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
                if node.ok && !node.is_excluded {
                    summary.alive_nodes += 1;
                }
            }
        }
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

    /// [`Storage::finish_round_with_counts`] plus the round's measured
    /// duration in seconds (Python `db.finish_round(..., duration_s=...)`).
    ///
    /// Additive: the untimed entries keep their behaviour, so a round they
    /// close still holds a NULL `duration_s`. A caller that measured nothing
    /// -- an orphan reaped at start-up, an aborted round -- writes `0.0`, which
    /// is what Python writes there.
    pub fn finish_round_timed(
        &self,
        round_id: RoundId,
        note: Option<&str>,
        total: i64,
        ok: i64,
        failed: i64,
        duration_s: f64,
    ) -> DomainResult<()> {
        self.conn
            .execute(
                "UPDATE rounds SET finished_at = ?1, note = COALESCE(?2, note),
                 total = ?3, ok = ?4, failed = ?5, duration_s = ?6 WHERE id = ?7",
                rusqlite::params![utc_now(), note, total, ok, failed, duration_s, round_id],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(())
    }

    /// [`Storage::finish_round_full`] plus the round's measured duration.
    ///
    /// The duration rides the same UPDATE as the close -- there is no second
    /// statement that could leave a closed round untimed -- and it adds no new
    /// failure path for the caller: the value belongs to the caller, and a
    /// broken measurement must never turn a close into an error.
    pub fn finish_round_full_timed(
        &self,
        round_id: RoundId,
        note: Option<&str>,
        summary: &RoundGuardSummary,
        duration_s: f64,
    ) -> DomainResult<()> {
        self.conn
            .execute(
                "UPDATE rounds SET finished_at = ?1, note = COALESCE(?2, note),
                 total = ?3, ok = ?4, failed = ?5, dropped = ?6, restored = ?7,
                 suspect = ?8, inconclusive = ?9, duration_s = ?10 WHERE id = ?11",
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
                    duration_s,
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

    /// The same open rounds with their stored `started_at`, oldest first.
    ///
    /// The orphan reaper needs the timestamp to decide whether a round has
    /// been open too long; it is the UTC text the row holds
    /// (`YYYY-MM-DDTHH:MM:SS`), not a parsed instant, so comparing it to
    /// "now" stays the caller's decision. Read-only, like `open_round_ids`.
    pub fn open_rounds(&self) -> DomainResult<Vec<(RoundId, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, started_at FROM rounds WHERE finished_at IS NULL ORDER BY id")
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    // --- Read-only list surface for the API ---
    //
    // Keyset pagination only: a page is "the rows after this cursor value",
    // never an OFFSET. A round or node written between two calls therefore
    // cannot make a later page skip or repeat a row, which is the paging
    // contract the API exposes. Every reader is bounded by MAX_READ_LIMIT and
    // refuses an out-of-range request instead of clamping it. Nothing here
    // writes: the readers reuse the write paths' column order and decoding so
    // a row reads back exactly as the ledger stored it.

    /// List nodes in `(source, fingerprint)` order, starting after `after`.
    ///
    /// `after` is the last row of the previous page. `status` and `source`
    /// filter before paging, so a cursor must be reused with the same filters.
    /// `limit` must be 1..=MAX_READ_LIMIT.
    pub fn list_nodes(
        &self,
        limit: usize,
        after: Option<(&str, &str)>,
        status: Option<&str>,
        source: Option<&str>,
    ) -> DomainResult<Vec<NodeRow>> {
        validate_read_limit(limit)?;
        let (cursor_source, cursor_fingerprint) = match after {
            Some((source, fingerprint)) => (Some(source), Some(fingerprint)),
            None => (None, None),
        };
        // `(source, fingerprint) > (?, ?)` is a row-value comparison against
        // the primary key, so the cursor is ordered by exactly the key the page
        // is ordered by. A NULL cursor turns the predicate off via the guard.
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {NODE_COLUMNS} FROM nodes
                 WHERE (?1 IS NULL OR status = ?1)
                   AND (?2 IS NULL OR source = ?2)
                   AND (?3 IS NULL OR (source, fingerprint) > (?3, ?4))
                 ORDER BY source, fingerprint LIMIT ?5"
            ))
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    status,
                    source,
                    cursor_source,
                    cursor_fingerprint,
                    limit as i64
                ],
                node_from_row,
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    /// Per-source node counts, one row per source that owns a node row.
    ///
    /// An upstream that is configured but has never yielded a node does not
    /// appear here; it is visible through the round note instead of a phantom
    /// zero row.
    pub fn source_summary(&self) -> DomainResult<Vec<SourceSummaryRow>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT source, COUNT(*) AS total,
                        COALESCE(SUM(CASE WHEN status = 'alive' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN status = 'dead' THEN 1 ELSE 0 END), 0)
                   FROM nodes GROUP BY source ORDER BY source",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok(SourceSummaryRow {
                    source: row.get(0)?,
                    total: row.get(1)?,
                    alive: row.get(2)?,
                    dead: row.get(3)?,
                })
            })
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    /// Keys with an active export snapshot (one indexed count).
    pub fn active_export_count(&self) -> DomainResult<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM active_exports", [], |row| row.get(0))
            .map_err(|e| DomainError::Storage(e.to_string()))
    }

    /// Immutable snapshots currently on the ledger (retention visibility).
    pub fn snapshot_count(&self) -> DomainResult<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM export_snapshots", [], |row| {
                row.get(0)
            })
            .map_err(|e| DomainError::Storage(e.to_string()))
    }

    /// Headline aggregates for `/metrics` (G16): counts only, no per-key labels.
    ///
    /// One row per query so a scrape is three indexed reads, never a scan of the
    /// result history. Statuses are folded through `COALESCE` so a Python-era
    /// row with a NULL status still counts as `unknown`.
    pub fn metrics_snapshot(&self) -> DomainResult<MetricsSnapshot> {
        let (rounds_finished, rounds_suspect, rounds_inconclusive, rounds_with_duration) = self
            .conn
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(COALESCE(suspect, 0)), 0),
                        COALESCE(SUM(COALESCE(inconclusive, 0)), 0),
                        COALESCE(SUM(CASE WHEN duration_s IS NOT NULL THEN 1 ELSE 0 END), 0)
                   FROM rounds WHERE finished_at IS NOT NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let mut statement = self
            .conn
            .prepare(
                "SELECT COALESCE(status, 'unknown'), COUNT(*) FROM nodes
                  GROUP BY 1 ORDER BY 1",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let nodes_by_status = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| DomainError::Storage(e.to_string()))?
            .collect::<Result<Vec<(String, i64)>, _>>()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let active_exports = self
            .conn
            .query_row("SELECT COUNT(*) FROM active_exports", [], |row| row.get(0))
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(MetricsSnapshot {
            rounds_finished,
            rounds_suspect,
            rounds_inconclusive,
            rounds_with_duration,
            nodes_by_status,
            active_exports,
        })
    }

    /// Read one node by fingerprint, or None when no source holds it.
    ///
    /// A fingerprint is unique only inside a source, and one address can appear
    /// in several pools, so this returns the row whose `source` sorts first in
    /// SQLite's default byte-wise (BINARY) order -- the same order
    /// [`Storage::list_nodes`] pages in, so the choice is reproducible from a
    /// listing.
    pub fn find_node_by_fingerprint(&self, fingerprint: &str) -> DomainResult<Option<NodeRow>> {
        use rusqlite::OptionalExtension;
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {NODE_COLUMNS} FROM nodes WHERE fingerprint = ?1
                 ORDER BY source LIMIT 1"
            ))
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        stmt.query_row([fingerprint], node_from_row)
            .optional()
            .map_err(|e| DomainError::Storage(e.to_string()))
    }

    /// The newest `limit` status moves of one node, newest first.
    ///
    /// A node with no recorded move returns an empty page, not an error.
    /// `limit` must be 1..=MAX_READ_LIMIT.
    pub fn node_state_history(
        &self,
        source: &str,
        fingerprint: &str,
        limit: usize,
    ) -> DomainResult<Vec<NodeHistoryRow>> {
        validate_read_limit(limit)?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, ts, source, fingerprint, from_status, to_status, reason, round_id
                 FROM node_state_history WHERE source = ?1 AND fingerprint = ?2
                 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![source, fingerprint, limit as i64],
                history_from_row,
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    /// Rounds newest first, starting below `before_id` when one is given.
    ///
    /// `before_id` is a cursor, not a validated ID: the page is exactly the
    /// rows with a smaller id, so the oldest round's own id (and anything below
    /// it) returns an empty page instead of an error. `limit` must be
    /// 1..=MAX_READ_LIMIT.
    pub fn list_rounds(&self, limit: usize, before_id: Option<i64>) -> DomainResult<Vec<RoundRow>> {
        validate_read_limit(limit)?;
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {ROUND_COLUMNS} FROM rounds
                 WHERE (?1 IS NULL OR id < ?1) ORDER BY id DESC LIMIT ?2"
            ))
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![before_id, limit as i64], round_from_row)
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    /// Read one round by id, or None when that round does not exist.
    pub fn round_by_id(&self, round_id: i64) -> DomainResult<Option<RoundRow>> {
        use rusqlite::OptionalExtension;
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {ROUND_COLUMNS} FROM rounds WHERE id = ?1"))
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        stmt.query_row([round_id], round_from_row)
            .optional()
            .map_err(|e| DomainError::Storage(e.to_string()))
    }

    /// The newest round that actually CLOSED -- distinct from
    /// [`Storage::list_rounds`]/`last_round`, which return the newest row
    /// whatever its state. The metrics surface wants this one: while a round
    /// is in flight its counts are NULL, and coalescing them to zero would
    /// make every scrape mid-round read as "all nodes dead".
    pub fn last_finished_round(&self) -> DomainResult<Option<RoundRow>> {
        use rusqlite::OptionalExtension;
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {ROUND_COLUMNS} FROM rounds
                 WHERE finished_at IS NOT NULL ORDER BY id DESC LIMIT 1"
            ))
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        stmt.query_row([], round_from_row)
            .optional()
            .map_err(|e| DomainError::Storage(e.to_string()))
    }
}

/// The `nodes` columns [`node_from_row`] decodes, in field order.
///
/// The status and the counters are coalesced: an additive migration never
/// rewrites existing rows, so a row written before a `NOT NULL DEFAULT` column
/// existed can still hold NULL where the current schema promises a value.
const NODE_COLUMNS: &str = "source, fingerprint, display, proto, server,
     COALESCE(status, 'unknown'), COALESCE(consec_fail, 0), COALESCE(total_ok, 0),
     COALESCE(total_fail, 0), last_delay_ms, last_reason, last_ok, country,
     category, first_seen, last_seen, ip_alive, ip_total, exit_ip, exit_asn";

/// The `rounds` columns [`round_from_row`] decodes, in field order. Same
/// coalescing rule as [`NODE_COLUMNS`]: `inconclusive` arrived with migration
/// 0003 and every counter is only `DEFAULT 0`, so an old row may hold NULL.
const ROUND_COLUMNS: &str = "id, started_at, finished_at, trigger, mode, note,
     COALESCE(total, 0), COALESCE(ok, 0), COALESCE(failed, 0), COALESCE(dropped, 0),
     COALESCE(restored, 0), COALESCE(suspect, 0), COALESCE(inconclusive, 0), duration_s";

/// Refuse a page size outside 1..=MAX_READ_LIMIT; never clamp it silently.
/// The message is a constant, so it never reflects caller input.
fn validate_read_limit(limit: usize) -> DomainResult<()> {
    if !(1..=MAX_READ_LIMIT).contains(&limit) {
        return Err(DomainError::Storage(format!(
            "read page limit outside 1..={MAX_READ_LIMIT}"
        )));
    }
    Ok(())
}

/// Decode one [`NODE_COLUMNS`] row.
fn node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRow> {
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
        first_seen: row.get(14)?,
        last_seen: row.get(15)?,
        ip_alive: row.get(16)?,
        ip_total: row.get(17)?,
        exit_ip: row.get(18)?,
        exit_asn: row.get(19)?,
    })
}

/// Decode one row of the history query.
fn history_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeHistoryRow> {
    Ok(NodeHistoryRow {
        id: row.get(0)?,
        ts: row.get(1)?,
        source: row.get(2)?,
        fingerprint: row.get(3)?,
        from_status: row.get(4)?,
        to_status: row.get(5)?,
        reason: row.get(6)?,
        round_id: row.get(7)?,
    })
}

/// Decode one [`ROUND_COLUMNS`] row; SQLite stores the guard flags as integers.
fn round_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RoundRow> {
    Ok(RoundRow {
        round_id: row.get(0)?,
        started_at: row.get(1)?,
        finished_at: row.get(2)?,
        trigger: row.get(3)?,
        mode: row.get(4)?,
        note: row.get(5)?,
        total: row.get(6)?,
        ok: row.get(7)?,
        failed: row.get(8)?,
        dropped: row.get(9)?,
        restored: row.get(10)?,
        suspect: row.get::<_, i64>(11)? != 0,
        inconclusive: row.get::<_, i64>(12)? != 0,
        duration_s: row.get(13)?,
    })
}

/// Drain a mapped query, converting row errors into storage errors.
fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> DomainResult<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| DomainError::Storage(e.to_string()))?);
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Throughput samples are keyed per round, so a retried write cannot
    /// duplicate rows, and an empty write is not a transaction.
    #[test]
    fn quality_samples_are_per_round_and_deduplicated() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("manual", None).unwrap();
        let rows = vec![QualitySampleRow {
            source: "air".into(),
            fingerprint: "a1".into(),
            ts: "2026-10-20T00:00:00Z".into(),
            bytes: 4096,
            elapsed_ms: 120.0,
        }];
        assert_eq!(storage.record_quality_samples(round, &rows).unwrap(), 1);
        assert_eq!(
            storage.record_quality_samples(round, &rows).unwrap(),
            0,
            "re-writing the same round is a no-op"
        );
        let read = storage.recent_quality("air", "a1", 10).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].bytes, 4096);
        assert!(storage.recent_quality("air", "a1", 0).is_err());
    }

    /// Events are appended newest-first, truncated, and a bad page is refused.
    #[test]
    fn events_are_appended_newest_first_and_truncated() {
        let storage = Storage::open_in_memory().unwrap();
        storage.record_event("info", "round 1 started").unwrap();
        storage.record_event("warn", "source saki failed").unwrap();
        let rows = storage.recent_events(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].message, "source saki failed", "newest first");
        assert_eq!(rows[0].level, "warn");
        assert!(storage.recent_events(0).is_err(), "limit 0 is refused");
        // A long message is truncated on a character boundary, not rejected.
        storage.record_event("info", &"x".repeat(2000)).unwrap();
        let rows = storage.recent_events(1).unwrap();
        assert_eq!(rows[0].message.chars().count(), MAX_EVENT_CHARS);
    }

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
    fn metrics_snapshot_counts_rounds_nodes_and_exports() {
        let storage = Storage::open_in_memory().unwrap();
        // An open round is not a finished round: the scrape must not count it.
        let _open = storage.start_round("cli", None).unwrap();
        let inconclusive = storage.start_round("schedule", None).unwrap();
        storage
            .finish_round_full_timed(
                inconclusive,
                Some("partial coverage"),
                &RoundGuardSummary {
                    total: 3,
                    ok: 1,
                    failed: 2,
                    inconclusive: true,
                    ..RoundGuardSummary::default()
                },
                12.3,
            )
            .unwrap();
        let suspect = storage.start_round("schedule", None).unwrap();
        storage
            .finish_round_full_timed(
                suspect,
                Some("below floor"),
                &RoundGuardSummary {
                    total: 4,
                    ok: 1,
                    failed: 3,
                    suspect: true,
                    ..RoundGuardSummary::default()
                },
                0.0,
            )
            .unwrap();
        // Two nodes, two statuses; the NULL-status path is covered by the
        // COALESCE and asserted in the scrape test rather than here.
        for (fingerprint, status) in [("aaaa", "alive"), ("bbbb", "dead")] {
            storage
                .conn
                .execute(
                    "INSERT INTO nodes(source, fingerprint, display, status)
                     VALUES('air', ?1, ?1, ?2)",
                    rusqlite::params![fingerprint, status],
                )
                .unwrap();
        }
        let snapshot = storage.metrics_snapshot().unwrap();
        assert_eq!(
            snapshot.rounds_finished, 2,
            "the open round is not finished"
        );
        assert_eq!(snapshot.rounds_suspect, 1);
        assert_eq!(snapshot.rounds_inconclusive, 1);
        assert_eq!(snapshot.rounds_with_duration, 2);
        assert_eq!(
            snapshot.nodes_by_status,
            vec![("alive".to_string(), 1), ("dead".to_string(), 1)]
        );
        assert_eq!(snapshot.active_exports, 0);
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
    fn open_rounds_pair_each_id_with_its_start_time() {
        let storage = Storage::open_in_memory().unwrap();
        assert!(storage.open_rounds().unwrap().is_empty());
        let closed = storage.start_round("cli", None).unwrap();
        storage.finish_round(closed, None).unwrap();
        let first = storage.start_round("schedule", None).unwrap();
        let second = storage.start_round("schedule", None).unwrap();
        let open = storage.open_rounds().unwrap();
        assert_eq!(
            open.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![first, second],
            "oldest first, and a closed round is not open"
        );
        for (id, started_at) in &open {
            let row = storage.round_by_id(*id).unwrap().unwrap();
            assert_eq!(row.started_at, *started_at, "the text the row stores");
            assert_eq!(started_at.len(), 19, "UTC second precision: {started_at}");
        }
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
                endpoint_ip: Some("192.0.2.1".into()),
                original_server: Some("origin.example".into()),
                protocol: Some("vless".into()),
                dns_incomplete: false,
                ..Default::default()
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
                ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    /// A failed observation has no replacement category; retain the last measured one.
    #[test]
    fn a_failure_keeps_the_last_measured_category() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("test", None).unwrap();
        storage
            .converge_nodes(round, &[pass("air", "node", 42, "chain")], &P)
            .unwrap();
        storage
            .converge_nodes(round, &[fail("air", "node", "timeout")], &P)
            .unwrap();
        let node = storage.get_node("air", "node").unwrap().unwrap();
        assert_eq!(node.category.as_deref(), Some("chain"));
        assert_eq!(node.status, "pending");
    }

    /// Inject failures after writes have started; all tables must roll back together.
    #[test]
    fn failed_measurements_roll_back_results_nodes_and_history() {
        for table in ["results", "node_state_history"] {
            let storage = Storage::open_in_memory().unwrap();
            let previous = storage.start_round("test", None).unwrap();
            storage
                .converge_nodes(previous, &[pass("air", "existing", 42, "chain")], &P)
                .unwrap();
            let before = storage.get_node("air", "existing").unwrap();
            let round = storage.start_round("test", None).unwrap();
            let nodes = [
                fail("air", "existing", "timeout"),
                pass("air", "poison", 50, "direct"),
            ];
            let rows: Vec<_> = nodes
                .iter()
                .map(|node| ResultRow {
                    source: node.source.into(),
                    fingerprint: node.fingerprint.into(),
                    verdict: Some(if node.ok { VERDICT_OK } else { VERDICT_FAIL }.into()),
                    ..Default::default()
                })
                .collect();
            storage
                .conn
                .execute_batch(&format!(
                    "CREATE TRIGGER poison BEFORE INSERT ON {table}
                 WHEN NEW.fingerprint = 'poison'
                 BEGIN SELECT RAISE(ABORT, 'poison row'); END;"
                ))
                .unwrap();
            assert!(storage
                .commit_measurements(round, &rows, &nodes, &P)
                .is_err());
            assert_eq!(storage.get_node("air", "existing").unwrap(), before);
            assert!(storage.get_node("air", "poison").unwrap().is_none());
            assert!(storage.results_for_round(round).unwrap().is_empty());
            let history: i64 = storage
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM node_state_history WHERE round_id = ?1",
                    [round],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(history, 0);

            storage.conn.execute_batch("DROP TRIGGER poison").unwrap();
            let (written, summary) = storage
                .commit_measurements(round, &rows, &nodes, &P)
                .unwrap();
            assert_eq!(written, 2);
            assert_eq!(summary.alive_nodes, 1);
            assert_eq!(storage.results_for_round(round).unwrap(), rows);
            assert_eq!(
                storage
                    .get_node("air", "existing")
                    .unwrap()
                    .unwrap()
                    .consec_fail,
                1
            );
        }
    }

    #[test]
    fn a_first_pass_is_new_and_a_history_row_is_written_on_status_moves() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("test", None).unwrap();
        let summary = storage
            .converge_nodes(round, &[pass("air", "n1", 42, "direct")], &P)
            .unwrap();
        assert_eq!(
            summary.new_alive, 1,
            "first sight + pass = New, not Restore"
        );

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
        storage
            .finish_round_full(
                r1,
                None,
                &RoundGuardSummary {
                    total: 10,
                    ok: 8,
                    failed: 2,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(storage.previous_alive_count().unwrap(), 8);

        // A suspect round: skipped as a baseline.
        let r2 = storage.start_round("test", None).unwrap();
        storage
            .finish_round_full(
                r2,
                None,
                &RoundGuardSummary {
                    total: 10,
                    ok: 1,
                    failed: 9,
                    suspect: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(storage.previous_alive_count().unwrap(), 8);

        // An inconclusive round: skipped the same way (ADR-0005).
        let r3 = storage.start_round("test", None).unwrap();
        storage
            .finish_round_full(
                r3,
                None,
                &RoundGuardSummary {
                    inconclusive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(storage.previous_alive_count().unwrap(), 8);

        // A good round after the bad ones becomes the new baseline.
        let r4 = storage.start_round("test", None).unwrap();
        storage
            .finish_round_full(
                r4,
                None,
                &RoundGuardSummary {
                    total: 10,
                    ok: 6,
                    failed: 4,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(storage.previous_alive_count().unwrap(), 6);
    }

    #[test]
    fn the_suspect_guard_folds_through_the_domain_policy() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("test", None).unwrap();
        let summary = storage
            .converge_nodes(
                round,
                &[
                    pass("air", "n1", 42, "direct"),
                    pass("air", "n2", 50, "direct"),
                ],
                &P,
            )
            .unwrap();
        assert_eq!(summary.alive_nodes, 2);
        let prev = storage.previous_alive_count().unwrap();
        assert!(probe_domain::round_is_suspect(
            summary.alive_nodes,
            prev + 10,
            &P
        ));
    }

    // --- Read-only list surface ---

    /// Insert the row shape a Python-era ledger holds: only the primary key is
    /// written, so every column added later (ip_alive, exit_ip, ...) stays NULL.
    fn insert_bare_node(storage: &Storage, source: &str, fingerprint: &str) {
        storage
            .conn
            .execute(
                "INSERT INTO nodes(source, fingerprint) VALUES(?1, ?2)",
                rusqlite::params![source, fingerprint],
            )
            .unwrap();
    }

    fn pairs(nodes: Vec<NodeRow>) -> Vec<(String, String)> {
        nodes
            .into_iter()
            .map(|node| (node.source, node.fingerprint))
            .collect()
    }

    #[test]
    fn node_pages_are_keyset_complete_without_gaps_or_repeats() {
        let storage = Storage::open_in_memory().unwrap();
        for (source, fingerprint) in [
            ("air", "a1"),
            ("air", "a2"),
            ("air", "a3"),
            ("mobile", "m1"),
            ("mobile", "m2"),
        ] {
            insert_bare_node(&storage, source, fingerprint);
        }
        // The upper bound is a legal page: it returns the whole ordered listing.
        let ordered = pairs(
            storage
                .list_nodes(MAX_READ_LIMIT, None, None, None)
                .unwrap(),
        );
        let expected: Vec<(String, String)> = [
            ("air", "a1"),
            ("air", "a2"),
            ("air", "a3"),
            ("mobile", "m1"),
            ("mobile", "m2"),
        ]
        .iter()
        .map(|(source, fingerprint)| (source.to_string(), fingerprint.to_string()))
        .collect();
        assert_eq!(ordered, expected);

        // Walk the listing two rows at a time, feeding back the last row's key.
        let mut seen = Vec::new();
        let mut cursor: Option<(String, String)> = None;
        loop {
            let page = storage
                .list_nodes(
                    2,
                    cursor.as_ref().map(|(s, f)| (s.as_str(), f.as_str())),
                    None,
                    None,
                )
                .unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= 2);
            seen.extend(pairs(page.clone()));
            let last = page.last().unwrap();
            cursor = Some((last.source.clone(), last.fingerprint.clone()));
        }
        assert_eq!(seen, ordered, "keyset pages cover every row exactly once");
        // The final cursor is past the end of the listing.
        let last = seen.last().unwrap();
        assert!(storage
            .list_nodes(2, Some((last.0.as_str(), last.1.as_str())), None, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn node_lists_filter_and_read_legacy_nulls_as_defaults() {
        let storage = Storage::open_in_memory().unwrap();
        assert!(storage.list_nodes(10, None, None, None).unwrap().is_empty());
        let round = storage.start_round("test", None).unwrap();
        storage
            .converge_nodes(round, &[pass("air", "a1", 12, "direct")], &P)
            .unwrap();
        insert_bare_node(&storage, "air", "a2");
        insert_bare_node(&storage, "mobile", "a3");

        let air: Vec<String> = storage
            .list_nodes(10, None, None, Some("air"))
            .unwrap()
            .into_iter()
            .map(|node| node.fingerprint)
            .collect();
        assert_eq!(air, ["a1", "a2"]);
        let alive = storage.list_nodes(10, None, Some("alive"), None).unwrap();
        assert_eq!(alive.len(), 1);
        assert_eq!(alive[0].fingerprint, "a1");
        assert_eq!(alive[0].last_delay_ms, Some(12));
        let unknown = storage.list_nodes(10, None, Some("unknown"), None).unwrap();
        assert_eq!(unknown.len(), 2);
        // An unstamped row keeps the schema defaults and leaves later columns NULL
        // instead of failing the read.
        assert_eq!(unknown[0].status, "unknown");
        assert_eq!(unknown[0].total_ok, 0);
        assert_eq!(unknown[0].consec_fail, 0);
        assert_eq!(unknown[0].first_seen, None);
        assert_eq!(unknown[0].ip_alive, None);
        assert_eq!(unknown[0].ip_total, None);
        assert_eq!(unknown[0].exit_ip, None);
        assert_eq!(unknown[0].exit_asn, None);

        // The cursor and the filters compose: only air, only after (air, a1).
        let page = storage
            .list_nodes(1, Some(("air", "a1")), None, Some("air"))
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].fingerprint, "a2");
    }

    #[test]
    fn find_node_by_fingerprint_selects_the_smallest_source() {
        let storage = Storage::open_in_memory().unwrap();
        insert_bare_node(&storage, "mobile", "shared");
        insert_bare_node(&storage, "air", "shared");
        let found = storage.find_node_by_fingerprint("shared").unwrap().unwrap();
        assert_eq!(found.source, "air", "the smallest source owns the name");
        assert_eq!(found.fingerprint, "shared");
        // The choice is the first row a listing shows for that fingerprint.
        let listed = storage
            .list_nodes(MAX_READ_LIMIT, None, None, None)
            .unwrap()
            .into_iter()
            .find(|node| node.fingerprint == "shared")
            .unwrap();
        assert_eq!(found, listed);
        assert!(storage
            .find_node_by_fingerprint("absent")
            .unwrap()
            .is_none());
        assert!(storage.find_node_by_fingerprint("").unwrap().is_none());
    }

    #[test]
    fn node_history_is_newest_first_bounded_and_per_node() {
        let storage = Storage::open_in_memory().unwrap();
        let round = storage.start_round("test", None).unwrap();
        for _ in 0..3 {
            storage
                .converge_nodes(round, &[fail("air", "n1", "timeout")], &P)
                .unwrap();
        }
        storage
            .converge_nodes(round, &[pass("air", "n1", 42, "direct")], &P)
            .unwrap();
        storage
            .converge_nodes(round, &[fail("air", "other", "timeout")], &P)
            .unwrap();

        let history = storage
            .node_state_history("air", "n1", MAX_READ_LIMIT)
            .unwrap();
        assert_eq!(history.len(), 2, "only status moves are recorded");
        assert!(history[0].id > history[1].id, "newest first");
        assert_eq!(history[0].source, "air");
        assert_eq!(history[0].fingerprint, "n1");
        assert_eq!(history[0].from_status.as_deref(), Some("dead"));
        assert_eq!(history[0].to_status, "alive");
        assert_eq!(history[0].reason, None, "a pass carries no reason");
        assert_eq!(history[0].round_id, Some(round));
        assert!(!history[0].ts.is_empty());
        assert_eq!(history[1].to_status, "dead");
        assert_eq!(history[1].from_status.as_deref(), Some("unknown"));
        assert_eq!(history[1].reason.as_deref(), Some("timeout"));

        let newest = storage.node_state_history("air", "n1", 1).unwrap();
        assert_eq!(newest.len(), 1, "a smaller limit keeps the newest entry");
        assert_eq!(newest[0].id, history[0].id);
        // A node that never moved has no history, and no other node's rows leak.
        assert!(storage
            .node_state_history("air", "other", 10)
            .unwrap()
            .is_empty());
        assert!(storage
            .node_state_history("mobile", "n1", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rounds_page_newest_first_with_before_id_boundaries() {
        let storage = Storage::open_in_memory().unwrap();
        assert!(storage.list_rounds(10, None).unwrap().is_empty());
        assert!(storage.round_by_id(1).unwrap().is_none());
        let mut ids = Vec::new();
        for trigger in ["first", "second", "third", "fourth"] {
            let id = storage.start_round(trigger, Some("chain")).unwrap();
            storage
                .finish_round_full(
                    id,
                    Some("done"),
                    &RoundGuardSummary {
                        total: 4,
                        ok: 3,
                        failed: 1,
                        dropped: 1,
                        restored: 2,
                        suspect: true,
                        inconclusive: false,
                    },
                )
                .unwrap();
            ids.push(id);
        }
        let all = storage.list_rounds(MAX_READ_LIMIT, None).unwrap();
        assert_eq!(
            all.iter().map(|row| row.round_id).collect::<Vec<_>>(),
            [ids[3], ids[2], ids[1], ids[0]]
        );
        // Two keyset pages cover the listing exactly: no gap, no repeat.
        let first = storage.list_rounds(2, None).unwrap();
        let second = storage.list_rounds(2, Some(first[1].round_id)).unwrap();
        assert_eq!(
            first
                .iter()
                .chain(second.iter())
                .map(|row| row.round_id)
                .collect::<Vec<_>>(),
            [ids[3], ids[2], ids[1], ids[0]]
        );
        // Boundaries: the oldest id closes the listing, one above the newest opens it.
        assert!(storage.list_rounds(2, Some(ids[0])).unwrap().is_empty());
        assert_eq!(
            storage
                .list_rounds(MAX_READ_LIMIT, Some(ids[3] + 1))
                .unwrap()
                .len(),
            4
        );
        // What the round writer stored reads back verbatim.
        let row = storage.round_by_id(ids[3]).unwrap().unwrap();
        assert_eq!(row.trigger.as_deref(), Some("fourth"));
        assert_eq!(row.mode.as_deref(), Some("chain"));
        assert_eq!(row.note.as_deref(), Some("done"));
        assert!(row.finished_at.is_some());
        assert_eq!(
            (row.total, row.ok, row.failed, row.dropped, row.restored),
            (4, 3, 1, 1, 2)
        );
        assert!(row.suspect);
        assert!(!row.inconclusive);
        assert_eq!(row.duration_s, None, "nothing writes duration_s yet");
    }

    #[test]
    fn round_rows_read_legacy_nulls_as_documented_defaults() {
        let storage = Storage::open_in_memory().unwrap();
        // A row from a ledger older than the counter and guard columns: every
        // one of them is NULL, and only started_at is set.
        storage
            .conn
            .execute(
                "INSERT INTO rounds(started_at, trigger, total, ok, failed, dropped,
                 restored, suspect, note, duration_s, mode)
                 VALUES('2026-01-01T00:00:00', NULL, NULL, NULL, NULL, NULL,
                 NULL, NULL, NULL, NULL, NULL)",
                [],
            )
            .unwrap();
        let id = storage.conn.last_insert_rowid();
        let row = storage.round_by_id(id).unwrap().unwrap();
        assert_eq!(row.round_id, id);
        assert_eq!(row.started_at, "2026-01-01T00:00:00");
        assert_eq!(row.finished_at, None);
        assert_eq!(row.trigger, None);
        assert_eq!(row.mode, None);
        assert_eq!(row.note, None);
        assert_eq!(
            (row.total, row.ok, row.failed, row.dropped, row.restored),
            (0, 0, 0, 0, 0)
        );
        assert!(!row.suspect);
        assert!(!row.inconclusive);
        assert_eq!(row.duration_s, None);
        assert_eq!(storage.list_rounds(10, None).unwrap(), vec![row.clone()]);
    }

    #[test]
    fn read_limits_outside_the_page_bound_are_errors_not_clamps() {
        let storage = Storage::open_in_memory().unwrap();
        for limit in [0, MAX_READ_LIMIT + 1, usize::MAX] {
            assert!(
                storage.list_nodes(limit, None, None, None).is_err(),
                "nodes accepted {limit}"
            );
            assert!(
                storage.list_rounds(limit, None).is_err(),
                "rounds accepted {limit}"
            );
            assert!(
                storage.node_state_history("air", "n1", limit).is_err(),
                "history accepted {limit}"
            );
        }
        // The boundary itself is a valid page, and an empty ledger is not an error.
        assert!(storage
            .list_nodes(MAX_READ_LIMIT, None, None, None)
            .unwrap()
            .is_empty());
        assert!(storage
            .list_rounds(MAX_READ_LIMIT, None)
            .unwrap()
            .is_empty());
        assert!(storage
            .node_state_history("air", "n1", MAX_READ_LIMIT)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn timed_closes_record_the_duration_and_untimed_ones_keep_it_null() {
        let storage = Storage::open_in_memory().unwrap();
        let timed = storage.start_round("test", Some("chain")).unwrap();
        storage
            .finish_round_timed(timed, Some("counted"), 3, 2, 1, 12.3)
            .unwrap();
        let row = storage.round_by_id(timed).unwrap().unwrap();
        assert_eq!(row.duration_s, Some(12.3));
        assert_eq!((row.total, row.ok, row.failed), (3, 2, 1));
        assert_eq!(row.note.as_deref(), Some("counted"));
        assert!(row.finished_at.is_some());

        let full = storage.start_round("test", Some("chain")).unwrap();
        storage
            .finish_round_full_timed(
                full,
                Some("guarded"),
                &RoundGuardSummary {
                    total: 5,
                    ok: 4,
                    failed: 1,
                    dropped: 2,
                    restored: 1,
                    suspect: true,
                    inconclusive: true,
                },
                0.0,
            )
            .unwrap();
        let row = storage.round_by_id(full).unwrap().unwrap();
        assert_eq!(
            row.duration_s,
            Some(0.0),
            "a reaped or aborted round writes 0, not NULL"
        );
        assert_eq!(
            (row.total, row.ok, row.failed, row.dropped, row.restored),
            (5, 4, 1, 2, 1)
        );
        assert!(row.suspect && row.inconclusive);

        // The untimed entries keep their old behaviour: no duration at all.
        let untimed = storage.start_round("test", None).unwrap();
        storage
            .finish_round_full(untimed, None, &RoundGuardSummary::default())
            .unwrap();
        assert_eq!(
            storage.round_by_id(untimed).unwrap().unwrap().duration_s,
            None
        );
        assert_eq!(storage.list_rounds(10, None).unwrap().len(), 3);
    }

    #[test]
    fn a_broken_measurement_cannot_fail_a_timed_close() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage.start_round("test", None).unwrap();
        // The engine clamps before it reaches the ledger; storage still must
        // not turn an impossible value into a failed close. SQLite stores a
        // NaN REAL as NULL, so the row reads back like a legacy one.
        storage
            .finish_round_timed(id, None, 0, 0, 0, f64::NAN)
            .unwrap();
        let row = storage.round_by_id(id).unwrap().unwrap();
        assert!(row.finished_at.is_some(), "the round is closed regardless");
        assert_eq!(row.duration_s, None);
    }
}
