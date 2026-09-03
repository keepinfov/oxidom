use oxidom_core::model::Subscription;

pub fn subscription_description(subscription: &Subscription) -> String {
    let mut parts = Vec::new();
    if let Some(description) = subscription.description.as_deref()
        && !description.trim().is_empty()
    {
        parts.push(description.trim().to_string());
    }
    if let Some(info) = &subscription.userinfo {
        let used = info.upload.saturating_add(info.download);
        if info.total > 0 {
            parts.push(format!(
                "{} used of {}",
                format_bytes(used),
                format_bytes(info.total)
            ));
        } else if used > 0 {
            parts.push(format!("{} used", format_bytes(used)));
        }
        if let Some(expire) = info.expire {
            let date = gtk::glib::DateTime::from_unix_local(expire)
                .and_then(|value| value.format("%x"))
                .map(|value| value.to_string())
                .unwrap_or_else(|_| expire.to_string());
            parts.push(format!("expires {date}"));
        }
    }
    if let Some(skipped) = skipped_note(subscription) {
        parts.push(skipped);
    }
    if parts.is_empty() {
        let count = subscription.servers.len();
        let plural = if count == 1 { "" } else { "s" };
        format!("{count} server{plural}")
    } else {
        parts.join(" · ")
    }
}

/// "4 links skipped: tuic, ssh" — or nothing, which is the usual case.
///
/// Beside the quota rather than in a toast: a refresh happens once and this
/// question ("the app on my phone shows twenty of these") is asked whenever
/// the list is looked at. Naming the schemes is what makes it actionable —
/// an unsupported protocol is the provider's choice, a `vless` in this list
/// would be a parser bug.
pub fn skipped_note(subscription: &Subscription) -> Option<String> {
    let skipped = &subscription.skipped;
    if skipped.is_empty() {
        return None;
    }
    let lines = skipped.lines;
    let plural = if lines == 1 { "" } else { "s" };
    Some(match skipped.schemes.as_slice() {
        [] => format!("{lines} link{plural} skipped"),
        schemes => format!("{lines} link{plural} skipped: {}", schemes.join(", ")),
    })
}

/// "This subscription carried 12 routing rules and 3 rule sets, none of which
/// oxidom applied" — or nothing, which is the usual case.
///
/// Beside the quota for the same reason `skipped_note` is: it answers a
/// question asked whenever the list is looked at ("this behaves differently in
/// the other app"), not once at refresh time when a toast would already be
/// gone. The wording is `NotTaken::summary`, in core, so the log line and this
/// cannot describe one import two ways.
pub fn not_taken_note(subscription: &Subscription) -> Option<String> {
    subscription
        .not_taken
        .summary()
        .map(|summary| format!("This subscription {summary}."))
}

/// Human-readable byte count, in the SI units panels quote their quotas in.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use oxidom_core::link::Skipped;
    use oxidom_core::model::{OutboundSpec, Protocol, Server, Subscription};
    use oxidom_core::subscription_format::NotTaken;

    use super::{not_taken_note, skipped_note, subscription_description};

    fn subscription(skipped: Skipped) -> Subscription {
        let mut subscription = Subscription::new("https://example.invalid/s".into(), None);
        subscription.skipped = skipped;
        subscription
    }

    /// Nothing carried is said as nothing. "0 routing rules" would read as an
    /// import that went wrong rather than as a plain subscription, and most
    /// subscriptions are plain.
    #[test]
    fn a_subscription_that_carried_only_servers_says_nothing_about_routing() {
        assert_eq!(not_taken_note(&subscription(Skipped::default())), None);
    }

    /// The two counts read as one sentence, and it says *not applied* rather
    /// than leaving a number to be read as something that worked.
    #[test]
    fn routing_that_arrived_and_was_left_is_reported_as_left() {
        let mut sub = subscription(Skipped::default());
        sub.not_taken = NotTaken {
            rules: 12,
            rule_sets: 3,
            own_source: true,
        };
        let note = not_taken_note(&sub).expect("something was carried");
        assert_eq!(
            note,
            "This subscription carried 12 routing rules and 3 rule sets, and its own \
             source for that data, none of which oxidom applied."
        );

        sub.not_taken = NotTaken {
            rules: 1,
            rule_sets: 0,
            own_source: false,
        };
        assert_eq!(
            not_taken_note(&sub).expect("one rule is still one rule"),
            "This subscription carried 1 routing rule, none of which oxidom applied."
        );

        // A body that named only a source, with no rules of its own to go with
        // it, still named one.
        sub.not_taken = NotTaken {
            rules: 0,
            rule_sets: 0,
            own_source: true,
        };
        assert_eq!(
            not_taken_note(&sub).expect("a source is something"),
            "This subscription carried its own source for rule or geo data, none of \
             which oxidom applied."
        );
    }

    #[test]
    fn a_healthy_subscription_says_nothing_about_skipping() {
        assert_eq!(skipped_note(&subscription(Skipped::default())), None);
    }

    /// The reported case, in the line the user actually reads: the count alone
    /// would say servers are missing without saying that the missing ones are
    /// a protocol this build does not speak.
    #[test]
    fn the_note_names_the_schemes_that_were_dropped() {
        let note = skipped_note(&subscription(Skipped {
            lines: 4,
            schemes: vec!["tuic".into(), "ssh".into()],
        }));
        assert_eq!(note.as_deref(), Some("4 links skipped: tuic, ssh"));
        // And it reaches the line under the subscription's name, which is
        // where the question gets asked.
        assert!(
            subscription_description(&subscription(Skipped {
                lines: 1,
                schemes: vec!["tuic".into()],
            }))
            .contains("1 link skipped: tuic")
        );
    }

    fn subscription_with_servers(count: usize) -> Subscription {
        let mut subscription = subscription(Skipped::default());
        subscription.servers = (0..count)
            .map(|index| Server {
                id: format!("server-{index}"),
                name: format!("Server {index}"),
                protocol: Protocol::Vless,
                address: format!("{index}.example.invalid"),
                port: 443,
                transport_label: "vless".to_string(),
                country: None,
                spec: OutboundSpec::Socks {
                    username: None,
                    password: None,
                },
                link: None,
                alias: None,
                outbound_patch: None,
                overrides: None,
                latency_ms: None,
            })
            .collect();
        subscription
    }

    /// The fallback subtitle counts like its neighbours do: `skipped_note`
    /// says "1 link", so a subscription holding one server says "1 server",
    /// not "1 servers".
    #[test]
    fn a_lone_server_is_counted_in_the_singular() {
        assert_eq!(
            subscription_description(&subscription_with_servers(1)),
            "1 server"
        );
    }

    #[test]
    fn several_servers_keep_the_plural_count() {
        assert_eq!(
            subscription_description(&subscription_with_servers(2)),
            "2 servers"
        );
        assert_eq!(
            subscription_description(&subscription_with_servers(0)),
            "0 servers"
        );
    }
}
