//! Mihomo kernel control: config generation, failure classification, and the
//! Controller API client.
//!
//! The kernel stays the sole owner of the proxy protocol stack -- this crate
//! only generates the config the kernel reads, asks its REST API to work, and
//! classifies the answers (GLM_5.3_Flash §5). Config validity is finally
//! decided by `mihomo -t`, never by a successful YAML serialize; the slice
//! writes the same file shape `core.build_config` produces so both
//! implementations can be diffed byte for byte during shadow runs.

use std::path::Path;

use probe_config::CoreConfig;
use serde_json::Value;

pub mod config_check;
pub mod controller;
pub mod exit;
pub mod lanes;

pub use config_check::{culprit_from, validate, ConfigCheck, ProxyRef};
pub use controller::{Controller, DelayOutcome, KernelError};
pub use exit::{egress, fetch, parse_trace, ExitIdentity};
pub use lanes::{lane_count, lane_group, lane_members, lane_name, lane_ports};

/// Loopback-only kernel config, field-for-field the Python `build_config`
/// output (same key order, same quoting style, inline-JSON proxies).
///
/// The inline JSON per proxy matters: the kernel's Go YAML parser still
/// applies the YAML 1.1 core schema, so a block-style `short-id: 123456e2`
/// arrives as a float (the REALITY short-id incident, engine.py
/// `_YAML_11_SCALAR`). JSON quoting keeps every scalar a string. The export
/// side has its own quoting problem -- that is engine.py's dumper, not ours.
pub fn build_config(core: &CoreConfig, secret: &str, proxies: &[Value]) -> String {
    let api_port = parse_port(&core.api, 19190);
    let lanes = core.lanes.clamp(1, 32);
    let mut lines: Vec<String> = vec![
        format!("mixed-port: {}", core.mixed_port),
        // allow-lan must be true for bind-address to take effect; with it
        // false the kernel binds the wildcard address (core.py build_config).
        "allow-lan: true".into(),
        "bind-address: 127.0.0.1".into(),
        "mode: rule".into(),
        "log-level: warning".into(),
        "ipv6: true".into(),
        "unified-delay: true".into(),
        "tcp-concurrent: true".into(),
        "find-process-mode: off".into(),
        format!("external-controller: 127.0.0.1:{api_port}"),
        format!("secret: \"{secret}\""),
        "profile:".into(),
        "  store-selected: false".into(),
        "  store-fake-ip: false".into(),
        "dns:".into(),
        "  enable: true".into(),
        "  ipv6: true".into(),
        "  enhanced-mode: fake-ip".into(),
        "  fake-ip-range: 198.18.0.1/16".into(),
        "  nameserver:".into(),
        "    - 223.5.5.5".into(),
        "    - 1.1.1.1".into(),
        "proxies:".into(),
    ];
    for proxy in proxies {
        // Compact JSON, non-ASCII kept literal (Python: ensure_ascii=False).
        let json = serde_json::to_string(proxy).unwrap_or_else(|_| "{}".into());
        lines.push(format!("  - {json}"));
    }
    let names: Vec<String> = proxies
        .iter()
        .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
        .map(str::to_string)
        .collect();
    lines.push("proxy-groups:".into());
    for i in 0..lanes {
        lines.push(format!("  - name: \"{}\"", lane_group(i)));
        lines.push("    type: select".into());
        lines.push("    proxies:".into());
        // Only this lane's own slice. Listing every proxy in every group is
        // what produced a 450KB config (93% of it the same names repeated 16
        // times) and killed every round on hk3 for nine days; see
        // `lanes::lane_members`.
        let members = lane_members(&names, i, lanes);
        if members.is_empty() {
            // mihomo rejects a select group with no members, so a lane that
            // owns nothing still gets a well-formed group.
            lines.push("      - DIRECT".into());
        } else {
            for name in members {
                lines.push(format!("      - {}", quote_name(name)));
            }
        }
    }
    lines.push("listeners:".into());
    for i in 0..lanes {
        lines.push(format!("  - name: \"{}\"", lane_name(i)));
        lines.push("    type: mixed".into());
        lines.push(format!("    port: {}", core.base_port.saturating_add(i)));
        lines.push("    listen: 127.0.0.1".into());
    }
    lines.push("rules:".into());
    for i in 0..lanes {
        lines.push(format!("  - IN-NAME,{},{}", lane_name(i), lane_group(i)));
    }
    lines.push(format!("  - MATCH,{}", lane_group(0)));
    lines.push(String::new());
    lines.join("\n")
}

/// Write the config to `<dir>/config.yaml`, creating the directory. Returns
/// the path (Python: `build_config(out_dir=...)`).
pub fn write_config(
    dir: &Path,
    core: &CoreConfig,
    secret: &str,
    proxies: &[Value],
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("config.yaml");
    std::fs::write(&path, build_config(core, secret, proxies))?;
    Ok(path)
}

/// YAML flow-scalar quoting for group member names: only names that would be
/// misread need quotes, and the kernel's parser accepts the quoted form
/// always. Keep it simple and match Python's `json.dumps(name)` behaviour,
/// which is what core.py actually emits (a JSON string is a valid YAML flow
/// scalar for our identifier-shaped names).
fn quote_name(name: &str) -> String {
    serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into())
}

fn parse_port(api: &str, default: u16) -> u16 {
    api.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

/// A classified failure: `kind` is one of the stable ledger reasons the panel
/// speaks, `message` the kernel's own text (bounded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: String,
    pub message: String,
}

/// Stable failure reasons for a failed delay call, ported from Python
/// `_reason_from` (core.py): the mapping the ledger and the panel speak, and
/// the one string both implementations must agree on per outcome.
pub fn classify_failure(status: u16, body: &str) -> Failure {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("error"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().chars().take(120).collect());
    let low = message.to_lowercase();
    let kind = if low.contains("timeout") || status == 504 {
        "timeout"
    } else if low.contains("delay test") || status == 503 {
        "kernel_error"
    } else if status == 400 {
        "bad_request"
    } else if status == 0 || low.contains("refused") {
        "unreachable"
    } else {
        // Short-lived allocation, owned by the Failure -- deliberately not a
        // leaked 'static str.
        return Failure {
            kind: format!("http_{status}"),
            message,
        };
    };
    Failure {
        kind: kind.into(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn core() -> CoreConfig {
        CoreConfig::defaults()
    }

    #[test]
    fn config_matches_the_python_shape() {
        // Two lanes keep the snapshot readable; CoreConfig::defaults() has
        // eight, and the lane loop is exercised at full width by
        // write_config_creates_the_file.
        let core = CoreConfig {
            lanes: 2,
            ..CoreConfig::defaults()
        };
        let text = build_config(
            &core,
            "s3cret",
            &[json!({"name": "n1", "type": "socks5", "server": "203.0.113.10", "port": 1080})],
        );
        let expected = "\
mixed-port: 19194
allow-lan: true
bind-address: 127.0.0.1
mode: rule
log-level: warning
ipv6: true
unified-delay: true
tcp-concurrent: true
find-process-mode: off
external-controller: 127.0.0.1:19190
secret: \"s3cret\"
profile:
  store-selected: false
  store-fake-ip: false
dns:
  enable: true
  ipv6: true
  enhanced-mode: fake-ip
  fake-ip-range: 198.18.0.1/16
  nameserver:
    - 223.5.5.5
    - 1.1.1.1
proxies:
  - {\"name\":\"n1\",\"type\":\"socks5\",\"server\":\"203.0.113.10\",\"port\":1080}
proxy-groups:
  - name: \"__LANE0__\"
    type: select
    proxies:
      - \"n1\"
  - name: \"__LANE1__\"
    type: select
    proxies:
      - DIRECT
listeners:
  - name: \"lane0\"
    type: mixed
    port: 19200
    listen: 127.0.0.1
  - name: \"lane1\"
    type: mixed
    port: 19201
    listen: 127.0.0.1
rules:
  - IN-NAME,lane0,__LANE0__
  - IN-NAME,lane1,__LANE1__
  - MATCH,__LANE0__
";
        assert_eq!(text, expected, "shadow runs diff this file byte for byte");
    }

    #[test]
    fn an_empty_proxy_list_falls_back_to_direct_members() {
        let text = build_config(&core(), "s", &[]);
        assert!(
            text.contains("      - DIRECT"),
            "kernel refuses an empty group"
        );
    }

    #[test]
    fn failure_classification_matches_python_reasons() {
        assert_eq!(
            classify_failure(504, "{\"message\":\"Timeout\"}"),
            Failure {
                kind: "timeout".into(),
                message: "Timeout".into()
            }
        );
        assert_eq!(
            classify_failure(503, "{\"message\":\"An error occurred in the delay test\"}"),
            Failure {
                kind: "kernel_error".into(),
                message: "An error occurred in the delay test".into()
            }
        );
        assert_eq!(
            classify_failure(400, "{\"message\":\"bad\"}").kind,
            "bad_request"
        );
        assert_eq!(
            classify_failure(418, "{\"message\":\"teapot\"}").kind,
            "http_418"
        );
        assert_eq!(
            classify_failure(500, "not json at all").message,
            "not json at all"
        );
        // A controller that cannot be reached at all is its own class (the
        // Python `controller_error`), never "the node is dead".
        assert_eq!(
            classify_failure(0, "connection refused").kind,
            "unreachable"
        );
    }

    #[test]
    fn write_config_creates_the_file() {
        let tmp = std::env::temp_dir().join(format!("probe-mihomo-test-{}", std::process::id()));
        let path = write_config(&tmp, &core(), "s", &[]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("mixed-port: 19194"));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
