use std::collections::HashSet;

use crate::model::{Server, Subscription};

const MAX_ALIAS_LEN: usize = 32;

/// Turn a display name into the portable spelling accepted by systemd instances.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in name.chars() {
        if !character.is_ascii() {
            continue;
        }
        let character = character.to_ascii_lowercase();
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }

    slug.truncate(slug.len().min(MAX_ALIAS_LEN));
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("server");
    }
    if is_bare_id(&slug) {
        slug.push_str("-s");
    }
    slug
}

/// Suggest a stable human handle without repeating the country already in the name.
pub fn suggest(server: &Server) -> String {
    let slug = slugify(&server.name);
    let Some(country) = server
        .country
        .as_deref()
        .map(str::to_ascii_lowercase)
        .filter(|country| country.len() == 2 && country.chars().all(|c| c.is_ascii_lowercase()))
    else {
        return slug;
    };
    if slug.starts_with(&format!("{country}-")) {
        slug
    } else {
        slugify(&format!("{country}-{slug}"))
    }
}

/// Assign missing or invalid aliases while preserving every usable existing handle.
pub fn assign(subscriptions: &mut [Subscription]) {
    let mut used = HashSet::new();

    // Reserve persisted aliases before generating any new ones. Otherwise a new
    // server earlier in traversal order could steal a user's later explicit alias.
    for server in subscriptions
        .iter_mut()
        .flat_map(|subscription| subscription.servers.iter_mut())
    {
        let keep = server
            .alias
            .as_ref()
            .is_some_and(|alias| is_valid(alias) && used.insert(alias.clone()));
        if !keep {
            server.alias = None;
        }
    }

    for server in subscriptions
        .iter_mut()
        .flat_map(|subscription| subscription.servers.iter_mut())
    {
        if server.alias.is_some() {
            continue;
        }
        let base = suggest(server);
        let alias = available_alias(&base, &used);
        used.insert(alias.clone());
        server.alias = Some(alias);
    }
}

/// Whether a handle is safe as both a CLI argument and a systemd instance name.
pub fn is_valid(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    (1..=MAX_ALIAS_LEN).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && !is_bare_id(alias)
}

fn is_bare_id(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn available_alias(base: &str, used: &HashSet<String>) -> String {
    if !used.contains(base) {
        return base.to_string();
    }
    for number in 2usize.. {
        let suffix = format!("-{number}");
        let keep = MAX_ALIAS_LEN.saturating_sub(suffix.len());
        let mut stem = base[..base.len().min(keep)].trim_end_matches('-');
        if stem.is_empty() {
            stem = "server";
        }
        let candidate = format!("{stem}{suffix}");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("the numeric suffix space is unbounded")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{assign, is_valid, slugify, suggest};
    use crate::link::parse_link;
    use crate::model::{Server, Subscription};

    fn server(link: &str) -> Server {
        parse_link(link).unwrap()
    }

    fn subscriptions() -> Vec<Subscription> {
        let mut first = Subscription::new("https://one.example".to_string(), Some("One".into()));
        first.servers = vec![
            server("trojan://pw@one.example:443#Shared Node"),
            server("trojan://pw@two.example:443#Shared Node"),
        ];
        let mut second = Subscription::new("https://two.example".to_string(), Some("Two".into()));
        second.servers = vec![
            server("vless://id@three.example:443#Shared Node"),
            server("socks://four.example:1080#Unique"),
        ];
        vec![first, second]
    }

    fn aliases(subscriptions: &[Subscription]) -> Vec<String> {
        subscriptions
            .iter()
            .flat_map(|subscription| subscription.servers.iter())
            .filter_map(|server| server.alias.clone())
            .collect()
    }

    #[test]
    fn slugify_is_portable_and_never_looks_like_an_id() {
        assert_eq!(slugify(" 🇨🇭 Hello,  WORLD!! "), "hello-world");
        assert_eq!(slugify("Привет"), "server");
        assert_eq!(slugify("deadbeefcafe1234"), "deadbeefcafe1234-s");
        assert!(is_valid("hello-world"));
        assert!(!is_valid("deadbeefcafe1234"));
    }

    #[test]
    fn country_prefix_is_not_duplicated() {
        let swiss = server("trojan://pw@example.com:443#%F0%9F%87%A8%F0%9F%87%AD%20Trojan");
        assert_eq!(swiss.country.as_deref(), Some("CH"));
        assert_eq!(suggest(&swiss), "ch-trojan");

        let mut named = swiss;
        named.name = "CH-Trojan".to_string();
        assert_eq!(suggest(&named), "ch-trojan");
    }

    #[test]
    fn assignment_is_global_deterministic_and_stable() {
        let mut first_run = subscriptions();
        let mut second_run = first_run.clone();

        assign(&mut first_run);
        assign(&mut second_run);

        let first_aliases = aliases(&first_run);
        assert_eq!(first_aliases, aliases(&second_run));
        assert_eq!(
            first_aliases.len(),
            first_aliases.iter().collect::<HashSet<_>>().len()
        );
        assert!(first_aliases.iter().all(|alias| is_valid(alias)));

        let before = first_aliases;
        assign(&mut first_run);
        assert_eq!(aliases(&first_run), before);
    }

    #[test]
    fn a_missing_alias_cannot_steal_a_later_persisted_one() {
        let mut subscriptions = subscriptions();
        subscriptions[0].servers[1].alias = Some("shared-node".to_string());

        assign(&mut subscriptions);

        assert_eq!(
            subscriptions[0].servers[1].alias.as_deref(),
            Some("shared-node")
        );
        assert_eq!(
            subscriptions[0].servers[0].alias.as_deref(),
            Some("shared-node-2")
        );
    }
}
