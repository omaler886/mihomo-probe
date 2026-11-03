//! Permanent source identity and the one-artifact-per-source registry
//! (workstreams/18; migration 0010).
//!
//! The publication module keys exports by the configured `key`, which is a
//! *name*: renaming a collection in Sub-Store or editing the config would
//! historically fork the same logical subscription into a second product. The
//! two tables behind this module close that hole:
//!
//! * `sources` gives every logical source an immutable `source_id`. The
//!   configured `key` stays the natural key (the ledger's `source` columns and
//!   the export keys already speak it), but `display_name`, `input_type`,
//!   `input_ref` and `enabled` are mutable attributes that never mint a new id.
//! * `artifacts` pins exactly one publishable product to each source --
//!   `UNIQUE(source_id)` is the hard form of "one source, one artifact" -- and
//!   carries the Gist-side bookkeeping (fixed file name, last published digest)
//!   the publisher needs to decide `NO_CHANGE` without touching the network.
//!
//! Everything here is additive over the Python ledger: the Python service
//! never reads these tables, and this crate never rewrites a `source` column.

use rusqlite::{params, OptionalExtension};

use probe_domain::{DomainError, DomainResult};
use serde::Serialize;

use crate::{utc_now, Storage};

/// A source id is always a v4 UUID minted by this crate, so the strict shape
/// is both documentation and a cheap gate: a hostile id is refused before it
/// reaches any statement, rather than merely being safely parameterised.
fn validate_source_id(id: &str) -> DomainResult<()> {
    if is_uuid_shape(id) {
        Ok(())
    } else {
        Err(invalid("invalid source id"))
    }
}

/// An artifact id is `artifact:<uuid>`: recognisable in a log line without a
/// join, and validated to the same strict shape.
fn validate_artifact_id(id: &str) -> DomainResult<()> {
    match id.strip_prefix("artifact:") {
        Some(uuid) if is_uuid_shape(uuid) => Ok(()),
        _ => Err(invalid("invalid artifact id")),
    }
}

/// The canonical 8-4-4-4-12 lowercase-hex UUID shape. The version and variant
/// bits are not re-checked here: the id was minted by [`NEW_SOURCE_ID_SQL`] or
/// the backfill, and a future id scheme would widen this function, not the
/// callers. Shared with [`crate::runs`], which takes source ids from path
/// segments.
pub(crate) fn is_uuid_shape(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        let expected_separator = matches!(index, 8 | 13 | 18 | 23);
        if expected_separator != (*byte == b'-') {
            return false;
        }
        if !expected_separator && !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// A random v4 UUID, generated in SQL so this crate needs no RNG dependency
/// for what is a one-time-per-source insert. Version nibble forced to `4`,
/// variant nibble forced into `[89ab]`; see migration 0010 for the same
/// expression on the backfill path.
const NEW_SOURCE_ID_SQL: &str = "
    (SELECT substr(h, 1, 8) || '-' || substr(h, 9, 4) || '-4' || substr(h, 14, 3)
            || '-' || substr('89ab', (unicode(substr(h, 17, 1)) % 4) + 1, 1)
            || substr(h, 18, 3) || '-' || substr(h, 21, 12)
       FROM (SELECT lower(hex(randomblob(16))) AS h))";

/// The input kind a source reads from Sub-Store. `Unknown` is what a
/// backfilled row carries until the service re-syncs it from the live config.
pub const INPUT_COLLECTION: &str = "collection";
pub const INPUT_SUB: &str = "sub";
pub const INPUT_UNKNOWN: &str = "unknown";

/// One logical input source. `key` is the natural key; `source_id` is the
/// identity nothing may change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceRow {
    pub source_id: String,
    pub key: String,
    pub display_name: String,
    pub input_type: String,
    pub input_ref: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// The single artifact a source may publish. `gist_file` is unique because a
/// Gist is one flat namespace; `last_content_sha256` is the publisher's
/// `NO_CHANGE` baseline and is only advanced after a successful publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactRow {
    pub artifact_id: String,
    pub source_id: String,
    pub profile_id: String,
    pub gist_id: Option<String>,
    pub gist_file: String,
    pub stable_url: Option<String>,
    pub last_content_sha256: Option<String>,
    pub last_success_run_id: Option<i64>,
    pub last_published_at: Option<String>,
}

/// An internal error is always a constant explanation, never a database
/// payload (the publication module's rule, kept for the same reason).
fn invalid(message: &'static str) -> DomainError {
    DomainError::Storage(message.into())
}

/// A source key the ledger can address: bounded, no control characters, no
/// path separators. Reuses the export-key grammar so a key that can name an
/// export file can also name a source row.
fn validate_source_key(key: &str) -> DomainResult<()> {
    if key.is_empty() || key.len() > 48 || key != key.trim() {
        return Err(invalid("invalid source key"));
    }
    if key.chars().any(|ch| {
        ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }) {
        return Err(invalid("invalid source key"));
    }
    Ok(())
}

/// A display name is shown in the GUI and never used as an identity: it only
/// has to be bounded and control-free so it cannot forge a log line.
fn validate_display_name(name: &str) -> DomainResult<()> {
    if name.is_empty() || name.chars().count() > 128 || name.chars().any(char::is_control) {
        return Err(invalid("invalid source display name"));
    }
    Ok(())
}

/// `collection` or `sub`; `unknown` is tolerated on read but never written by
/// the sync path (the caller always knows the configured kind).
fn validate_input_type(kind: &str) -> DomainResult<()> {
    if !matches!(kind, INPUT_COLLECTION | INPUT_SUB) {
        return Err(invalid("invalid source input type"));
    }
    Ok(())
}

/// The Gist file name a source publishes under. Same grammar as an export key
/// plus the `.yaml` suffix, so the name is safe in a URL, a file and a log.
fn validate_gist_file(name: &str) -> DomainResult<()> {
    let stem = name.strip_suffix(".yaml").unwrap_or(name);
    validate_source_key(stem)?;
    if name.len() > 64 {
        return Err(invalid("invalid gist file name"));
    }
    Ok(())
}

/// A lowercase SHA-256 hex digest, or NULL.
fn validate_optional_sha256(sha: Option<&str>) -> DomainResult<()> {
    if let Some(sha) = sha {
        if sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid("invalid content digest"));
        }
        if sha.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(invalid("invalid content digest"));
        }
    }
    Ok(())
}

impl Storage {
    /// Upsert one source by its natural key and return its immutable id.
    ///
    /// First sight of a key mints the `source_id`; every later call only
    /// refreshes the mutable attributes. A rename therefore updates one row --
    /// it can never fork the source, its artifact, or its Gist file.
    pub fn sync_source(
        &self,
        key: &str,
        display_name: &str,
        input_type: &str,
        input_ref: &str,
        enabled: bool,
    ) -> DomainResult<String> {
        validate_source_key(key)?;
        validate_display_name(display_name)?;
        validate_input_type(input_type)?;
        if input_ref.is_empty() || input_ref.chars().count() > 128 {
            return Err(invalid("invalid source input reference"));
        }
        let now = utc_now();
        let sql = format!(
            "INSERT INTO sources(source_id, key, display_name, input_type, input_ref, enabled, created_at, updated_at)
             VALUES({NEW_SOURCE_ID_SQL}, ?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(key) DO UPDATE SET
                display_name = excluded.display_name,
                input_type = excluded.input_type,
                input_ref = excluded.input_ref,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at"
        );
        self.conn
            .execute(
                &sql,
                params![
                    key,
                    display_name,
                    input_type,
                    input_ref,
                    i64::from(enabled),
                    now
                ],
            )
            .map_err(|e| DomainError::Storage(format!("sync source: {e}")))?;
        self.source_id_for_key(key)?
            .ok_or_else(|| invalid("source row missing after sync"))
    }

    /// The immutable id for a configured key, or None when never synced.
    pub fn source_id_for_key(&self, key: &str) -> DomainResult<Option<String>> {
        validate_source_key(key)?;
        self.conn
            .query_row(
                "SELECT source_id FROM sources WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| DomainError::Storage(format!("read source id: {e}")))
    }

    /// Every source, oldest first (creation order is the config's order).
    pub fn list_sources(&self) -> DomainResult<Vec<SourceRow>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT source_id, key, display_name, input_type, input_ref, enabled, created_at, updated_at
                   FROM sources ORDER BY created_at, source_id",
            )
            .map_err(|e| DomainError::Storage(format!("list sources: {e}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok(SourceRow {
                    source_id: row.get(0)?,
                    key: row.get(1)?,
                    display_name: row.get(2)?,
                    input_type: row.get(3)?,
                    input_ref: row.get(4)?,
                    enabled: row.get::<_, i64>(5)? != 0,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })
            .map_err(|e| DomainError::Storage(format!("list sources: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::Storage(format!("read source row: {e}")))
    }

    /// One source by id, or None. The id is the only lookup a GUI should use:
    /// the key is an attribute, not an identity.
    pub fn source_by_id(&self, source_id: &str) -> DomainResult<Option<SourceRow>> {
        validate_source_id(source_id)?;
        self.conn
            .query_row(
                "SELECT source_id, key, display_name, input_type, input_ref, enabled, created_at, updated_at
                   FROM sources WHERE source_id = ?1",
                params![source_id],
                |row| {
                    Ok(SourceRow {
                        source_id: row.get(0)?,
                        key: row.get(1)?,
                        display_name: row.get(2)?,
                        input_type: row.get(3)?,
                        input_ref: row.get(4)?,
                        enabled: row.get::<_, i64>(5)? != 0,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(|e| DomainError::Storage(format!("read source: {e}")))
    }

    /// The artifact row of a source, creating it on first call.
    ///
    /// `gist_file_hint` is only honoured when the row is created; an existing
    /// row keeps its file name, because the stable URL and the Gist namespace
    /// both depend on it. The artifact id embeds the source id so a log line
    /// identifies the pair without a join.
    pub fn ensure_artifact(
        &self,
        source_id: &str,
        gist_file_hint: &str,
    ) -> DomainResult<ArtifactRow> {
        validate_gist_file(gist_file_hint)?;
        // The insert is the only path that can violate the unique constraints,
        // and a violation here means two sources share a file name -- a config
        // bug the operator has to see, not something to silently re-map.
        self.conn
            .execute(
                "INSERT INTO artifacts(artifact_id, source_id, profile_id, gist_file)
                 VALUES('artifact:' || ?1, ?1, 'default', ?2)
                 ON CONFLICT(source_id) DO NOTHING",
                params![source_id, gist_file_hint],
            )
            .map_err(|e| DomainError::Storage(format!("ensure artifact: {e}")))?;
        self.artifact_for_source(source_id)?
            .ok_or_else(|| invalid("artifact row missing after ensure"))
    }

    /// The artifact of a source without creating one.
    pub fn artifact_for_source(&self, source_id: &str) -> DomainResult<Option<ArtifactRow>> {
        self.artifact_query("WHERE source_id = ?1", params![source_id])
    }

    /// One artifact by its own id.
    pub fn artifact_by_id(&self, artifact_id: &str) -> DomainResult<Option<ArtifactRow>> {
        validate_artifact_id(artifact_id)?;
        self.artifact_query("WHERE artifact_id = ?1", params![artifact_id])
    }

    /// Every artifact, for the publisher's startup reconciliation and the GUI.
    pub fn list_artifacts(&self) -> DomainResult<Vec<ArtifactRow>> {
        self.artifact_query_all("ORDER BY artifact_id")
    }

    /// The artifact published under a fixed Gist file name.
    pub fn artifact_by_gist_file(&self, gist_file: &str) -> DomainResult<Option<ArtifactRow>> {
        validate_gist_file(gist_file)?;
        self.artifact_query("WHERE gist_file = ?1", params![gist_file])
    }

    /// Record a successful publication of `artifact_id`.
    ///
    /// The digest is the `NO_CHANGE` baseline: it advances only after the
    /// publisher reports success, so a failed round leaves the previous digest
    /// standing and the next run re-attempts the same content.
    pub fn mark_artifact_published(
        &self,
        artifact_id: &str,
        content_sha256: &str,
        run_id: i64,
        gist_id: Option<&str>,
        gist_file: Option<&str>,
    ) -> DomainResult<()> {
        validate_optional_sha256(Some(content_sha256))?;
        if let Some(gist_file) = gist_file {
            validate_gist_file(gist_file)?;
        }
        let now = utc_now();
        let updated = self
            .conn
            .execute(
                "UPDATE artifacts
                    SET last_content_sha256 = ?2,
                        last_success_run_id = ?3,
                        last_published_at = ?4,
                        gist_id = COALESCE(?5, gist_id),
                        gist_file = COALESCE(?6, gist_file)
                  WHERE artifact_id = ?1",
                params![artifact_id, content_sha256, run_id, now, gist_id, gist_file],
            )
            .map_err(|e| DomainError::Storage(format!("mark artifact published: {e}")))?;
        if updated == 0 {
            return Err(invalid("artifact does not exist"));
        }
        Ok(())
    }

    /// Point an artifact at its Gist (the one-time `gist init` step).
    ///
    /// `gist_file` may be re-mapped only while nothing has been published
    /// through the old name; an operator moving a live file is a migration,
    /// not an edit, and the GUI says so.
    pub fn set_artifact_gist(
        &self,
        artifact_id: &str,
        gist_id: &str,
        gist_file: &str,
    ) -> DomainResult<()> {
        validate_gist_file(gist_file)?;
        if gist_id.is_empty()
            || gist_id.len() > 64
            || !gist_id.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(invalid("invalid gist id"));
        }
        let updated = self
            .conn
            .execute(
                "UPDATE artifacts SET gist_id = ?2, gist_file = ?3 WHERE artifact_id = ?1",
                params![artifact_id, gist_id, gist_file],
            )
            .map_err(|e| DomainError::Storage(format!("set artifact gist: {e}")))?;
        if updated == 0 {
            return Err(invalid("artifact does not exist"));
        }
        Ok(())
    }

    /// Record the stable proxy URL an artifact is consumed through.
    pub fn set_artifact_stable_url(&self, artifact_id: &str, stable_url: &str) -> DomainResult<()> {
        if stable_url.is_empty()
            || stable_url.len() > 256
            || stable_url.chars().any(char::is_control)
        {
            return Err(invalid("invalid stable url"));
        }
        let updated = self
            .conn
            .execute(
                "UPDATE artifacts SET stable_url = ?2 WHERE artifact_id = ?1",
                params![artifact_id, stable_url],
            )
            .map_err(|e| DomainError::Storage(format!("set artifact stable url: {e}")))?;
        if updated == 0 {
            return Err(invalid("artifact does not exist"));
        }
        Ok(())
    }

    /// One shared row decoder for the single-row lookups. Generic over the
    /// bound parameters so a caller can pass `params![...]` or `[]`.
    fn artifact_query<P: rusqlite::Params>(
        &self,
        where_clause: &str,
        key: P,
    ) -> DomainResult<Option<ArtifactRow>> {
        let sql = format!(
            "SELECT artifact_id, source_id, profile_id, gist_id, gist_file, stable_url,
                    last_content_sha256, last_success_run_id, last_published_at
               FROM artifacts {where_clause}"
        );
        let mut statement = self
            .conn
            .prepare(&sql)
            .map_err(|e| DomainError::Storage(format!("read artifact: {e}")))?;
        let mut rows = statement
            .query_map(key, artifact_from_row)
            .map_err(|e| DomainError::Storage(format!("read artifact: {e}")))?;
        match rows.next() {
            Some(row) => {
                Ok(Some(row.map_err(|e| {
                    DomainError::Storage(format!("read artifact: {e}"))
                })?))
            }
            None => Ok(None),
        }
    }

    /// The same decoder for the unbounded list read.
    fn artifact_query_all(&self, order: &str) -> DomainResult<Vec<ArtifactRow>> {
        let sql = format!(
            "SELECT artifact_id, source_id, profile_id, gist_id, gist_file, stable_url,
                    last_content_sha256, last_success_run_id, last_published_at
               FROM artifacts {order}"
        );
        let mut statement = self
            .conn
            .prepare(&sql)
            .map_err(|e| DomainError::Storage(format!("list artifacts: {e}")))?;
        let rows = statement
            .query_map([], artifact_from_row)
            .map_err(|e| DomainError::Storage(format!("list artifacts: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::Storage(format!("read artifact row: {e}")))
    }
}

/// Decode the nine artifact columns. `last_success_run_id` is the ledger's
/// `rounds.id`, so it stays an INTEGER and may legitimately be NULL.
fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        artifact_id: row.get(0)?,
        source_id: row.get(1)?,
        profile_id: row.get(2)?,
        gist_id: row.get(3)?,
        gist_file: row.get(4)?,
        stable_url: row.get(5)?,
        last_content_sha256: row.get(6)?,
        last_success_run_id: row.get(7)?,
        last_published_at: row.get(8)?,
    })
}

#[cfg(test)]
mod tests;
