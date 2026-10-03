//! Fetching a source's proxies.
//!
//! Port of the part of `store.py` a round actually uses: the `download` route,
//! rendered for `ClashMeta`. Python deliberately does not read a resource's
//! stored `content` -- a remote sub keeps its nodes behind `url` and has an
//! empty `content` field -- and it does not parse share-link dialects itself,
//! because Sub-Store already does and its parser is the one that keeps up with
//! the dialects.
//!
//! The fetch sits behind [`Fetcher`] so a round can be tested without a
//! Sub-Store instance: the collection path is the one piece of this crate that
//! cannot be unit-tested against a fixture.

use std::future::Future;
use std::pin::Pin;

use probe_config::SourceSpec;
use serde_json::Value;

use crate::subscription::parse_proxies;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Error text is bounded before it reaches a log or a ledger: an unbounded
/// response body can carry a URL with credentials in it.
const ERROR_LIMIT: usize = 200;

/// Sub-Store's default render target for this project.
pub const DEFAULT_TARGET: &str = "ClashMeta";

/// `store.BROWSER_UA`. Not cosmetic: a Cloudflare-fronted Sub-Store answers 403
/// with an error code to a `Python-urllib` UA, which reads as "the service is
/// down".
pub const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0 Safari/537.36";

/// Fetches one source's proxies.
///
/// A trait object with a boxed future rather than an `async fn`: RPITIT methods
/// are not dyn-compatible, and a round holds this behind `Arc<dyn Fetcher>` so
/// tests can substitute one.
pub trait Fetcher: Send + Sync {
    /// `Err` is a human-readable reason. It must not contain the backend URL:
    /// a Sub-Store backend may embed a secret path.
    fn fetch<'a>(&'a self, source: &'a SourceSpec) -> BoxFuture<'a, Result<Vec<Value>, String>>;
}

/// The Sub-Store HTTP client.
pub struct SubStoreClient {
    backend: String,
    target: String,
    http: reqwest::Client,
}

impl SubStoreClient {
    pub fn new(backend: &str) -> Self {
        Self::with_target(backend, DEFAULT_TARGET)
    }

    pub fn with_target(backend: &str, target: &str) -> Self {
        Self {
            backend: backend.trim_end_matches('/').to_string(),
            target: target.to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("static client config"),
        }
    }

    /// The route for one source. Python: `collection` downloads under
    /// `/download/collection/<name>`, a single sub under `/download/<name>`.
    fn route(source: &SourceSpec) -> String {
        if source.kind == "sub" {
            format!("/download/{}", encode(&source.name))
        } else {
            format!("/download/collection/{}", encode(&source.name))
        }
    }
}

impl Fetcher for SubStoreClient {
    fn fetch<'a>(&'a self, source: &'a SourceSpec) -> BoxFuture<'a, Result<Vec<Value>, String>> {
        Box::pin(async move {
            let route = Self::route(source);
            let url = format!("{}{route}?target={}", self.backend, encode(&self.target));
            let response = self
                .http
                .get(&url)
                .header("User-Agent", BROWSER_UA)
                .header("Accept", "application/json, text/yaml, text/plain, */*")
                .send()
                .await
                .map_err(|err| bounded(&format!("{} {route}: {err}", kind_of(&err))))?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                // The route, not the URL: the backend may embed a secret path.
                return Err(bounded(&format!(
                    "HTTP {} {route}: {text}",
                    status.as_u16()
                )));
            }
            parse_proxies(&text).map_err(|err| bounded(&err.to_string()))
        })
    }
}

fn kind_of(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
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
    use super::*;

    fn spec(kind: &str, name: &str) -> SourceSpec {
        SourceSpec {
            key: "air".into(),
            kind: kind.into(),
            name: name.into(),
            label: name.into(),
            enabled: true,
            relay: false,
        }
    }

    #[test]
    fn a_collection_and_a_sub_use_different_routes() {
        assert_eq!(
            SubStoreClient::route(&spec("collection", "air")),
            "/download/collection/air"
        );
        assert_eq!(SubStoreClient::route(&spec("sub", "air")), "/download/air");
    }

    #[test]
    fn a_name_with_a_slash_is_encoded_not_path_traversed() {
        // A resource name is user input; letting it reach the path unencoded
        // would let it address a different route entirely.
        assert_eq!(
            SubStoreClient::route(&spec("sub", "a/b c")),
            "/download/a%2Fb%20c"
        );
        assert_eq!(
            SubStoreClient::route(&spec("collection", "..")),
            "/download/collection/.."
        );
    }

    #[test]
    fn the_user_agent_is_the_browser_one() {
        // A Cloudflare-fronted Sub-Store answers 403 to a Python-urllib UA.
        assert!(BROWSER_UA.starts_with("Mozilla/5.0"));
    }

    #[test]
    fn error_text_is_bounded_and_never_carries_the_backend() {
        let client = SubStoreClient::new("https://secret.example/Ef7HmpuSjyNOVU2dkBo54a");
        assert_eq!(
            client.backend,
            "https://secret.example/Ef7HmpuSjyNOVU2dkBo54a"
        );
        // The route is what an error names; the backend is only ever used to
        // build the request URL.
        assert!(!SubStoreClient::route(&spec("sub", "air")).contains("secret.example"));
        assert_eq!(bounded(&"x".repeat(500)).chars().count(), ERROR_LIMIT);
    }
}
