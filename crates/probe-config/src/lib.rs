//! Config loading, compatible with the Python service's `data/config.json`.
//!
//! The Python side (`mihomo_test/config.py`) is the schema owner: this module
//! reads the same file with the same defaults and the same tolerance rules --
//! an unreadable or partially-filled file must still yield a usable config,
//! because that is exactly how the Python `load()` behaves and a shadow run
//! that crashes on a file the Python service accepts would poison the
//! comparison (workstreams/13).

use std::path::{Path, PathBuf};

use probe_domain::{DomainError, DomainResult};
use serde_json::Value;

/// Defaults for the `core` section; keys mirror `config.DEFAULTS["core"]`.
///
/// `mixed_port` gained a default on the Python side in R0 (2026-09-30): the
/// key used to exist only in deployed config.json files and a fresh install
/// died on its first round with a bare KeyError. 19194 is the value the
/// deployment docs standardise on; MIHOMO_TEST_MIXED_PORT overrides.
pub const DEFAULT_API: &str = "http://127.0.0.1:19190";
pub const DEFAULT_LANES: u16 = 8;
pub const DEFAULT_BASE_PORT: u16 = 19200;
pub const DEFAULT_MIXED_PORT: u16 = 19194;
pub const DEFAULT_CONTAINER: &str = "mihomo-probe";
pub const DEFAULT_CONTAINER_CONFIG_PATH: &str = "/root/.config/mihomo/config.yaml";

/// The kernel API secret file, next to config.json (Python: `data/core.secret`).
pub const CORE_SECRET_FILE: &str = "core.secret";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreConfig {
    pub api: String,
    pub lanes: u16,
    pub base_port: u16,
    pub mixed_port: u16,
    pub container: String,
    pub container_config_path: String,
}

impl CoreConfig {
    pub fn defaults() -> Self {
        Self {
            api: DEFAULT_API.into(),
            lanes: DEFAULT_LANES,
            base_port: DEFAULT_BASE_PORT,
            mixed_port: default_mixed_port(),
            container: DEFAULT_CONTAINER.into(),
            container_config_path: DEFAULT_CONTAINER_CONFIG_PATH.into(),
        }
    }

    fn from_value(value: &Value) -> Self {
        let defaults = Self::defaults();
        let get_u16 = |key: &str, fallback: u16| -> u16 {
            value
                .get(key)
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok())
                .unwrap_or(fallback)
        };
        Self {
            api: value
                .get("api")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_API)
                .to_string(),
            lanes: get_u16("lanes", defaults.lanes),
            base_port: get_u16("base_port", defaults.base_port),
            mixed_port: get_u16("mixed_port", defaults.mixed_port),
            container: value
                .get("container")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_CONTAINER)
                .to_string(),
            container_config_path: value
                .get("container_config_path")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_CONTAINER_CONFIG_PATH)
                .to_string(),
        }
    }
}

fn default_mixed_port() -> u16 {
    std::env::var("MIHOMO_TEST_MIXED_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MIXED_PORT)
}

/// The `test` section, mirroring `config.DEFAULTS["test"]`.
///
/// Only the four fields the Rust round actually uses are modelled. The
/// remaining Python keys (`timeout_ms_retry`, `max_attempts`, `retry_pause_s`)
/// belong to the retry loop, which is not ported yet -- carrying them here
/// would be a field nothing reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestConfig {
    /// Targets in the order the docs and the panel show them. `engine.test_one`
    /// re-partitions them (an `https://` target is always tried first, whatever
    /// this order says); whether a pass must be HTTPS follows from that
    /// partition -- see [`TestConfig::https_required`].
    pub targets: Vec<String>,
    pub expected_status: String,
    pub timeout_ms: u64,
    pub concurrency: usize,
}

pub const DEFAULT_TARGETS: [&str; 3] = [
    "https://www.gstatic.com/generate_204",
    "https://cp.cloudflare.com/generate_204",
    "http://connectivitycheck.platform.hicloud.com/generate_204",
];
pub const DEFAULT_EXPECTED_STATUS: &str = "204";
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_CONCURRENCY: usize = 20;

impl TestConfig {
    pub fn defaults() -> Self {
        Self {
            targets: DEFAULT_TARGETS.iter().map(|t| (*t).to_string()).collect(),
            expected_status: DEFAULT_EXPECTED_STATUS.into(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            concurrency: DEFAULT_CONCURRENCY,
        }
    }

    fn from_value(value: &Value) -> Self {
        let defaults = Self::defaults();
        // An empty `targets` is refused rather than accepted: Python restores
        // the default list when it is empty ("test.targets 不能为空"), because
        // a round with no target tests nothing and would look like a total
        // outage.
        let targets: Vec<String> = value
            .get("targets")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|t| t.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .filter(|list: &Vec<String>| !list.is_empty())
            .unwrap_or(defaults.targets);
        Self {
            targets,
            expected_status: value
                .get("expected_status")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_EXPECTED_STATUS)
                .to_string(),
            timeout_ms: value
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_TIMEOUT_MS),
            concurrency: value
                .get("concurrency")
                .and_then(|v| v.as_u64())
                .and_then(|v| usize::try_from(v).ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_CONCURRENCY),
        }
    }

    /// The target a single-attempt test uses: the head of Python's stable
    /// partition in `engine.test_one` -- the first HTTPS entry when the list
    /// has one, otherwise the first entry.
    ///
    /// Never panics: `targets` is non-empty by construction (`from_value`
    /// falls back to the defaults), and the final fallback covers a
    /// hand-built `TestConfig`.
    pub fn preferred_target(&self) -> &str {
        self.targets
            .iter()
            .find(|t| t.starts_with("https://"))
            .or_else(|| self.targets.first())
            .map(String::as_str)
            .unwrap_or(DEFAULT_TARGETS[0])
    }

    /// Whether a pass over this target list must be HTTPS to count.
    ///
    /// Python: `https_required = urls[0].startswith("https://")` *after* the
    /// stable partition, so it is true exactly when the list contains an HTTPS
    /// target. With a list of nothing but plain-HTTP targets Python accepts an
    /// HTTP 204 as alive -- the "only HTTPS can mark a node alive" rule in the
    /// docs describes the shipped default list, not a hard requirement.
    pub fn https_required(&self) -> bool {
        self.preferred_target().starts_with("https://")
    }
}

/// The subset of the deployment config the Rust slice reads.
#[derive(Debug, Clone)]
pub struct Config {
    /// The `core` section after defaults were applied.
    pub core: CoreConfig,
    /// The `test` section after defaults were applied.
    pub test: TestConfig,
    /// The admin token. `None` means "not configured", and like the Python
    /// `auth_ok` (which denies on a falsy token) every authenticated endpoint
    /// must then deny instead of allowing.
    pub auth_token: Option<String>,
}

impl Config {
    /// Deep-merge the stored config over the defaults; invalid JSON is
    /// tolerated as "no stored config" (the Python failure shape).
    pub fn load(config_path: &Path) -> DomainResult<Self> {
        let stored = match std::fs::read_to_string(config_path) {
            Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
            Err(_) => Value::Null,
        };
        let mut cfg = default_tree();
        deep_merge(&mut cfg, &stored);
        Ok(Self {
            core: CoreConfig::from_value(cfg.get("core").unwrap_or(&Value::Null)),
            test: TestConfig::from_value(cfg.get("test").unwrap_or(&Value::Null)),
            auth_token: cfg
                .pointer("/auth/token")
                .and_then(|v| v.as_str())
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string),
        })
    }

    /// Read the kernel API secret, creating it on first use -- the Python
    /// `config.core_secret()` contract: one secret file, 0600, generated once.
    pub fn core_secret(data_dir: &Path) -> DomainResult<String> {
        let path = data_dir.join(CORE_SECRET_FILE);
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
        let secret = random_hex(16);
        std::fs::create_dir_all(data_dir).map_err(|e| {
            DomainError::Config(format!("cannot create {}: {e}", data_dir.display()))
        })?;
        std::fs::write(&path, &secret)
            .map_err(|e| DomainError::Config(format!("cannot write {}: {e}", path.display())))?;
        Ok(secret)
    }
}

pub fn default_tree() -> Value {
    serde_json::json!({
        "core": {
            "api": DEFAULT_API,
            "lanes": DEFAULT_LANES,
            "base_port": DEFAULT_BASE_PORT,
            "mixed_port": default_mixed_port(),
            "container": DEFAULT_CONTAINER,
            "container_config_path": DEFAULT_CONTAINER_CONFIG_PATH,
        },
        "test": {
            "targets": DEFAULT_TARGETS,
            "expected_status": DEFAULT_EXPECTED_STATUS,
            "timeout_ms": DEFAULT_TIMEOUT_MS,
            "concurrency": DEFAULT_CONCURRENCY,
        },
    })
}

/// Python `_deep_merge`: override wins per key, recursing into mappings.
pub fn deep_merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(base_map), Value::Object(over_map)) => {
            for (key, value) in over_map {
                match base_map.get_mut(key) {
                    Some(slot) if slot.is_object() && value.is_object() => {
                        deep_merge(slot, value);
                    }
                    _ => {
                        base_map.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, over) => *base = over.clone(),
    }
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("OS RNG is always available");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the Rust slice looks for its files. Same env var as Python, with a
/// `--root` override available to the CLI.
pub fn resolve_root(explicit: Option<&Path>) -> PathBuf {
    if let Some(root) = explicit {
        return root.to_path_buf();
    }
    if let Ok(root) = std::env::var("MIHOMO_TEST_ROOT") {
        if !root.trim().is_empty() {
            return PathBuf::from(root);
        }
    }
    PathBuf::from("/srv/mihomo-test")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_file_yields_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load(&tmp.path().join("absent.json")).unwrap();
        assert_eq!(cfg.core, CoreConfig::defaults());
        assert!(
            cfg.auth_token.is_none(),
            "absent token denies, never allows"
        );
    }

    #[test]
    fn an_unparseable_config_file_yields_defaults() {
        // Python tolerates a truncated config.json (`except: stored = {}`);
        // the Rust side must accept the same file.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{\"auth\": {\"token\": \"abc").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.core.mixed_port, DEFAULT_MIXED_PORT);
        assert!(cfg.auth_token.is_none());
    }

    #[test]
    fn stored_values_override_defaults_and_defaults_fill_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "core": {"mixed_port": 19100},
                "auth": {"token": "t".repeat(32)},
            })
            .to_string(),
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.core.mixed_port, 19100);
        assert_eq!(cfg.core.base_port, DEFAULT_BASE_PORT);
        assert_eq!(cfg.auth_token.as_deref(), Some("t".repeat(32).as_str()));
    }

    #[test]
    fn an_empty_auth_token_reads_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"auth": {"token": "   "}}"#).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.auth_token.is_none());
    }

    #[test]
    fn core_secret_is_generated_once_and_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let first = Config::core_secret(tmp.path()).unwrap();
        assert_eq!(first.len(), 32);
        let second = Config::core_secret(tmp.path()).unwrap();
        assert_eq!(first, second, "a regenerated secret would break the kernel");
    }

    #[test]
    fn deep_merge_recurses_into_maps_but_replaces_scalars() {
        let mut base = serde_json::json!({"a": {"x": 1, "y": 2}, "b": [1]});
        let over = serde_json::json!({"a": {"y": 3, "z": 4}, "b": [9]});
        deep_merge(&mut base, &over);
        assert_eq!(
            base,
            serde_json::json!({"a": {"x": 1, "y": 3, "z": 4}, "b": [9]})
        );
    }

    #[test]
    fn the_test_section_defaults_match_python() {
        let test = TestConfig::defaults();
        assert_eq!(test.expected_status, "204");
        assert_eq!(test.timeout_ms, 5_000);
        assert_eq!(test.concurrency, 20, "the deployed value");
        assert_eq!(test.targets.len(), 3);
        assert_eq!(test.preferred_target(), DEFAULT_TARGETS[0]);
    }

    #[test]
    fn an_empty_target_list_falls_back_instead_of_testing_nothing() {
        // Python restores the default list here; a round with no target would
        // otherwise report every node dead.
        let value = serde_json::json!({"targets": [], "concurrency": 0});
        let test = TestConfig::from_value(&value);
        assert_eq!(test.targets, TestConfig::defaults().targets);
        assert_eq!(
            test.concurrency, DEFAULT_CONCURRENCY,
            "0 is not a valid width"
        );
    }

    #[test]
    fn a_plain_http_only_target_list_still_yields_a_target() {
        // Python does NOT refuse this: `https_required` is False when the list
        // has no HTTPS entry, and an HTTP 204 then counts as alive. Treating
        // it as "nothing to test" would be a divergence the shadow run would
        // flag on every round.
        let value = serde_json::json!({"targets": ["http://example.com/generate_204"]});
        let test = TestConfig::from_value(&value);
        assert_eq!(test.targets.len(), 1);
        assert_eq!(test.preferred_target(), "http://example.com/generate_204");
        assert!(!test.https_required());
    }

    #[test]
    fn https_required_tracks_the_head_of_the_partition() {
        let defaults = TestConfig::defaults();
        assert_eq!(defaults.preferred_target(), DEFAULT_TARGETS[0]);
        assert!(
            defaults.https_required(),
            "the shipped list leads with HTTPS"
        );
        // A mixed list still leads with the HTTPS entry.
        let mixed = TestConfig::from_value(&serde_json::json!({
            "targets": ["http://plain.example/generate_204",
                        "https://secure.example/generate_204"]
        }));
        assert_eq!(
            mixed.preferred_target(),
            "https://secure.example/generate_204"
        );
        assert!(mixed.https_required());
    }

    #[test]
    fn stored_test_values_override_the_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "test": {"targets": ["https://example.com/generate_204"],
                         "timeout_ms": 9000, "concurrency": 4}
            })
            .to_string(),
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.test.targets, vec!["https://example.com/generate_204"]);
        assert_eq!(cfg.test.timeout_ms, 9_000);
        assert_eq!(cfg.test.concurrency, 4);
        assert_eq!(
            cfg.test.expected_status, "204",
            "untouched keys keep their default"
        );
    }
}
