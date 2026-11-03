//! The GitHub Gist publisher (workstreams/18 B2, F-031..F-037).
//!
//! One dedicated Gist holds one file per source plus a `manifest.json` that
//! records, per `source_id`, what is published (file, sha256, revision,
//! status). Every write is a content-hash-gated PATCH that touches ONLY the
//! target file key and `manifest.json` -- never `content: null`, never the
//! other sources' files.
//!
//! The write protocol per publish, under a process-wide per-gist lock:
//!
//! 1. `GET /gists/{id}` -- current remote state (F-033 step 2).
//! 2. Merge the source's manifest entry into the remote manifest.
//! 3. `PATCH /gists/{id}` with `{"files": {"<gist_file>": {"content": ...},
//!    "manifest.json": {"content": ...}}}`.
//!
//! Retries: 401/403 and 404 are terminal; 429 honors `Retry-After` (bounded);
//! 5xx and network errors back off exponentially (1s -> 2s -> 4s in prod,
//! injectable base for tests), `max_retries` attempts in total; 409/422 get
//! one re-read-and-merge retry. The token is read from a file at
//! construction and is redacted from every error message.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use probe_config::GistSection;
use reqwest::header::{HeaderMap, ACCEPT, AUTHORIZATION, RETRY_AFTER, USER_AGENT};
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Map, Value};

use crate::types::{
    sha256_hex, validate_gist_file, ArtifactRef, PublishError, PublishErrorCode, PublishOutcome,
};
use crate::Publisher;

/// Production API base. Tests point the publisher at a local stub instead --
/// the only URL that ever leaves this process in tests is 127.0.0.1.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// The manifest every dedicated Gist carries (F-031). Reserved: a source
/// cannot publish under this name.
pub const MANIFEST_FILE: &str = "manifest.json";

/// GitHub rejects unsigned clients; a stable UA also makes the deployment
/// recognizable in GitHub's request logs.
const USER_AGENT_VALUE: &str = concat!("mihomo-probe-publish/", env!("CARGO_PKG_VERSION"));

/// The API-version header GitHub documents for the current REST surface.
const API_VERSION: &str = "2022-11-28";

/// Production backoff between attempts: 1s -> 2s -> 4s (F-037). Injectable
/// so the stub tests run in milliseconds.
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// A `Retry-After` above this is clamped: a server asking for an hour must
/// not hang a round; the bounded failure stays retryable and the next round
/// (or the operator) takes over.
pub const DEFAULT_MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// The manifest status this publisher writes for its own entries.
const STATUS_ACTIVE: &str = "active";

/// Process-wide per-gist write locks (F-033): two sources PATCHing one Gist
/// must never interleave their read-merge-write, whoever constructed their
/// publisher. The map is behind a std Mutex because its critical section
/// never awaits; the per-gist locks are tokio Mutexes because they are held
/// across HTTP calls.
static GIST_LOCKS: OnceLock<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

fn lock_for(gist_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let map = GIST_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    map.lock()
        .expect("gist lock map poisoned")
        .entry(gist_id.to_string())
        .or_default()
        .clone()
}

/// The manifest record one source owns inside a Gist (F-031). The manifest is
/// an object keyed by `source_id`, so merging this source's entry preserves
/// every other entry untouched.
#[derive(Debug, Clone)]
struct ManifestEntry {
    source_id: String,
    file: String,
    sha256: String,
    revision: String,
    published_at: String,
    status: &'static str,
}

impl ManifestEntry {
    fn new(source_id: &str, file: &str, sha256: &str) -> Self {
        Self {
            source_id: source_id.to_string(),
            file: file.to_string(),
            sha256: sha256.to_string(),
            revision: crate::types::revision_of(sha256),
            published_at: now_rfc3339(),
            status: STATUS_ACTIVE,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "source_id": self.source_id,
            "file": self.file,
            "sha256": self.sha256,
            "published_at": self.published_at,
            "revision": self.revision,
            "status": self.status,
        })
    }
}

/// RFC3339 UTC now; a clock that cannot format still yields a stable string
/// so a manifest write never fails on the timestamp alone.
fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// A gist id must be a plain URL path segment: GitHub ids are alphanumeric,
/// and anything with slashes or control characters would change the request
/// path or smuggle a header.
fn validate_gist_id(gist_id: &str) -> Result<(), PublishError> {
    let id = gist_id.trim();
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    if valid {
        Ok(())
    } else {
        Err(PublishError::terminal(
            PublishErrorCode::Config,
            "publish.gist.gist_id must be a bare gist id",
        ))
    }
}

/// Remove any accidental occurrence of `secret` from `text`. Defense in
/// depth: response bodies and client error strings are third-party input,
/// and a message echoing the token would put it in logs and SQLite.
fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "[redacted]")
}

/// A char-safe prefix of `text`, for the bounded body snippets in messages.
fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Read the GitHub token from its file. This is the only way a token enters
/// the publisher: never config.json, never SQLite, never the environment.
/// Failing here -- missing, unreadable, empty, or (on Unix) group/other
/// readable -- is a CONSTRUCTION error; the publisher never discovers a
/// broken token setup as a network round-trip.
fn read_token(section: &GistSection) -> Result<String, PublishError> {
    let path = section.token_file.as_deref().ok_or_else(|| {
        PublishError::terminal(
            PublishErrorCode::Config,
            "publish.gist.token_file is not configured",
        )
    })?;
    let raw = std::fs::read_to_string(path).map_err(|error| {
        PublishError::terminal(
            PublishErrorCode::Config,
            format!("token file {} is unreadable: {error}", path.display()),
        )
    })?;
    let token = raw.trim();
    if token.is_empty() {
        return Err(PublishError::terminal(
            PublishErrorCode::Config,
            format!("token file {} is empty", path.display()),
        ));
    }
    check_token_mode(path)?;
    Ok(token.to_string())
}

/// The token file must not be readable by group or others (0400 and 0600
/// both pass). Only meaningful on Unix; NTFS ACLs are out of std's reach.
#[cfg(unix)]
fn check_token_mode(path: &Path) -> Result<(), PublishError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|error| {
            PublishError::terminal(
                PublishErrorCode::Config,
                format!("token file {} is unreadable: {error}", path.display()),
            )
        })?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(PublishError::terminal(
            PublishErrorCode::Config,
            format!(
                "token file {} must not be group/other readable (chmod 600), found {mode:o}",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_token_mode(_path: &Path) -> Result<(), PublishError> {
    Ok(())
}

/// Publishes one artifact into a GitHub Gist.
///
/// Constructed from `probe_config::GistSection`; the token comes ONLY from
/// the configured token file and every constructor below fails before any
/// network use if that file is missing or unusable.
#[derive(Debug, Clone)]
pub struct GistPublisher {
    http: Client,
    api_base: String,
    token: String,
    gist_id: Option<String>,
    max_retries: u32,
    backoff_base: Duration,
    max_retry_after: Duration,
}

impl GistPublisher {
    /// Production constructor: token from `section.token_file`, API at
    /// `https://api.github.com`.
    pub fn from_config(section: &GistSection) -> Result<Self, PublishError> {
        Self::with_api_base(section, GITHUB_API_BASE)
    }

    /// Constructor against a custom API base -- how the stub tests aim the
    /// publisher at 127.0.0.1 and never at GitHub.
    pub fn with_api_base(section: &GistSection, api_base: &str) -> Result<Self, PublishError> {
        let token = read_token(section)?;
        let http = Client::builder()
            .timeout(Duration::from_secs(section.timeout_seconds))
            .build()
            .map_err(|error| {
                PublishError::terminal(PublishErrorCode::Config, format!("http client: {error}"))
            })?;
        Ok(Self {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            token,
            gist_id: section
                .gist_id
                .clone()
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty()),
            max_retries: section.max_retries.max(1),
            backoff_base: DEFAULT_BACKOFF_BASE,
            max_retry_after: DEFAULT_MAX_RETRY_AFTER,
        })
    }

    /// Injectable base backoff (prod 1s; tests use milliseconds). Builder
    /// style: `publisher.with_backoff_base(Duration::from_millis(2))`.
    pub fn with_backoff_base(mut self, base: Duration) -> Self {
        self.backoff_base = base;
        self
    }

    /// Injectable `Retry-After` clamp (prod 30s; tests use milliseconds) so
    /// a stub answering "retry in 3600 seconds" does not hang a test.
    pub fn with_max_retry_after(mut self, cap: Duration) -> Self {
        self.max_retry_after = cap;
        self
    }

    fn gist_url(&self, gist_id: &str) -> String {
        format!("{}/gists/{gist_id}", self.api_base)
    }

    /// The full publish protocol for one pass; the caller owns the lock.
    async fn publish_pass(
        &self,
        gist_id: &str,
        artifact: &ArtifactRef,
        content: &str,
        sha: &str,
        entry: &ManifestEntry,
    ) -> Result<PublishOutcome, PublishError> {
        let current = self.get_gist(gist_id).await?;
        let files = current.get("files").and_then(Value::as_object);
        // The remote content may be `null` when GitHub truncated the file
        // (>1MB); then no comparison is possible and the publish proceeds.
        let remote_target_sha = files
            .and_then(|map| map.get(artifact.gist_file.as_str()))
            .and_then(|file| file.get("content"))
            .and_then(Value::as_str)
            .map(|remote| sha256_hex(remote.as_bytes()));
        let manifest_raw = files
            .and_then(|map| map.get(MANIFEST_FILE))
            .and_then(|file| file.get("content"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let content_is_live = remote_target_sha.as_deref() == Some(sha);
        if content_is_live && manifest_entry_matches(manifest_raw.as_deref(), entry) {
            // Gate 2 (one GET, zero PATCH): the destination already holds
            // exactly this content with a current manifest entry.
            return Ok(PublishOutcome::NoChange);
        }

        let manifest = merged_manifest(manifest_raw.as_deref(), entry)?;
        let mut files_body = Map::new();
        if !content_is_live {
            // Only ever this file key, only ever `{"content": "..."}` --
            // never a null, which is how GitHub DELETES a file.
            files_body.insert(artifact.gist_file.clone(), json!({"content": content}));
        }
        files_body.insert(MANIFEST_FILE.to_string(), json!({"content": manifest}));
        self.send_with_retries(
            Method::PATCH,
            &self.gist_url(gist_id),
            Some(json!({"files": files_body})),
        )
        .await?;
        Ok(PublishOutcome::Published {
            revision: Some(entry.revision.clone()),
            gist_failed: None,
        })
    }

    async fn get_gist(&self, gist_id: &str) -> Result<Value, PublishError> {
        let response = self
            .send_with_retries(Method::GET, &self.gist_url(gist_id), None)
            .await?;
        response.json::<Value>().await.map_err(|error| {
            PublishError::retryable(
                PublishErrorCode::Network,
                redact(&format!("gist response was not JSON: {error}"), &self.token),
            )
        })
    }

    async fn send_with_retries(
        &self,
        method: Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response, PublishError> {
        let mut attempt = 1u32;
        loop {
            match self.send_once(method.clone(), url, body.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) if error.retryable && attempt < self.max_retries => {
                    let delay = self.delay_before_retry(attempt, &error);
                    tracing::warn!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        code = error.code.as_str(),
                        "gist request failed; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn send_once(
        &self,
        method: Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response, PublishError> {
        let mut request = self
            .http
            .request(method, url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(body) = &body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| {
            // Connect failures and timeouts are exactly the class a later
            // attempt can fix; the error text carries the URL, never headers.
            PublishError::retryable(
                PublishErrorCode::Network,
                redact(&format!("request failed: {error}"), &self.token),
            )
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = parse_retry_after(response.headers());
        let snippet = redact(
            &truncate(&response.text().await.unwrap_or_default(), 300),
            &self.token,
        );
        Err(status_error(status, retry_after, snippet))
    }

    /// The wait before retry number `attempt` (1-based) after `error`.
    fn delay_before_retry(&self, attempt: u32, error: &PublishError) -> Duration {
        if let Some(after) = error.retry_after {
            // Honor Retry-After, bounded: jitter spreads it, the clamp cuts
            // a server asking for an hour down to the configured maximum.
            return jitter(after).min(self.max_retry_after);
        }
        // base * 2^(attempt-1): with the 1s base, delays are 1s, 2s, 4s, ...
        let factor = 1u32 << (attempt - 1).min(16);
        jitter(self.backoff_base.saturating_mul(factor))
    }

    /// `GET /gists/{id}` as a health probe: one request, no retries. 200 is
    /// healthy; 401/403 map to `gist_auth`, 404 to `gist_missing`, and a
    /// rate limit or outage to their retryable codes.
    async fn probe(&self, gist_id: &str) -> Result<(), PublishError> {
        let target = match gist_id.trim() {
            "" => self.gist_id.as_deref().unwrap_or(""),
            explicit => explicit,
        };
        if target.is_empty() {
            return Err(PublishError::terminal(
                PublishErrorCode::Config,
                "no gist id configured to probe",
            ));
        }
        validate_gist_id(target)?;
        self.send_once(Method::GET, &self.gist_url(target), None)
            .await
            .map(|_| ())
    }
}

impl Publisher for GistPublisher {
    async fn publish(&self, artifact: &ArtifactRef, content: &str) -> PublishOutcome {
        if let Err(error) = validate_gist_file(&artifact.gist_file) {
            return PublishOutcome::from(error);
        }
        if artifact.gist_file == MANIFEST_FILE {
            return PublishOutcome::from(PublishError::terminal(
                PublishErrorCode::Config,
                "manifest.json is reserved for the manifest",
            ));
        }
        let Some(gist_id) = self
            .gist_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return PublishOutcome::from(PublishError::terminal(
                PublishErrorCode::Config,
                "publish.gist.gist_id is not configured",
            ));
        };
        if let Err(error) = validate_gist_id(gist_id) {
            return PublishOutcome::from(error);
        }

        let sha = sha256_hex(content.as_bytes());
        // Gate 1, before any HTTP: the artifact row already holds this
        // content, so there is nothing to PATCH (F-032). ZERO requests.
        if artifact.previous_sha256.as_deref() == Some(sha.as_str()) {
            return PublishOutcome::NoChange;
        }

        let entry = ManifestEntry::new(&artifact.source_id, &artifact.gist_file, &sha);
        // One writer per gist, process-wide (F-033): the read-merge-write
        // below never interleaves with another publisher instance targeting
        // the same gist, whatever constructed them.
        let lock = lock_for(gist_id);
        let _guard = lock.lock().await;
        let mut merge_retried = false;
        loop {
            match self
                .publish_pass(gist_id, artifact, content, &sha, &entry)
                .await
            {
                Ok(outcome) => return outcome,
                Err(error) if !merge_retried && matches!(error.status, Some(409 | 422)) => {
                    // F-037: a conflict means remote state moved; re-read and
                    // merge once. The lock already prevents OUR writers from
                    // racing -- this covers a human or a second deployment.
                    merge_retried = true;
                    tracing::warn!(
                        status = error.status.unwrap_or(0),
                        "gist conflict; re-reading remote state and merging once"
                    );
                }
                Err(error) => return PublishOutcome::from(error),
            }
        }
    }

    async fn healthcheck(&self, gist_id: &str) -> Result<(), PublishError> {
        self.probe(gist_id).await
    }
}

/// Map a non-2xx response to its typed error. 401/403 and 404 are terminal;
/// 429 honors `Retry-After`; 5xx (and 409/422, which the caller re-reads and
/// merges for once) are retryable; any other 4xx is GitHub answering and is
/// not retried.
fn status_error(
    status: StatusCode,
    retry_after: Option<Duration>,
    body_snippet: String,
) -> PublishError {
    let mut error = match status.as_u16() {
        401 | 403 => PublishError::terminal(
            PublishErrorCode::GistAuth,
            format!("GitHub rejected the credentials ({status})"),
        ),
        404 => PublishError::terminal(
            PublishErrorCode::GistMissing,
            format!("gist not found or not visible ({status})"),
        ),
        429 => PublishError::retryable(
            PublishErrorCode::RateLimited,
            format!("rate limited ({status}); honoring Retry-After"),
        ),
        409 | 422 => PublishError::terminal(
            PublishErrorCode::Remote,
            format!("gist conflict ({status}); will re-read and merge once"),
        ),
        _ if status.is_server_error() => PublishError::retryable(
            PublishErrorCode::Remote,
            format!("GitHub server error ({status})"),
        ),
        _ => PublishError::terminal(
            PublishErrorCode::Remote,
            format!("unexpected GitHub response ({status})"),
        ),
    };
    error.status = Some(status.as_u16());
    error.retry_after = retry_after;
    if !body_snippet.is_empty() {
        error.message = format!("{}: {body_snippet}", error.message);
    }
    error
}

/// The `Retry-After` header in seconds; the HTTP-date form is ignored (the
/// backoff covers it).
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?;
    let seconds: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

/// ±10% jitter from clock entropy: spreads retry storms across clients
/// without making the delay unpredictable enough to matter (tests assert
/// attempt counts, not delays).
fn jitter(delay: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| u64::from(t.subsec_nanos()))
        .unwrap_or(0);
    let percent = 90 + nanos % 21; // 90..=110
    Duration::from_nanos(u64::try_from(delay.as_nanos()).unwrap_or(u64::MAX) * percent / 100)
}

/// Build the full `manifest.json` content: the current manifest (if any)
/// with this source's entry merged in. Foreign entries are preserved
/// verbatim (F-031). A manifest that is not a JSON object is a hard stop --
/// overwriting it could destroy the other sources' records.
fn merged_manifest(existing: Option<&str>, entry: &ManifestEntry) -> Result<String, PublishError> {
    let mut root = match existing.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => Map::new(),
        Some(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(map)) => map,
            Ok(_) => {
                return Err(PublishError::terminal(
                    PublishErrorCode::ManifestInvalid,
                    "manifest.json is not a JSON object",
                ))
            }
            Err(_) => {
                return Err(PublishError::terminal(
                    PublishErrorCode::ManifestInvalid,
                    "manifest.json is not valid JSON",
                ))
            }
        },
    };
    root.insert(entry.source_id.clone(), entry.to_json());
    Ok(Value::Object(root).to_string())
}

/// Whether the raw manifest already carries this source's entry with exactly
/// this file, sha and an active status. Anything else (missing, drifted,
/// corrupt) reads as "needs a manifest write".
fn manifest_entry_matches(existing: Option<&str>, entry: &ManifestEntry) -> bool {
    existing
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|manifest| manifest.get(&entry.source_id).cloned())
        .map(|record| {
            record.get("file").and_then(Value::as_str) == Some(entry.file.as_str())
                && record.get("sha256").and_then(Value::as_str) == Some(entry.sha256.as_str())
                && record.get("status").and_then(Value::as_str) == Some(STATUS_ACTIVE)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(source_id: &str, file: &str, sha256: &str) -> ManifestEntry {
        ManifestEntry::new(source_id, file, sha256)
    }

    /// Merging preserves the foreign entries verbatim and replaces only the
    /// own source's record (F-031).
    #[test]
    fn merging_the_manifest_preserves_foreign_entries() {
        let existing = r#"{
            "aaaaaaaa": {"source_id": "aaaaaaaa", "file": "alpha-a1.yaml", "sha256": "01", "status": "active"},
            "cccccccc": {"source_id": "cccccccc", "file": "ceta-c3.yaml", "sha256": "03", "status": "active"}
        }"#;
        let own = entry("bbbbbbbb", "bravo-b2.yaml", "02");
        let merged = merged_manifest(Some(existing), &own).unwrap();
        let manifest: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(manifest.get("aaaaaaaa").unwrap()["file"], "alpha-a1.yaml");
        assert_eq!(manifest.get("cccccccc").unwrap()["file"], "ceta-c3.yaml");
        let own_record = manifest.get("bbbbbbbb").unwrap();
        assert_eq!(own_record["sha256"], "02");
        assert_eq!(own_record["status"], "active");
        assert_eq!(own_record["file"], "bravo-b2.yaml");
        assert_eq!(own_record["revision"], crate::types::revision_of("02"));
        assert!(!own_record["published_at"].as_str().unwrap().is_empty());
    }

    /// A missing manifest starts as a fresh object; a blank one is the same.
    #[test]
    fn a_missing_manifest_starts_empty() {
        let own = entry("aaaaaaaa", "alpha-a1.yaml", "01");
        for absent in [None, Some(""), Some("   \n")] {
            let merged = merged_manifest(absent, &own).unwrap();
            let manifest: Value = serde_json::from_str(&merged).unwrap();
            let own_record = &manifest["aaaaaaaa"];
            assert_eq!(own_record["source_id"], "aaaaaaaa");
            assert_eq!(own_record["file"], "alpha-a1.yaml");
        }
    }

    /// A manifest that is not an object is a hard stop: overwriting it could
    /// destroy other sources' records.
    #[test]
    fn a_corrupt_manifest_is_refused_not_overwritten() {
        let own = entry("aaaaaaaa", "alpha-a1.yaml", "01");
        for hostile in ["[1, 2, 3]", "\"string\"", "42", "not json at all"] {
            let error = merged_manifest(Some(hostile), &own).unwrap_err();
            assert_eq!(error.code, PublishErrorCode::ManifestInvalid, "{hostile}");
            assert!(!error.retryable, "{hostile}");
        }
    }

    /// The no-change manifest check is exact: same file, same sha, active.
    #[test]
    fn manifest_entry_matching_is_exact() {
        let own = entry("aaaaaaaa", "alpha-a1.yaml", "01");
        // The manifest is the entry wrapped in a source_id-keyed object.
        let manifest = serde_json::json!({ "aaaaaaaa": own.to_json() }).to_string();
        assert!(manifest_entry_matches(Some(&manifest), &own));
        // A drifted sha or file, or a non-active status, needs a write.
        assert!(!manifest_entry_matches(
            Some(&manifest.replace("\"01\"", "\"02\"")),
            &own
        ));
        assert!(!manifest_entry_matches(
            Some(&manifest.replace("alpha-a1.yaml", "other.yaml")),
            &own
        ));
        assert!(!manifest_entry_matches(None, &own));
        assert!(!manifest_entry_matches(Some("garbage"), &own));
    }

    /// A gist id is a bare path segment; anything URL-ish is refused before
    /// a request can be built from it.
    #[test]
    fn gist_ids_are_validated_before_use() {
        assert!(validate_gist_id("abc123DEF-_").is_ok());
        for hostile in [
            "",
            "  ",
            "a/b",
            "../etc",
            "a b",
            "id\nextra",
            "a\\b",
            "id:1",
        ] {
            assert!(validate_gist_id(hostile).is_err(), "{hostile:?}");
        }
    }

    /// Redaction is defense in depth: no matter where the token leaked from,
    /// it must not survive into a message.
    #[test]
    fn redaction_removes_the_token_everywhere() {
        let text = "Bearer abcSECRETabc failed at abcSECRETabc/x";
        assert_eq!(
            redact(text, "abcSECRETabc"),
            "Bearer [redacted] failed at [redacted]/x"
        );
        assert_eq!(redact("plain", ""), "plain");
    }

    #[test]
    fn retry_after_parsing_reads_seconds_only() {
        let mut headers = HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
        headers.insert(RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(3600)));
        // HTTP-date form is ignored (the backoff covers it).
        headers.insert(
            RETRY_AFTER,
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    /// Status mapping: the mission's contract, one row per class.
    #[test]
    fn error_statuses_map_to_the_contract() {
        let build = |status: u16| {
            let error = status_error(StatusCode::from_u16(status).unwrap(), None, String::new());
            (error.code, error.retryable)
        };
        assert_eq!(build(401), (PublishErrorCode::GistAuth, false));
        assert_eq!(build(403), (PublishErrorCode::GistAuth, false));
        assert_eq!(build(404), (PublishErrorCode::GistMissing, false));
        assert_eq!(build(429), (PublishErrorCode::RateLimited, true));
        assert_eq!(build(500), (PublishErrorCode::Remote, true));
        assert_eq!(build(503), (PublishErrorCode::Remote, true));
        // 409/422 are retried once through the re-read-and-merge path, not
        // blindly by the HTTP retry loop.
        assert_eq!(build(409), (PublishErrorCode::Remote, false));
        assert_eq!(build(422), (PublishErrorCode::Remote, false));
        // Any other 4xx: GitHub answered; retrying changes nothing.
        assert_eq!(build(418), (PublishErrorCode::Remote, false));
    }

    /// The backoff curve: base * 2^(attempt-1), inside the jitter band, and
    /// a Retry-After wins until it exceeds the clamp.
    #[tokio::test]
    async fn the_backoff_curve_is_exponential_with_jitter() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "test-token").unwrap();
        let section = GistSection {
            token_file: Some(token),
            ..GistSection::default()
        };
        let publisher = GistPublisher::with_api_base(&section, "http://127.0.0.1:1")
            .unwrap()
            .with_backoff_base(Duration::from_secs(1));

        let band = |delay: Duration, base_ms: u64| {
            let low = base_ms * 9 / 10;
            let high = base_ms * 11 / 10;
            let got = delay.as_millis() as u64;
            assert!(
                (low..=high).contains(&got),
                "delay {got}ms not in {low}..={high}"
            );
        };
        let plain = PublishError::retryable(PublishErrorCode::Remote, "boom");
        band(publisher.delay_before_retry(1, &plain), 1000);
        band(publisher.delay_before_retry(2, &plain), 2000);
        band(publisher.delay_before_retry(3, &plain), 4000);

        // Retry-After wins until clamped: 3s is taken as asked, 3600s is cut
        // to the 30s prod clamp.
        let wait = PublishError::retryable(PublishErrorCode::RateLimited, "slow down");
        let wait = PublishError {
            retry_after: Some(Duration::from_secs(3)),
            ..wait
        };
        band(publisher.delay_before_retry(1, &wait), 3000);
        let capped = publisher.delay_before_retry(
            1,
            &PublishError {
                retry_after: Some(Duration::from_secs(3600)),
                ..PublishError::retryable(PublishErrorCode::RateLimited, "slow down")
            },
        );
        assert_eq!(capped, publisher.max_retry_after);
    }
}
