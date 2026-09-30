//! SQLite state for the Rust slice, reading and writing the SAME `rounds`
//! table the Python service uses (`mihomo_test/db.py`).
//!
//! Column names, types and the UTC second-precision timestamp format are
//! shared on purpose: during shadow runs both implementations hit one ledger,
//! and a divergent schema or clock format would make every comparison a
//! manual exercise (workstreams/13). The slice only touches `rounds`; the
//! full migration framework arrives in R2 with real SQL migration files.

use std::path::Path;

use probe_domain::{DomainError, DomainResult, RoundId, RoundSummary};
use rusqlite::Connection;

/// The `rounds` DDL, mirroring db.py's SCHEMA block (a subset is fine; a
/// *conflict* is not). R2 replaces in-code DDL with migration files.
const ROUNDS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS rounds (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  trigger TEXT,
  total INTEGER DEFAULT 0,
  ok INTEGER DEFAULT 0,
  failed INTEGER DEFAULT 0,
  dropped INTEGER DEFAULT 0,
  restored INTEGER DEFAULT 0,
  suspect INTEGER DEFAULT 0,
  note TEXT,
  duration_s REAL,
  mode TEXT
);
"#;

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

    fn init(conn: Connection) -> DomainResult<Self> {
        // WAL + a real busy timeout are correctness here, not tuning: the
        // Python service and this slice may share one ledger file during
        // shadow runs (db.py says the same about its own CLI).
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.busy_timeout(std::time::Duration::from_secs(30));
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        conn.execute_batch(ROUNDS_DDL)
            .map_err(|e| DomainError::Storage(format!("schema: {e}")))?;
        Ok(Self { conn })
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
}
