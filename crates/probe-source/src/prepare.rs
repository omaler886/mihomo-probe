//! `core.prepare`: turn raw subscription entries into kernel-ready proxies.
//!
//! Three things happen here, and all three are load-bearing:
//!
//! * **Names are made unique.** mihomo keys proxies by name and silently
//!   collapses duplicates, so a subscription with two `BageVM` nodes loses one
//!   without a word.
//! * **Fields are stripped** (`DROP_FIELDS`), because a `dialer-proxy` naming
//!   something the config does not contain is a config error, not a test
//!   result. A chained node keeps it, or the round tests it direct and reports
//!   a node alive on a path its owner never uses.
//! * **The ledger identity is carried, never recomputed.** `prepare` changes
//!   the proxy it returns -- stripped fields, coerced port, renamed -- so
//!   hashing that copy yields a *different* identity for the same node. Python
//!   learned this the hard way (`engine._orig_fp`); the fingerprint therefore
//!   travels on the entry.

use std::collections::HashMap;

use serde_json::Value;

use crate::identity::{DROP_FIELDS, REQUIRED};

/// One node as it came off a subscription, before `prepare` touched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    pub source: String,
    /// Position in the flattened source list -- the fallback name for an
    /// entry whose own name is blank.
    pub index: usize,
    pub name: String,
    pub proxy: Value,
    /// The ledger identity, computed from the *original* proxy by
    /// `collect_entries`. Never recompute this from `proxy`.
    pub fingerprint: String,
    /// `direct` / `relay` / `chain`.
    pub category: String,
    /// What the node does in this round (Python's `entry["role"]`): a front
    /// is a dialer tested first, a chain variant is dialled through one
    /// front, everything else is direct. Defaults to [`Role::Direct`]; only
    /// the front pool and `expand_chains` produce the other two.
    pub role: Role,
    /// For a [`Role::Chain`] variant: the front it dials through, by kernel
    /// name. `None` on a chain entry with an empty pool means "no front was
    /// left" -- the runner fails it `front_dead` without dialling.
    pub front: Option<String>,
}

/// The part a node plays in one round. Distinct from `category`: a relay not
/// in the front pool is *tested* like any direct node (`Role::Direct`) while
/// its numbers are still reported under 中转节点 -- `category` says what kind
/// of node it is, `role` says what it does in this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    /// Tested as its own server.
    #[default]
    Direct,
    /// A front-pool dialer; tested in the first phase, before the chains.
    Front,
    /// A chain variant, dialled through the front in `front`.
    Chain,
}

/// A node the kernel can be given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedNode {
    pub source: String,
    /// The name upstream used; the export and the panel show this.
    pub original_name: String,
    /// The unique name the kernel sees.
    pub proxy_name: String,
    /// The stripped, renamed proxy to write into the config.
    pub proxy: Value,
    pub fingerprint: String,
    pub category: String,
    pub server: Option<String>,
    pub proto: Option<String>,
    pub role: Role,
    pub front: Option<String>,
}

/// A node that never reached the kernel, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedNode {
    pub name: Option<String>,
    pub why: String,
}

#[derive(Debug, Clone, Default)]
pub struct Prepared {
    pub nodes: Vec<PreparedNode>,
    pub dropped: Vec<DroppedNode>,
}

/// Port of `core.prepare`.
///
/// `keep_dialer` names the proxies that exist in this config and may therefore
/// serve as a dialer; `strip_ech` removes `ech-opts` for kernels that cannot
/// parse it.
pub fn prepare(entries: &[RawEntry], keep_dialer: &[String], strip_ech: bool) -> Prepared {
    let mut out = Prepared::default();
    let mut seen: HashMap<String, usize> = HashMap::new();

    for entry in entries {
        let mut proxy = serde_json::Map::new();
        if let Some(map) = entry.proxy.as_object() {
            for (key, value) in map {
                if key == "dialer-proxy" {
                    // Kept only when the named dialer is really in this config.
                    let keep = value
                        .as_str()
                        .map(|name| keep_dialer.iter().any(|d| d == name))
                        .unwrap_or(false);
                    if keep {
                        proxy.insert(key.clone(), value.clone());
                    }
                    continue;
                }
                if !DROP_FIELDS.contains(&key.as_str()) {
                    proxy.insert(key.clone(), value.clone());
                }
            }
        }
        if strip_ech {
            proxy.remove("ech-opts");
        }

        // Python: `proxy.get(f) in (None, "")`. Note `0` and `false` are NOT
        // missing by that test -- see the port handling below.
        let missing: Vec<&str> = REQUIRED
            .iter()
            .filter(|field| match proxy.get(**field) {
                None => true,
                Some(value) => value.is_null() || value.as_str() == Some(""),
            })
            .copied()
            .collect();
        if !missing.is_empty() {
            out.dropped.push(DroppedNode {
                name: Some(entry.name.clone()),
                why: format!("missing {}", missing.join(",")),
            });
            continue;
        }

        match coerce_port(proxy.get("port")) {
            Some(port) => {
                proxy.insert("port".to_string(), Value::from(port));
            }
            None => {
                out.dropped.push(DroppedNode {
                    name: Some(entry.name.clone()),
                    why: "bad port".into(),
                });
                continue;
            }
        }

        let base = name_text(proxy.get("name").unwrap_or(&Value::Null));
        let base = if base.is_empty() {
            format!("node-{}", entry.index)
        } else {
            base
        };
        let name = unique_name(&mut seen, &base);
        proxy.insert("name".to_string(), Value::from(name.clone()));

        out.nodes.push(PreparedNode {
            source: entry.source.clone(),
            original_name: entry.name.clone(),
            proxy_name: name,
            server: proxy
                .get("server")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            proto: proxy
                .get("type")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            proxy: Value::Object(proxy),
            fingerprint: entry.fingerprint.clone(),
            category: entry.category.clone(),
            role: entry.role,
            front: entry.front.clone(),
        });
    }

    out
}

/// `int(proxy["port"])` -- an integer literal from a number or a string.
///
/// Returns `None` where Python raises `ValueError`/`TypeError`, which `prepare`
/// turns into a `bad port` drop.
///
/// **Deliberate divergence:** Python's `isinstance(port, int)` is true for
/// `True`/`False` (bool is a subclass of int), so `port: true` skips coercion
/// there and is handed to the kernel, which rejects the config. This drops it
/// instead, with a reason that points at the node rather than at "the kernel
/// refused the config". Same outcome -- the node is not tested -- but the
/// ledger's `dropped` reason differs. Recorded in workstreams/13.
fn coerce_port(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number
            .as_i64()
            // Python `int(443.7)` truncates toward zero.
            .or_else(|| number.as_f64().map(|f| f as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        // bool / array / object / null: Python raises or passes garbage on.
        _ => None,
    }
}

/// Python `str(value).strip()` for the shapes a name realistically takes.
fn name_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        // Python `str(True)` is "True", not "true".
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string().trim().to_string(),
    }
}

/// Python's collision loop, exactly:
///
/// ```text
/// if base in seen: seen[base] += 1; name = f"{base} #{seen[base]}"
///                  while name in seen: seen[base] += 1; name = ...
/// seen[name] = 1
/// ```
fn unique_name(seen: &mut HashMap<String, usize>, base: &str) -> String {
    let mut name = base.to_string();
    if let Some(mut count) = seen.get(base).copied() {
        loop {
            count += 1;
            name = format!("{base} #{count}");
            if !seen.contains_key(&name) {
                break;
            }
        }
        // Python leaves `seen[base]` at the count it stopped on, so the next
        // collision continues from there rather than rescanning.
        seen.insert(base.to_string(), count);
    }
    seen.insert(name.clone(), 1);
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(name: &str, index: usize, proxy: Value) -> RawEntry {
        RawEntry {
            source: "air".into(),
            index,
            name: name.to_string(),
            fingerprint: crate::identity::fingerprint_proxy(&proxy),
            proxy,
            category: "direct".into(),
            role: Role::default(),
            front: None,
        }
    }

    fn ss(name: &str, server: &str, port: Value) -> Value {
        json!({"name": name, "type": "ss", "server": server, "port": port,
               "cipher": "aes-128-gcm", "password": "pw"})
    }

    #[test]
    fn duplicate_names_are_made_unique_the_python_way() {
        // Two BageVM nodes is the real case that motivated this: mihomo keys
        // by name and silently dropped one.
        let entries = vec![
            entry("BageVM", 0, ss("BageVM", "1.1.1.1", json!(1))),
            entry("BageVM", 1, ss("BageVM", "2.2.2.2", json!(2))),
            entry("BageVM", 2, ss("BageVM", "3.3.3.3", json!(3))),
        ];
        let prepared = prepare(&entries, &[], false);
        let names: Vec<&str> = prepared
            .nodes
            .iter()
            .map(|n| n.proxy_name.as_str())
            .collect();
        assert_eq!(names, vec!["BageVM", "BageVM #2", "BageVM #3"]);
        assert!(prepared.dropped.is_empty());
        // The upstream names survive for the export.
        assert!(prepared.nodes.iter().all(|n| n.original_name == "BageVM"));
    }

    #[test]
    fn a_derived_name_does_not_collide_with_a_later_upstream_name() {
        // "x #2" exists upstream AND is generated for the second "x".
        let entries = vec![
            entry("x", 0, ss("x", "1.1.1.1", json!(1))),
            entry("x", 1, ss("x", "2.2.2.2", json!(2))),
            entry("x #2", 2, ss("x #2", "3.3.3.3", json!(3))),
        ];
        let prepared = prepare(&entries, &[], false);
        let names: Vec<&str> = prepared
            .nodes
            .iter()
            .map(|n| n.proxy_name.as_str())
            .collect();
        assert_eq!(names, vec!["x", "x #2", "x #2 #2"]);
    }

    #[test]
    fn dropped_fields_are_stripped_from_the_kernel_proxy() {
        let mut proxy = ss("c", "5.6.7.8", json!(1));
        proxy["interface-name"] = json!("eth0");
        proxy["routing-mark"] = json!(7);
        proxy["dialer-proxy"] = json!("front");
        let prepared = prepare(&[entry("c", 0, proxy)], &[], false);
        let node = &prepared.nodes[0];
        assert!(node.proxy.get("interface-name").is_none());
        assert!(node.proxy.get("routing-mark").is_none());
        assert!(
            node.proxy.get("dialer-proxy").is_none(),
            "a dialer that is not in this config must be stripped"
        );
    }

    #[test]
    fn a_dialer_present_in_the_config_survives() {
        let mut proxy = ss("chained", "5.6.7.8", json!(1));
        proxy["dialer-proxy"] = json!("front-a");
        let keep = vec!["front-a".to_string()];
        let prepared = prepare(&[entry("chained", 0, proxy)], &keep, false);
        assert_eq!(
            prepared.nodes[0].proxy.get("dialer-proxy"),
            Some(&json!("front-a"))
        );
    }

    #[test]
    fn strip_ech_removes_ech_opts() {
        let mut proxy = ss("e", "5.6.7.8", json!(1));
        proxy["ech-opts"] = json!({"enable": true});
        let kept = prepare(&[entry("e", 0, proxy.clone())], &[], false);
        assert!(kept.nodes[0].proxy.get("ech-opts").is_some());
        let stripped = prepare(&[entry("e", 0, proxy)], &[], true);
        assert!(stripped.nodes[0].proxy.get("ech-opts").is_none());
    }

    #[test]
    fn a_missing_required_field_drops_the_node_with_a_reason() {
        let proxy = json!({"name": "no-server", "type": "ss", "port": 1});
        let prepared = prepare(&[entry("no-server", 0, proxy)], &[], false);
        assert!(prepared.nodes.is_empty());
        assert_eq!(prepared.dropped.len(), 1);
        assert_eq!(prepared.dropped[0].why, "missing server");
        assert_eq!(prepared.dropped[0].name.as_deref(), Some("no-server"));
    }

    #[test]
    fn an_empty_string_counts_as_missing() {
        let proxy = json!({"name": "blank", "type": "ss", "server": "", "port": 1});
        let prepared = prepare(&[entry("blank", 0, proxy)], &[], false);
        assert_eq!(prepared.dropped[0].why, "missing server");
    }

    #[test]
    fn a_string_port_is_coerced_to_a_number() {
        let prepared = prepare(
            &[entry("b", 0, ss("b", "example.com", json!("443")))],
            &[],
            false,
        );
        assert_eq!(prepared.nodes[0].proxy.get("port"), Some(&json!(443)));
        assert!(prepared.nodes[0].proxy["port"].is_i64());
    }

    #[test]
    fn a_float_port_truncates_like_python_int() {
        let prepared = prepare(
            &[entry("b", 0, ss("b", "example.com", json!(443.7)))],
            &[],
            false,
        );
        assert_eq!(prepared.nodes[0].proxy.get("port"), Some(&json!(443)));
    }

    #[test]
    fn a_non_numeric_port_drops_the_node() {
        let prepared = prepare(
            &[entry("b", 0, ss("b", "example.com", json!("abc")))],
            &[],
            false,
        );
        assert!(prepared.nodes.is_empty());
        assert_eq!(prepared.dropped[0].why, "bad port");
    }

    #[test]
    fn a_boolean_port_is_dropped_rather_than_handed_to_the_kernel() {
        // Deliberate divergence from Python, which treats bool as int and lets
        // the kernel reject the config instead. Documented above.
        let prepared = prepare(
            &[entry("b", 0, ss("b", "example.com", json!(true)))],
            &[],
            false,
        );
        assert!(prepared.nodes.is_empty());
        assert_eq!(prepared.dropped[0].why, "bad port");
    }

    #[test]
    fn a_blank_name_falls_back_to_the_entry_index() {
        let proxy = json!({"name": "   ", "type": "ss", "server": "1.1.1.1", "port": 1});
        let prepared = prepare(&[entry("   ", 7, proxy)], &[], false);
        assert_eq!(prepared.nodes[0].proxy_name, "node-7");
    }

    #[test]
    fn the_fingerprint_is_the_one_carried_in_not_a_rehash() {
        // The stripped/renamed copy hashes differently; using it would re-key
        // the node every round. This is the `engine._orig_fp` lesson.
        let original = ss("orig", "1.2.3.4", json!("443"));
        let carried = crate::identity::fingerprint_proxy(&original);
        let prepared = prepare(&[entry("orig", 0, original.clone())], &[], false);
        assert_eq!(prepared.nodes[0].fingerprint, carried);
        assert_ne!(
            crate::identity::fingerprint_proxy(&prepared.nodes[0].proxy),
            carried,
            "the transformed copy must NOT reproduce the identity -- that is \
             exactly why the entry carries it"
        );
    }
}
