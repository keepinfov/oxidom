//! Stable loopback addresses for independently running profiles.

use std::collections::HashSet;
use std::net::Ipv4Addr;

use anyhow::{Result, bail};

use crate::model::stable_hash;

const FIRST_OCTET_COUNT: usize = 254;
const SECOND_OCTET_COUNT: usize = 256;
const PROFILE_ADDRESS_COUNT: usize = FIRST_OCTET_COUNT * SECOND_OCTET_COUNT;
const DEVICE_PREFIX: &str = "oxi-";
const DEVICE_NAME_MAX: usize = 15;
const DEVICE_LAST_OCTET_COUNT: usize = 254;
const DEVICE_ADDRESS_COUNT: usize = 256 * DEVICE_LAST_OCTET_COUNT;
const ROUTING_MARK_DEFAULT: u32 = 0x6f00;
const ROUTING_MARK_FIRST: u32 = 0x6f01;
const ROUTING_MARK_LAST: u32 = 0x6fff;

/// Stable inbound address for a profile.
///
/// `default` always uses 127.0.0.1: external consumers (including the user's
/// system redsocks configuration) point there, so moving it would break
/// networking outside oxidom.
pub fn address_for(profile: &str, taken: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    if profile == "default" {
        return Some(Ipv4Addr::LOCALHOST);
    }

    let hash = stable_hash(profile);
    let first = 1 + (hash % FIRST_OCTET_COUNT as u64) as usize;
    let second = ((hash >> 8) % SECOND_OCTET_COUNT as u64) as usize;
    let start = (first - 1) * SECOND_OCTET_COUNT + second;
    let taken: HashSet<Ipv4Addr> = taken.iter().copied().collect();

    (0..PROFILE_ADDRESS_COUNT).find_map(|offset| {
        let index = (start + offset) % PROFILE_ADDRESS_COUNT;
        let first = 1 + index / SECOND_OCTET_COUNT;
        let second = index % SECOND_OCTET_COUNT;
        let address = Ipv4Addr::new(127, first as u8, second as u8, 1);
        (!taken.contains(&address)).then_some(address)
    })
}

/// Derive the session device name as `oxi-<profile>`.
///
/// Linux's IFNAMSIZ includes its terminating NUL, leaving 15 bytes total and
/// therefore 11 bytes after the prefix. Longer profile names must use the
/// explicit `[interface] device` key instead of being silently truncated.
pub fn device_name(profile: &str) -> Result<String> {
    let name = format!("{DEVICE_PREFIX}{profile}");
    validate_device_name(&name).map_err(|_| {
        anyhow::anyhow!(
            "profile {profile:?} does not fit in a Linux interface name; set [interface] device \
             to 1-15 ASCII letters, digits, dots, underscores, or hyphens"
        )
    })?;
    Ok(name)
}

/// Validate a user-supplied `[interface] device` name.
pub fn validate_device_name(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    if !(1..=DEVICE_NAME_MAX).contains(&bytes.len()) {
        bail!("[interface] device must be 1-15 bytes; choose a shorter ASCII interface name");
    }
    if matches!(name, "." | "..")
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!(
            "[interface] device may contain only ASCII letters, digits, dots, underscores, and \
             hyphens, and cannot be `.` or `..`"
        );
    }
    Ok(())
}

/// Stable address of the profile's TUN device.
///
/// 198.18.0.0/15 is reserved by RFC 2544 for benchmark networks and is not
/// routed on the Internet; tun2socks uses it in its own examples. Oxidom stays
/// inside 198.18.0.0/16 and skips host octets 0 and 255 so every derived
/// address remains suitable even when consumers later apply subnet semantics.
pub fn device_address_for(profile: &str, taken: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    if profile == "default" {
        return Some(Ipv4Addr::new(198, 18, 0, 1));
    }

    let hash = stable_hash(profile);
    let start = (hash % DEVICE_ADDRESS_COUNT as u64) as usize;
    let taken: HashSet<Ipv4Addr> = taken.iter().copied().collect();
    (0..DEVICE_ADDRESS_COUNT).find_map(|offset| {
        let index = (start + offset) % DEVICE_ADDRESS_COUNT;
        let third = index / DEVICE_LAST_OCTET_COUNT;
        let fourth = 1 + index % DEVICE_LAST_OCTET_COUNT;
        let address = Ipv4Addr::new(198, 18, third as u8, fourth as u8);
        (!taken.contains(&address)).then_some(address)
    })
}

/// Stable fwmark, private routing-table id, and rule priority for a profile.
///
/// 0x6f00..=0x6fff is deliberately above the user's existing 0x1/0x2/0x3
/// marks for route-direct/route-proxy/route-vpn; overlap would break that
/// routing. `0x6f` is ASCII `o`. The same numeric range also avoids Linux's
/// local (255), main (254), and default (253) routing-table ids.
pub fn routing_mark(profile: &str, taken: &[u32]) -> Option<u32> {
    if profile == "default" {
        return Some(ROUTING_MARK_DEFAULT);
    }

    let count = ROUTING_MARK_LAST - ROUTING_MARK_FIRST + 1;
    let start = (stable_hash(profile) % u64::from(count)) as u32;
    let taken: HashSet<u32> = taken.iter().copied().collect();
    (0..count).find_map(|offset| {
        let mark = ROUTING_MARK_FIRST + (start + offset) % count;
        (!taken.contains(&mark)).then_some(mark)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn default_never_moves_even_when_marked_taken() {
        assert_eq!(
            address_for("default", &[Ipv4Addr::LOCALHOST]),
            Some(Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    fn a_profile_address_is_stable_between_calls() {
        assert_eq!(address_for("work", &[]), address_for("work", &[]));
    }

    #[test]
    fn different_profiles_get_different_addresses() {
        assert_ne!(address_for("work", &[]), address_for("home", &[]));
    }

    #[test]
    fn profile_addresses_never_enter_the_default_subnet() {
        for profile in ["work", "home", "travel", "vpn-42"] {
            let octets = address_for(profile, &[]).unwrap().octets();
            assert_eq!(octets[0], 127);
            assert_ne!(octets[1], 0, "{profile}");
        }
    }

    #[test]
    fn colliding_profile_candidates_are_separated() {
        let mut candidates = HashMap::new();
        let (first_profile, second_profile, candidate) = (0_u32..)
            .find_map(|index| {
                let profile = format!("profile-{index}");
                let candidate = address_for(&profile, &[]).unwrap();
                candidates
                    .insert(candidate, profile.clone())
                    .map(|first| (first, profile, candidate))
            })
            .expect("the finite address space must eventually produce a collision");

        assert_ne!(first_profile, second_profile);
        assert_eq!(address_for(&first_profile, &[]), Some(candidate));
        assert_eq!(address_for(&second_profile, &[]), Some(candidate));
        assert_ne!(address_for(&second_profile, &[candidate]), Some(candidate));
    }

    #[test]
    fn a_taken_address_is_not_returned_again() {
        let candidate = address_for("work", &[]).unwrap();
        let allocated = address_for("work", &[candidate]).unwrap();
        assert_ne!(allocated, candidate);
    }

    #[test]
    fn exhausting_the_space_returns_none_instead_of_looping() {
        let taken: Vec<Ipv4Addr> = (1..=254)
            .flat_map(|first| (0..=255).map(move |second| Ipv4Addr::new(127, first, second, 1)))
            .collect();
        assert_eq!(address_for("work", &taken), None);
    }

    #[test]
    fn derived_device_names_are_valid_and_long_profiles_name_the_override() {
        assert_eq!(device_name("work").unwrap(), "oxi-work");
        let error = device_name("twelve-chars").unwrap_err().to_string();
        assert!(error.contains("[interface] device"), "{error}");
    }

    #[test]
    fn invalid_device_names_are_rejected() {
        for valid in ["a", "oxi-work", "tun.2", "UPPER_case-1"] {
            validate_device_name(valid).unwrap();
        }
        for invalid in [
            "",
            ".",
            "..",
            "name/with/slash",
            "non-ascii-ø",
            "1234567890123456",
        ] {
            assert!(
                validate_device_name(invalid).is_err(),
                "{invalid:?} should be rejected"
            );
        }
    }

    #[test]
    fn device_addresses_are_stable_and_avoid_reserved_host_octets() {
        assert_eq!(
            device_address_for("default", &[]),
            Some(Ipv4Addr::new(198, 18, 0, 1))
        );
        assert_eq!(
            device_address_for("default", &[Ipv4Addr::new(198, 18, 0, 1)]),
            Some(Ipv4Addr::new(198, 18, 0, 1))
        );
        let first = device_address_for("work", &[]).unwrap();
        assert_eq!(device_address_for("work", &[]), Some(first));
        assert_eq!(first.octets()[..2], [198, 18]);
        assert!(!matches!(first.octets()[3], 0 | 255));
    }

    #[test]
    fn device_address_collisions_probe_and_exhaustion_returns_none() {
        let mut candidates = HashMap::new();
        let (first_profile, second_profile, candidate) = (0_u32..)
            .find_map(|index| {
                let profile = format!("profile-{index}");
                let candidate = device_address_for(&profile, &[]).unwrap();
                candidates
                    .insert(candidate, profile.clone())
                    .map(|first| (first, profile, candidate))
            })
            .expect("the finite device address space must produce a collision");
        assert_ne!(first_profile, second_profile);
        assert_eq!(device_address_for(&first_profile, &[]), Some(candidate));
        assert_eq!(device_address_for(&second_profile, &[]), Some(candidate));
        assert_ne!(
            device_address_for(&second_profile, &[candidate]),
            Some(candidate),
            "a collision must probe forward"
        );
        let taken: Vec<Ipv4Addr> = (0..=255)
            .flat_map(|third| (1..=254).map(move |fourth| Ipv4Addr::new(198, 18, third, fourth)))
            .collect();
        assert_eq!(device_address_for("work", &taken), None);
    }

    #[test]
    fn routing_marks_are_stable_and_probe_collisions() {
        assert_eq!(routing_mark("default", &[]), Some(0x6f00));
        assert_eq!(routing_mark("default", &[0x6f00]), Some(0x6f00));
        let mut candidates = HashMap::new();
        let (first_profile, second_profile, candidate) = (0_u32..)
            .find_map(|index| {
                let profile = format!("profile-{index}");
                let candidate = routing_mark(&profile, &[]).unwrap();
                candidates
                    .insert(candidate, profile.clone())
                    .map(|first| (first, profile, candidate))
            })
            .expect("the finite routing-mark space must produce a collision");
        assert_ne!(first_profile, second_profile);
        assert_eq!(routing_mark(&first_profile, &[]), Some(candidate));
        assert_eq!(routing_mark(&second_profile, &[]), Some(candidate));
        assert!((0x6f01..=0x6fff).contains(&candidate));
        assert_ne!(routing_mark(&second_profile, &[candidate]), Some(candidate));
    }

    #[test]
    fn routing_mark_exhaustion_returns_none() {
        let taken: Vec<u32> = (0x6f01..=0x6fff).collect();
        assert_eq!(routing_mark("work", &taken), None);
    }
}
