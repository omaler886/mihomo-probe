//! Publish destinations for probe artifacts (workstreams/18 B2).
//!
//! One [`Publisher`] per destination, one contract everywhere:
//!
//! * **Idempotence** (F-032): a publish whose content hash equals the
//!   artifact's `last_content_sha256` is [`PublishOutcome::NoChange`] with
//!   ZERO writes/requests; the destination's live content is a second gate.
//! * **Stable names** (F-030): the artifact carries its fixed
//!   `<slug>-<id8>.yaml` name; nothing here ever derives a name from a run.
//! * **Manifest** (F-031): the Gist publisher maintains `manifest.json` as a
//!   `source_id -> entry` map, preserving foreign entries.
//! * **Serialization** (F-033): all writes to one Gist hold a process-wide
//!   per-gist lock across their read-merge-write.
//! * **Errors** (F-037): 401/403 (`gist_auth`) and 404 (`gist_missing`) are
//!   terminal; 429 honors `Retry-After` (bounded); 5xx/network back off
//!   exponentially (1s -> 2s -> 4s, injectable) with `max_retries` attempts
//!   in total, then a retryable failure; 409/422 get one re-read-and-merge
//!   retry. Every message is redacted against the token.
//!
//! The token exists ONLY in a 0600 token file named by
//! `probe_config::GistSection::token_file`; it is read at construction, so a
//! broken setup fails before any request. No test in this crate ever talks
//! to GitHub: `GistPublisher::with_api_base` aims everything at a local
//! axum stub.
//!
//! Wiring into the engine, the CLI `gist init|test` commands and the API
//! test endpoint is the follow-up task (A2); this crate is the library
//! layer plus its contract tests.

pub mod composite;
pub mod gist;
pub mod local;
pub mod types;

use std::future::Future;

pub use composite::CompositePublisher;
pub use gist::{
    GistPublisher, DEFAULT_BACKOFF_BASE, DEFAULT_MAX_RETRY_AFTER, GITHUB_API_BASE, MANIFEST_FILE,
};
pub use local::LocalPublisher;
pub use types::{
    revision_of, sha256_hex, ArtifactRef, PublishError, PublishErrorCode, PublishOutcome,
};

/// One publish destination.
///
/// The futures are explicitly `Send` (RPITIT instead of the `async-trait`
/// box) so the engine can publish every source inside a `tokio::spawn`;
/// implementations are written as plain `async fn`.
pub trait Publisher: Send + Sync {
    /// Publish `content` as `artifact`. MUST be idempotent: identical
    /// content is a no-op against the destination, so a replayed round or a
    /// retried call can never create a second file or a second Gist.
    fn publish(
        &self,
        artifact: &ArtifactRef,
        content: &str,
    ) -> impl Future<Output = PublishOutcome> + Send;

    /// Probe the destination. `gist_id` selects the Gist (empty = the
    /// configured one); the local publisher ignores it and checks its root.
    fn healthcheck(&self, gist_id: &str) -> impl Future<Output = Result<(), PublishError>> + Send;
}
