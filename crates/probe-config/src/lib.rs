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

pub mod network;
pub use network::{
    DnsSection, DnsView, PayloadScope, PayloadSection, VerifySection, WatchdogSection,
};

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
        // A port of 0 parses but binds nothing usable: `mixed-port: 0` makes
        // the kernel pick an ephemeral port and `base_port: 0` puts the lane
        // listeners on 0..lanes-1, which are not dialable. Python bounds
        // mixed_port at (1024, 65535); the narrower repair here is to treat
        // 0 like any other invalid value and fall back to the default.
        // (`lanes` is separately clamped at every consumer, so 0 lanes
        // cannot reach the kernel either.)
        let get_port = |key: &str, fallback: u16| -> u16 {
            value
                .get(key)
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok())
                .filter(|v| *v != 0)
                .unwrap_or(fallback)
        };
        Self {
            api: value
                .get("api")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_API)
                .to_string(),
            lanes: get_port("lanes", defaults.lanes),
            base_port: get_port("base_port", defaults.base_port),
            mixed_port: get_port("mixed_port", defaults.mixed_port),
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
    /// Produce a subscription export for this source. Python
    /// `normalize_sources`: `entry.get("export") is not False`, so only an
    /// explicit JSON `false` turns it off -- a hand-edited `0` or `"no"`
    /// still exports, matching the Python identity check.
    pub export: bool,
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
                export: map.get("export") != Some(&Value::Bool(false)),
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
    /// The `publish` section (R8): export root, prefix, region tags, publish token.
    pub publish: PublishSection,
    /// The `schedule` section: the in-process round timer (R9).
    pub schedule: ScheduleSection,
    /// The `policy` section: convergence thresholds and the round guard.
    pub policy: PolicySection,
    pub dns: DnsSection,
    pub verify: VerifySection,
    pub watchdog: WatchdogSection,
    /// G07 quality sampling: bounded throughput transfers for a subset of
    /// alive nodes. Off by default -- it spends real traffic on top of the
    /// verification the round already did.
    pub quality: QualitySection,
}

/// `quality`: per-round throughput sampling for a small, spread subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualitySection {
    pub enabled: bool,
    /// Alive nodes sampled per round; 0 reads as "disabled".
    pub sample_size: usize,
    /// Hard cap on the bytes one sample may transfer.
    pub max_bytes: usize,
    pub timeout_s: u64,
}

impl Default for QualitySection {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_size: 5,
            max_bytes: 262_144,
            timeout_s: 15,
        }
    }
}

impl QualitySection {
    fn from_value(value: &Value) -> Self {
        let defaults = Self::default();
        Self {
            enabled: value
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(defaults.enabled),
            sample_size: value
                .get("sample_size")
                .and_then(Value::as_u64)
                .map(|size| size.min(64) as usize)
                .unwrap_or(defaults.sample_size),
            max_bytes: value
                .get("max_bytes")
                .and_then(Value::as_u64)
                .map(|bytes| bytes.clamp(1024, 4 * 1024 * 1024) as usize)
                .unwrap_or(defaults.max_bytes),
            timeout_s: value
                .get("timeout_s")
                .and_then(Value::as_u64)
                .map(|secs| secs.clamp(1, 120))
                .unwrap_or(defaults.timeout_s),
        }
    }
}

impl Config {
    /// Deep-merge the stored config over the defaults. A missing file or
    /// invalid JSON reads as an empty object (Python `stored = {}`), so the
    /// defaults -- including the `sources` list -- survive; `Value::Null`
    /// would instead replace the whole tree and silently drop them.
    pub fn load(config_path: &Path) -> DomainResult<Self> {
        let text = std::fs::read_to_string(config_path).unwrap_or_default();
        Ok(Self::parse(&text)?.0)
    }

    /// Parse a stored config document without touching the filesystem.
    ///
    /// The boolean is `json_valid`: `false` means the text was not JSON at all
    /// and the returned configuration is therefore the defaults. `load` takes
    /// that same silent-fallback path (Python `stored = {}`), so the only way
    /// an operator can notice a broken file is by asking -- which is what the
    /// self-check endpoint does.
    pub fn parse(text: &str) -> DomainResult<(Self, bool)> {
        let (stored, json_valid) = match serde_json::from_str::<Value>(text) {
            Ok(value) => (value, true),
            Err(_) => (json_object(), false),
        };
        let mut cfg = default_tree();
        deep_merge(&mut cfg, &stored);
        let sources = cfg
            .get("sources")
            .and_then(|v| v.as_array())
            .map(|list| SourceSpec::normalize(list))
            .transpose()?
            .unwrap_or_default();
        let publish = PublishSection::from_value(cfg.get("publish").unwrap_or(&Value::Null));
        let schedule = ScheduleSection::from_value(cfg.get("schedule").unwrap_or(&Value::Null));
        Ok((
            Self {
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
                publish_prefix: publish.prefix.clone(),
                publish,
                schedule,
                policy: PolicySection::from_value(cfg.get("policy").unwrap_or(&Value::Null)),
                dns: DnsSection::from_value(cfg.get("dns").unwrap_or(&Value::Null)),
                verify: VerifySection::from_value(cfg.get("verify").unwrap_or(&Value::Null)),
                watchdog: WatchdogSection::from_value(cfg.get("watchdog").unwrap_or(&Value::Null)),
                quality: QualitySection::from_value(cfg.get("quality").unwrap_or(&Value::Null)),
            },
            json_valid,
        ))
    }

    /// Read the kernel API secret, creating it on first use -- the Python
    /// `config.core_secret()` contract: one secret file, 0600, generated once.
    /// Python chmods on every read, so an existing file with the wrong mode
    /// (a plain `fs::write` used to create it 0644) is repaired here too.
    ///
    /// `Ok` is only ever returned for a value that IS durably persisted in a
    /// readable file: handing out a generated secret that never reached disk
    /// would let a round write an unauthenticated kernel config, which is the
    /// one outcome this function must never produce.
    pub fn core_secret(data_dir: &Path) -> DomainResult<String> {
        let path = data_dir.join(CORE_SECRET_FILE);
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                restrict_secret_permissions(&path);
                return Ok(trimmed.to_string());
            }
            // An empty secret file is healed like Python heals it
            // (`write_text` truncates): generate and overwrite in place.
            return write_new_secret(&path);
        }
        // The read failed: the path is absent, unreadable, or not a file at
        // all. Creating a NEW file must also be the only write -- and on
        // Linux `open(O_CREAT|O_EXCL)` on an existing path reports EEXIST
        // whether that path is a file, a directory, or anything else, so the
        // AlreadyExists branch re-verifies by READING the file back instead
        // of assuming one exists.
        let secret = random_hex(16);
        std::fs::create_dir_all(data_dir).map_err(|e| {
            DomainError::Config(format!("cannot create {}: {e}", data_dir.display()))
        })?;
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(secret.as_bytes()).map_err(|e| {
                        DomainError::Config(format!("cannot write {}: {e}", path.display()))
                    })?;
                    Ok(secret)
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Someone created it between the failed read and here --
                    // or the path is not a readable file. Only a read-back
                    // decides which, and only a readable non-empty FILE may
                    // produce Ok (a directory named `core.secret`, an
                    // unreadable file, or anything else blocks the round).
                    let existing = std::fs::read_to_string(&path).map_err(|e| {
                        DomainError::Config(format!("cannot read {}: {e}", path.display()))
                    })?;
                    let trimmed = existing.trim();
                    if trimmed.is_empty() {
                        return Err(DomainError::Config(format!(
                            "core secret at {} is empty",
                            path.display()
                        )));
                    }
                    restrict_secret_permissions(&path);
                    Ok(trimmed.to_string())
                }
                Err(err) => Err(DomainError::Config(format!(
                    "cannot create {}: {err}",
                    path.display()
                ))),
            }
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&path, &secret).map_err(|e| {
                DomainError::Config(format!("cannot write {}: {e}", path.display()))
            })?;
            Ok(secret)
        }
    }
}

/// Overwrite an existing-but-empty secret file with a freshly generated one.
/// Python parity: `write_text` truncates in place, and the 0600 repair runs
/// after the write because mode() only applies at creation time.
fn write_new_secret(path: &Path) -> DomainResult<String> {
    let secret = random_hex(16);
    std::fs::write(path, &secret)
        .map_err(|e| DomainError::Config(format!("cannot write {}: {e}", path.display())))?;
    restrict_secret_permissions(path);
    Ok(secret)
}

/// Python parity (`config.py` chmods the file on every access): force 0600 on
/// the secret file, repairing files created world-readable by earlier builds.
/// Best effort -- a failure to chmod must not break a working secret, but it
/// is reported by the caller's own diagnostics (the API self-check shows the
/// effective mode side nowhere else, and an unwritable dir is already a
/// deployment error).
#[cfg(unix)]
fn restrict_secret_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_secret_permissions(_path: &Path) {}

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
/// Python `schedule.interval_minutes` default.
pub const DEFAULT_SCHEDULE_MINUTES: i64 = 30;
/// `config.PATCH_RANGES["schedule.interval_minutes"]`: clamped, never rejected.
pub const MIN_SCHEDULE_MINUTES: i64 = 1;
pub const MAX_SCHEDULE_MINUTES: i64 = 1440;
/// Python `publish.hostname` default (`MIHOMO_TEST_HOSTNAME` or this).
pub const DEFAULT_PUBLISH_HOSTNAME: &str = "probe.example.com";

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
        let raw = if self.max_fronts == 0 {
            8
        } else {
            self.max_fronts
        };
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

/// The `publish` section (R8): what gets exported, where it lands, and which
/// credential may read it back.
///
/// `token` is the read-only publish credential, kept separate from
/// `auth.token` on purpose: the export URL is pasted into Sub-Store and shown
/// in dashboards, so it leaks by design -- and the admin token also drives the
/// host's container runtime. An absent/empty token denies every export
/// download (never "auth off"), the same rule `auth.token` follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishSection {
    /// Master switch for producing exports. Off = the round writes no files
    /// and activates no snapshot.
    pub enabled: bool,
    /// Keep pushing exports into Sub-Store as `<prefix>-<key>-local`.
    pub push_to_substore: bool,
    /// Subscription name prefix (`<prefix>-<key>-local`).
    pub prefix: String,
    /// Hostname used when building export URLs for display/push.
    pub hostname: String,
    /// Rewrite the `[CC]` region tag from the measured exit country.
    pub add_region_tag: bool,
    /// Read-only credential for `GET /api/export/<key>.yaml`. `None` (or an
    /// empty/whitespace value) means no export download is authorized.
    pub token: Option<String>,
    /// Export root. `None` = `<root>/exports` (see [`PublishSection::export_root`]).
    pub dir: Option<String>,
    /// Snapshots kept per key; the active one is always kept. `None` keeps
    /// everything, which is what the publication module assumes by default:
    /// deleting history is an operator decision, not a silent default.
    pub keep_snapshots: Option<usize>,
}

/// Directory under the service root that holds exports by default.
pub const EXPORT_DIR_NAME: &str = "exports";

impl PublishSection {
    /// The absolute export root every caller must agree on: the configured
    /// `dir`, else `<service root>/exports`.
    pub fn export_root(&self, root: &Path) -> PathBuf {
        match self.dir.as_deref() {
            Some(dir) if !dir.trim().is_empty() => PathBuf::from(dir.trim()),
            _ => root.join(EXPORT_DIR_NAME),
        }
    }
}

impl Default for PublishSection {
    fn default() -> Self {
        Self {
            enabled: true,
            push_to_substore: false,
            prefix: DEFAULT_PUBLISH_PREFIX.to_string(),
            hostname: DEFAULT_PUBLISH_HOSTNAME.to_string(),
            add_region_tag: true,
            token: None,
            dir: None,
            keep_snapshots: None,
        }
    }
}

impl PublishSection {
    fn from_value(value: &Value) -> Self {
        let defaults = Self::default();
        let get_bool = |key: &str, fallback: bool| -> bool {
            value.get(key).and_then(Value::as_bool).unwrap_or(fallback)
        };
        let text = |key: &str, fallback: &str| -> String {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .unwrap_or(fallback)
                .to_string()
        };
        Self {
            enabled: get_bool("enabled", defaults.enabled),
            push_to_substore: get_bool("push_to_substore", defaults.push_to_substore),
            prefix: text("prefix", &defaults.prefix),
            hostname: text("hostname", &defaults.hostname),
            add_region_tag: get_bool("add_region_tag", defaults.add_region_tag),
            token: value
                .get("token")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_string),
            dir: value
                .get("dir")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|dir| !dir.is_empty())
                .map(str::to_string),
            // Below one the pass would have nothing to keep, so a zero reads
            // as "not configured" rather than "delete everything".
            keep_snapshots: value
                .get("keep_snapshots")
                .and_then(Value::as_u64)
                .filter(|keep| *keep >= 1)
                .map(|keep| keep as usize),
        }
    }
}

/// The `schedule` section: the in-process round timer (Python `scheduler_loop`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleSection {
    /// Off = no round is ever started by the timer (manual and API rounds still work).
    pub enabled: bool,
    /// Minutes between scheduled rounds; Python clamps to 1..=1440.
    pub interval_minutes: i64,
}

impl Default for ScheduleSection {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: DEFAULT_SCHEDULE_MINUTES,
        }
    }
}

impl ScheduleSection {
    fn from_value(value: &Value) -> Self {
        let defaults = Self::default();
        Self {
            enabled: value
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(defaults.enabled),
            interval_minutes: value
                .get("interval_minutes")
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_f64().map(|f| f as i64))
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                })
                .unwrap_or(defaults.interval_minutes)
                .clamp(MIN_SCHEDULE_MINUTES, MAX_SCHEDULE_MINUTES),
        }
    }

    /// The clamped interval. Every caller must use this instead of the raw
    /// field so a hand-edited zero cannot turn the timer into a busy loop.
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.interval_minutes
                .clamp(MIN_SCHEDULE_MINUTES, MAX_SCHEDULE_MINUTES) as u64
                * 60,
        )
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
        "dns": DnsSection::default(),
        "verify": VerifySection::default(),
        "watchdog": WatchdogSection::default(),
        "quality": {
            "enabled": false,
            "sample_size": 5,
            "max_bytes": 262144,
            "timeout_s": 15,
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
        // Python's `publish` defaults; `dir` is Rust-only (Python derives the
        // export root from its data directory).
        "publish": {
            "enabled": true,
            "push_to_substore": false,
            "prefix": DEFAULT_PUBLISH_PREFIX,
            "hostname": DEFAULT_PUBLISH_HOSTNAME,
            "add_region_tag": true,
            "token": "",
            "dir": null,
            "keep_snapshots": null,
        },
        // Mirrors config.DEFAULTS["schedule"] (the Rust serve loop reads it).
        "schedule": {
            "interval_minutes": DEFAULT_SCHEDULE_MINUTES,
            "enabled": true,
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

    #[cfg(unix)]
    #[test]
    fn core_secret_file_is_never_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let secret = Config::core_secret(tmp.path()).unwrap();
        let mode = std::fs::metadata(tmp.path().join("core.secret"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the kernel bearer secret must not be readable by other local users"
        );
        // A file left world-readable by an older build is repaired on read,
        // exactly like Python's chmod-on-every-access.
        let path = tmp.path().join("core.secret");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let again = Config::core_secret(tmp.path()).unwrap();
        assert_eq!(again, secret);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "an inherited 0644 secret is repaired");
    }

    #[test]
    fn zero_ports_fall_back_to_defaults_instead_of_binding_nothing() {
        // Python bounds mixed_port at (1024, 65535); `mixed-port: 0` in the
        // kernel config picks an ephemeral port and `base_port: 0` puts the
        // lane listeners on privileged ports 0..7. Zero is not a usable
        // port here, so it reads as unset.
        let core = CoreConfig::from_value(&serde_json::json!({
            "mixed_port": 0,
            "base_port": 0,
            "lanes": 0,
        }));
        let defaults = CoreConfig::defaults();
        assert_eq!(core.mixed_port, defaults.mixed_port);
        assert_eq!(core.base_port, defaults.base_port);
        assert_eq!(core.lanes, defaults.lanes);
    }

    #[test]
    fn an_empty_secret_file_is_healed_and_the_value_is_persisted() {
        // Python `write_text` truncates an empty secret file in place; the
        // healed value must be exactly what the file holds afterwards, or a
        // restart would generate a DIFFERENT secret and break the kernel.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("core.secret"), "").unwrap();
        let secret = Config::core_secret(tmp.path()).unwrap();
        assert_eq!(secret.len(), 32);
        let on_disk = std::fs::read_to_string(tmp.path().join("core.secret"))
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(
            on_disk, secret,
            "the handed-out secret must be the persisted one"
        );
        // And the next read reuses it.
        assert_eq!(Config::core_secret(tmp.path()).unwrap(), secret);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_named_like_the_secret_never_yields_a_secret() {
        // On Linux, open(O_CREAT|O_EXCL) on an existing directory reports
        // EEXIST, and treating that as "the file exists, carry on" handed out
        // a secret that never reached disk -- an unauthenticated kernel
        // config. Only a readable non-empty FILE may produce Ok.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("core.secret")).unwrap();
        let err = Config::core_secret(tmp.path()).unwrap_err().to_string();
        assert!(
            err.contains("cannot read"),
            "the error must say the secret is unreadable: {err}"
        );
        // The directory's mode was not touched either.
        let mode = std::fs::metadata(tmp.path().join("core.secret"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "a non-file path is never chmodded");
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
    fn the_publish_section_defaults_match_python_and_deny_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load(&tmp.path().join("absent.json")).unwrap();
        assert!(cfg.publish.enabled);
        assert!(!cfg.publish.push_to_substore);
        assert_eq!(cfg.publish.prefix, DEFAULT_PUBLISH_PREFIX);
        assert_eq!(cfg.publish.hostname, DEFAULT_PUBLISH_HOSTNAME);
        assert!(cfg.publish.add_region_tag);
        assert_eq!(
            cfg.publish.token, None,
            "an empty token must deny export downloads, never allow them"
        );
        assert_eq!(cfg.publish.dir, None);
        assert_eq!(cfg.publish_prefix, cfg.publish.prefix);
    }

    #[test]
    fn the_publish_section_reads_a_stored_block_without_inventing_values() {
        let publish = PublishSection::from_value(&serde_json::json!({
            "enabled": false,
            "push_to_substore": true,
            "prefix": "  mx  ",
            "hostname": "probe.example.net",
            "add_region_tag": false,
            "token": "  secret-token  ",
            "dir": "  /srv/exports  ",
        }));
        assert!(!publish.enabled);
        assert!(publish.push_to_substore);
        assert_eq!(publish.prefix, "mx");
        assert_eq!(publish.hostname, "probe.example.net");
        assert!(!publish.add_region_tag);
        assert_eq!(publish.token.as_deref(), Some("secret-token"));
        assert_eq!(publish.dir.as_deref(), Some("/srv/exports"));
        // A whitespace-only token is not a token.
        let blank = PublishSection::from_value(&serde_json::json!({"token": "   "}));
        assert_eq!(blank.token, None);
    }

    /// Retention is off by default: deleting publication history is an
    /// operator decision, and zero must never read as "delete everything".
    /// Deterministic mini-fuzz: `Config::parse` must never panic on hostile
    /// text, only ever answer `Ok`/`Err`.
    ///
    /// Stable-toolchain stand-in for libFuzzer (the real campaign procedure is
    /// in TEST_REPORT_RUST.md): a fixed seed and a fixed mutation schedule mean
    /// any failure is reproducible from the report alone.
    #[test]
    fn config_parsing_survives_random_input() {
        const SEEDS: [&str; 4] = [
            r#"{"sources":[],"publish":{"keep_snapshots":3}}"#,
            r#"{"dns":{"views":[]},"verify":{"chain_payload":{"scope":"all"}}}"#,
            "{\"publish\": {\"prefix\": \"pr",
            "",
        ];
        let mut state = 0x5eed_1234_abcd_9876u64;
        let mut next = |bound: usize| -> usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % (bound.max(1) as u64)) as usize
        };
        for iteration in 0..2_000 {
            let seed = SEEDS[next(SEEDS.len())];
            let mut bytes = seed.as_bytes().to_vec();
            match next(5) {
                0 => {
                    if !bytes.is_empty() {
                        let at = next(bytes.len());
                        bytes[at] = next(256) as u8;
                    }
                }
                1 => {
                    let cut = next(bytes.len() + 1);
                    bytes.truncate(cut);
                }
                2 => {
                    let at = next(bytes.len() + 1);
                    let byte = next(256) as u8;
                    bytes.insert(at, byte);
                }
                3 => bytes.extend_from_slice(&[0x00, 0xff, 0xfe, b'{', b'}', b'\"']),
                _ => bytes.reverse(),
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let outcome = std::panic::catch_unwind(|| Config::parse(&text));
            assert!(
                outcome.is_ok(),
                "Config::parse panicked on iteration {iteration} with {text:?}"
            );
        }
    }

    #[test]
    fn publish_retention_is_opt_in_and_rejects_zero() {
        assert_eq!(PublishSection::default().keep_snapshots, None);
        assert_eq!(
            PublishSection::from_value(&serde_json::json!({"keep_snapshots": 7})).keep_snapshots,
            Some(7)
        );
        assert_eq!(
            PublishSection::from_value(&serde_json::json!({"keep_snapshots": 0})).keep_snapshots,
            None,
            "zero would prune everything, so it reads as unset"
        );
    }

    #[test]
    fn the_schedule_section_clamps_like_the_python_patch_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load(&tmp.path().join("absent.json")).unwrap();
        assert!(cfg.schedule.enabled);
        assert_eq!(cfg.schedule.interval_minutes, DEFAULT_SCHEDULE_MINUTES);
        assert_eq!(cfg.schedule.interval().as_secs(), 1800);

        let zero = ScheduleSection::from_value(&serde_json::json!({
            "interval_minutes": 0,
            "enabled": false,
        }));
        assert!(!zero.enabled);
        assert_eq!(
            zero.interval_minutes, MIN_SCHEDULE_MINUTES,
            "0 clamps up: the timer must never become a busy loop"
        );
        assert_eq!(zero.interval().as_secs(), 60);

        let huge = ScheduleSection::from_value(&serde_json::json!({
            "interval_minutes": 99999,
        }));
        assert_eq!(huge.interval_minutes, MAX_SCHEDULE_MINUTES);
        assert!(huge.enabled, "a missing enabled key keeps the default");
    }

    #[test]
    fn only_an_explicit_false_disables_a_source_export() {
        let stored = serde_json::json!([
            {"key": "a", "name": "a"},
            {"key": "b", "name": "b", "export": false},
            {"key": "c", "name": "c", "export": 0},
            {"key": "d", "name": "d", "export": "no"},
        ]);
        let sources = SourceSpec::normalize(stored.as_array().unwrap()).unwrap();
        let flags: Vec<bool> = sources.iter().map(|s| s.export).collect();
        assert_eq!(
            flags,
            vec![true, false, true, true],
            "Python uses `is not False`: only JSON false turns export off"
        );
        assert!(sources[1].enabled, "export off does not disable measuring");
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
        assert_eq!(
            chain.front_source_kind, "sub",
            "an unknown kind reads as sub"
        );
        assert_eq!(chain.front_source_name, "pool");
        assert_eq!(
            chain.front_pick,
            vec!["b", "a"],
            "deduped, order kept, blank dropped"
        );
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
        assert_eq!(
            cfg.policy.suspect_floor_ratio, 0.4,
            "a numeric string parses"
        );
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
