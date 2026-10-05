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
#[derive(Debug, Clone, PartialEq)]
pub struct TestConfig {
    /// Targets in the order the docs and the panel show them. `engine.test_one`
    /// re-partitions them (an `https://` target is always tried first, whatever
    /// this order says); whether a pass must be HTTPS follows from that
    /// partition -- see [`TestConfig::https_required`].
    pub targets: Vec<String>,
    pub expected_status: String,
    pub timeout_ms: u64,
    /// The budget a *retry* after a timeout gets. A timeout is the one failure
    /// a bigger budget can overturn, so it is the only one that escalates.
    pub timeout_ms_retry: u64,
    pub concurrency: usize,
    pub max_attempts: usize,
    pub retry_pause_s: f64,
}

pub const DEFAULT_TARGETS: [&str; 3] = [
    "https://www.gstatic.com/generate_204",
    "https://cp.cloudflare.com/generate_204",
    "http://connectivitycheck.platform.hicloud.com/generate_204",
];
pub const DEFAULT_EXPECTED_STATUS: &str = "204";
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_TIMEOUT_MS_RETRY: u64 = 9_000;
pub const DEFAULT_CONCURRENCY: usize = 20;
pub const DEFAULT_MAX_ATTEMPTS: usize = 3;
pub const DEFAULT_RETRY_PAUSE_S: f64 = 0.3;

impl TestConfig {
    pub fn defaults() -> Self {
        Self {
            targets: DEFAULT_TARGETS.iter().map(|t| (*t).to_string()).collect(),
            expected_status: DEFAULT_EXPECTED_STATUS.into(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            timeout_ms_retry: DEFAULT_TIMEOUT_MS_RETRY,
            concurrency: DEFAULT_CONCURRENCY,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            retry_pause_s: DEFAULT_RETRY_PAUSE_S,
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
        let positive_u64 = |key: &str, fallback: u64| -> u64 {
            value
                .get(key)
                .and_then(|v| v.as_u64())
                .filter(|v| *v > 0)
                .unwrap_or(fallback)
        };
        Self {
            targets,
            expected_status: value
                .get("expected_status")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_EXPECTED_STATUS)
                .to_string(),
            timeout_ms: positive_u64("timeout_ms", DEFAULT_TIMEOUT_MS),
            timeout_ms_retry: positive_u64("timeout_ms_retry", DEFAULT_TIMEOUT_MS_RETRY),
            concurrency: value
                .get("concurrency")
                .and_then(|v| v.as_u64())
                .and_then(|v| usize::try_from(v).ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_CONCURRENCY),
            // Python `max(1, int(...))`: a zero or negative attempt budget
            // would make every node look dead without dialling anything.
            max_attempts: value
                .get("max_attempts")
                .and_then(|v| v.as_i64())
                .map(|v| usize::try_from(v.max(1)).unwrap_or(DEFAULT_MAX_ATTEMPTS))
                .unwrap_or(DEFAULT_MAX_ATTEMPTS),
            retry_pause_s: value
                .get("retry_pause_s")
                .and_then(|v| v.as_f64())
                .filter(|v| *v >= 0.0)
                .unwrap_or(DEFAULT_RETRY_PAUSE_S),
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

/// The two resource kinds Sub-Store exposes.
pub const SOURCE_KINDS: [&str; 2] = ["collection", "sub"];
/// `config.KEY_MAX`: a source key becomes a file name and a URL segment.
pub const KEY_MAX: usize = 48;

/// One entry of the `sources` list, after `config.normalize_sources` repaired
/// it.
///
/// The Rust side reads the same fields the Python round needs. `export` is
/// deliberately not modelled yet: it drives publishing (R8), and a field
/// nothing reads is a field that silently drifts from its Python default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    pub key: String,
    /// `collection` or `sub`.
    pub kind: String,
    pub name: String,
    pub label: String,
    pub enabled: bool,
    /// The source's nodes are transit hops rather than exits. Only feeds the
    /// per-category statistics; `is True` rather than truthiness, so a
    /// hand-edited `"yes"` reads as "not marked" instead of reclassifying a
    /// whole source.
    pub relay: bool,
    /// Measure this source's nodes direct. Read by the round as
    /// `s.get("direct", True)`: absent means on, an explicit `false` means
    /// off, anything else is not a `false` and therefore on.
    pub direct: bool,
    /// Measure this source's nodes through the front pool. Both switches off
    /// falls back to direct at expansion time (`_measure_flags`), so a source
    /// the operator un-ticked on both sides still gets measured -- and still
    /// shows up in the ledger -- instead of silently losing its history.
    pub chain: bool,
}

impl SourceSpec {
    /// Port of `config.normalize_sources` for the fields above.
    ///
    /// Returns an error where Python raises `ValueError` -- an explicitly
    /// supplied key that is unusable as a file name is a config error the
    /// operator has to see, not something to silently repair.
    pub fn normalize(sources: &[Value]) -> DomainResult<Vec<SourceSpec>> {
        let mut out = Vec::new();
        let mut used: Vec<String> = Vec::new();
        for entry in sources {
            let Some(map) = entry.as_object() else {
                continue;
            };
            let name = map
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if name.is_empty() {
                continue;
            }
            let kind = map
                .get("kind")
                .and_then(|v| v.as_str())
                .filter(|k| SOURCE_KINDS.contains(k))
                .unwrap_or("collection")
                .to_string();
            let raw_key = map
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let mut key = if raw_key.is_empty() {
                safe_key(&name, "src")
            } else {
                validate_key(&raw_key)?
            };
            // Unique keys, `name`, `name-2`, `name-3` ...
            let base = key.clone();
            let mut suffix = 2;
            while used.contains(&key) {
                key = format!("{base}-{suffix}");
                suffix += 1;
            }
            used.push(key.clone());

            out.push(SourceSpec {
                label: map
                    .get("label")
                    .and_then(|v| v.as_str())
                    .filter(|l| !l.is_empty())
                    .unwrap_or(&name)
                    .to_string(),
                enabled: map.get("enabled") != Some(&Value::Bool(false)),
                relay: map.get("relay") == Some(&Value::Bool(true)),
                direct: map.get("direct") != Some(&Value::Bool(false)),
                chain: map.get("chain") != Some(&Value::Bool(false)),
                key,
                kind,
                name,
            });
        }
        Ok(out)
    }
}

/// `config.safe_key`: derive a filesystem- and URL-safe key from a name.
pub fn safe_key(value: &str, fallback: &str) -> String {
    let stripped: String = value.chars().filter(|c| !is_unsafe_key_char(*c)).collect();
    // `re.sub(r"\s+", "-", text.strip()).strip(".")`
    let mut collapsed = String::new();
    let mut in_space = false;
    for ch in stripped.trim().chars() {
        if ch.is_whitespace() {
            if !in_space {
                collapsed.push('-');
            }
            in_space = true;
        } else {
            collapsed.push(ch);
            in_space = false;
        }
    }
    let trimmed = collapsed.trim_matches('.');
    let truncated: String = trimmed.chars().take(KEY_MAX).collect();
    if truncated.is_empty() || truncated == "." || truncated == ".." {
        fallback.to_string()
    } else {
        truncated
    }
}

/// `config.validate_key`: reject anything unusable as a file name / URL segment.
pub fn validate_key(key: &str) -> DomainResult<String> {
    if key.is_empty() {
        return Err(DomainError::Config("key 不能为空".into()));
    }
    if key != key.trim() {
        return Err(DomainError::Config(format!("key 首尾不能有空白: {key:?}")));
    }
    if key.starts_with('.') {
        return Err(DomainError::Config(format!("key 不能以点开头: {key:?}")));
    }
    if key.chars().any(is_unsafe_key_char) {
        return Err(DomainError::Config(format!(
            "key 含非法字符 (/ \\ : * ? \" < > |): {key:?}"
        )));
    }
    if key.chars().count() > KEY_MAX {
        return Err(DomainError::Config(format!(
            "key 过长（上限 {KEY_MAX}）: {key:?}"
        )));
    }
    Ok(key.to_string())
}

/// `_UNSAFE_KEY = re.compile(r'[/\\:*?"<>|\x00-\x1f]')`, as a predicate.
fn is_unsafe_key_char(ch: char) -> bool {
    matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || (ch as u32) < 0x20
}

/// The subset of the deployment config the Rust slice reads.
#[derive(Debug, Clone)]
pub struct Config {
    /// The `core` section after defaults were applied.
    pub core: CoreConfig,
    /// The `test` section after defaults were applied.
    pub test: TestConfig,
    /// The `sources` list after `normalize_sources` repaired it.
    pub sources: Vec<SourceSpec>,
    /// The admin token. `None` means "not configured", and like the Python
    /// `auth_ok` (which denies on a falsy token) every authenticated endpoint
    /// must then deny instead of allowing.
    pub auth_token: Option<String>,
    /// The `substore` section (Rust-only; embedded Sub-Store).
    pub substore: SubStoreSection,
    /// The `chain` section after `normalize_chain` repaired it.
    pub chain: ChainSection,
    /// `publish.prefix`: the manual-front subscription is named from it.
    pub publish_prefix: String,
    /// The `policy` section: convergence thresholds and the round guard.
    pub policy: PolicySection,
}

impl Config {
    /// Deep-merge the stored config over the defaults. A missing file or
    /// invalid JSON reads as an empty object (Python `stored = {}`), so the
    /// defaults -- including the `sources` list -- survive; `Value::Null`
    /// would instead replace the whole tree and silently drop them.
    pub fn load(config_path: &Path) -> DomainResult<Self> {
        let stored = match std::fs::read_to_string(config_path) {
            Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or(json_object()),
            Err(_) => json_object(),
        };
        let mut cfg = default_tree();
        deep_merge(&mut cfg, &stored);
        let sources = cfg
            .get("sources")
            .and_then(|v| v.as_array())
            .map(|list| SourceSpec::normalize(list))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            core: CoreConfig::from_value(cfg.get("core").unwrap_or(&Value::Null)),
            test: TestConfig::from_value(cfg.get("test").unwrap_or(&Value::Null)),
            sources,
            auth_token: cfg
                .pointer("/auth/token")
                .and_then(|v| v.as_str())
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string),
            substore: SubStoreSection::from_value(cfg.get("substore").unwrap_or(&Value::Null)),
            chain: ChainSection::from_value(cfg.get("chain").unwrap_or(&Value::Null)),
            publish_prefix: cfg
                .pointer("/publish/prefix")
                .and_then(|v| v.as_str())
                .filter(|p| !p.trim().is_empty())
                .unwrap_or(DEFAULT_PUBLISH_PREFIX)
                .to_string(),
            policy: PolicySection::from_value(cfg.get("policy").unwrap_or(&Value::Null)),
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

/// The `substore` section: Rust-only (the Python service has no embedded
/// Sub-Store). `embedded: true` makes `probe-cli serve` start the embedded
/// Sub-Store (probe-substore crate) on `listen` and point the collection
/// fetcher at it, unless `SUBSTORE_BACKEND` explicitly overrides. All keys
/// are tolerated-absent; the defaults keep the section a no-op so existing
/// deployments read byte-identical behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubStoreSection {
    pub embedded: bool,
    pub listen: String,
    /// Explicit secret path; generated once under `data/substore/` when None.
    pub backend_path: Option<String>,
    pub gh_proxy: Option<String>,
    pub auto_update: bool,
    pub push_service: Option<String>,
    /// Gist-sync cron (`/api/sync/artifacts`); absent = no cron job.
    pub sync_cron: Option<String>,
    /// Produce-cache spec: `<cron>,<sub|col>,<names...>` entries separated by
    /// `;` — each name gets a host-side `/download/...` warm-up job.
    pub produce_cron: Option<String>,
}

pub const DEFAULT_SUBSTORE_LISTEN: &str = "127.0.0.1:8299";

impl Default for SubStoreSection {
    fn default() -> Self {
        Self {
            embedded: false,
            listen: DEFAULT_SUBSTORE_LISTEN.into(),
            backend_path: None,
            gh_proxy: None,
            auto_update: false,
            push_service: None,
            sync_cron: None,
            produce_cron: None,
        }
    }
}

impl SubStoreSection {
    fn from_value(value: &Value) -> Self {
        let defaults = Self::default();
        let get_str = |key: &str| -> Option<String> {
            value
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
        };
        Self {
            embedded: value
                .get("embedded")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            listen: get_str("listen").unwrap_or(defaults.listen),
            backend_path: get_str("backend_path"),
            gh_proxy: get_str("gh_proxy"),
            auto_update: value
                .get("auto_update")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            push_service: get_str("push_service"),
            sync_cron: get_str("sync_cron"),
            produce_cron: get_str("produce_cron"),
        }
    }
}

/// The `chain` section: the front pool a chained node dials through, port of
/// `config.normalize_chain`.
///
/// Three inputs name the pool and they compose rather than exclude each other:
/// `front_text` (pasted share links, materialised into a Sub-Store sub),
/// `front_pick` (display names to keep out of `front_source`), and
/// `front_source` (the whole resource). The pasted text wins the head of the
/// pool because it is the operator's explicit, just-typed choice.
///
/// Every malformed value repairs to the disabled default rather than raising:
/// this runs on every load, and a hand-edited config.json must not be able to
/// stop the service from booting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSection {
    pub enabled: bool,
    /// The Sub-Store resource kind. A kind outside `SOURCE_KINDS` reads as
    /// `sub` (Python: `if ref.get("kind") not in SOURCE_KINDS`).
    pub front_source_kind: String,
    /// The resource's name, stripped; empty means "no resource".
    pub front_source_name: String,
    /// Display names to keep from the resource, deduplicated with order
    /// preserved and capped at [`MAX_FRONT_PICK`].
    pub front_pick: Vec<String>,
    /// The pasted front list (share links or base64), stripped and capped at
    /// [`MAX_FRONT_TEXT`] bytes.
    pub front_text: String,
    /// The configured pool cap; `collect_fronts` clamps it into `1..=64`
    /// (Python does the clamp there, not in the normalizer).
    pub max_fronts: i64,
    /// Client-path mode: plain nodes (no upstream `dialer-proxy`) also get
    /// chained variants -- and no direct twin -- so the ledger's verdict is
    /// the client's verdict.
    pub test_plain_nodes: bool,
}

/// `config.MAX_FRONT_TEXT`: the paste is a JSON-blob field, not a file.
pub const MAX_FRONT_TEXT: usize = 262_144;
/// `config.MAX_FRONT_PICK`: names, not data, but still bounded.
pub const MAX_FRONT_PICK: usize = 500;

pub const DEFAULT_PUBLISH_PREFIX: &str = "probe";

impl Default for ChainSection {
    fn default() -> Self {
        Self {
            enabled: false,
            front_source_kind: "sub".into(),
            front_source_name: String::new(),
            front_pick: Vec::new(),
            front_text: String::new(),
            max_fronts: 8,
            test_plain_nodes: false,
        }
    }
}

impl ChainSection {
    fn from_value(value: &Value) -> Self {
        let source = value.get("front_source");
        let front_pick = value
            .get("front_pick")
            .and_then(|v| v.as_array())
            .map(|list| {
                let mut picked: Vec<String> = Vec::new();
                for item in list {
                    let name = item.as_str().unwrap_or("").trim();
                    if name.is_empty() || picked.iter().any(|p| p == name) {
                        continue;
                    }
                    picked.push(name.to_string());
                    if picked.len() >= MAX_FRONT_PICK {
                        break;
                    }
                }
                picked
            })
            .unwrap_or_default();
        let mut front_text = value
            .get("front_text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // Character truncation, like Python's str slicing; a byte cut would
        // land inside a UTF-8 sequence.
        if front_text.chars().count() > MAX_FRONT_TEXT {
            front_text = front_text.chars().take(MAX_FRONT_TEXT).collect();
        }
        // Python `int(block.get("max_fronts", 8) or 8)`: a falsy value (0,
        // null, false, "") falls back to 8, a float truncates, a numeric
        // string parses, anything else raises and is caught back to 8.
        let max_fronts = value.get("max_fronts").map_or(8, |raw| {
            let as_int = raw
                .as_i64()
                .or_else(|| raw.as_f64().map(|f| f as i64))
                .or_else(|| raw.as_str().and_then(|s| s.trim().parse::<i64>().ok()));
            match as_int {
                Some(n) if n != 0 => n,
                _ => 8,
            }
        });
        Self {
            enabled: value.get("enabled") == Some(&Value::Bool(true)),
            // A kind outside `SOURCE_KINDS` reads as `sub` (Python:
            // `if ref.get("kind") not in SOURCE_KINDS`).
            front_source_kind: source
                .and_then(|s| s.get("kind"))
                .and_then(|k| k.as_str())
                .filter(|k| SOURCE_KINDS.contains(k))
                .unwrap_or("sub")
                .to_string(),
            front_source_name: source
                .and_then(|s| s.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .trim()
                .to_string(),
            front_pick,
            front_text,
            max_fronts,
            test_plain_nodes: value
                .get("test_plain_nodes")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    }

    /// `engine.chain_block`: chaining counts as configured when it is enabled
    /// *and* a pool is named -- either input counts, because they are two ways
    /// to fill the same pool. `None`-for-half-configured is deliberate: a
    /// half-configured chain would otherwise fail every chained node with
    /// `front_dead` because of a missing config field, which reads as a
    /// network problem.
    pub fn is_configured(&self) -> bool {
        self.enabled && (!self.front_source_name.is_empty() || !self.front_text.is_empty())
    }

    /// `engine.collect_fronts`'s cap:
    /// `max(1, min(64, int(block.get("max_fronts", 8) or 8)))`, falling back
    /// to 8 when the value does not parse. An unclamped cap is the multiplier
    /// a fat CF subscription silently amplifies the round by.
    pub fn max_fronts_cap(&self) -> usize {
        let raw = if self.max_fronts == 0 { 8 } else { self.max_fronts };
        raw.clamp(1, 64) as usize
    }

    /// The Sub-Store subscription the pasted front list is materialised into:
    /// `{publish.prefix}-front-manual` (Python `manual_front_sub_name`).
    pub fn manual_sub_name(&self, publish_prefix: &str) -> String {
        format!("{}-front-manual", publish_prefix)
    }
}

/// The `policy` section: convergence thresholds and the round guard, port of
/// `config.DEFAULTS["policy"]`. Feeds `probe_domain::policy` (workstreams/03);
/// 0.5 / 3 / 3 are experience values pending a re-check against real round
/// history before the Rust path takes traffic.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicySection {
    /// `drop_after_consecutive_fails`: rounds of consecutive failure before a
    /// node is judged dead.
    pub drop_after_consecutive_fails: i64,
    /// `suspect_floor_ratio`: alive_now below `alive_prev * ratio` is suspect.
    pub suspect_floor_ratio: f64,
    /// `suspect_floor_absolute`: never trust a floor smaller than this.
    pub suspect_floor_absolute: i64,
}

impl Default for PolicySection {
    fn default() -> Self {
        Self {
            drop_after_consecutive_fails: 3,
            suspect_floor_ratio: 0.5,
            suspect_floor_absolute: 3,
        }
    }
}

impl From<PolicySection> for probe_domain::Policy {
    fn from(section: PolicySection) -> Self {
        Self {
            drop_after_consecutive_fails: section.drop_after_consecutive_fails,
            suspect_floor_ratio: section.suspect_floor_ratio,
            suspect_floor_absolute: section.suspect_floor_absolute,
        }
    }
}

impl PolicySection {
    fn from_value(value: &Value) -> Self {
        // Python: `int(policy.get("drop_after_consecutive_fails", 3))` and
        // `float(policy.get("suspect_floor_ratio", 0.5))` -- a malformed value
        // raises and the round dies, but a *missing* key takes the default.
        // JSON values that do not convert read as the default here (config
        // repair over config death, the same rule `normalize_chain` follows).
        let get_i64 = |key: &str, fallback: i64| -> i64 {
            value
                .get(key)
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_f64().map(|f| f as i64))
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                })
                .unwrap_or(fallback)
        };
        let get_f64 = |key: &str, fallback: f64| -> f64 {
            value
                .get(key)
                .and_then(|v| {
                    v.as_f64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                })
                .unwrap_or(fallback)
        };
        Self {
            drop_after_consecutive_fails: get_i64("drop_after_consecutive_fails", 3),
            suspect_floor_ratio: get_f64("suspect_floor_ratio", 0.5),
            suspect_floor_absolute: get_i64("suspect_floor_absolute", 3),
        }
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
            "timeout_ms_retry": DEFAULT_TIMEOUT_MS_RETRY,
            "concurrency": DEFAULT_CONCURRENCY,
            "max_attempts": DEFAULT_MAX_ATTEMPTS,
            "retry_pause_s": DEFAULT_RETRY_PAUSE_S,
        },
        // Mirrors config.DEFAULTS["sources"]. A stored list replaces it
        // wholesale (`deep_merge` replaces arrays rather than merging them),
        // which is what Python's `cfg["sources"] = normalize_sources(...)` does
        // on the file it loaded.
        "sources": [
            {"key": "air", "kind": "collection", "name": "air", "label": "air",
             "enabled": true},
        ],
        // Rust-only section; Python prunes nothing and tolerates unknown keys,
        // so a config shared between the two stays valid for both.
        "substore": {
            "embedded": false,
            "listen": DEFAULT_SUBSTORE_LISTEN,
            "backend_path": null,
            "gh_proxy": "",
            "auto_update": false,
            "push_service": "",
            "sync_cron": null,
            "produce_cron": null,
        },
        // Mirrors config.DEFAULTS["chain"] (minus the doc-only fields). Same
        // rules as `substore`: unknown-key tolerant, disabled by default.
        "chain": {
            "enabled": false,
            "front_source": {"kind": "sub", "name": ""},
            "front_pick": [],
            "front_text": "",
            "max_fronts": 8,
            "test_plain_nodes": false,
        },
        // Only the field the Rust slice reads; the rest of Python's `publish`
        // defaults stay Python-side until R8.
        "publish": {
            "prefix": DEFAULT_PUBLISH_PREFIX,
        },
        // Mirrors config.DEFAULTS["policy"].
        "policy": {
            "drop_after_consecutive_fails": 3,
            "suspect_floor_ratio": 0.5,
            "suspect_floor_absolute": 3,
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

/// An empty stored config: merges to a no-op, unlike `Value::Null` which
/// would replace the default tree wholesale (see `Config::load`).
fn json_object() -> Value {
    Value::Object(serde_json::Map::new())
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
    use serde_json::json;

    #[test]
    fn a_missing_config_file_yields_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load(&tmp.path().join("absent.json")).unwrap();
        assert_eq!(cfg.core, CoreConfig::defaults());
        assert_eq!(
            cfg.sources.len(),
            1,
            "fresh installs keep the default source"
        );
        assert_eq!(cfg.sources[0].key, "air");
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
        assert_eq!(cfg.sources.len(), 1, "a corrupt file must not drop sources");
        assert!(cfg.auth_token.is_none());
    }

    #[test]
    fn substore_section_defaults_to_a_noop_and_takes_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        // Absent section: embedded stays off, everything else default.
        let cfg = Config::load(&tmp.path().join("absent.json")).unwrap();
        assert_eq!(cfg.substore, SubStoreSection::default());
        assert!(!cfg.substore.embedded);

        // Stored overrides win per key; unspecified keys fall back.
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "substore": {"embedded": true, "listen": "127.0.0.1:18300",
                              "sync_cron": "0 9 * * *", "produce_cron": "*/30 * * * *,sub,air"},
            })
            .to_string(),
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.substore.embedded);
        assert_eq!(cfg.substore.listen, "127.0.0.1:18300");
        assert_eq!(cfg.substore.backend_path, None);
        assert!(!cfg.substore.auto_update);
        assert_eq!(cfg.substore.sync_cron.as_deref(), Some("0 9 * * *"));
        assert_eq!(
            cfg.substore.produce_cron.as_deref(),
            Some("*/30 * * * *,sub,air")
        );
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
        assert_eq!(test.timeout_ms_retry, 9_000);
        assert_eq!(test.concurrency, 20, "the deployed value");
        assert_eq!(test.max_attempts, 3);
        assert_eq!(test.retry_pause_s, 0.3);
        assert_eq!(test.targets.len(), 3);
        assert_eq!(test.preferred_target(), DEFAULT_TARGETS[0]);
    }

    #[test]
    fn a_zero_attempt_budget_is_repaired_not_obeyed() {
        // Python `max(1, int(...))`. Zero attempts would mark every node dead
        // without dialling anything.
        let test = TestConfig::from_value(&serde_json::json!({"max_attempts": 0}));
        assert_eq!(test.max_attempts, 1);
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

    // --- chain section (R6, port of config.normalize_chain) ---

    #[test]
    fn the_chain_section_defaults_to_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load(&tmp.path().join("absent.json")).unwrap();
        assert_eq!(cfg.chain, ChainSection::default());
        assert!(!cfg.chain.is_configured());
        assert_eq!(cfg.publish_prefix, DEFAULT_PUBLISH_PREFIX);
    }

    #[test]
    fn a_chain_block_repairs_each_field_like_normalize_chain() {
        let chain = ChainSection::from_value(&serde_json::json!({
            "enabled": true,
            "front_source": {"kind": "weird", "name": "  pool  "},
            "front_pick": ["b", "a", "b", "", " a ", "a"],
            "front_text": "  vless://x  ",
            "max_fronts": 3,
        }));
        assert!(chain.enabled);
        assert_eq!(chain.front_source_kind, "sub", "an unknown kind reads as sub");
        assert_eq!(chain.front_source_name, "pool");
        assert_eq!(chain.front_pick, vec!["b", "a"], "deduped, order kept, blank dropped");
        assert_eq!(chain.front_text, "vless://x");
        assert_eq!(chain.max_fronts, 3);
        assert!(chain.is_configured());
    }

    #[test]
    fn chain_block_enabled_is_strictly_true() {
        // Python `out.get("enabled") is True`: a truthy string does not count.
        for value in ["yes", "true", "1"] {
            let chain = ChainSection::from_value(&serde_json::json!({
                "enabled": value,
                "front_text": "vless://x",
            }));
            assert!(!chain.enabled, "{value:?} must not enable chaining");
            assert!(!chain.is_configured());
        }
    }

    #[test]
    fn a_half_configured_chain_is_not_configured() {
        // `chain_block` returns None when enabled but no pool is named: a
        // misconfigured chain must read as "off", not as "every chain dead".
        let enabled_no_pool = ChainSection::from_value(&serde_json::json!({"enabled": true}));
        assert!(!enabled_no_pool.is_configured());
        // A pool without enabled is equally not a chain round.
        let pool_disabled = ChainSection::from_value(&serde_json::json!({
            "front_source": {"kind": "sub", "name": "pool"},
        }));
        assert!(!pool_disabled.is_configured());
        // Either input alone is enough when enabled.
        let manual_only = ChainSection::from_value(&serde_json::json!({
            "enabled": true, "front_text": "vless://x",
        }));
        assert!(manual_only.is_configured());
    }

    #[test]
    fn max_fronts_cap_mirrors_the_python_clamp() {
        let cap = |max_fronts: Value| {
            ChainSection::from_value(&serde_json::json!({ "max_fronts": max_fronts }))
                .max_fronts_cap()
        };
        assert_eq!(cap(json!(8)), 8, "the default");
        assert_eq!(cap(json!(0)), 8, "falsy falls back to 8");
        assert_eq!(cap(json!(null)), 8);
        assert_eq!(cap(json!(false)), 8);
        assert_eq!(cap(json!(-5)), 1, "clamped low");
        assert_eq!(cap(json!(100)), 64, "clamped high");
        assert_eq!(cap(json!("12")), 12, "a numeric string parses");
        assert_eq!(cap(json!("abc")), 8, "unparseable falls back to 8");
        assert_eq!(cap(json!(7.9)), 7, "a float truncates like int()");
    }

    #[test]
    fn a_giant_front_text_is_truncated_by_characters() {
        let text = "x".repeat(MAX_FRONT_TEXT + 100);
        let chain = ChainSection::from_value(&serde_json::json!({"front_text": text}));
        assert_eq!(chain.front_text.chars().count(), MAX_FRONT_TEXT);
    }

    #[test]
    fn the_manual_front_sub_is_named_from_the_publish_prefix() {
        let chain = ChainSection::default();
        assert_eq!(chain.manual_sub_name("probe"), "probe-front-manual");
        assert_eq!(chain.manual_sub_name("mx"), "mx-front-manual");
    }

    #[test]
    fn a_prefix_only_config_loads_into_the_chain_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({"publish": {"prefix": "mx"},
                               "chain": {"enabled": true, "front_text": "vless://y"}})
            .to_string(),
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.publish_prefix, "mx");
        assert_eq!(
            cfg.chain.manual_sub_name(&cfg.publish_prefix),
            "mx-front-manual"
        );
        assert!(cfg.chain.is_configured());
    }

    #[test]
    fn the_policy_section_defaults_and_repairs() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load(&tmp.path().join("absent.json")).unwrap();
        assert_eq!(cfg.policy, PolicySection::default());
        assert_eq!(cfg.policy.drop_after_consecutive_fails, 3);
        assert_eq!(cfg.policy.suspect_floor_ratio, 0.5);
        assert_eq!(cfg.policy.suspect_floor_absolute, 3);

        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "policy": {"drop_after_consecutive_fails": 5,
                           "suspect_floor_ratio": "0.4",
                           "suspect_floor_absolute": 2.9}
            })
            .to_string(),
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.policy.drop_after_consecutive_fails, 5);
        assert_eq!(cfg.policy.suspect_floor_ratio, 0.4, "a numeric string parses");
        assert_eq!(cfg.policy.suspect_floor_absolute, 2, "a float truncates");
    }

    #[test]
    fn the_policy_section_converts_into_the_domain_policy() {
        let policy = probe_domain::Policy::from(PolicySection::default());
        assert_eq!(policy.drop_after_consecutive_fails, 3);
        assert!(!probe_domain::round_is_suspect(5, 0, &policy));
        assert!(probe_domain::round_is_suspect(1, 10, &policy));
    }

    #[test]
    fn source_direct_and_chain_switches_default_on_and_false_turns_them_off() {
        let sources = SourceSpec::normalize(&[
            json!({"name": "plain"}),
            json!({"name": "no-chain", "chain": false}),
            json!({"name": "no-direct", "direct": false}),
            json!({"name": "truthy", "direct": "yes"}),
        ])
        .unwrap();
        assert!(sources[0].direct && sources[0].chain, "absent means on");
        assert!(sources[1].direct && !sources[1].chain);
        assert!(!sources[2].direct && sources[2].chain);
        // `s.get("direct", True)`: anything that is not literally false is on.
        assert!(sources[3].direct);
    }
}
