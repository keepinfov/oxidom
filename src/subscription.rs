use anyhow::{bail, Context, Result};

use crate::link::{b64_decode, parse_links};
use crate::model::{Server, Subscription, UserInfo};

/// Fallback User-Agent when neither the subscription nor config specify one.
/// Panels commonly gate the response on a recognized client string.
const DEFAULT_USER_AGENT: &str = "v2rayNG/1.9.5";

/// Result of fetching + parsing a subscription URL.
pub struct FetchResult {
    pub servers: Vec<Server>,
    pub userinfo: Option<UserInfo>,
    pub title: Option<String>,
    pub update_interval: Option<u64>,
}

/// Fetch and parse a subscription.
///
/// `user_agent` matters: many panels (Remnawave, Marzban, Happ) return an
/// "app not supported" page instead of configs when the client is unknown, so
/// callers should pass a recognized client identifier.
///
/// `hwid` is only sent when `send_hwid` is true, and uses the Happ header set
/// (`x-hwid` + optional device metadata) that Remnawave-style panels expect.
pub fn fetch(
    url: &str,
    user_agent: &str,
    send_hwid: bool,
    hwid: Option<&str>,
) -> Result<FetchResult> {
    let ua = if user_agent.trim().is_empty() { DEFAULT_USER_AGENT } else { user_agent };
    let mut req = ureq::get(url).set("User-Agent", ua);
    if send_hwid {
        if let Some(id) = hwid {
            // Happ/Remnawave device identification headers. Only x-hwid is
            // required; the rest help the panel label the device. Sent solely
            // when the user opts in per subscription.
            req = req
                .set("x-hwid", id)
                .set("x-device-os", "Linux")
                .set("x-ver-os", std::env::consts::ARCH)
                .set("x-device-model", "oxidom");
        }
    }
    let resp = req.call().with_context(|| format!("fetching subscription {url}"))?;

    let userinfo = resp.header("subscription-userinfo").and_then(parse_userinfo);
    let title = resp
        .header("profile-title")
        .map(decode_maybe_b64_header)
        .filter(|s| !s.is_empty());
    let update_interval = resp
        .header("profile-update-interval")
        .and_then(|s| s.trim().parse::<u64>().ok());

    let body = resp.into_string().context("reading subscription body")?;
    let text = decode_body(&body);
    let servers = parse_links(&text);

    // A non-empty response that yields no servers and carries no link scheme is
    // almost always a gate page ("app not supported", captcha, HTML error).
    // Surface it so the user can fix the User-Agent instead of seeing "0 servers".
    if servers.is_empty() && !text.trim().is_empty() && !text.contains("://") {
        let snippet: String = text.trim().chars().take(200).collect();
        bail!(
            "subscription returned no servers — the panel likely rejected the client \
             (User-Agent \"{ua}\"). Response: {snippet}"
        );
    }

    Ok(FetchResult { servers, userinfo, title, update_interval })
}

/// Fetch into an existing Subscription, updating servers/userinfo/name/timestamp.
pub fn refresh(sub: &mut Subscription, user_agent: &str, hwid: Option<&str>) -> Result<()> {
    let ua = sub.user_agent.as_deref().unwrap_or(user_agent);
    let res = fetch(&sub.url, ua, sub.send_hwid, hwid)?;
    sub.servers = res.servers;
    sub.userinfo = res.userinfo;
    if let Some(t) = res.title {
        sub.name = t;
    }
    sub.updated_at = Some(now_unix());
    Ok(())
}

/// If the whole body is base64 that decodes to link text, use that; else raw.
fn decode_body(body: &str) -> String {
    let looks_b64 = body
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_') || c.is_ascii_whitespace());
    if looks_b64 && !body.contains("://") {
        if let Some(bytes) = b64_decode(body) {
            if let Ok(text) = String::from_utf8(bytes) {
                if text.contains("://") {
                    return text;
                }
            }
        }
    }
    body.to_string()
}

fn decode_maybe_b64_header(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("base64:") {
        if let Some(bytes) = b64_decode(rest) {
            if let Ok(text) = String::from_utf8(bytes) {
                return text;
            }
        }
    }
    raw.to_string()
}

/// Parse `upload=..; download=..; total=..; expire=..` header.
fn parse_userinfo(raw: &str) -> Option<UserInfo> {
    let mut info = UserInfo::default();
    let mut any = false;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            let v = v.trim();
            match k.trim() {
                "upload" => {
                    info.upload = v.parse().unwrap_or(0);
                    any = true;
                }
                "download" => {
                    info.download = v.parse().unwrap_or(0);
                    any = true;
                }
                "total" => {
                    info.total = v.parse().unwrap_or(0);
                    any = true;
                }
                "expire" => {
                    info.expire = v.parse().ok();
                    any = true;
                }
                _ => {}
            }
        }
    }
    if any {
        Some(info)
    } else {
        None
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
