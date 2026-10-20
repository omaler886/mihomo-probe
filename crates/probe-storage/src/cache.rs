//! Persistent caches with explicit UTC freshness checks.
//!
//! Invalid arguments, corrupt DNS rows, and SQLite failures return
//! [`DomainError::Storage`]. Missing, expired, or future-dated DNS rows are
//! misses. Invalid legacy geo rows are skipped, as are unknown countries.
//! JSON structure and DNS key derivation remain the caller's responsibility.

use std::collections::HashSet;
use std::net::IpAddr;

use probe_domain::{DomainError, DomainResult};
use rusqlite::{params, params_from_iter, OptionalExtension};

use crate::Storage;

/// Maximum UTF-8 payload size; also enforced by migration 0004.
pub const MAX_DNS_CACHE_VALUE_BYTES: usize = 1024 * 1024;

// Four-digit UTC years keep SQLite and Python's timestamp representation equal.
const MAX_UNIX_TIMESTAMP: i64 = 253_402_300_799;
// Match Python's conservative limit, including SQLite builds capped at 999.
const IP_QUERY_CHUNK_SIZE: usize = 400;

pub use probe_domain::geo::is_country_code;
#[cfg(test)]
use probe_domain::geo::COUNTRY_CODES;

/// A caller-serialized DNS answer with UTC Unix-second freshness bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsCacheRecord {
    pub value_json: String,
    pub stored_at: i64,
    pub expires_at: i64,
}

/// An IP lookup, converted to/from Python's UTC `checked_at` text on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeoCacheRecord {
    pub ip: String,
    pub country: Option<String>,
    pub isp: Option<String>,
    pub checked_at: i64,
}

impl Storage {
    /// Read `key` at UTC `now`; return an answer only for `stored_at <= now < expires_at`.
    ///
    /// Keys are exactly 64 ASCII hex characters, preserved without normalization.
    /// Missing/expired/future rows return `None`; malformed timestamps, oversized
    /// payloads, or invalid JSON return a storage error, never a usable answer.
    pub fn read_dns_cache(&self, key: &str, now: i64) -> DomainResult<Option<DnsCacheRecord>> {
        validate_dns_key(key)?;
        validate_timestamp(now)?;
        // Check the byte bound before returning a payload to Rust, even if a
        // foreign writer bypassed the table constraints.
        let entry = self
            .conn
            .query_row(
                "SELECT stored_at, expires_at,
                 CASE WHEN typeof(value_json) = 'text'
                   AND length(CAST(value_json AS BLOB)) BETWEEN 1 AND ?2
                   AND instr(value_json, char(0)) = 0
                 THEN CASE WHEN json_valid(value_json) THEN value_json END END
                 FROM dns_cache WHERE key = ?1",
                params![key, MAX_DNS_CACHE_VALUE_BYTES as i64],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| DomainError::Storage(format!("dns cache read: {error}")))?;
        let Some((stored_at, expires_at, value_json)) = entry else {
            return Ok(None);
        };
        validate_dns_times(stored_at, expires_at)?;
        if now < stored_at || now >= expires_at {
            return Ok(None);
        }
        let value_json = value_json.ok_or_else(|| {
            DomainError::Storage("dns cache read: corrupt or oversized JSON payload".into())
        })?;
        Ok(Some(DnsCacheRecord {
            value_json,
            stored_at,
            expires_at,
        }))
    }

    /// Upsert `record` under an opaque digest `key`; return an error for invalid input.
    ///
    /// Payload JSON is validated but never reserialized. Timestamps must be in
    /// `0..=253402300799` and expiry must follow storage time. Future timestamps
    /// can be persisted, but reads cannot use them until the caller's clock catches up.
    pub fn write_dns_cache(&self, key: &str, record: &DnsCacheRecord) -> DomainResult<()> {
        validate_dns_key(key)?;
        validate_dns_times(record.stored_at, record.expires_at)?;
        if record.value_json.is_empty() || record.value_json.len() > MAX_DNS_CACHE_VALUE_BYTES {
            return Err(DomainError::Storage(
                "dns cache write: payload must contain 1..=1048576 UTF-8 bytes".into(),
            ));
        }
        let is_valid_json: bool = self
            .conn
            .query_row("SELECT json_valid(?1)", [&record.value_json], |row| {
                row.get(0)
            })
            .map_err(|error| DomainError::Storage(format!("dns cache validation: {error}")))?;
        if !is_valid_json || record.value_json.contains('\0') {
            return Err(DomainError::Storage(
                "dns cache write: payload must be valid JSON without NUL bytes".into(),
            ));
        }
        self.conn
            .execute(
                "INSERT INTO dns_cache(key, value_json, stored_at, expires_at)
                 VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json,
                   stored_at = excluded.stored_at, expires_at = excluded.expires_at",
                params![key, record.value_json, record.stored_at, record.expires_at],
            )
            .map_err(|error| DomainError::Storage(format!("dns cache write: {error}")))?;
        Ok(())
    }

    /// Delete DNS rows with `expires_at <= now` (UTC seconds); return the deleted count.
    ///
    /// Future-dated, not-yet-expired rows are retained but remain unreadable.
    pub fn prune_dns_cache(&self, now: i64) -> DomainResult<usize> {
        validate_timestamp(now)?;
        self.conn
            .execute("DELETE FROM dns_cache WHERE expires_at <= ?1", [now])
            .map_err(|error| DomainError::Storage(format!("dns cache prune: {error}")))
    }

    /// Read known-country rows for `ips` with `0 <= now - checked_at < max_age_secs`.
    ///
    /// UTC `now` must be in the supported timestamp range. Duplicate/invalid IP
    /// inputs and invalid/future/expired legacy rows are skipped. A zero max age
    /// disables hits; empty input is a no-op. Output order is unspecified.
    pub fn read_ip_geo(
        &self,
        ips: &[String],
        now: i64,
        max_age_secs: u64,
    ) -> DomainResult<Vec<GeoCacheRecord>> {
        if ips.is_empty() {
            return Ok(Vec::new());
        }
        validate_timestamp(now)?;
        if max_age_secs == 0 {
            return Ok(Vec::new());
        }
        let mut seen = HashSet::new();
        let unique: Vec<&str> = ips
            .iter()
            .map(String::as_str)
            .filter(|ip| ip.parse::<IpAddr>().is_ok() && seen.insert(*ip))
            .collect();
        let mut records = Vec::new();
        for chunk in unique.chunks(IP_QUERY_CHUNK_SIZE) {
            records.extend(self.read_geo_chunk(chunk, now, max_age_secs)?);
        }
        Ok(records)
    }

    /// Upsert `records` in one transaction; return the number of known-country writes.
    ///
    /// IPs must be literals, countries assigned uppercase ISO alpha-2 codes, and
    /// times valid UTC seconds. `country: None` is skipped, so a failed lookup
    /// cannot overwrite a known country. Duplicate IPs use the last known result.
    /// Any invalid row or SQLite failure rolls back the whole batch; empty input
    /// returns zero without opening a transaction.
    pub fn write_ip_geo(&self, records: &[GeoCacheRecord]) -> DomainResult<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        let transaction = self
            .conn
            .unchecked_transaction()
            .map_err(|error| DomainError::Storage(format!("ip geo transaction: {error}")))?;
        let mut written = 0;
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO ip_geo(ip, country, isp, checked_at)
                     VALUES(?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%S', ?4, 'unixepoch'))
                     ON CONFLICT(ip) DO UPDATE SET country = excluded.country,
                       isp = excluded.isp, checked_at = excluded.checked_at",
                )
                .map_err(|error| DomainError::Storage(format!("ip geo prepare: {error}")))?;
            for record in records {
                validate_geo_record(record)?;
                if record.country.is_none() {
                    continue;
                }
                written += statement
                    .execute(params![
                        record.ip,
                        record.country,
                        record.isp,
                        record.checked_at
                    ])
                    .map_err(|error| DomainError::Storage(format!("ip geo write: {error}")))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| DomainError::Storage(format!("ip geo commit: {error}")))?;
        Ok(written)
    }

    /// Read a bounded, unique IP `chunk`; return rows fresh at UTC `now` under `max_age_secs`.
    fn read_geo_chunk(
        &self,
        chunk: &[&str],
        now: i64,
        max_age_secs: u64,
    ) -> DomainResult<Vec<GeoCacheRecord>> {
        let placeholders = vec!["?"; chunk.len()].join(",");
        // A normalized round-trip rejects malformed dates, offsets, fractions,
        // and calendar overflow instead of letting SQLite silently repair them.
        let sql = format!(
            "SELECT ip, country,
               CASE WHEN length(CAST(isp AS BLOB)) <= 1024 THEN isp END,
               CAST(strftime('%s', checked_at) AS INTEGER)
             FROM ip_geo WHERE ip IN ({placeholders})
               AND typeof(ip) = 'text' AND typeof(country) = 'text'
               AND length(CAST(country AS BLOB)) = 2
               AND typeof(isp) IN ('text', 'null') AND typeof(checked_at) = 'text'
               AND length(CAST(checked_at AS BLOB)) = 19
               AND checked_at = strftime('%Y-%m-%dT%H:%M:%S', checked_at, '+0 seconds')"
        );
        let mut statement = self
            .conn
            .prepare(&sql)
            .map_err(|error| DomainError::Storage(format!("ip geo read: {error}")))?;
        let rows = statement
            .query_map(params_from_iter(chunk.iter().copied()), read_geo_row)
            .map_err(|error| DomainError::Storage(format!("ip geo read: {error}")))?;
        let mut records = Vec::new();
        for row in rows {
            let Some(record) =
                row.map_err(|error| DomainError::Storage(format!("ip geo read: {error}")))?
            else {
                continue;
            };
            let is_known_country = record.country.as_deref().is_some_and(is_country_code);
            if is_known_country
                && (0..=now).contains(&record.checked_at)
                && ((now - record.checked_at) as u64) < max_age_secs
            {
                records.push(record);
            }
        }
        Ok(records)
    }
}

/// Decode a geo SQL `row`; skip corrupt field values but propagate unrelated SQLite errors.
fn read_geo_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<GeoCacheRecord>> {
    let record = (|| {
        Ok(GeoCacheRecord {
            ip: row.get(0)?,
            country: row.get(1)?,
            isp: row.get(2)?,
            checked_at: row.get(3)?,
        })
    })();
    match record {
        Ok(record) => Ok(Some(record)),
        Err(
            rusqlite::Error::InvalidColumnType(..) | rusqlite::Error::FromSqlConversionFailure(..),
        ) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Validate opaque `key` without echoing possible secrets; return an error on malformed digests.
fn validate_dns_key(key: &str) -> DomainResult<()> {
    if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DomainError::Storage(
            "dns cache: key must be exactly 64 ASCII hexadecimal characters".into(),
        ));
    }
    Ok(())
}

/// Validate UTC Unix `timestamp` for Python/SQLite compatibility; return an error out of range.
fn validate_timestamp(timestamp: i64) -> DomainResult<()> {
    if !(0..=MAX_UNIX_TIMESTAMP).contains(&timestamp) {
        return Err(DomainError::Storage(
            "cache timestamp must be UTC Unix seconds in 0..=253402300799".into(),
        ));
    }
    Ok(())
}

/// Validate DNS `stored_at` and `expires_at`; return an error for invalid or empty lifetimes.
fn validate_dns_times(stored_at: i64, expires_at: i64) -> DomainResult<()> {
    validate_timestamp(stored_at)?;
    validate_timestamp(expires_at)?;
    if stored_at >= expires_at {
        return Err(DomainError::Storage(
            "dns cache: expires_at must be greater than stored_at".into(),
        ));
    }
    Ok(())
}

/// Validate a geo `record` before any write; return an error without including lookup data.
fn validate_geo_record(record: &GeoCacheRecord) -> DomainResult<()> {
    if record.isp.as_ref().is_some_and(|isp| isp.len() > 1024) {
        return Err(DomainError::Storage(
            "ip geo write: ISP metadata exceeds 1024 bytes".into(),
        ));
    }
    if record.ip.parse::<IpAddr>().is_err() {
        return Err(DomainError::Storage(
            "ip geo write: ip must be an IPv4 or IPv6 literal".into(),
        ));
    }
    validate_timestamp(record.checked_at)?;
    if record
        .country
        .as_deref()
        .is_some_and(|country| !is_country_code(country))
    {
        return Err(DomainError::Storage(
            "ip geo write: country must be an assigned uppercase ISO alpha-2 code".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
