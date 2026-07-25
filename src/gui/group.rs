use crate::model::Subscription;

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
    if parts.is_empty() {
        format!("{} servers", subscription.servers.len())
    } else {
        parts.join(" · ")
    }
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
