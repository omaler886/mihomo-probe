//! SQL migration runner (GLM_5.3_Flash §11): versioned SQL files applied in
//! order, recorded in `schema_migrations`, additive over the Python ledger.
//!
//! The migration files live in `migrations/` at the repository root and are
//! compiled in with `include_str!` -- a deployment cannot run a Rust binary
//! whose migrations silently drifted from the ones that were tested. Python's
//! in-code `PRAGMA`+`ALTER` migrations (db.py) remain the writer for its own
//! columns; both systems are idempotent over the same file, and neither drops
//! data (the one historical DROP in db.py predates this runner and carries a
//! backup of its own).

use rusqlite::Connection;

use crate::{utc_now, DomainResult};

pub const LATEST_VERSION: i64 = 2;

/// (version, name, sql). Versions are strictly increasing and append-only:
/// an applied migration is never edited, corrections come as new files.
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "0001_python_compat",
        include_str!("../../../migrations/0001_python_compat.sql"),
    ),
    (
        2,
        "0002_probe_ledger",
        include_str!("../../../migrations/0002_probe_ledger.sql"),
    ),
];

/// Tables whose presence `db verify` requires after a full apply: the Python
/// compat core plus the Rust extensions.
pub const REQUIRED_TABLES: &[&str] = &[
    "nodes",
    "rounds",
    "results",
    "events",
    "ip_geo",
    "domain_views",
    "node_state_history",
    "export_snapshots",
    "config_audit",
    "security_audit",
];

fn ensure_registry(conn: &Connection) -> DomainResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
           version INTEGER PRIMARY KEY,
           name TEXT NOT NULL,
           applied_at TEXT NOT NULL
         );",
    )
    .map_err(|e| probe_domain::DomainError::Storage(format!("registry: {e}")))
}

pub fn applied(conn: &Connection) -> DomainResult<Vec<(i64, String, String)>> {
    // Read-only by contract: `db check` calls this on databases it must not
    // mutate, so an absent registry simply means "nothing applied yet". The
    // registry is created by `apply`, never here.
    use rusqlite::OptionalExtension;
    let registered = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |_| Ok(true),
        )
        .optional()
        .map_err(|e| probe_domain::DomainError::Storage(e.to_string()))?
        .unwrap_or(false);
    if !registered {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare("SELECT version, name, applied_at FROM schema_migrations ORDER BY version")
        .map_err(|e| probe_domain::DomainError::Storage(e.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| probe_domain::DomainError::Storage(e.to_string()))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| probe_domain::DomainError::Storage(e.to_string()))?);
    }
    Ok(out)
}

/// Apply every pending migration inside its own transaction; returns the
/// versions applied by this call (empty when the database was current).
pub fn apply(conn: &mut Connection) -> DomainResult<Vec<i64>> {
    ensure_registry(conn)?;
    let done: Vec<i64> = applied(conn)?.into_iter().map(|(v, _, _)| v).collect();
    let mut applied_now = Vec::new();
    for (version, name, sql) in MIGRATIONS {
        if done.contains(version) {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|e| probe_domain::DomainError::Storage(e.to_string()))?;
        tx.execute_batch(sql)
            .map_err(|e| probe_domain::DomainError::Storage(format!("{name}: {e}")))?;
        tx.execute(
            "INSERT INTO schema_migrations(version, name, applied_at) VALUES(?1, ?2, ?3)",
            rusqlite::params![version, name, utc_now()],
        )
        .map_err(|e| probe_domain::DomainError::Storage(e.to_string()))?;
        tx.commit()
            .map_err(|e| probe_domain::DomainError::Storage(e.to_string()))?;
        applied_now.push(*version);
    }
    Ok(applied_now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_versioned_in_order_up_to_latest() {
        let mut previous = 0;
        for (version, name, sql) in MIGRATIONS {
            assert!(*version == previous + 1, "gap at {name}");
            assert!(!sql.trim().is_empty());
            previous = *version;
        }
        assert_eq!(previous, LATEST_VERSION);
    }

    #[test]
    fn apply_records_all_versions_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        let first = apply(&mut conn).unwrap();
        assert_eq!(first, vec![1, 2]);
        let second = apply(&mut conn).unwrap();
        assert!(second.is_empty(), "re-open must not re-apply");
        let done = applied(&conn).unwrap();
        assert_eq!(done.len(), 2);
    }
}
