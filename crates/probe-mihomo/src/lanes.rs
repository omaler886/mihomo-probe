//! Verification lanes: one select group plus one loopback inbound per lane,
//! pinned by an IN-NAME rule (GLM_5.3_Flash §5; Python core.py lanes).
//!
//! Lanes are what makes egress verification concurrent: the kernel's selector
//! is global state, so a single group serialises the whole phase. Each lane
//! owns its own group + port and they run independently (verified on the
//! Python side: swapping two lanes' selections swaps their reported exits).

use probe_config::CoreConfig;

pub fn lane_group(i: u16) -> String {
    format!("__LANE{i}__")
}

pub fn lane_name(i: u16) -> String {
    format!("lane{i}")
}

/// How many lanes the kernel config builds (Python `lane_count`): clamped to
/// 1..=32, falling back to 8 on unusable config values.
pub fn lane_count(core: &CoreConfig) -> u16 {
    core.lanes.clamp(1, 32)
}

/// Loopback port of each lane's inbound listener (Python `lane_ports`):
/// base_port .. base_port+count-1.
pub fn lane_ports(core: &CoreConfig) -> Vec<u16> {
    let count = lane_count(core);
    (0..count)
        .map(|i| core.base_port.saturating_add(i))
        .collect()
}

/// The proxies one lane's select group lists: every `lanes`-th name starting
/// at `lane` -- Python's `names[lane::lanes]`.
///
/// Since 2026-10-08 `core.build_config` splits the proxies across the lane
/// groups instead of listing all of them in every group. The old layout wrote
/// 486 nodes x 16 lanes = 7776 group members into a 450KB config whose
/// `PUT /configs?force=true` outlived the controller's 20s HTTP timeout, so
/// every scheduled round on hk3 aborted before testing a single node -- 287
/// rounds over nine days.
///
/// The split is load-bearing on both sides: `core.select` succeeds only on the
/// lane whose group contains the node, so whatever buckets the verification
/// pass must use exactly this function's answer.
pub fn lane_members(names: &[String], lane: u16, lanes: u16) -> Vec<&String> {
    let step = lanes.max(1) as usize;
    names.iter().skip(lane as usize).step_by(step).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(lanes: u16, base: u16) -> CoreConfig {
        CoreConfig {
            lanes,
            base_port: base,
            ..CoreConfig::defaults()
        }
    }

    #[test]
    fn ports_cover_the_lane_range() {
        let ports = lane_ports(&core(4, 19200));
        assert_eq!(ports, vec![19200, 19201, 19202, 19203]);
    }

    #[test]
    fn lane_count_clamps_extremes() {
        assert_eq!(lane_count(&core(0, 19200)), 1);
        assert_eq!(lane_count(&core(99, 19200)), 32);
        assert_eq!(lane_count(&core(8, 19200)), 8);
    }

    #[test]
    fn group_and_listener_names_match_python() {
        // The kernel config and the IN-NAME rules must name the same groups
        // the Python side did, or a shadow diff is unreadable.
        assert_eq!(lane_group(0), "__LANE0__");
        assert_eq!(lane_name(0), "lane0");
        assert_eq!(lane_group(7), "__LANE7__");
    }

    #[test]
    fn lane_members_partition_the_names() {
        // Same example as the Python test
        // `test_every_proxy_lands_in_exactly_one_lane_group`.
        let names: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(lane_members(&names, 0, 3), vec![&names[0], &names[3]]);
        assert_eq!(lane_members(&names, 1, 3), vec![&names[1]]);
        assert_eq!(lane_members(&names, 2, 3), vec![&names[2]]);

        // The invariant that has to hold: a partition. A node missing from its
        // lane loses its exit check; a node in two lanes means the config grew
        // back to the shape that caused the outage.
        let mut seen: Vec<&String> = (0..3).flat_map(|i| lane_members(&names, i, 3)).collect();
        seen.sort();
        assert_eq!(seen, vec![&names[0], &names[1], &names[2], &names[3]]);
    }

    #[test]
    fn lane_members_handles_edge_shapes() {
        let names: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        // fewer names than lanes: the trailing lanes are empty, and a lane
        // never receives a name it should not have
        assert!(lane_members(&names, 2, 8).is_empty());
        assert_eq!(lane_members(&names, 0, 8), vec![&names[0]]);
        assert_eq!(lane_members(&names, 1, 8), vec![&names[1]]);
        // a single lane owns everything
        assert_eq!(lane_members(&names, 0, 1).len(), 2);
        // no names at all
        let empty: Vec<String> = Vec::new();
        assert!(lane_members(&empty, 0, 4).is_empty());
    }
}
