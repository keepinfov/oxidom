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
        format!("{} servers", subscription.servers.len())
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
    use oxidom_core::model::Subscription;

    use super::{skipped_note, subscription_description};

    fn subscription(skipped: Skipped) -> Subscription {
        let mut subscription = Subscription::new("https://example.invalid/s".into(), None);
        subscription.skipped = skipped;
        subscription
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
}
