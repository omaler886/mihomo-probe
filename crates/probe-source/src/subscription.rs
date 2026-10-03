//! Subscription text in, proxy dicts out.
//!
//! Port of `store._parse_proxies`. Sub-Store's `download` route answers with a
//! Clash document (`proxies:` at the top level), and the only tolerance worth
//! keeping is the one Python has: entries that are not mappings are skipped
//! rather than fatal, because one malformed node must not cost the whole
//! round.
//!
//! ## YAML dialect caveat
//!
//! Python parses this with PyYAML, which implements YAML **1.1**; `serde_yaml`
//! implements **1.2**. The dialects disagree about bare scalars such as
//! `123456e2` (1.1: float; 1.2: string) and `yes`/`on` (1.1: bool; 1.2:
//! string). In practice this does not bite, because Sub-Store generates the
//! ClashMeta output itself and quotes exactly the scalars that would otherwise
//! change type -- the same YAML-1.1 lookalike problem this project documents on
//! its own *output* side (`YamlScalarQuotingTest`). An upstream that emits an
//! unquoted `short-id: 123456e2` would still be parsed differently here; that
//! is recorded as a known divergence in workstreams/13 rather than papered
//! over.

use probe_domain::{DomainError, DomainResult};
use serde_json::Value;

/// Parse a Clash document and return its `proxies` entries.
pub fn parse_proxies(text: &str) -> DomainResult<Vec<Value>> {
    let parsed: Value = serde_yaml::from_str(text)
        .map_err(|err| DomainError::Config(format!("subscription is not valid YAML: {err}")))?;
    // Python: `parsed.get("proxies")` where `parsed` may be any shape; a
    // non-mapping document has no `proxies` and falls through to the same
    // error.
    let Some(proxies) = parsed.get("proxies").and_then(|v| v.as_array()) else {
        return Err(DomainError::Config(
            "subscription returned no proxies list".into(),
        ));
    };
    Ok(proxies.iter().filter(|p| p.is_object()).cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_clash_document_yields_its_proxies() {
        let text = r#"
proxies:
  - name: a
    type: ss
    server: 1.2.3.4
    port: 8388
    cipher: aes-128-gcm
    password: pw
  - name: b
    type: vmess
    server: example.com
    port: "443"
    uuid: "1111"
port: 7890
mode: rule
"#;
        let proxies = parse_proxies(text).unwrap();
        assert_eq!(proxies.len(), 2);
        assert_eq!(proxies[0]["name"], json!("a"));
        assert_eq!(proxies[0]["port"], json!(8388), "YAML int stays an int");
        assert_eq!(
            proxies[1]["port"],
            json!("443"),
            "a quoted port stays a string -- prepare coerces it later"
        );
    }

    #[test]
    fn a_document_without_a_proxies_list_is_an_error() {
        let err = parse_proxies("port: 7890\n").unwrap_err();
        assert!(err.to_string().contains("no proxies list"), "{err}");
        let err = parse_proxies("").unwrap_err();
        assert!(err.to_string().contains("no proxies list"), "{err}");
    }

    #[test]
    fn invalid_yaml_is_an_error_not_a_panic() {
        let err = parse_proxies("proxies: [oops\n").unwrap_err();
        assert!(err.to_string().contains("not valid YAML"), "{err}");
    }

    #[test]
    fn non_mapping_entries_are_skipped() {
        // One bad node must not cost the whole subscription.
        let text = "proxies:\n  - name: a\n    type: ss\n    server: s\n    port: 1\n  - just-a-string\n  - 42\n";
        let proxies = parse_proxies(text).unwrap();
        assert_eq!(proxies.len(), 1);
        assert_eq!(proxies[0]["name"], json!("a"));
    }

    #[test]
    fn an_empty_proxies_list_is_not_an_error() {
        // A source that is genuinely empty is a legitimate answer; the round
        // decides what to do about it (Python: demote, never kill).
        assert!(parse_proxies("proxies: []\n").unwrap().is_empty());
    }
}
