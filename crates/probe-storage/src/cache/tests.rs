use super::*;
use crate::migrations;
use rusqlite::types::Value;
use rusqlite::Connection;

const NOW: i64 = 1_700_000_000;
const UTC_STAMP: &str = "2023-11-14T22:13:20";

/// Build a distinct, valid opaque digest from `index`; return its hexadecimal text.
fn digest(index: u64) -> String {
    format!("{index:064x}")
}

/// Wrap caller `payload` in a deterministic sixty-second DNS record; return the record.
fn dns_record(payload: &str) -> DnsCacheRecord {
    DnsCacheRecord {
        value_json: payload.into(),
        stored_at: NOW,
        expires_at: NOW + 60,
    }
}

/// Build a successful lookup for literal `ip` at the test clock; return its record.
fn geo_record(ip: &str) -> GeoCacheRecord {
    GeoCacheRecord {
        ip: ip.into(),
        country: Some("US".into()),
        isp: Some("Example ISP".into()),
        checked_at: NOW,
    }
}

/// Extract IP strings from `records` in order; return an owned request batch.
fn request_ips(records: &[GeoCacheRecord]) -> Vec<String> {
    records.iter().map(|record| record.ip.clone()).collect()
}

/// Positive, negative, and opaque JSON payloads retain their original bytes.
#[test]
fn dns_positive_and_negative_payloads_round_trip_verbatim() {
    let storage = Storage::open_in_memory().unwrap();
    for (index, payload) in [
        r#"{ "status": "NOERROR", "answers": ["192.0.2.1"] }"#,
        r#"{"status":"NXDOMAIN","answers":[]}"#,
        r#"{"status":"NOERROR","answers":[],"negative":true}"#,
        "[]",
        "null",
    ]
    .into_iter()
    .enumerate()
    {
        let key = digest(index as u64);
        let record = dns_record(payload);
        storage.write_dns_cache(&key, &record).unwrap();
        assert_eq!(storage.read_dns_cache(&key, NOW).unwrap(), Some(record));
    }
    assert!(storage.read_dns_cache(&digest(99), NOW).unwrap().is_none());
}

/// DNS freshness includes storage time, excludes expiry, and rejects clock rollback.
#[test]
fn dns_freshness_and_prune_use_exact_boundaries() {
    let storage = Storage::open_in_memory().unwrap();
    let record = dns_record("[]");
    let key = digest(1);
    storage.write_dns_cache(&key, &record).unwrap();
    for (now, is_hit) in [
        (NOW - 1, false),
        (NOW, true),
        (NOW + 59, true),
        (NOW + 60, false),
    ] {
        assert_eq!(storage.read_dns_cache(&key, now).unwrap().is_some(), is_hit);
    }
    let future = DnsCacheRecord {
        stored_at: NOW + 61,
        expires_at: NOW + 120,
        ..record.clone()
    };
    storage.write_dns_cache(&digest(2), &future).unwrap();
    assert_eq!(storage.prune_dns_cache(NOW + 59).unwrap(), 0);
    assert_eq!(storage.prune_dns_cache(NOW + 60).unwrap(), 1);
    assert!(storage
        .read_dns_cache(&digest(2), NOW + 60)
        .unwrap()
        .is_none());
    assert_eq!(
        storage.read_dns_cache(&digest(2), NOW + 61).unwrap(),
        Some(future)
    );
    assert_eq!(storage.prune_dns_cache(NOW + 120).unwrap(), 1);
    assert_eq!(storage.prune_dns_cache(NOW + 120).unwrap(), 0);
}

/// Different caller-derived resolver digests stay isolated when one answer changes.
#[test]
fn dns_resolver_keys_remain_separate_during_upserts() {
    let storage = Storage::open_in_memory().unwrap();
    let first_key = "a".repeat(64);
    let second_key = "B".repeat(64);
    let positive = dns_record(r#"{"answers":["192.0.2.1"]}"#);
    let negative = dns_record(r#"{"answers":[],"rcode":3}"#);
    storage.write_dns_cache(&first_key, &positive).unwrap();
    storage.write_dns_cache(&second_key, &negative).unwrap();
    let replacement = dns_record(r#"{"answers":["2001:db8::1"]}"#);
    storage.write_dns_cache(&first_key, &replacement).unwrap();
    assert_eq!(
        storage.read_dns_cache(&first_key, NOW).unwrap(),
        Some(replacement)
    );
    assert_eq!(
        storage.read_dns_cache(&second_key, NOW).unwrap(),
        Some(negative)
    );
    let count: i64 = storage
        .conn
        .query_row("SELECT COUNT(*) FROM dns_cache", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

/// Reject raw resolver URLs and malformed hashes without exposing their contents in errors.
#[test]
fn dns_invalid_keys_are_rejected_without_echoing_input() {
    let storage = Storage::open_in_memory().unwrap();
    for key in [
        String::new(),
        "a".repeat(63),
        "a".repeat(65),
        "g".repeat(64),
        "\u{e9}".repeat(32),
        format!("{}\0", "a".repeat(63)),
        "https://resolver.invalid/dns-query?private-placeholder".into(),
    ] {
        let write_error = storage
            .write_dns_cache(&key, &dns_record("[]"))
            .unwrap_err();
        let read_error = storage.read_dns_cache(&key, NOW).unwrap_err();
        for error in [write_error, read_error] {
            assert!(error.to_string().contains("64 ASCII hexadecimal"));
            if !key.is_empty() {
                assert!(!error.to_string().contains(&key));
            }
        }
    }
}

/// Invalid write/read clocks cannot replace or prune a previously valid answer.
#[test]
fn dns_invalid_times_are_errors_without_partial_updates() {
    let storage = Storage::open_in_memory().unwrap();
    let key = digest(1);
    let original = dns_record("[]");
    storage.write_dns_cache(&key, &original).unwrap();
    for (stored_at, expires_at) in [
        (-1, NOW + 1),
        (NOW, NOW),
        (NOW, NOW - 1),
        (NOW, i64::MAX),
        (i64::MIN, NOW),
    ] {
        let invalid = DnsCacheRecord {
            stored_at,
            expires_at,
            ..original.clone()
        };
        assert!(storage.write_dns_cache(&key, &invalid).is_err());
    }
    for now in [-1, i64::MIN, MAX_UNIX_TIMESTAMP + 1, i64::MAX] {
        assert!(storage.read_dns_cache(&key, now).is_err());
        assert!(storage.prune_dns_cache(now).is_err());
    }
    assert_eq!(storage.read_dns_cache(&key, NOW).unwrap(), Some(original));
}

/// Both supported Unix-second endpoints work without arithmetic overflow.
#[test]
fn dns_supported_timestamp_extremes_are_usable() {
    let storage = Storage::open_in_memory().unwrap();
    for (index, stored_at, expires_at) in
        [(1, 0, 1), (2, MAX_UNIX_TIMESTAMP - 1, MAX_UNIX_TIMESTAMP)]
    {
        let record = DnsCacheRecord {
            stored_at,
            expires_at,
            ..dns_record("[]")
        };
        let key = digest(index);
        storage.write_dns_cache(&key, &record).unwrap();
        assert_eq!(
            storage.read_dns_cache(&key, stored_at).unwrap(),
            Some(record)
        );
        assert!(storage.read_dns_cache(&key, expires_at).unwrap().is_none());
    }
}

/// Payload limits count UTF-8 bytes, and invalid JSON cannot overwrite a valid row.
#[test]
fn dns_payload_validation_enforces_json_and_byte_bound() {
    let storage = Storage::open_in_memory().unwrap();
    let key = digest(1);
    let maximum_payload = format!(
        "\"{}\"",
        "\u{e9}".repeat((MAX_DNS_CACHE_VALUE_BYTES - 2) / 2)
    );
    assert_eq!(maximum_payload.len(), MAX_DNS_CACHE_VALUE_BYTES);
    let original = dns_record(&maximum_payload);
    storage.write_dns_cache(&key, &original).unwrap();
    assert_eq!(
        storage.read_dns_cache(&key, NOW).unwrap(),
        Some(original.clone())
    );
    for payload in [
        String::new(),
        " ".into(),
        "{".into(),
        "[] trailing".into(),
        "{unquoted:true}".into(),
        "null\0trailing".into(),
        format!("\"{}\"", "\u{e9}".repeat(MAX_DNS_CACHE_VALUE_BYTES / 2)),
    ] {
        assert!(storage
            .write_dns_cache(&key, &dns_record(&payload))
            .is_err());
    }
    assert_eq!(storage.read_dns_cache(&key, NOW).unwrap(), Some(original));
}

/// Foreign writers bypassing constraints still cannot supply a corrupt DNS answer.
#[test]
fn dns_corrupt_or_oversized_payloads_are_read_errors() {
    let storage = Storage::open_in_memory().unwrap();
    let key = digest(1);
    storage.write_dns_cache(&key, &dns_record("[]")).unwrap();
    storage
        .conn
        .execute_batch("PRAGMA ignore_check_constraints = ON")
        .unwrap();
    for payload in [
        Value::Text("{".into()),
        Value::Text(String::new()),
        Value::Text("null\0trailing".into()),
        Value::Blob(b"[]".to_vec()),
        Value::Text(format!("\"{}\"", "x".repeat(MAX_DNS_CACHE_VALUE_BYTES))),
    ] {
        storage
            .conn
            .execute(
                "UPDATE dns_cache SET value_json = ?1 WHERE key = ?2",
                params![payload, key],
            )
            .unwrap();
        let error = storage.read_dns_cache(&key, NOW).unwrap_err();
        assert!(
            error.to_string().contains("corrupt or oversized"),
            "{error}"
        );
        assert!(storage.read_dns_cache(&key, NOW + 60).unwrap().is_none());
        assert!(storage.read_dns_cache(&key, NOW - 1).unwrap().is_none());
    }
}

/// Corrupt stored timestamp types, ranges, and lifetimes are never returned as hits.
#[test]
fn dns_corrupt_timestamps_are_read_errors() {
    let storage = Storage::open_in_memory().unwrap();
    let key = digest(1);
    storage.write_dns_cache(&key, &dns_record("[]")).unwrap();
    storage
        .conn
        .execute_batch("PRAGMA ignore_check_constraints = ON")
        .unwrap();
    for (stored_at, expires_at) in [
        (Value::Integer(-1), Value::Integer(NOW + 60)),
        (Value::Integer(NOW), Value::Integer(NOW)),
        (Value::Integer(NOW + 60), Value::Integer(NOW)),
        (Value::Integer(NOW), Value::Integer(i64::MAX)),
        (Value::Text("invalid".into()), Value::Integer(NOW + 60)),
        (Value::Integer(NOW), Value::Real((NOW + 60) as f64 + 0.5)),
    ] {
        storage
            .conn
            .execute(
                "UPDATE dns_cache SET stored_at = ?1, expires_at = ?2 WHERE key = ?3",
                params![stored_at, expires_at, key],
            )
            .unwrap();
        assert!(storage.read_dns_cache(&key, NOW).is_err());
    }
}

/// Migration-level constraints also protect callers which issue SQL directly.
#[test]
fn dns_schema_rejects_invalid_direct_inserts() {
    let storage = Storage::open_in_memory().unwrap();
    for (key, payload, stored_at, expires_at) in [
        ("not-a-digest".into(), "[]", NOW, NOW + 60),
        (digest(1), "not-json", NOW, NOW + 60),
        (digest(1), "null\0trailing", NOW, NOW + 60),
        (digest(1), "[]", -1, NOW + 60),
        (digest(1), "[]", NOW, NOW),
        (digest(1), "[]", NOW, i64::MAX),
    ] {
        assert!(storage
            .conn
            .execute(
                "INSERT INTO dns_cache(key, value_json, stored_at, expires_at) VALUES(?1, ?2, ?3, ?4)",
                params![key, payload, stored_at, expires_at],
            )
            .is_err());
    }
    let index_count: i64 = storage
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_dns_cache_expires_at'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(index_count, 1);
}

/// Fresh files retain both cache types and a single migration registry across repeated opens.
#[test]
fn caches_survive_repeated_fresh_database_opens() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache.db");
    let dns = dns_record(r#"{"rcode":3,"answers":[]}"#);
    let geo = geo_record("192.0.2.1");
    {
        let storage = Storage::open(&path).unwrap();
        storage.write_dns_cache(&digest(1), &dns).unwrap();
        storage.write_ip_geo(std::slice::from_ref(&geo)).unwrap();
    }
    for _ in 0..3 {
        let storage = Storage::open(&path).unwrap();
        assert_eq!(
            storage.read_dns_cache(&digest(1), NOW).unwrap(),
            Some(dns.clone())
        );
        assert_eq!(
            storage
                .read_ip_geo(std::slice::from_ref(&geo.ip), NOW, 60)
                .unwrap(),
            std::slice::from_ref(&geo)
        );
        let applied = storage.applied_migrations().unwrap();
        assert_eq!(applied.len(), migrations::LATEST_VERSION as usize);
        assert!(applied
            .iter()
            .any(|(version, name, _)| *version == 4 && name == "0004_dns_cache"));
        assert_eq!(applied.last().unwrap().0, migrations::LATEST_VERSION);
        assert_eq!(storage.integrity_check().unwrap(), "ok");
    }
}

/// Legacy Python cache rows and unrelated extension tables survive migration verbatim.
#[test]
fn legacy_python_geo_rows_survive_migrations_and_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("python.db");
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(include_str!(
                "../../../../migrations/0001_python_compat.sql"
            ))
            .unwrap();
        connection
            .execute_batch(
                "INSERT INTO ip_geo(ip, country, isp, checked_at)
               VALUES('192.0.2.1', 'US', 'Example ISP', '2023-11-14T22:13:20');
             INSERT INTO domain_views(domain, views, checked_at)
               VALUES('example.invalid', '{\"legacy\":[]}', '2023-11-14T22:13:20');
             CREATE TABLE python_extension(value TEXT);
             INSERT INTO python_extension VALUES('preserved');",
            )
            .unwrap();
    }
    for _ in 0..2 {
        let storage = Storage::open(&path).unwrap();
        assert_eq!(
            storage.read_ip_geo(&["192.0.2.1".into()], NOW, 60).unwrap(),
            [geo_record("192.0.2.1")]
        );
        let stamp: String = storage
            .conn
            .query_row("SELECT checked_at FROM ip_geo", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stamp, UTC_STAMP);
        let views: String = storage
            .conn
            .query_row("SELECT views FROM domain_views", [], |row| row.get(0))
            .unwrap();
        assert_eq!(views, r#"{"legacy":[]}"#);
        let extension: String = storage
            .conn
            .query_row("SELECT value FROM python_extension", [], |row| row.get(0))
            .unwrap();
        assert_eq!(extension, "preserved");
        assert_eq!(
            storage.applied_migrations().unwrap().len(),
            migrations::LATEST_VERSION as usize
        );
    }
}

/// Geo writes use Python's exact UTC text and round-trip IPv4, IPv6, and optional ISP values.
#[test]
fn geo_rows_round_trip_with_python_timestamp_format() {
    let storage = Storage::open_in_memory().unwrap();
    let records = vec![
        geo_record("192.0.2.1"),
        GeoCacheRecord {
            isp: None,
            ..geo_record("2001:db8::1")
        },
    ];
    assert_eq!(storage.write_ip_geo(&records).unwrap(), 2);
    let mut actual = storage
        .read_ip_geo(&request_ips(&records), NOW, 60)
        .unwrap();
    actual.sort_by(|left, right| left.ip.cmp(&right.ip));
    assert_eq!(actual, records);
    let stamp: String = storage
        .conn
        .query_row("SELECT checked_at FROM ip_geo LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stamp, UTC_STAMP);
    let updated = GeoCacheRecord {
        country: Some("JP".into()),
        isp: None,
        checked_at: NOW + 1,
        ..records[0].clone()
    };
    assert_eq!(
        storage
            .write_ip_geo(std::slice::from_ref(&updated))
            .unwrap(),
        1
    );
    assert_eq!(
        storage
            .read_ip_geo(std::slice::from_ref(&updated.ip), NOW + 1, 60)
            .unwrap(),
        [updated]
    );
}

/// Geo freshness excludes expiry/future dates, and very large TTLs never overflow.
#[test]
fn geo_freshness_handles_boundaries_zero_and_unbounded_max_age() {
    let storage = Storage::open_in_memory().unwrap();
    let records: Vec<_> = [NOW, NOW - 59, NOW - 60, NOW + 1, 0, MAX_UNIX_TIMESTAMP]
        .into_iter()
        .enumerate()
        .map(|(index, checked_at)| GeoCacheRecord {
            checked_at,
            ..geo_record(&format!("192.0.2.{}", index + 1))
        })
        .collect();
    storage.write_ip_geo(&records).unwrap();
    let ips = request_ips(&records);
    let mut fresh = storage.read_ip_geo(&ips, NOW, 60).unwrap();
    fresh.sort_by(|left, right| left.ip.cmp(&right.ip));
    assert_eq!(fresh, records[..2]);
    assert!(storage.read_ip_geo(&ips, NOW, 0).unwrap().is_empty());
    assert_eq!(
        storage.read_ip_geo(&ips, NOW, 1).unwrap(),
        [records[0].clone()]
    );
    assert_eq!(storage.read_ip_geo(&ips, NOW, u64::MAX).unwrap().len(), 4);
    assert_eq!(
        storage
            .read_ip_geo(&ips, MAX_UNIX_TIMESTAMP, u64::MAX)
            .unwrap()
            .len(),
        records.len()
    );
    for now in [-1, i64::MAX] {
        assert!(storage.read_ip_geo(&ips, now, 60).is_err());
    }
}

/// Invalid Python-era timestamps and countries are misses, not repaired or promoted to hits.
#[test]
fn geo_invalid_legacy_rows_and_invalid_requested_ips_are_skipped() {
    let storage = Storage::open_in_memory().unwrap();
    let valid = geo_record("192.0.2.1");
    storage.write_ip_geo(std::slice::from_ref(&valid)).unwrap();
    let mut ips = vec![valid.ip.clone(), String::new(), "not-an-ip".into()];
    for (index, (country, stamp)) in [
        (Some("US"), None),
        (Some("US"), Some("")),
        (Some("US"), Some("not-a-timestamp")),
        (Some("US"), Some("2023-11-14 22:13:20")),
        (Some("US"), Some("2023-11-14T22:13:20Z")),
        (Some("US"), Some("2023-11-14T22:13:20.5")),
        (Some("US"), Some("2023-11-14T22:13:20+00:00")),
        (Some("US"), Some("2023-02-29T00:00:00")),
        (Some("US"), Some("2023-11-14T24:00:00")),
        (Some("US"), Some("1969-12-31T23:59:59")),
        (Some("US"), Some("2023-11-14T22:13:20\0")),
        (None, Some(UTC_STAMP)),
        (Some(""), Some(UTC_STAMP)),
        (Some("us"), Some(UTC_STAMP)),
        (Some("USA"), Some(UTC_STAMP)),
        (Some("ZZ"), Some(UTC_STAMP)),
    ]
    .into_iter()
    .enumerate()
    {
        let ip = format!("2001:db8::{index:x}");
        storage
            .conn
            .execute(
                "INSERT INTO ip_geo(ip, country, checked_at) VALUES(?1, ?2, ?3)",
                params![ip, country, stamp],
            )
            .unwrap();
        ips.push(ip);
    }
    storage.conn.execute_batch(
        "INSERT INTO ip_geo(ip, country, checked_at) VALUES('not-an-ip', 'US', '2023-11-14T22:13:20');
         INSERT INTO ip_geo(ip, country, isp, checked_at)
           VALUES('192.0.2.2', 'US', x'0001', '2023-11-14T22:13:20');
         INSERT INTO ip_geo(ip, country, checked_at) VALUES('192.0.2.3', x'5553', '2023-11-14T22:13:20');
         INSERT INTO ip_geo(ip, country, checked_at) VALUES('192.0.2.4', 'US', x'0001');",
    ).unwrap();
    ips.extend(["192.0.2.2".into(), "192.0.2.3".into(), "192.0.2.4".into()]);
    assert_eq!(
        storage
            .read_ip_geo(&ips, MAX_UNIX_TIMESTAMP, u64::MAX)
            .unwrap(),
        [valid]
    );
}

/// Malformed UTF-8 in a legacy text column must not hide neighboring valid rows.
#[test]
fn geo_invalid_utf8_text_is_skipped_without_failing_the_batch() {
    let storage = Storage::open_in_memory().unwrap();
    let valid = geo_record("192.0.2.1");
    storage.write_ip_geo(std::slice::from_ref(&valid)).unwrap();
    storage
        .conn
        .execute_batch(
            "INSERT INTO ip_geo(ip, country, isp, checked_at)
               VALUES('192.0.2.2', 'US', CAST(x'80' AS TEXT), '2023-11-14T22:13:20');
             INSERT INTO ip_geo(ip, country, checked_at)
               VALUES('192.0.2.3', CAST(x'80' AS TEXT), '2023-11-14T22:13:20');",
        )
        .unwrap();
    let ips = ["192.0.2.1".into(), "192.0.2.2".into(), "192.0.2.3".into()];
    assert_eq!(storage.read_ip_geo(&ips, NOW, 60).unwrap(), [valid]);
}

/// The known-country allowlist stays sorted for binary search and excludes placeholder codes.
#[test]
fn geo_country_codes_are_assigned_and_sorted() {
    assert_eq!(COUNTRY_CODES.len(), 249);
    assert!(COUNTRY_CODES.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(COUNTRY_CODES
        .iter()
        .all(|code| code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_uppercase())));
    for code in ["CN", "HK", "US", "JP", "GB"] {
        assert!(is_country_code(code));
    }
    for code in ["", "us", "USA", "U1", "ZZ", "XX", "AA", "\u{e9}\u{e9}"] {
        assert!(!is_country_code(code));
    }
}

/// A missing country never inserts a failed lookup or replaces previously known data.
#[test]
fn geo_unknown_results_are_noops_and_duplicates_use_last_known_result() {
    let storage = Storage::open_in_memory().unwrap();
    let known = geo_record("192.0.2.1");
    storage.write_ip_geo(std::slice::from_ref(&known)).unwrap();
    let unknown = GeoCacheRecord {
        country: None,
        checked_at: NOW + 1,
        ..known.clone()
    };
    let missing = GeoCacheRecord {
        ip: "192.0.2.2".into(),
        ..unknown.clone()
    };
    assert_eq!(
        storage
            .write_ip_geo(&[unknown.clone(), missing.clone()])
            .unwrap(),
        0
    );
    assert_eq!(
        storage
            .read_ip_geo(&[known.ip.clone(), missing.ip], NOW + 1, 60)
            .unwrap(),
        std::slice::from_ref(&known)
    );
    let second = GeoCacheRecord {
        country: Some("JP".into()),
        ..known.clone()
    };
    let last = GeoCacheRecord {
        country: Some("DE".into()),
        ..known
    };
    assert_eq!(
        storage
            .write_ip_geo(&[second, last.clone(), unknown])
            .unwrap(),
        2
    );
    assert_eq!(
        storage
            .read_ip_geo(std::slice::from_ref(&last.ip), NOW, 60)
            .unwrap(),
        [last]
    );
}

/// Invalid later rows roll back both updates and inserts already attempted in the batch.
#[test]
fn geo_invalid_batch_rows_roll_back_all_prior_writes() {
    let storage = Storage::open_in_memory().unwrap();
    let original = geo_record("192.0.2.1");
    storage
        .write_ip_geo(std::slice::from_ref(&original))
        .unwrap();
    let updated = GeoCacheRecord {
        country: Some("JP".into()),
        ..original.clone()
    };
    let new = geo_record("192.0.2.2");
    let mut invalid = Vec::new();
    for ip in [
        "",
        "example.invalid",
        "192.0.2.256",
        "192.0.2.1:53",
        "[2001:db8::1]",
        "fe80::1%eth0",
    ] {
        invalid.push(geo_record(ip));
    }
    for country in ["", "us", "USA", "U1", "ZZ", "XX", "AA"] {
        invalid.push(GeoCacheRecord {
            country: Some(country.into()),
            ..geo_record("192.0.2.3")
        });
    }
    for checked_at in [-1, MAX_UNIX_TIMESTAMP + 1, i64::MAX] {
        invalid.push(GeoCacheRecord {
            checked_at,
            ..geo_record("192.0.2.3")
        });
    }
    for record in invalid {
        assert!(storage
            .write_ip_geo(&[updated.clone(), new.clone(), record])
            .is_err());
        assert_eq!(
            storage
                .read_ip_geo(&[original.ip.clone(), new.ip.clone()], NOW, 60)
                .unwrap(),
            std::slice::from_ref(&original)
        );
    }
}

/// An actual SQLite failure rolls back the entire transaction and leaves the connection usable.
#[test]
fn geo_sql_failure_rolls_back_upserts_and_inserts() {
    let storage = Storage::open_in_memory().unwrap();
    let original = geo_record("192.0.2.1");
    storage
        .write_ip_geo(std::slice::from_ref(&original))
        .unwrap();
    let updated = GeoCacheRecord {
        country: Some("JP".into()),
        ..original.clone()
    };
    let records = vec![updated, geo_record("192.0.2.2"), geo_record("192.0.2.3")];
    storage
        .conn
        .execute_batch(
            "CREATE TRIGGER poison_geo BEFORE INSERT ON ip_geo
         WHEN NEW.ip = '192.0.2.3'
         BEGIN SELECT RAISE(ABORT, 'poison geo row'); END;",
        )
        .unwrap();
    let error = storage.write_ip_geo(&records).unwrap_err();
    assert!(error.to_string().contains("poison geo row"));
    assert_eq!(
        storage
            .read_ip_geo(&request_ips(&records), NOW, 60)
            .unwrap(),
        [original]
    );
    storage
        .conn
        .execute_batch("DROP TRIGGER poison_geo")
        .unwrap();
    assert_eq!(storage.write_ip_geo(&records).unwrap(), 3);
    assert_eq!(
        storage
            .read_ip_geo(&request_ips(&records), NOW, 60)
            .unwrap()
            .len(),
        3
    );
}

/// Large, repeated IP input fits even a legacy SQLite connection capped at 999 variables.
#[test]
fn geo_large_batches_are_chunked_and_deduplicated() {
    let storage = Storage::open_in_memory().unwrap();
    // SAFETY: this test exclusively owns the connection and has no active statements.
    unsafe {
        rusqlite::ffi::sqlite3_limit(
            storage.conn.handle(),
            rusqlite::ffi::SQLITE_LIMIT_VARIABLE_NUMBER,
            999,
        );
    }
    let records: Vec<_> = (0..2405)
        .map(|index| geo_record(&format!("2001:db8::{index:x}")))
        .collect();
    assert_eq!(storage.write_ip_geo(&records).unwrap(), records.len());
    let mut ips = request_ips(&records);
    ips.extend(request_ips(&records));
    ips.extend(request_ips(&records));
    let result = storage.read_ip_geo(&ips, NOW, 60).unwrap();
    let unique: HashSet<_> = result.iter().map(|record| record.ip.as_str()).collect();
    assert_eq!(result.len(), records.len());
    assert_eq!(unique.len(), records.len());
    assert!(records
        .iter()
        .all(|record| unique.contains(record.ip.as_str())));
}

/// Empty batches do not require schema access, a transaction, or a clock.
#[test]
fn empty_geo_batches_are_noops_without_tables() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open_without_migrating(&directory.path().join("empty.db")).unwrap();
    assert_eq!(storage.write_ip_geo(&[]).unwrap(), 0);
    assert!(storage
        .read_ip_geo(&[], i64::MIN, u64::MAX)
        .unwrap()
        .is_empty());
    assert!(storage
        .read_ip_geo(&["not-an-ip".into(), String::new()], NOW, 60)
        .unwrap()
        .is_empty());
}

/// Operational SQLite errors remain errors rather than masquerading as cache misses.
#[test]
fn missing_cache_tables_are_reported_explicitly() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open_without_migrating(&directory.path().join("empty.db")).unwrap();
    assert!(storage.read_dns_cache(&digest(1), NOW).is_err());
    assert!(storage
        .write_dns_cache(&digest(1), &dns_record("[]"))
        .is_err());
    assert!(storage.prune_dns_cache(NOW).is_err());
    assert!(storage.read_ip_geo(&["192.0.2.1".into()], NOW, 60).is_err());
    assert!(storage.write_ip_geo(&[geo_record("192.0.2.1")]).is_err());
}
