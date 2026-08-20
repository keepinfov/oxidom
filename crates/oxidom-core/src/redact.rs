//! Turning log lines into something safe to paste in public.
//!
//! The log holds hostnames and addresses — `logbook`'s own file sink says so,
//! and creates its file private for that reason. The bug form asks the reporter
//! to guarantee that no share link, UUID, password or server address is in what
//! they send, and until now the only way to keep that promise was to read every
//! line by hand. One missed line puts a live credential in a public issue.
//!
//! This module removes those shapes and **marks each removal in place**, so a
//! reader can tell a redaction from an absence: a line that never named an
//! address and a line whose address was taken out must not look the same.
//!
//! It lives in `oxidom-core` rather than in the GUI so that the CLI, the window
//! and anything later produce the same report from the same rules. There is no
//! regular-expression engine here and none is wanted: the shapes are recognised
//! by `std`'s own address parsers and by hand, the way `link` parses links, and
//! every rule below is pinned by the corpus at the bottom of this file.
//!
//! ## What is deliberately kept
//!
//! Over-redaction is a failure too. A report whose every line reads
//! `[host] [address] [redacted]` tells nobody anything, and the issue this
//! answers asks for a redactor that can neither pass a credential nor blank a
//! whole log. So:
//!
//! - **Loopback and the unspecified address stay.** `127.0.0.1`, `::1` and
//!   `0.0.0.0` describe this machine's own listening sockets, name nobody, and
//!   are very often the crux of the report.
//! - **Ports stay.** Which port a thing was refused on is diagnosis; a port
//!   number identifies no one.
//! - **A private address is marked as private.** `[private address]` rather
//!   than `[address]`, because whether the address was on the user's own LAN is
//!   the difference between a routing bug and a server bug, and the range says
//!   that much without saying which machine.
//! - **oxidom's own dotted names stay** — the application id, bus names, and
//!   file names like `geoip.dat`, none of which are hostnames however much they
//!   look like one.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use crate::versions::Versions;

/// Schemes whose whole token is a credential. A share link carries the id, the
/// password and the address together, so nothing in one may survive.
const SHARE_SCHEMES: &[&str] = &[
    "vless",
    "vmess",
    "trojan",
    "ss",
    "ssr",
    "hysteria",
    "hysteria2",
    "hy2",
    "tuic",
    "socks",
    "http-proxy",
];

/// Query and assignment keys whose value is a secret whatever it looks like.
/// Matched on the whole key, lowercased, so `password` matches and `passwords`
/// does not — a partial match would swallow keys nobody meant to name.
const SECRET_KEYS: &[&str] = &[
    "password",
    "pass",
    "psk",
    "key",
    "secret",
    "token",
    "auth",
    "authorization",
    "credential",
    "seed",
    "uuid",
    "id",
    "sid",
    "user",
    "username",
    "email",
    "hwid",
];

/// Hosts that appear in the log only because oxidom itself fetched from them.
/// Keeping them is what leaves a failed geo-data download diagnosable; none of
/// them is a place a user's subscription lives.
const OXIDOM_HOSTS: &[&str] = &[
    "github.com",
    "api.github.com",
    "raw.githubusercontent.com",
    "objects.githubusercontent.com",
];

/// Last labels that make a dotted token a file rather than a host. Without this
/// `geoip.dat` reads as a hostname and the one line explaining a routing
/// refusal is redacted away.
const FILE_SUFFIXES: &[&str] = &[
    "dat", "json", "toml", "yaml", "yml", "log", "txt", "conf", "cfg", "ini", "service", "socket",
    "sock", "desktop", "xml", "db", "sqlite", "pem", "crt", "cer", "der", "rs", "so", "sh", "md",
    "png", "svg", "gz", "xz", "zip", "tmp", "lock", "pid", "old", "bak",
];

/// Reverse-DNS prefixes that are bus names and application ids, not hosts.
const REVERSE_DNS_PREFIXES: &[&str] = &[
    "dev.keepinfov.",
    "org.freedesktop.",
    "org.gnome.",
    "org.gtk.",
    "com.github.",
];

/// Addresses that name this machine's own sockets and nobody at all.
fn is_public_nothing(v4: Ipv4Addr) -> bool {
    v4.is_loopback() || v4.is_unspecified() || v4.is_broadcast()
}

/// Rewrites one line at a time, and knows this machine's own name.
///
/// Built once per report rather than per line: the machine name is read from
/// the filesystem, and a six-hundred-line report would otherwise read it six
/// hundred times.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    machine: Option<String>,
    home: Option<String>,
}

impl Redactor {
    /// Read this machine's own name and this account's own home directory.
    pub fn here() -> Self {
        Redactor {
            machine: machine_name(
                std::fs::read_to_string("/etc/hostname").ok().as_deref(),
                std::env::var("HOSTNAME").ok().as_deref(),
            ),
            home: dirs::home_dir()
                .as_deref()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| name.len() >= 2),
        }
    }

    /// The same, with both facts supplied. The corpus uses this: a test that
    /// read the machine it runs on would assert something different on every
    /// machine, and would say nothing about the rule.
    pub fn with(machine: Option<&str>, home: Option<&str>) -> Self {
        Redactor {
            machine: machine
                .map(str::trim)
                .filter(|name| name.len() >= 3)
                .map(str::to_string),
            home: home
                .map(str::trim)
                .filter(|name| name.len() >= 2)
                .map(str::to_string),
        }
    }

    /// One line, with everything identifying in it marked and taken out.
    ///
    /// Whitespace-separated tokens, each classified on its own. The scan is
    /// token-wise rather than character-wise because every shape being looked
    /// for is a whole word — an address, a link, an assignment — and a
    /// character scan would have to decide where one ends by guessing at
    /// punctuation that means different things inside a URL and outside one.
    pub fn line(&self, text: &str) -> String {
        // The account name first, before anything is replaced: it appears
        // inside paths — `/home/name/.local/share/oxidom` — where no token rule
        // would find it, and the path around it is worth keeping.
        let text = match &self.home {
            Some(home) => replace_home(text, home),
            None => text.to_string(),
        };
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let space = rest.len() - rest.trim_start().len();
            out.push_str(&rest[..space]);
            rest = &rest[space..];
            if rest.is_empty() {
                break;
            }
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push_str(&self.token(&rest[..end]));
            rest = &rest[end..];
        }
        // The machine's own name last, over what the token rules left. It is a
        // literal rather than a shape — it can be any word at all — so it is
        // matched on word boundaries and only from three characters up. A
        // two-letter hostname would match half the log, and blanking a whole
        // log is the failure this module is judged against as much as leaking
        // is.
        match &self.machine {
            Some(machine) => replace_word(&out, machine, "[machine]"),
            None => out,
        }
    }

    /// Every line of a slice, in order.
    pub fn lines<'a>(&self, lines: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        lines.into_iter().map(|line| self.line(line)).collect()
    }

    /// One whitespace-delimited token, with whatever punctuation surrounds it
    /// put back. `(1.2.3.4)` and `1.2.3.4,` are the same address as `1.2.3.4`,
    /// and a rule that only recognised the bare form would pass both.
    fn token(&self, raw: &str) -> String {
        let lead_len = raw.len() - raw.trim_start_matches(is_edge).len();
        let (lead, rest) = raw.split_at(lead_len);
        let core_len = rest.trim_end_matches(is_edge).len();
        let (core, trail) = rest.split_at(core_len);
        // A URL is looked at whole, because a comma is legal inside one. What
        // is left is split on the separators that pack several facts into one
        // token — `from=1.2.3.4,to=5.6.7.8` is two assignments, and classifying
        // it as one would find the second address and walk past the first.
        let body = match scheme_token(core) {
            Some(replaced) => replaced,
            None => split_keep(core, &[',', ';'], classify),
        };
        format!("{lead}{body}{trail}")
    }
}

/// Punctuation that can sit around a token without being part of it.
///
/// `:` is not here — it separates a port. `/` is not either, since a path is a
/// token's own business. Neither are `[` and `]`, which are how an IPv6 address
/// carries a port: trimming those would leave `2001:db8::1` and `:8388` as two
/// tokens, and the second would read as a bare port with nothing in front of it.
fn is_edge(c: char) -> bool {
    matches!(
        c,
        '(' | ')' | '{' | '}' | '<' | '>' | ',' | ';' | '"' | '\'' | '`' | '!' | '?'
    )
}

/// A token carrying `scheme://`, if that is what it is.
fn scheme_token(core: &str) -> Option<String> {
    let split = core.find("://")?;
    let scheme = &core[..split];
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let lower = scheme.to_ascii_lowercase();
    if SHARE_SCHEMES.contains(&lower.as_str()) {
        // Not "[address]" plus "[uuid]" plus "[password]": the whole token is
        // one credential, and reporting its parts separately would describe how
        // it was built.
        return Some("[share link]".to_string());
    }
    if lower != "http" && lower != "https" {
        return Some(format!("{scheme}://[redacted]"));
    }
    let after = &core[split + 3..];
    let host_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..host_end];
    // Anything before an `@` is userinfo, which is a credential wherever it
    // stands, so a host is kept only when the authority is nothing but a host.
    let host = authority.split(':').next().unwrap_or(authority);
    if !authority.contains('@') && OXIDOM_HOSTS.contains(&host.to_ascii_lowercase().as_str()) {
        let tail = if host_end < after.len() {
            "/[redacted]"
        } else {
            ""
        };
        return Some(format!("{lower}://{authority}{tail}"));
    }
    Some(format!("{lower}://[redacted]"))
}

/// Apply `f` to each piece of `core` between `separators`, putting the
/// separators back where they were.
fn split_keep(core: &str, separators: &[char], f: fn(&str) -> String) -> String {
    let mut out = String::with_capacity(core.len());
    let mut piece = String::new();
    for c in core.chars() {
        if separators.contains(&c) {
            out.push_str(&f(&piece));
            piece.clear();
            out.push(c);
        } else {
            piece.push(c);
        }
    }
    out.push_str(&f(&piece));
    out
}

/// A bare token, once its surrounding punctuation is off.
fn classify(core: &str) -> String {
    if core.is_empty() {
        return String::new();
    }
    // `ann@desk-01`, `someone@example.com`, `uuid@203.0.113.9:443`: whatever
    // stands in front of an `@` is an account and whatever stands behind it is
    // a machine. Neither half is reportable, and marking them separately would
    // say how the pair was made. The tail is not required to look like a
    // *dotted* host, because a machine on a local network has one label and
    // `ann@desk-01` names a person and a machine just as plainly.
    if let Some((head, tail)) = core.rsplit_once('@')
        && !head.is_empty()
        && !tail.is_empty()
        && tail.chars().any(|c| c.is_ascii_alphanumeric())
        && tail
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
    {
        return "[redacted]".to_string();
    }
    if let Some((key, value)) = core.split_once('=') {
        let lower = key.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        if SECRET_KEYS.contains(&lower.to_ascii_lowercase().as_str()) {
            return format!("{key}=[redacted]");
        }
        // A key nobody named as secret still holds a value that may be an
        // address or a link, so the value goes back through the same rules.
        return format!("{key}={}", classify(value));
    }
    if is_uuid(core) {
        return "[uuid]".to_string();
    }
    if let Some(marked) = address(core) {
        return marked;
    }
    if let Some(marked) = host(core) {
        return marked;
    }
    core.to_string()
}

/// `[v6]:port`, `v4:port`, or a bare address of either family.
fn address(core: &str) -> Option<String> {
    if let Some(rest) = core.strip_prefix('[')
        && let Some((inside, tail)) = rest.split_once(']')
    {
        let v6: Ipv6Addr = inside.parse().ok()?;
        // The brackets go back on when the address stays: `[::1]:1081` is how a
        // v6 socket is written, and `::1:1081` is a different address entirely.
        let mark = if v6.is_loopback() || v6.is_unspecified() {
            format!("[{inside}]")
        } else {
            "[address]".to_string()
        };
        return Some(format!("{mark}{tail}"));
    }
    // A link-local address carries a zone — `fe80::1%wlan0` — which `std` will
    // not parse. Taking it off first is what keeps that shape from falling
    // through every rule below and reaching the report intact.
    let bare = core.split('%').next().unwrap_or(core);
    if let Ok(v6) = bare.parse::<Ipv6Addr>() {
        return Some(if v6.is_loopback() || v6.is_unspecified() {
            core.to_string()
        } else {
            "[address]".to_string()
        });
    }
    let (head, port) = match core.rsplit_once(':') {
        Some((head, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (head, format!(":{port}"))
        }
        _ => (core, String::new()),
    };
    let v4: Ipv4Addr = head.parse().ok()?;
    let mark = if is_public_nothing(v4) {
        head.to_string()
    } else if v4.is_private() || v4.is_link_local() {
        "[private address]".to_string()
    } else {
        "[address]".to_string()
    };
    Some(format!("{mark}{port}"))
}

/// A dotted name that is a host rather than a file, a bus name or a version.
fn host(core: &str) -> Option<String> {
    let (head, port) = match core.rsplit_once(':') {
        Some((head, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (head, format!(":{port}"))
        }
        _ => (core, String::new()),
    };
    // A path is not a host, and the first segment of one is not either: taking
    // `oxidom.log` out of `/var/log/oxidom.log` would leave a path to nowhere.
    if head.contains('/') || head.contains('@') {
        return None;
    }
    let lower = head.to_ascii_lowercase();
    if REVERSE_DNS_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return None;
    }
    let labels: Vec<&str> = head.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    if labels.iter().any(|label| {
        label.is_empty()
            || label.len() > 63
            || !label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }) {
        return None;
    }
    let last = labels[labels.len() - 1];
    // A version is dotted too. Requiring the last label to be letters is what
    // keeps `oxidom 0.2.0` and `Xray 26.3.27` out of this rule entirely.
    if last.len() < 2 || last.len() > 24 || !last.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    if FILE_SUFFIXES.contains(&last.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some(format!("[host]{port}"))
}

/// The 8-4-4-4-12 hexadecimal form, which is what every one of these protocols
/// uses for its account id.
fn is_uuid(core: &str) -> bool {
    let bytes = core.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            *byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

/// `/home/<name>/` wherever it appears, keeping the path around it.
fn replace_home(text: &str, home: &str) -> String {
    let mut out = replace_word(text, &format!("/home/{home}"), "/home/[user]");
    if !home.eq_ignore_ascii_case("root") {
        out = replace_word(&out, &format!("~{home}"), "~[user]");
    }
    out
}

/// Every whole-word occurrence of `needle`, case-insensitively.
///
/// Whole-word so a hostname that happens to be a common substring — `pi`,
/// `arch`, `box` — cannot eat the words around it. A character before or after
/// that is a letter, a digit, `-` or `_` means the match is part of something
/// longer and is left alone.
fn replace_word(text: &str, needle: &str, mark: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let lower_text = text.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(found) = lower_text[cursor..].find(&lower_needle) {
        let start = cursor + found;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word_byte(lower_text.as_bytes()[start - 1]);
        let after_ok = end == text.len() || !is_word_byte(lower_text.as_bytes()[end]);
        out.push_str(&text[cursor..start]);
        if before_ok && after_ok {
            out.push_str(mark);
        } else {
            out.push_str(&text[start..end]);
        }
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// This machine's name, from the file that holds it or from the environment.
///
/// Kept pure so the corpus can state what each source produces. An empty file,
/// a name of one or two characters, and `localhost` all give `None`: none of
/// them identifies a machine, and the last two would match half a log.
pub fn machine_name(etc_hostname: Option<&str>, env: Option<&str>) -> Option<String> {
    [etc_hostname, env]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| {
            name.len() >= 3
                && !name.eq_ignore_ascii_case("localhost")
                && !name.contains(char::is_whitespace)
        })
        .map(str::to_string)
}

/// What a report says about the machine, beyond the versions.
///
/// The two extra facts the bug form asks for that `Versions` cannot know: what
/// the connection is made of, and which User-Agent the subscription fetch
/// presents. Both come from the daemon's own state rather than from the window,
/// so a report assembled by the CLI says the same thing.
#[derive(Debug, Clone, Default)]
pub struct ReportContext {
    /// `transport_label` for the server in use — "vless + xhttp + reality".
    /// `None` when nothing is connected, which is itself worth reporting.
    pub transport: Option<String>,
    /// The preset the subscription fetch sends. Not a credential: it is a
    /// string the user picked from a list, and it decides what a provider
    /// returns.
    pub user_agent: String,
}

/// The whole report: what is running, what happened, and where to send it.
///
/// The header is [`Versions::rows`] and nothing rewritten, so the About
/// dialog's Troubleshooting page and this report cannot describe one machine
/// two ways. The lines are already-rendered log lines — the same text the page
/// shows — because a report that re-rendered them from records could differ
/// from what the reporter read before they sent it.
pub fn report(
    versions: &Versions,
    context: &ReportContext,
    lines: &[String],
    redactor: &Redactor,
) -> String {
    let mut out = String::new();
    out.push_str("oxidom problem report\n\n");
    for (label, value) in versions.rows() {
        out.push_str(&format!("{label}: {value}\n"));
    }
    out.push_str(&format!(
        "Connection: {}\n",
        context.transport.as_deref().unwrap_or("not connected")
    ));
    out.push_str(&format!(
        "Subscription User-Agent: {}\n",
        context.user_agent
    ));

    out.push_str("\nLog lines\n");
    if lines.is_empty() {
        out.push_str("(none were selected)\n");
    } else {
        for line in lines {
            out.push_str(redactor.line(line.trim_end_matches('\n')).trim_end());
            out.push('\n');
        }
    }

    out.push_str(
        "\nEverything marked in square brackets was removed by oxidom before this report was \
         written: an address, a host name, an account id or a credential stood there. A bracket \
         is a redaction, not an absence.\n\
         \n\
         Read this through before sending it. Then open an issue at\n\
         https://github.com/keepinfov/oxidom/issues and paste it into the bug form.\n\
         A security flaw does not go there — SECURITY.md in the repository says how to \
         report one privately.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Redactor {
        Redactor::with(None, None)
    }

    fn test_versions() -> Versions {
        Versions {
            app: "0.2.0".to_string(),
            daemon: Some("0.2.0".to_string()),
            core: Some("Xray 26.3.27".to_string()),
            source: Some(crate::client::DaemonSource::System),
            install: crate::versions::Install::Package,
            distribution: Some("Fedora Linux 42".to_string()),
            desktop: Some("GNOME, wayland".to_string()),
        }
    }

    /// The corpus the issue asks for, first half: every shape that must not
    /// survive. Each is asserted against literal bytes rather than against a
    /// pattern, so a rule that stops matching fails here rather than in a
    /// public issue.
    #[test]
    fn every_identifying_shape_is_removed_and_marked_where_it_stood() {
        let r = plain();
        for (line, expected) in [
            (
                "dialing 203.0.113.9:443 failed",
                "dialing [address]:443 failed",
            ),
            (
                "peer 2001:db8::1 did not answer",
                "peer [address] did not answer",
            ),
            (
                "bound [2001:db8::1]:8388 for the session",
                "bound [address]:8388 for the session",
            ),
            (
                "gateway 192.168.1.1 is not the tunnel",
                "gateway [private address] is not the tunnel",
            ),
            (
                "resolving relay.example.com took 900 ms",
                "resolving [host] took 900 ms",
            ),
            (
                "connecting to relay.example.com:8443",
                "connecting to [host]:8443",
            ),
            (
                "account 6f8c3d2e-1a4b-4c7d-9e0f-5a6b7c8d9e0f rejected",
                "account [uuid] rejected",
            ),
            (
                "importing vless://6f8c3d2e-1a4b-4c7d-9e0f-5a6b7c8d9e0f@relay.example.com:443?security=reality#Berlin",
                "importing [share link]",
            ),
            (
                "importing hysteria2://sekrit@relay.example.com:443/?obfs=salamander",
                "importing [share link]",
            ),
            (
                "fetching https://subs.example.net/token/aaaa",
                "fetching https://[redacted]",
            ),
            ("password=hunter2 refused", "password=[redacted] refused"),
            ("psk=abcdef0123 refused", "psk=[redacted] refused"),
            (
                "uuid=6f8c3d2e-1a4b-4c7d-9e0f-5a6b7c8d9e0f",
                "uuid=[redacted]",
            ),
            ("(203.0.113.9)", "([address])"),
            (
                "from=1.2.3.4,to=5.6.7.8 dropped",
                "from=[address],to=[address] dropped",
            ),
            ("logged in as ann@desk-01", "logged in as [redacted]"),
            (
                "mail someone@example.com bounced",
                "mail [redacted] bounced",
            ),
            (
                "neighbour fe80::1%wlan0 answered",
                "neighbour [address] answered",
            ),
            (
                "saw 203.0.113.9, then gave up",
                "saw [address], then gave up",
            ),
        ] {
            assert_eq!(r.line(line), expected, "input: {line}");
        }
    }

    /// The other half, and the more important one: a redactor that blanked a
    /// whole log would pass every assertion above and be useless. These lines
    /// must come through byte for byte.
    #[test]
    fn everything_a_report_is_written_for_survives_untouched() {
        let r = plain();
        for line in [
            "oxidom 0.2.0 starting",
            "Xray 26.3.27 (Xray, Penetrator) resolved",
            "the core has no geo data (geoip.dat, geosite.dat)",
            "reading /var/lib/oxidom/geoip.dat",
            "listening on 127.0.0.1:1080",
            "listening on [::1]:1081",
            "bound 0.0.0.0:8080 for the tunnel",
            "claimed dev.keepinfov.oxidom1 on the bus",
            "org.freedesktop.DBus.Error.UnknownMethod",
            "18:42:03  INFO   oxidom/oxidom_core::probe  the check ran out of time",
            "the check took 41 ms",
            "socks_port=1080 http_port=8118",
            "app/proxyman/outbound refused the config",
            "writing /home/[user]/.local/share/oxidom/oxidom.log",
        ] {
            assert_eq!(r.line(line), line, "input: {line}");
        }
    }

    /// The corpus as a whole line rather than a fragment: real log lines carry
    /// a clock, a severity, a source and a module path around whatever is being
    /// looked for, and every one of those has a shape that a careless rule
    /// mistakes for something else — a timestamp for an IPv6 address, a Rust
    /// module path for a host, `app/proxyman/outbound` for either.
    #[test]
    fn a_real_line_keeps_its_clock_its_source_and_its_module_path() {
        let r = Redactor::with(Some("desk-01"), Some("ann"));
        for (line, expected) in [
            (
                "19:02:11  WARN   xray/app/proxyman/outbound  failed > proxy/vless/outbound: context canceled",
                "19:02:11  WARN   xray/app/proxyman/outbound  failed > proxy/vless/outbound: context canceled",
            ),
            (
                "19:02:11  INFO   oxidom/oxidom_core::probe  probing de-fra-01.provider.net:443 by http_get",
                "19:02:11  INFO   oxidom/oxidom_core::probe  probing [host]:443 by http_get",
            ),
            (
                "19:02:12  INFO   oxidom/oxidom::daemon  listening on 127.0.0.1:1080 and 127.0.0.1:8118",
                "19:02:12  INFO   oxidom/oxidom::daemon  listening on 127.0.0.1:1080 and 127.0.0.1:8118",
            ),
            (
                "19:02:13  INFO   oxidom/oxidom_core::tun  oxidom-tun0 up, mtu 1500, gateway 10.0.0.1",
                "19:02:13  INFO   oxidom/oxidom_core::tun  oxidom-tun0 up, mtu 1500, gateway [private address]",
            ),
            (
                "19:02:15  INFO   oxidom/oxidom_core::run  spawning /nix/store/ab-xray-26.3.27/bin/xray -c /run/user/1000/oxidom/default.json",
                "19:02:15  INFO   oxidom/oxidom_core::run  spawning /nix/store/ab-xray-26.3.27/bin/xray -c /run/user/1000/oxidom/default.json",
            ),
            (
                "19:02:16  INFO   oxidom/oxidom_core::assets  geoip.dat in /home/ann/.local/share/oxidom/assets",
                "19:02:16  INFO   oxidom/oxidom_core::assets  geoip.dat in /home/[user]/.local/share/oxidom/assets",
            ),
            (
                "19:02:17  ERROR  oxidom/oxidom_core::link  vmess://eyJhZGQiOiJ4In0= could not be parsed",
                "19:02:17  ERROR  oxidom/oxidom_core::link  [share link] could not be parsed",
            ),
            (
                "19:02:18  INFO   oxidom/oxidom::daemon  desk-01 claimed dev.keepinfov.oxidom on the bus",
                "19:02:18  INFO   oxidom/oxidom::daemon  [machine] claimed dev.keepinfov.oxidom on the bus",
            ),
        ] {
            assert_eq!(r.line(line), expected, "input: {line}");
        }
    }

    /// oxidom's own downloads stay readable, because a geo-data fetch that
    /// failed is a report people file and "https://[redacted] refused" names
    /// nothing anyone can act on. The path still goes: a release URL is not
    /// secret, but nothing outside the host is worth the risk of guessing.
    #[test]
    fn a_fetch_oxidom_itself_made_keeps_the_host_it_named() {
        let r = plain();
        assert_eq!(
            r.line("downloading from https://github.com/XTLS/Xray-core/releases/geoip.dat"),
            "downloading from https://github.com/[redacted]"
        );
        assert_eq!(
            r.line("HEAD https://raw.githubusercontent.com"),
            "HEAD https://raw.githubusercontent.com"
        );
    }

    /// A host that oxidom fetches from can still be reached with a credential
    /// in front of it, and the allowlist must not be a way through: userinfo
    /// disqualifies the whole authority.
    #[test]
    fn a_credential_in_front_of_an_allowed_host_is_not_allowed_through() {
        assert_eq!(
            plain().line("https://token:secret@github.com/private"),
            "https://[redacted]"
        );
    }

    /// The machine's own name is a literal, not a shape, so it is matched on
    /// word boundaries: a hostname that is also a common substring must not eat
    /// the words around it.
    #[test]
    fn the_machines_own_name_goes_without_taking_the_words_around_it() {
        let r = Redactor::with(Some("arch"), None);
        assert_eq!(r.line("arch answered"), "[machine] answered");
        assert_eq!(r.line("ARCH answered"), "[machine] answered");
        assert_eq!(
            r.line("architecture x86_64 on archlinux"),
            "architecture x86_64 on archlinux",
            "a longer word that merely starts with the name is not the name"
        );
    }

    /// A two-character hostname, an empty one and `localhost` are not names
    /// worth matching; matching them would replace half a log.
    #[test]
    fn a_name_too_short_to_be_a_machine_is_not_treated_as_one() {
        assert_eq!(machine_name(Some("pi"), None), None);
        assert_eq!(machine_name(Some("  "), None), None);
        assert_eq!(machine_name(Some("localhost"), None), None);
        assert_eq!(
            machine_name(Some("\n"), Some("desk-01")),
            Some("desk-01".into())
        );
        assert_eq!(
            machine_name(Some("desk-01\n"), None),
            Some("desk-01".into())
        );
    }

    /// The account name appears in paths, where no token rule would find it,
    /// and the path around it is worth keeping — it says which daemon wrote
    /// the file.
    #[test]
    fn the_account_name_goes_and_the_path_around_it_stays() {
        let r = Redactor::with(None, Some("ann"));
        assert_eq!(
            r.line("reading /home/ann/.config/oxidom/config.toml"),
            "reading /home/[user]/.config/oxidom/config.toml"
        );
        assert_eq!(
            r.line("reading /home/annabel/.config/oxidom/config.toml"),
            "reading /home/annabel/.config/oxidom/config.toml",
            "a longer account name that starts with this one is a different account"
        );
    }

    /// A value under a key nobody declared secret can still be an address or a
    /// link, so it goes back through the same rules rather than being trusted
    /// for having a name in front of it.
    #[test]
    fn a_value_under_an_ordinary_key_is_still_examined() {
        let r = plain();
        assert_eq!(r.line("server=203.0.113.9"), "server=[address]");
        assert_eq!(r.line("host=relay.example.com"), "host=[host]");
        assert_eq!(r.line("port=443"), "port=443");
    }

    /// The report is one block of bytes and its shape is what a reporter
    /// pastes, so it is frozen here rather than described.
    #[test]
    fn a_report_states_what_is_running_then_what_happened_then_where_to_send_it() {
        let versions = test_versions();
        let context = ReportContext {
            transport: Some("vless + xhttp + reality".to_string()),
            user_agent: "v2rayN/6.45".to_string(),
        };
        let lines = vec!["dialing 203.0.113.9:443 failed".to_string()];
        let text = report(&versions, &context, &lines, &plain());

        assert!(text.starts_with("oxidom problem report\n\n"), "{text}");
        assert!(text.contains("Version: oxidom 0.2.0\n"), "{text}");
        assert!(
            text.contains("Connection: vless + xhttp + reality\n"),
            "{text}"
        );
        assert!(
            text.contains("Subscription User-Agent: v2rayN/6.45\n"),
            "{text}"
        );
        assert!(
            text.contains("\nLog lines\ndialing [address]:443 failed\n"),
            "{text}"
        );
        assert!(
            text.contains("A bracket is a redaction, not an absence."),
            "{text}"
        );
        assert!(
            text.contains("https://github.com/keepinfov/oxidom/issues"),
            "{text}"
        );
        assert!(text.contains("SECURITY.md"), "{text}");
    }

    /// A report with no lines selected says so, rather than showing a heading
    /// with nothing under it — which reads as a report that lost them.
    #[test]
    fn a_report_with_nothing_selected_says_nothing_was_selected() {
        let text = report(&test_versions(), &ReportContext::default(), &[], &plain());
        assert!(
            text.contains("\nLog lines\n(none were selected)\n"),
            "{text}"
        );
        assert!(text.contains("Connection: not connected\n"), "{text}");
    }
}
