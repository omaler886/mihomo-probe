//! `engine.collect_entries`: flatten fetched sources into testable entries.
//!
//! The one thing that must happen *here* and not later is the category: it is
//! computed while the source's `relay` flag is still in hand. `prepare` only
//! receives proxies, so a category derived afterwards would have lost the
//! source-level signal and could not tell a marked transit hop from an ordinary
//! node.

use serde_json::Value;

use probe_config::SourceSpec;

use crate::identity::fingerprint_proxy;
use crate::prepare::{RawEntry, Role};

pub const CAT_DIRECT: &str = "direct";
pub const CAT_RELAY: &str = "relay";
pub const CAT_CHAIN: &str = "chain";

/// What one enabled source yielded.
#[derive(Debug, Clone)]
pub struct FetchedSource {
    pub key: String,
    pub proxies: Vec<Value>,
    pub relay: bool,
}

impl FetchedSource {
    pub fn from_spec(spec: &SourceSpec, proxies: Vec<Value>) -> Self {
        Self {
            key: spec.key.clone(),
            proxies,
            relay: spec.relay,
        }
    }
}

/// `engine.classify_category`: direct, relay, or chain.
///
/// Precedence matters: a node that is both a marked transit hop and carries
/// `dialer-proxy` is a **relay**, because that is the role it plays in the
/// chain being measured. Reporting it as a chain would double-count it and make
/// the chain total disagree with the front pool.
///
/// `dialer-proxy` is checked in both spellings because mihomo normalises `_` to
/// `-` and upstream subscriptions use both.
pub fn classify_category(proxy: &Value, source_relay: bool) -> &'static str {
    if source_relay {
        return CAT_RELAY;
    }
    let has_dialer = proxy.get("dialer-proxy").is_some() || proxy.get("dialer_proxy").is_some();
    if has_dialer {
        CAT_CHAIN
    } else {
        CAT_DIRECT
    }
}

/// Flatten every fetched source into entries, assigning the ledger identity.
///
/// This is the ONLY place a fingerprint is derived, mirroring `engine._orig_fp`.
/// Everything downstream reuses the `fingerprint` carried on the entry, because
/// recomputation would happen on a proxy `prepare` has already transformed.
pub fn collect_entries(sources: &[FetchedSource]) -> Vec<RawEntry> {
    let mut entries = Vec::new();
    let mut index = 0usize;
    for source in sources {
        for proxy in &source.proxies {
            // Python: `str(proxy.get("name") or f"node-{index}")` -- a falsy
            // name falls back to the position.
            let name = proxy
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("node-{index}"));
            entries.push(RawEntry {
                source: source.key.clone(),
                index,
                name,
                proxy: proxy.clone(),
                fingerprint: fingerprint_proxy(proxy),
                category: classify_category(proxy, source.relay).to_string(),
                role: Role::default(),
                front: None,
            });
            index += 1;
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fetched(key: &str, relay: bool, proxies: Vec<Value>) -> FetchedSource {
        FetchedSource {
            key: key.to_string(),
            proxies,
            relay,
        }
    }

    fn ss(name: &str, server: &str) -> Value {
        json!({"name": name, "type": "ss", "server": server, "port": 8388,
               "cipher": "aes-128-gcm", "password": "pw"})
    }

    #[test]
    fn entries_carry_their_source_and_a_global_index() {
        let sources = vec![
            fetched("air", false, vec![ss("a", "1.1.1.1"), ss("b", "2.2.2.2")]),
            fetched("backup", false, vec![ss("c", "3.3.3.3")]),
        ];
        let entries = collect_entries(&sources);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries
                .iter()
                .map(|e| e.source.as_str())
                .collect::<Vec<_>>(),
            vec!["air", "air", "backup"]
        );
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "the index is global, not per source -- it names unnamed nodes"
        );
    }

    #[test]
    fn a_relay_source_marks_all_its_nodes() {
        let sources = vec![fetched("transit", true, vec![ss("a", "1.1.1.1")])];
        let entries = collect_entries(&sources);
        assert_eq!(entries[0].category, CAT_RELAY);
    }

    #[test]
    fn a_dialer_proxy_makes_a_node_a_chain() {
        let mut proxy = ss("c", "1.1.1.1");
        proxy["dialer-proxy"] = json!("front");
        let entries = collect_entries(&[fetched("air", false, vec![proxy])]);
        assert_eq!(entries[0].category, CAT_CHAIN);
    }

    #[test]
    fn the_underscore_spelling_counts_too() {
        // mihomo normalises `_` to `-`; upstream uses both spellings.
        let mut proxy = ss("c", "1.1.1.1");
        proxy["dialer_proxy"] = json!("front");
        let entries = collect_entries(&[fetched("air", false, vec![proxy])]);
        assert_eq!(entries[0].category, CAT_CHAIN);
    }

    #[test]
    fn relay_outranks_chain() {
        // A marked transit hop carrying a dialer is a relay: reporting it as a
        // chain would double-count it against the front pool.
        let mut proxy = ss("c", "1.1.1.1");
        proxy["dialer-proxy"] = json!("front");
        let entries = collect_entries(&[fetched("transit", true, vec![proxy])]);
        assert_eq!(entries[0].category, CAT_RELAY);
    }

    #[test]
    fn a_plain_node_is_direct() {
        let entries = collect_entries(&[fetched("air", false, vec![ss("a", "1.1.1.1")])]);
        assert_eq!(entries[0].category, CAT_DIRECT);
    }

    #[test]
    fn an_unnamed_node_falls_back_to_its_index() {
        let mut proxy = ss("a", "1.1.1.1");
        proxy.as_object_mut().unwrap().remove("name");
        let entries = collect_entries(&[fetched("air", false, vec![proxy])]);
        assert_eq!(entries[0].name, "node-0");
    }

    #[test]
    fn the_identity_is_the_original_proxy_not_the_entry() {
        let proxy = ss("a", "1.1.1.1");
        let expected = crate::identity::fingerprint_proxy(&proxy);
        let entries = collect_entries(&[fetched("air", false, vec![proxy])]);
        assert_eq!(entries[0].fingerprint, expected);
        assert_eq!(entries[0].fingerprint.len(), 16);
    }
}
