//! Ledger-side source registry helpers (workstreams/18).
//!
//! The config file lists what to measure; the ledger records *which logical
//! source* a measurement belongs to, for the GUI, for the stable artifact URL,
//! and for the publisher's NO_CHANGE gate. `sync_ledger_sources` is the single
//! bridge between the two: it runs wherever a `Vec<SourceSpec>` meets an open
//! ledger (service startup, manual rounds), and it is idempotent -- re-running
//! it can only ever update attributes, never mint a second source or a second
//! artifact for the same key.

use probe_config::SourceSpec;
use probe_storage::sources::{ArtifactRow, SourceRow, INPUT_UNKNOWN};
use probe_storage::Storage;

/// Sync every configured source into the registry and make sure each has its
/// one artifact. Returns the number of sources touched.
///
/// A key missing from the config is left alone: disabling a source is an
/// attribute change history has to keep, not a reason to delete identity.
pub fn sync_ledger_sources(storage: &Storage, sources: &[SourceSpec]) -> Result<usize, String> {
    let mut touched = 0;
    for spec in sources {
        sync_one(storage, spec).map_err(|error| format!("source {}: {error}", spec.key))?;
        touched += 1;
    }
    Ok(touched)
}

/// Sync one configured source and return its artifact.
fn sync_one(storage: &Storage, spec: &SourceSpec) -> Result<ArtifactRow, String> {
    // The label is the human name; a source that never set one shows its
    // config key. `input_ref` is the Sub-Store object the rows came from.
    let display_name = pick_name(&spec.label).unwrap_or_else(|| spec.key.clone());
    let input_ref = pick_name(&spec.name).unwrap_or_else(|| spec.key.clone());
    let source_id = storage
        .sync_source(
            &spec.key,
            &display_name,
            &spec.kind,
            &input_ref,
            spec.enabled,
        )
        .map_err(|error| error.to_string())?;
    storage
        .ensure_artifact(&source_id, &default_gist_file(&spec.key))
        .map_err(|error| error.to_string())
}

/// The Gist file an export key publishes under: the key is already a safe file
/// name (it names an export directory entry today), so the mapping is the key
/// plus the YAML suffix.
pub fn default_gist_file(key: &str) -> String {
    format!("{key}.yaml")
}

/// A non-empty trimmed string, or None -- the shape both display names and
/// input references need before they reach the ledger.
fn pick_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The artifact an export key publishes through, creating the registry rows on
/// first sight so a publisher works even before the startup sync ran (a CLI
/// one-shot round, or a round against an older database).
///
/// Falls back to `default_gist_file(key)` for the file name, which is exactly
/// what the startup sync would have chosen; a key configured under a *different*
/// name keeps that name, because the row already exists.
pub fn artifact_for_export_key(
    storage: &Storage,
    key: &str,
    display_name: Option<&str>,
    enabled: bool,
) -> Result<ArtifactRow, String> {
    let existing = storage
        .source_id_for_key(key)
        .map_err(|error| error.to_string())?;
    let source_id = match existing {
        Some(source_id) => source_id,
        None => {
            let name = pick_name(display_name.unwrap_or("")).unwrap_or_else(|| key.to_string());
            // A publisher that met an unknown key still has to record it; the
            // kind is unknown here, and the startup sync corrects it.
            storage
                .sync_source(key, &name, INPUT_UNKNOWN, key, enabled)
                .map_err(|error| error.to_string())?
        }
    };
    storage
        .ensure_artifact(&source_id, &default_gist_file(key))
        .map_err(|error| error.to_string())
}

/// The source row behind an export key, for the GUI's per-source card.
pub fn source_for_export_key(storage: &Storage, key: &str) -> Result<Option<SourceRow>, String> {
    match storage
        .source_id_for_key(key)
        .map_err(|error| error.to_string())?
    {
        Some(source_id) => storage
            .source_by_id(&source_id)
            .map_err(|error| error.to_string()),
        None => Ok(None),
    }
}
