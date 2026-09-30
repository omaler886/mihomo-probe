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
}
