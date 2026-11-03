//! Outcome and error vocabulary shared by every publisher.
//!
//! The engine (workstreams/18 A2) records `PublishErrorCode::as_str()` in
//! `source_runs.error_code` and the sanitized `Failed` message in its
//! `publish_status`; nothing here carries credential material.

use std::time::Duration;

use sha2::{Digest, Sha256};

/// Identifies the artifact a publish call targets. Deliberately detached from
/// `probe_storage::ArtifactRow`: the engine maps the row onto this (it owns
/// the database side), and the publishers stay testable without SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    /// Ledger identity of the source (`sources.source_id`). It is the manifest
    /// key in a Gist, so a source keeps its entry across renames.
    pub source_id: String,
    /// Stable destination file name (`<slug>-<id8>.yaml`, F-030). For the
    /// Gist publisher this is the key inside the gist's `files` map, for the
    /// local publisher the file name under the export root.
    pub gist_file: String,
    /// SHA-256 (lowercase hex) of the content the artifact row last published
    /// successfully (`artifacts.last_content_sha256`). `None` = never
    /// published, or the row predates the column: the publisher then compares
    /// against the destination's current content instead (gate 2).
    pub previous_sha256: Option<String>,
}

impl ArtifactRef {
    /// A reference for `source_id` publishing `gist_file`, never published
    /// before (the caller fills `previous_sha256` from the artifact row).
    pub fn new(source_id: impl Into<String>, gist_file: impl Into<String>) -> Self {
        Self {
            source_id: source_id.into(),
            gist_file: gist_file.into(),
            previous_sha256: None,
        }
    }
}

/// Lowercase-hex SHA-256 of `content` -- the artifact identity everywhere in
/// this crate (no-change gate, manifest entry, revision).
pub fn sha256_hex(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The human-facing revision of a content hash: its first 12 hex chars. This
/// (not GitHub's `updated_at`) is what the manifest entry and the
/// `Published` outcome carry, so a revision always names the content it was
/// derived from and is known before the PATCH round-trips.
pub fn revision_of(sha256: &str) -> String {
    sha256.chars().take(12).collect()
}

/// Terminal classification of a publish failure. Stored as a short string in
/// `source_runs.error_code`, so the spellings are frozen API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishErrorCode {
    /// GitHub rejected the credentials (401/403). Never retried; the token
    /// must be rotated before anything can succeed.
    GistAuth,
    /// The Gist does not exist, or the token cannot see it (404). Never
    /// retried; `gist init` must run first.
    GistMissing,
    /// Rate limited (429) and the bounded Retry-After budget ran out.
    RateLimited,
    /// GitHub answered 5xx (or an unexpected 4xx) past the retry budget.
    Remote,
    /// The request never completed -- connect failure, timeout, or a body
    /// that was not the JSON we asked for -- past the retry budget.
    Network,
    /// The gist's `manifest.json` is not a JSON object. A publish refuses
    /// rather than overwrite it: rebuilding would destroy the other sources'
    /// entries sharing that gist.
    ManifestInvalid,
    /// Configuration is incomplete or malformed (no gist id, bad file name,
    /// unusable token file). Discovered before any request is sent.
    Config,
    /// Local filesystem failure while writing the artifact.
    Io,
}

impl PublishErrorCode {
    /// The canonical spelling for the `source_runs.error_code` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GistAuth => "gist_auth",
            Self::GistMissing => "gist_missing",
            Self::RateLimited => "rate_limited",
            Self::Remote => "remote",
            Self::Network => "network",
            Self::ManifestInvalid => "manifest_invalid",
            Self::Config => "config",
            Self::Io => "io",
        }
    }
}

/// Why a publish failed. The message is safe to log and store: every text
/// that passed near a response body or client error is redacted against the
/// publisher's token before it lands here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct PublishError {
    pub code: PublishErrorCode,
    /// Whether a *later* attempt could plausibly succeed (rate limit, 5xx,
    /// network). Auth/missing/config/manifest failures are terminal: the
    /// caller must not blindly re-run the round.
    pub retryable: bool,
    pub message: String,
    /// The HTTP status, when the failure came from a response. This is how a
    /// caller distinguishes a 409/422 merge conflict from other remotes.
    pub status: Option<u16>,
    /// `Retry-After` from a 429, when the server sent one that parsed.
    pub retry_after: Option<Duration>,
}

impl PublishError {
    /// A failure that no retry can fix.
    pub fn terminal(code: PublishErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            retryable: false,
            message: message.into(),
            status: None,
            retry_after: None,
        }
    }

    /// A failure a later attempt may still fix.
    pub fn retryable(code: PublishErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            retryable: true,
            message: message.into(),
            status: None,
            retry_after: None,
        }
    }
}

/// `<code>: <message>` -- the shape every log line and run record uses.
impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

/// What one `publish` call decided.
///
/// Composite semantics (`CompositePublisher`): the local leg is
/// authoritative, so its failure fails the whole publish, while a Gist
/// failure is *reported*, not fatal -- it lands in
/// `Published { gist_failed: Some(...) }`. `NoChange` requires both legs to
/// agree there is nothing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The content hash equals the previously published hash (or the
    /// destination already holds exactly this content with a current
    /// manifest): nothing was sent, nothing changed.
    NoChange,
    Published {
        /// Revision (first 12 hex of the content sha256) now live in the
        /// destination. `None` only in the composite case where the local leg
        /// found no change and the Gist leg failed -- there is no new
        /// revision to name.
        revision: Option<String>,
        /// Composite publisher only: the Gist leg failed with this sanitized
        /// reason while the local leg succeeded. The local artifact is
        /// intact; the caller records the reason against the run. Always
        /// `None` for the single providers.
        gist_failed: Option<String>,
    },
    Failed {
        code: PublishErrorCode,
        /// See [`PublishError::retryable`].
        retryable: bool,
        /// Sanitized (token-redacted) human-readable reason.
        message: String,
    },
}

impl From<PublishError> for PublishOutcome {
    fn from(error: PublishError) -> Self {
        PublishOutcome::Failed {
            code: error.code,
            retryable: error.retryable,
            message: error.message,
        }
    }
}

impl PublishOutcome {
    /// `true` when nothing needed to change anywhere.
    pub fn is_no_change(&self) -> bool {
        matches!(self, Self::NoChange)
    }

    /// The revision published by this call, when there is one.
    pub fn revision(&self) -> Option<&str> {
        match self {
            Self::Published { revision, .. } => revision.as_deref(),
            _ => None,
        }
    }
}

/// An artifact file name must be a bare name. The GitHub `files` map has no
/// directories, the local root must not be escapable, and Windows refuses
/// `:` in paths -- so one rule serves both publishers.
pub(crate) fn validate_gist_file(name: &str) -> Result<(), PublishError> {
    let invalid = name.is_empty()
        || name.len() > 255
        || name != name.trim()
        || name
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '/' | '\\' | ':'))
        || name == "."
        || name == "..";
    if invalid {
        return Err(PublishError::terminal(
            PublishErrorCode::Config,
            format!("invalid artifact file name {name:?}"),
        ));
    }
    Ok(())
}
