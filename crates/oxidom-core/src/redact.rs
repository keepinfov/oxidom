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
//! **Marks are numbered per report.** Telling a redaction from an absence is
//! the first requirement; telling one redaction from another is the same
//! requirement one step further. With one mark for every host, two different
//! hosts read identically and one host appearing twice cannot be told from two,
//! so a reader following a failure through a log can come away with a different
//! sequence of events from the one that happened. `[host 1]` is the same host
//! wherever it stands, for the length of one report — which is why [`Redactor`]
//! carries state and [`Redactor::line`] takes `&mut self`.
//!
//! **Some names have no shape, and are passed in.** An outbound tag is `s-`
//! plus a server's alias, and `alias::suggest` derives that alias from the
//! server's name and its country — so `s-nl-soda-vpn` names the provider and
//! the exit country, in every access line and every observatory line. It has no
//! dot, so the host rule never sees it, and there is no shape that separates it
//! from an ordinary word. [`Redactor::for_servers`] takes the server list the
//! caller already holds; recognising the `s-` prefix instead would have meant
//! redacting `s-01` too, i.e. redacting because a token looks like words.
//! Passing the set also catches a server's address by name rather than only by
//! shape. A tag whose handle is not a known server survives, and the corpus
//! asserts that.
//!
//! [`Server::name`] is fed in as well, though the log lines carry tags and
//! addresses rather than display names: a name with spaces is not one
//! whitespace token, so it is caught only where it happens to be one word.
//! That is a real limit and it is stated here rather than half-implemented.
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
//! - **A private address is marked as private.** `[private address N]` rather
//!   than `[address N]`, because whether the address was on the user's own LAN is
//!   the difference between a routing bug and a server bug, and the range says
//!   that much without saying which machine.
//! - **oxidom's own dotted names stay** — the application id, bus names, and
//!   file names like `geoip.dat`, none of which are hostnames however much they
//!   look like one.
//! - **A dotted identifier stays.** `Client.Timeout`, `io.EOF`,
//!   `net.ErrClosed`: the libraries below oxidom write errors this way, and
//!   `[host] exceeded while awaiting headers` sends a reader after a network
//!   problem that is not there. Case is what tells the two apart — DNS is
//!   written in lower case.
//! - **The hosts oxidom itself reaches for stay**, whether or not a scheme
//!   stands in front of them: a geo-data fetch and a reachability check that
//!   failed are both reports people file, and neither is readable once the
//!   name is gone.
//!
//! And the report says all of this, in its own footer. The reader is told to
//! read it through before sending, and that instruction is only actionable if
//! they know what the rules were meant to catch — a reader who sees
//! `127.0.0.1:1080` and `geoip.dat` intact beside a redaction cannot otherwise
//! tell a decision from a miss. [`MARKS`] is the one list, read both by the
//! code that emits the marks and by the footer that explains them.
//!
//! ## What the shapes are read from
//!
//! The lines are the ones the log actually holds, not invented ones. Xray's
//! access log writes `network:host:port` on both sides of `accepted` on every
//! line it emits, so the network comes off before the address is looked at;
//! its observatory writes a Go error with a quoted URL in the middle of a
//! sentence, so the punctuation after a URL is put back where it stood.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use crate::model::Server;
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
    // The two reachability targets oxidom ships as defaults: the pool
    // observatory's `core_options::DEFAULT_POOL_PROBE` and the default
    // `latency_test_url`. Both are settings, so a user who points either
    // somewhere else no longer matches this list, and their host is taken out
    // as any other is. Kept as defaults rather than resolved from the live
    // config because this list is about what oxidom ships: a redactor built by
    // the CLI and one built by the window must produce the same report.
    "connectivitycheck.gstatic.com",
    "www.gstatic.com",
];

/// Networks Xray writes in front of an address in its access log:
/// `from tcp:127.0.0.1:42204 accepted tcp:relay.example.net:443`. With the
/// network still attached the token parses as neither an address nor a host,
/// so the commonest line in the log needs the prefix taken off first.
const NETWORK_PREFIXES: &[&str] = &["tcp", "udp"];

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
    /// Every outbound tag oxidom could have written for a server the daemon
    /// knows — `alias` or, failing that, the id — and every server address.
    ///
    /// Literals, not a model. The caller derives them from the server list it
    /// already holds, so the CLI and the window read one fact from one source
    /// rather than each recognising a shape its own way.
    /// Paired with the thing each names, so a server's alias, its display name
    /// and its address all carry one number: they are one server, and numbering
    /// them apart would read as three.
    handles: Vec<(String, String)>,
    /// Which value got which number, for this report. Numbering is why `line`
    /// takes `&mut self`: it is state that genuinely exists, and hiding it
    /// behind interior mutability would let a cloned redactor silently split or
    /// share a numbering that has to be one per report.
    marks: Marks,
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
            ..Redactor::default()
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
            ..Redactor::default()
        }
    }

    /// Names this report must take out that no shape rule reaches.
    ///
    /// An outbound tag is `s-` plus a server's alias, and `alias::suggest`
    /// derives that alias from the server's name and its country — so
    /// `s-nl-soda-vpn` names the provider and the exit country, and it appears
    /// in every access line and every observatory line. As a token it has no
    /// dot, so the host rule's two-label guard rejects it; and there is no
    /// shape that separates it from an ordinary word, because an alias can be
    /// any word at all.
    ///
    /// The alternative was to recognise the `s-` shape, which would have had to
    /// take out `s-01` as readily as `s-nl-soda-vpn` — i.e. to redact when a
    /// token looks like words, which is guessing. Passing the set instead also
    /// catches a server's address by name, which the address and host rules
    /// reach only by shape.
    ///
    /// The same discipline as the machine name applies: the bare form is only
    /// matched from three characters up, because an alias of `us` would blank
    /// half a log, and blanking a report is the failure this module is judged
    /// against as much as leaking is. The `s-` prefixed form is unambiguous at
    /// any length.
    pub fn with_handles(self, handles: impl IntoIterator<Item = String>) -> Self {
        // Each name stands for itself: a caller passing bare strings has said
        // nothing about which of them are the same server.
        self.with_named_handles(handles.into_iter().map(|handle| {
            let key = handle.clone();
            (handle, key)
        }))
    }

    /// The same, with each name paired with what it names.
    fn with_named_handles(mut self, handles: impl IntoIterator<Item = (String, String)>) -> Self {
        for (handle, key) in handles {
            let handle = handle.trim().to_string();
            if handle.is_empty() || self.handles.iter().any(|(known, _)| *known == handle) {
                continue;
            }
            self.handles.push((handle, key));
        }
        // Longest first, so `nl-soda` inside `nl-soda-vpn` cannot take the
        // match and leave `-vpn` standing in the report.
        self.handles
            .sort_by_key(|(handle, _)| std::cmp::Reverse(handle.len()));
        self
    }

    /// The handles of a set of servers, derived once so that the CLI and the
    /// window cannot derive them differently.
    pub fn for_servers<'a>(self, servers: impl IntoIterator<Item = &'a Server>) -> Self {
        let mut handles = Vec::new();
        for server in servers {
            // The id is the key rather than one of the names, so a server whose
            // alias and address both appear is one number in the report.
            let key = server.id.clone();
            // The handle oxidom would have built a tag from: the alias, or the
            // id when a server has none.
            handles.push((
                server.alias.clone().unwrap_or_else(|| server.id.clone()),
                key.clone(),
            ));
            handles.push((server.name.clone(), key.clone()));
            handles.push((server.address.clone(), key));
        }
        self.with_named_handles(handles)
    }

    /// One line, with everything identifying in it marked and taken out.
    ///
    /// Whitespace-separated tokens, each classified on its own. The scan is
    /// token-wise rather than character-wise because every shape being looked
    /// for is a whole word — an address, a link, an assignment — and a
    /// character scan would have to decide where one ends by guessing at
    /// punctuation that means different things inside a URL and outside one.
    pub fn line(&mut self, text: &str) -> String {
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
    pub fn lines<'a>(&mut self, lines: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        lines.into_iter().map(|line| self.line(line)).collect()
    }

    /// One whitespace-delimited token, with whatever punctuation surrounds it
    /// put back. `(1.2.3.4)` and `1.2.3.4,` are the same address as `1.2.3.4`,
    /// and a rule that only recognised the bare form would pass both.
    fn token(&mut self, raw: &str) -> String {
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
            None => split_keep(core, &[',', ';'], |piece| self.classify(piece)),
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
    // The punctuation a sentence puts after a URL is the sentence's. It cannot
    // come off with the token's other edges, because `:` separates a port and
    // `.` separates a label inside a URL; only a token already known to carry a
    // scheme can tell those two uses apart.
    let body = split + 3;
    let end = body + core[body..].trim_end_matches(is_url_tail).len();
    let (url, tail) = core.split_at(end);
    Some(format!("{}{tail}", scheme_url(scheme, &url[body..])))
}

/// Punctuation that ends a phrase around a URL rather than belonging to it.
///
/// `]` and `}` are not here: a URL carries an IPv6 address in brackets, and
/// trimming the closing one would leave `https://[::1` to be read as a host.
fn is_url_tail(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '`' | ',' | ';' | ':' | '.' | '!' | '?' | ')' | '>'
    )
}

/// A URL whose scheme is already split off and whose trailing punctuation is
/// already set aside.
fn scheme_url(scheme: &str, after: &str) -> String {
    let lower = scheme.to_ascii_lowercase();
    if SHARE_SCHEMES.contains(&lower.as_str()) {
        // Not "[address]" plus "[uuid]" plus "[password]": the whole token is
        // one credential, and reporting its parts separately would describe how
        // it was built.
        return "[share link]".to_string();
    }
    if lower != "http" && lower != "https" {
        return format!("{scheme}://[redacted]");
    }
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
        return format!("{lower}://{authority}{tail}");
    }
    format!("{lower}://[redacted]")
}

/// Apply `f` to each piece of `core` between `separators`, putting the
/// separators back where they were.
fn split_keep(core: &str, separators: &[char], mut f: impl FnMut(&str) -> String) -> String {
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

impl Redactor {
    /// A bare token, once its surrounding punctuation is off.
    fn classify(&mut self, core: &str) -> String {
        if core.is_empty() {
            return String::new();
        }
        // The network comes off first, before the `@` rule and before either
        // parser: `tcp:203.0.113.9:443` is an address with four characters in
        // front of it, and every rule below reads it as a word it does not know.
        if let Some(marked) = self.network_token(core) {
            return marked;
        }
        // A server's own name, before every shape rule below: an alias has no dot,
        // so the host rule would never see it, and an address the daemon named is
        // caught here even where its shape is one the parsers do not recognise.
        if let Some(marked) = self.handle(core) {
            return marked;
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
            return format!("{key}={}", self.classify(value));
        }
        if is_uuid(core) {
            return self.marks.mark(MarkKind::Uuid, core);
        }
        if let Some(marked) = self.address(core) {
            return marked;
        }
        if let Some(marked) = self.host(core) {
            return marked;
        }
        core.to_string()
    }

    /// `tcp:HOST:PORT` — Xray's access-log shape — with the network kept and what
    /// follows it put back through the same rules.
    ///
    /// The network is worth keeping: which protocol a connection was refused on is
    /// diagnosis, and it names nobody.
    fn network_token(&mut self, core: &str) -> Option<String> {
        let (network, rest) = core.split_once(':')?;
        if rest.is_empty() || !NETWORK_PREFIXES.contains(&network.to_ascii_lowercase().as_str()) {
            return None;
        }
        Some(format!("{network}:{}", self.classify(rest)))
    }

    /// A server the daemon named: its alias, its display name, its address, or the
    /// outbound tag oxidom built from the alias.
    ///
    /// The `s-` namespace is kept and only the handle replaced, because the prefix
    /// names nobody and the line is about a pool member — `[socks-in >> s-[node 1]]`
    /// still reads as an access line through a pool.
    fn handle(&mut self, core: &str) -> Option<String> {
        // `]` is deliberately not an edge character — trimming it would split an
        // IPv6 socket into an address and a bare port — so Xray's access-log shape
        // `[socks-in >> s-fra]` arrives here with the bracket still attached. A
        // token that never opened one did not own the closing one either.
        let (core, close) = match core.strip_suffix(']') {
            Some(rest) if !rest.contains('[') => (rest, "]"),
            _ => (core, ""),
        };
        let bare = core.strip_prefix(crate::xray::config::SELECTABLE_TAG_PREFIX);
        let (candidate, prefixed) = match bare {
            Some(rest) if !rest.is_empty() => (rest, true),
            // A bare handle needs the same floor as the machine name: two
            // characters would match half a log.
            _ if core.len() >= 3 => (core, false),
            _ => return None,
        };
        let key = self
            .handles
            .iter()
            .find(|(handle, _)| handle.eq_ignore_ascii_case(candidate))?
            .1
            .clone();
        let mark = self.marks.mark(MarkKind::Node, &key);
        Some(if prefixed {
            format!(
                "{}{mark}{close}",
                crate::xray::config::SELECTABLE_TAG_PREFIX
            )
        } else {
            format!("{mark}{close}")
        })
    }

    /// `[v6]:port`, `v4:port`, or a bare address of either family.
    fn address(&mut self, core: &str) -> Option<String> {
        if let Some(rest) = core.strip_prefix('[')
            && let Some((inside, tail)) = rest.split_once(']')
        {
            let v6: Ipv6Addr = inside.parse().ok()?;
            // The brackets go back on when the address stays: `[::1]:1081` is how a
            // v6 socket is written, and `::1:1081` is a different address entirely.
            let mark = if v6.is_loopback() || v6.is_unspecified() {
                format!("[{inside}]")
            } else {
                self.marks.mark(MarkKind::Address, inside)
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
                self.marks.mark(MarkKind::Address, bare)
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
            self.marks.mark(MarkKind::PrivateAddress, head)
        } else {
            self.marks.mark(MarkKind::Address, head)
        };
        Some(format!("{mark}{port}"))
    }

    /// A dotted name that is a host rather than a file, a bus name or a version.
    fn host(&mut self, core: &str) -> Option<String> {
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
        // A host oxidom itself reaches for is kept whether or not a scheme stood in
        // front of it. Consulting this list only where one did meant the same name
        // was kept in a URL and taken out on its own line.
        if OXIDOM_HOSTS.contains(&lower.as_str()) {
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
        // A dotted identifier is not a host: `Client.Timeout`, `io.EOF`,
        // `net.ErrClosed`, and every bus name outside the prefix list above. Case
        // is what separates them from a name that was resolved — DNS is written in
        // lower case and an exported Go identifier is not. A token that is upper
        // case throughout stays in the rule, because a host shouted in capitals is
        // still a host.
        let shouted = head
            .chars()
            .filter(char::is_ascii_alphabetic)
            .all(|c| c.is_ascii_uppercase());
        if !shouted && last.chars().any(|c| c.is_ascii_uppercase()) {
            return None;
        }
        Some(format!("{}{port}", self.marks.mark(MarkKind::Host, head)))
    }
}

/// One kind of thing a report takes out, and how its marks are numbered.
///
/// Numbering is per kind and per report: the first host is `[host 1]` wherever
/// it appears, the second `[host 2]`, and the same value carries the same
/// number on every line. Without it two different hosts read identically and
/// one host appearing twice cannot be told from two — so a redacted report can
/// be read as a different sequence of events from the one that happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MarkKind {
    Host,
    Address,
    PrivateAddress,
    Uuid,
    Node,
}

impl MarkKind {
    fn word(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Address => "address",
            Self::PrivateAddress => "private address",
            Self::Uuid => "uuid",
            Self::Node => "node",
        }
    }
}

/// Which value got which number, for the length of one report.
///
/// Values are keyed lower-cased: DNS is case-insensitive, and one host written
/// two ways is still one host.
#[derive(Debug, Clone, Default)]
struct Marks {
    numbers: HashMap<(MarkKind, String), usize>,
    counts: HashMap<MarkKind, usize>,
}

impl Marks {
    fn mark(&mut self, kind: MarkKind, value: &str) -> String {
        let key = (kind, value.to_ascii_lowercase());
        let number = match self.numbers.get(&key) {
            Some(number) => *number,
            None => {
                let count = self.counts.entry(kind).or_insert(0);
                *count += 1;
                let number = *count;
                self.numbers.insert(key, number);
                number
            }
        };
        format!("[{} {number}]", kind.word())
    }
}

/// Every mark a report can contain, and what it means.
///
/// One table, read both by the code that emits the marks and by the footer that
/// explains them, because the footer used to name four categories and zero
/// marks — and `[machine]` and `[user]` appeared in reports without being named
/// anywhere at all. A test asserts the footer names every entry here.
pub struct Mark {
    /// How it appears in a report, `N` standing for the number.
    pub text: &'static str,
    /// What stood there.
    pub means: &'static str,
}

pub const MARKS: &[Mark] = &[
    Mark {
        text: "[host N]",
        means: "a host name",
    },
    Mark {
        text: "[address N]",
        means: "an address outside your own network",
    },
    Mark {
        text: "[private address N]",
        means: "an address on your local network — which side of the tunnel it was on is often \
                the difference between a routing problem and a server problem, so it is marked \
                apart rather than hidden",
    },
    Mark {
        text: "[uuid N]",
        means: "an account id",
    },
    Mark {
        text: "[node N]",
        means: "a server's alias, which names your provider and usually its exit country",
    },
    Mark {
        text: "[share link]",
        means: "a share link, taken out whole: its address, its id and its password are one \
                credential, and marking the parts separately would describe how it was built",
    },
    Mark {
        text: "[redacted]",
        means: "a password, a key, or something else named as secret",
    },
    Mark {
        text: "[machine]",
        means: "this computer's name",
    },
    Mark {
        text: "[user]",
        means: "your account name, wherever it stood inside a path",
    },
];

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
    redactor: &mut Redactor,
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

    out.push_str(&footer());
    out
}

/// What the report says about its own rules, at the end of it.
///
/// The reader is asked to read the report through before sending it, and that
/// instruction is only actionable if they know what the rules were meant to
/// catch — otherwise it is ceremony. So this says three things the old footer
/// did not: what every mark means, what is kept and why, and that the rules are
/// shape-based and best-effort.
///
/// The marks come from [`MARKS`] rather than being listed here, because the
/// list and the code that emits them used to be separate and one of two copies
/// is always the one that stops being updated.
fn footer() -> String {
    let mut out = String::from(
        "\nWhat was taken out\n\
         \n\
         Everything in square brackets stood for something oxidom removed before this report \
         was written. A bracket is a redaction, not an absence: a line that never named an \
         address and a line whose address was taken out must not read the same.\n\
         \n\
         Marks are numbered per report, so the same value carries the same number wherever it \
         appears and two different values never read alike.\n\
         \n",
    );
    for mark in MARKS {
        out.push_str(&format!("  {} — {}\n", mark.text, mark.means));
    }
    out.push_str(
        "\nWhat was kept, on purpose\n\
         \n\
         Loopback, the unspecified address and every port number stay. They name nobody, and \
         they are usually the point of the report. So do oxidom's own names — the application \
         id, its bus names, geoip.dat and geosite.dat, and the hosts oxidom itself fetches \
         from — because a failed download that named nothing could not be diagnosed at all.\n\
         \n\
         The rules read shapes, not meanings, and they are best-effort. Read this through \
         before sending it. Then open an issue at\n\
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
        let mut r = plain();
        for (line, expected) in [
            (
                "dialing 203.0.113.9:443 failed",
                "dialing [address 1]:443 failed",
            ),
            (
                "peer 2001:db8::1 did not answer",
                "peer [address 2] did not answer",
            ),
            (
                "bound [2001:db8::1]:8388 for the session",
                "bound [address 2]:8388 for the session",
            ),
            (
                "gateway 192.168.1.1 is not the tunnel",
                "gateway [private address 1] is not the tunnel",
            ),
            (
                "resolving relay.example.com took 900 ms",
                "resolving [host 1] took 900 ms",
            ),
            (
                "connecting to relay.example.com:8443",
                "connecting to [host 1]:8443",
            ),
            (
                "account 6f8c3d2e-1a4b-4c7d-9e0f-5a6b7c8d9e0f rejected",
                "account [uuid 1] rejected",
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
            ("(203.0.113.9)", "([address 1])"),
            (
                "from=1.2.3.4,to=5.6.7.8 dropped",
                "from=[address 3],to=[address 4] dropped",
            ),
            ("logged in as ann@desk-01", "logged in as [redacted]"),
            (
                "mail someone@example.com bounced",
                "mail [redacted] bounced",
            ),
            (
                "neighbour fe80::1%wlan0 answered",
                "neighbour [address 5] answered",
            ),
            (
                "saw 203.0.113.9, then gave up",
                "saw [address 1], then gave up",
            ),
            (
                "from tcp:203.0.113.9:42204 accepted tcp:relay.example.com:443",
                "from tcp:[address 1]:42204 accepted tcp:[host 1]:443",
            ),
            (
                "relaying udp:198.51.100.7:53 to the tunnel",
                "relaying udp:[address 6]:53 to the tunnel",
            ),
            (
                "accepted tcp:[2001:db8::1]:443 from the pool",
                "accepted tcp:[address 2]:443 from the pool",
            ),
            ("RELAY.EXAMPLE.COM answered", "[host 1] answered"),
        ] {
            assert_eq!(r.line(line), expected, "input: {line}");
        }
    }

    /// The other half, and the more important one: a redactor that blanked a
    /// whole log would pass every assertion above and be useless. These lines
    /// must come through byte for byte.
    ///
    /// This includes `s-01`: with no server list, that tag names nothing this
    /// redactor knows, and taking it out would mean redacting on the strength
    /// of a two-character prefix.
    #[test]
    fn everything_a_report_is_written_for_survives_untouched() {
        let mut r = plain();
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
            "from tcp:127.0.0.1:42204 accepted tcp:www.gstatic.com:443 [socks-in >> s-01]",
            "context deadline exceeded (Client.Timeout exceeded while awaiting headers)",
            "the stream ended with io.EOF",
            "github.com answered in 200 ms",
        ] {
            assert_eq!(r.line(line), line, "input: {line}");
        }
    }

    /// `s-01` survives here, and that is the assertion rather than an
    /// oversight: this redactor was built with no server list, so the tag names
    /// a server it has never heard of. A rule that took it out anyway would be
    /// redacting because a token starts with `s-`, which is guessing at every
    /// other token that does. The paired test below covers the case where the
    /// handle *is* known.
    ///
    /// The corpus as a whole line rather than a fragment: real log lines carry
    /// a clock, a severity, a source and a module path around whatever is being
    /// looked for, and every one of those has a shape that a careless rule
    /// mistakes for something else — a timestamp for an IPv6 address, a Rust
    /// module path for a host, `app/proxyman/outbound` for either.
    #[test]
    fn a_real_line_keeps_its_clock_its_source_and_its_module_path() {
        let mut r = Redactor::with(Some("desk-01"), Some("ann"));
        for (line, expected) in [
            (
                "19:02:11  WARN   xray/app/proxyman/outbound  failed > proxy/vless/outbound: context canceled",
                "19:02:11  WARN   xray/app/proxyman/outbound  failed > proxy/vless/outbound: context canceled",
            ),
            (
                "19:02:11  INFO   oxidom/oxidom_core::probe  probing de-fra-01.provider.net:443 by http_get",
                "19:02:11  INFO   oxidom/oxidom_core::probe  probing [host 1]:443 by http_get",
            ),
            (
                "19:02:12  INFO   oxidom/oxidom::daemon  listening on 127.0.0.1:1080 and 127.0.0.1:8118",
                "19:02:12  INFO   oxidom/oxidom::daemon  listening on 127.0.0.1:1080 and 127.0.0.1:8118",
            ),
            (
                "19:02:13  INFO   oxidom/oxidom_core::tun  oxidom-tun0 up, mtu 1500, gateway 10.0.0.1",
                "19:02:13  INFO   oxidom/oxidom_core::tun  oxidom-tun0 up, mtu 1500, gateway [private address 1]",
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
            (
                "19:02:19  INFO   xray  from tcp:127.0.0.1:42204 accepted tcp:relay.example.com:443 [socks-in >> s-01]",
                "19:02:19  INFO   xray  from tcp:127.0.0.1:42204 accepted tcp:[host 2]:443 [socks-in >> s-01]",
            ),
            (
                "19:02:20  WARN   xray/app/observatory/burst  error ping https://connectivitycheck.gstatic.com/generate_204 with s-01: Head \"https://connectivitycheck.gstatic.com/generate_204\": context deadline exceeded (Client.Timeout exceeded while awaiting headers)",
                "19:02:20  WARN   xray/app/observatory/burst  error ping https://connectivitycheck.gstatic.com/[redacted] with s-01: Head \"https://connectivitycheck.gstatic.com/[redacted]\": context deadline exceeded (Client.Timeout exceeded while awaiting headers)",
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
        let mut r = plain();
        assert_eq!(
            r.line("downloading from https://github.com/XTLS/Xray-core/releases/geoip.dat"),
            "downloading from https://github.com/[redacted]"
        );
        assert_eq!(
            r.line("HEAD https://raw.githubusercontent.com"),
            "HEAD https://raw.githubusercontent.com"
        );
    }

    /// The punctuation a sentence puts after a URL belongs to the sentence. It
    /// cannot be trimmed with the token's other edges, because `:` separates a
    /// port and `.` separates a label inside a URL, so only a token already
    /// known to be one can tell the two uses apart.
    #[test]
    fn punctuation_after_a_url_belongs_to_the_line_and_stays() {
        let mut r = plain();
        assert_eq!(
            r.line("Head \"https://subs.example.net/token\": timed out"),
            "Head \"https://[redacted]\": timed out"
        );
        assert_eq!(
            r.line("fetched https://subs.example.net/list."),
            "fetched https://[redacted]."
        );
    }

    /// Xray's access log writes `network:host:port` on both sides of
    /// `accepted`, on every line it emits. Neither half of the pair parses as
    /// an address or as a host while the network is still attached, so without
    /// this the commonest line in the log the report is written from carries a
    /// server address out whole.
    #[test]
    fn a_network_in_front_of_an_address_does_not_carry_it_past_the_rules() {
        let mut r = plain();
        assert_eq!(r.line("tcp:203.0.113.9:443"), "tcp:[address 1]:443");
        assert_eq!(r.line("udp:relay.example.com:53"), "udp:[host 1]:53");
        assert_eq!(r.line("TCP:203.0.113.9:443"), "TCP:[address 1]:443");
        assert_eq!(
            r.line("dest=tcp:203.0.113.9:443"),
            "dest=tcp:[address 1]:443",
            "a value under a key goes through the same rules"
        );
        assert_eq!(
            r.line("tcp:127.0.0.1:1080"),
            "tcp:127.0.0.1:1080",
            "loopback is kept with a network in front of it as it is without"
        );
    }

    /// A dotted name whose last label is not written the way DNS is written is
    /// an identifier, not a host. `[host] exceeded while awaiting headers`
    /// sends a reader looking for a network problem that is not there.
    #[test]
    fn a_dotted_identifier_is_not_a_host() {
        let mut r = plain();
        for line in [
            "Client.Timeout exceeded while awaiting headers",
            "the stream ended with io.EOF",
            "closed with net.ErrClosed",
            "com.example.Some.Error was raised",
        ] {
            assert_eq!(r.line(line), line, "input: {line}");
        }
        assert_eq!(
            r.line("RELAY.EXAMPLE.COM answered"),
            "[host 1] answered",
            "a host shouted in capitals is still a host"
        );
    }

    /// `OXIDOM_HOSTS` was consulted only where a scheme stood in front of the
    /// host, so the same name was kept in a URL and taken out on its own. The
    /// pool's probe destination is oxidom's own constant and belongs there:
    /// without it an observatory failure names nothing anyone can act on.
    #[test]
    fn a_host_oxidom_reaches_for_itself_is_kept_with_or_without_a_scheme() {
        let mut r = plain();
        assert_eq!(r.line("github.com answered"), "github.com answered");
        assert_eq!(
            r.line("ping connectivitycheck.gstatic.com timed out"),
            "ping connectivitycheck.gstatic.com timed out"
        );
        assert_eq!(
            r.line("probing www.gstatic.com:443"),
            "probing www.gstatic.com:443"
        );
        assert_eq!(
            r.line("resolving relay.example.com"),
            "resolving [host 1]",
            "a name that is not oxidom's own is still taken out"
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
        let mut r = Redactor::with(Some("arch"), None);
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
        let mut r = Redactor::with(None, Some("ann"));
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
        let mut r = plain();
        assert_eq!(r.line("server=203.0.113.9"), "server=[address 1]");
        assert_eq!(r.line("host=relay.example.com"), "host=[host 1]");
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
        let text = report(&versions, &context, &lines, &mut plain());

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
            text.contains("\nLog lines\ndialing [address 1]:443 failed\n"),
            "{text}"
        );
        assert!(
            text.contains("A bracket is a redaction, not an absence"),
            "{text}"
        );
        assert!(
            text.contains("https://github.com/keepinfov/oxidom/issues"),
            "{text}"
        );
        assert!(text.contains("SECURITY.md"), "{text}");
    }

    /// A server the daemon knows, with the alias `alias::suggest` would derive.
    fn node(alias: &str, name: &str, address: &str) -> Server {
        Server {
            id: "0123456789abcdef".to_string(),
            name: name.to_string(),
            protocol: crate::model::Protocol::Vless,
            address: address.to_string(),
            port: 443,
            transport_label: "vless + tcp".to_string(),
            country: None,
            spec: crate::model::OutboundSpec::Vless {
                uuid: String::new(),
                encryption: "none".to_string(),
                stream: crate::model::StreamSettings::default(),
            },
            link: None,
            alias: Some(alias.to_string()),
            outbound_patch: None,
            overrides: None,
            latency_ms: None,
        }
    }

    /// An outbound tag is `s-` plus the alias, and the alias is derived from the
    /// server's name and its country — so it names the provider and usually the
    /// exit country, in every access line and every observatory line. No shape
    /// rule reaches it: as a token it has no dot, so the host rule's two-label
    /// guard rejects it.
    #[test]
    fn a_report_does_not_name_the_provider() {
        let mut r =
            plain().for_servers(&[node("nl-soda-vpn", "Soda VPN NL", "de-fra-01.soda.example")]);

        assert_eq!(
            r.line("from tcp:127.0.0.1:42204 accepted tcp:www.gstatic.com:443 [socks-in >> s-nl-soda-vpn]"),
            "from tcp:127.0.0.1:42204 accepted tcp:www.gstatic.com:443 [socks-in >> s-[node 1]]",
            "the outbound tag carried the provider through"
        );
        assert_eq!(
            r.line("nl-soda-vpn dropped out of rotation"),
            "[node 1] dropped out of rotation",
            "the bare alias is the same name without the namespace"
        );
        assert_eq!(
            r.line("probing de-fra-01.soda.example by tcp"),
            "probing [node 1] by tcp",
            "an address the daemon named is caught by name, not only by shape"
        );
    }

    /// The `s-` namespace stays: it names nobody, and the line is about a pool
    /// member, so `[socks-in >> s-[node 1]]` still reads as an access line
    /// through a pool while `[socks-in >> [node 1]]` would not.
    #[test]
    fn a_tag_keeps_the_namespace_that_names_nobody() {
        let mut r = plain().for_servers(&[node("fra", "Frankfurt", "one.example")]);
        assert!(
            r.line("[socks-in >> s-fra]").contains("s-[node 1]"),
            "the prefix went with the handle"
        );
    }

    /// A two-character alias in its bare form would match half a log — the same
    /// floor the machine name has, and for the same reason. The prefixed form
    /// is unambiguous at any length.
    #[test]
    fn a_very_short_alias_is_only_taken_out_where_it_cannot_be_a_word() {
        let mut r = plain().for_servers(&[node("us", "US", "one.example")]);
        assert_eq!(
            r.line("routing us traffic through the tunnel"),
            "routing us traffic through the tunnel",
            "a two-letter alias blanked an ordinary word"
        );
        assert_eq!(r.line("[socks-in >> s-us]"), "[socks-in >> s-[node 1]]");
    }

    /// The whole point of numbering: a reader following a failure through a log
    /// has to know whether the same address recurs. With one mark for all of
    /// them, a redacted report can be read as a different sequence of events
    /// from the one that happened.
    #[test]
    fn one_value_twice_carries_one_number_and_two_values_carry_two() {
        let mut r = plain();
        assert_eq!(
            r.line("error ping relay.example.com with relay.example.com"),
            "error ping [host 1] with [host 1]",
            "one host appearing twice read as two"
        );
        assert_eq!(
            r.line("relay.example.com then other.example.org"),
            "[host 1] then [host 2]",
            "the number is per report, so the first host keeps its own"
        );
        assert_eq!(
            r.line("still other.example.org"),
            "still [host 2]",
            "the numbering did not survive the line it was assigned on"
        );
    }

    /// Kinds are numbered apart, so a report never has to be read as though
    /// `[host 2]` and `[address 2]` were related.
    #[test]
    fn each_kind_is_numbered_on_its_own() {
        let mut r = plain();
        assert_eq!(
            r.line("relay.example.com 203.0.113.9 other.example.org 198.51.100.7"),
            "[host 1] [address 1] [host 2] [address 2]"
        );
    }

    /// The report ends by telling the reporter to read it through. That is only
    /// actionable if they know what the rules were meant to catch — so every
    /// mark the module can emit is named, and the two lists cannot drift
    /// because there is only one.
    #[test]
    fn the_footer_names_every_mark_a_report_can_contain() {
        let text = report(
            &test_versions(),
            &ReportContext::default(),
            &[],
            &mut plain(),
        );
        for mark in MARKS {
            assert!(
                text.contains(mark.text),
                "the footer does not name {}: {text}",
                mark.text
            );
            assert!(
                text.contains(mark.means),
                "the footer names {} without saying what it means",
                mark.text
            );
        }
    }

    /// And it says what is kept, and on what principle — a reader who sees
    /// 127.0.0.1:1080 and geoip.dat intact beside a redaction cannot otherwise
    /// tell a decision from a miss.
    #[test]
    fn the_footer_says_what_was_kept_and_that_the_rules_are_best_effort() {
        let text = report(
            &test_versions(),
            &ReportContext::default(),
            &[],
            &mut plain(),
        );
        for phrase in [
            "Loopback",
            "every port number stay",
            "geoip.dat",
            "shapes, not meanings",
            "best-effort",
        ] {
            assert!(text.contains(phrase), "the footer does not say {phrase:?}");
        }
    }

    /// A server's alias, its display name and its address are one server, and
    /// numbering them apart would read as three — the same failure numbering
    /// exists to prevent, one level up.
    #[test]
    fn every_name_one_server_goes_by_carries_that_servers_number() {
        let mut r = plain().for_servers(&[
            node("nl-soda", "Soda NL", "one.example"),
            Server {
                id: "fedcba9876543210".to_string(),
                ..node("de-frank", "Frank DE", "two.example")
            },
        ]);
        assert_eq!(
            r.line("s-nl-soda reached one.example, not Soda"),
            "s-[node 1] reached [node 1], not Soda",
            "one server read as two"
        );
        assert_eq!(
            r.line("s-de-frank is elsewhere"),
            "s-[node 2] is elsewhere",
            "the second server did not get a second number"
        );
    }

    /// A caller passing bare names has said nothing about which of them are the
    /// same server, so each stands for itself. Named handles are how the
    /// grouping is expressed, and `for_servers` is the one place that does it —
    /// which is what keeps the CLI and the window from grouping differently.
    #[test]
    fn bare_handles_stand_for_themselves() {
        let mut r = plain().with_handles(vec!["nl-soda".to_string(), "one.example".to_string()]);
        assert_eq!(
            r.line("s-nl-soda reached one.example"),
            "s-[node 1] reached [node 2]"
        );
    }

    /// A report with no lines selected says so, rather than showing a heading
    /// with nothing under it — which reads as a report that lost them.
    #[test]
    fn a_report_with_nothing_selected_says_nothing_was_selected() {
        let text = report(
            &test_versions(),
            &ReportContext::default(),
            &[],
            &mut plain(),
        );
        assert!(
            text.contains("\nLog lines\n(none were selected)\n"),
            "{text}"
        );
        assert!(text.contains("Connection: not connected\n"), "{text}");
    }
}
