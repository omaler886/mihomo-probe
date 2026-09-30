//! Kernel config validation and culprit location (Python `core.config_test`
//! / `core._culprit_from` / `core.make_testable`'s prune loop).
//!
//! Validity is decided by the kernel itself (`mihomo -t`), never by a
//! successful YAML serialize (GLM_5.3_Flash §5). The validator tries, in
//! order: the `MIHOMO_BIN` binary (fixed argv), then `docker run` against the
//! pinned image, and when neither exists it reports `Degraded` **explicitly**
//! -- the Python side silently skipped validation when docker was missing,
//! which read as a pass; a degraded check is a check that did not happen and
//! must be logged as such by the caller (workstreams/04).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigCheck {
    /// The kernel accepted the config; carries its output.
    Passed(String),
    /// The kernel rejected the config; carries the error output.
    Failed(String),
    /// No validator was available. Not a pass -- callers must record this.
    Degraded(String),
}

impl ConfigCheck {
    pub fn is_ok(&self) -> bool {
        matches!(self, ConfigCheck::Passed(_))
    }
}

const KERNEL_TIMEOUT: Duration = Duration::from_secs(120);
const DOCKER_PROBE_TTL: Duration = Duration::from_secs(60);

fn docker_cli() -> Option<PathBuf> {
    static CACHE: Mutex<Option<(Instant, Option<PathBuf>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().ok()?;
    if let Some((at, path)) = cache.as_ref() {
        if at.elapsed() < DOCKER_PROBE_TTL {
            return path.clone();
        }
    }
    let probe = || -> Option<PathBuf> {
        let cli = which::which_docker()?;
        let ok = std::process::Command::new(&cli)
            .args(["version", "--format", "{{.Server.Version}}"])
            .output()
            .ok()?
            .status
            .success();
        ok.then_some(cli)
    };
    let found = probe();
    *cache = Some((Instant::now(), found.clone()));
    found
}

// A two-line PATH search, local to this module so the crate needs no
// `which` dependency.
mod which {
    use std::path::PathBuf;

    pub fn which_docker() -> Option<PathBuf> {
        let exe = if cfg!(windows) {
            "docker.exe"
        } else {
            "docker"
        };
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(exe))
            .find(|candidate| candidate.is_file())
    }
}

fn run_validator(argv: &[String]) -> (i32, String) {
    use std::process::Stdio;

    let child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        // A vanished binary degrades to "could not run", same as the Python
        // SubprocessError path.
        Err(err) => return (-1, format!("config test could not run: {err}")),
    };
    // Python bounded this with subprocess timeout=120; a hung `docker run`
    // must not wedge the round. std has no wait_timeout, so poll.
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The pipes were piped; read what the kernel said.
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut io) = child.stdout.take() {
                    use std::io::Read;
                    let _ = io.read_to_string(&mut stdout);
                }
                if let Some(mut io) = child.stderr.take() {
                    use std::io::Read;
                    let _ = io.read_to_string(&mut stderr);
                }
                let text = format!("{stdout}{stderr}");
                return (status.code().unwrap_or(-1), text.trim().to_string());
            }
            Ok(None) => {
                if started.elapsed() > KERNEL_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (-1, "config test timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(err) => return (-1, format!("config test could not run: {err}")),
        }
    }
}

fn interpret(code: i32, output: String) -> ConfigCheck {
    if code == 0 {
        ConfigCheck::Passed(output)
    } else {
        ConfigCheck::Failed(output)
    }
}

/// Validate `<dir>/config.yaml` with the kernel. Host-side path: the docker
/// validator bind-mounts it, exactly like the Python one.
pub fn validate(host_dir: &Path) -> ConfigCheck {
    if let Some(bin) = std::env::var_os("MIHOMO_BIN") {
        let bin = PathBuf::from(bin);
        if bin.is_file() {
            let argv = vec![
                bin.to_string_lossy().into_owned(),
                "-t".into(),
                "-d".into(),
                host_dir.to_string_lossy().into_owned(),
                "-f".into(),
                host_dir.join("config.yaml").to_string_lossy().into_owned(),
            ];
            let (code, output) = run_validator(&argv);
            return interpret(code, output);
        }
    }
    if let Some(cli) = docker_cli() {
        let host = host_dir.to_string_lossy().into_owned();
        let argv = vec![
            cli.to_string_lossy().into_owned(),
            "run".into(),
            "--rm".into(),
            "-v".into(),
            format!("{host}:/root/.config/mihomo"),
            "metacubex/mihomo:latest".into(),
            "-t".into(),
            "-d".into(),
            "/root/.config/mihomo".into(),
            "-f".into(),
            "/root/.config/mihomo/config.yaml".into(),
        ];
        let (code, output) = run_validator(&argv);
        return interpret(code, output);
    }
    ConfigCheck::Degraded(
        "no mihomo binary (MIHOMO_BIN) and no docker daemon: config validation skipped".to_string(),
    )
}

/// One candidate the kernel's error text may point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyRef {
    pub name: String,
    pub server: String,
}

/// Locate the proxy the kernel's error text points at, by index into
/// `proxies`; None when nothing matches confidently.
///
/// Deliberately conservative, matching the Python rule that one bad drop cost
/// whole rounds: a quoted name wins first, then a bounded token match on
/// names long enough to be meaningful (`jp` matches everything), then the
/// server address.
pub fn culprit_from(output: &str, proxies: &[ProxyRef]) -> Option<usize> {
    for line in output.lines() {
        for (idx, proxy) in proxies.iter().enumerate() {
            if !proxy.name.is_empty() && line.contains(&format!("\"{}\"", proxy.name)) {
                return Some(idx);
            }
        }
    }
    for line in output.lines() {
        for (idx, proxy) in proxies.iter().enumerate() {
            if proxy.name.len() >= 4 && find_word(line, &proxy.name) {
                return Some(idx);
            }
        }
    }
    let lowered = output.to_lowercase();
    for (idx, proxy) in proxies.iter().enumerate() {
        if proxy.server.len() >= 4 && lowered.contains(&proxy.server.to_lowercase()) {
            return Some(idx);
        }
    }
    None
}

/// Bounded token match: `needle` must appear with non-identifier characters
/// (or line edges) on both sides -- the `(?<![\w-])...(?![\w-])` guard in the
/// Python regex, hand-rolled so this crate keeps zero regex dependencies.
fn find_word(hay: &str, needle: &str) -> bool {
    let hay_lower = hay.to_lowercase();
    let needle_lower = needle.to_lowercase();
    let mut start = 0;
    while let Some(pos) = hay_lower[start..].find(&needle_lower) {
        let abs = start + pos;
        let end = abs + needle_lower.len();
        let before_ok = hay_lower[..abs]
            .chars()
            .next_back()
            .map(|c| !is_word_char(c))
            .unwrap_or(true);
        let after_ok = hay_lower[end..]
            .chars()
            .next()
            .map(|c| !is_word_char(c))
            .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_interpret_into_verdicts() {
        assert!(interpret(0, "fine".into()).is_ok());
        assert!(matches!(
            interpret(1, "proxy 147: invalid REALITY short ID".into()),
            ConfigCheck::Failed(_)
        ));
    }

    #[test]
    fn degraded_is_explicit_when_no_validator_exists() {
        // On a machine with neither MIHOMO_BIN nor docker this must say so
        // instead of reading as a pass. (With docker -- e.g. CI -- the check
        // runs for real; the assertion only covers the degraded branch.)
        if std::env::var_os("MIHOMO_BIN").is_some() || docker_cli().is_some() {
            return;
        }
        let tmp = std::env::temp_dir().join("probe-config-check-empty");
        std::fs::create_dir_all(&tmp).unwrap();
        let check = validate(&tmp);
        assert!(matches!(check, ConfigCheck::Degraded(_)), "{check:?}");
    }

    #[test]
    fn a_quoted_name_wins_over_everything() {
        let proxies = vec![
            ProxyRef {
                name: "alpha".into(),
                server: "203.0.113.10".into(),
            },
            ProxyRef {
                name: "beta".into(),
                server: "203.0.113.11".into(),
            },
        ];
        let out = "proxy [\"beta\"] server not found";
        assert_eq!(culprit_from(out, &proxies), Some(1));
    }

    #[test]
    fn a_short_name_never_token_matches() {
        // The historical bug: a node called "jp" matched almost any line and
        // whatever it picked was dropped from the round.
        let proxies = vec![
            ProxyRef {
                name: "jp".into(),
                server: "198.51.100.1".into(),
            },
            ProxyRef {
                name: "other".into(),
                server: "198.51.100.2".into(),
            },
        ];
        assert_eq!(
            culprit_from("unrelated line about japan proxies", &proxies),
            None
        );
    }

    #[test]
    fn a_long_name_token_matches_on_boundaries_only() {
        let proxies = vec![
            ProxyRef {
                name: "BageVM-Tokyo".into(),
                server: "198.51.100.3".into(),
            },
            ProxyRef {
                name: "filler".into(),
                server: "198.51.100.4".into(),
            },
        ];
        assert_eq!(
            culprit_from("dns resolution failed for BageVM-Tokyo", &proxies),
            Some(0)
        );
        // Substring of a longer identifier is NOT a boundary match.
        assert_eq!(
            culprit_from("BageVM-Tokyo-2 timed out", &proxies),
            None,
            "BageVM-Tokyo must not match inside BageVM-Tokyo-2"
        );
    }

    #[test]
    fn the_server_address_is_the_last_resort() {
        let proxies = vec![ProxyRef {
            name: "alpha".into(),
            server: "198.51.100.9".into(),
        }];
        assert_eq!(
            culprit_from("dial 198.51.100.9:443 refused", &proxies),
            Some(0)
        );
    }
}
