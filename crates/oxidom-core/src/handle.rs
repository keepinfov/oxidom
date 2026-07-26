use crate::model::Server;

pub enum HandleMatch<'a> {
    One(&'a Server),
    None,
    Ambiguous(Vec<&'a Server>),
}

/// Resolve a user-typed handle. Exactness wins over convenience: an alias or id
/// that matches exactly is never overridden by a substring hit elsewhere.
pub fn resolve<'a>(servers: impl Iterator<Item = &'a Server>, needle: &str) -> HandleMatch<'a> {
    if needle.is_empty() {
        return HandleMatch::None;
    }
    let servers: Vec<&Server> = servers.collect();
    if let Some(server) = servers
        .iter()
        .copied()
        .find(|server| server.alias.as_deref() == Some(needle))
    {
        return HandleMatch::One(server);
    }
    if let Some(server) = servers.iter().copied().find(|server| server.id == needle) {
        return HandleMatch::One(server);
    }

    let needle = needle.to_lowercase();
    let candidates: Vec<&Server> = servers
        .into_iter()
        .filter(|server| {
            server
                .alias
                .as_deref()
                .is_some_and(|alias| alias.to_lowercase().contains(&needle))
                || server.name.to_lowercase().contains(&needle)
        })
        .collect();
    match candidates.len() {
        0 => HandleMatch::None,
        1 => HandleMatch::One(candidates[0]),
        _ => HandleMatch::Ambiguous(candidates),
    }
}

#[cfg(test)]
mod tests {
    use super::{HandleMatch, resolve};
    use crate::link::parse_link;
    use crate::model::Server;

    enum Expected<'a> {
        One(&'a str),
        None,
        Ambiguous(&'a [&'a str]),
    }

    fn server(link: &str, id: &str, alias: &str) -> Server {
        let mut server = parse_link(link).unwrap();
        server.id = id.to_string();
        server.alias = Some(alias.to_string());
        server
    }

    #[test]
    fn resolves_handles_in_priority_order() {
        let servers = [
            server(
                "trojan://pw@one.example:443#Swiss%20Trojan",
                "1111111111111111",
                "ch-trojan",
            ),
            server(
                "trojan://pw@two.example:443#Backup%20Trojan",
                "2222222222222222",
                "ch-trojan-2",
            ),
            server(
                "vless://id@three.example:443#Berlin%20Fast",
                "3333333333333333",
                "de-vless",
            ),
        ];
        let cases = [
            ("ch-trojan", Expected::One("1111111111111111")),
            ("2222222222222222", Expected::One("2222222222222222")),
            ("BERLIN", Expected::One("3333333333333333")),
            (
                "trojan",
                Expected::Ambiguous(&["1111111111111111", "2222222222222222"]),
            ),
            ("", Expected::None),
            ("missing", Expected::None),
        ];

        for (needle, expected) in cases {
            match (resolve(servers.iter(), needle), expected) {
                (HandleMatch::One(server), Expected::One(id)) => assert_eq!(server.id, id),
                (HandleMatch::None, Expected::None) => {}
                (HandleMatch::Ambiguous(servers), Expected::Ambiguous(ids)) => {
                    assert_eq!(
                        servers
                            .iter()
                            .map(|server| server.id.as_str())
                            .collect::<Vec<_>>(),
                        ids
                    );
                }
                _ => panic!("unexpected resolution for {needle:?}"),
            }
        }
    }
}
