//! Durable publication of bounded JSON documents (also valid ClashMeta YAML).
//!
//! The active pointer, snapshot insert/reuse, and audit entry commit together.
//! Lists deliberately select metadata only. Content-bearing types implement
//! neither Debug nor Serialize: only authenticated download paths should read
//! them. Existing non-JSON snapshots are preserved, but require an explicit
//! validated import before activation. No automatic history deletion occurs.

use probe_domain::{DomainError, DomainResult};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{utc_now, Storage};

pub const MAX_EXPORT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_EXPORT_NODES: usize = 50_000;
pub const MAX_EXPORT_PAGE: usize = 100;
pub const MAX_EXPORT_KEY_CHARS: usize = 48;

/// Safe list/download metadata; `is_active` is current at read/commit time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotSummary {
    pub id: i64,
    pub key: String,
    pub created_at: String,
    pub node_count: usize,
    pub sha256: String,
    pub is_active: bool,
}

/// Secret-bearing content, intentionally not an API list DTO or Debug value.
#[derive(Clone)]
pub struct ExportSnapshot {
    pub summary: SnapshotSummary,
    pub content: String,
}

/// CAS input: `expected_active = None` means the key must have no active export.
/// `actor` must be a non-secret principal identifier, never a token or URL.
pub struct PublicationInput<'a> {
    pub key: &'a str,
    pub content: &'a str,
    pub sha256: &'a str,
    pub node_count: usize,
    pub actor: &'a str,
    pub forced: bool,
    pub expected_active: Option<i64>,
}

/// Publication metadata only. A reused historical snapshot can still activate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublicationResult {
    pub snapshot: SnapshotSummary,
    pub reused: bool,
    pub changed: bool,
}

/// Validate a source key without reflecting rejected values in errors.
pub fn validate_export_key(key: &str) -> DomainResult<()> {
    let has_unsafe = key.chars().any(|ch| {
        ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    });
    if key.is_empty()
        || key != key.trim()
        || key.starts_with('.')
        || key.ends_with('.')
        || key.chars().count() > MAX_EXPORT_KEY_CHARS
        || has_unsafe
    {
        return Err(invalid("invalid export key"));
    }
    Ok(())
}

/// Validate a non-secret actor label; callers remain responsible for its provenance.
pub fn validate_export_actor(actor: &str) -> DomainResult<()> {
    if actor.is_empty()
        || actor.len() > 64
        || !actor
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.@-".contains(&byte))
    {
        return Err(invalid("invalid export actor"));
    }
    Ok(())
}

/// Compute the lowercase SHA-256 of exact UTF-8 export bytes.
pub fn export_hash(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// Check bounded JSON content, exact hash, and actual nonempty proxy count.
/// Topology/protocol validation belongs to the engine, not the ledger.
pub fn validate_export_content(content: &str, sha256: &str, count: usize) -> DomainResult<()> {
    validate_metadata(sha256, count)?;
    if content.is_empty() || content.len() > MAX_EXPORT_BYTES || content.contains('\0') {
        return Err(invalid("invalid export content size"));
    }
    if export_hash(content) != sha256 {
        return Err(invalid("export hash mismatch"));
    }
    let document: Value =
        serde_json::from_str(content).map_err(|_| invalid("invalid export JSON"))?;
    let proxies = document
        .get("proxies")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("export proxies must be an array"))?;
    if proxies.len() != count || proxies.iter().any(|proxy| !proxy.is_object()) {
        return Err(invalid("export proxy count or dictionary mismatch"));
    }
    Ok(())
}

impl Storage {
    /// List active exports after `after_key`, at most `limit` metadata rows.
    /// `limit` must be 1..=MAX_EXPORT_PAGE; no content column is selected.
    pub fn list_exports(
        &self,
        limit: usize,
        after_key: Option<&str>,
    ) -> DomainResult<Vec<SnapshotSummary>> {
        validate_page(limit)?;
        if let Some(key) = after_key {
            validate_export_key(key)?;
        }
        read_summaries(
            &self.conn,
            "SELECT s.id, s.key, s.created_at, s.node_count, s.sha256, 1
             FROM active_exports a JOIN export_snapshots s ON s.id = a.snapshot_id AND s.key = a.key
             WHERE (?1 IS NULL OR s.key > ?1) ORDER BY s.key LIMIT ?2",
            params![after_key, limit as i64],
        )
    }

    /// Read descending snapshot metadata for `key`, before an optional ID cursor.
    /// History is paged, not pruned: migration and publication preserve old rows.
    pub fn export_history(
        &self,
        key: &str,
        limit: usize,
        before_id: Option<i64>,
    ) -> DomainResult<Vec<SnapshotSummary>> {
        validate_export_key(key)?;
        validate_page(limit)?;
        validate_optional_id(before_id)?;
        read_summaries(
            &self.conn,
            "SELECT s.id, s.key, s.created_at, s.node_count, s.sha256,
               EXISTS(SELECT 1 FROM active_exports a WHERE a.key = s.key AND a.snapshot_id = s.id)
             FROM export_snapshots s WHERE s.key = ?1 AND (?2 IS NULL OR s.id < ?2)
             ORDER BY s.id DESC LIMIT ?3",
            params![key, before_id, limit as i64],
        )
    }

    /// Distinct export keys that hold at least one snapshot, oldest first.
    pub fn export_keys(&self) -> DomainResult<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT key FROM export_snapshots ORDER BY key")
            .map_err(|_| invalid("cannot list export keys"))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| invalid("cannot list export keys"))?;
        let mut keys = Vec::new();
        for row in rows {
            keys.push(row.map_err(|_| invalid("cannot read export key"))?);
        }
        Ok(keys)
    }

    /// Delete superseded snapshots of `key`, keeping the newest `keep` plus
    /// whatever is currently active.
    ///
    /// The active row is excluded twice on purpose: the `active_exports`
    /// triggers refuse to delete it anyway, and naming it here keeps the
    /// statement honest about what it is allowed to touch. Returns the number
    /// of rows removed.
    pub fn prune_export_snapshots(&self, key: &str, keep: usize) -> DomainResult<usize> {
        validate_export_key(key)?;
        if keep == 0 {
            return Err(invalid("export retention must keep at least one snapshot"));
        }
        let deleted = self
            .conn
            .execute(
                "DELETE FROM export_snapshots
                  WHERE key = ?1
                    AND id NOT IN (
                        SELECT id FROM export_snapshots WHERE key = ?1 ORDER BY id DESC LIMIT ?2
                    )
                    AND id NOT IN (SELECT snapshot_id FROM active_exports WHERE key = ?1)",
                params![key, keep as i64],
            )
            .map_err(|_| invalid("cannot prune export snapshots"))?;
        Ok(deleted)
    }

    /// Content hashes still referenced by any snapshot row of `key`.
    ///
    /// The file GC compares these against the files on disk: an immutable file
    /// whose hash is still referenced belongs to a row the ledger can restore,
    /// and deleting it would break rollback.
    pub fn export_content_hashes(&self, key: &str) -> DomainResult<Vec<String>> {
        validate_export_key(key)?;
        let mut statement = self
            .conn
            .prepare("SELECT sha256 FROM export_snapshots WHERE key = ?1")
            .map_err(|_| invalid("cannot list export content hashes"))?;
        let rows = statement
            .query_map(params![key], |row| row.get::<_, String>(0))
            .map_err(|_| invalid("cannot list export content hashes"))?;
        let mut hashes = Vec::new();
        for row in rows {
            hashes.push(row.map_err(|_| invalid("cannot read export content hash"))?);
        }
        Ok(hashes)
    }

    /// Read and validate the authoritative content for `key`, or None if unpublished.
    pub fn current_export(&self, key: &str) -> DomainResult<Option<ExportSnapshot>> {
        validate_export_key(key)?;
        // Keep the pointer and is_active metadata in the same SQLite read view.
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|_| invalid("cannot start current export read"))?;
        let Some(id) = read_active_id(&tx, key)? else {
            return Ok(None);
        };
        let snapshot = read_snapshot(&tx, key, id)?
            .ok_or_else(|| invalid("active export snapshot is missing"))?;
        tx.commit()
            .map_err(|_| invalid("cannot finish current export read"))?;
        Ok(Some(snapshot))
    }

    /// Read `id` only if it belongs to `key`; corrupt content returns an error.
    /// Wrong-key and absent lookups both return None, without disclosing content.
    pub fn export_snapshot(&self, key: &str, id: i64) -> DomainResult<Option<ExportSnapshot>> {
        validate_export_key(key)?;
        validate_optional_id(Some(id))?;
        read_snapshot(&self.conn, key, id)
    }

    /// The active snapshot id for `key`, without loading or validating the
    /// body: a rollback's CAS pre-check needs only the pointer, and hashing
    /// a ≤8 MiB body under the ledger mutex froze every other endpoint.
    pub fn active_export_id(&self, key: &str) -> DomainResult<Option<i64>> {
        validate_export_key(key)?;
        read_active_id(&self.conn, key)
    }

    /// Whether snapshot `id` exists for `key` -- metadata only, so a
    /// rollback's existence check costs one indexed read, not a full
    /// content load plus SHA-256 plus JSON validation.
    pub fn export_snapshot_exists(&self, key: &str, id: i64) -> DomainResult<bool> {
        validate_export_key(key)?;
        validate_optional_id(Some(id))?;
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM export_snapshots WHERE key = ?1 AND id = ?2",
                params![key, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| invalid("cannot read export snapshot metadata"))?;
        Ok(found.is_some())
    }

    /// Atomically insert/reuse, activate, and audit an already-rendered export.
    /// Reject a stale expected pointer; forced no-ops are audited too.
    pub fn publish_export(&self, input: PublicationInput<'_>) -> DomainResult<PublicationResult> {
        validate_export_key(input.key)?;
        validate_export_actor(input.actor)?;
        validate_export_content(input.content, input.sha256, input.node_count)?;
        validate_optional_id(input.expected_active)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|_| invalid("cannot start export transaction"))?;
        check_active(&tx, input.key, input.expected_active)?;
        let existing = find_identical(&tx, &input)?;
        let reused = existing.is_some();
        let id = match existing {
            Some(id) => id,
            None => insert_snapshot(&tx, &input)?,
        };
        let snapshot = read_snapshot(&tx, input.key, id)?
            .ok_or_else(|| invalid("new export snapshot is missing"))?;
        let kind = if input.forced {
            "export_force_publish"
        } else {
            "export_publish"
        };
        activate_and_audit(
            &tx,
            &snapshot.summary,
            input.expected_active,
            input.actor,
            kind,
        )?;
        tx.commit()
            .map_err(|_| invalid("cannot commit export transaction"))?;
        Ok(PublicationResult {
            snapshot: SnapshotSummary {
                is_active: true,
                ..snapshot.summary
            },
            reused,
            changed: input.expected_active != Some(id),
        })
    }

    /// Explicit audited rollback to a validated same-key snapshot using CAS.
    /// The engine must materialize/validate the target file before this call.
    pub fn rollback_export(
        &self,
        key: &str,
        target_id: i64,
        actor: &str,
        expected_active: Option<i64>,
    ) -> DomainResult<SnapshotSummary> {
        validate_export_key(key)?;
        validate_export_actor(actor)?;
        validate_optional_id(Some(target_id))?;
        validate_optional_id(expected_active)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|_| invalid("cannot start rollback transaction"))?;
        check_active(&tx, key, expected_active)?;
        let snapshot = read_snapshot(&tx, key, target_id)?
            .ok_or_else(|| invalid("rollback snapshot not found for export key"))?;
        activate_and_audit(
            &tx,
            &snapshot.summary,
            expected_active,
            actor,
            "export_rollback",
        )?;
        tx.commit()
            .map_err(|_| invalid("cannot commit rollback transaction"))?;
        Ok(SnapshotSummary {
            is_active: true,
            ..snapshot.summary
        })
    }
}

/// Build an error using only a constant explanation, never a database payload.
fn invalid(message: &'static str) -> DomainError {
    DomainError::Storage(message.into())
}

/// Validate summary hash/count without loading its secret-bearing content.
fn validate_metadata(hash: &str, count: usize) -> DomainResult<()> {
    if count == 0 || count > MAX_EXPORT_NODES {
        return Err(invalid("export count outside allowed range"));
    }
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid("invalid export hash"));
    }
    Ok(())
}

/// Bound a requested page; reject zero instead of accidentally returning everything.
fn validate_page(limit: usize) -> DomainResult<()> {
    if !(1..=MAX_EXPORT_PAGE).contains(&limit) {
        return Err(invalid("export page limit outside allowed range"));
    }
    Ok(())
}

/// Check that an optional database ID can refer to a real snapshot.
fn validate_optional_id(id: Option<i64>) -> DomainResult<()> {
    if id.is_some_and(|value| value <= 0) {
        return Err(invalid("invalid export snapshot id"));
    }
    Ok(())
}

/// Decode the six metadata columns shared by list and content queries.
fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotSummary> {
    Ok(SnapshotSummary {
        id: row.get(0)?,
        key: row.get(1)?,
        created_at: row.get(2)?,
        node_count: row.get(3)?,
        sha256: row.get(4)?,
        is_active: row.get(5)?,
    })
}

/// Read a bounded metadata query with bound parameters; never select content.
fn read_summaries(
    conn: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> DomainResult<Vec<SnapshotSummary>> {
    let mut statement = conn
        .prepare(sql)
        .map_err(|_| invalid("cannot list export metadata"))?;
    let rows = statement
        .query_map(parameters, summary_from_row)
        .map_err(|_| invalid("cannot list export metadata"))?;
    let summaries = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("corrupt export metadata"))?;
    for summary in &summaries {
        validate_export_key(&summary.key)?;
        validate_metadata(&summary.sha256, summary.node_count)?;
    }
    Ok(summaries)
}

/// Fetch a single key-bound row; cap content in SQL before allocating it in Rust.
fn read_snapshot(conn: &Connection, key: &str, id: i64) -> DomainResult<Option<ExportSnapshot>> {
    let row = conn
        .query_row(
            "SELECT s.id, s.key, s.created_at, s.node_count, s.sha256,
           EXISTS(SELECT 1 FROM active_exports a WHERE a.key = s.key AND a.snapshot_id = s.id),
           CASE WHEN typeof(s.content) = 'text'
             AND length(CAST(s.content AS BLOB)) BETWEEN 1 AND ?3
             THEN s.content ELSE NULL END
         FROM export_snapshots s WHERE s.key = ?1 AND s.id = ?2",
            params![key, id, MAX_EXPORT_BYTES as i64],
            |row| Ok((summary_from_row(row)?, row.get::<_, Option<String>>(6)?)),
        )
        .optional()
        .map_err(|_| invalid("cannot read export snapshot"))?;
    let Some((summary, content)) = row else {
        return Ok(None);
    };
    let content = content.ok_or_else(|| invalid("export content outside allowed range"))?;
    validate_export_content(&content, &summary.sha256, summary.node_count)?;
    Ok(Some(ExportSnapshot { summary, content }))
}

/// Read the pointer without treating an orphan join as an unpublished key.
fn read_active_id(conn: &Connection, key: &str) -> DomainResult<Option<i64>> {
    conn.query_row(
        "SELECT snapshot_id FROM active_exports WHERE key = ?1",
        [key],
        |row| row.get(0),
    )
    .optional()
    .map_err(|_| invalid("cannot read active export"))
}

/// Enforce the caller's comparison baseline inside the immediate write transaction.
fn check_active(conn: &Connection, key: &str, expected: Option<i64>) -> DomainResult<()> {
    if read_active_id(conn, key)? != expected {
        return Err(invalid("active export changed; retry publication decision"));
    }
    Ok(())
}

/// Reuse an exact same-key payload, including a historical version after rollback.
fn find_identical(conn: &Connection, input: &PublicationInput<'_>) -> DomainResult<Option<i64>> {
    let id: Option<i64> = conn.query_row(
        "SELECT id FROM export_snapshots WHERE key = ?1 AND sha256 = ?2 ORDER BY id DESC LIMIT 1",
        params![input.key, input.sha256], |row| row.get(0),
    ).optional().map_err(|_| invalid("cannot find existing export"))?;
    let Some(id) = id else { return Ok(None) };
    let snapshot = read_snapshot(conn, input.key, id)?
        .ok_or_else(|| invalid("existing export snapshot is missing"))?;
    if snapshot.content != input.content || snapshot.summary.node_count != input.node_count {
        return Err(invalid("existing export content mismatch"));
    }
    Ok(Some(id))
}

/// Insert a prevalidated payload in the caller's transaction and return its ID.
fn insert_snapshot(conn: &Connection, input: &PublicationInput<'_>) -> DomainResult<i64> {
    conn.execute(
        "INSERT INTO export_snapshots(key, created_at, node_count, sha256, content)
         VALUES(?1, ?2, ?3, ?4, ?5)",
        params![
            input.key,
            utc_now(),
            input.node_count as i64,
            input.sha256,
            input.content
        ],
    )
    .map_err(|_| invalid("cannot insert export snapshot"))?;
    Ok(conn.last_insert_rowid())
}

/// Activate one snapshot and append allowlisted metadata in the same transaction.
fn activate_and_audit(
    conn: &Connection,
    snapshot: &SnapshotSummary,
    previous_id: Option<i64>,
    actor: &str,
    kind: &str,
) -> DomainResult<()> {
    let now = utc_now();
    conn.execute(
        "INSERT INTO active_exports(key, snapshot_id, activated_at) VALUES(?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET snapshot_id = excluded.snapshot_id,
           activated_at = excluded.activated_at",
        params![snapshot.key, snapshot.id, now],
    )
    .map_err(|_| invalid("cannot activate export snapshot"))?;
    let detail = serde_json::json!({
        "key": snapshot.key, "snapshot_id": snapshot.id, "previous_id": previous_id,
        "sha256": snapshot.sha256, "node_count": snapshot.node_count, "actor": actor,
    })
    .to_string();
    conn.execute(
        "INSERT INTO security_audit(ts, kind, detail) VALUES(?1, ?2, ?3)",
        params![now, kind, detail],
    )
    .map_err(|_| invalid("cannot audit export activation"))?;
    Ok(())
}

#[cfg(test)]
mod tests;
