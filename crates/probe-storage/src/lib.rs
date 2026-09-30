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
        // The applied registry now holds both migrations, without dropping data.
        let applied = migrations::applied(&storage.conn).unwrap();
        assert_eq!(
            applied.iter().map(|(v, _, _)| *v).collect::<Vec<_>>(),
            vec![1, 2]
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
}
