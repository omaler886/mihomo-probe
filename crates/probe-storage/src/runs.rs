//! Per-source run slots: the single-flight and idempotency ledger
//! (workstreams/18; migration 0011).
//!
//! A measuring round is global, but the API hands out *per-source* triggers,
//! so the rule that has to survive an operator clicking twice -- or a client
//! retrying a request it believes was lost -- is one the database enforces:
//!
//! * at most one OPEN slot (`claimed` or `active`) per source, through
//!   `ux_source_runs_one_open`;
//! * one slot per `Idempotency-Key` for as long as the caller may retry it,
//!   through `ux_source_runs_idempotency`;
//! * a minimum spacing per source, measured from the last run that reached a
//!   round, so a fast click sequence coalesces instead of queueing work.
//!
//! A trigger CLAIMS a slot, and a claim is deliberately cheap to release: if
//! no round can start (the global lock is held), the slot is deleted and the
//! idempotency key is free again -- a key pinned to a failed claim would
//! refuse the retry that was supposed to succeed. Once the round exists the
//! slot is ATTACHED to it, and from then on it is history: the caller's
//! `GET /api/v1/runs/{id}` reads exactly this row.

use std::time::Duration;

use rusqlite::{params, OptionalExtension};

use probe_domain::{DomainError, DomainResult};
use serde::Serialize;

use crate::{utc_now, Storage};

/// Statuses that occupy a source's single slot.
pub const STATUS_CLAIMED: &str = "claimed";
pub const STATUS_ACTIVE: &str = "active";
/// Terminal statuses. `failed`/`cancelled` mirror the round they belonged to;
/// `abandoned` is written by the reaper for a slot whose round vanished.
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_CANCELLED: &str = "cancelled";
pub const STATUS_ABANDONED: &str = "abandoned";

/// The `source_runs.publish_status` vocabulary (workstreams/18 B2): what the
/// remote publish leg did with this run's publication. `published` is the
/// authoritative success (the artifact row's digest advanced); a composite
/// publish whose Gist leg failed also lands here, with the sanitized reason
/// in `error_code` -- the local artifact is what the stable URL serves.
pub const PUBLISH_PUBLISHED: &str = "published";
/// The destination already held exactly this content: nothing was sent.
pub const PUBLISH_NO_CHANGE: &str = "no_change";
/// The publish failed; `error_code` names the frozen publisher code.
pub const PUBLISH_FAILED: &str = "publish_failed";
/// The publisher could not even be constructed (broken token file, no gist
/// id): the round's local results stand, nothing remote was attempted.
pub const PUBLISH_CONFIG_ERROR: &str = "gist_config_error";

/// The publish statuses this crate accepts. Frozen spellings; anything else
/// is refused rather than stored for a GUI to render.
const PUBLISH_STATUSES: &[&str] = &[
    PUBLISH_PUBLISHED,
    PUBLISH_NO_CHANGE,
    PUBLISH_FAILED,
    PUBLISH_CONFIG_ERROR,
];

/// Slot timestamps are `utc_now()` stamps: second resolution, UTC.
const STAMP: &[time::format_description::BorrowedFormatItem<'_>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");

fn storage_error(action: &str) -> impl Fn(rusqlite::Error) -> DomainError + '_ {
    move |e| DomainError::Storage(format!("{action}: {e}"))
}

/// A key a caller may retry with: bounded and control-free. It is never
/// echoed back -- the row answers with `has_idempotency_key`.
fn validate_idempotency_key(key: &str) -> DomainResult<()> {
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(DomainError::Storage("invalid idempotency key".into()));
    }
    Ok(())
}

/// A trigger label is written by our own call sites, so it is bounded rather
/// than trusted to shape an HTTP body into a legal tag.
fn validate_trigger(trigger: &str) -> DomainResult<()> {
    if trigger.is_empty() || trigger.len() > 32 || trigger.chars().any(char::is_control) {
        return Err(DomainError::Storage("invalid run trigger".into()));
    }
    Ok(())
}

/// The slot ids are ours; the source id must be the UUID shape this crate
/// mints, so a hostile path segment is refused before it reaches a statement.
fn validate_source_id(source_id: &str) -> DomainResult<()> {
    if crate::sources::is_uuid_shape(source_id) {
        Ok(())
    } else {
        Err(DomainError::Storage("invalid source id".into()))
    }
}

/// One run slot row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceRunRow {
    pub id: i64,
    pub source_id: String,
    pub round_id: Option<i64>,
    pub trigger: String,
    pub status: String,
    pub has_idempotency_key: bool,
    pub claimed_at: String,
    pub updated_at: String,
    /// What the remote publish leg did with this run's publication
    /// (workstreams/18 B2). `None` = the local-only pipeline ran, or the row
    /// predates migration 0012.
    pub publish_status: Option<String>,
    /// The publisher's frozen error code (`gist_auth`, ...) or the sanitized
    /// gist-side note a composite publish carries. Never a credential.
    pub error_code: Option<String>,
}

/// The outcome of trying to claim a slot. Every arm carries the run id, so the
/// caller can always point the client at the run that answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The caller owns the slot: start the round, then attach it.
    Claimed { run_id: i64 },
    /// A slot already covers this request: the same idempotency key (retry),
    /// or the same source with a run in flight (duplicate). `open` says
    /// whether that run is still in flight.
    Deduped {
        run_id: i64,
        round_id: Option<i64>,
        open: bool,
    },
    /// The source ran within the minimum spacing: `round_id` is that run.
    Coalesced { run_id: i64, round_id: Option<i64> },
    /// One idempotency key was reused across two sources. That is a client
    /// bug, and the answer is the other source's run -- never a new one.
    KeyConflict { run_id: i64 },
}

impl ClaimOutcome {
    /// True when the caller owns a fresh slot.
    pub fn is_claimed(&self) -> bool {
        matches!(self, Self::Claimed { .. })
    }

    /// The run the caller should observe: the duplicate's, or None when the
    /// caller owns the slot (its round does not exist yet).
    pub fn run_to_watch(&self) -> Option<i64> {
        match self {
            Self::Claimed { .. } => None,
            Self::Deduped { run_id, .. }
            | Self::Coalesced { run_id, .. }
            | Self::KeyConflict { run_id } => Some(*run_id),
        }
    }
}

impl Storage {
    /// Claim (or reuse) this source's run slot.
    ///
    /// `min_spacing` is measured from the start of the last run that reached a
    /// round; `Duration::ZERO` disables the check. The whole decision runs in
    /// one IMMEDIATE transaction so two racing triggers serialise on the write
    /// lock and exactly one of them is told it owns the slot.
    pub fn claim_source_run(
        &self,
        source_id: &str,
        trigger: &str,
        idempotency_key: Option<&str>,
        min_spacing: Duration,
    ) -> DomainResult<ClaimOutcome> {
        validate_source_id(source_id)?;
        validate_trigger(trigger)?;
        if let Some(key) = idempotency_key {
            validate_idempotency_key(key)?;
        }
        let transaction = self.immediate_transaction()?;

        // 1. The caller's own key wins over everything else: a retry must see
        //    the same run, even after that run finished.
        if let Some(key) = idempotency_key {
            if let Some(row) = self.run_for_idempotency_key(&transaction, key)? {
                // Whatever else happens, this key belongs to a source; a
                // different source asking with it is a bug, not a race.
                let outcome = if row.source_id != source_id {
                    ClaimOutcome::KeyConflict { run_id: row.id }
                } else {
                    ClaimOutcome::Deduped {
                        run_id: row.id,
                        round_id: row.round_id,
                        open: is_open(&row.status),
                    }
                };
                transaction.rollback().ok();
                return Ok(outcome);
            }
        }

        // 2. An open slot for this source: the run is already happening.
        if let Some(row) = self.open_run_for_source(&transaction, source_id)? {
            transaction.rollback().ok();
            return Ok(ClaimOutcome::Deduped {
                run_id: row.id,
                round_id: row.round_id,
                open: true,
            });
        }

        // 3. The spacing window: a source that just ran does not need another
        //    round because someone double-clicked.
        if min_spacing > Duration::ZERO {
            if let Some(row) = self.last_attached_run(&transaction, source_id)? {
                let anchor = match row.round_id {
                    Some(round_id) => self.round_started_at(&transaction, round_id)?,
                    None => row.claimed_at.clone(),
                };
                if elapsed_since(&anchor) < min_spacing {
                    transaction.rollback().ok();
                    return Ok(ClaimOutcome::Coalesced {
                        run_id: row.id,
                        round_id: row.round_id,
                    });
                }
            }
        }

        // 4. Nothing covers this request: the caller owns the slot.
        let now = utc_now();
        transaction
            .execute(
                "INSERT INTO source_runs(source_id, round_id, trigger, status, idempotency_key, claimed_at, updated_at)
                 VALUES(?1, NULL, ?2, 'claimed', ?3, ?4, ?4)",
                params![source_id, trigger, idempotency_key, now],
            )
            .map_err(storage_error("claim source run"))?;
        let run_id = transaction.last_insert_rowid();
        transaction
            .commit()
            .map_err(storage_error("claim source run"))?;
        Ok(ClaimOutcome::Claimed { run_id })
    }

    /// Bind a claimed slot to the round that is now running.
    pub fn attach_source_run(&self, run_id: i64, round_id: i64) -> DomainResult<()> {
        let updated = self
            .conn
            .execute(
                "UPDATE source_runs SET round_id = ?2, status = 'active', updated_at = ?3
                  WHERE id = ?1",
                params![run_id, round_id, utc_now()],
            )
            .map_err(storage_error("attach source run"))?;
        if updated == 0 {
            return Err(DomainError::Storage("source run does not exist".into()));
        }
        Ok(())
    }

    /// Release a slot whose round never started. Deleting the row frees the
    /// idempotency key too, so the retry that follows a transient busy state
    /// can succeed instead of being pinned to a run that does not exist.
    pub fn release_source_run(&self, run_id: i64) -> DomainResult<()> {
        let deleted = self
            .conn
            .execute(
                "DELETE FROM source_runs WHERE id = ?1 AND status IN ('claimed', 'active')",
                params![run_id],
            )
            .map_err(storage_error("release source run"))?;
        if deleted == 0 {
            return Err(DomainError::Storage("source run is not open".into()));
        }
        Ok(())
    }

    /// Close a slot with the round's final status. Idempotent: only an open
    /// slot matches, so a task's guard racing an explicit close loses quietly.
    pub fn close_source_run(&self, run_id: i64, status: &str) -> DomainResult<()> {
        if !is_closed(status) {
            return Err(DomainError::Storage("invalid source run status".into()));
        }
        self.conn
            .execute(
                "UPDATE source_runs SET status = ?2, updated_at = ?3
                  WHERE id = ?1 AND status IN ('claimed', 'active')",
                params![run_id, status, utc_now()],
            )
            .map_err(storage_error("close source run"))?;
        Ok(())
    }

    /// Close slots whose round is gone (crash recovery) so the source can be
    /// triggered again. Returns how many slots were abandoned.
    ///
    /// A slot is orphaned when it has no round and the process that claimed it
    /// is gone (the caller decides that, by only reaping slots older than the
    /// round budget), when its round finished, or when its round does not
    /// exist at all.
    pub fn reap_orphan_source_runs(&self, older_than: Duration) -> DomainResult<usize> {
        let cutoff = (time::OffsetDateTime::now_utc()
            - time::Duration::seconds(older_than.as_secs() as i64))
        .format(STAMP)
        .map_err(|e| DomainError::Storage(format!("reap cutoff: {e}")))?;
        let updated = self
            .conn
            .execute(
                "UPDATE source_runs
                    SET status = 'abandoned', updated_at = ?1
                  WHERE status IN ('claimed', 'active')
                    AND claimed_at <= ?2
                    AND (round_id IS NULL
                         OR round_id NOT IN (SELECT id FROM rounds WHERE finished_at IS NULL))",
                params![utc_now(), cutoff],
            )
            .map_err(storage_error("reap source runs"))?;
        Ok(updated)
    }

    /// One slot by id.
    pub fn source_run(&self, run_id: i64) -> DomainResult<Option<SourceRunRow>> {
        self.run_query("WHERE id = ?1", params![run_id])
    }

    /// The source's open slot, if any.
    pub fn open_source_run(&self, source_id: &str) -> DomainResult<Option<SourceRunRow>> {
        validate_source_id(source_id)?;
        let transaction = self
            .conn
            .unchecked_transaction()
            .map_err(storage_error("open run read"))?;
        let row = self.open_run_for_source(&transaction, source_id)?;
        transaction.rollback().ok();
        Ok(row)
    }

    /// The source's run history, newest first.
    pub fn list_source_runs(&self, source_id: &str, limit: i64) -> DomainResult<Vec<SourceRunRow>> {
        validate_source_id(source_id)?;
        let limit = limit.clamp(1, 500);
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, source_id, round_id, trigger, status, idempotency_key, claimed_at,
                        updated_at, publish_status, error_code
                   FROM source_runs WHERE source_id = ?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(storage_error("list source runs"))?;
        let rows = statement
            .query_map(params![source_id, limit], run_from_row)
            .map_err(storage_error("list source runs"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(storage_error("read source run"))
    }

    /// Record a closed slot for a round triggered outside the claim path (a
    /// CLI one-shot that walked every enabled source).
    ///
    /// The insert is skipped when the source already has an open slot -- that
    /// slot is the truth, and this is only history. Returns the new id.
    pub fn record_source_run(
        &self,
        source_id: &str,
        round_id: i64,
        trigger: &str,
        status: &str,
    ) -> DomainResult<Option<i64>> {
        validate_source_id(source_id)?;
        validate_trigger(trigger)?;
        if !is_closed(status) {
            return Err(DomainError::Storage("invalid source run status".into()));
        }
        let now = utc_now();
        let inserted = self
            .conn
            .execute(
                "INSERT INTO source_runs(source_id, round_id, trigger, status, idempotency_key, claimed_at, updated_at)
                 SELECT ?1, ?2, ?3, ?4, NULL, ?5, ?5
                 WHERE NOT EXISTS (SELECT 1 FROM source_runs
                                    WHERE source_id = ?1 AND status IN ('claimed', 'active'))",
                params![source_id, round_id, trigger, status, now],
            )
            .map_err(storage_error("record source run"))?;
        Ok((inserted > 0).then(|| self.conn.last_insert_rowid()))
    }

    /// Record what the remote publish leg did with one source's publication
    /// for `round_id` (workstreams/18 B2).
    ///
    /// The run slot the API claim path opened already carries the round id, so
    /// the update finds it whether it is still open or already closed; a slot
    /// from a caller that never claims (a CLI one-shot) does not exist yet, so
    /// a closed history row is inserted instead -- the same shape
    /// [`Self::record_source_run`] writes, plus the publish columns. Returns
    /// the number of rows written (always 1).
    ///
    /// The artifact's `last_content_sha256` is NOT touched here: only a
    /// successful publish advances that digest (《指导》§3.4), and the engine
    /// does that through `mark_artifact_published`.
    pub fn record_source_publish_status(
        &self,
        source_id: &str,
        round_id: i64,
        publish_status: &str,
        error_code: Option<&str>,
    ) -> DomainResult<usize> {
        validate_source_id(source_id)?;
        if !PUBLISH_STATUSES.contains(&publish_status) {
            return Err(DomainError::Storage("invalid publish status".into()));
        }
        if let Some(code) = error_code {
            if code.is_empty() || code.chars().count() > 128 || code.chars().any(char::is_control) {
                return Err(DomainError::Storage("invalid publish error code".into()));
            }
        }
        let now = utc_now();
        let updated = self
            .conn
            .execute(
                "UPDATE source_runs
                    SET publish_status = ?2, error_code = ?3, updated_at = ?4
                  WHERE source_id = ?1 AND round_id = ?5",
                params![source_id, publish_status, error_code, now, round_id],
            )
            .map_err(storage_error("record publish status"))?;
        if updated > 0 {
            return Ok(updated);
        }
        // No slot for this round (the claim path was not used): history only.
        self.conn
            .execute(
                "INSERT INTO source_runs(source_id, round_id, trigger, status, idempotency_key,
                                        claimed_at, updated_at, publish_status, error_code)
                 VALUES(?1, ?2, 'publish', 'completed', NULL, ?3, ?3, ?4, ?5)",
                params![source_id, round_id, now, publish_status, error_code],
            )
            .map_err(storage_error("record publish status"))?;
        Ok(1)
    }

    /// Shared single-row decoder for the id-keyed lookups.
    fn run_query<P: rusqlite::Params>(
        &self,
        where_clause: &str,
        key: P,
    ) -> DomainResult<Option<SourceRunRow>> {
        let sql = format!(
            "SELECT id, source_id, round_id, trigger, status, idempotency_key, claimed_at,
                    updated_at, publish_status, error_code
               FROM source_runs {where_clause}"
        );
        let mut statement = self
            .conn
            .prepare(&sql)
            .map_err(storage_error("read source run"))?;
        let mut rows = statement
            .query_map(key, run_from_row)
            .map_err(storage_error("read source run"))?;
        match rows.next() {
            Some(row) => Ok(Some(row.map_err(storage_error("read source run"))?)),
            None => Ok(None),
        }
    }

    /// The slot a key already resolves to. The unique index makes at most one
    /// row possible; `ORDER BY id DESC` only pins which one we report.
    fn run_for_idempotency_key(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        key: &str,
    ) -> DomainResult<Option<SourceRunRow>> {
        transaction
            .query_row(
                "SELECT id, source_id, round_id, trigger, status, idempotency_key, claimed_at,
                        updated_at, publish_status, error_code
                   FROM source_runs WHERE idempotency_key = ?1 ORDER BY id DESC LIMIT 1",
                params![key],
                run_from_row,
            )
            .optional()
            .map_err(storage_error("read idempotency key"))
    }

    /// The source's open slot, if any.
    fn open_run_for_source(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        source_id: &str,
    ) -> DomainResult<Option<SourceRunRow>> {
        transaction
            .query_row(
                "SELECT id, source_id, round_id, trigger, status, idempotency_key, claimed_at,
                        updated_at, publish_status, error_code
                   FROM source_runs
                  WHERE source_id = ?1 AND status IN ('claimed', 'active')
                  ORDER BY id DESC LIMIT 1",
                params![source_id],
                run_from_row,
            )
            .optional()
            .map_err(storage_error("read open source run"))
    }

    /// The source's most recent slot that reached a round.
    fn last_attached_run(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        source_id: &str,
    ) -> DomainResult<Option<SourceRunRow>> {
        transaction
            .query_row(
                "SELECT id, source_id, round_id, trigger, status, idempotency_key, claimed_at,
                        updated_at, publish_status, error_code
                   FROM source_runs
                  WHERE source_id = ?1 AND round_id IS NOT NULL
                  ORDER BY id DESC LIMIT 1",
                params![source_id],
                run_from_row,
            )
            .optional()
            .map_err(storage_error("read last source run"))
    }

    /// `rounds.started_at` for the spacing anchor: the interval counts from
    /// when the measuring began, not from when the caller asked.
    fn round_started_at(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        round_id: i64,
    ) -> DomainResult<String> {
        transaction
            .query_row(
                "SELECT started_at FROM rounds WHERE id = ?1",
                params![round_id],
                |row| row.get(0),
            )
            .map_err(storage_error("read round start"))
    }
}

/// A slot occupies its source while it is claimed or active.
fn is_open(status: &str) -> bool {
    matches!(status, STATUS_CLAIMED | STATUS_ACTIVE)
}

/// The statuses a closed slot may carry.
fn is_closed(status: &str) -> bool {
    matches!(
        status,
        STATUS_COMPLETED | STATUS_FAILED | STATUS_CANCELLED | STATUS_ABANDONED
    )
}

/// Seconds since a `utc_now()` stamp, saturating at zero for a future or
/// unparseable value: a clock jump must never block work forever.
///
/// Hand-rolled for the same reason `scheduler::parse_stored_instant` is: the
/// workspace's `time` dependency enables `formatting` but not `parsing`, and
/// the grammar is fixed width, digits only, `T` or space.
fn elapsed_since(stamp: &str) -> Duration {
    let then = parse_stored_instant(stamp)
        .and_then(|when| u64::try_from(when.unix_timestamp()).ok())
        .unwrap_or(0);
    let now = time::OffsetDateTime::now_utc().unix_timestamp().max(0) as u64;
    Duration::from_secs(now.saturating_sub(then))
}

/// `2026-11-03T19:07:40` (or a space separator) as a UTC instant. Anything
/// else reads as "no timestamp", which the caller treats as zero age.
fn parse_stored_instant(text: &str) -> Option<time::OffsetDateTime> {
    let stamp = text.trim();
    let bytes = stamp.as_bytes();
    if bytes.len() != 19
        || (bytes[10] != b'T' && bytes[10] != b' ')
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let field = |start: usize, end: usize| -> Option<i64> { stamp.get(start..end)?.parse().ok() };
    let month = u8::try_from(field(5, 7)?).ok()?;
    let day = u8::try_from(field(8, 10)?).ok()?;
    let hour = u8::try_from(field(11, 13)?).ok()?;
    let minute = u8::try_from(field(14, 16)?).ok()?;
    let second = u8::try_from(field(17, 19)?).ok()?;
    let date = time::Date::from_calendar_date(
        i32::try_from(field(0, 4)?).ok()?,
        time::Month::try_from(month).ok()?,
        day,
    )
    .ok()?;
    let clock = time::Time::from_hms(hour, minute, second).ok()?;
    Some(date.with_time(clock).assume_utc())
}

/// Decode the ten slot columns, folding `idempotency_key` into a boolean:
/// the key belongs to the caller, not to anything we echo back.
fn run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceRunRow> {
    Ok(SourceRunRow {
        id: row.get(0)?,
        source_id: row.get(1)?,
        round_id: row.get(2)?,
        trigger: row.get(3)?,
        status: row.get(4)?,
        has_idempotency_key: row.get::<_, Option<String>>(5)?.is_some(),
        claimed_at: row.get(6)?,
        updated_at: row.get(7)?,
        publish_status: row.get(8)?,
        error_code: row.get(9)?,
    })
}

#[cfg(test)]
mod tests;
