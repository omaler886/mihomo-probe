//! Publication after the parent has selected nodes from the complete round.
//!
//! Feed original domain-form dictionaries from `Collected::originals`, not
//! per-IP `Collected::proxies`. Selection, region labels, and front remapping
//! stay with the round integrator. This module never substitutes for official
//! Sub-Store producers, strips topology, or decides which verdict is alive.
//!
//! Files are immutable and precede DB activation. Downloads must resolve the
//! DB active snapshot, never guess the newest file or use a mutable meta sidecar.
//! The absolute, existing export root must be owned exclusively by the service;
//! do not allow another principal to mutate it or its ancestors during writes.
//!
//! Round integration (select_source_exports, publish_round) turns one round's
//! verdicts into per-source bodies. It owns selection, region tags, and dialer
//! closure; the round guard still owns whether anything is published at all.
//! Partial coverage publishes -- only a breached alive floor, or an
//! inconclusive round that measured nothing, withholds a publication.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use probe_config::{PublishSection, SourceSpec};
use probe_source::{Role, SubAdmin};
use probe_storage::publication::{
    export_hash, validate_export_actor, validate_export_content, validate_export_key,
    ExportSnapshot, PublicationInput, PublicationResult, SnapshotSummary, MAX_EXPORT_BYTES,
    MAX_EXPORT_NODES,
};
use probe_storage::Storage;
use serde::Serialize;
use serde_json::Value;

use crate::collect::Collected;
use crate::round::{GuardDecision, NodeVerdict};

const BUILTIN_TARGETS: &[&str] = &["DIRECT", "REJECT", "REJECT-DROP", "PASS", "COMPATIBLE"];
const MAX_TOPOLOGY_EDGES: usize = MAX_EXPORT_NODES * 4;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

/// Errors contain static context and OS error kinds, never proxy values or paths.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("invalid export: {0}")]
    Invalid(&'static str),
    #[error("export filesystem operation failed: {0:?}")]
    Io(io::ErrorKind),
    #[error("{0}")]
    Storage(#[from] probe_domain::DomainError),
}

pub type PublishResult<T> = Result<T, PublishError>;

/// Validated secret-bearing output; intentionally not Debug or Serialize.
#[derive(Clone)]
pub struct RenderedExport {
    content: String,
    sha256: String,
    node_count: usize,
}

impl RenderedExport {
    /// Return exact YAML-compatible JSON bytes for an authenticated download.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Return the digest of the exact materialized bytes.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Return actual proxy dictionaries, including explicit dependency fronts.
    pub fn node_count(&self) -> usize {
        self.node_count
    }
}

/// Diffs contain opaque identities derived ONLY from kind/name, never credentials.
/// A rename is an add/remove; credential or topology changes under a name are changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SnapshotDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub previous_count: usize,
    pub current_count: usize,
    pub change_basis_points: u16,
}

impl SnapshotDiff {
    /// Count changed identities, including groups, for the absolute diff guard.
    pub fn changed_identities(&self) -> usize {
        self.added.len() + self.removed.len() + self.changed.len()
    }
}

/// Supply the complete round/source guard result; no verdicts are reinterpreted here.
#[derive(Debug, Clone, Copy)]
pub struct PublishPolicy {
    pub enabled: bool,
    pub guarded: bool,
    pub max_changed_identities: Option<usize>,
    /// 0..=10_000; first publication has no baseline and skips diff limits.
    pub max_change_basis_points: Option<u16>,
}

/// Disabled/empty cannot be forced. Guard/diff overrides require `force = true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PublishDecision {
    Disabled,
    Empty,
    Guarded,
    DiffLimit,
    Publish { forced: bool },
}

/// Already-selected input; actor is a safe principal label, not an authorization token.
pub struct PublishRequest<'a> {
    pub key: &'a str,
    pub actor: &'a str,
    pub proxies: &'a [Value],
    pub groups: &'a [Value],
    pub policy: PublishPolicy,
    /// The authenticated parent must authorize this explicit override.
    pub force: bool,
}

/// Safe publication response; no proxy dictionaries or content are serialized.
#[derive(Debug, Clone, Serialize)]
pub struct PublishOutcome {
    pub decision: PublishDecision,
    pub diff: Option<SnapshotDiff>,
    pub publication: Option<PublicationResult>,
    pub content_path: Option<PathBuf>,
}

/// Encode selected dictionaries as JSON, a YAML subset with unambiguous scalars.
/// Preserve all fields and domain/SNI strings; reject ambiguous or open topology.
pub fn render_clash_meta(proxies: &[Value], groups: &[Value]) -> PublishResult<RenderedExport> {
    validate_topology(proxies, groups)?;
    let mut document = serde_json::json!({"proxies": proxies});
    if !groups.is_empty() {
        document["proxy-groups"] = Value::Array(groups.to_vec());
    }
    // Dictionary insertion order is not a meaningful publication change.
    // Array order IS meaningful (e.g. relay hops), and remains untouched.
    document.sort_all_objects();
    let mut output = BoundedOutput(Vec::new());
    serde_json::to_writer_pretty(&mut output, &document)
        .map_err(|_| PublishError::Invalid("serialization or size limit"))?;
    output
        .write_all(b"\n")
        .map_err(|_| PublishError::Invalid("size limit"))?;
    let content =
        String::from_utf8(output.0).map_err(|_| PublishError::Invalid("invalid UTF-8 output"))?;
    Ok(RenderedExport {
        sha256: export_hash(&content),
        content,
        node_count: proxies.len(),
    })
}

/// Compare validated exports; digest names rather than printing untrusted labels.
pub fn compare_snapshots(previous: &str, current: &str) -> PublishResult<SnapshotDiff> {
    let previous = parse_document(previous)?;
    let current = parse_document(current)?;
    let previous_entries = index_identities(&previous);
    let current_entries = index_identities(&current);
    let mut diff = SnapshotDiff {
        previous_count: previous["proxies"].as_array().map_or(0, Vec::len),
        current_count: current["proxies"].as_array().map_or(0, Vec::len),
        ..SnapshotDiff::default()
    };
    for (identity, value) in &current_entries {
        match previous_entries.get(identity) {
            None => diff.added.push(identity.clone()),
            Some(previous) if previous != value => diff.changed.push(identity.clone()),
            _ => {}
        }
    }
    for identity in previous_entries.keys() {
        if !current_entries.contains_key(identity) {
            diff.removed.push(identity.clone());
        }
    }
    // Union normalization counts complete replacement as 100%, not 200%.
    let union = previous_entries.len() + diff.added.len();
    if union > 0 {
        let changed = diff.changed_identities();
        diff.change_basis_points = ((changed * 10_000).div_ceil(union)) as u16;
    }
    Ok(diff)
}

/// Decide from policy/count/diff; forcing never permits disabled or zero-node output.
pub fn decide_publication(
    policy: PublishPolicy,
    node_count: usize,
    diff: Option<&SnapshotDiff>,
    force: bool,
) -> PublishResult<PublishDecision> {
    if policy
        .max_change_basis_points
        .is_some_and(|limit| limit > 10_000)
    {
        return Err(PublishError::Invalid(
            "diff limit must be at most 10000 basis points",
        ));
    }
    if !policy.enabled {
        return Ok(PublishDecision::Disabled);
    }
    if node_count == 0 {
        return Ok(PublishDecision::Empty);
    }
    if policy.guarded && !force {
        return Ok(PublishDecision::Guarded);
    }
    let exceeds_diff = diff.is_some_and(|diff| {
        policy
            .max_changed_identities
            .is_some_and(|limit| diff.changed_identities() > limit)
            || policy
                .max_change_basis_points
                .is_some_and(|limit| diff.change_basis_points > limit)
    });
    if exceeds_diff && !force {
        return Ok(PublishDecision::DiffLimit);
    }
    Ok(PublishDecision::Publish { forced: force })
}

/// Render, guard, durably materialize, then CAS-activate/audit one selected export.
/// Skips perform no filesystem writes. Any failure preserves the old DB pointer/file.
pub fn publish_export(
    storage: &Storage,
    root: &Path,
    request: PublishRequest<'_>,
) -> PublishResult<PublishOutcome> {
    validate_export_key(request.key)?;
    validate_export_actor(request.actor)?;
    let preliminary =
        decide_publication(request.policy, request.proxies.len(), None, request.force)?;
    if !matches!(preliminary, PublishDecision::Publish { .. }) {
        return Ok(skipped(preliminary, None));
    }
    let rendered = render_clash_meta(request.proxies, request.groups)?;
    let previous = storage.current_export(request.key)?;
    let diff = previous
        .as_ref()
        .map(|snapshot| compare_snapshots(&snapshot.content, rendered.content()))
        .transpose()?;
    let decision = decide_publication(
        request.policy,
        rendered.node_count(),
        diff.as_ref(),
        request.force,
    )?;
    if !matches!(decision, PublishDecision::Publish { .. }) {
        return Ok(skipped(decision, diff));
    }
    let (publication, path) = materialize_then(root, request.key, &rendered, || {
        storage.publish_export(PublicationInput {
            key: request.key,
            content: rendered.content(),
            sha256: rendered.sha256(),
            node_count: rendered.node_count(),
            actor: request.actor,
            forced: request.force,
            expected_active: previous.as_ref().map(|snapshot| snapshot.summary.id),
        })
    })?;
    Ok(PublishOutcome {
        decision,
        diff,
        publication: Some(publication),
        content_path: Some(path),
    })
}

/// Validate/materialize an explicit same-key target before audited CAS rollback.
/// Return metadata and immutable path; an absent/cross-key target is always rejected.
pub fn rollback_export(
    storage: &Storage,
    root: &Path,
    key: &str,
    target_id: i64,
    actor: &str,
    expected_active: Option<i64>,
) -> PublishResult<(SnapshotSummary, PathBuf)> {
    validate_export_actor(actor)?;
    let snapshot = storage
        .export_snapshot(key, target_id)?
        .ok_or(PublishError::Invalid(
            "rollback snapshot not found for export key",
        ))?;
    let rendered = render_snapshot(&snapshot)?;
    let (summary, path) = materialize_then(root, key, &rendered, || {
        storage.rollback_export(key, target_id, actor, expected_active)
    })?;
    Ok((summary, path))
}

/// One source's export body for this round, or the reason it has none.
#[derive(Debug, Clone)]
pub struct SourceExport {
    /// The source key. Subscription names such as `<prefix>-<key>-local` are
    /// the caller's, not this module's.
    pub key: String,
    /// Domain-form dictionaries in stable publish order; empty when skipped.
    pub proxies: Vec<Value>,
    /// Selection-level reason this source publishes nothing.
    pub skipped: Option<SkipReason>,
    /// Nodes left out of the body, with the reason.
    pub dropped: Vec<ExportDrop>,
}

impl SourceExport {
    /// A source with no body and one reason.
    fn skipped(key: String, reason: SkipReason) -> Self {
        Self {
            key,
            proxies: Vec::new(),
            skipped: Some(reason),
            dropped: Vec::new(),
        }
    }
}

/// Why a round or one source produced no publication.
///
/// A skipped source keeps whatever it published last round: an immutable file
/// and the ledger pointer only ever move through a successful publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `publish.enabled = false`: the round writes no files and activates nothing.
    PublishDisabled,
    /// The round guard withheld publication: the alive floor was breached, or
    /// the round was inconclusive *and* measured nothing at all. Partial
    /// coverage publishes.
    GuardNotApplied,
    /// `spec.enabled = false`.
    SourceDisabled,
    /// `spec.export = false`: tested and shown, but never published.
    SourceMuted,
    /// Nothing alive in this source this round. An empty export is never
    /// written -- it would replace a working subscription with zero nodes.
    NoAliveNodes,
}

/// A node left out of one source's body. The identity is the ledger
/// fingerprint, which is already opaque; node names are never echoed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportDrop {
    pub identity: String,
    pub reason: ExportDropReason,
}

impl ExportDrop {
    /// Record a dropped node by ledger identity.
    fn of(verdict: &NodeVerdict, reason: ExportDropReason) -> Self {
        Self {
            identity: verdict.job.fingerprint.clone(),
            reason,
        }
    }
}

/// Why a node could not enter an export body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportDropReason {
    /// The verdict has no original domain-form dictionary for its identity.
    MissingOriginal,
    /// The original dictionary has no usable name.
    UnusableOriginal,
    /// Another variant of the same display name was selected instead.
    SupersededVariant,
    /// The node's dialer names something this file does not publish, and one
    /// dangling dialer makes a client kernel reject the whole file.
    UnclosedDialer,
}

/// The topology field a client has to resolve inside the published file.
const DIALER_FIELD: &str = "dialer-proxy";

/// One node chosen for a body, before region tagging and dialer closure.
struct SelectedExport {
    /// Ledger identity, kept so a later drop never has to report a name.
    identity: String,
    kind: String,
    display: String,
    country: Option<String>,
    proxy: Value,
}

/// A body entry paired with the ledger identity that produced it.
struct BodyNode {
    identity: String,
    proxy: Value,
}

/// Select this round's per-source export bodies from the collected round.
///
/// Only alive nodes are published, and only in their original domain form
/// (`Collected::originals`): the kernel-facing proxies carry per-address aliases
/// and rewritten names that must never reach a client. One entry is returned per
/// source spec, in config order, so a caller can log the skipped ones too.
///
/// Sources in `sources` order, bodies sorted by (type, name), variant collapse
/// and region tagging are all deterministic: an unchanged round renders
/// byte-identical content and therefore reuses its snapshot instead of writing a
/// second file.
pub fn select_source_exports(
    publish: &PublishSection,
    sources: &[SourceSpec],
    collected: &Collected,
    verdicts: &[NodeVerdict],
) -> Vec<SourceExport> {
    let mut alive: HashMap<&str, Vec<&NodeVerdict>> = HashMap::new();
    for verdict in verdicts.iter().filter(|verdict| verdict.is_alive()) {
        alive
            .entry(verdict.job.source_id.as_str())
            .or_default()
            .push(verdict);
    }
    sources
        .iter()
        .map(|source| {
            if !source.enabled {
                return SourceExport::skipped(source.key.clone(), SkipReason::SourceDisabled);
            }
            if !source.export {
                return SourceExport::skipped(source.key.clone(), SkipReason::SourceMuted);
            }
            let candidates = alive
                .get(source.key.as_str())
                .map_or(&[][..], Vec::as_slice);
            let (proxies, dropped) =
                source_body(&source.key, candidates, collected, publish.add_region_tag);
            let skipped = proxies.is_empty().then_some(SkipReason::NoAliveNodes);
            SourceExport {
                key: source.key.clone(),
                proxies,
                skipped,
                dropped,
            }
        })
        .collect()
}

/// Build one source's body: collapse variants, keep the original form, rewrite
/// the region tag, then close every dialer inside the file.
fn source_body(
    source: &str,
    candidates: &[&NodeVerdict],
    collected: &Collected,
    add_region_tag: bool,
) -> (Vec<Value>, Vec<ExportDrop>) {
    let mut ordered = candidates.to_vec();
    ordered.sort_by(|left, right| {
        (&left.job.variant, &left.job.display, &left.job.fingerprint).cmp(&(
            &right.job.variant,
            &right.job.display,
            &right.job.fingerprint,
        ))
    });

    // One display name is one proxy in the file -- mihomo keys proxies and
    // groups in a single namespace -- so two variants of one node cannot both
    // be published. A chain verdict outranks its direct twin (the export speaks
    // the source's configured form); with no chain verdict alive the direct one
    // stands. That is Python's alive_forms rule: a node whose chain failed must
    // not go out wearing the dialer the round just disproved.
    let mut chosen: BTreeMap<&str, (&NodeVerdict, bool)> = BTreeMap::new();
    let mut dropped = Vec::new();
    for verdict in ordered {
        let chained = verdict.job.role == Role::Chain;
        match chosen.entry(verdict.job.display.as_str()) {
            Entry::Vacant(slot) => {
                slot.insert((verdict, chained));
            }
            Entry::Occupied(mut slot) => {
                dropped.push(ExportDrop::of(verdict, ExportDropReason::SupersededVariant));
                if chained && !slot.get().1 {
                    slot.insert((verdict, chained));
                }
            }
        }
    }

    let mut selected = Vec::with_capacity(chosen.len());
    for (verdict, _) in chosen.into_values() {
        let Some(original) = collected
            .originals
            .get(&(source.to_string(), verdict.job.fingerprint.clone()))
        else {
            dropped.push(ExportDrop::of(verdict, ExportDropReason::MissingOriginal));
            continue;
        };
        let Some(name) = original
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
        else {
            dropped.push(ExportDrop::of(verdict, ExportDropReason::UnusableOriginal));
            continue;
        };
        selected.push(SelectedExport {
            identity: verdict.job.fingerprint.clone(),
            kind: original
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            display: name.to_string(),
            country: measured_country(verdict),
            proxy: original.clone(),
        });
    }

    // Stable publish order (Python sorts by type then display), so an unchanged
    // round is byte-identical and reuses its snapshot.
    selected.sort_by(|left, right| (&left.kind, &left.display).cmp(&(&right.kind, &right.display)));

    let mut seen: HashSet<String> = HashSet::new();
    let mut renames: HashMap<String, String> = HashMap::new();
    let mut body: Vec<BodyNode> = Vec::with_capacity(selected.len());
    for mut entry in selected {
        // The published name, not the original, is the one that has to be
        // unique: the region re-tag can map two originals onto one name
        // (`[SG] 01` measured through an HK exit and `[HK] 01` both publish
        // as `[HK] 01`), and mihomo keys proxies by that final name -- a
        // duplicate would fail the whole source's topology validation every
        // round. The first (sort-stable) entry keeps the name; the loser is
        // superseded, exactly like a same-name variant.
        let published = match entry.country {
            Some(country) if add_region_tag => {
                format!("[{country}] {}", strip_leading_region_tags(&entry.display))
            }
            _ => entry.display.clone(),
        };
        if !seen.insert(published.clone()) {
            dropped.push(ExportDrop {
                identity: entry.identity,
                reason: ExportDropReason::SupersededVariant,
            });
            continue;
        }
        if published != entry.display {
            renames.insert(entry.display, published.clone());
        }
        if let Some(object) = entry.proxy.as_object_mut() {
            object.insert("name".to_string(), Value::String(published));
        }
        body.push(BodyNode {
            identity: entry.identity,
            proxy: entry.proxy,
        });
    }

    close_dialers(&mut body, &renames, &mut dropped);
    (body.into_iter().map(|node| node.proxy).collect(), dropped)
}

/// Make every published chained node's dialer resolve inside the file.
///
/// A chained node reaches a client as `dialer-proxy: <name>`, and the probe's own
/// kernel config is invisible to a client: one dangling dialer makes mihomo
/// reject the whole file, taking every working node down with it. So a dialer
/// that already names a published proxy is left alone (the upstream author's
/// topology may be meaningful), one that names a sibling by its pre-tag name is
/// rewritten to the published name so the reference really closes, and anything
/// else is dropped. Dropping a node can dangle the nodes that dial through it,
/// so this repeats until the body stops shrinking.
fn close_dialers(
    body: &mut Vec<BodyNode>,
    renames: &HashMap<String, String>,
    dropped: &mut Vec<ExportDrop>,
) {
    loop {
        let published: HashSet<String> = body
            .iter()
            .filter_map(|node| node.proxy.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        let mut unresolved = Vec::new();
        for (index, node) in body.iter_mut().enumerate() {
            let Some(dialer) = node
                .proxy
                .get(DIALER_FIELD)
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if published.contains(&dialer) {
                continue;
            }
            match renames.get(&dialer) {
                Some(name) => {
                    if let Some(object) = node.proxy.as_object_mut() {
                        object.insert(DIALER_FIELD.to_string(), Value::String(name.clone()));
                    }
                }
                None => unresolved.push(index),
            }
        }
        if unresolved.is_empty() {
            return;
        }
        for index in unresolved.into_iter().rev() {
            let node = body.remove(index);
            dropped.push(ExportDrop {
                identity: node.identity,
                reason: ExportDropReason::UnclosedDialer,
            });
        }
    }
}

/// Remove every leading region tag this service may have written on an earlier
/// round, so re-tagging replaces the tag instead of stacking another one.
///
/// The Python pattern matched one tag at the start and required the name to
/// begin with a bracket, so a name already carrying a flag emoji grew one tag
/// per round inside a self-feeding collection. This strips any number of region
/// tags and flag emojis, in any order, separated by optional spaces.
fn strip_leading_region_tags(name: &str) -> &str {
    let mut rest = name;
    loop {
        let trimmed = rest.trim_start();
        let Some(after) = strip_region_tag(trimmed) else {
            return rest;
        };
        rest = after.trim_start();
    }
}

/// Strip exactly one leading region tag: two uppercase letters with an optional
/// alphanumeric suffix, or one flag emoji.
fn strip_region_tag(name: &str) -> Option<&str> {
    if let Some(rest) = name.strip_prefix('[') {
        let end = rest.find(']')?;
        let (tag, after) = rest.split_at(end);
        let mut parts = tag.split('-');
        let region = parts.next().unwrap_or_default();
        let suffix = parts.next();
        let region_ok = region.len() == 2 && region.bytes().all(|byte| byte.is_ascii_uppercase());
        let suffix_ok = match suffix {
            None => true,
            Some(suffix) => {
                !suffix.is_empty()
                    && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
                    && parts.next().is_none()
            }
        };
        return (region_ok && suffix_ok).then(|| &after[1..]);
    }
    let mut characters = name.chars();
    match (characters.next(), characters.next()) {
        (Some(first), Some(second))
            if is_regional_indicator(first) && is_regional_indicator(second) =>
        {
            Some(&name[first.len_utf8() + second.len_utf8()..])
        }
        _ => None,
    }
}

/// A flag emoji is two regional-indicator code points in a row.
fn is_regional_indicator(character: char) -> bool {
    ('🇦'..='🇿').contains(&character)
}

/// The measured exit country, when the round has one and it is a valid
/// uppercase ISO alpha-2 code. The tag is built from a measured value, so
/// anything else is refused rather than pasted into a published name -- and a
/// tag that cannot be recognized again would stack on the next round.
fn measured_country(verdict: &NodeVerdict) -> Option<String> {
    let country = verdict
        .exit
        .as_ref()?
        .country
        .as_deref()?
        .trim()
        .to_ascii_uppercase();
    probe_domain::geo::is_country_code(&country).then_some(country)
}

/// Everything one round's publication needs.
pub struct PublishRoundRequest<'a> {
    pub publish: &'a PublishSection,
    /// The normalized sources list, in config order.
    pub sources: &'a [SourceSpec],
    pub collected: &'a Collected,
    pub verdicts: &'a [NodeVerdict],
    /// The round guard. `PreservePreviousState` always withholds publication;
    /// `MarkRoundInconclusive` withholds it only when the round measured
    /// nothing (see `measured`).
    pub guard: GuardDecision,
    /// Whether this round actually committed measurements -- the caller's
    /// `!outcome.cancelled && outcome.results_written > 0`. An inconclusive
    /// round that still measured nodes publishes them, the way Python publishes
    /// on partial coverage; one that measured nothing has nothing to publish.
    pub measured: bool,
    /// Safe principal label for the audit row.
    pub actor: &'a str,
}

/// One source's publication result. The key is always present, so a caller can
/// log per source without re-deriving the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourcePublication {
    Published {
        key: String,
        node_count: usize,
        snapshot_id: i64,
        content_path: PathBuf,
    },
    Skipped {
        key: String,
        reason: SkipReason,
    },
    Failed {
        key: String,
        reason: String,
    },
}

/// What a round's publication did.
#[derive(Debug, Clone, Default)]
pub struct PublishRoundOutcome {
    /// Set when nothing was published at all; every source then carries the
    /// same reason in `sources`.
    pub skipped: Option<SkipReason>,
    pub sources: Vec<SourcePublication>,
    /// Nodes left out of a body this round, for the caller's log.
    pub dropped: Vec<ExportDrop>,
    /// Sub-Store write-back results (`publish.push_to_substore`), one record
    /// per sub touched. Empty when the push mode is off.
    pub push: Vec<PushRecord>,
    /// What the retention pass removed, when `publish.keep_snapshots` is set.
    pub pruned: Option<RetentionReport>,
}

/// What one retention pass removed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetentionReport {
    /// Export keys the pass looked at.
    pub keys: usize,
    /// Superseded snapshot rows deleted.
    pub snapshots_deleted: usize,
    /// Immutable files deleted because no surviving row referenced them.
    pub files_deleted: usize,
}

/// One sub's push outcome -- Python `engine._push_record`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRecord {
    /// The source key the record belongs to.
    pub key: String,
    /// The Sub-Store sub name: `<prefix>-<key>-local`.
    pub name: String,
    /// `true` when the backend accepted the write. A removal that had nothing
    /// to remove also counts: the sub is gone either way.
    pub ok: bool,
    /// Nodes the pushed body carried; 0 for a removal.
    pub node_count: usize,
    /// Bounded, backend-free summary for the log: the action Sub-Store reported,
    /// or why nothing happened. Never carries export content.
    pub detail: String,
}

/// Publish this round's per-source exports.
///
/// Only two situations withhold a publication, matching Python
/// (`engine._apply_and_publish`: only the `suspect` alive-floor guard skips
/// `_publish_sources`):
///
/// * the `publish` section is disabled, or
/// * the guard withheld it -- the alive floor was breached
///   ([GuardDecision::PreservePreviousState]), or the round was inconclusive
///   **and** measured nothing at all (`measured == false`: cancelled, a kernel
///   or ledger failure, or nothing tested).
///
/// Partial coverage publishes. Nodes that never finished exit verification are
/// not alive, so they cannot enter a body, and a source with nothing alive
/// keeps its previous snapshot as [SkipReason::NoAliveNodes] -- a stronger
/// constraint than Python's "publish unverified unless a country filter is
/// configured", and the reason an inconclusive round is safe to publish.
///
/// Otherwise every selected source goes through `publish_export`, which
/// renders, diffs, materializes the immutable file and CAS-activates the ledger
/// row. One source failing never stops the others.
pub fn publish_round(
    storage: &Storage,
    export_root: &Path,
    request: PublishRoundRequest<'_>,
) -> PublishRoundOutcome {
    let round_skip = if !request.publish.enabled {
        Some(SkipReason::PublishDisabled)
    } else {
        match request.guard {
            // The alive floor was breached: this round's numbers are not a
            // trustworthy baseline, so last round's publication stands.
            GuardDecision::PreservePreviousState => Some(SkipReason::GuardNotApplied),
            // An inconclusive round that never measured anything has nothing to
            // publish; one that did measure publishes like any other round.
            GuardDecision::MarkRoundInconclusive if !request.measured => {
                Some(SkipReason::GuardNotApplied)
            }
            GuardDecision::ApplyConvergence | GuardDecision::MarkRoundInconclusive => None,
        }
    };
    // Retention is independent of this round's publication, so it runs even
    // when the guard withholds one -- otherwise a long run of guarded rounds
    // would let the ledger and the export root grow without bound.
    let pruned = request
        .publish
        .keep_snapshots
        .map(|keep| prune_publications(storage, export_root, keep));
    if let Some(reason) = round_skip {
        return PublishRoundOutcome {
            skipped: Some(reason),
            sources: request
                .sources
                .iter()
                .map(|source| SourcePublication::Skipped {
                    key: source.key.clone(),
                    reason,
                })
                .collect(),
            dropped: Vec::new(),
            push: Vec::new(),
            pruned: pruned.clone(),
        };
    }

    let mut sources = Vec::new();
    let mut dropped = Vec::new();
    for selection in select_source_exports(
        request.publish,
        request.sources,
        request.collected,
        request.verdicts,
    ) {
        dropped.extend(selection.dropped.iter().cloned());
        if let Some(reason) = selection.skipped {
            sources.push(SourcePublication::Skipped {
                key: selection.key,
                reason,
            });
            continue;
        }
        sources.push(publish_one(storage, export_root, request.actor, &selection));
    }
    PublishRoundOutcome {
        skipped: None,
        sources,
        dropped,
        push: Vec::new(),
        pruned,
    }
}

/// Keep the newest `keep` snapshots per key (plus the active one) and delete
/// the immutable files no surviving row references.
///
/// The service owns the export root, but ownership is a contract with the
/// deployment rather than a licence to delete whatever is there: only files
/// named `<sha256(key)>-<sha256(content)>.yaml` for a key the ledger knows are
/// candidates. Anything else -- a README, a backup, another programme's file --
/// is left alone, and a failure to read the directory simply means a smaller
/// report rather than a failed round.
pub fn prune_publications(storage: &Storage, export_root: &Path, keep: usize) -> RetentionReport {
    let mut report = RetentionReport::default();
    let Ok(keys) = storage.export_keys() else {
        return report;
    };
    for key in keys {
        report.keys += 1;
        // Delete rows first: a file whose row is gone is what makes it garbage.
        if let Ok(deleted) = storage.prune_export_snapshots(&key, keep) {
            report.snapshots_deleted += deleted;
        }
        let referenced: HashSet<String> = storage
            .export_content_hashes(&key)
            .unwrap_or_default()
            .into_iter()
            .map(|hash| format!("{}-{hash}.yaml", export_hash(&key)))
            .collect();
        let prefix = format!("{}-", export_hash(&key));
        let Ok(entries) = std::fs::read_dir(export_root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) || !name.ends_with(".yaml") || referenced.contains(&name)
            {
                continue;
            }
            if std::fs::remove_file(entry.path()).is_ok() {
                report.files_deleted += 1;
            }
        }
    }
    report
}

/// Publish one selected body, mapping every failure to a reportable reason.
fn publish_one(
    storage: &Storage,
    export_root: &Path,
    actor: &str,
    selection: &SourceExport,
) -> SourcePublication {
    let key = selection.key.clone();
    // A refused key or actor is a configuration problem; report it as such
    // instead of surfacing later as an opaque ledger failure.
    if validate_export_key(&key).is_err() {
        return SourcePublication::Failed {
            key,
            reason: "invalid export key".to_string(),
        };
    }
    if validate_export_actor(actor).is_err() {
        return SourcePublication::Failed {
            key,
            reason: "invalid export actor".to_string(),
        };
    }
    let outcome = match publish_export(
        storage,
        export_root,
        PublishRequest {
            key: &key,
            actor,
            proxies: &selection.proxies,
            groups: &[],
            policy: PublishPolicy {
                enabled: true,
                guarded: false,
                max_changed_identities: None,
                max_change_basis_points: None,
            },
            force: false,
        },
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            return SourcePublication::Failed {
                key,
                reason: publish_failure(&error),
            };
        }
    };
    match (outcome.publication, outcome.content_path) {
        (Some(publication), Some(content_path)) => SourcePublication::Published {
            key,
            node_count: selection.proxies.len(),
            snapshot_id: publication.snapshot.id,
            content_path,
        },
        // Unreachable through the policy above, but a skip must never be
        // reported as a success.
        _ => SourcePublication::Skipped {
            key,
            reason: match outcome.decision {
                PublishDecision::Disabled => SkipReason::PublishDisabled,
                PublishDecision::Guarded | PublishDecision::DiffLimit => {
                    SkipReason::GuardNotApplied
                }
                PublishDecision::Empty | PublishDecision::Publish { .. } => {
                    SkipReason::NoAliveNodes
                }
            },
        },
    }
}

/// Render a publication failure for logs and DTOs: static context and an OS
/// error kind only, never a path or a proxy value (the module's error contract).
fn publish_failure(error: &PublishError) -> String {
    match error {
        PublishError::Invalid(context) => (*context).to_string(),
        PublishError::Io(kind) => format!("filesystem operation failed: {kind:?}"),
        PublishError::Storage(_) => "ledger operation failed".to_string(),
    }
}

/// The push-mode sub name. Deliberately distinct from the pull-mode
/// `<prefix>-<key>` so the two integration styles never overwrite each other.
fn local_sub_name(prefix: &str, key: &str) -> String {
    format!("{prefix}-{key}-local")
}

/// Push this round's exports into Sub-Store as `<prefix>-<key>-local` subs.
///
/// Ports Python `engine.push_exports` + `engine._prune_local_subs`. It runs
/// after `publish_round` has committed, and reads each body from the active
/// snapshot rather than from the file: the ledger is authoritative, so the bytes
/// pushed are the bytes a download would serve.
///
/// Only sources that actually published this round are upserted, and the whole
/// call is a no-op without `publish.push_to_substore`. Nothing here can fail the
/// round: every backend call becomes a [PushRecord], and one source failing never
/// stops the others, the prune, or the round that already finished. (Python also
/// re-upserts enabled sources that did not publish from their file on disk; this
/// is narrower and idempotent -- same content, fewer writes.)
pub async fn push_round_exports(
    admin: &dyn SubAdmin,
    storage: &Storage,
    publish: &PublishSection,
    sources: &[SourceSpec],
    outcome: &mut PublishRoundOutcome,
) {
    if !publish.push_to_substore {
        return;
    }
    let published: Vec<String> = outcome
        .sources
        .iter()
        .filter_map(|source| match source {
            SourcePublication::Published { key, .. } => Some(key.clone()),
            SourcePublication::Skipped { .. } | SourcePublication::Failed { .. } => None,
        })
        .collect();
    let mut records = Vec::with_capacity(published.len());
    for key in &published {
        records.push(push_one(admin, storage, publish, key).await);
    }
    records.extend(prune_local_subs(admin, publish, sources, &published).await);
    outcome.push = records;
}

/// Upsert one source's active export as its `-local` sub.
async fn push_one(
    admin: &dyn SubAdmin,
    storage: &Storage,
    publish: &PublishSection,
    key: &str,
) -> PushRecord {
    let name = local_sub_name(&publish.prefix, key);
    let mut record = PushRecord {
        key: key.to_string(),
        name: name.clone(),
        ok: false,
        node_count: 0,
        detail: String::new(),
    };
    let snapshot = match storage.current_export(key) {
        Ok(Some(snapshot)) => snapshot,
        // Python reports the missing export file the same way: a warning, not a
        // failed round. Nothing is upserted, so a stale sub is never resurrected.
        Ok(None) => {
            record.detail = "no active export; run a round first".to_string();
            return record;
        }
        Err(_) => {
            record.detail = "ledger read failed".to_string();
            return record;
        }
    };
    record.node_count = snapshot.summary.node_count;
    // Python's payload field for field. `source = local` is what keeps the prune
    // scoped, and the content is embedded so the sub keeps working without this
    // service.
    let payload = serde_json::json!({
        "name": name,
        "displayName": format!("{key} 测活 ({})", snapshot.summary.node_count),
        "source": "local",
        "url": "",
        "content": snapshot.content,
        "mergeSources": "",
        "ignoreFailedRemoteSub": "quiet",
        "passThroughUA": false,
        "process": [],
    });
    match admin.upsert_sub(&name, &payload).await {
        Ok(action) => {
            record.ok = true;
            record.detail = action;
        }
        Err(error) => record.detail = error,
    }
    record
}

/// Delete the `-local` subs of sources this push no longer publishes.
///
/// Scoped three ways, because a wrong delete cannot be undone: the name must
/// start with our prefix and end in `-local`, and the backend must report
/// `source == "local"` -- a remote sub of the same name belongs to the pull
/// mode, and anything else is not ours to remove.
///
/// The keep set takes two inputs, exactly like Python: the keys this push wrote,
/// plus every source that may still legitimately own a sub. Pruning on the pushed
/// keys alone would delete every other source's list during a single-source round.
async fn prune_local_subs(
    admin: &dyn SubAdmin,
    publish: &PublishSection,
    sources: &[SourceSpec],
    published: &[String],
) -> Vec<PushRecord> {
    let mut keep: HashSet<String> = published
        .iter()
        .map(|key| local_sub_name(&publish.prefix, key))
        .collect();
    keep.extend(
        sources
            .iter()
            .filter(|source| source.enabled && source.export)
            .map(|source| local_sub_name(&publish.prefix, &source.key)),
    );
    let existing = match admin.list_subs().await {
        Ok(existing) => existing,
        // Without the list there is nothing safe to delete; the next round
        // retries, exactly as Python's `return` on a failed listing does.
        Err(_) => return Vec::new(),
    };
    let scope = format!("{}-", publish.prefix);
    let mut records = Vec::new();
    for entry in existing {
        if !entry.name.ends_with("-local") || !entry.name.starts_with(&scope) {
            continue;
        }
        if keep.contains(&entry.name) || entry.source.as_deref() != Some("local") {
            continue;
        }
        let key = entry.name[scope.len()..entry.name.len() - "-local".len()].to_string();
        let mut record = PushRecord {
            key,
            name: entry.name.clone(),
            ok: false,
            node_count: 0,
            detail: String::new(),
        };
        match admin.delete_sub(&entry.name).await {
            Ok(_) => {
                record.ok = true;
                record.detail = "removed".to_string();
            }
            Err(error) => record.detail = error,
        }
        records.push(record);
    }
    records
}

/// Keep the shared write-before-activation ordering explicit for publish and rollback.
fn materialize_then<T>(
    root: &Path,
    key: &str,
    rendered: &RenderedExport,
    activate: impl FnOnce() -> probe_domain::DomainResult<T>,
) -> PublishResult<(T, PathBuf)> {
    let path = materialize_export(root, key, rendered)?;
    let activated = activate()?;
    Ok((activated, path))
}

/// Re-materialize validated DB bytes without changing whitespace/hash on rollback.
fn render_snapshot(snapshot: &ExportSnapshot) -> PublishResult<RenderedExport> {
    validate_export_content(
        &snapshot.content,
        &snapshot.summary.sha256,
        snapshot.summary.node_count,
    )?;
    parse_document(&snapshot.content)?;
    Ok(RenderedExport {
        content: snapshot.content.clone(),
        sha256: snapshot.summary.sha256.clone(),
        node_count: snapshot.summary.node_count,
    })
}

/// Construct a response with no accidental activation or mutable file metadata.
fn skipped(decision: PublishDecision, diff: Option<SnapshotDiff>) -> PublishOutcome {
    PublishOutcome {
        decision,
        diff,
        publication: None,
        content_path: None,
    }
}

/// Bound and parse a stored JSON document, validating all topology before diff/use.
fn parse_document(content: &str) -> PublishResult<Value> {
    if content.is_empty() || content.len() > MAX_EXPORT_BYTES {
        return Err(PublishError::Invalid("content size limit"));
    }
    let document: Value = serde_json::from_str(content)
        .map_err(|_| PublishError::Invalid("invalid snapshot JSON"))?;
    let proxies = document
        .get("proxies")
        .and_then(Value::as_array)
        .ok_or(PublishError::Invalid("missing proxy dictionaries"))?;
    let groups = match document.get("proxy-groups") {
        None => &[][..],
        Some(value) => value
            .as_array()
            .ok_or(PublishError::Invalid("invalid proxy groups"))?
            .as_slice(),
    };
    if document.as_object().is_none_or(|object| {
        object
            .keys()
            .any(|key| key != "proxies" && key != "proxy-groups")
    }) {
        return Err(PublishError::Invalid(
            "unsupported snapshot top-level fields",
        ));
    }
    validate_topology(proxies, groups)?;
    Ok(document)
}

/// Index named objects by kind plus SHA-256(name); values stay private to comparison.
fn index_identities(document: &Value) -> BTreeMap<String, &Value> {
    let mut entries = BTreeMap::new();
    for (field, kind) in [("proxies", "proxy"), ("proxy-groups", "group")] {
        if let Some(objects) = document.get(field).and_then(Value::as_array) {
            for object in objects {
                let name = object["name"].as_str().expect("topology validated names");
                entries.insert(format!("{kind}:{}", export_hash(name)), object);
            }
        }
    }
    entries
}

/// Validate a bounded single namespace, explicit references, and acyclic closure.
fn validate_topology(proxies: &[Value], groups: &[Value]) -> PublishResult<()> {
    if proxies.is_empty() || proxies.len() > MAX_EXPORT_NODES || groups.len() > MAX_EXPORT_NODES {
        return Err(PublishError::Invalid(
            "proxy or group count outside allowed range",
        ));
    }
    let mut graph = HashMap::new();
    let mut edge_count = 0;
    for (objects, is_group) in [(proxies, false), (groups, true)] {
        for object in objects {
            let name = validate_name(object)?;
            let references = collect_references(object, is_group)?;
            edge_count += references.len();
            if edge_count > MAX_TOPOLOGY_EDGES {
                return Err(PublishError::Invalid("too many topology references"));
            }
            if graph.insert(name, references).is_some() {
                return Err(PublishError::Invalid("duplicate proxy or group name"));
            }
        }
    }
    check_closure(&graph)
}

/// Read a safe nonempty name and protocol/group type without echoing either value.
fn validate_name(object: &Value) -> PublishResult<&str> {
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| {
            !name.trim().is_empty() && name.len() <= 512 && !name.chars().any(char::is_control)
        })
        .ok_or(PublishError::Invalid("invalid proxy or group name"))?;
    if BUILTIN_TARGETS.contains(&name) || name == "GLOBAL" {
        return Err(PublishError::Invalid(
            "name collides with a built-in target",
        ));
    }
    if object
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind.trim().is_empty())
    {
        return Err(PublishError::Invalid("missing proxy or group type"));
    }
    if object.get("dialer_proxy").is_some() {
        return Err(PublishError::Invalid(
            "normalize dialer_proxy to dialer-proxy before rendering",
        ));
    }
    if object
        .get("reality-opts")
        .and_then(|opts| opts.get("short-id"))
        .is_some_and(|id| !id.is_string())
    {
        return Err(PublishError::Invalid(
            "REALITY short-id must remain a string",
        ));
    }
    Ok(name)
}

/// Gather all explicit edges; dynamic provider/group membership is unsupported.
fn collect_references(object: &Value, is_group: bool) -> PublishResult<Vec<&str>> {
    let mut references = Vec::new();
    if let Some(dialer) = object.get("dialer-proxy") {
        references.push(
            dialer
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or(PublishError::Invalid("invalid dialer reference"))?,
        );
    }
    if !is_group {
        return Ok(references);
    }
    for field in [
        "use",
        "include-all",
        "include-all-proxies",
        "include-all-providers",
    ] {
        if object.get(field).is_some() {
            return Err(PublishError::Invalid(
                "group requires explicit self-contained membership",
            ));
        }
    }
    let kind = object["type"].as_str().unwrap_or_default();
    if !["select", "url-test", "fallback", "load-balance", "relay"].contains(&kind) {
        return Err(PublishError::Invalid("unsupported group type"));
    }
    let members = object
        .get("proxies")
        .and_then(Value::as_array)
        .filter(|members| !members.is_empty())
        .ok_or(PublishError::Invalid("group has no explicit members"))?;
    if members.len() > MAX_TOPOLOGY_EDGES {
        return Err(PublishError::Invalid("too many group members"));
    }
    let mut seen = HashSet::new();
    for member in members {
        let name = member
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or(PublishError::Invalid("invalid group reference"))?;
        if !seen.insert(name) {
            return Err(PublishError::Invalid("duplicate group member"));
        }
        references.push(name);
    }
    Ok(references)
}

/// Reject dangling targets and cycles using an iterative topological traversal.
fn check_closure(graph: &HashMap<&str, Vec<&str>>) -> PublishResult<()> {
    let mut incoming: HashMap<&str, usize> = graph.keys().map(|name| (*name, 0)).collect();
    for targets in graph.values() {
        for target in targets {
            if BUILTIN_TARGETS.contains(target) {
                continue;
            }
            let count = incoming
                .get_mut(target)
                .ok_or(PublishError::Invalid("dangling dialer or group reference"))?;
            *count += 1;
        }
    }
    let mut ready: VecDeque<&str> = incoming
        .iter()
        .filter_map(|(name, count)| (*count == 0).then_some(*name))
        .collect();
    let mut visited = 0;
    while let Some(name) = ready.pop_front() {
        visited += 1;
        for target in &graph[name] {
            if let Some(count) = incoming.get_mut(target) {
                *count -= 1;
                if *count == 0 {
                    ready.push_back(target);
                }
            }
        }
    }
    if visited != graph.len() {
        return Err(PublishError::Invalid("cyclic export topology"));
    }
    Ok(())
}

/// Serialize with a hard byte cap instead of building an unbounded output string.
struct BoundedOutput(Vec<u8>);

impl Write for BoundedOutput {
    /// Append `bytes` only within the hard export limit, returning the written length.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_EXPORT_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "export size limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// Memory writes require no flush; return success.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Write an immutable content-addressed file before the caller changes DB authority.
/// The root must already exist and pass the single containment gate; every path
/// written below is derived from that validated root. Existing files are verified,
/// never replaced or repaired.
pub fn materialize_export(
    root: &Path,
    key: &str,
    rendered: &RenderedExport,
) -> PublishResult<PathBuf> {
    validate_export_key(key)?;
    validate_export_content(rendered.content(), rendered.sha256(), rendered.node_count())?;
    let root = checked_root(root)?;
    let destination =
        root.join_file(&format!("{}-{}.yaml", export_hash(key), rendered.sha256()))?;
    if inspect_existing(&destination, rendered)? {
        sync_directory(&root)?;
        return Ok(destination);
    }
    let (temporary, mut file) = create_temporary(&root)?;
    file.write_all(rendered.content().as_bytes())
        .map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    drop(file);
    // Re-resolve the root after staging: a root swapped for a link, or one that has
    // become shared, must stop the install instead of publishing outside the root.
    let root = checked_root(root.path())?;
    match install_file(&temporary.0, &destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(error)),
    }
    // Atomic no-clobber installation makes only complete bytes visible. An orphan
    // after a DB failure is harmless; never delete a potentially active file.
    temporary.remove()?;
    if !inspect_existing(&destination, rendered)? {
        return Err(PublishError::Invalid("materialized export disappeared"));
    }
    sync_directory(&root)?;
    Ok(destination)
}

/// Install without overwriting: hard-link on Unix, write-through rename on Windows.
fn install_file(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows_files::install_private(source, destination)
    }
    #[cfg(not(windows))]
    {
        fs::hard_link(source, destination)
    }
}

/// A caller-supplied export root that already passed containment and
/// exclusive-ownership validation. Only `checked_root` constructs it, so every write
/// entry point (`materialize_export`, and through it `materialize_then`,
/// `publish_export`, and `rollback_export`) is forced through the same gate: staging
/// files, installs, and directory syncs accept nothing else.
struct CheckedRoot(PathBuf);

impl CheckedRoot {
    /// Absolute, link-free, canonical root path.
    fn path(&self) -> &Path {
        &self.0
    }

    /// Join exactly one normal file name and re-check containment afterwards.
    /// Names here are service-generated; this check is the last line of defense.
    fn join_file(&self, name: &str) -> PublishResult<PathBuf> {
        let mut components = Path::new(name).components();
        let single_name = matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        );
        if !single_name || name.contains(['/', '\\']) {
            return Err(PublishError::Invalid("export file name escapes root"));
        }
        let destination = self.0.join(name);
        // starts_with compares whole components, so a sibling such as
        // /srv/export-backup/file.yaml never matches the root /srv/export. Requiring
        // the parent to be exactly the root rejects that remaining prefix confusion.
        if destination.parent() != Some(self.0.as_path()) || !destination.starts_with(&self.0) {
            return Err(PublishError::Invalid("export path escapes root"));
        }
        Ok(destination)
    }
}

/// Resolve an absolute, existing root after rejecting symlinks/reparse ancestors.
///
/// Containment is enforced on the resolved path: literal `.` and `..` components are
/// refused, every ancestor including the root must be a real directory rather than a
/// link or reparse point, and the canonical result is the only path later writes use.
/// Windows resolves `PathBuf::join("..")` on a verbatim (canonical) path lexically, so
/// a caller can pass a root that already escaped to a shared parent before this
/// function runs; the exclusive-ownership scan refuses that case, which a
/// component-only check cannot see.
fn checked_root(root: &Path) -> PublishResult<CheckedRoot> {
    if !root.is_absolute()
        || root
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(PublishError::Invalid(
            "export root must be absolute without traversal",
        ));
    }
    for ancestor in root.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(io_error)?;
        if is_link(&metadata) || !metadata.is_dir() {
            return Err(PublishError::Invalid(
                "export root has a link or non-directory ancestor",
            ));
        }
    }
    let metadata = fs::symlink_metadata(root).map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(PublishError::Invalid("export root must be owner-only"));
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    let canonical = root.canonicalize().map_err(io_error)?;
    ensure_exclusive_ownership(&canonical)?;
    Ok(CheckedRoot(canonical))
}

/// Require a root only this service populates: every existing entry must be either an
/// immutable content-addressed export file or one of this module's staging files. A
/// directory holding anything else is shared with another principal, so the service
/// cannot control it exclusively and refuses to write exports there.
fn ensure_exclusive_ownership(root: &Path) -> PublishResult<()> {
    for entry in fs::read_dir(root).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        if !is_export_artifact(&entry.file_name()) {
            return Err(PublishError::Invalid(
                "export root is shared with entries this service did not create",
            ));
        }
    }
    Ok(())
}

/// Recognize the only two names this module ever creates inside an export root.
fn is_export_artifact(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if let Some(stem) = name.strip_suffix(".yaml") {
        let mut halves = stem.split('-');
        return matches!(
            (halves.next(), halves.next(), halves.next()),
            (Some(key_hash), Some(content_hash), None)
                if is_sha256(key_hash) && is_sha256(content_hash)
        );
    }
    name.starts_with(".publication-") && name.ends_with(".tmp")
}

/// Lowercase SHA-256 hex is the only digest form used in export file names.
fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Detect symlinks on Unix and all reparse points (including junctions) on Windows.
fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

/// Check an immutable file's type, restrictive mode, size, and exact bytes.
fn inspect_existing(path: &Path, rendered: &RenderedExport) -> PublishResult<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(io_error(error)),
    };
    if is_link(&metadata) || !metadata.is_file() {
        return Err(PublishError::Invalid("export target is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err(PublishError::Invalid(
                "export file is shared or not owner-only",
            ));
        }
    }
    #[cfg(windows)]
    windows_files::check_private(path).map_err(io_error)?;
    if metadata.len() != rendered.content.len() as u64 {
        return Err(PublishError::Invalid("existing export file size mismatch"));
    }
    let file = open_existing_file(path).map_err(io_error)?;
    let opened_metadata = file.metadata().map_err(io_error)?;
    if is_link(&opened_metadata) || !opened_metadata.is_file() {
        return Err(PublishError::Invalid("export file became a link"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.dev() != opened_metadata.dev() || metadata.ino() != opened_metadata.ino() {
            return Err(PublishError::Invalid(
                "export file changed during inspection",
            ));
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_EXPORT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes != rendered.content.as_bytes() {
        return Err(PublishError::Invalid(
            "existing export file content mismatch",
        ));
    }
    Ok(true)
}

/// Open existing Windows files without following a newly swapped reparse point.
fn open_existing_file(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x0020_0000)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

/// Create a unique owner-only staging file inside the validated root; CREATE_NEW
/// never follows a planted link, and the staging name passes the same gate.
fn create_temporary(root: &CheckedRoot) -> PublishResult<(TemporaryFile, File)> {
    for _ in 0..64 {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let path = root.join_file(&format!(
            ".publication-{}-{sequence}.tmp",
            std::process::id()
        ))?;
        match create_private_file(&path) {
            Ok(file) => return Ok((TemporaryFile(path), file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(PublishError::Invalid("cannot reserve export staging file"))
}

/// Create a new restrictive file using atomic OS permissions, never chmod-after-write.
fn create_private_file(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(windows)]
    {
        windows_files::create_private(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private file creation unavailable",
        ))
    }
}

/// Flush directory entries on Unix; Windows installation uses write-through rename.
fn sync_directory(root: &CheckedRoot) -> PublishResult<()> {
    #[cfg(unix)]
    File::open(root.path())
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)?;
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}

/// Strip paths/OS text before an I/O failure crosses into logs or API DTOs.
fn io_error(error: io::Error) -> PublishError {
    PublishError::Io(error.kind())
}

/// Own a staging path only; final content-addressed files are never removed here.
struct TemporaryFile(PathBuf);

impl TemporaryFile {
    /// Remove the staging link before activation, reporting failures to the caller.
    fn remove(self) -> PublishResult<()> {
        match fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    }
}

impl Drop for TemporaryFile {
    /// Best-effort staging cleanup on error; the original error remains authoritative.
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(kind = ?error.kind(), "export staging cleanup failed");
            }
        }
    }
}

/// Small Win32 boundary: std OpenOptions cannot supply an atomic private DACL.
/// No shell/ACL mutation is used, and a restrictive DACL is present at creation.
#[cfg(windows)]
mod windows_files {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::ptr::null_mut;

    const PRIVATE_DACL: &str = "D:P(A;;FA;;;OW)";
    const DACL_INFORMATION: u32 = 4;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor: *const c_void,
            revision: u32,
            information: u32,
            text: *mut *mut u16,
            size: *mut u32,
        ) -> i32;
        fn GetFileSecurityW(
            path: *const u16,
            information: u32,
            descriptor: *mut c_void,
            length: u32,
            needed: *mut u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            path: *const u16,
            access: u32,
            share: u32,
            security: *const SecurityAttributes,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
        fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
    }

    /// Create `path` with a protected owner-only DACL and no inherited broad ACEs.
    pub(super) fn create_private(path: &Path) -> io::Result<File> {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let text: Vec<u16> = PRIVATE_DACL.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = null_mut();
        // SAFETY: all pointers refer to live, sized buffers. Win32 owns the
        // allocated descriptor until LocalFree; the File owns a successful handle.
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let attributes = SecurityAttributes {
                length: std::mem::size_of::<SecurityAttributes>() as u32,
                descriptor,
                inherit: 0,
            };
            let handle = CreateFileW(
                path.as_ptr(),
                0x4000_0000,
                0,
                &attributes,
                1,
                0x8000_0000 | 0x0020_0000,
                null_mut(),
            );
            let error = io::Error::last_os_error();
            LocalFree(descriptor);
            if handle == -1isize as *mut c_void {
                return Err(error);
            }
            Ok(File::from_raw_handle(handle))
        }
    }

    /// Rename on the same volume with WRITE_THROUGH and without REPLACE_EXISTING.
    pub(super) fn install_private(source: &Path, destination: &Path) -> io::Result<()> {
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: both terminated path buffers remain live throughout the call.
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 8) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Reject an existing target unless it still has the exact private DACL.
    pub(super) fn check_private(path: &Path) -> io::Result<()> {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut needed = 0;
        // SAFETY: first call requests the size only. The second writes into
        // aligned owned storage. The converted string is read before LocalFree.
        unsafe {
            GetFileSecurityW(path.as_ptr(), DACL_INFORMATION, null_mut(), 0, &mut needed);
            if needed == 0 || needed > 65536 {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
            if GetFileSecurityW(
                path.as_ptr(),
                DACL_INFORMATION,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut text = null_mut();
            let mut size = 0;
            if ConvertSecurityDescriptorToStringSecurityDescriptorW(
                buffer.as_ptr().cast(),
                1,
                DACL_INFORMATION,
                &mut text,
                &mut size,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let actual = String::from_utf16_lossy(std::slice::from_raw_parts(text, size as usize));
            LocalFree(text.cast());
            if actual.trim_end_matches('\0') != PRIVATE_DACL {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "export DACL is not private",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
