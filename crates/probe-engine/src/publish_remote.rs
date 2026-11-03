//! The remote publish leg (workstreams/18 B2): carry each source's
//! just-published export to a remote destination (GitHub Gist, or the
//! composite local+Gist chain).
//!
//! This module runs AFTER [`crate::publish::publish_round`] has committed the
//! local pipeline and never changes that round's outcome: everything here is
//! reported per source and persisted as bookkeeping, and a remote failure
//! leaves the previous artifact -- file, ledger pointer and content digest --
//! standing (《指导》§3.4 (6)). The mapping it implements, per locally
//! published source:
//!
//! * publisher outcome `NoChange` -> `source_runs.publish_status =
//!   no_change`; the artifact row is NOT written (the digest only ever
//!   advances after a real publish, §3.4 (5)).
//! * `Published` -> `mark_artifact_published` (digest, run id, gist id) and
//!   `publish_status = published`. A composite publish whose Gist leg failed
//!   is still `published` -- the local leg is authoritative -- with the
//!   sanitized Gist reason recorded in `error_code` as a note.
//! * `Failed { code, .. }` -> `publish_status = publish_failed`,
//!   `error_code = code.as_str()`; no retry at this layer, because the
//!   publisher's own bounded backoff already ran inside the call
//!   (`Failed.retryable` only says a LATER ROUND may succeed).
//!
//! Provider `local` (the default) is an immediate no-op: zero construction,
//! zero requests, zero writes -- the default configuration behaves exactly as
//! before this module existed. The publisher is constructed ONCE per call
//! (the token file is read there); a construction failure marks every
//! eligible source `gist_config_error` and keeps the local results.
//!
//! The ledger is authoritative for content, exactly like the legacy Sub-Store
//! push path: the bytes published are the active snapshot's bytes, not a
//! re-read of the export root.

use probe_config::{PublishProvider, PublishSection, SourceSpec};
use probe_publish::{
    ArtifactRef, CompositePublisher, GistPublisher, LocalPublisher, PublishOutcome, Publisher,
};
use probe_storage::runs::{
    PUBLISH_CONFIG_ERROR, PUBLISH_FAILED, PUBLISH_NO_CHANGE, PUBLISH_PUBLISHED,
};
use probe_storage::Storage;

use crate::publish::{PublishRoundOutcome, SourcePublication};

/// Everything one remote publish pass needs.
pub struct RemotePublishRequest<'a> {
    /// The `publish` section: `provider` picks the publisher, `gist` carries
    /// the credentials and retry bounds.
    pub publish: &'a PublishSection,
    /// The export root the local leg materialized into (the composite's local
    /// leg writes there too; it is a content-gated no-op after the round).
    pub export_root: &'a std::path::Path,
    /// The just-finished round's publication outcome. Only sources that
    /// actually published locally (`Published`) are eligible for the remote
    /// leg; skipped and failed ones keep their previous artifact everywhere.
    pub outcome: &'a PublishRoundOutcome,
    /// The round row this publication belongs to: the run slot's `round_id`
    /// and the artifact's `last_success_run_id`.
    pub round_id: i64,
    /// The configured sources, for the display name and enabled flag a
    /// not-yet-registered key needs when its registry rows are created here.
    pub sources: &'a [SourceSpec],
}

/// One source's remote publish result, as it was persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePublishEntry {
    /// The export key (the configured source name).
    pub key: String,
    /// The ledger identity the artifact publishes under.
    pub source_id: String,
    /// The fixed destination file name.
    pub gist_file: String,
    /// The `source_runs.publish_status` written for this source.
    pub status: &'static str,
    /// The publisher's error code, or the composite's gist note, exactly as
    /// persisted in `source_runs.error_code`.
    pub error_code: Option<String>,
}

/// What one remote publish pass did. Never carries content or credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePublishReport {
    /// The provider the request carried, echoed for the caller's log.
    pub provider: PublishProvider,
    /// `false` when no publisher was built: the provider is `local` (none is
    /// needed) or construction failed (see `entries` for what was recorded).
    pub constructed: bool,
    /// One entry per locally published source, in round order.
    pub entries: Vec<RemotePublishEntry>,
}

/// Run the remote leg for a finished round's publication.
///
/// The publisher is constructed once per call; the caller owns the storage
/// lock discipline exactly as for `publish_round` (a std guard must never
/// cross an await -- see the CLI and API call sites, which run this on a
/// blocking thread with its own current-thread runtime).
pub async fn publish_round_remote(
    storage: &Storage,
    request: RemotePublishRequest<'_>,
) -> RemotePublishReport {
    match request.publish.provider {
        // Zero behavior change on the default configuration: the local
        // pipeline already committed, and there is no remote leg to run.
        PublishProvider::Local => RemotePublishReport {
            provider: PublishProvider::Local,
            constructed: false,
            entries: Vec::new(),
        },
        PublishProvider::Gist => match GistPublisher::from_config(&request.publish.gist) {
            Ok(publisher) => publish_round_remote_with(storage, request, &publisher).await,
            Err(error) => construction_failed(storage, request, &error),
        },
        PublishProvider::Composite => {
            let build = || -> Result<CompositePublisher, probe_publish::PublishError> {
                Ok(CompositePublisher::new(
                    LocalPublisher::new(request.export_root),
                    GistPublisher::from_config(&request.publish.gist)?,
                ))
            };
            match build() {
                Ok(publisher) => publish_round_remote_with(storage, request, &publisher).await,
                Err(error) => construction_failed(storage, request, &error),
            }
        }
    }
}

/// The same pass with a caller-supplied publisher -- how tests (and any
/// future provider) inject a destination without touching the config.
pub async fn publish_round_remote_with<P: Publisher>(
    storage: &Storage,
    request: RemotePublishRequest<'_>,
    publisher: &P,
) -> RemotePublishReport {
    let gist_id = request
        .publish
        .gist
        .gist_id
        .clone()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    let mut entries = Vec::new();
    for publication in &request.outcome.sources {
        let SourcePublication::Published { key, .. } = publication else {
            continue;
        };
        entries
            .push(publish_one_remote(storage, &request, publisher, key, gist_id.as_deref()).await);
    }
    RemotePublishReport {
        provider: request.publish.provider,
        constructed: true,
        entries,
    }
}

/// The publisher could not be built (broken token file, unusable path): the
/// round's local results stand, nothing remote was attempted, and every
/// eligible source learns why through its run row.
fn construction_failed(
    storage: &Storage,
    request: RemotePublishRequest<'_>,
    error: &probe_publish::PublishError,
) -> RemotePublishReport {
    // One error for the operator, redacted by the publisher like every message.
    tracing::error!(
        code = error.code.as_str(),
        "cannot construct the remote publisher; local results stand, nothing was sent"
    );
    let mut entries = Vec::new();
    for key in eligible_keys(request.outcome) {
        let resolved = resolve_artifact(storage, &request, &key);
        let (source_id, gist_file) = match &resolved {
            Ok(row) => (row.source_id.clone(), row.gist_file.clone()),
            Err(_) => (key.clone(), String::new()),
        };
        if let Ok(row) = resolved {
            record_publish_status(
                storage,
                &row.source_id,
                request.round_id,
                PUBLISH_CONFIG_ERROR,
                Some(error.code.as_str()),
            );
        }
        entries.push(RemotePublishEntry {
            key,
            source_id,
            gist_file,
            status: PUBLISH_CONFIG_ERROR,
            error_code: Some(error.code.as_str().to_string()),
        });
    }
    RemotePublishReport {
        provider: request.publish.provider,
        constructed: false,
        entries,
    }
}

/// The export keys that published locally this round, in round order.
fn eligible_keys(outcome: &PublishRoundOutcome) -> Vec<String> {
    outcome
        .sources
        .iter()
        .filter_map(|publication| match publication {
            SourcePublication::Published { key, .. } => Some(key.clone()),
            SourcePublication::Skipped { .. } | SourcePublication::Failed { .. } => None,
        })
        .collect()
}

/// The artifact row a key publishes through, creating the registry rows on
/// first sight (the startup sync may not have met this key yet).
fn resolve_artifact(
    storage: &Storage,
    request: &RemotePublishRequest<'_>,
    key: &str,
) -> Result<probe_storage::sources::ArtifactRow, String> {
    let spec = request.sources.iter().find(|spec| spec.key == key);
    crate::sources::artifact_for_export_key(
        storage,
        key,
        spec.map(|spec| spec.label.as_str()),
        spec.map(|spec| spec.enabled).unwrap_or(true),
    )
    .map_err(|error| format!("artifact registry: {error}"))
}

/// Publish one source's active snapshot remotely and persist the outcome.
async fn publish_one_remote<P: Publisher>(
    storage: &Storage,
    request: &RemotePublishRequest<'_>,
    publisher: &P,
    key: &str,
    gist_id: Option<&str>,
) -> RemotePublishEntry {
    let artifact = match resolve_artifact(storage, request, key) {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(key, %error, "remote publish skipped: no artifact row");
            return RemotePublishEntry {
                key: key.to_string(),
                source_id: key.to_string(),
                gist_file: String::new(),
                status: PUBLISH_FAILED,
                error_code: Some(probe_publish::PublishErrorCode::Config.as_str().to_string()),
            };
        }
    };
    // The ledger is authoritative: publish exactly the bytes the active
    // snapshot serves, and record its digest -- never a re-read of the file.
    let snapshot = match storage.current_export(key) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            tracing::warn!(key, "remote publish skipped: no active export snapshot");
            return RemotePublishEntry {
                key: key.to_string(),
                source_id: artifact.source_id.clone(),
                gist_file: artifact.gist_file.clone(),
                status: PUBLISH_FAILED,
                error_code: Some(probe_publish::PublishErrorCode::Config.as_str().to_string()),
            };
        }
        Err(error) => {
            tracing::warn!(key, %error, "remote publish skipped: ledger read failed");
            return RemotePublishEntry {
                key: key.to_string(),
                source_id: artifact.source_id.clone(),
                gist_file: artifact.gist_file.clone(),
                status: PUBLISH_FAILED,
                error_code: Some(probe_publish::PublishErrorCode::Config.as_str().to_string()),
            };
        }
    };
    let artifact_ref = ArtifactRef {
        source_id: artifact.source_id.clone(),
        gist_file: artifact.gist_file.clone(),
        previous_sha256: artifact.last_content_sha256.clone(),
    };
    let outcome = publisher.publish(&artifact_ref, &snapshot.content).await;
    let status = match &outcome {
        // The destination already holds exactly this content: record it, and
        // write NOTHING to the artifact row (the digest advances only after a
        // real publish, 《指导》§3.4 (5)).
        PublishOutcome::NoChange => {
            record_publish_status(
                storage,
                &artifact.source_id,
                request.round_id,
                PUBLISH_NO_CHANGE,
                None,
            );
            PUBLISH_NO_CHANGE
        }
        // 2xx with the file present -- `Published` means exactly that. The
        // composite's gist-side failure rides along as a note: the local leg
        // is authoritative and the publish did not fail.
        PublishOutcome::Published { gist_failed, .. } => {
            if let Err(error) = storage.mark_artifact_published(
                &artifact.artifact_id,
                &snapshot.summary.sha256,
                request.round_id,
                gist_id,
                None,
            ) {
                tracing::error!(key, %error, "published remotely; the artifact row could not be updated");
            }
            record_publish_status(
                storage,
                &artifact.source_id,
                request.round_id,
                PUBLISH_PUBLISHED,
                gist_failed.as_deref(),
            );
            PUBLISH_PUBLISHED
        }
        // The publisher already ran its bounded backoff inside the call; this
        // layer never retries (`retryable` says a LATER ROUND may succeed).
        // The previous snapshot, file and digest all stand (§3.4 (6)).
        PublishOutcome::Failed {
            code, retryable, ..
        } => {
            tracing::warn!(
                key,
                code = code.as_str(),
                retryable,
                "remote publish failed; the previous artifact stands"
            );
            record_publish_status(
                storage,
                &artifact.source_id,
                request.round_id,
                PUBLISH_FAILED,
                Some(code.as_str()),
            );
            PUBLISH_FAILED
        }
    };
    RemotePublishEntry {
        key: key.to_string(),
        source_id: artifact.source_id.clone(),
        gist_file: artifact.gist_file.clone(),
        status,
        error_code: match &outcome {
            PublishOutcome::Failed { code, .. } => Some(code.as_str().to_string()),
            PublishOutcome::Published {
                gist_failed: Some(note),
                ..
            } => Some(note.clone()),
            _ => None,
        },
    }
}

/// Persist one source's publish outcome on its run slot. A failure to record
/// bookkeeping is logged, never propagated: the publish itself already
/// happened (or did not), and the round must not learn about this.
fn record_publish_status(
    storage: &Storage,
    source_id: &str,
    round_id: i64,
    status: &'static str,
    error_code: Option<&str>,
) {
    if let Err(error) =
        storage.record_source_publish_status(source_id, round_id, status, error_code)
    {
        tracing::error!(%source_id, round_id, status, %error, "could not record the publish status");
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use probe_config::{GistSection, PublishProvider, PublishSection};
    use probe_publish::{ArtifactRef, PublishError, PublishErrorCode, PublishOutcome, Publisher};
    use probe_storage::publication::{export_hash, PublicationInput};

    use super::*;

    /// Valid export documents (the ledger stores exactly these bytes): one
    /// opaque proxy dictionary, matching the `node_count` the input carries.
    const CONTENT_A: &str = "{\"proxies\": [{\"name\": \"a\", \"type\": \"ss\"}]}\n";

    /// One recorded publish call: (source_id, gist_file, previous_sha256,
    /// content).
    type RecordedCall = (String, String, Option<String>, String);

    /// A publisher that records every invocation and answers with a canned
    /// outcome. Never talks to a network.
    struct FakePublisher {
        calls: Mutex<Vec<RecordedCall>>,
        outcome: PublishOutcome,
    }

    impl FakePublisher {
        fn new(outcome: PublishOutcome) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                outcome,
            }
        }

        fn recorded(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Publisher for FakePublisher {
        async fn publish(&self, artifact: &ArtifactRef, content: &str) -> PublishOutcome {
            self.calls.lock().unwrap().push((
                artifact.source_id.clone(),
                artifact.gist_file.clone(),
                artifact.previous_sha256.clone(),
                content.to_string(),
            ));
            self.outcome.clone()
        }

        async fn healthcheck(&self, _gist_id: &str) -> Result<(), PublishError> {
            Ok(())
        }
    }

    /// A ledger with one synced source ("air"), one real round row and one
    /// active export snapshot -- the state `publish_round` leaves behind.
    fn fixture() -> (Storage, i64, PublishRoundOutcome) {
        let storage = Storage::open_in_memory().unwrap();
        let source_id = storage
            .sync_source(
                "air",
                "Air",
                probe_storage::sources::INPUT_COLLECTION,
                "air",
                true,
            )
            .unwrap();
        storage.ensure_artifact(&source_id, "air.yaml").unwrap();
        let round_id = storage.start_round("test", None).unwrap();
        let publication = storage
            .publish_export(PublicationInput {
                key: "air",
                content: CONTENT_A,
                sha256: &export_hash(CONTENT_A),
                node_count: 1,
                actor: "test",
                forced: false,
                expected_active: None,
            })
            .unwrap();
        let outcome = PublishRoundOutcome {
            skipped: None,
            sources: vec![SourcePublication::Published {
                key: "air".to_string(),
                node_count: 1,
                snapshot_id: publication.snapshot.id,
                content_path: PathBuf::from("/unused"),
            }],
            dropped: Vec::new(),
            push: Vec::new(),
            pruned: None,
        };
        (storage, round_id, outcome)
    }

    /// An owned publish section for a provider, with a configured gist id.
    fn publish_cfg(provider: PublishProvider) -> PublishSection {
        PublishSection {
            provider,
            gist: GistSection {
                gist_id: Some("gist0001abcd".to_string()),
                ..GistSection::default()
            },
            ..PublishSection::default()
        }
    }

    /// A request borrowing the caller-owned config (the struct borrows, so the
    /// config must outlive it in the test body).
    fn request<'a>(
        publish: &'a PublishSection,
        round_id: i64,
        outcome: &'a PublishRoundOutcome,
    ) -> RemotePublishRequest<'a> {
        RemotePublishRequest {
            publish,
            export_root: std::path::Path::new("/unused"),
            outcome,
            round_id,
            sources: &[],
        }
    }

    fn artifact_of(storage: &Storage, source_id: &str) -> probe_storage::sources::ArtifactRow {
        storage
            .artifact_for_source(source_id)
            .unwrap()
            .expect("the fixture created the artifact")
    }

    fn run_row_of(storage: &Storage, source_id: &str) -> probe_storage::runs::SourceRunRow {
        storage
            .list_source_runs(source_id, 10)
            .unwrap()
            .into_iter()
            .next()
            .expect("a publish status was recorded")
    }

    /// The default provider is an exact no-op: nothing constructed, nothing
    /// recorded, nothing written.
    #[tokio::test]
    async fn provider_local_never_runs_the_remote_leg() {
        let (storage, round_id, outcome) = fixture();
        let source_id = storage.source_id_for_key("air").unwrap().unwrap();
        let publish = publish_cfg(PublishProvider::Local);
        let request = request(&publish, round_id, &outcome);
        let report = publish_round_remote(&storage, request).await;
        assert_eq!(report.provider, PublishProvider::Local);
        assert!(!report.constructed);
        assert!(report.entries.is_empty());
        assert!(
            storage.list_source_runs(&source_id, 10).unwrap().is_empty(),
            "no run rows without a remote leg"
        );
        assert_eq!(artifact_of(&storage, &source_id).last_content_sha256, None);
    }

    /// A locally skipped source is never eligible: the fake publisher is
    /// never called and nothing is recorded.
    #[tokio::test]
    async fn skipped_sources_are_not_eligible() {
        let (storage, round_id, mut outcome) = fixture();
        outcome.sources = vec![SourcePublication::Skipped {
            key: "air".to_string(),
            reason: crate::publish::SkipReason::NoAliveNodes,
        }];
        let publisher = FakePublisher::new(PublishOutcome::NoChange);
        let publish = publish_cfg(PublishProvider::Gist);
        let request = request(&publish, round_id, &outcome);
        let report = publish_round_remote_with(&storage, request, &publisher).await;
        assert!(report.entries.is_empty());
        assert!(publisher.recorded().is_empty());
    }

    /// NoChange records `no_change` and writes NOTHING to the artifact row
    /// (the digest advances only after a real publish, §3.4 (5)). The fake
    /// received the row's (unknown) baseline and the snapshot's exact bytes.
    #[tokio::test]
    async fn no_change_records_the_status_and_no_artifact_write() {
        let (storage, round_id, outcome) = fixture();
        let source_id = storage.source_id_for_key("air").unwrap().unwrap();
        let publisher = FakePublisher::new(PublishOutcome::NoChange);
        let publish = publish_cfg(PublishProvider::Gist);
        let request = request(&publish, round_id, &outcome);
        let report = publish_round_remote_with(&storage, request, &publisher).await;
        assert_eq!(report.entries.len(), 1);
        let entry = &report.entries[0];
        assert_eq!(entry.key, "air");
        assert_eq!(entry.status, PUBLISH_NO_CHANGE);
        assert_eq!(entry.error_code, None);

        assert_eq!(publisher.recorded().len(), 1, "one publish call");
        let (called_source, called_file, previous, content) = &publisher.recorded()[0];
        assert_eq!(called_source, &source_id);
        assert_eq!(called_file, "air.yaml");
        assert_eq!(
            previous.as_deref(),
            artifact_of(&storage, &source_id)
                .last_content_sha256
                .as_deref()
        );
        assert_eq!(content, &CONTENT_A, "the snapshot's exact bytes");

        assert_eq!(
            artifact_of(&storage, &source_id).last_content_sha256,
            None,
            "a no-change must not advance the digest"
        );
        let row = run_row_of(&storage, &source_id);
        assert_eq!(row.publish_status.as_deref(), Some(PUBLISH_NO_CHANGE));
        assert_eq!(row.error_code, None);
        assert_eq!(row.round_id, Some(round_id));
    }

    /// `Published` advances the artifact's digest exactly once (to the
    /// snapshot's digest), binds the run id and the configured gist id, and
    /// records `published` with no error.
    #[tokio::test]
    async fn published_advances_the_artifact_and_records_the_run() {
        let (storage, round_id, outcome) = fixture();
        let source_id = storage.source_id_for_key("air").unwrap().unwrap();
        let publisher = FakePublisher::new(PublishOutcome::Published {
            revision: Some(export_hash(CONTENT_A)[..12].to_string()),
            gist_failed: None,
        });
        let publish = publish_cfg(PublishProvider::Gist);
        let request = request(&publish, round_id, &outcome);
        let report = publish_round_remote_with(&storage, request, &publisher).await;
        assert_eq!(report.entries[0].status, PUBLISH_PUBLISHED);
        assert_eq!(report.entries[0].error_code, None);

        let artifact = artifact_of(&storage, &source_id);
        assert_eq!(
            artifact.last_content_sha256.as_deref(),
            Some(export_hash(CONTENT_A).as_str()),
            "the digest advanced to the published content"
        );
        assert_eq!(artifact.last_success_run_id, Some(round_id));
        assert_eq!(artifact.gist_id.as_deref(), Some("gist0001abcd"));
        let row = run_row_of(&storage, &source_id);
        assert_eq!(row.publish_status.as_deref(), Some(PUBLISH_PUBLISHED));
        assert_eq!(row.error_code, None);
    }

    /// `Failed` records `publish_failed` with the frozen error code and
    /// leaves the artifact row (digest, run id, gist id) untouched -- the
    /// next round re-attempts the same content (§3.4 (6)).
    #[tokio::test]
    async fn a_failed_publish_persists_the_error_code() {
        let (storage, round_id, outcome) = fixture();
        let source_id = storage.source_id_for_key("air").unwrap().unwrap();
        let publisher = FakePublisher::new(PublishOutcome::Failed {
            code: PublishErrorCode::GistAuth,
            retryable: false,
            message: "GitHub rejected the credentials (401)".to_string(),
        });
        let publish = publish_cfg(PublishProvider::Gist);
        let request = request(&publish, round_id, &outcome);
        let report = publish_round_remote_with(&storage, request, &publisher).await;
        assert_eq!(report.entries[0].status, PUBLISH_FAILED);
        assert_eq!(
            report.entries[0].error_code.as_deref(),
            Some("gist_auth"),
            "the frozen code spelling is what is persisted"
        );

        let artifact = artifact_of(&storage, &source_id);
        assert_eq!(artifact.last_content_sha256, None, "the digest stands");
        assert_eq!(artifact.last_success_run_id, None);
        assert_eq!(artifact.gist_id, None);
        let row = run_row_of(&storage, &source_id);
        assert_eq!(row.publish_status.as_deref(), Some(PUBLISH_FAILED));
        assert_eq!(row.error_code.as_deref(), Some("gist_auth"));
    }

    /// A composite publish whose Gist leg failed is still a success -- the
    /// local leg is authoritative -- with the sanitized note recorded.
    #[tokio::test]
    async fn a_gist_side_note_is_recorded_without_failing_the_publish() {
        let (storage, round_id, outcome) = fixture();
        let source_id = storage.source_id_for_key("air").unwrap().unwrap();
        let note = "gist_missing: gist not found or not visible (404)".to_string();
        let publisher = FakePublisher::new(PublishOutcome::Published {
            revision: Some(export_hash(CONTENT_A)[..12].to_string()),
            gist_failed: Some(note.clone()),
        });
        let publish = publish_cfg(PublishProvider::Composite);
        let request = request(&publish, round_id, &outcome);
        let report = publish_round_remote_with(&storage, request, &publisher).await;
        assert_eq!(report.entries[0].status, PUBLISH_PUBLISHED);
        assert_eq!(report.entries[0].error_code.as_deref(), Some(note.as_str()));

        let artifact = artifact_of(&storage, &source_id);
        assert_eq!(
            artifact.last_content_sha256.as_deref(),
            Some(export_hash(CONTENT_A).as_str()),
            "the local leg is authoritative: the digest advanced"
        );
        let row = run_row_of(&storage, &source_id);
        assert_eq!(row.publish_status.as_deref(), Some(PUBLISH_PUBLISHED));
        assert_eq!(row.error_code.as_deref(), Some(note.as_str()));
    }

    /// A broken publisher setup (here: no token file) is a construction
    /// failure: one report, every eligible source `gist_config_error`, the
    /// artifact row untouched, and the round's own outcome object unchanged.
    #[tokio::test]
    async fn a_construction_failure_marks_config_error_and_keeps_local_results() {
        let (storage, round_id, outcome) = fixture();
        let source_id = storage.source_id_for_key("air").unwrap().unwrap();
        let publish = PublishSection {
            provider: PublishProvider::Gist,
            // `token_file: None` is the construction failure.
            gist: GistSection::default(),
            ..PublishSection::default()
        };
        let request = RemotePublishRequest {
            publish: &publish,
            export_root: std::path::Path::new("/unused"),
            outcome: &outcome,
            round_id,
            sources: &[],
        };
        let before = outcome.clone();
        let report = publish_round_remote(&storage, request).await;
        assert!(!report.constructed);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].status, PUBLISH_CONFIG_ERROR);
        assert_eq!(
            report.entries[0].error_code.as_deref(),
            Some(PublishErrorCode::Config.as_str())
        );
        assert_eq!(
            report.entries[0].source_id, source_id,
            "the entry still names the real registry identity"
        );
        assert_eq!(
            outcome.sources, before.sources,
            "the round's outcome object is never modified"
        );
        assert_eq!(outcome.skipped, before.skipped);
        assert_eq!(
            artifact_of(&storage, &source_id).last_content_sha256,
            None,
            "nothing remote happened, so nothing advanced"
        );
        let row = run_row_of(&storage, &source_id);
        assert_eq!(row.publish_status.as_deref(), Some(PUBLISH_CONFIG_ERROR));
        assert_eq!(row.error_code.as_deref(), Some("config"));

        // The composite provider fails the same way through the same path.
        let publish = PublishSection {
            provider: PublishProvider::Composite,
            ..PublishSection::default()
        };
        let request = RemotePublishRequest {
            publish: &publish,
            export_root: std::path::Path::new("/unused"),
            outcome: &outcome,
            round_id,
            sources: &[],
        };
        let report = publish_round_remote(&storage, request).await;
        assert!(!report.constructed);
        assert_eq!(report.entries[0].status, PUBLISH_CONFIG_ERROR);
    }

    /// A source that published locally but whose snapshot vanished before the
    /// remote leg ran is recorded as a failed publish with the config code,
    /// and never panics the round.
    #[tokio::test]
    async fn a_missing_snapshot_is_a_recorded_failure() {
        let (storage, round_id, mut outcome) = fixture();
        outcome.sources[0] = SourcePublication::Published {
            key: "ghost".to_string(),
            node_count: 1,
            snapshot_id: 1,
            content_path: PathBuf::from("/unused"),
        };
        let publisher = FakePublisher::new(PublishOutcome::NoChange);
        let publish = publish_cfg(PublishProvider::Gist);
        let request = request(&publish, round_id, &outcome);
        let report = publish_round_remote_with(&storage, request, &publisher).await;
        assert_eq!(report.entries[0].status, PUBLISH_FAILED);
        assert_eq!(report.entries[0].error_code.as_deref(), Some("config"));
        assert!(
            publisher.recorded().is_empty(),
            "no content, no publish call"
        );
    }
}
