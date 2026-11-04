//! The Sub-Store 测活产物 cleanup leg (B8): scan, classify, back up, delete,
//! restore.
//!
//! The refactor's rule (F-001) is that the *pipeline* never writes to
//! Sub-Store: a round only reads, and the one legacy write path that remains
//! is doubly gated and update-only. Removing what the OLD pipeline left
//! behind is a different operation -- the `-local` push artifacts, the manual
//! front pool, the ipmap mapping subs are all objects this system itself once
//! created, and F-084's stage 4 explicitly reserves their removal for a
//! user-confirmed step. This module implements that step behind its own
//! trait, so the F-001 surfaces stay structurally write-free:
//!
//! * [`crate::migrate::MigrationStore`] still has no delete method.
//! * [`CleanupStore`] is the ONLY place a Sub-Store DELETE exists, and every
//!   caller that reaches it must already hold both gates
//!   (`substore.allow_delete` AND the `ALLOW_SUBSTORE_DELETE` environment
//!   override, merged at config load) plus the explicit dry-run opt-in
//!   (`--execute` / `execute: true`). A plan or a dry run only ever GETs.
//!
//! Every deletion is preceded by a full local backup of the object's record,
//! and [`rollback_cleanup`] recreates objects from such a backup through the
//! upsert path. The scan never writes. Subs only: collections are the
//! operator's own data and out of scope by construction.
//!
//! Error text is bounded and backend-free at the HTTP layer (see `fetch.rs`);
//! the backend URL is itself a credential and never reaches a report.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fetch::{SubAdmin, SubEntry, SubStoreClient};
use crate::migrate::now_rfc3339;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The store the cleanup leg talks to. Deliberately separate from both
/// [`crate::fetch::SubAdmin`] (which cannot enumerate safely on its own) and
/// [`crate::migrate::MigrationStore`] (which structurally cannot delete):
/// holding one of those does not mean holding this.
pub trait CleanupStore: Send + Sync {
    /// Every sub the backend holds. Without the list there is nothing safe
    /// to classify, let alone delete.
    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SubEntry>, String>>;
    /// One sub's raw record, `None` when it is not there.
    fn record<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Value>, String>>;
    /// Remove a sub; `Ok(false)` = it was already gone.
    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<bool, String>>;
    /// Create-or-replace, the restore path for [`rollback_cleanup`].
    fn upsert<'a>(
        &'a self,
        name: &'a str,
        payload: &'a Value,
    ) -> BoxFuture<'a, Result<String, String>>;
}

impl CleanupStore for SubStoreClient {
    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SubEntry>, String>> {
        Box::pin(SubAdmin::list_subs(self))
    }

    fn record<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Value>, String>> {
        Box::pin(self.sub_record(name))
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(SubAdmin::delete_sub(self, name))
    }

    fn upsert<'a>(
        &'a self,
        name: &'a str,
        payload: &'a Value,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(SubAdmin::upsert_sub(self, name, payload))
    }
}

/// What the scan looks for. Both prefixes are name prefixes as the old
/// pipeline spelled them: the push artifacts are `<push_prefix>-<key>-local`
/// plus the `<push_prefix>-front-manual` pool, the ipmap artifacts are
/// `<ipmap_prefix>*` confirmed by their display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupRules {
    /// `publish.prefix` (no trailing dash; the dash is added here).
    pub push_prefix: String,
    /// The ipmap push naming (`--push-sub` names are operator-chosen, so this
    /// prefix alone is only ever *suggestive* -- the record's display name
    /// decides).
    pub ipmap_prefix: String,
}

impl Default for CleanupRules {
    fn default() -> Self {
        Self {
            push_prefix: "probe".to_string(),
            ipmap_prefix: DEFAULT_IPMAP_PREFIX.to_string(),
        }
    }
}

/// The ipmap push prefix (`mihomo_test/ipmap.py` examples all use it).
pub const DEFAULT_IPMAP_PREFIX: &str = "ipmap-";

/// The display-name marker `ipmap.py` writes (`ipmap 实测映射(<key>)`).
pub const IPMAP_DISPLAY: &str = "ipmap 实测映射";

/// The manual-front sub the legacy chain pool materialized
/// (`mihomo_test/engine.py` `MANUAL_FRONT_SUFFIX`).
pub const MANUAL_FRONT_SUFFIX: &str = "-front-manual";

/// The legacy push-artifact suffix (`<prefix>-<key>-local`).
pub const LOCAL_SUFFIX: &str = "-local";

/// Scan caps: how many records a single scan may fetch to confirm a
/// suggestive name. Candidates beyond the cap fall to `uncertain` and are
/// never removed by a plan built from this scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupCaps {
    pub record_fetch_cap: usize,
}

impl Default for CleanupCaps {
    fn default() -> Self {
        Self {
            record_fetch_cap: 50,
        }
    }
}

/// One removal candidate: the name and the rule that named it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupTarget {
    pub name: String,
    pub reason: String,
}

/// One sub the scan refuses to decide about. Reported, never removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupUncertain {
    pub name: String,
    pub why: String,
}

/// Per-class counts, duplicated from the vectors so a report reader does not
/// have to walk them (the GUI's cleanup page counts by key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupCounts {
    pub keep: usize,
    pub duplicate: usize,
    pub uncertain: usize,
}

/// The scan's output: everything the backend holds, classified. Serializes
/// redacted-only data -- names and reasons, never records or URLs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub generated_at: String,
    pub push_prefix: String,
    pub ipmap_prefix: String,
    pub counts: CleanupCounts,
    /// Subs the scan leaves alone, names only.
    pub keep: Vec<String>,
    /// Removal candidates. Nothing is removed until an apply says so.
    pub duplicate: Vec<CleanupTarget>,
    pub uncertain: Vec<CleanupUncertain>,
    /// The scan's own caps, so the plan explains its own blindness.
    pub record_fetch_cap: usize,
    /// Subs only -- collections were never listed, let alone classified.
    pub scope: String,
}

/// A deletion that did not happen, with the bounded reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupFailure {
    pub name: String,
    pub reason: String,
}

/// What one apply did. `dry_run: true` reports the plan's size and nothing
/// else; a real apply lists what left, what was skipped (a target whose
/// record could not be backed up is never deleted), and the backup file the
/// rollback reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupApplyReport {
    pub dry_run: bool,
    pub planned: usize,
    pub deleted: Vec<String>,
    pub skipped: Vec<CleanupFailure>,
    pub failed: Vec<CleanupFailure>,
    /// File NAME (no path) under the service's `data/cleanup-backups/`.
    /// `None` for a dry run or an apply with nothing to back up.
    pub backup_file: Option<String>,
}

/// What one rollback did: every backup entry was offered back to the store
/// through the upsert path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupRollbackReport {
    pub restored: Vec<String>,
    pub failed: Vec<CleanupFailure>,
}

/// One backed-up object: the name and the full record as it was read
/// immediately before the delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BackupEntry {
    name: String,
    record: Value,
}

/// The on-disk backup document. The schema tag keeps a future change from
/// silently misreading an older file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BackupDocument {
    schema: &'static str,
    created_at: String,
    push_prefix: String,
    entries: Vec<BackupEntry>,
}

const BACKUP_SCHEMA: &str = "cleanup-backup/1";

/// The internal three-way verdict for one listed sub.
enum Verdict {
    /// A name-only rule matched: remove with this reason, no fetch needed.
    Remove(&'static str),
    /// The name suggests one of ours but a required field disagrees.
    Uncertain(&'static str),
    /// The name is suggestive (ipmap prefix); the record decides.
    Record,
    Keep,
}

fn classify(entry: &SubEntry, rules: &CleanupRules) -> Verdict {
    let scope = format!("{}-", rules.push_prefix.trim_end_matches('-'));
    let name = entry.name.as_str();
    if name.starts_with(&scope) && name.ends_with(LOCAL_SUFFIX) {
        return if entry.source.as_deref() == Some("local") {
            Verdict::Remove("旧测活推送产物 (<prefix>-*-local, source=local)")
        } else {
            Verdict::Uncertain("命名像推送产物，但 source 字段不是 local")
        };
    }
    if name
        == format!(
            "{}{}",
            rules.push_prefix.trim_end_matches('-'),
            MANUAL_FRONT_SUFFIX
        )
    {
        return Verdict::Remove("manual front 池 (<prefix>-front-manual)");
    }
    if name.starts_with(&rules.ipmap_prefix) {
        return Verdict::Record;
    }
    Verdict::Keep
}

/// `display` is Sub-Store's `displayName` field; absent or wrong means the
/// sub only *looks* like ours, which is exactly what `uncertain` is for.
fn display_name(record: &Value) -> Option<&str> {
    record.get("displayName").and_then(Value::as_str)
}

/// Scan the backend and classify everything it lists. Read-only by
/// construction: `list` and `record` are both GETs.
pub async fn scan_cleanup(
    store: &dyn CleanupStore,
    rules: &CleanupRules,
    caps: CleanupCaps,
) -> Result<CleanupPlan, String> {
    let entries = store
        .list()
        .await
        .map_err(|err| crate::fetch::bounded(&err))?;
    let mut keep = Vec::new();
    let mut duplicate = Vec::new();
    let mut uncertain = Vec::new();
    // Record-confirmed candidates are resolved in listed order; the cap
    // bounds the fetches, and whatever it leaves undecided stays undecided.
    let mut record_checks: Vec<String> = Vec::new();
    for entry in &entries {
        match classify(entry, rules) {
            Verdict::Remove(reason) => duplicate.push(CleanupTarget {
                name: entry.name.clone(),
                reason: reason.to_string(),
            }),
            Verdict::Uncertain(why) => uncertain.push(CleanupUncertain {
                name: entry.name.clone(),
                why: why.to_string(),
            }),
            Verdict::Record => record_checks.push(entry.name.clone()),
            Verdict::Keep => keep.push(entry.name.clone()),
        }
    }
    for (index, name) in record_checks.iter().enumerate() {
        if index >= caps.record_fetch_cap {
            uncertain.push(CleanupUncertain {
                name: name.clone(),
                why: format!("超出单次扫描的记录核验上限 {}", caps.record_fetch_cap),
            });
            continue;
        }
        match store.record(name).await {
            Ok(Some(record)) => match display_name(&record) {
                Some(display) if display.starts_with(IPMAP_DISPLAY) => {
                    duplicate.push(CleanupTarget {
                        name: name.clone(),
                        reason: format!("ipmap 实测映射产物 (displayName={display:?})"),
                    });
                }
                _ => uncertain.push(CleanupUncertain {
                    name: name.clone(),
                    why: "名字带 ipmap 前缀，但 displayName 不是本系统的标注".to_string(),
                }),
            },
            Ok(None) => uncertain.push(CleanupUncertain {
                name: name.clone(),
                why: "扫描时在列表里，取记录时已经不在".to_string(),
            }),
            Err(err) => uncertain.push(CleanupUncertain {
                name: name.clone(),
                why: crate::fetch::bounded(&err),
            }),
        }
    }
    let counts = CleanupCounts {
        keep: keep.len(),
        duplicate: duplicate.len(),
        uncertain: uncertain.len(),
    };
    Ok(CleanupPlan {
        generated_at: now_rfc3339(),
        push_prefix: rules.push_prefix.clone(),
        ipmap_prefix: rules.ipmap_prefix.clone(),
        counts,
        keep,
        duplicate,
        uncertain,
        record_fetch_cap: caps.record_fetch_cap,
        scope: "subs".to_string(),
    })
}

/// The backup directory under a service root: `data/cleanup-backups/`.
pub fn backup_dir(root: &Path) -> std::path::PathBuf {
    root.join("data").join("cleanup-backups")
}

/// Apply a plan: back every target up locally, then delete. `execute: false`
/// is a dry run -- the report says what WOULD happen and the store is never
/// asked to write.
///
/// The backup-before-delete contract is strict: a target whose record cannot
/// be read is *skipped*, never deleted -- the rollback's value is exactly the
/// ability to restore what was removed, and removing what could not be backed
/// up would break it. A delete error fails that target alone; the rest still
/// go through.
pub async fn apply_cleanup(
    plan: &CleanupPlan,
    store: &dyn CleanupStore,
    backup_dir: &Path,
    execute: bool,
) -> Result<CleanupApplyReport, String> {
    if !execute || plan.duplicate.is_empty() {
        return Ok(CleanupApplyReport {
            dry_run: !execute,
            planned: plan.duplicate.len(),
            deleted: Vec::new(),
            skipped: Vec::new(),
            failed: Vec::new(),
            backup_file: None,
        });
    }

    // Pass one: read every record. A failure here is a skip, not a delete.
    let mut entries: Vec<BackupEntry> = Vec::new();
    let mut skipped = Vec::new();
    for target in &plan.duplicate {
        match store.record(&target.name).await {
            Ok(Some(record)) => entries.push(BackupEntry {
                name: target.name.clone(),
                record,
            }),
            Ok(None) => skipped.push(CleanupFailure {
                name: target.name.clone(),
                reason: "取记录时对象已不存在，无需删除".to_string(),
            }),
            Err(err) => skipped.push(CleanupFailure {
                name: target.name.clone(),
                reason: crate::fetch::bounded(&format!("记录不可读，拒绝删除: {err}")),
            }),
        }
    }

    // Pass two: write the backup to disk. An unwritable backup aborts the
    // whole apply BEFORE the first delete: deleting without a restore path
    // is the one thing this module must never do.
    let mut backup_file = None;
    if !entries.is_empty() {
        let file_name = format!("cleanup-{}.json", now_rfc3339().replace(['-', ':'], ""));
        let path = backup_dir.join(&file_name);
        let document = BackupDocument {
            schema: BACKUP_SCHEMA,
            created_at: now_rfc3339(),
            push_prefix: plan.push_prefix.clone(),
            entries: entries.clone(),
        };
        let text = serde_json::to_string_pretty(&document)
            .map_err(|err| format!("backup serialize failed: {err}"))?;
        std::fs::create_dir_all(backup_dir)
            .map_err(|err| format!("cannot create {}: {err}", backup_dir.display()))?;
        std::fs::write(&path, format!("{text}\n"))
            .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        backup_file = Some(file_name);
    }

    // Pass three: delete what was backed up (or already gone).
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    for entry in &entries {
        match store.delete(&entry.name).await {
            Ok(_) => deleted.push(entry.name.clone()),
            Err(err) => failed.push(CleanupFailure {
                name: entry.name.clone(),
                reason: crate::fetch::bounded(&err),
            }),
        }
    }
    Ok(CleanupApplyReport {
        dry_run: false,
        planned: plan.duplicate.len(),
        deleted,
        skipped,
        failed,
        backup_file,
    })
}

/// Restore every entry of a backup document through the upsert path. The
/// malformed-entry cases (wrong schema, a non-object entry, a missing name)
/// fail that entry alone; the rest still restore.
pub async fn rollback_cleanup(backup: &Value, store: &dyn CleanupStore) -> CleanupRollbackReport {
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    let mut reject = |name: &str, reason: String| {
        failed.push(CleanupFailure {
            name: name.to_string(),
            reason: crate::fetch::bounded(&reason),
        })
    };
    if backup.get("schema").and_then(Value::as_str) != Some(BACKUP_SCHEMA) {
        reject(
            "<backup>",
            format!("backup schema mismatch (expected {BACKUP_SCHEMA})"),
        );
        return CleanupRollbackReport { restored, failed };
    }
    let Some(entries) = backup.get("entries").and_then(Value::as_array) else {
        reject("<backup>", "backup has no entries array".to_string());
        return CleanupRollbackReport { restored, failed };
    };
    for entry in entries {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            reject("<unnamed>", "backup entry has no name".to_string());
            continue;
        };
        let Some(record) = entry.get("record").filter(|record| record.is_object()) else {
            reject(name, "backup entry has no usable record".to_string());
            continue;
        };
        match store.upsert(name, record).await {
            Ok(_) => restored.push(name.to_string()),
            Err(err) => reject(name, err),
        }
    }
    CleanupRollbackReport { restored, failed }
}

/// Read a backup file from the service's backup directory. The name is
/// validated against the directory: no separators, no parent steps -- the
/// API layer hands the file NAME it got from an apply report, and a hostile
/// one must resolve to an error, not to an arbitrary path.
pub fn read_backup(dir: &Path, file_name: &str) -> Result<Value, String> {
    let clean = file_name.trim();
    let valid = !clean.is_empty()
        && clean
            == Path::new(clean)
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("")
        && clean.ends_with(".json");
    if !valid {
        return Err(format!("invalid backup file name: {clean:?}"));
    }
    let path = dir.join(clean);
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("backup is not valid JSON: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct StubStore {
        subs: Vec<SubEntry>,
        records: HashMap<String, Value>,
        deletes: Mutex<Vec<String>>,
        upserts: Mutex<Vec<(String, Value)>>,
        fail_delete_for: Vec<String>,
        fail_record_for: Vec<String>,
    }

    impl StubStore {
        fn new(entries: &[(&str, Option<&str>)]) -> Self {
            Self {
                subs: entries
                    .iter()
                    .map(|(name, source)| SubEntry {
                        name: (*name).to_string(),
                        source: source.map(|source| source.to_string()),
                    })
                    .collect(),
                records: HashMap::new(),
                deletes: Mutex::new(Vec::new()),
                upserts: Mutex::new(Vec::new()),
                fail_delete_for: Vec::new(),
                fail_record_for: Vec::new(),
            }
        }

        fn with_record(mut self, name: &str, record: Value) -> Self {
            self.records.insert(name.to_string(), record);
            self
        }

        fn failing_record(mut self, name: &str) -> Self {
            self.fail_record_for.push(name.to_string());
            self
        }

        fn failing_delete(mut self, name: &str) -> Self {
            self.fail_delete_for.push(name.to_string());
            self
        }
    }

    impl CleanupStore for StubStore {
        fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SubEntry>, String>> {
            Box::pin(async { Ok(self.subs.clone()) })
        }

        fn record<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Value>, String>> {
            Box::pin(async move {
                if self.fail_record_for.iter().any(|bad| bad == name) {
                    return Err("stub record failure".to_string());
                }
                Ok(self.records.get(name).cloned())
            })
        }

        fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<bool, String>> {
            Box::pin(async move {
                if self.fail_delete_for.iter().any(|bad| bad == name) {
                    return Err("stub delete failure".to_string());
                }
                self.deletes.lock().unwrap().push(name.to_string());
                Ok(true)
            })
        }

        fn upsert<'a>(
            &'a self,
            name: &'a str,
            payload: &'a Value,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                self.upserts
                    .lock()
                    .unwrap()
                    .push((name.to_string(), payload.clone()));
                Ok("created".to_string())
            })
        }
    }

    fn rules() -> CleanupRules {
        CleanupRules {
            push_prefix: "probe".to_string(),
            ipmap_prefix: DEFAULT_IPMAP_PREFIX.to_string(),
        }
    }

    fn plan_of(plan: &CleanupPlan, name: &str) -> bool {
        plan.duplicate.iter().any(|target| target.name == name)
    }

    #[tokio::test]
    async fn scan_classifies_push_front_and_ipmap_artifacts() {
        let store = StubStore::new(&[
            ("probe-air-local", Some("local")),
            ("probe-official-local", Some("remote")), // 命名像，source 不像
            ("probe-front-manual", Some("local")),
            ("ipmap-demo-v6", Some("local")),
            ("ipmap-unknown", Some("local")),
            ("我的机场", Some("remote")),
            ("probe-extra", Some("local")),
        ])
        .with_record(
            "ipmap-demo-v6",
            json!({"name": "ipmap-demo-v6", "displayName": "ipmap 实测映射(demo-v6)"}),
        )
        .with_record("ipmap-unknown", json!({"displayName": "别人的订阅"}));
        let plan = scan_cleanup(&store, &rules(), CleanupCaps::default())
            .await
            .unwrap();
        assert!(plan_of(&plan, "probe-air-local"));
        assert!(plan_of(&plan, "probe-front-manual"));
        assert!(plan_of(&plan, "ipmap-demo-v6"));
        assert!(
            !plan_of(&plan, "probe-official-local"),
            "source != local stays out"
        );
        assert!(!plan_of(&plan, "我的机场"));
        assert!(!plan_of(&plan, "probe-extra"));
        assert!(plan
            .uncertain
            .iter()
            .any(|entry| entry.name == "probe-official-local"));
        assert!(plan
            .uncertain
            .iter()
            .any(|entry| entry.name == "ipmap-unknown"));
        assert_eq!(plan.counts.keep, 2);
        assert_eq!(plan.counts.duplicate, 3);
        assert_eq!(plan.counts.uncertain, 2);
    }

    #[tokio::test]
    async fn scan_caps_the_record_checks_and_reports_the_overflow_as_uncertain() {
        let subs: Vec<SubEntry> = (0..7)
            .map(|index| SubEntry {
                name: format!("ipmap-n{index}"),
                source: Some("local".to_string()),
            })
            .chain(std::iter::once(SubEntry {
                name: "plain".to_string(),
                source: Some("remote".to_string()),
            }))
            .collect();
        // A record exists for every listed name, so the cap is the only thing
        // deciding how many get confirmed.
        let mut records = HashMap::new();
        for entry in &subs {
            records.insert(
                entry.name.clone(),
                json!({"displayName": format!("{}(k)", IPMAP_DISPLAY)}),
            );
        }
        let store = StubStore {
            subs,
            records,
            ..StubStore::new(&[])
        };
        let plan = scan_cleanup(
            &store,
            &rules(),
            CleanupCaps {
                record_fetch_cap: 3,
            },
        )
        .await
        .unwrap();
        // 3 confirmed + 4 over the cap; the un-prefixed one keeps.
        assert_eq!(plan.counts.duplicate, 3);
        assert_eq!(plan.counts.uncertain, 4);
        assert_eq!(plan.counts.keep, 1);
        assert_eq!(plan.record_fetch_cap, 3);
    }

    #[tokio::test]
    async fn a_dry_run_touches_nothing() {
        let store = StubStore::new(&[("probe-air-local", Some("local"))]);
        let plan = scan_cleanup(&store, &rules(), CleanupCaps::default())
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let report = apply_cleanup(&plan, &store, dir.path(), false)
            .await
            .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.planned, 1);
        assert!(report.deleted.is_empty());
        assert!(report.backup_file.is_none());
        assert!(store.deletes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_apply_backs_everything_up_then_deletes_only_the_backed_up() {
        let store = StubStore::new(&[
            ("probe-air-local", Some("local")),
            ("probe-gone-local", Some("local")),
            ("probe-broken-local", Some("local")),
            ("keep-me", Some("remote")),
        ])
        .with_record(
            "probe-air-local",
            json!({"name": "probe-air-local", "content": "x"}),
        )
        // probe-gone-local has no record: it vanished between scan and apply.
        .failing_record("probe-broken-local");
        let plan = scan_cleanup(&store, &rules(), CleanupCaps::default())
            .await
            .unwrap();
        assert_eq!(plan.duplicate.len(), 3);
        let dir = tempfile::tempdir().unwrap();
        let report = apply_cleanup(&plan, &store, dir.path(), true)
            .await
            .unwrap();
        assert!(!report.dry_run);
        // The broken-record target was skipped, NOT deleted: no backup, no delete.
        assert_eq!(report.skipped.len(), 2);
        assert!(report
            .skipped
            .iter()
            .any(|skip| skip.name == "probe-broken-local" && skip.reason.contains("拒绝删除")));
        assert_eq!(report.deleted, ["probe-air-local"]);
        assert_eq!(*store.deletes.lock().unwrap(), ["probe-air-local"]);
        // The backup file holds exactly what was deleted.
        let file = report.backup_file.unwrap();
        let backup = read_backup(dir.path(), &file).unwrap();
        assert_eq!(backup["schema"], BACKUP_SCHEMA);
        assert_eq!(backup["entries"].as_array().unwrap().len(), 1);
        assert_eq!(backup["entries"][0]["name"], "probe-air-local");
    }

    #[tokio::test]
    async fn an_unwritable_backup_dir_aborts_before_any_delete() {
        let store = StubStore::new(&[("probe-air-local", Some("local"))])
            .with_record("probe-air-local", json!({"name": "probe-air-local"}));
        let plan = scan_cleanup(&store, &rules(), CleanupCaps::default())
            .await
            .unwrap();
        // A file where the backup directory should be: create_dir_all fails.
        let blocker = tempfile::tempdir().unwrap();
        let path = blocker.path().join("not-a-dir");
        std::fs::write(&path, "x").unwrap();
        let outcome = apply_cleanup(&plan, &store, &path, true).await;
        assert!(outcome.is_err());
        assert!(
            store.deletes.lock().unwrap().is_empty(),
            "no delete without a backup"
        );
    }

    #[tokio::test]
    async fn a_delete_error_fails_one_target_and_does_not_stop_the_rest() {
        let store = StubStore::new(&[
            ("probe-a-local", Some("local")),
            ("probe-b-local", Some("local")),
        ])
        .with_record("probe-a-local", json!({"name": "probe-a-local"}))
        .with_record("probe-b-local", json!({"name": "probe-b-local"}))
        .failing_delete("probe-a-local");
        let plan = scan_cleanup(&store, &rules(), CleanupCaps::default())
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let report = apply_cleanup(&plan, &store, dir.path(), true)
            .await
            .unwrap();
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].name, "probe-a-local");
        assert_eq!(report.deleted, ["probe-b-local"]);
    }

    #[tokio::test]
    async fn rollback_restores_from_the_backup_document() {
        let store = StubStore::new(&[]);
        let backup = json!({
            "schema": BACKUP_SCHEMA,
            "created_at": "2026-11-04T00:00:00Z",
            "push_prefix": "probe",
            "entries": [
                {"name": "probe-air-local", "record": {"name": "probe-air-local", "content": "y"}},
            ],
        });
        let report = rollback_cleanup(&backup, &store).await;
        assert_eq!(report.restored, ["probe-air-local"]);
        assert!(report.failed.is_empty());
        let upserts = store.upserts.lock().unwrap();
        assert_eq!(upserts[0].0, "probe-air-local");
        assert_eq!(upserts[0].1["content"], "y");
    }

    #[tokio::test]
    async fn rollback_reports_a_malformed_backup_instead_of_panicking() {
        let store = StubStore::new(&[]);
        let wrong_schema = json!({"schema": "something/else", "entries": []});
        let report = rollback_cleanup(&wrong_schema, &store).await;
        assert!(report.restored.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].name, "<backup>");

        let broken_entries = json!({
            "schema": BACKUP_SCHEMA,
            "entries": [
                {"record": {"name": "no-name"}},
                {"name": "no-record"},
                {"name": "bad-record", "record": "not-an-object"},
                {"name": "good", "record": {"name": "good"}},
            ],
        });
        let report = rollback_cleanup(&broken_entries, &store).await;
        assert_eq!(report.restored, ["good"]);
        assert_eq!(report.failed.len(), 3);
    }

    #[test]
    fn backup_names_are_jailed_to_the_backup_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cleanup-ok.json"), "{\"schema\": \"x\"}").unwrap();
        assert!(read_backup(dir.path(), "cleanup-ok.json").is_ok());
        for hostile in [
            "../escape.json",
            "sub/dir.json",
            "..",
            "",
            "not-json.txt",
            "cleanup-ok.json/../../secret",
        ] {
            assert!(read_backup(dir.path(), hostile).is_err(), "{hostile:?}");
        }
    }
}
