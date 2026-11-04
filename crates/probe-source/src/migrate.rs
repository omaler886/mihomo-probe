//! One-shot Sub-Store duplicate-object migration: scan -> plan -> apply ->
//! rollback.
//!
//! Port of the migration algorithm `link_substore.py` is demoted to (guidance
//! §4.2, workstreams/18 §B5). The pre-refactor pipeline minted a
//! `probe-<key>*` Sub-Store object per key, which forked one logical
//! subscription into many objects. This module finds those objects, groups
//! them into logical sources, moves the surviving canonical object onto the
//! stable artifact URL, and quarantines the rest -- without ever creating or
//! deleting a Sub-Store object.
//!
//! ## Safety contract
//!
//! * [`scan`] is strictly read-only: `GET /api/subs` plus the existing
//!   `GET /download/{name}` content route. A failed listing fails the whole
//!   scan -- there is no partial plan.
//! * [`apply`] writes through the EXISTING update-only admin path
//!   ([`SubAdmin::update_sub`], which checks existence first and refuses to
//!   create). Retargeting reads the stored record first and PATCHes that same
//!   record with only `url` swapped; a record that cannot be read is never
//!   replaced with an invented partial one.
//! * **No delete exists here.** [`MigrationStore`] has no delete method to
//!   call, and every stub test fails if the request log shows a `DELETE`.
//!   Quarantine is a rename to `deprecated-<name>`; per F-058 an unattended
//!   agent produces a plan and audit trail, never a deletion.
//! * Renaming is a *capability*: [`MigrationStore::rename_supported`] defaults
//!   to `false` and the HTTP store keeps that default, because Sub-Store's
//!   rename-through-PATCH semantics are unverified. An unsupported rename is
//!   recorded as `skipped` and the object is left untouched -- the
//!   conservative direction wins.
//! * URLs may carry tokens. Everything that leaves the process (plan and
//!   report serialization) goes through [`RedactedUrl`], which strips the
//!   query string and fragment. The raw URL survives only in memory, which is
//!   what lets [`rollback`] restore it exactly; a report re-read from disk can
//!   therefore restore only the redacted form (documented limitation -- the
//!   in-memory report is the rollback source of truth).
//! * Entries never embed YAML bodies or node credentials: only names, host
//!   paths and redacted URLs.
//!
//! ## Grouping (guidance §4.2 priority order)
//!
//! Candidates are objects whose name starts with the publish prefix. Each
//! candidate joins the first group it matches, highest priority first:
//!
//! 1. **saved source_id** -- the caller's map from the object's key (its name
//!    minus the prefix) to the ledger's `source_id`;
//! 2. **identical upstream URL**;
//! 3. **identical normalized node-set digest** -- content is fetched per
//!    candidate through the existing download route, each node fingerprinted
//!    with the ledger identity ([`fingerprint_proxy`], name-insensitive), the
//!    fingerprint list sorted, and the digest taken over the joined list.
//!    Fetches are capped ([`ScanCaps::digest_fetch_cap`]) and cached per
//!    upstream URL; over-cap, unfetchable or empty candidates are *not*
//!    merged -- they fall to name similarity as report-only;
//! 4. **name similarity** -- always a report-only group, never auto-merged.
//!
//! The canonical of a group is its lexicographically smallest name
//! (candidates are processed in sorted order), so the choice is deterministic
//! and stable across rescans. Groups formed at tiers 1-3 are `retarget`
//! groups (canonical = keep class, other members = duplicate); tier-4 groups
//! and every unverifiable candidate are `uncertain`. A candidate that
//! survives tier 3 unmatched is a verified-unique `keep` solo. Objects
//! without the prefix -- including `deprecated-*` leftovers of a previous
//! run -- are untouched.
//!
//! ## Coverage note (recorded in every plan)
//!
//! Subscriptions are covered end to end. Collections are `not_run`: the
//! Sub-Store surface this crate has (`fetch.rs` `SubAdmin` plus this module's
//! store) enumerates and updates subscriptions only, so collection objects are
//! never candidates and the collection path is reported as such rather than
//! half-implemented.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fetch::{content_digest, SubAdmin, SubStoreClient, DEFAULT_TARGET};
use crate::identity::fingerprint_proxy;
use crate::subscription::parse_proxies;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Error text is bounded before it reaches a report entry: an unbounded body
/// can carry a URL with credentials in it. Same rule as `fetch.rs`.
const ERROR_LIMIT: usize = 200;

/// Schema version of the plan/report documents. Bumped only when the JSON
/// shape breaks; `apply` refuses plans of any other version.
pub const PLAN_SCHEMA: u32 = 1;

/// The publish prefix the old pipeline used; also the scan default.
pub const DEFAULT_PREFIX: &str = "probe-";

/// Prefix a quarantined duplicate is renamed to. Renamed objects stop being
/// candidates (they no longer carry the publish prefix), which is what makes a
/// rescan after a partial apply idempotent.
pub const QUARANTINE_PREFIX: &str = "deprecated-";

const COVERAGE_SUBSCRIPTIONS: &str = "covered: listed via GET /api/subs, \
     content via GET /download/{name}, writes via the update-only PATCH path \
     (never create)";

const COVERAGE_COLLECTIONS: &str = "not_run: the Sub-Store surface this crate \
     has (fetch.rs SubAdmin plus migrate::SubStoreHttpStore) enumerates and \
     updates subscriptions only; there is no existing collection list or \
     update path, so collection objects are never candidates";

/// Which kind of Sub-Store object a reference addresses.
///
/// Only subscriptions are reachable today; the variant exists so rollback
/// entries and audit lines stay self-describing when collections ever gain a
/// read/write path (see the module coverage note).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    Sub,
}

/// A minimal object address: kind plus name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreRef {
    pub kind: StoreKind,
    pub name: String,
}

/// One object as the store listed it. The URL is raw because grouping must
/// distinguish upstreams that differ only in their token query string; the
/// type is never serialized (manual `Debug` redacts).
#[derive(Clone, PartialEq, Eq)]
pub struct StoreObject {
    pub kind: StoreKind,
    pub name: String,
    pub url: Option<String>,
    pub source: Option<String>,
}

impl fmt::Debug for StoreObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreObject")
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("url", &self.url.as_deref().map(redact_url))
            .field("source", &self.source)
            .finish()
    }
}

/// A URL that serializes with its query string and fragment stripped.
///
/// Sub-Store upstream URLs routinely embed a token. The raw value is kept in
/// memory (rollback restores it exactly) but never allowed through
/// serialization or `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct RedactedUrl(String);

impl RedactedUrl {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The unredacted URL. In-memory only: rollback needs the exact original.
    pub fn raw(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RedactedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&redact_url(&self.0))
    }
}

impl fmt::Debug for RedactedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RedactedUrl({})", redact_url(&self.0))
    }
}

impl Serialize for RedactedUrl {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&redact_url(&self.0))
    }
}

impl<'de> Deserialize<'de> for RedactedUrl {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(String::deserialize(deserializer)?))
    }
}

/// Strip the query string and fragment from a URL, replacing them with a
/// visible marker. `https://up.example/s?token=x` becomes
/// `https://up.example/s?<redacted>`.
pub fn redact_url(url: &str) -> String {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    if end == url.len() {
        return url.to_string();
    }
    let mut out = String::with_capacity(end + 12);
    out.push_str(&url[..end]);
    out.push_str("?<redacted>");
    out
}

/// What a plan/report entry carries as its `before`/`after` value:
/// `{"field": "url", "value": "https://host/path?<redacted>"}` or
/// `{"field": "name", "value": "..."}`. URLs are redacted at construction
/// time, so the audit trail cannot leak a token even if serialized raw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryValue {
    pub field: EntryField,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryField {
    Url,
    Name,
}

impl EntryValue {
    fn url(raw: &str) -> Self {
        Self {
            field: EntryField::Url,
            value: redact_url(raw),
        }
    }

    fn name(name: &str) -> Self {
        Self {
            field: EntryField::Name,
            value: name.to_string(),
        }
    }
}

/// A typed migration failure. Messages are bounded and never name the backend
/// (the backend URL is the credential, same rule as `fetch.rs`).
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("listing Sub-Store objects failed: {0}")]
    List(String),
    #[error("reading sub `{name}` failed: {reason}")]
    Read { name: String, reason: String },
    #[error("updating sub `{name}` failed: {reason}")]
    Update { name: String, reason: String },
    #[error("{0}")]
    Unsupported(&'static str),
}

/// The read/write surface the migration machinery needs.
///
/// Deliberately different from [`SubAdmin`]: listing returns the upstream URL
/// (grouping tier 2), content comes back parsed (the digest tier), and --
/// critically -- **there is no delete method**. The missing method is the
/// structural half of the no-delete rule; the tests pin the other half by
/// failing on any observed DELETE request.
pub trait MigrationStore: Send + Sync {
    /// Every object the store holds. Objects without a name are skipped.
    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<StoreObject>, MigrationError>>;
    /// One object's stored record, `Ok(None)` when absent.
    fn record<'a>(
        &'a self,
        object: &'a StoreRef,
    ) -> BoxFuture<'a, Result<Option<Value>, MigrationError>>;
    /// One object's rendered content (the download route), parsed into proxy
    /// entries. `Ok(None)` when the object has no downloadable content.
    fn content<'a>(
        &'a self,
        object: &'a StoreRef,
    ) -> BoxFuture<'a, Result<Option<Vec<Value>>, MigrationError>>;
    /// Replace an EXISTING object's record, never create one. Returns
    /// `"updated"` on success.
    fn update<'a>(
        &'a self,
        object: &'a StoreRef,
        payload: &'a Value,
    ) -> BoxFuture<'a, Result<String, MigrationError>>;
    /// Whether renaming through [`MigrationStore::update`] is known to work
    /// for this store. Defaults to `false`: quarantining then becomes a
    /// recorded skip instead of a guess.
    fn rename_supported(&self) -> bool {
        false
    }
}

/// The real Sub-Store HTTP store.
///
/// Reads (`list`/`record`/`content`) are this module's own bounded GETs
/// against the same routes the existing client uses; writes are delegated to
/// the EXISTING update-only path (`SubAdmin::update_sub` on an internal
/// [`SubStoreClient`]), which checks existence and refuses to create. There
/// are no retries here: a migration scan is interactive, and a transient 5xx
/// should surface as a typed error the operator re-runs, not as hidden
/// traffic. Renaming is reported unsupported (conservative default).
pub struct SubStoreHttpStore {
    backend: String,
    http: reqwest::Client,
    admin: SubStoreClient,
}

impl SubStoreHttpStore {
    pub fn new(backend: &str) -> Self {
        Self {
            backend: backend.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("static client config"),
            admin: SubStoreClient::new(backend),
        }
    }

    /// Bounded GET returning `(status, body)`. The `Err` message is bounded
    /// and backend-free; the caller wraps it in the variant matching its
    /// operation.
    async fn get(&self, path: &str) -> Result<(u16, String), String> {
        let outcome = self
            .http
            .get(format!("{}{path}", self.backend))
            .header("User-Agent", crate::fetch::BROWSER_UA)
            .send()
            .await;
        match outcome {
            Ok(response) => {
                let status = response.status().as_u16();
                let text = bounded_body(response).await?;
                Ok((status, text))
            }
            // Classify only: reqwest's Display embeds the request URL, and
            // this backend's URL is the credential itself.
            Err(err) => Err(bounded(&format!("{} {path}", kind_of(&err)))),
        }
    }
}

impl MigrationStore for SubStoreHttpStore {
    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<StoreObject>, MigrationError>> {
        Box::pin(async move {
            let path = "/api/subs";
            let (status, text) = self.get(path).await.map_err(MigrationError::List)?;
            if status != 200 {
                return Err(MigrationError::List(bounded(&format!(
                    "HTTP {status} {path}: {}",
                    summarize(&text)
                ))));
            }
            let items: Vec<Value> = serde_json::from_str(&text)
                .map_err(|_| MigrationError::List("GET /api/subs returned unusable JSON".into()))?;
            Ok(items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name")?.as_str()?.to_string();
                    let url = item.get("url").and_then(Value::as_str).map(str::to_string);
                    let source = item
                        .get("source")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    Some(StoreObject {
                        kind: StoreKind::Sub,
                        name,
                        url,
                        source,
                    })
                })
                .collect())
        })
    }

    fn record<'a>(
        &'a self,
        object: &'a StoreRef,
    ) -> BoxFuture<'a, Result<Option<Value>, MigrationError>> {
        Box::pin(async move {
            let path = format!("/api/sub/{}", encode(&object.name));
            let (status, text) = self
                .get(&path)
                .await
                .map_err(|reason| MigrationError::Read {
                    name: object.name.clone(),
                    reason,
                })?;
            if status == 200 {
                return serde_json::from_str::<Value>(&text).map(Some).map_err(|_| {
                    MigrationError::Read {
                        name: object.name.clone(),
                        reason: "GET returned unusable JSON".into(),
                    }
                });
            }
            if status == 404 || is_missing(status, &text) {
                return Ok(None);
            }
            Err(MigrationError::Read {
                name: object.name.clone(),
                reason: bounded(&format!("HTTP {status}: {}", summarize(&text))),
            })
        })
    }

    fn content<'a>(
        &'a self,
        object: &'a StoreRef,
    ) -> BoxFuture<'a, Result<Option<Vec<Value>>, MigrationError>> {
        Box::pin(async move {
            let path = format!(
                "/download/{}?target={}",
                encode(&object.name),
                encode(DEFAULT_TARGET)
            );
            let (status, text) = self
                .get(&path)
                .await
                .map_err(|reason| MigrationError::Read {
                    name: object.name.clone(),
                    reason,
                })?;
            if status == 200 {
                return parse_proxies(&text)
                    .map(Some)
                    .map_err(|err| MigrationError::Read {
                        name: object.name.clone(),
                        reason: bounded(&format!("download returned unusable Clash YAML: {err}")),
                    });
            }
            if status == 404 || is_missing(status, &text) {
                return Ok(None);
            }
            Err(MigrationError::Read {
                name: object.name.clone(),
                reason: bounded(&format!("HTTP {status}: {}", summarize(&text))),
            })
        })
    }

    fn update<'a>(
        &'a self,
        object: &'a StoreRef,
        payload: &'a Value,
    ) -> BoxFuture<'a, Result<String, MigrationError>> {
        Box::pin(async move {
            // The EXISTING update-only path: existence-checked, never creates.
            SubAdmin::update_sub(&self.admin, &object.name, payload)
                .await
                .map_err(|reason| MigrationError::Update {
                    name: object.name.clone(),
                    reason: bounded(&reason),
                })
        })
    }

    // rename_supported keeps the trait default (`false`): Sub-Store's
    // rename-through-PATCH semantics are unverified, so the conservative
    // default stands until they are.
}

/// Scan caps: how much a single scan may fetch, and the quarantine window the
/// plan documents (guidance §4.2 `--quarantine-hours`, default 24h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCaps {
    /// Maximum content fetches (digest computations) per scan. Candidates
    /// beyond the cap are not fetched and are never auto-merged; they fall to
    /// name similarity as report-only.
    pub digest_fetch_cap: usize,
    /// Quarantine window recorded on the plan. This module never deletes;
    /// the window is advisory for the operator.
    pub quarantine_hours: u64,
}

impl Default for ScanCaps {
    fn default() -> Self {
        Self {
            digest_fetch_cap: 50,
            quarantine_hours: 24,
        }
    }
}

/// Why a candidate joined its group -- guidance §4.2's evidence priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupReason {
    SavedSourceId,
    SameUrl,
    SameNodeDigest,
    NameSimilarity,
}

/// What apply is allowed to do with a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupAction {
    /// Retarget the canonical to the stable artifact URL and quarantine the
    /// duplicates (rename only, if the store supports it).
    Retarget,
    /// Report only: never auto-merged, never written by apply.
    ReportOnly,
}

/// F-058 classification: keep / duplicate / uncertain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectClass {
    /// The object a group keeps, or a verified-unique solo candidate.
    Keep,
    /// A member of a retarget group other than its canonical.
    Duplicate,
    /// Report-only: evidence too weak to merge or to clear.
    Uncertain,
}

/// One candidate object inside a plan, with its evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateRef {
    pub kind: StoreKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<RedactedUrl>,
    /// sha256 over the sorted node-fingerprint list, when computed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub class: ObjectClass,
    /// Evidence that pulled this candidate into its group. Always present for
    /// group members; solos carry a `note` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<GroupReason>,
    /// Precise cause for solos and unverifiable candidates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl CandidateRef {
    fn object(&self) -> StoreRef {
        StoreRef {
            kind: self.kind,
            name: self.name.clone(),
        }
    }
}

/// One logical source: a canonical plus its duplicates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationGroup {
    pub action: GroupAction,
    pub reason: GroupReason,
    /// Ledger `source_id` when tier 1 (the saved map) supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    pub canonical: CandidateRef,
    pub duplicates: Vec<CandidateRef>,
}

/// An operation the plan intends (guidance §4.2's actionable summary). Stable
/// URLs are filled in at apply time -- the plan is produced before any
/// artifact URL is known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlannedAction {
    Retarget {
        object: StoreRef,
        group: usize,
    },
    QuarantineRename {
        object: StoreRef,
        to: String,
        group: usize,
    },
    QuarantineSkip {
        object: StoreRef,
        group: usize,
        detail: String,
    },
}

/// One rollback instruction: restore a URL, or rename a quarantined object
/// back. Serialized URLs are redacted (raw values survive only in memory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackEntry {
    /// The object to address. For URL restores this is the canonical (its
    /// name never changes); for name restores it is the quarantined name.
    pub object: StoreRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_url: Option<RedactedUrl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    /// The `deprecated-*` name a rename gave the object, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantined_as: Option<String>,
}

/// One audit line: `{action, object, before, after, at}` plus status. No YAML
/// bodies, no credentials -- only names and redacted URLs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub action: AuditAction,
    pub object: StoreRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<EntryValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<EntryValue>,
    pub status: ActionStatus,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    Retarget,
    QuarantineRename,
    QuarantineSkip,
    RestoreUrl,
    RestoreName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    /// Written for real.
    Applied,
    /// dry-run preview: computed, not written.
    WouldDo,
    Failed,
    Skipped,
    /// The object already carried the target value; no write needed.
    AlreadyCurrent,
    /// Rollback found the original value already in place; no write needed.
    AlreadyRestored,
}

/// What the store was scanned for, honestly recorded (F-058 scope).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    pub subscriptions: String,
    pub collections: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlanCounts {
    pub keep: usize,
    pub duplicate: usize,
    pub uncertain: usize,
    pub untouched: usize,
}

/// The migration plan: versioned, JSON-serializable, the only artifact an
/// unattended agent produces (F-058).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub schema: u32,
    pub generated_at: String,
    pub prefix: String,
    pub coverage: Coverage,
    pub quarantine_hours: u64,
    pub digest_fetch_cap: usize,
    pub fetches_used: usize,
    pub counts: PlanCounts,
    pub groups: Vec<MigrationGroup>,
    /// Candidates matching no group: verified-unique keeps and unverifiable
    /// uncertains.
    pub solo: Vec<CandidateRef>,
    /// Scanned objects that carry no publish prefix (including
    /// `deprecated-*` leftovers): never candidates, never touched.
    pub untouched: Vec<StoreRef>,
    pub actions: Vec<PlannedAction>,
    pub rollback_entries: Vec<RollbackEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApplyCounts {
    pub applied: usize,
    pub would_do: usize,
    pub failed: usize,
    pub skipped: usize,
    pub already_current: usize,
}

/// The result of an apply: one audit entry per action, plus the rollback list
/// reflecting what was actually (or would be) changed. This is the rollback
/// source of truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub schema: u32,
    pub applied_at: String,
    pub dry_run: bool,
    pub plan_generated_at: String,
    pub entries: Vec<AuditEntry>,
    pub rollback_entries: Vec<RollbackEntry>,
    pub counts: ApplyCounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RollbackCounts {
    pub restored: usize,
    pub already_restored: usize,
    pub failed: usize,
    pub skipped: usize,
}

/// The result of a rollback: one audit entry per restore, with before/after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackReport {
    pub schema: u32,
    pub restored_at: String,
    pub entries: Vec<AuditEntry>,
    pub counts: RollbackCounts,
}

/// Identifies the stable artifact URL target: the ledger `source_id` when the
/// scan had one, else the object's key (name minus the publish prefix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    pub source_id: Option<String>,
    pub key: String,
}

// ---------------------------------------------------------------------------
// scan
// ---------------------------------------------------------------------------

/// Scan the store for duplicate publish-prefix objects and produce a plan.
///
/// Strictly read-only. A failed listing fails the whole scan (no partial
/// plan); a candidate whose content cannot be fetched simply stays
/// `uncertain` with a note.
pub async fn scan(
    store: &dyn MigrationStore,
    prefix: &str,
    source_id_map: Option<&HashMap<String, String>>,
    caps: ScanCaps,
) -> Result<MigrationPlan, MigrationError> {
    if prefix.is_empty() {
        // Every object would be a candidate; that is a misconfiguration, not
        // a scan.
        return Err(MigrationError::Unsupported(
            "publish prefix must not be empty",
        ));
    }
    let objects = store.list().await?;
    let mut candidates: Vec<&StoreObject> = objects
        .iter()
        .filter(|object| object.name.starts_with(prefix))
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    let untouched: Vec<StoreRef> = objects
        .iter()
        .filter(|object| !object.name.starts_with(prefix))
        .map(|object| StoreRef {
            kind: object.kind,
            name: object.name.clone(),
        })
        .collect();

    let mut fetches = 0usize;
    // Digest cache keyed by raw upstream URL: two candidates sharing an URL
    // need one content fetch between them (defensive -- tier 2 usually groups
    // same-URL candidates before any fetch).
    let mut cache: HashMap<String, String> = HashMap::new();
    let mut groups: Vec<WorkGroup> = Vec::new();
    let mut parked: Vec<CandidateRef> = Vec::new();

    for object in candidates {
        let key = &object.name[prefix.len()..];
        let source_id = source_id_map.and_then(|map| map.get(key)).cloned();
        let url = object.url.clone().filter(|u| !u.is_empty());
        let url_red = url.as_ref().map(|u| RedactedUrl::new(u.clone()));
        let object_ref = StoreRef {
            kind: object.kind,
            name: object.name.clone(),
        };

        // Tier 1: the saved source_id map. A hit either joins its group or
        // founds one -- the map is authoritative, so the candidate never
        // falls through to weaker evidence.
        if let Some(source_id) = &source_id {
            if let Some(index) = groups
                .iter()
                .position(|group| group.source_id.as_deref() == Some(source_id.as_str()))
            {
                let mut candidate = base_candidate(object, url_red, &None);
                candidate.reason = Some(GroupReason::SavedSourceId);
                groups[index].add_member(candidate, url.clone());
                continue;
            }
            let mut canonical = base_candidate(object, url_red, &None);
            canonical.reason = Some(GroupReason::SavedSourceId);
            groups.push(WorkGroup::new(
                GroupAction::Retarget,
                Formed::SourceId,
                Some(source_id.clone()),
                canonical,
                url.clone(),
            ));
            continue;
        }

        // Tier 2: identical upstream URL.
        if let Some(url_raw) = &url {
            if let Some(index) = groups.iter().position(|group| {
                group.action == GroupAction::Retarget && group.urls.iter().any(|u| u == url_raw)
            }) {
                let mut candidate = base_candidate(object, url_red, &None);
                candidate.reason = Some(GroupReason::SameUrl);
                groups[index].add_member(candidate, url.clone());
                continue;
            }
        }

        // Tier 3: identical normalized node-set digest. The candidate's own
        // digest is fetched (cap + cache); a group whose canonical digest is
        // still unknown gets it fetched now, once.
        let digest: String = match digest_of(
            store,
            &object_ref,
            url.as_deref(),
            &mut fetches,
            caps.digest_fetch_cap,
            &mut cache,
        )
        .await
        {
            Ok(digest) => digest,
            Err(note) => {
                let mut candidate = base_candidate(object, url_red, &None);
                candidate.note = Some(note);
                parked.push(candidate);
                continue;
            }
        };
        let mut comparable = true;
        for group in &mut groups {
            if group.action != GroupAction::Retarget {
                continue;
            }
            // Owned copies: nothing borrowed across the await that mutates
            // the group afterwards.
            let canonical_object = group.canonical.object();
            let canonical_url: Option<String> =
                group.canonical.url.as_ref().map(|u| u.raw().to_string());
            match group.digest {
                GroupDigest::Unknown => {
                    match digest_of(
                        store,
                        &canonical_object,
                        canonical_url.as_deref(),
                        &mut fetches,
                        caps.digest_fetch_cap,
                        &mut cache,
                    )
                    .await
                    {
                        Ok(digest) => group.digest = GroupDigest::Value(digest),
                        // Cannot verify against this group: the candidate
                        // might be its duplicate, so it must not be merged
                        // anywhere else either.
                        Err(_) => {
                            group.digest = GroupDigest::Unavailable;
                            comparable = false;
                            break;
                        }
                    }
                }
                GroupDigest::Unavailable => {
                    comparable = false;
                    break;
                }
                GroupDigest::Value(_) => {}
            }
        }
        if comparable {
            if let Some(index) = groups.iter().position(|group| {
                group.action == GroupAction::Retarget
                    && matches!(&group.digest, GroupDigest::Value(value) if *value == digest)
            }) {
                let mut candidate = base_candidate(object, url_red, &Some(digest.clone()));
                candidate.reason = Some(GroupReason::SameNodeDigest);
                groups[index].add_member(candidate, url.clone());
                continue;
            }
        } else {
            let mut candidate = base_candidate(object, url_red, &Some(digest));
            candidate.note = Some(
                "another group's node digest could not be computed; \
                 uniqueness could not be verified"
                    .to_string(),
            );
            parked.push(candidate);
            continue;
        }
        // Verified unique so far: founds a tentative group. If nobody ever
        // joins it, it is demoted to a keep solo before export.
        let canonical = base_candidate(object, url_red, &Some(digest));
        groups.push(WorkGroup::new(
            GroupAction::Retarget,
            Formed::Digest,
            None,
            canonical,
            url.clone(),
        ));
    }

    // Tentative groups nobody joined become keep solos.
    let mut solos: Vec<CandidateRef> = Vec::new();
    let mut finished: Vec<WorkGroup> = Vec::new();
    for group in groups {
        if group.action == GroupAction::Retarget && group.duplicates.is_empty() {
            let mut canonical = group.canonical;
            canonical.class = ObjectClass::Keep;
            canonical.reason = None;
            solos.push(canonical);
        } else {
            finished.push(group);
        }
    }

    // Tier 4: name similarity over everything still unresolved (over-cap,
    // unfetchable, unverifiable, and verified-unique solos -- an unfetched
    // name-mate could still be a duplicate, so even keeps go conservative
    // here). Report-only groups only; never auto-merged.
    let mut report_groups: Vec<WorkGroup> = Vec::new();
    let mut final_solos: Vec<CandidateRef> = Vec::new();
    let pool: Vec<CandidateRef> = parked.into_iter().chain(solos).collect();
    for mut candidate in pool {
        let stem = normalized_stem(&candidate.name, prefix);
        if let Some(index) = report_groups
            .iter()
            .position(|group| names_similar(&group.seed_stem, &stem))
        {
            candidate.reason = Some(GroupReason::NameSimilarity);
            candidate.class = ObjectClass::Uncertain;
            report_groups[index].add_member(candidate, None);
        } else if let Some(seed_index) = final_solos
            .iter()
            .position(|solo| names_similar(&normalized_stem(&solo.name, prefix), &stem))
        {
            let mut seed = final_solos.remove(seed_index);
            seed.reason = Some(GroupReason::NameSimilarity);
            seed.class = ObjectClass::Uncertain;
            let seed_stem = normalized_stem(&seed.name, prefix);
            let mut group =
                WorkGroup::new(GroupAction::ReportOnly, Formed::Digest, None, seed, None);
            group.seed_stem = seed_stem;
            candidate.reason = Some(GroupReason::NameSimilarity);
            candidate.class = ObjectClass::Uncertain;
            group.add_member(candidate, None);
            report_groups.push(group);
        } else {
            final_solos.push(candidate);
        }
    }
    finished.extend(report_groups);
    let groups = finished;

    // Export.
    let mut exported: Vec<MigrationGroup> = Vec::with_capacity(groups.len());
    let mut counts = PlanCounts {
        untouched: untouched.len(),
        ..PlanCounts::default()
    };
    let mut actions: Vec<PlannedAction> = Vec::new();
    let mut rollback_entries: Vec<RollbackEntry> = Vec::new();
    let rename_capable = store.rename_supported();
    for (index, group) in groups.iter().enumerate() {
        let mut canonical = group.canonical.clone();
        let mut duplicates = group.duplicates.clone();
        if group.action == GroupAction::Retarget {
            canonical.class = ObjectClass::Keep;
            counts.keep += 1;
            for duplicate in &mut duplicates {
                duplicate.class = ObjectClass::Duplicate;
            }
            counts.duplicate += duplicates.len();
            actions.push(PlannedAction::Retarget {
                object: canonical.object(),
                group: index,
            });
            if let Some(url) = &canonical.url {
                rollback_entries.push(RollbackEntry {
                    object: canonical.object(),
                    original_url: Some(url.clone()),
                    original_name: None,
                    quarantined_as: None,
                });
            }
            for duplicate in &duplicates {
                if duplicate.name.starts_with(QUARANTINE_PREFIX) {
                    actions.push(PlannedAction::QuarantineSkip {
                        object: duplicate.object(),
                        group: index,
                        detail: "already quarantined".to_string(),
                    });
                } else if rename_capable {
                    let to = format!("{QUARANTINE_PREFIX}{}", duplicate.name);
                    actions.push(PlannedAction::QuarantineRename {
                        object: duplicate.object(),
                        to: to.clone(),
                        group: index,
                    });
                    rollback_entries.push(RollbackEntry {
                        object: StoreRef {
                            kind: duplicate.kind,
                            name: to.clone(),
                        },
                        original_url: None,
                        original_name: Some(duplicate.name.clone()),
                        quarantined_as: Some(to),
                    });
                } else {
                    actions.push(PlannedAction::QuarantineSkip {
                        object: duplicate.object(),
                        group: index,
                        detail: "renaming via update is not supported by this \
                                 store; object left untouched"
                            .to_string(),
                    });
                }
            }
        } else {
            canonical.class = ObjectClass::Uncertain;
            for duplicate in &mut duplicates {
                duplicate.class = ObjectClass::Uncertain;
            }
            counts.uncertain += 1 + duplicates.len();
        }
        let reason = if group.action == GroupAction::ReportOnly {
            GroupReason::NameSimilarity
        } else if group.source_id.is_some() {
            GroupReason::SavedSourceId
        } else {
            match group.formed {
                Formed::SourceId => GroupReason::SavedSourceId,
                Formed::Digest => GroupReason::SameNodeDigest,
            }
        };
        exported.push(MigrationGroup {
            action: group.action,
            reason,
            source_id: group.source_id.clone(),
            canonical,
            duplicates,
        });
    }
    for solo in &final_solos {
        match solo.class {
            ObjectClass::Keep => counts.keep += 1,
            ObjectClass::Duplicate | ObjectClass::Uncertain => counts.uncertain += 1,
        }
    }

    Ok(MigrationPlan {
        schema: PLAN_SCHEMA,
        generated_at: now_rfc3339(),
        prefix: prefix.to_string(),
        coverage: Coverage {
            subscriptions: COVERAGE_SUBSCRIPTIONS.to_string(),
            collections: COVERAGE_COLLECTIONS.to_string(),
        },
        quarantine_hours: caps.quarantine_hours,
        digest_fetch_cap: caps.digest_fetch_cap,
        fetches_used: fetches,
        counts,
        groups: exported,
        solo: final_solos,
        untouched,
        actions,
        rollback_entries,
    })
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

/// Apply a plan: retarget each retarget group's canonical to its stable
/// artifact URL and quarantine its duplicates (rename only, if the store
/// supports renaming).
///
/// `dry_run=true` performs zero writes and previews every action with
/// `would_do` statuses. Every action -- performed, previewed, failed or
/// skipped -- lands in the report's audit entries with before/after and a
/// timestamp. One group failing never stops the others; a failed retarget
/// also leaves its own duplicates untouched (renaming them would strand the
/// source with no working object).
///
/// Report-only groups are never written. Callers reach for `dry_run=true`
/// first and pass `false` only after reviewing the plan.
pub async fn apply(
    plan: &MigrationPlan,
    store: &dyn MigrationStore,
    stable_url_for: impl Fn(&ArtifactRef) -> String,
    dry_run: bool,
) -> ApplyReport {
    let mut entries: Vec<AuditEntry> = Vec::new();
    let mut rollback_entries: Vec<RollbackEntry> = Vec::new();
    let mut counts = ApplyCounts::default();

    if plan.schema != PLAN_SCHEMA {
        entries.push(AuditEntry {
            action: AuditAction::Retarget,
            object: StoreRef {
                kind: StoreKind::Sub,
                name: "<plan>".to_string(),
            },
            before: None,
            after: None,
            status: ActionStatus::Failed,
            at: now_rfc3339(),
            error: Some(format!(
                "unsupported plan schema {}; regenerate the plan with a \
                 matching scanner",
                plan.schema
            )),
            detail: None,
        });
        counts.failed += 1;
        return finish_apply(entries, rollback_entries, counts, plan, dry_run);
    }

    for group in &plan.groups {
        if group.action != GroupAction::Retarget {
            continue;
        }
        let canonical = group.canonical.object();
        let key = group
            .canonical
            .name
            .strip_prefix(&plan.prefix)
            .unwrap_or(&group.canonical.name);
        let artifact = ArtifactRef {
            source_id: group.source_id.clone(),
            key: key.to_string(),
        };
        let stable = stable_url_for(&artifact);

        let mut retarget_ok = false;
        match store.record(&canonical).await {
            Err(error) => {
                push_entry(
                    &mut entries,
                    &mut counts,
                    AuditAction::Retarget,
                    canonical.clone(),
                    None,
                    Some(EntryValue::url(&stable)),
                    ActionStatus::Failed,
                    Some(bounded(&error.to_string())),
                    None,
                );
            }
            Ok(None) => {
                push_entry(
                    &mut entries,
                    &mut counts,
                    AuditAction::Retarget,
                    canonical.clone(),
                    None,
                    Some(EntryValue::url(&stable)),
                    ActionStatus::Failed,
                    None,
                    Some("record unavailable; refusing to write a partial record".to_string()),
                );
            }
            Ok(Some(record)) => {
                let current = record
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|url| !url.is_empty());
                match current {
                    None => {
                        push_entry(
                            &mut entries,
                            &mut counts,
                            AuditAction::Retarget,
                            canonical.clone(),
                            None,
                            Some(EntryValue::url(&stable)),
                            ActionStatus::Failed,
                            None,
                            Some(
                                "record has no upstream url; refusing to \
                                 retarget a content-embedded sub"
                                    .to_string(),
                            ),
                        );
                    }
                    Some(current) if current == stable => {
                        retarget_ok = true;
                        push_entry(
                            &mut entries,
                            &mut counts,
                            AuditAction::Retarget,
                            canonical.clone(),
                            Some(EntryValue::url(current)),
                            Some(EntryValue::url(&stable)),
                            ActionStatus::AlreadyCurrent,
                            None,
                            Some("canonical already points at the stable URL".to_string()),
                        );
                    }
                    Some(current) => {
                        let mut payload = record.clone();
                        if payload.as_object_mut().is_none() {
                            push_entry(
                                &mut entries,
                                &mut counts,
                                AuditAction::Retarget,
                                canonical.clone(),
                                None,
                                Some(EntryValue::url(&stable)),
                                ActionStatus::Failed,
                                None,
                                Some("stored record is not an object".to_string()),
                            );
                            continue;
                        }
                        payload["url"] = Value::String(stable.clone());
                        payload["name"] = Value::String(canonical.name.clone());
                        let (status, error) = if dry_run {
                            (ActionStatus::WouldDo, None)
                        } else {
                            match store.update(&canonical, &payload).await {
                                Ok(_) => (ActionStatus::Applied, None),
                                Err(error) => {
                                    (ActionStatus::Failed, Some(bounded(&error.to_string())))
                                }
                            }
                        };
                        if matches!(status, ActionStatus::Applied | ActionStatus::WouldDo) {
                            retarget_ok = true;
                            rollback_entries.push(RollbackEntry {
                                object: canonical.clone(),
                                original_url: Some(RedactedUrl::new(current)),
                                original_name: None,
                                quarantined_as: None,
                            });
                        }
                        push_entry(
                            &mut entries,
                            &mut counts,
                            AuditAction::Retarget,
                            canonical.clone(),
                            Some(EntryValue::url(current)),
                            Some(EntryValue::url(&stable)),
                            status,
                            error,
                            None,
                        );
                    }
                }
            }
        }

        // Quarantine the duplicates -- only while the canonical is in a good
        // state. A failed retarget strands the source otherwise.
        for duplicate in &group.duplicates {
            if duplicate.class != ObjectClass::Duplicate {
                continue;
            }
            let duplicate_ref = duplicate.object();
            if !retarget_ok {
                push_entry(
                    &mut entries,
                    &mut counts,
                    AuditAction::QuarantineSkip,
                    duplicate_ref,
                    None,
                    None,
                    ActionStatus::Skipped,
                    None,
                    Some("retarget did not succeed; duplicate left untouched".to_string()),
                );
                continue;
            }
            if duplicate.name.starts_with(QUARANTINE_PREFIX) {
                push_entry(
                    &mut entries,
                    &mut counts,
                    AuditAction::QuarantineSkip,
                    duplicate_ref,
                    None,
                    None,
                    ActionStatus::Skipped,
                    None,
                    Some("already quarantined".to_string()),
                );
                continue;
            }
            if !store.rename_supported() {
                push_entry(
                    &mut entries,
                    &mut counts,
                    AuditAction::QuarantineSkip,
                    duplicate_ref,
                    None,
                    None,
                    ActionStatus::Skipped,
                    None,
                    Some(
                        "renaming via update is not supported by this store; \
                         object left untouched"
                            .to_string(),
                    ),
                );
                continue;
            }
            let new_name = format!("{QUARANTINE_PREFIX}{}", duplicate.name);
            match store.record(&duplicate_ref).await {
                Err(error) => {
                    push_entry(
                        &mut entries,
                        &mut counts,
                        AuditAction::QuarantineRename,
                        duplicate_ref,
                        Some(EntryValue::name(&duplicate.name)),
                        Some(EntryValue::name(&new_name)),
                        ActionStatus::Failed,
                        Some(bounded(&error.to_string())),
                        None,
                    );
                }
                Ok(None) => {
                    push_entry(
                        &mut entries,
                        &mut counts,
                        AuditAction::QuarantineRename,
                        duplicate_ref,
                        Some(EntryValue::name(&duplicate.name)),
                        Some(EntryValue::name(&new_name)),
                        ActionStatus::Skipped,
                        None,
                        Some(
                            "duplicate record unavailable (absent, or already \
                             quarantined by an earlier apply)"
                                .to_string(),
                        ),
                    );
                }
                Ok(Some(record)) => {
                    let target = StoreRef {
                        kind: duplicate.kind,
                        name: new_name.clone(),
                    };
                    if matches!(store.record(&target).await, Ok(Some(_))) {
                        push_entry(
                            &mut entries,
                            &mut counts,
                            AuditAction::QuarantineRename,
                            duplicate_ref,
                            Some(EntryValue::name(&duplicate.name)),
                            Some(EntryValue::name(&new_name)),
                            ActionStatus::Skipped,
                            None,
                            Some("target name already exists".to_string()),
                        );
                        continue;
                    }
                    let mut payload = record.clone();
                    if payload.as_object_mut().is_none() {
                        push_entry(
                            &mut entries,
                            &mut counts,
                            AuditAction::QuarantineRename,
                            duplicate_ref,
                            Some(EntryValue::name(&duplicate.name)),
                            Some(EntryValue::name(&new_name)),
                            ActionStatus::Failed,
                            None,
                            Some("stored record is not an object".to_string()),
                        );
                        continue;
                    }
                    payload["name"] = Value::String(new_name.clone());
                    let (status, error) = if dry_run {
                        (ActionStatus::WouldDo, None)
                    } else {
                        match store.update(&duplicate_ref, &payload).await {
                            Ok(_) => (ActionStatus::Applied, None),
                            Err(error) => (ActionStatus::Failed, Some(bounded(&error.to_string()))),
                        }
                    };
                    if matches!(status, ActionStatus::Applied | ActionStatus::WouldDo) {
                        rollback_entries.push(RollbackEntry {
                            object: target,
                            original_url: None,
                            original_name: Some(duplicate.name.clone()),
                            quarantined_as: Some(new_name.clone()),
                        });
                    }
                    push_entry(
                        &mut entries,
                        &mut counts,
                        AuditAction::QuarantineRename,
                        duplicate_ref,
                        Some(EntryValue::name(&duplicate.name)),
                        Some(EntryValue::name(&new_name)),
                        status,
                        error,
                        Some(format!(
                            "quarantine marker only; this module never \
                             deletes (observe {}h before any manual removal)",
                            plan.quarantine_hours
                        )),
                    );
                }
            }
        }
    }

    finish_apply(entries, rollback_entries, counts, plan, dry_run)
}

#[allow(clippy::too_many_arguments)]
fn push_entry(
    entries: &mut Vec<AuditEntry>,
    counts: &mut ApplyCounts,
    action: AuditAction,
    object: StoreRef,
    before: Option<EntryValue>,
    after: Option<EntryValue>,
    status: ActionStatus,
    error: Option<String>,
    detail: Option<String>,
) {
    match status {
        ActionStatus::Applied => counts.applied += 1,
        ActionStatus::WouldDo => counts.would_do += 1,
        ActionStatus::Failed => counts.failed += 1,
        ActionStatus::Skipped => counts.skipped += 1,
        ActionStatus::AlreadyCurrent | ActionStatus::AlreadyRestored => counts.already_current += 1,
    }
    entries.push(AuditEntry {
        action,
        object,
        before,
        after,
        status,
        at: now_rfc3339(),
        error,
        detail,
    });
}

fn finish_apply(
    entries: Vec<AuditEntry>,
    rollback_entries: Vec<RollbackEntry>,
    counts: ApplyCounts,
    plan: &MigrationPlan,
    dry_run: bool,
) -> ApplyReport {
    ApplyReport {
        schema: PLAN_SCHEMA,
        applied_at: now_rfc3339(),
        dry_run,
        plan_generated_at: plan.generated_at.clone(),
        entries,
        rollback_entries,
        counts,
    }
}

// ---------------------------------------------------------------------------
// rollback
// ---------------------------------------------------------------------------

/// Roll back a prior apply: restore original URLs via the update-only path,
/// and rename quarantined duplicates back when renames were applied and the
/// store supports renaming.
///
/// Idempotent: a restore that finds the original value already in place is
/// recorded `already_restored` without a write.
pub async fn rollback(report: &ApplyReport, store: &dyn MigrationStore) -> RollbackReport {
    restore(&report.rollback_entries, store).await
}

/// Best-effort rollback straight from a plan (guidance §4.2's
/// `--rollback migration-report.json`). The plan's rollback list reflects the
/// scanned state; prefer [`rollback`] with the apply report when one exists.
pub async fn rollback_plan(plan: &MigrationPlan, store: &dyn MigrationStore) -> RollbackReport {
    restore(&plan.rollback_entries, store).await
}

async fn restore(entries: &[RollbackEntry], store: &dyn MigrationStore) -> RollbackReport {
    let mut out: Vec<AuditEntry> = Vec::new();
    let mut counts = RollbackCounts::default();
    for entry in entries {
        if let Some(original_url) = &entry.original_url {
            let target = original_url.raw().to_string();
            match store.record(&entry.object).await {
                Err(error) => push_restore(
                    &mut out,
                    &mut counts,
                    AuditAction::RestoreUrl,
                    entry.object.clone(),
                    None,
                    Some(EntryValue::url(&target)),
                    ActionStatus::Failed,
                    Some(bounded(&error.to_string())),
                    None,
                ),
                Ok(None) => push_restore(
                    &mut out,
                    &mut counts,
                    AuditAction::RestoreUrl,
                    entry.object.clone(),
                    None,
                    Some(EntryValue::url(&target)),
                    ActionStatus::Skipped,
                    None,
                    Some("object absent; nothing to restore".to_string()),
                ),
                Ok(Some(record)) => {
                    let current = record.get("url").and_then(Value::as_str);
                    match current {
                        Some(current) if current == target => push_restore(
                            &mut out,
                            &mut counts,
                            AuditAction::RestoreUrl,
                            entry.object.clone(),
                            Some(EntryValue::url(current)),
                            Some(EntryValue::url(&target)),
                            ActionStatus::AlreadyRestored,
                            None,
                            Some("original URL already in place".to_string()),
                        ),
                        None => push_restore(
                            &mut out,
                            &mut counts,
                            AuditAction::RestoreUrl,
                            entry.object.clone(),
                            None,
                            Some(EntryValue::url(&target)),
                            ActionStatus::Failed,
                            None,
                            Some("record has no upstream url to restore".to_string()),
                        ),
                        Some(current) => {
                            let mut payload = record.clone();
                            if payload.as_object_mut().is_none() {
                                push_restore(
                                    &mut out,
                                    &mut counts,
                                    AuditAction::RestoreUrl,
                                    entry.object.clone(),
                                    None,
                                    Some(EntryValue::url(&target)),
                                    ActionStatus::Failed,
                                    None,
                                    Some("stored record is not an object".to_string()),
                                );
                                continue;
                            }
                            payload["url"] = Value::String(target.clone());
                            match store.update(&entry.object, &payload).await {
                                Ok(_) => push_restore(
                                    &mut out,
                                    &mut counts,
                                    AuditAction::RestoreUrl,
                                    entry.object.clone(),
                                    Some(EntryValue::url(current)),
                                    Some(EntryValue::url(&target)),
                                    ActionStatus::Applied,
                                    None,
                                    None,
                                ),
                                Err(error) => push_restore(
                                    &mut out,
                                    &mut counts,
                                    AuditAction::RestoreUrl,
                                    entry.object.clone(),
                                    Some(EntryValue::url(current)),
                                    Some(EntryValue::url(&target)),
                                    ActionStatus::Failed,
                                    Some(bounded(&error.to_string())),
                                    None,
                                ),
                            }
                        }
                    }
                }
            }
        }
        if let Some(original_name) = &entry.original_name {
            let address = entry
                .quarantined_as
                .as_deref()
                .unwrap_or(&entry.object.name);
            let address_ref = StoreRef {
                kind: entry.object.kind,
                name: address.to_string(),
            };
            if !store.rename_supported() {
                push_restore(
                    &mut out,
                    &mut counts,
                    AuditAction::RestoreName,
                    address_ref,
                    Some(EntryValue::name(address)),
                    Some(EntryValue::name(original_name)),
                    ActionStatus::Skipped,
                    None,
                    Some("renaming via update is not supported by this store".to_string()),
                );
                continue;
            }
            match store.record(&address_ref).await {
                Err(error) => push_restore(
                    &mut out,
                    &mut counts,
                    AuditAction::RestoreName,
                    address_ref,
                    Some(EntryValue::name(address)),
                    Some(EntryValue::name(original_name)),
                    ActionStatus::Failed,
                    Some(bounded(&error.to_string())),
                    None,
                ),
                Ok(None) => push_restore(
                    &mut out,
                    &mut counts,
                    AuditAction::RestoreName,
                    address_ref,
                    None,
                    Some(EntryValue::name(original_name)),
                    ActionStatus::AlreadyRestored,
                    None,
                    Some(
                        "quarantined object absent (already restored, or the \
                         planned rename never happened)"
                            .to_string(),
                    ),
                ),
                Ok(Some(record)) => {
                    let already = record
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|name| name == original_name)
                        .unwrap_or(false);
                    if already {
                        push_restore(
                            &mut out,
                            &mut counts,
                            AuditAction::RestoreName,
                            address_ref,
                            Some(EntryValue::name(address)),
                            Some(EntryValue::name(original_name)),
                            ActionStatus::AlreadyRestored,
                            None,
                            Some("original name already in place".to_string()),
                        );
                        continue;
                    }
                    let original_ref = StoreRef {
                        kind: entry.object.kind,
                        name: original_name.clone(),
                    };
                    if matches!(store.record(&original_ref).await, Ok(Some(_))) {
                        push_restore(
                            &mut out,
                            &mut counts,
                            AuditAction::RestoreName,
                            address_ref,
                            Some(EntryValue::name(address)),
                            Some(EntryValue::name(original_name)),
                            ActionStatus::Skipped,
                            None,
                            Some("original name is occupied by another object".to_string()),
                        );
                        continue;
                    }
                    let mut payload = record.clone();
                    if payload.as_object_mut().is_none() {
                        push_restore(
                            &mut out,
                            &mut counts,
                            AuditAction::RestoreName,
                            address_ref,
                            Some(EntryValue::name(address)),
                            Some(EntryValue::name(original_name)),
                            ActionStatus::Failed,
                            None,
                            Some("stored record is not an object".to_string()),
                        );
                        continue;
                    }
                    payload["name"] = Value::String(original_name.clone());
                    match store.update(&address_ref, &payload).await {
                        Ok(_) => push_restore(
                            &mut out,
                            &mut counts,
                            AuditAction::RestoreName,
                            address_ref,
                            Some(EntryValue::name(address)),
                            Some(EntryValue::name(original_name)),
                            ActionStatus::Applied,
                            None,
                            None,
                        ),
                        Err(error) => push_restore(
                            &mut out,
                            &mut counts,
                            AuditAction::RestoreName,
                            address_ref,
                            Some(EntryValue::name(address)),
                            Some(EntryValue::name(original_name)),
                            ActionStatus::Failed,
                            Some(bounded(&error.to_string())),
                            None,
                        ),
                    }
                }
            }
        }
        if entry.original_url.is_none() && entry.original_name.is_none() {
            push_restore(
                &mut out,
                &mut counts,
                AuditAction::RestoreUrl,
                entry.object.clone(),
                None,
                None,
                ActionStatus::Skipped,
                None,
                Some("rollback entry carries nothing to restore".to_string()),
            );
        }
    }
    RollbackReport {
        schema: PLAN_SCHEMA,
        restored_at: now_rfc3339(),
        counts,
        entries: out,
    }
}

#[allow(clippy::too_many_arguments)]
fn push_restore(
    entries: &mut Vec<AuditEntry>,
    counts: &mut RollbackCounts,
    action: AuditAction,
    object: StoreRef,
    before: Option<EntryValue>,
    after: Option<EntryValue>,
    status: ActionStatus,
    error: Option<String>,
    detail: Option<String>,
) {
    match status {
        ActionStatus::Applied => counts.restored += 1,
        ActionStatus::AlreadyRestored => counts.already_restored += 1,
        ActionStatus::Failed => counts.failed += 1,
        _ => counts.skipped += 1,
    }
    entries.push(AuditEntry {
        action,
        object,
        before,
        after,
        status,
        at: now_rfc3339(),
        error,
        detail,
    });
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The digest tier's unit: sha256 over the sorted, name-insensitive node
/// fingerprint list. Renaming or reordering a subscription's nodes does not
/// change the digest; changing a node's connection parameters does.
pub fn node_digest(proxies: &[Value]) -> String {
    let mut fingerprints: Vec<String> = proxies.iter().map(fingerprint_proxy).collect();
    fingerprints.sort();
    content_digest(&fingerprints.join("\n"))
}

enum Formed {
    SourceId,
    Digest,
}

enum GroupDigest {
    Unknown,
    Value(String),
    Unavailable,
}

/// Working group while scanning. `seed_stem` matters only for tier-4 groups.
struct WorkGroup {
    action: GroupAction,
    formed: Formed,
    source_id: Option<String>,
    urls: Vec<String>,
    digest: GroupDigest,
    seed_stem: String,
    canonical: CandidateRef,
    duplicates: Vec<CandidateRef>,
}

impl WorkGroup {
    fn new(
        action: GroupAction,
        formed: Formed,
        source_id: Option<String>,
        canonical: CandidateRef,
        url: Option<String>,
    ) -> Self {
        let mut urls = Vec::new();
        if let Some(url) = url {
            urls.push(url);
        }
        Self {
            action,
            formed,
            source_id,
            urls,
            digest: GroupDigest::Unknown,
            seed_stem: String::new(),
            canonical,
            duplicates: Vec::new(),
        }
    }

    fn add_member(&mut self, candidate: CandidateRef, url: Option<String>) {
        if let Some(url) = url {
            if !self.urls.contains(&url) {
                self.urls.push(url);
            }
        }
        self.duplicates.push(candidate);
    }
}

fn base_candidate(
    object: &StoreObject,
    url: Option<RedactedUrl>,
    digest: &Option<String>,
) -> CandidateRef {
    CandidateRef {
        kind: object.kind,
        name: object.name.clone(),
        url,
        digest: digest.clone(),
        class: ObjectClass::Uncertain,
        reason: None,
        note: None,
    }
}

/// Fetch one object's content and compute its node-set digest, honouring the
/// URL cache and the scan cap. `Err` carries the report note.
async fn digest_of(
    store: &dyn MigrationStore,
    object: &StoreRef,
    url: Option<&str>,
    fetches: &mut usize,
    cap: usize,
    cache: &mut HashMap<String, String>,
) -> Result<String, String> {
    if let Some(url) = url {
        if let Some(digest) = cache.get(url) {
            return Ok(digest.clone());
        }
    }
    if *fetches >= cap {
        return Err(format!("digest fetch skipped: cap {cap} reached"));
    }
    *fetches += 1;
    let proxies = match store.content(object).await {
        Ok(Some(proxies)) => proxies,
        Ok(None) => return Err("no downloadable content for this object".to_string()),
        Err(error) => {
            return Err(format!(
                "content fetch failed: {}",
                bounded(&error.to_string())
            ))
        }
    };
    if proxies.is_empty() {
        return Err("content has an empty node set".to_string());
    }
    let digest = node_digest(&proxies);
    if let Some(url) = url {
        cache.insert(url.to_string(), digest.clone());
    }
    Ok(digest)
}

/// Tier 4's similarity rule: after stripping the publish prefix, lowercasing
/// and dropping non-alphanumerics, one name is a prefix of the other and the
/// shorter is at least three characters. Deliberately loose -- the tier only
/// produces report-only groups.
fn normalized_stem(name: &str, prefix: &str) -> String {
    let stripped = name.strip_prefix(prefix).unwrap_or(name);
    stripped
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn names_similar(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= 3 && long.starts_with(short)
}

/// RFC 3339 UTC timestamp from a Unix instant (`2026-11-04T12:34:56Z`).
/// Hand-rolled because this crate's dependency set has no time crate and the
/// format must not drift between entries. Shared with the B8 cleanup leg.
pub(crate) fn rfc3339_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z",
        h = tod / 3600,
        m = (tod % 3600) / 60,
        s = tod % 60,
    )
}

pub(crate) fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rfc3339_utc(secs)
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (y, m, d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

// `fetch.rs` helpers duplicated on purpose: they are module-private there and
// this batch must not modify fetch.rs.

/// Read a response body, refusing anything past 8 MiB (same bound as the
/// fetch path).
async fn bounded_body(mut response: reqwest::Response) -> Result<String, String> {
    const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| format!("{}: {err}", kind_of(&err)))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(format!("response body exceeds {MAX_BODY_BYTES} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// `store.Client._is_missing`: a 500 that really means "absent".
fn is_missing(status: u16, text: &str) -> bool {
    if status != 500 {
        return false;
    }
    if text.contains("SUBSCRIPTION_NOT_FOUND")
        || text.contains("RESOURCE_NOT_FOUND")
        || text.contains("Cannot convert undefined or null to object")
    {
        return true;
    }
    match serde_json::from_str::<Value>(text) {
        Ok(payload) => payload
            .get("error")
            .and_then(Value::as_object)
            .map(|error| {
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .map(|code| code.contains("NOT_FOUND"))
                    .unwrap_or(false)
                    || error.get("details") == Some(&Value::from(404))
            })
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn summarize(text: &str) -> String {
    let head = text.trim_start().chars().take(160).collect::<String>();
    head.lines().next().unwrap_or("").to_string()
}

fn kind_of(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else if err.is_redirect() {
        "redirect"
    } else {
        "request"
    }
}

fn bounded(text: &str) -> String {
    text.chars().take(ERROR_LIMIT).collect()
}

/// `urllib.parse.quote(value, safe="")`.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use serde_json::{json, Value};

    use super::*;

    // -- fixtures -----------------------------------------------------------

    const CLASH_A: &str = "proxies:\n  - {name: n1, type: ss, server: 1.1.1.1, port: 443}\n  \
                           - {name: n2, type: ss, server: 2.2.2.2, port: 443}\n";
    const CLASH_B: &str = "proxies:\n  - {name: n3, type: ss, server: 3.3.3.3, port: 443}\n";
    const CLASH_C: &str = "proxies:\n  - {name: n4, type: ss, server: 4.4.4.4, port: 443}\n";
    const CLASH_D: &str = "proxies:\n  - {name: n5, type: ss, server: 5.5.5.5, port: 443}\n";

    /// A stateful Sub-Store stub on a real listener, in the shape of the
    /// `fetch.rs` admin tests: canned routes, one-shot faults, and a request
    /// log that every test pins to DELETE-free.
    #[derive(Clone)]
    struct Stub(Arc<Mutex<StubState>>);

    struct StubState {
        subs: BTreeMap<String, Value>,
        content: BTreeMap<String, String>,
        faults: VecDeque<Fault>,
        log: Vec<String>,
    }

    struct Fault {
        method: &'static str,
        path: String,
        status: u16,
        body: String,
    }

    impl Stub {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(StubState {
                subs: BTreeMap::new(),
                content: BTreeMap::new(),
                faults: VecDeque::new(),
                log: Vec::new(),
            })))
        }

        fn add_sub(&self, name: &str, url: Option<&str>) {
            self.0.lock().unwrap().subs.insert(
                name.to_string(),
                json!({
                    "name": name,
                    "url": url.unwrap_or(""),
                    "source": "remote",
                    "displayName": name,
                }),
            );
        }

        fn set_content(&self, name: &str, yaml: &str) {
            self.0
                .lock()
                .unwrap()
                .content
                .insert(name.to_string(), yaml.to_string());
        }

        fn fault(&self, method: &'static str, path: &str, status: u16, body: &str) {
            self.0.lock().unwrap().faults.push_back(Fault {
                method,
                path: path.to_string(),
                status,
                body: body.to_string(),
            });
        }

        fn note(&self, line: String) {
            self.0.lock().unwrap().log.push(line);
        }

        fn take_fault(&self, method: &str, path: &str) -> Option<(u16, String)> {
            let mut state = self.0.lock().unwrap();
            let index = state
                .faults
                .iter()
                .position(|fault| fault.method == method && fault.path == path)?;
            let fault = state.faults.remove(index).unwrap();
            Some((fault.status, fault.body))
        }

        fn log(&self) -> Vec<String> {
            self.0.lock().unwrap().log.clone()
        }

        fn record_url(&self, name: &str) -> Option<String> {
            self.0
                .lock()
                .unwrap()
                .subs
                .get(name)
                .and_then(|record| record.get("url"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }

        fn has_sub(&self, name: &str) -> bool {
            self.0.lock().unwrap().subs.contains_key(name)
        }

        async fn serve(self) -> String {
            let app = Router::new()
                .route("/api/subs", get(list_subs))
                .route("/api/sub/{name}", get(get_sub).patch(patch_sub))
                .route("/download/{name}", get(download))
                .fallback(not_found)
                .with_state(self.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://{addr}")
        }
    }

    async fn not_found() -> Response {
        (StatusCode::NOT_FOUND, "unmapped route".to_string()).into_response()
    }

    async fn list_subs(State(stub): State<Stub>) -> Response {
        stub.note("GET /api/subs".to_string());
        if let Some((status, body)) = stub.take_fault("GET", "/api/subs") {
            return (StatusCode::from_u16(status).unwrap(), body).into_response();
        }
        let state = stub.0.lock().unwrap();
        let items: Vec<Value> = state.subs.values().cloned().collect();
        (StatusCode::OK, serde_json::to_string(&items).unwrap()).into_response()
    }

    async fn get_sub(State(stub): State<Stub>, Path(name): Path<String>) -> Response {
        let path = format!("/api/sub/{name}");
        stub.note(format!("GET {path}"));
        if let Some((status, body)) = stub.take_fault("GET", &path) {
            return (StatusCode::from_u16(status).unwrap(), body).into_response();
        }
        let state = stub.0.lock().unwrap();
        match state.subs.get(&name) {
            Some(record) => {
                (StatusCode::OK, serde_json::to_string(record).unwrap()).into_response()
            }
            None => (StatusCode::NOT_FOUND, "not found".to_string()).into_response(),
        }
    }

    async fn patch_sub(
        State(stub): State<Stub>,
        Path(name): Path<String>,
        body: String,
    ) -> Response {
        let path = format!("/api/sub/{name}");
        stub.note(format!("PATCH {path} {body}"));
        if let Some((status, fault_body)) = stub.take_fault("PATCH", &path) {
            return (StatusCode::from_u16(status).unwrap(), fault_body).into_response();
        }
        let payload: Value = match serde_json::from_str(&body) {
            Ok(payload) => payload,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "bad json".to_string()).into_response();
            }
        };
        // Model rename-through-PATCH: a payload whose `name` differs from the
        // path re-keys the record. This is the semantics rename_supported()
        // would rely on, which is exactly why the real store keeps it off
        // until verified.
        let mut state = stub.0.lock().unwrap();
        state.subs.remove(&name);
        let new_name = payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&name)
            .to_string();
        state.subs.insert(new_name, payload);
        (StatusCode::OK, "{}".to_string()).into_response()
    }

    async fn download(State(stub): State<Stub>, Path(name): Path<String>) -> Response {
        let path = format!("/download/{name}");
        stub.note(format!("GET {path}"));
        if let Some((status, body)) = stub.take_fault("GET", &path) {
            return (StatusCode::from_u16(status).unwrap(), body).into_response();
        }
        let state = stub.0.lock().unwrap();
        match state.content.get(&name) {
            Some(text) => (StatusCode::OK, text.clone()).into_response(),
            None => (StatusCode::NOT_FOUND, "no content".to_string()).into_response(),
        }
    }

    async fn setup() -> (Stub, String) {
        let stub = Stub::new();
        let backend = stub.clone().serve().await;
        (stub, backend)
    }

    /// The rename-capable wrapper: delegates everything to the HTTP store and
    /// flips the rename capability, the way a verified future client would.
    struct RenameCapable(SubStoreHttpStore);

    impl MigrationStore for RenameCapable {
        fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<StoreObject>, MigrationError>> {
            self.0.list()
        }
        fn record<'a>(
            &'a self,
            object: &'a StoreRef,
        ) -> BoxFuture<'a, Result<Option<Value>, MigrationError>> {
            self.0.record(object)
        }
        fn content<'a>(
            &'a self,
            object: &'a StoreRef,
        ) -> BoxFuture<'a, Result<Option<Vec<Value>>, MigrationError>> {
            self.0.content(object)
        }
        fn update<'a>(
            &'a self,
            object: &'a StoreRef,
            payload: &'a Value,
        ) -> BoxFuture<'a, Result<String, MigrationError>> {
            self.0.update(object, payload)
        }
        fn rename_supported(&self) -> bool {
            true
        }
    }

    fn assert_no_delete(log: &[String]) {
        assert!(
            !log.iter().any(|line| line.starts_with("DELETE")),
            "a DELETE request was observed: {log:?}"
        );
    }

    fn write_count(log: &[String]) -> usize {
        log.iter()
            .filter(|line| {
                line.starts_with("PATCH") || line.starts_with("POST") || line.starts_with("DELETE")
            })
            .count()
    }

    fn stable_url_for(artifact: &ArtifactRef) -> String {
        format!(
            "https://probe.example.com/artifacts/{}/subscription",
            artifact.source_id.as_deref().unwrap_or(&artifact.key)
        )
    }

    fn map_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    // -- unit tests ---------------------------------------------------------

    #[test]
    fn redact_url_strips_query_and_fragment() {
        assert_eq!(
            redact_url("https://up.example/s?token=AAA&x=1"),
            "https://up.example/s?<redacted>"
        );
        assert_eq!(
            redact_url("https://up.example/s#frag"),
            "https://up.example/s?<redacted>"
        );
        assert_eq!(redact_url("https://up.example/s"), "https://up.example/s");
    }

    #[test]
    fn redacted_url_is_redacted_in_debug_and_serde() {
        let url = RedactedUrl::new("https://up.example/s?token=AAA");
        assert!(!format!("{url:?}").contains("token=AAA"));
        let json = serde_json::to_string(&url).unwrap();
        assert!(!json.contains("token=AAA"));
        assert!(json.contains("?<redacted>"));
        // The raw value survives in memory for rollback.
        assert_eq!(url.raw(), "https://up.example/s?token=AAA");
    }

    #[test]
    fn rfc3339_utc_known_instants() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_735_689_600), "2025-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_762_214_400), "2025-11-04T00:00:00Z");
        assert_eq!(rfc3339_utc(86_399), "1970-01-01T23:59:59Z");
    }

    #[test]
    fn node_digest_ignores_names_and_order() {
        let a = json!({"name": "n1", "type": "ss", "server": "1.1.1.1", "port": 443});
        let b = json!({"name": "renamed", "type": "ss", "server": "2.2.2.2", "port": 443});
        let digest = node_digest(&[a.clone(), b.clone()]);
        // Same nodes, reordered and renamed: same digest.
        assert_eq!(node_digest(&[b, a]), digest);
        // A changed connection parameter: different digest.
        let changed = json!({"name": "n1", "type": "ss", "server": "1.1.1.1", "port": 8443});
        assert_ne!(node_digest(&[changed]), digest);
        assert_eq!(digest.len(), 64, "sha256 hex");
    }

    #[test]
    fn name_similarity_rules() {
        assert!(names_similar("air", "airold"));
        assert!(names_similar("airold", "air"));
        assert!(names_similar("air", "air")); // equal stems
        assert!(!names_similar("ab", "abc"), "too short a stem");
        assert!(!names_similar("r1", "r2"), "siblings are not prefixes");
        assert!(!names_similar("alpha", "omega"));
        // Normalization: case and separators are dropped before comparing.
        assert_eq!(normalized_stem("probe-Air-Old", "probe-"), "airold");
        assert!(names_similar(
            &normalized_stem("probe-Air", "probe-"),
            &normalized_stem("probe-air-old", "probe-")
        ));
    }

    #[test]
    fn scan_caps_default_to_the_guidance_values() {
        let caps = ScanCaps::default();
        assert_eq!(caps.digest_fetch_cap, 50);
        assert_eq!(caps.quarantine_hours, 24);
    }

    #[tokio::test]
    async fn scan_rejects_an_empty_prefix() {
        // The store is never reached; a dead address proves that.
        let store = SubStoreHttpStore::new("http://127.0.0.1:1");
        let error = scan(&store, "", None, ScanCaps::default())
            .await
            .unwrap_err();
        assert!(matches!(error, MigrationError::Unsupported(_)), "{error}");
    }

    #[tokio::test]
    async fn apply_refuses_a_foreign_plan_schema_without_writes() {
        let store = SubStoreHttpStore::new("http://127.0.0.1:1");
        let plan: MigrationPlan = serde_json::from_value(json!({
            "schema": 99,
            "generated_at": "2026-01-01T00:00:00Z",
            "prefix": "probe-",
            "coverage": {"subscriptions": "covered", "collections": "not_run: x"},
            "quarantine_hours": 24,
            "digest_fetch_cap": 50,
            "fetches_used": 0,
            "counts": {"keep": 0, "duplicate": 0, "uncertain": 0, "untouched": 0},
            "groups": [],
            "solo": [],
            "untouched": [],
            "actions": [],
            "rollback_entries": [],
        }))
        .unwrap();
        let report = apply(&plan, &store, stable_url_for, true).await;
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].status, ActionStatus::Failed);
        assert!(report.entries[0].error.as_deref().unwrap().contains("99"));
    }

    // -- scan: grouping tiers -------------------------------------------------

    #[tokio::test]
    async fn scan_groups_by_saved_source_id() {
        let (stub, backend) = setup().await;
        stub.add_sub("probe-air", Some("https://up.example/a?token=AAA"));
        stub.add_sub("probe-air-old", Some("https://up.example/old?token=BBB"));
        stub.set_content("probe-air", CLASH_A);
        stub.set_content("probe-air-old", CLASH_A);
        let store = SubStoreHttpStore::new(&backend);

        let map = map_of(&[("air", "sid-1"), ("air-old", "sid-1")]);
        let plan = scan(&store, DEFAULT_PREFIX, Some(&map), ScanCaps::default())
            .await
            .unwrap();

        assert_no_delete(&stub.log());
        assert_eq!(plan.groups.len(), 1);
        let group = &plan.groups[0];
        assert_eq!(group.action, GroupAction::Retarget);
        assert_eq!(group.reason, GroupReason::SavedSourceId);
        assert_eq!(group.source_id.as_deref(), Some("sid-1"));
        assert_eq!(group.canonical.name, "probe-air");
        assert_eq!(group.canonical.reason, Some(GroupReason::SavedSourceId));
        assert_eq!(group.duplicates.len(), 1);
        assert_eq!(group.duplicates[0].name, "probe-air-old");
        assert_eq!(group.duplicates[0].reason, Some(GroupReason::SavedSourceId));
        assert_eq!(group.duplicates[0].class, ObjectClass::Duplicate);
        // Saved ids need no content fetch at all.
        assert_eq!(plan.fetches_used, 0);
        assert_eq!(plan.counts.keep, 1);
        assert_eq!(plan.counts.duplicate, 1);
    }

    #[tokio::test]
    async fn scan_groups_by_identical_url() {
        let (stub, backend) = setup().await;
        stub.add_sub("probe-x", Some("https://up.example/x?token=AAA"));
        stub.add_sub("probe-x-old", Some("https://up.example/x?token=AAA"));
        stub.set_content("probe-x", CLASH_A);
        stub.set_content("probe-x-old", CLASH_B);
        let store = SubStoreHttpStore::new(&backend);

        let plan = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap();

        assert_no_delete(&stub.log());
        assert_eq!(plan.groups.len(), 1);
        let group = &plan.groups[0];
        assert_eq!(group.action, GroupAction::Retarget);
        // The group was discovered through its first member's digest; the
        // second member was pulled in by the identical upstream URL, which is
        // the evidence recorded per entry.
        assert_eq!(group.reason, GroupReason::SameNodeDigest);
        assert_eq!(group.canonical.name, "probe-x");
        assert_eq!(group.duplicates.len(), 1);
        assert_eq!(group.duplicates[0].reason, Some(GroupReason::SameUrl));
        // Only the canonical's content was fetched; the duplicate joined by
        // URL and needed no fetch of its own.
        assert_eq!(plan.fetches_used, 1);
    }

    #[tokio::test]
    async fn scan_groups_by_node_digest() {
        // Three probe-* subs sharing a node set, with distinct upstream URLs:
        // only the digest tier can tell they are one logical source.
        let (stub, backend) = setup().await;
        stub.add_sub("probe-air", Some("https://up.example/a?token=AAA"));
        stub.add_sub("probe-air-b", Some("https://up.example/b?token=BBB"));
        stub.add_sub("probe-air-c", Some("https://up.example/c?token=CCC"));
        stub.set_content("probe-air", CLASH_A);
        stub.set_content("probe-air-b", CLASH_A);
        stub.set_content("probe-air-c", CLASH_A);
        let store = SubStoreHttpStore::new(&backend);

        let plan = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap();

        assert_no_delete(&stub.log());
        assert_eq!(plan.groups.len(), 1);
        let group = &plan.groups[0];
        assert_eq!(group.action, GroupAction::Retarget);
        assert_eq!(group.reason, GroupReason::SameNodeDigest);
        assert_eq!(group.source_id, None);
        // Canonical = lexicographically smallest name.
        assert_eq!(group.canonical.name, "probe-air");
        assert_eq!(group.canonical.class, ObjectClass::Keep);
        assert_eq!(group.duplicates.len(), 2);
        assert!(
            group.duplicates.iter().all(|d| {
                d.reason == Some(GroupReason::SameNodeDigest) && d.class == ObjectClass::Duplicate
            }),
            "all duplicates join by digest: {:?}",
            group.duplicates
        );
        let digest = group.canonical.digest.clone().unwrap();
        assert_eq!(digest.len(), 64);
        assert!(group
            .duplicates
            .iter()
            .all(|d| d.digest.as_deref() == Some(digest.as_str())));
        assert_eq!(plan.fetches_used, 3);
        assert_eq!(plan.counts.keep, 1);
        assert_eq!(plan.counts.duplicate, 2);
    }

    #[tokio::test]
    async fn name_similarity_only_group_stays_report_only() {
        let (stub, backend) = setup().await;
        // Similar names, but different URLs AND different node sets: only the
        // name-similarity tier can connect them, so both stay uncertain and
        // nothing may ever be written.
        stub.add_sub("probe-gamma", Some("https://up.example/g?token=AAA"));
        stub.add_sub("probe-gamma-extra", Some("https://up.example/gx?token=BBB"));
        stub.set_content("probe-gamma", CLASH_C);
        stub.set_content("probe-gamma-extra", CLASH_D);
        let store = RenameCapable(SubStoreHttpStore::new(&backend));

        let plan = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap();
        assert_eq!(plan.groups.len(), 1);
        let group = &plan.groups[0];
        assert_eq!(group.action, GroupAction::ReportOnly);
        assert_eq!(group.reason, GroupReason::NameSimilarity);
        assert_eq!(group.canonical.name, "probe-gamma");
        assert_eq!(group.canonical.class, ObjectClass::Uncertain);
        assert_eq!(group.duplicates.len(), 1);
        assert_eq!(group.duplicates[0].class, ObjectClass::Uncertain);
        assert_eq!(plan.counts.uncertain, 2);

        // apply on a report-only plan performs zero writes, even for real.
        let report = apply(&plan, &store, stable_url_for, false).await;
        assert!(report.entries.is_empty(), "{:?}", report.entries);
        assert_eq!(write_count(&stub.log()), 0);
        assert_no_delete(&stub.log());
    }

    #[tokio::test]
    async fn digest_fetch_cap_is_honored() {
        let (stub, backend) = setup().await;
        stub.add_sub("probe-r1", Some("https://up.example/1?token=A"));
        stub.add_sub("probe-r2", Some("https://up.example/2?token=B"));
        stub.add_sub("probe-r3", Some("https://up.example/3?token=C"));
        stub.set_content("probe-r1", CLASH_A);
        stub.set_content("probe-r2", CLASH_A);
        stub.set_content("probe-r3", CLASH_A);
        let store = SubStoreHttpStore::new(&backend);

        let caps = ScanCaps {
            digest_fetch_cap: 2,
            quarantine_hours: 24,
        };
        let plan = scan(&store, DEFAULT_PREFIX, None, caps).await.unwrap();

        assert_no_delete(&stub.log());
        // Exactly two downloads happened; the third candidate was never fetched.
        let downloads = stub
            .log()
            .iter()
            .filter(|line| line.starts_with("GET /download/"))
            .count();
        assert_eq!(downloads, 2);
        assert_eq!(plan.fetches_used, 2);
        assert_eq!(plan.digest_fetch_cap, 2);
        // The first two still group by digest.
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].reason, GroupReason::SameNodeDigest);
        assert_eq!(plan.groups[0].duplicates.len(), 1);
        // The over-cap candidate lands report-only with a cap note.
        assert_eq!(plan.solo.len(), 1);
        let solo = &plan.solo[0];
        assert_eq!(solo.name, "probe-r3");
        assert_eq!(solo.class, ObjectClass::Uncertain);
        let note = solo.note.clone().unwrap();
        assert!(note.contains("cap 2"), "{note}");
        assert_eq!(plan.counts.uncertain, 1);
    }

    #[tokio::test]
    async fn a_mixed_scan_classifies_keep_duplicate_uncertain_and_untouched() {
        let (stub, backend) = setup().await;
        // probe-air: saved source_id; air-old joins by URL; beta joins by
        // digest after the group's canonical digest is fetched lazily.
        stub.add_sub("probe-air", Some("https://up.example/a?token=AAA"));
        stub.add_sub("probe-air-old", Some("https://up.example/a?token=AAA"));
        stub.add_sub("probe-beta", Some("https://up.example/b?token=BBB"));
        // probe-solo: unique URL and unique node set -> verified keep.
        stub.add_sub("probe-solo", Some("https://up.example/s?token=SSS"));
        // probe-broken: listed but not downloadable -> uncertain.
        stub.add_sub("probe-broken", Some("https://up.example/x?token=XXX"));
        // Not candidates at all.
        stub.add_sub("unrelated", Some("https://up.example/other?token=UUU"));
        stub.add_sub(
            "deprecated-probe-air-old",
            Some("https://up.example/dead?token=DDD"),
        );
        stub.set_content("probe-air", CLASH_A);
        stub.set_content("probe-air-old", CLASH_A);
        stub.set_content("probe-beta", CLASH_A);
        stub.set_content("probe-solo", CLASH_B);
        let store = SubStoreHttpStore::new(&backend);

        let map = map_of(&[("air", "11111111-2222-4333-8444-555555555555")]);
        let plan = scan(&store, DEFAULT_PREFIX, Some(&map), ScanCaps::default())
            .await
            .unwrap();

        assert_no_delete(&stub.log());
        // Scan is strictly read-only.
        assert!(
            stub.log().iter().all(|line| line.starts_with("GET ")),
            "scan issued a non-GET: {:?}",
            stub.log()
        );

        assert_eq!(plan.groups.len(), 1);
        let group = &plan.groups[0];
        assert_eq!(group.action, GroupAction::Retarget);
        assert_eq!(group.reason, GroupReason::SavedSourceId);
        assert_eq!(group.canonical.name, "probe-air");
        let dup_names: Vec<&str> = group.duplicates.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(dup_names, ["probe-air-old", "probe-beta"]);
        assert_eq!(group.duplicates[0].reason, Some(GroupReason::SameUrl));
        assert_eq!(
            group.duplicates[1].reason,
            Some(GroupReason::SameNodeDigest)
        );
        assert_eq!(
            plan.fetches_used, 4,
            "beta + canonical digest + solo + broken"
        );

        assert_eq!(plan.solo.len(), 2);
        let solo = plan.solo.iter().find(|s| s.name == "probe-solo").unwrap();
        assert_eq!(solo.class, ObjectClass::Keep);
        let broken = plan.solo.iter().find(|s| s.name == "probe-broken").unwrap();
        assert_eq!(broken.class, ObjectClass::Uncertain);
        assert!(broken
            .note
            .as_deref()
            .unwrap()
            .contains("no downloadable content"));

        let untouched: Vec<&str> = plan.untouched.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(untouched, ["deprecated-probe-air-old", "unrelated"]);

        assert_eq!(plan.counts.keep, 2);
        assert_eq!(plan.counts.duplicate, 2);
        assert_eq!(plan.counts.uncertain, 1);
        assert_eq!(plan.counts.untouched, 2);

        // Coverage: subscriptions covered, collections recorded NOT_RUN.
        assert!(plan.coverage.subscriptions.starts_with("covered"));
        assert!(plan.coverage.collections.starts_with("not_run"));
        assert_eq!(plan.quarantine_hours, 24);

        // Serialization redacts every URL and round-trips.
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains("token="), "leaked query string: {json}");
        assert!(json.contains("?<redacted>"));
        let back: MigrationPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            json,
            "redacted form is a fixed point"
        );

        // Plan actions: one retarget, two quarantine skips (rename unsupported).
        assert_eq!(plan.actions.len(), 3);
        assert_eq!(
            plan.actions[0],
            PlannedAction::Retarget {
                object: StoreRef {
                    kind: StoreKind::Sub,
                    name: "probe-air".to_string(),
                },
                group: 0,
            }
        );
        // Rollback list: the canonical's original URL (redacted in JSON).
        assert_eq!(plan.rollback_entries.len(), 1);
        assert_eq!(plan.rollback_entries[0].object.name, "probe-air");
        assert_eq!(
            plan.rollback_entries[0]
                .original_url
                .as_ref()
                .unwrap()
                .raw(),
            "https://up.example/a?token=AAA"
        );
    }

    #[tokio::test]
    async fn a_list_403_fails_the_whole_scan_with_a_typed_error() {
        let (stub, backend) = setup().await;
        stub.add_sub("probe-air", Some("https://up.example/a?token=AAA"));
        stub.fault("GET", "/api/subs", 403, "blocked by cloudflare");
        let store = SubStoreHttpStore::new(&backend);

        let error = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap_err();
        match error {
            MigrationError::List(message) => {
                assert!(message.contains("HTTP 403"), "{message}");
                assert!(!message.contains(&backend), "no backend in the error");
            }
            other => panic!("expected a List error, got: {other:?}"),
        }
        // No partial plan exists because scan returned Err.
    }

    #[tokio::test]
    async fn a_list_500_fails_the_whole_scan_too() {
        let (stub, backend) = setup().await;
        stub.fault("GET", "/api/subs", 500, "boom");
        let store = SubStoreHttpStore::new(&backend);

        let error = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap_err();
        assert!(matches!(error, MigrationError::List(_)), "{error}");
        let message = error.to_string();
        assert!(message.contains("HTTP 500"), "{message}");
    }

    // -- apply -----------------------------------------------------------------

    #[tokio::test]
    async fn a_dry_run_performs_zero_writes() {
        let (stub, backend) = setup().await;
        stub.add_sub("probe-air", Some("https://up.example/a?token=AAA"));
        stub.add_sub("probe-air-b", Some("https://up.example/b?token=BBB"));
        stub.add_sub("probe-air-c", Some("https://up.example/c?token=CCC"));
        stub.set_content("probe-air", CLASH_A);
        stub.set_content("probe-air-b", CLASH_A);
        stub.set_content("probe-air-c", CLASH_A);
        let store = RenameCapable(SubStoreHttpStore::new(&backend));

        let plan = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap();
        let writes_before = write_count(&stub.log());

        let report = apply(&plan, &store, stable_url_for, true).await;

        assert!(report.dry_run);
        assert_eq!(write_count(&stub.log()), writes_before, "dry run wrote");
        assert_no_delete(&stub.log());
        // 1 retarget + 2 renames, all previews.
        assert_eq!(report.entries.len(), 3);
        assert!(
            report
                .entries
                .iter()
                .all(|entry| entry.status == ActionStatus::WouldDo),
            "{:?}",
            report.entries
        );
        assert_eq!(report.counts.would_do, 3);
        // The preview still carries the rollback list for later.
        assert_eq!(report.rollback_entries.len(), 3);
        assert_eq!(
            report.rollback_entries[0]
                .original_url
                .as_ref()
                .unwrap()
                .raw(),
            "https://up.example/a?token=AAA"
        );
    }

    #[tokio::test]
    async fn an_apply_retargets_via_update_only_and_skips_unsupported_renames() {
        let (stub, backend) = setup().await;
        stub.add_sub("probe-air", Some("https://up.example/a?token=AAA"));
        stub.add_sub("probe-air-b", Some("https://up.example/b?token=BBB"));
        stub.add_sub("probe-air-c", Some("https://up.example/c?token=CCC"));
        stub.set_content("probe-air", CLASH_A);
        stub.set_content("probe-air-b", CLASH_A);
        stub.set_content("probe-air-c", CLASH_A);
        let store = SubStoreHttpStore::new(&backend);

        let plan = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap();
        let report = apply(&plan, &store, stable_url_for, false).await;

        assert_no_delete(&stub.log());
        // Exactly one write: the canonical retarget, update-only.
        let log = stub.log();
        let patches: Vec<String> = log
            .iter()
            .filter(|line| line.starts_with("PATCH "))
            .cloned()
            .collect();
        assert_eq!(patches.len(), 1, "{:?}", stub.log());
        assert!(
            patches[0].starts_with("PATCH /api/sub/probe-air "),
            "{}",
            patches[0]
        );
        assert!(
            patches[0].contains("\"url\":\"https://probe.example.com/artifacts/air/subscription\""),
            "{}",
            patches[0]
        );
        // Existence was checked before the write (never create).
        assert!(
            stub.log()
                .iter()
                .any(|line| line == "GET /api/sub/probe-air"),
            "existence check missing: {:?}",
            stub.log()
        );
        assert_eq!(
            stub.record_url("probe-air").as_deref(),
            Some("https://probe.example.com/artifacts/air/subscription")
        );

        // Duplicates untouched, but recorded as skipped renames.
        assert_eq!(report.entries.len(), 3);
        assert_eq!(report.entries[0].action, AuditAction::Retarget);
        assert_eq!(report.entries[0].status, ActionStatus::Applied);
        assert!(report.entries[1..].iter().all(|entry| {
            entry.action == AuditAction::QuarantineSkip
                && entry.status == ActionStatus::Skipped
                && entry.detail.as_deref().unwrap().contains("not supported")
        }));
        assert!(stub.has_sub("probe-air-b") && stub.has_sub("probe-air-c"));
        assert_eq!(report.counts.applied, 1);
        assert_eq!(report.counts.skipped, 2);

        // The report serializes redacted.
        let report_json = serde_json::to_string(&report).unwrap();
        assert!(!report_json.contains("token="), "{report_json}");
        assert!(report_json.contains("?<redacted>"));
    }

    #[tokio::test]
    async fn a_rename_capable_store_quarantines_duplicates_by_renaming() {
        let (stub, backend) = setup().await;
        stub.add_sub("probe-air", Some("https://up.example/a?token=AAA"));
        stub.add_sub("probe-air-b", Some("https://up.example/b?token=BBB"));
        stub.set_content("probe-air", CLASH_A);
        stub.set_content("probe-air-b", CLASH_A);
        let store = RenameCapable(SubStoreHttpStore::new(&backend));

        let plan = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap();
        let report = apply(&plan, &store, stable_url_for, false).await;

        assert_no_delete(&stub.log());
        assert_eq!(report.counts.applied, 2, "{:?}", report.entries);
        let log = stub.log();
        let rename: Vec<String> = log
            .iter()
            .filter(|line| line.starts_with("PATCH /api/sub/probe-air-b "))
            .cloned()
            .collect();
        assert_eq!(rename.len(), 1);
        assert!(
            rename[0].contains("\"name\":\"deprecated-probe-air-b\""),
            "{}",
            rename[0]
        );
        assert!(stub.has_sub("deprecated-probe-air-b"));
        assert!(!stub.has_sub("probe-air-b"));
        // The rollback list knows how to undo the rename.
        let rename_entry = report
            .rollback_entries
            .iter()
            .find(|entry| entry.original_name.is_some())
            .unwrap();
        assert_eq!(rename_entry.object.name, "deprecated-probe-air-b");
        assert_eq!(rename_entry.original_name.as_deref(), Some("probe-air-b"));
    }

    #[tokio::test]
    async fn a_failed_retarget_does_not_stop_other_groups() {
        let (stub, backend) = setup().await;
        // Group 1: URL pair. Group 2: digest pair.
        stub.add_sub("probe-air", Some("https://up.example/a?token=AAA"));
        stub.add_sub("probe-air-old", Some("https://up.example/a?token=AAA"));
        stub.add_sub("probe-be1", Some("https://up.example/1?token=E11"));
        stub.add_sub("probe-be2", Some("https://up.example/2?token=E22"));
        stub.set_content("probe-air", CLASH_A);
        stub.set_content("probe-air-old", CLASH_A);
        stub.set_content("probe-be1", CLASH_B);
        stub.set_content("probe-be2", CLASH_B);
        let store = SubStoreHttpStore::new(&backend);

        let plan = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap();
        assert_eq!(plan.groups.len(), 2);

        // The retarget of group 1's canonical hits a 404 on the record read;
        // the write cannot happen either way.
        stub.fault("GET", "/api/sub/probe-air", 404, "not found");
        let report = apply(&plan, &store, stable_url_for, false).await;

        assert_no_delete(&stub.log());
        let failed: Vec<&AuditEntry> = report
            .entries
            .iter()
            .filter(|entry| entry.status == ActionStatus::Failed)
            .collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].object.name, "probe-air");
        // The 404 reads as "absent" through the store, and apply answers the
        // only safe way: refuse to write rather than create.
        assert!(
            failed[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("refusing to write"),
            "{:?}",
            failed[0]
        );
        // Group 1's duplicate was left untouched (renaming it would strand
        // the source), recorded as skipped; group 2's duplicate also shows a
        // skip (this store cannot rename) -- but for a different reason.
        let skipped: Vec<&AuditEntry> = report
            .entries
            .iter()
            .filter(|entry| entry.status == ActionStatus::Skipped)
            .collect();
        assert_eq!(skipped.len(), 2);
        let stranded = skipped
            .iter()
            .find(|entry| entry.object.name == "probe-air-old")
            .unwrap();
        assert!(stranded
            .detail
            .as_deref()
            .unwrap()
            .contains("retarget did not succeed"));
        assert!(stub.has_sub("probe-air-old"));
        // Group 2 retargeted fine.
        assert_eq!(report.counts.applied, 1);
        assert_eq!(
            stub.record_url("probe-be1").as_deref(),
            Some("https://probe.example.com/artifacts/be1/subscription")
        );
    }

    // -- rollback ----------------------------------------------------------------

    #[tokio::test]
    async fn rollback_restores_the_original_url_exactly_and_twice() {
        let (stub, backend) = setup().await;
        stub.add_sub("probe-x", Some("https://up.example/x?token=AAA"));
        stub.add_sub("probe-x-old", Some("https://up.example/x?token=AAA"));
        stub.set_content("probe-x", CLASH_A);
        stub.set_content("probe-x-old", CLASH_B);
        let store = SubStoreHttpStore::new(&backend);

        let plan = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap();
        let report = apply(&plan, &store, stable_url_for, false).await;
        assert_eq!(report.counts.applied, 1);
        assert_eq!(
            stub.record_url("probe-x").as_deref(),
            Some("https://probe.example.com/artifacts/x/subscription")
        );

        let first = rollback(&report, &store).await;
        assert_no_delete(&stub.log());
        assert_eq!(first.counts.restored, 1);
        // The EXACT original, token query string included.
        assert_eq!(
            stub.record_url("probe-x").as_deref(),
            Some("https://up.example/x?token=AAA")
        );
        let entry = &first.entries[0];
        assert_eq!(entry.action, AuditAction::RestoreUrl);
        assert_eq!(entry.status, ActionStatus::Applied);
        assert_eq!(
            entry.before,
            Some(EntryValue::url(
                "https://probe.example.com/artifacts/x/subscription"
            ))
        );
        // The audit value is the redacted form; the raw token only lived in
        // memory.
        assert_eq!(
            entry.after,
            Some(EntryValue::url("https://up.example/x?token=AAA"))
        );
        assert!(entry.after.as_ref().unwrap().value.contains("?<redacted>"));
        assert!(!entry.at.is_empty());

        // Second rollback is a no-op: everything already in place.
        let writes_before = write_count(&stub.log());
        let second = rollback(&report, &store).await;
        assert_eq!(second.counts.already_restored, 1);
        assert_eq!(second.counts.restored, 0);
        assert_eq!(
            write_count(&stub.log()),
            writes_before,
            "second rollback wrote"
        );
        assert_eq!(
            stub.record_url("probe-x").as_deref(),
            Some("https://up.example/x?token=AAA")
        );
    }

    #[tokio::test]
    async fn rollback_renames_quarantined_duplicates_back_and_is_idempotent() {
        let (stub, backend) = setup().await;
        stub.add_sub("probe-m", Some("https://up.example/m?token=AAA"));
        stub.add_sub("probe-m-n", Some("https://up.example/n?token=BBB"));
        stub.set_content("probe-m", CLASH_A);
        stub.set_content("probe-m-n", CLASH_A);
        let store = RenameCapable(SubStoreHttpStore::new(&backend));

        let plan = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap();
        let report = apply(&plan, &store, stable_url_for, false).await;
        assert!(stub.has_sub("deprecated-probe-m-n"));
        assert!(!stub.has_sub("probe-m-n"));

        let first = rollback(&report, &store).await;
        assert_no_delete(&stub.log());
        assert_eq!(first.counts.restored, 2, "{:?}", first.entries);
        // Names and URL back to the scanned state.
        assert!(stub.has_sub("probe-m-n"));
        assert!(!stub.has_sub("deprecated-probe-m-n"));
        assert_eq!(
            stub.record_url("probe-m").as_deref(),
            Some("https://up.example/m?token=AAA")
        );

        let writes_before = write_count(&stub.log());
        let second = rollback(&report, &store).await;
        assert_eq!(second.counts.already_restored, 2);
        assert_eq!(second.counts.restored, 0);
        assert_eq!(
            write_count(&stub.log()),
            writes_before,
            "second rollback wrote"
        );
    }

    #[tokio::test]
    async fn rollback_from_a_plan_is_a_safe_best_effort() {
        let (stub, backend) = setup().await;
        stub.add_sub("probe-x", Some("https://up.example/x?token=AAA"));
        stub.add_sub("probe-x-old", Some("https://up.example/x?token=AAA"));
        stub.set_content("probe-x", CLASH_A);
        stub.set_content("probe-x-old", CLASH_B);
        let store = SubStoreHttpStore::new(&backend);

        let plan = scan(&store, DEFAULT_PREFIX, None, ScanCaps::default())
            .await
            .unwrap();
        assert_eq!(plan.rollback_entries.len(), 1);
        // Rollback the plan directly, without ever applying: the canonical's
        // URL is still the original, so nothing is written and the entry
        // says so.
        let report = rollback_plan(&plan, &store).await;
        assert_no_delete(&stub.log());
        assert_eq!(report.counts.already_restored, 1);
        assert_eq!(
            stub.record_url("probe-x").as_deref(),
            Some("https://up.example/x?token=AAA")
        );
    }
}
