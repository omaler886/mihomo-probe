use super::*;

mod native;

/// Build synthetic references without requiring a kernel or subscription.
fn proxies(entries: &[(&str, &str)]) -> Vec<ProxyRef> {
    entries
        .iter()
        .map(|(name, server)| ProxyRef {
            name: (*name).into(),
            server: (*server).into(),
        })
        .collect()
}

#[test]
fn quoted_names_take_priority_over_server_mentions() {
    let nodes = proxies(&[("alpha", "203.0.113.10"), ("beta", "203.0.113.11")]);
    assert_eq!(
        culprit_from("proxy [\"beta\"] failed at 203.0.113.10", &nodes),
        Some(1)
    );
}

#[test]
fn short_names_and_identifier_substrings_do_not_match() {
    let nodes = proxies(&[("jp", "198.51.100.1"), ("BageVM-Tokyo", "198.51.100.3")]);
    assert_eq!(
        culprit_from("unrelated line about japan proxies", &nodes),
        None
    );
    assert_eq!(culprit_from("BageVM-Tokyo-2 timed out", &nodes), None);
    assert_eq!(culprit_from("failed for BageVM-Tokyo", &nodes), Some(1));
    assert_eq!(culprit_from("\"jp\" failed", &nodes), Some(0));
}

/// A `.` is a boundary in Python's `(?<![\w-])`/`(?![\w-])` NAME match, so
/// `proxy 'v1.tokyo' is not a valid proxy` prunes a node named `tokyo`
/// instead of blocking the whole round. `_` and `-` stay non-boundaries, and
/// the server tier keeps its stricter glued-address rule.
#[test]
fn a_dot_is_a_name_boundary_like_python() {
    let nodes = proxies(&[("hk", "203.0.113.1"), ("tokyo", "203.0.113.2")]);
    assert_eq!(
        culprit_from("proxy 'v1.tokyo' is not a valid proxy", &nodes),
        Some(1),
        "the dotted prefix must not swallow the match"
    );
    // Parity, not a typo: Python's lookahead also accepts the trailing dot,
    // so `tokyo.example` matches the name `tokyo` on both sides.
    assert_eq!(
        culprit_from("dial tokyo.example:443 refused", &nodes),
        Some(1)
    );
    // `_` and `-` still glue a name together: no match mid-token.
    assert_eq!(culprit_from("dial v1_tokyo-dial:443", &nodes), None);
    // The server tier stays strict: a dotted subdomain does not name its
    // parent, where Python's substring match would prune the wrong node.
    let domains = proxies(&[("alpha", "edge.example.test")]);
    assert_eq!(
        culprit_from("dial sub.edge.example.test:443", &domains),
        None
    );
}

#[test]
fn server_matches_require_address_boundaries() {
    let nodes = proxies(&[("alpha", "198.51.100.9"), ("beta", "edge.example.test")]);
    assert_eq!(
        culprit_from("dial 198.51.100.9:443 refused", &nodes),
        Some(0)
    );
    for output in [
        "dial 198.51.100.90:443",
        "dial 1198.51.100.9:443",
        "dial sub.edge.example.test:443",
        "dial edge.example.test.evil:443",
    ] {
        assert_eq!(culprit_from(output, &nodes), None, "{output}");
    }
    assert_eq!(culprit_from("dial EDGE.EXAMPLE.TEST:443", &nodes), Some(1));
    let nodes = proxies(&[("alpha", "2001:db8::1")]);
    assert_eq!(culprit_from("dial [2001:db8::1]:443", &nodes), Some(0));
    assert_eq!(culprit_from("dial [2001:db8::1:2]:443", &nodes), None);
}

#[test]
fn ambiguous_names_servers_and_cross_tier_mentions_never_prune() {
    let cases = [
        (
            vec![("same", "one.test"), ("same", "two.test")],
            "\"same\" failed",
        ),
        (
            vec![("Alpha", "one.test"), ("alpha", "two.test")],
            "ALPHA failed",
        ),
        (
            vec![("alpha", "same.test"), ("beta", "same.test")],
            "same.test failed",
        ),
        (
            vec![("alpha", "one.test"), ("beta", "two.test")],
            "\"alpha\" and \"beta\"",
        ),
        (
            vec![("alpha", "one.test"), ("beta", "two.test")],
            "alpha failed at two.test",
        ),
        (
            vec![("alpha", "one.test"), ("beta", "alpha.example.test")],
            "alpha.example.test failed",
        ),
    ];
    for (entries, output) in &cases[..5] {
        assert_eq!(culprit_from(output, &proxies(entries)), None);
    }
    // With `.` a name boundary (Python parity, 2026-10-21), `alpha` now
    // word-matches the alpha-NAMED node while `alpha.example.test` still
    // address-matches beta's server -- two candidates in one tier, so the
    // merged-tier ambiguity rule refuses to prune and the round blocks.
    // That stays the fail-safe direction: Python would guess proxy 0 (its
    // name tier answers first); refusing to guess prunes no innocent node.
    // The simple `v1.tokyo` shape, where only ONE node can be meant, is the
    // case the boundary fix restores -- see
    // `a_dot_is_a_name_boundary_like_python`.
    assert_eq!(culprit_from(cases[5].1, &proxies(&cases[5].0)), None);
}

#[test]
fn unicode_boundaries_and_case_expansion_are_safe() {
    let node_name = "\u{4e1c}\u{4eac}\u{8282}\u{70b9}";
    let nodes = proxies(&[(node_name, "one.test")]);
    // First occurrence is not bounded; advancing one byte used to panic here.
    let output = format!("x{node_name}-suffix; failed {node_name}");
    assert_eq!(culprit_from(&output, &nodes), Some(0));
    assert_eq!(culprit_from(&format!("{node_name}\u{7532}"), &nodes), None);
    assert!(find_word(
        "x\u{130}stanbul; \u{130}STANBUL",
        "\u{130}stanbul",
        false
    ));
    assert!(!find_word("cafe\u{301}", "cafe", false));
    assert!(!find_word("\u{301}cafe", "cafe", false));
    assert!(!find_word("anything", "", false));
    assert_eq!(culprit_from("", &nodes), None);
    assert_eq!(culprit_from("anything", &[]), None);
}

#[test]
fn truncated_output_cannot_authorize_pruning() {
    let nodes = proxies(&[("alpha", "one.test")]);
    assert_eq!(culprit_from(&format!("\"alpha\"{TRUNCATED}"), &nodes), None);
    let text = merge_output(
        (vec![0xff; MAX_PIPE_BYTES], false),
        (vec![0xff; MAX_PIPE_BYTES], false),
    );
    assert!(text.len() <= MAX_OUTPUT_BYTES);
    assert!(text.ends_with(TRUNCATED));
}

#[test]
fn verdict_debug_does_not_expose_sensitive_diagnostics() {
    for check in [
        ConfigCheck::Passed("private-marker".into()),
        ConfigCheck::Failed("private-marker".into()),
        ConfigCheck::Degraded("private-marker".into()),
    ] {
        assert!(!format!("{check:?}").contains("private-marker"));
    }
    assert!(!ConfigCheck::Degraded("no verdict".into()).is_ok());
    assert!(!ConfigCheck::Failed("rejected".into()).is_ok());
    assert!(ConfigCheck::Passed("accepted".into()).is_ok());
}

#[test]
fn unavailable_inputs_degrade_without_probing_docker() {
    let fixture = native::Fixture::empty();
    let missing = fixture.directory.join("missing");
    assert!(matches!(validate(&missing), ConfigCheck::Degraded(_)));
    assert!(matches!(
        validate(&fixture.directory),
        ConfigCheck::Degraded(_)
    ));
    assert!(matches!(
        validate_with_binary(&missing, &missing, KERNEL_TIMEOUT),
        ConfigCheck::Degraded(_)
    ));
    assert_eq!(VALIDATOR_IMAGE, "metacubex/mihomo:v1.19.32");
    assert_eq!(DOCKER_PROBE_TIMEOUT, Duration::from_secs(3));
}

#[cfg(unix)]
#[test]
fn config_symlink_escape_degrades_before_environment_or_docker_lookup() {
    let fixture = native::Fixture::empty();
    let inner = fixture.directory.join("inner");
    std::fs::create_dir(&inner).unwrap();
    let outside = fixture.directory.join("outside.yaml");
    std::fs::write(&outside, "pass").unwrap();
    std::os::unix::fs::symlink(outside, inner.join("config.yaml")).unwrap();
    assert!(matches!(validate(&inner), ConfigCheck::Degraded(_)));
}
