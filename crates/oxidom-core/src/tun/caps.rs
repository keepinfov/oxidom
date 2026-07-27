//! Linux capability checks for interface operations.

/// CAP_NET_ADMIN.
const CAP_NET_ADMIN: u32 = 12;

/// Parse the effective capability mask from `/proc/<pid>/status`.
pub fn parse_cap_eff(status: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        let value = line.strip_prefix("CapEff:")?.trim();
        u64::from_str_radix(value, 16).ok()
    })
}

pub fn has_net_admin() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| parse_cap_eff(&status))
        .is_some_and(|mask| mask & (1_u64 << CAP_NET_ADMIN) != 0)
}

/// One refusal shared by every caller; oxidom never elevates itself.
pub fn missing_capability_error(profile: &str) -> String {
    format!(
        "profile `{profile}` asks for a network interface, but this daemon has no CAP_NET_ADMIN. \
         Interfaces are only available from the system daemon: enable `services.oxidom.enable` \
         together with `services.oxidom.tun.enable`. oxidom will not escalate privileges on its \
         own."
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn cap_eff_is_parsed_and_net_admin_bit_is_detected() {
        let with = "Name:\toxidom\nCapInh:\t0000000000000000\nCapEff:\t0000000000001000\n";
        let without = "Name:\toxidom\nCapEff:\t0000000000000400\n";
        let mask = super::parse_cap_eff(with).unwrap();
        assert_ne!(mask & (1_u64 << super::CAP_NET_ADMIN), 0);
        let mask = super::parse_cap_eff(without).unwrap();
        assert_eq!(mask & (1_u64 << super::CAP_NET_ADMIN), 0);
    }

    #[test]
    fn missing_or_malformed_cap_eff_is_unknown() {
        assert_eq!(super::parse_cap_eff("Name:\toxidom\n"), None);
        assert_eq!(super::parse_cap_eff("CapEff:\tnot-hex\n"), None);
    }
}
