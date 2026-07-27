//! Stable loopback addresses for independently running profiles.

use std::collections::HashSet;
use std::net::Ipv4Addr;

use crate::model::stable_hash;

const FIRST_OCTET_COUNT: usize = 254;
const SECOND_OCTET_COUNT: usize = 256;
const PROFILE_ADDRESS_COUNT: usize = FIRST_OCTET_COUNT * SECOND_OCTET_COUNT;

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
}
