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

pub const LATEST_VERSION: i64 = 9;

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
    (
        3,
        "0003_r7_round_guard",
        include_str!("../../../migrations/0003_r7_round_guard.sql"),
    ),
    (
        4,
        "0004_dns_cache",
        include_str!("../../../migrations/0004_dns_cache.sql"),
    ),
    (
        5,
        "0005_endpoint_observations",
        include_str!("../../../migrations/0005_endpoint_observations.sql"),
    ),
    (
        6,
        "0006_publication",
        include_str!("../../../migrations/0006_publication.sql"),
    ),
    (
        7,
        "0007_exit_observations",
        include_str!("../../../migrations/0007_exit_observations.sql"),
    ),
    (
        8,
        "0008_results_category",
        include_str!("../../../migrations/0008_results_category.sql"),
    ),
    (
        9,
        "0009_quality_samples",
        include_str!("../../../migrations/0009_quality_samples.sql"),
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
    "dns_cache",
    "active_exports",
    "exit_history",
    "quality_samples",
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
///
/// A recorded version whose `name` no longer matches this binary's registry
/// is a hard error, not a skip: migrations 0004+ were drafted and deployed
/// while uncommitted, and a ledger migrated by a build carrying different
/// bytes under the same version number must fail loudly here -- at startup,
/// naming the version -- rather than surface as "no such column" on every
/// round commit.
pub fn apply(conn: &mut Connection) -> DomainResult<Vec<i64>> {
    ensure_registry(conn)?;
    let done = applied(conn)?;
    for (version, recorded_name, _) in &done {
        let expected = MIGRATIONS
            .iter()
            .find(|(v, _, _)| v == version)
            .map(|(_, name, _)| *name);
        match expected {
            Some(name) if name == recorded_name => {}
            Some(_) => {
                return Err(probe_domain::DomainError::Storage(format!(
                    "migration {version} was applied as a different revision \
                     ({recorded_name:?}); the schema and this binary disagree -- \
                     restore the binary that migrated this ledger or rebuild \
                     the ledger from a backup"
                )));
            }
            None => {
                return Err(probe_domain::DomainError::Storage(format!(
                    "ledger carries migration {version} ({recorded_name:?}), \
                     which this binary does not know; refusing to run against \
                     a schema it cannot read"
                )));
            }
        }
    }
    let done: Vec<i64> = done.into_iter().map(|(v, _, _)| v).collect();
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

    /// Column names in `cid` order. ADD COLUMN appends, so a declared
    /// addition keeps every earlier column in place.
    fn columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        statement
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

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
        assert_eq!(
            first,
            (1..=LATEST_VERSION).collect::<Vec<_>>(),
            "all versions applied, in order"
        );
        let second = apply(&mut conn).unwrap();
        assert!(second.is_empty(), "re-open must not re-apply");
        let done = applied(&conn).unwrap();
        assert_eq!(done.len(), LATEST_VERSION as usize);
    }

    /// Upgrade a version-three ledger; preserve old rows and add only declared metadata.
    #[test]
    fn version_three_ledger_only_gains_the_dns_cache_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_registry(&conn).unwrap();
        for (version, name, sql) in MIGRATIONS.iter().take(3) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version, name, applied_at) VALUES(?1, ?2, ?3)",
                rusqlite::params![version, name, "2023-11-14T22:13:20"],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO ip_geo(ip, country, isp, checked_at)
               VALUES('192.0.2.1', 'US', 'Example ISP', '2023-11-14T22:13:20');
             INSERT INTO rounds(started_at, trigger) VALUES('2023-11-14T22:13:20', 'legacy');
             INSERT INTO results(round_id, source, fingerprint, display, verdict)
               VALUES(1, 'legacy', 'node', 'original node', 'ok');",
        )
        .unwrap();
        let schema: Vec<(String, Option<String>)> = conn
            .prepare("SELECT name, sql FROM sqlite_master ORDER BY name")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            apply(&mut conn).unwrap(),
            (4..=LATEST_VERSION).collect::<Vec<_>>()
        );
        for (name, sql) in schema {
            // 0007 gives nodes its exit identity with ALTER TABLE ADD COLUMN,
            // and SQLite rewrites the stored CREATE TABLE text for that -- the
            // same declared, additive change results gets below. Pin the column
            // list instead of byte identity: an undeclared column still fails,
            // and deleting 0007's two ALTER statements fails here too.
            if name == "nodes" {
                assert_eq!(
                    columns(&conn, "nodes"),
                    [
                        "source",
                        "fingerprint",
                        "display",
                        "proto",
                        "server",
                        "first_seen",
                        "last_seen",
                        "last_ok",
                        "last_delay_ms",
                        "country",
                        "status",
                        "consec_fail",
                        "total_ok",
                        "total_fail",
                        "last_reason",
                        "ip_alive",
                        "ip_total",
                        "category",
                        "exit_ip",
                        "exit_asn",
                    ]
                );
                continue;
            }
            // 0005 and 0007 add columns to results and 0008 rebuilds the table
            // to carry all of them; the declared list is 0008's definition.
            if name == "results" {
                assert_eq!(
                    columns(&conn, "results"),
                    [
                        "round_id",
                        "source",
                        "fingerprint",
                        "display",
                        "verdict",
                        "delay_ms",
                        "reason",
                        "country",
                        "attempts",
                        "detail",
                        "category",
                        "endpoint_ip",
                        "original_server",
                        "protocol",
                        "dns_incomplete",
                        "exit_ip",
                        "exit_asn",
                        "verification_error",
                        "verification_bytes",
                    ]
                );
                continue;
            }
            let after: Option<String> = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name = ?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(after, sql);
        }
        let row: (String, String, String, String) = conn
            .query_row(
                "SELECT ip, country, isp, checked_at FROM ip_geo",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "192.0.2.1".into(),
                "US".into(),
                "Example ISP".into(),
                "2023-11-14T22:13:20".into()
            )
        );
        let result: (String, String, String, Option<String>, i64) = conn
            .query_row(
                "SELECT source, fingerprint, verdict, endpoint_ip, dns_incomplete FROM results",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            result,
            ("legacy".into(), "node".into(), "ok".into(), None, 0)
        );
        assert!(apply(&mut conn).unwrap().is_empty());
    }

    /// A registry row whose name does not match this binary's registry is a
    /// hard error: the deployed ledger was migrated by a build carrying
    /// different bytes under the same version, and silently skipping it is
    /// exactly how "no such column" becomes the round commit's nightly error.
    #[test]
    fn a_registry_row_with_a_foreign_name_fails_loudly() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(!apply(&mut conn).unwrap().is_empty());
        conn.execute(
            "UPDATE schema_migrations SET name = '0009_a_draft_that_shipped' WHERE version = 9",
            [],
        )
        .unwrap();
        let err = apply(&mut conn).unwrap_err().to_string();
        assert!(
            err.contains("different revision") && err.contains("migration 9"),
            "the error must name the version: {err}"
        );
    }

    /// A ledger carrying a version this binary does not know (migrated by a
    /// NEWER build, then rolled back) is refused, not best-effort read.
    #[test]
    fn an_unknown_higher_version_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(!apply(&mut conn).unwrap().is_empty());
        conn.execute(
            "INSERT INTO schema_migrations(version, name, applied_at)
             VALUES(?1, '0010_from_the_future', '2023-11-14T22:13:20')",
            [LATEST_VERSION + 1],
        )
        .unwrap();
        let err = apply(&mut conn).unwrap_err().to_string();
        assert!(
            err.contains("does not know"),
            "the error must say the binary cannot read this schema: {err}"
        );
    }
}
