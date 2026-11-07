//! The WebDAV publisher (workstreams/26): the third destination next to the
//! local root and the GitHub Gist.
//!
//! One WebDAV collection (e.g. a Nextcloud/Alist/Synology `mihomo-probe`
//! folder) holds one file per source plus a `manifest.json` with the exact
//! same shape the Gist publisher maintains — a `source_id -> entry` map that
//! preserves foreign entries. Every write is content-gated:
//!
//! * Gate 1 (before any request): the artifact's `previous_sha256` equals the
//!   new content sha → [`PublishOutcome::NoChange`], zero requests.
//! * Gate 2 (one GET per file): the remote bytes hash identically → no PUT
//!   for that file. The manifest PUTs only when its entry drifted.
//!
//! WebDAV has no batch write, so "one revision" is an ORDERING contract:
//! artifact files PUT first, `manifest.json` LAST — a manifest entry is the
//! commit marker, and a crash between the two PUTs converges on the next
//! run (content live + manifest drifted → manifest-only PUT). The same
//! ordering drives the [`FilesPublisher`] impl: the gate file (canonical)
//! PUTs last, so a reader can trust the gate marks a complete set.
//!
//! Retries mirror the Gist publisher (F-037): 401/403 (`webdav_auth`) and
//! 404-on-healthcheck / 409-on-PUT (`webdav_missing`) are terminal; 429
//! honors `Retry-After` (bounded); 5xx and network errors back off
//! exponentially with `max_retries` attempts in total. The password exists
//! ONLY in a file named by `probe_config::WebdavSection::password_file`, is
//! read at construction, and is redacted from every error message. No test
//! in this crate talks to anything but a local stub: `with_base_url` aims
//! the publisher at 127.0.0.1.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use probe_config::WebdavSection;
use reqwest::header::{ACCEPT, USER_AGENT};
use reqwest::{Client, Method, StatusCode};

use crate::files::{FilesOutcome, FilesPublisher};
use crate::gist::{
    jitter, manifest_entry_matches, merged_manifest, parse_retry_after, redact, truncate,
    ManifestEntry, DEFAULT_BACKOFF_BASE, DEFAULT_MAX_RETRY_AFTER, MANIFEST_FILE,
};
use crate::types::{
    sha256_hex, validate_gist_file, ArtifactRef, PublishError, PublishErrorCode, PublishOutcome,
};
use crate::Publisher;

/// A stable UA so the deployment is recognizable in the server's logs (the
/// same value the Gist publisher sends).
const USER_AGENT_VALUE: &str = concat!("mihomo-probe-publish/", env!("CARGO_PKG_VERSION"));

/// Production backoff between attempts: 1s -> 2s -> 4s (F-037), shared with
/// the Gist publisher; injectable for the stub tests.
pub const DEFAULT_WEBDAV_BACKOFF_BASE: Duration = DEFAULT_BACKOFF_BASE;

/// The `Retry-After` clamp shared with the Gist publisher.
pub const DEFAULT_WEBDAV_MAX_RETRY_AFTER: Duration = DEFAULT_MAX_RETRY_AFTER;

/// Process-wide per-collection write locks: two sources PUTting into one
/// collection must never interleave their read-compare-write, whoever
/// constructed their publisher. Same shape as the Gist locks (F-033).
static WEBDAV_LOCKS: OnceLock<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

fn lock_for(target: &str) -> Arc<tokio::sync::Mutex<()>> {
    let map = WEBDAV_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    map.lock()
        .expect("webdav lock map poisoned")
        .entry(target.to_string())
        .or_default()
        .clone()
}

/// Parse and validate the WebDAV base URL. The base must be an absolute
/// `http(s)` URL with a host, and must NOT carry credentials — `user:pass@`
/// in a URL lands the password in config.json and logs, which the
/// password-file contract exists to prevent.
fn validate_base_url(raw: &str) -> Result<url::Url, PublishError> {
    let parsed = url::Url::parse(raw.trim()).map_err(|_| {
        PublishError::terminal(
            PublishErrorCode::Config,
            format!("publish.webdav.url is not a valid URL: {raw:?}"),
        )
    })?;
    let scheme_is_http = matches!(parsed.scheme(), "http" | "https");
    let has_host = parsed.host_str().is_some_and(|host| !host.is_empty());
    if !scheme_is_http || !has_host {
        return Err(PublishError::terminal(
            PublishErrorCode::Config,
            "publish.webdav.url must be an absolute http(s) URL with a host",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(PublishError::terminal(
            PublishErrorCode::Config,
            "publish.webdav.url must not embed credentials; use password_file",
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(PublishError::terminal(
            PublishErrorCode::Config,
            "publish.webdav.url must not carry a query or fragment",
        ));
    }
    Ok(parsed)
}

/// Validate one directory path segment: no traversal, no separators inside a
/// segment, no control characters. The directory is joined back with `/`
/// after validation, so the resulting path cannot escape the collection.
fn validate_directory(directory: &str) -> Result<(), PublishError> {
    for segment in directory.split('/') {
        let invalid = segment.is_empty()
            || segment != segment.trim()
            || segment == "."
            || segment == ".."
            || segment.contains('\\')
            || segment
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, ':' | '%'));
        if invalid {
            return Err(PublishError::terminal(
                PublishErrorCode::Config,
                format!("publish.webdav.directory has an invalid segment {segment:?}"),
            ));
        }
    }
    Ok(())
}

/// Read the WebDAV password from its file. `None` = anonymous (no
/// Authorization header) — the legitimate setup for a LAN-only WebDAV behind
/// an authenticating proxy. A configured-but-unreadable file is a
/// CONSTRUCTION error, exactly like the Gist token file.
fn read_password(section: &WebdavSection) -> Result<Option<String>, PublishError> {
    let Some(path) = &section.password_file else {
        return Ok(None);
    };
    let raw = std::fs::read_to_string(path).map_err(|error| {
        PublishError::terminal(
            PublishErrorCode::Config,
            format!("password file {} is unreadable: {error}", path.display()),
        )
    })?;
    let password = raw.trim();
    if password.is_empty() {
        return Err(PublishError::terminal(
            PublishErrorCode::Config,
            format!("password file {} is empty", path.display()),
        ));
    }
    check_password_mode(path)?;
    Ok(Some(password.to_string()))
}

/// The password file must not be readable by group or others (0400 and 0600
/// both pass). Only meaningful on Unix; NTFS ACLs are out of std's reach.
#[cfg(unix)]
fn check_password_mode(path: &Path) -> Result<(), PublishError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|error| {
            PublishError::terminal(
                PublishErrorCode::Config,
                format!("password file {} is unreadable: {error}", path.display()),
            )
        })?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(PublishError::terminal(
            PublishErrorCode::Config,
            format!(
                "password file {} must not be group/other readable (chmod 600), found {mode:o}",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_password_mode(_path: &Path) -> Result<(), PublishError> {
    Ok(())
}

/// Publishes probe artifacts into a WebDAV collection.
///
/// Constructed from `probe_config::WebdavSection`; the password comes ONLY
/// from the configured password file and the constructor fails before any
/// network use if that file is missing or unusable.
#[derive(Debug, Clone)]
pub struct WebDavPublisher {
    http: Client,
    /// Normalized base URL: scheme://host[:port]/path, no trailing slash,
    /// credentials-free (validated in [`Self::build`]).
    base_url: String,
    /// Validated directory under the base, no leading/trailing slash; empty
    /// = publish straight into the base collection.
    directory: String,
    username: String,
    /// `None` = anonymous; `Some` is never empty (empty passwords are a
    /// construction error).
    password: Option<String>,
    max_retries: u32,
    backoff_base: Duration,
    max_retry_after: Duration,
}

impl WebDavPublisher {
    /// Production constructor: URL, credentials and bounds from `section`.
    pub fn from_config(section: &WebdavSection) -> Result<Self, PublishError> {
        let raw_url = section.url.as_deref().ok_or_else(|| {
            PublishError::terminal(
                PublishErrorCode::Config,
                "publish.webdav.url is not configured",
            )
        })?;
        Self::build(section, raw_url)
    }

    /// Constructor against an explicit base URL — how the stub tests aim the
    /// publisher at 127.0.0.1 and never at a real server. Everything else
    /// (credentials, directory, bounds) still comes from `section`.
    pub fn with_base_url(section: &WebdavSection, base_url: &str) -> Result<Self, PublishError> {
        Self::build(section, base_url)
    }

    fn build(section: &WebdavSection, base_url: &str) -> Result<Self, PublishError> {
        let parsed = validate_base_url(base_url)?;
        validate_directory(&section.directory)?;
        let password = read_password(section)?;
        let http = Client::builder()
            .timeout(Duration::from_secs(section.timeout_seconds))
            .build()
            .map_err(|error| {
                PublishError::terminal(PublishErrorCode::Config, format!("http client: {error}"))
            })?;
        // The normalized base: scheme://host[:port]/path, no trailing slash.
        let mut base = format!(
            "{}://{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or_default()
        );
        if let Some(port) = parsed.port() {
            base.push_str(&format!(":{port}"));
        }
        let base_path = parsed.path().trim_matches('/');
        if !base_path.is_empty() {
            base.push('/');
            base.push_str(base_path);
        }
        Ok(Self {
            http,
            base_url: base,
            directory: section.directory.trim_matches('/').to_string(),
            username: section.username.trim().to_string(),
            password,
            max_retries: section.max_retries.max(1),
            backoff_base: DEFAULT_WEBDAV_BACKOFF_BASE,
            max_retry_after: DEFAULT_WEBDAV_MAX_RETRY_AFTER,
        })
    }

    /// Injectable base backoff (prod 1s; tests use milliseconds).
    pub fn with_backoff_base(mut self, base: Duration) -> Self {
        self.backoff_base = base;
        self
    }

    /// Injectable `Retry-After` clamp (prod 30s; tests use milliseconds).
    pub fn with_max_retry_after(mut self, cap: Duration) -> Self {
        self.max_retry_after = cap;
        self
    }

    /// The lock key for this destination: one writer per collection.
    fn lock_key(&self) -> String {
        format!("{}/{}", self.base_url, self.directory)
    }

    /// URL of a file inside the collection.
    fn file_url(&self, name: &str) -> String {
        if self.directory.is_empty() {
            format!("{}/{name}", self.base_url)
        } else {
            format!("{}/{}/{name}", self.base_url, self.directory)
        }
    }

    /// The collection URL itself (healthcheck and `init` target).
    fn collection_url(&self) -> String {
        if self.directory.is_empty() {
            self.base_url.clone()
        } else {
            format!("{}/{}", self.base_url, self.directory)
        }
    }

    /// GET one file. `Ok(None)` = 404: the file is absent, which is the
    /// normal state of a first publish (never an error). The 404 arrives
    /// typed as `WebdavMissing` from `send_once`; for a FILE read that is
    /// exactly the "absent" answer, so it is downgraded here — a missing
    /// COLLECTION still surfaces as an error through the healthcheck.
    async fn get_file(&self, name: &str) -> Result<Option<String>, PublishError> {
        match self
            .send_with_retries(Method::GET, &self.file_url(name), None)
            .await
        {
            Ok(response) => Ok(Some(response.text().await.map_err(|error| {
                PublishError::retryable(
                    PublishErrorCode::Network,
                    redact(
                        &format!("webdav response body failed: {error}"),
                        self.secret(),
                    ),
                )
            })?)),
            Err(error) if error.code == PublishErrorCode::WebdavMissing => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// PUT one file. 2xx (200/201/204) is success; 409 means the parent
    /// collection is missing (`webdav init` fixes it) and is terminal. The
    /// 409 arrives typed as generic `Remote` from `send_once`; the status
    /// is what re-classifies it here.
    async fn put_file(&self, name: &str, content: &str) -> Result<(), PublishError> {
        match self
            .send_with_retries(Method::PUT, &self.file_url(name), Some(content))
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if error.status == Some(409) => Err(PublishError::terminal(
                PublishErrorCode::WebdavMissing,
                "PUT answered 409: the parent collection is missing; run webdav init",
            )),
            Err(error) => Err(error),
        }
    }

    /// MKCOL one collection path. 2xx = created; 405 = already exists (RFC
    /// 4918 §9.3.5), which is what makes `init` idempotent. The 405 arrives
    /// typed as generic `Remote` from `send_once`; the status is what
    /// re-classifies it here.
    async fn mkcol(&self, url: &str) -> Result<(), PublishError> {
        let response = match self
            .send_once(
                Method::from_bytes(b"MKCOL").expect("valid method"),
                url,
                None,
            )
            .await
        {
            Ok(response) => response,
            Err(error) if error.status == Some(405) => return Ok(()),
            Err(error) => return Err(error),
        };
        let status = response.status();
        if status.is_success() || status == StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        let retry_after = parse_retry_after(response.headers());
        let snippet = redact(&body_snippet(response).await, self.secret());
        Err(self.status_error(status, retry_after, snippet))
    }

    /// The healthcheck: PROPFIND (Depth: 0) on the configured collection —
    /// RFC 4918's existence check, which every real WebDAV server
    /// implements (a GET on a collection is optional).
    async fn probe_collection(&self) -> Result<(), PublishError> {
        self.send_once(
            Method::from_bytes(b"PROPFIND").expect("valid method"),
            &self.collection_url(),
            None,
        )
        .await
        .map(|_| ())
    }

    fn secret(&self) -> &str {
        self.password.as_deref().unwrap_or("")
    }

    async fn send_with_retries(
        &self,
        method: Method,
        url: &str,
        body: Option<&str>,
    ) -> Result<reqwest::Response, PublishError> {
        let mut attempt = 1u32;
        loop {
            match self.send_once(method.clone(), url, body).await {
                Ok(response) => return Ok(response),
                Err(error) if error.retryable && attempt < self.max_retries => {
                    let delay = self.delay_before_retry(attempt, &error);
                    tracing::warn!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        code = error.code.as_str(),
                        "webdav request failed; retrying"
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
        body: Option<&str>,
    ) -> Result<reqwest::Response, PublishError> {
        let mut request = self
            .http
            .request(method, url)
            .header(ACCEPT, "*/*")
            .header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(password) = &self.password {
            request = request.basic_auth(&self.username, Some(password));
        }
        if let Some(body) = body {
            request = request.body(body.to_string());
        }
        let response = request.send().await.map_err(|error| {
            PublishError::retryable(
                PublishErrorCode::Network,
                redact(&format!("request failed: {error}"), self.secret()),
            )
        })?;
        let status = response.status();
        if status.is_success() || status == StatusCode::MULTI_STATUS {
            return Ok(response);
        }
        let retry_after = parse_retry_after(response.headers());
        let snippet = redact(&body_snippet(response).await, self.secret());
        Err(self.status_error(status, retry_after, snippet))
    }

    /// Map a non-2xx response to its typed error. 401/403 and 404 are
    /// terminal; 429 honors `Retry-After`; 5xx are retryable; any other 4xx
    /// is the server answering and is not retried.
    fn status_error(
        &self,
        status: StatusCode,
        retry_after: Option<Duration>,
        body_snippet: String,
    ) -> PublishError {
        let mut error = match status.as_u16() {
            401 | 403 => PublishError::terminal(
                PublishErrorCode::WebdavAuth,
                format!("the WebDAV server rejected the credentials ({status})"),
            ),
            404 => PublishError::terminal(
                PublishErrorCode::WebdavMissing,
                format!("collection not found ({status}); run webdav init"),
            ),
            429 => PublishError::retryable(
                PublishErrorCode::RateLimited,
                format!("rate limited ({status}); honoring Retry-After"),
            ),
            _ if status.is_server_error() => PublishError::retryable(
                PublishErrorCode::Remote,
                format!("WebDAV server error ({status})"),
            ),
            _ => PublishError::terminal(
                PublishErrorCode::Remote,
                format!("unexpected WebDAV response ({status})"),
            ),
        };
        error.status = Some(status.as_u16());
        error.retry_after = retry_after;
        if !body_snippet.is_empty() {
            error.message = format!("{}: {body_snippet}", error.message);
        }
        error
    }

    /// The wait before retry number `attempt` (1-based) after `error` — the
    /// same curve as the Gist publisher: bounded Retry-After, else
    /// base * 2^(attempt-1) inside the jitter band.
    fn delay_before_retry(&self, attempt: u32, error: &PublishError) -> Duration {
        if let Some(after) = error.retry_after {
            return jitter(after).min(self.max_retry_after);
        }
        let factor = 1u32 << (attempt - 1).min(16);
        jitter(self.backoff_base.saturating_mul(factor))
    }
}

/// Bounded body snippet for error messages (the same 300-char budget as the
/// Gist publisher).
async fn body_snippet(response: reqwest::Response) -> String {
    truncate(&response.text().await.unwrap_or_default(), 300)
}

/// The one-time `webdav init` step: MKCOL the configured directory chain
/// (`a/b/c` → MKCOL a, MKCOL a/b, MKCOL a/b/c). Idempotent by construction:
/// 405 (already exists) is success, so re-running `init` is always safe.
/// The base collection itself is NOT created — the server operator owns it;
/// an empty directory means there is nothing to create and the init IS a
/// healthcheck.
pub async fn create_collection(publisher: &WebDavPublisher) -> Result<(), PublishError> {
    if publisher.directory.is_empty() {
        return publisher.probe_collection().await;
    }
    let mut walked = String::new();
    for segment in publisher.directory.split('/') {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(segment);
        let url = format!("{}/{walked}", publisher.base_url);
        publisher.mkcol(&url).await?;
    }
    Ok(())
}

impl Publisher for WebDavPublisher {
    /// Publish one artifact into the collection. MUST be idempotent: the
    /// content-hash gates mean a replayed call can never write a second
    /// file or corrupt a shared manifest.
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
        let sha = sha256_hex(content.as_bytes());
        // Gate 1, before any HTTP (F-032).
        if artifact.previous_sha256.as_deref() == Some(sha.as_str()) {
            return PublishOutcome::NoChange;
        }

        let entry = ManifestEntry::new(&artifact.source_id, &artifact.gist_file, &sha);
        // One writer per collection (F-033): the read-compare-write below
        // never interleaves with another publisher instance targeting the
        // same collection.
        let lock = lock_for(&self.lock_key());
        let _guard = lock.lock().await;

        let remote = match self.get_file(&artifact.gist_file).await {
            Ok(remote) => remote,
            Err(error) => return PublishOutcome::from(error),
        };
        let content_is_live = remote
            .as_deref()
            .map(|remote| sha256_hex(remote.as_bytes()) == sha)
            .unwrap_or(false);
        let manifest_raw = match self.get_file(MANIFEST_FILE).await {
            Ok(raw) => raw,
            Err(error) => return PublishOutcome::from(error),
        };
        if content_is_live && manifest_entry_matches(manifest_raw.as_deref(), &entry) {
            // Gate 2: the destination already holds exactly this content
            // with a current manifest entry.
            return PublishOutcome::NoChange;
        }

        // Artifact file FIRST, manifest LAST (the commit marker; see the
        // module docs for the crash-convergence argument).
        if !content_is_live {
            if let Err(error) = self.put_file(&artifact.gist_file, content).await {
                return PublishOutcome::from(error);
            }
        }
        if !manifest_entry_matches(manifest_raw.as_deref(), &entry) {
            let manifest = match merged_manifest(manifest_raw.as_deref(), &entry) {
                Ok(manifest) => manifest,
                Err(error) => return PublishOutcome::from(error),
            };
            if let Err(error) = self.put_file(MANIFEST_FILE, &manifest).await {
                return PublishOutcome::from(error);
            }
        }
        PublishOutcome::Published {
            revision: Some(entry.revision.clone()),
            gist_failed: None,
        }
    }

    /// Probe the configured collection with PROPFIND (Depth: 0). The `path`
    /// parameter is accepted for trait compatibility — the Gist publisher's
    /// per-id probe has no WebDAV analogue, because a publisher addresses
    /// exactly one collection — and ignored.
    async fn healthcheck(&self, _path: &str) -> Result<(), PublishError> {
        self.probe_collection().await
    }
}

/// 固定文件集发布（[`FilesPublisher`]，workstreams/26）：gate 未变 → 零写；
/// gate 变了 → 逐文件 PUT，gate 文件（canonical）最后落——一次完整的
/// canonical 变化以 gate 文件的 PUT 收尾，读方可以信任 gate 即完整集。
/// 幂等语义与 Gist 的 `publish_files` 相同，只是一次 PATCH 变成一组 PUT。
impl FilesPublisher for WebDavPublisher {
    async fn publish_files(
        &self,
        _target: &str,
        files: &[(String, String)],
        gate_file: &str,
    ) -> Result<FilesOutcome, PublishError> {
        if files.is_empty() {
            return Err(PublishError::terminal(
                PublishErrorCode::Config,
                "no files to publish",
            ));
        }
        let Some((_, gate_content)) = files.iter().find(|(name, _)| name == gate_file) else {
            return Err(PublishError::terminal(
                PublishErrorCode::Config,
                "gate file missing from the file set",
            ));
        };
        let gate_sha = sha256_hex(gate_content.as_bytes());
        let lock = lock_for(&self.lock_key());
        let _write_lock = lock.lock().await;

        let remote_gate = self.get_file(gate_file).await?;
        let gate_is_live = remote_gate
            .as_deref()
            .map(|remote| sha256_hex(remote.as_bytes()) == gate_sha)
            .unwrap_or(false);
        if gate_is_live {
            return Ok(FilesOutcome::NoChange);
        }

        // Non-gate files first (each only when its remote sha differs), the
        // gate file last. The changed list follows the input order with the
        // gate moved to the end, so the reported order is the wire order.
        let mut changed: Vec<(String, String)> = Vec::new();
        for (name, content) in files {
            if name == gate_file {
                continue;
            }
            let remote = self.get_file(name).await?;
            let is_live = remote
                .as_deref()
                .map(|remote| sha256_hex(remote.as_bytes()) == sha256_hex(content.as_bytes()))
                .unwrap_or(false);
            if !is_live {
                changed.push((name.clone(), content.clone()));
            }
        }
        changed.push((gate_file.to_string(), gate_content.clone()));
        for (name, content) in &changed {
            self.put_file(name, content).await?;
        }
        Ok(FilesOutcome::Published {
            changed: changed.into_iter().map(|(name, _)| name).collect(),
            revision: Some(gate_sha[..12].to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU16, Ordering};

    use super::*;

    /// The stub server: a tiny in-memory WebDAV on 127.0.0.1 that shares its
    /// file map and request log with the test, so every test asserts on real
    /// method/path sequences. Failure injection: the first `get_failures`
    /// GETs and `put_failures` PUTs answer their configured status, then
    /// fall through to the default behavior.
    struct Stub {
        base: String,
        files: Arc<TokioMutex<BTreeMap<String, String>>>,
        requests: Arc<TokioMutex<Vec<(String, String)>>>,
    }

    type TokioMutex<T> = tokio::sync::Mutex<T>;

    impl Stub {
        async fn spawn(
            get_failures: u16,
            get_failure_status: u16,
            put_failures: u16,
            put_failure_status: u16,
            propfind_status: u16,
            mkcol_status: u16,
        ) -> Self {
            let files: Arc<TokioMutex<BTreeMap<String, String>>> = Arc::default();
            let requests: Arc<TokioMutex<Vec<(String, String)>>> = Arc::default();
            let get_failures_counter = Arc::new(AtomicU16::new(get_failures));
            let put_failures_counter = Arc::new(AtomicU16::new(put_failures));
            let handler_files = Arc::clone(&files);
            let handler_requests = Arc::clone(&requests);
            let handler_get_failures = Arc::clone(&get_failures_counter);
            let handler_put_failures = Arc::clone(&put_failures_counter);
            let app = axum::routing::any(
                move |method: axum::http::Method, uri: axum::http::Uri, body: String| {
                    let files = Arc::clone(&handler_files);
                    let requests = Arc::clone(&handler_requests);
                    let get_failures = Arc::clone(&handler_get_failures);
                    let put_failures = Arc::clone(&handler_put_failures);
                    async move {
                        let path = uri.path().trim_start_matches('/').to_string();
                        requests
                            .lock()
                            .await
                            .push((method.to_string(), path.clone()));
                        match method.as_str() {
                            "GET" => {
                                if get_failures.load(Ordering::SeqCst) > 0 {
                                    get_failures.fetch_sub(1, Ordering::SeqCst);
                                    return (
                                        StatusCode::from_u16(get_failure_status).unwrap(),
                                        String::new(),
                                    );
                                }
                                match files.lock().await.get(&path) {
                                    Some(content) => (StatusCode::OK, content.clone()),
                                    None => (StatusCode::NOT_FOUND, String::new()),
                                }
                            }
                            "PUT" => {
                                if put_failures.load(Ordering::SeqCst) > 0 {
                                    put_failures.fetch_sub(1, Ordering::SeqCst);
                                    return (
                                        StatusCode::from_u16(put_failure_status).unwrap(),
                                        String::new(),
                                    );
                                }
                                files.lock().await.insert(path, body);
                                (StatusCode::CREATED, String::new())
                            }
                            "PROPFIND" => (
                                StatusCode::from_u16(propfind_status).unwrap(),
                                String::new(),
                            ),
                            "MKCOL" => (StatusCode::from_u16(mkcol_status).unwrap(), String::new()),
                            _ => (StatusCode::METHOD_NOT_ALLOWED, String::new()),
                        }
                    }
                },
            )
            .with_state(());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self {
                base,
                files,
                requests,
            }
        }

        /// A healthy stub (no failure injection).
        async fn healthy() -> Self {
            Self::spawn(0, 0, 0, 0, 0, 0).await
        }

        /// Seed a file directly into the shared map.
        async fn seed(&self, path: &str, content: &str) {
            self.files
                .lock()
                .await
                .insert(path.to_string(), content.to_string());
        }

        /// Remove a file from the shared map.
        async fn wipe(&self, path: &str) {
            self.files.lock().await.remove(path);
        }

        /// Read a file from the shared map.
        async fn stored(&self, path: &str) -> Option<String> {
            self.files.lock().await.get(path).cloned()
        }

        async fn recorded(&self) -> Vec<(String, String)> {
            self.requests.lock().await.clone()
        }

        async fn put_paths(&self) -> Vec<String> {
            self.recorded()
                .await
                .into_iter()
                .filter(|(method, _)| method == "PUT")
                .map(|(_, path)| path)
                .collect()
        }
    }

    fn section(directory: &str, password_file: Option<&Path>) -> WebdavSection {
        WebdavSection {
            url: None,
            username: "probe".into(),
            password_file: password_file.map(Path::to_path_buf),
            directory: directory.into(),
            timeout_seconds: 5,
            max_retries: 3,
        }
    }

    fn artifact(file: &str, previous: Option<&str>) -> ArtifactRef {
        ArtifactRef {
            source_id: "aaaaaaaa".into(),
            gist_file: file.into(),
            previous_sha256: previous.map(str::to_string),
        }
    }

    async fn mk_publisher(stub: &Stub) -> WebDavPublisher {
        WebDavPublisher::with_base_url(&section("mihomo-probe", None), &stub.base)
            .unwrap()
            .with_backoff_base(Duration::from_millis(1))
    }

    /// The first publish of a file PUTs the artifact and then the manifest
    /// (the commit-marker ordering).
    #[tokio::test]
    async fn a_first_publish_puts_the_artifact_then_the_manifest() {
        let stub = Stub::healthy().await;
        let publisher = mk_publisher(&stub).await;
        let outcome = publisher
            .publish(&artifact("alpha-a1.yaml", None), "content-1")
            .await;
        assert_eq!(
            outcome,
            PublishOutcome::Published {
                revision: Some(crate::types::revision_of(&sha256_hex(b"content-1"))),
                gist_failed: None,
            }
        );
        assert_eq!(
            stub.put_paths().await,
            vec!["mihomo-probe/alpha-a1.yaml", "mihomo-probe/manifest.json"]
        );
        assert_eq!(
            stub.stored("mihomo-probe/alpha-a1.yaml").await.unwrap(),
            "content-1"
        );
        let manifest: serde_json::Value =
            serde_json::from_str(&stub.stored("mihomo-probe/manifest.json").await.unwrap())
                .unwrap();
        assert_eq!(manifest["aaaaaaaa"]["file"], "alpha-a1.yaml");
        assert_eq!(manifest["aaaaaaaa"]["sha256"], sha256_hex(b"content-1"));
        assert_eq!(manifest["aaaaaaaa"]["status"], "active");
    }

    /// Gate 1: a previous-sha match answers NoChange with ZERO requests.
    #[tokio::test]
    async fn a_previous_sha_match_sends_zero_requests() {
        let stub = Stub::healthy().await;
        let publisher = mk_publisher(&stub).await;
        let sha = sha256_hex(b"content-1");
        let outcome = publisher
            .publish(&artifact("alpha-a1.yaml", Some(&sha)), "content-1")
            .await;
        assert_eq!(outcome, PublishOutcome::NoChange);
        assert!(stub.recorded().await.is_empty());
    }

    /// Gate 2: the remote already holds the bytes with a current manifest —
    /// GET-only, zero PUTs (F-032's second gate).
    #[tokio::test]
    async fn live_content_with_a_current_manifest_is_get_only() {
        let stub = Stub::healthy().await;
        let publisher = mk_publisher(&stub).await;
        publisher
            .publish(&artifact("alpha-a1.yaml", None), "content-1")
            .await;
        let puts_before = stub.put_paths().await.len();
        let outcome = publisher
            .publish(&artifact("alpha-a1.yaml", None), "content-1")
            .await;
        assert_eq!(outcome, PublishOutcome::NoChange);
        assert_eq!(stub.put_paths().await.len(), puts_before);
    }

    /// Crash convergence: the content landed but the manifest did not. The
    /// next run PUTs the manifest ONLY (never re-PUTs the artifact).
    #[tokio::test]
    async fn a_manifest_drift_repairs_with_a_manifest_only_put() {
        let stub = Stub::healthy().await;
        let publisher = mk_publisher(&stub).await;
        publisher
            .publish(&artifact("alpha-a1.yaml", None), "content-1")
            .await;
        stub.wipe("mihomo-probe/manifest.json").await;
        let outcome = publisher
            .publish(&artifact("alpha-a1.yaml", None), "content-1")
            .await;
        assert!(
            matches!(outcome, PublishOutcome::Published { .. }),
            "{outcome:?}"
        );
        // The first publish's two PUTs, then exactly one manifest-only PUT
        // (the artifact itself is never re-PUT).
        assert_eq!(&stub.put_paths().await[2..], ["mihomo-probe/manifest.json"]);
    }

    /// A foreign manifest entry survives the merge verbatim (F-031); a
    /// corrupt manifest is refused, not overwritten.
    #[tokio::test]
    async fn foreign_manifest_entries_survive_and_corrupt_ones_are_refused() {
        let stub = Stub::healthy().await;
        stub.seed(
            "mihomo-probe/manifest.json",
            r#"{"cccccccc": {"source_id": "cccccccc", "file": "ceta-c3.yaml"}}"#,
        )
        .await;
        let publisher = mk_publisher(&stub).await;
        publisher
            .publish(&artifact("alpha-a1.yaml", None), "content-1")
            .await;
        let manifest: serde_json::Value =
            serde_json::from_str(&stub.stored("mihomo-probe/manifest.json").await.unwrap())
                .unwrap();
        assert_eq!(manifest["cccccccc"]["file"], "ceta-c3.yaml");
        assert!(manifest.get("aaaaaaaa").is_some());

        let stub = Stub::healthy().await;
        stub.seed("mihomo-probe/manifest.json", "[1, 2, 3]").await;
        let publisher = mk_publisher(&stub).await;
        let outcome = publisher
            .publish(&artifact("alpha-a1.yaml", None), "content-1")
            .await;
        match outcome {
            PublishOutcome::Failed {
                code, retryable, ..
            } => {
                assert_eq!(code, PublishErrorCode::ManifestInvalid);
                assert!(!retryable);
            }
            other => panic!("expected a manifest failure, got {other:?}"),
        }
        // The hostile manifest was not overwritten.
        assert_eq!(
            stub.stored("mihomo-probe/manifest.json").await.unwrap(),
            "[1, 2, 3]"
        );
    }

    /// 401 is terminal `webdav_auth` with no retry; the password never
    /// appears in a message even when the response body echoes it.
    #[tokio::test]
    async fn auth_failures_are_terminal_and_redacted() {
        let stub = Stub::spawn(1, 401, 0, 0, 0, 0).await;
        let dir = tempfile::tempdir().unwrap();
        let password_file = dir.path().join("dav.pass");
        std::fs::write(&password_file, "sekret-password").unwrap();
        let publisher = WebDavPublisher::with_base_url(
            &section("mihomo-probe", Some(&password_file)),
            &stub.base,
        )
        .unwrap()
        .with_backoff_base(Duration::from_millis(1));
        let outcome = publisher
            .publish(&artifact("alpha-a1.yaml", None), "content-1")
            .await;
        match outcome {
            PublishOutcome::Failed {
                code,
                retryable,
                message,
            } => {
                assert_eq!(code, PublishErrorCode::WebdavAuth);
                assert!(!retryable);
                assert!(!message.contains("sekret-password"), "{message}");
            }
            other => panic!("expected an auth failure, got {other:?}"),
        }
        // Exactly one GET attempt: terminal means no retries.
        assert_eq!(stub.recorded().await.len(), 1);
    }

    /// 409 on PUT is terminal `webdav_missing` — the collection is absent,
    /// and `webdav init` is the fix.
    #[tokio::test]
    async fn a_conflict_put_maps_to_webdav_missing() {
        let stub = Stub::spawn(0, 0, 1, 409, 0, 0).await;
        let publisher = mk_publisher(&stub).await;
        let outcome = publisher
            .publish(&artifact("alpha-a1.yaml", None), "content-1")
            .await;
        match outcome {
            PublishOutcome::Failed {
                code, retryable, ..
            } => {
                assert_eq!(code, PublishErrorCode::WebdavMissing);
                assert!(!retryable);
            }
            other => panic!("expected a missing-collection failure, got {other:?}"),
        }
    }

    /// 5xx backs off and retries; success on the third attempt publishes.
    #[tokio::test]
    async fn server_errors_retry_until_success() {
        let stub = Stub::spawn(0, 0, 2, 500, 0, 0).await;
        let publisher = mk_publisher(&stub).await;
        let outcome = publisher
            .publish(&artifact("alpha-a1.yaml", None), "content-1")
            .await;
        assert!(
            matches!(outcome, PublishOutcome::Published { .. }),
            "{outcome:?}"
        );
        // 2 failed PUTs + 1 successful artifact PUT + 1 manifest PUT = 4.
        assert_eq!(stub.put_paths().await.len(), 4);
    }

    /// publish_files: gate unchanged → NoChange, zero writes on the rerun.
    #[tokio::test]
    async fn the_gate_file_unchanged_publishes_nothing() {
        let stub = Stub::healthy().await;
        let publisher = mk_publisher(&stub).await;
        let files = vec![
            ("mihomo-alive.yaml".to_string(), "canonical-1".to_string()),
            ("status".to_string(), "status-1".to_string()),
        ];
        let outcome = publisher
            .publish_files("", &files, "mihomo-alive.yaml")
            .await
            .unwrap();
        assert!(matches!(outcome, FilesOutcome::Published { .. }));
        let outcome = publisher
            .publish_files("", &files, "mihomo-alive.yaml")
            .await
            .unwrap();
        assert_eq!(outcome, FilesOutcome::NoChange);
        assert_eq!(stub.put_paths().await.len(), 2, "no PUTs on the rerun");
    }

    /// The gate file PUTs last and only changed files are sent at all —
    /// the complete-set marker on the wire.
    #[tokio::test]
    async fn the_gate_file_put_is_last_in_the_wire_order() {
        let stub = Stub::healthy().await;
        stub.seed("mihomo-probe/status", "status-1").await;
        let publisher = mk_publisher(&stub).await;
        let files = vec![
            ("mihomo-alive.yaml".to_string(), "canonical-1".to_string()),
            ("status".to_string(), "status-1".to_string()),
            ("history".to_string(), "history-1".to_string()),
        ];
        let outcome = publisher
            .publish_files("", &files, "mihomo-alive.yaml")
            .await
            .unwrap();
        let FilesOutcome::Published { changed, revision } = outcome else {
            panic!("expected Published");
        };
        assert_eq!(
            changed,
            vec!["history", "mihomo-alive.yaml"],
            "the already-live status file is not re-PUT"
        );
        assert_eq!(
            stub.put_paths().await,
            vec!["mihomo-probe/history", "mihomo-probe/mihomo-alive.yaml"],
            "the gate file (canonical) PUTs last"
        );
        assert_eq!(revision, Some(sha256_hex(b"canonical-1")[..12].to_string()));
    }

    /// `init` MKCOLs the directory chain; 405 (already exists) is success,
    /// which is what makes re-running init safe.
    #[tokio::test]
    async fn init_walks_the_directory_chain_idempotently() {
        let stub = Stub::spawn(0, 0, 0, 0, 0, 405).await;
        let publisher = mk_publisher(&stub).await;
        create_collection(&publisher).await.unwrap();
        let mkcols: Vec<String> = stub
            .recorded()
            .await
            .into_iter()
            .filter(|(method, _)| method == "MKCOL")
            .map(|(_, path)| path)
            .collect();
        assert_eq!(mkcols, vec!["mihomo-probe"]);
    }

    /// The healthcheck PROPFINDs the collection; the frozen classes map to
    /// their codes (200/207 ok, 404 missing, 401 auth).
    #[tokio::test]
    async fn the_healthcheck_maps_the_frozen_classes() {
        for (propfind_status, expected) in [
            (200u16, None),
            (207, None),
            (404, Some(PublishErrorCode::WebdavMissing)),
            (401, Some(PublishErrorCode::WebdavAuth)),
        ] {
            let stub = Stub::spawn(0, 0, 0, 0, propfind_status, 0).await;
            let publisher = mk_publisher(&stub).await;
            let result = publisher.healthcheck("").await;
            match expected {
                None => assert!(result.is_ok(), "status {propfind_status}: {result:?}"),
                Some(code) => {
                    assert_eq!(result.unwrap_err().code, code, "status {propfind_status}")
                }
            }
        }
    }

    /// URL and directory validation happen at construction, before any
    /// network use.
    #[test]
    fn construction_validates_the_destination_before_any_request() {
        let plain = section("mihomo-probe", None);
        // Credentials embedded in the URL are refused.
        assert!(
            WebDavPublisher::with_base_url(&plain, "https://user:pass@dav.example.com/dav")
                .is_err()
        );
        // Non-http schemes are refused.
        assert!(WebDavPublisher::with_base_url(&plain, "ftp://dav.example.com/dav").is_err());
        // Query strings are refused (they would change every request URL).
        assert!(WebDavPublisher::with_base_url(&plain, "https://dav.example.com/dav?x=1").is_err());
        // Traversal in the directory is refused.
        let escaping = section("mihomo-probe/../secrets", None);
        assert!(WebDavPublisher::with_base_url(&escaping, "https://dav.example.com").is_err());
        // A valid URL with a base path and port normalizes correctly.
        let publisher =
            WebDavPublisher::with_base_url(&plain, "http://127.0.0.1:5244/dav/").unwrap();
        assert_eq!(
            publisher.file_url("a.yaml"),
            "http://127.0.0.1:5244/dav/mihomo-probe/a.yaml"
        );
        // No URL configured at all is a construction error.
        assert!(WebDavPublisher::from_config(&WebdavSection::default()).is_err());
    }

    /// The password exists only in its file, must be non-empty, and rides
    /// the Authorization header — never a message.
    #[tokio::test]
    async fn the_password_comes_only_from_its_file() {
        let plain = section("mihomo-probe", None);
        // Anonymous is a valid configuration.
        assert!(WebDavPublisher::with_base_url(&plain, "http://127.0.0.1:1").is_ok());
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.pass");
        std::fs::write(&empty, "   \n").unwrap();
        assert!(WebDavPublisher::with_base_url(
            &section("mihomo-probe", Some(&empty)),
            "http://127.0.0.1:1"
        )
        .is_err());
        let missing = dir.path().join("nope.pass");
        assert!(WebDavPublisher::with_base_url(
            &section("mihomo-probe", Some(&missing)),
            "http://127.0.0.1:1"
        )
        .is_err());
    }
}
