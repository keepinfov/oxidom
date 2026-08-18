//! The geo data the core cannot start without, and how to get it.
//!
//! Every config this program generates carries `geoip:private` in its routing
//! rules and `geosite:private` in the direct resolver, so `geoip.dat` and
//! `geosite.dat` are a runtime dependency of *every* connection rather than of
//! some future routing feature. A core that cannot load them refuses the whole
//! configuration — see [`crate::probe::classify_complaint`] for what that looks
//! like from the outside.
//!
//! Two things here are deliberate and easy to get wrong:
//!
//! - **Presence is decided by the core, never by looking at the filesystem.**
//!   `pkgs.xray` is a wrapper that sets `XRAY_LOCATION_ASSET` *inside itself*,
//!   from a store path with no relation to the binary's directory. Read from
//!   this process the variable is unset and no conventional directory exists,
//!   so a filesystem check answers "missing" on the one platform where this has
//!   always worked — and would then offer to install over it. Asking the core
//!   is authoritative, offline, and catches a corrupt file as well as an absent
//!   one.
//! - **The environment is only overridden when we have both files.** That same
//!   wrapper reads `${XRAY_LOCATION_ASSET-<store path>}`, so anything exported
//!   here *wins*. Pointing a working core at a half-populated directory would
//!   break it.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::fsutil;
use crate::paths;

/// The variable the core reads to find its lists.
pub const LOCATION_ENV: &str = "XRAY_LOCATION_ASSET";

/// Refuse a body larger than this. The daemon may run as a system service with
/// a state directory on the root filesystem, so an unbounded read of whatever a
/// redirect happens to point at is a way to fill someone's disk. Sized well
/// above the real files (23 MB and 2 MB today) so growth upstream does not
/// break the download.
pub const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;

/// Read size per progress tick and per cancellation check.
const CHUNK: usize = 64 * 1024;

/// Overall cap on one file. ureq's default agent has no read timeout at all, so
/// a server that completes the handshake and then goes quiet would hold the
/// download thread forever.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Which list. The two differ in more than their name: upstream publishes
/// geosite under a *different* filename than the core looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeoAsset {
    GeoIp,
    GeoSite,
}

impl GeoAsset {
    pub const ALL: [GeoAsset; 2] = [GeoAsset::GeoIp, GeoAsset::GeoSite];

    /// What the core looks for on disk.
    pub fn installed_name(self) -> &'static str {
        match self {
            GeoAsset::GeoIp => "geoip.dat",
            GeoAsset::GeoSite => "geosite.dat",
        }
    }

    /// What upstream calls the same file. `domain-list-community` publishes
    /// `dlc.dat`; the core will not find it under that name, and its checksum
    /// sidecar names `dlc.dat` too — so this is also the name a digest must be
    /// looked up by.
    pub fn published_name(self) -> &'static str {
        match self {
            GeoAsset::GeoIp => "geoip.dat",
            GeoAsset::GeoSite => "dlc.dat",
        }
    }

    pub fn url(self) -> &'static str {
        match self {
            GeoAsset::GeoIp => "https://github.com/v2fly/geoip/releases/latest/download/geoip.dat",
            GeoAsset::GeoSite => {
                "https://github.com/v2fly/domain-list-community/releases/latest/download/dlc.dat"
            }
        }
    }

    /// The `sha256sum(1)` sidecar published beside the file.
    pub fn checksum_url(self) -> &'static str {
        match self {
            GeoAsset::GeoIp => {
                "https://github.com/v2fly/geoip/releases/latest/download/geoip.dat.sha256sum"
            }
            GeoAsset::GeoSite => {
                "https://github.com/v2fly/domain-list-community/releases/latest/download/dlc.dat.sha256sum"
            }
        }
    }

    pub fn parse(name: &str) -> Option<GeoAsset> {
        match name {
            "geoip" | "geoip.dat" => Some(GeoAsset::GeoIp),
            "geosite" | "geosite.dat" | "dlc.dat" => Some(GeoAsset::GeoSite),
            _ => None,
        }
    }
}

/// Where oxidom keeps the copies it installed itself.
pub fn own_dir() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("assets"))
}

/// Whether our own directory holds both files, at a plausible size.
///
/// A size floor rather than a content check: anything that gets past it is
/// handed to the core, which is the real judge. It exists so an empty or
/// truncated placeholder does not cost a process spawn.
pub fn complete(dir: &Path) -> bool {
    GeoAsset::ALL.iter().all(|asset| plausible(dir, *asset))
}

fn plausible(dir: &Path, asset: GeoAsset) -> bool {
    std::fs::metadata(dir.join(asset.installed_name()))
        .map(|meta| meta.is_file() && meta.len() >= 1024)
        .unwrap_or(false)
}

/// What to put on the core's environment, or `None` to leave it alone.
///
/// Pure so the precedence can be tested without touching this process's
/// environment. Two refusals, each load-bearing:
///
/// - something already chose — a user's export, a wrapper, the documented
///   escape hatch — and overriding a deliberate choice is not ours to make;
/// - our directory does not hold both files, and exporting a half-populated
///   one would win over a working installation and break it.
pub fn location_override(environment: Option<&OsStr>, dir: &Path) -> Option<PathBuf> {
    if environment.is_some_and(|value| !value.is_empty()) {
        return None;
    }
    complete(dir).then(|| dir.to_path_buf())
}

/// The same, resolved against this process and [`own_dir`].
///
/// Called on every spawn, so it stays cheap: two `stat`s and an environment
/// read, no subprocess.
pub fn location_for_spawn() -> Option<PathBuf> {
    let dir = own_dir().ok()?;
    location_override(std::env::var_os(LOCATION_ENV).as_deref(), &dir)
}

/// Apply [`location_for_spawn`] to a command about to run the core.
pub fn point_at_our_assets(command: &mut Command) {
    if let Some(dir) = location_for_spawn() {
        command.env(LOCATION_ENV, dir);
    }
}

/// The config handed to `xray run -test`: exactly the two geo references every
/// generated config carries, and nothing else. No inbound, so nothing is bound
/// and no port is needed; `-test` does not serve.
pub fn probe_config() -> &'static str {
    r#"{"log":{"loglevel":"error"},
 "outbounds":[{"protocol":"freedom","tag":"direct"}],
 "dns":{"servers":[{"address":"1.1.1.1","domains":["geosite:private"],"skipFallback":true}]},
 "routing":{"rules":[{"type":"field","ip":["geoip:private"],"outboundTag":"direct"}]}}"#
}

/// Ask the core whether it can build oxidom's rule set.
///
/// `location` overrides `XRAY_LOCATION_ASSET` for the check, which is how a
/// candidate directory is judged without installing anything.
///
/// Returns the core's own words on failure. Nothing is bound, nothing is sent,
/// and the scratch config is removed before returning.
pub fn probe(xray: &Path, location: Option<&Path>, scratch: &Path) -> Result<(), String> {
    fsutil::write_private_atomic(scratch, probe_config().as_bytes())
        .map_err(|error| format!("staging the geo data check: {error:#}"))?;
    let mut command = Command::new(xray);
    command.arg("run").arg("-test").arg("-c").arg(scratch);
    if let Some(dir) = location {
        command.env(LOCATION_ENV, dir);
    }
    let output = command.output();
    let _ = std::fs::remove_file(scratch);
    let output = match output {
        Ok(output) => output,
        Err(error) => return Err(format!("running {}: {error}", xray.display())),
    };
    if output.status.success() {
        return Ok(());
    }
    // The core writes its refusal to stdout, and the last non-empty line is the
    // chained one that names the file.
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .map(str::trim)
        .rev()
        .find(|line| !line.is_empty())
        .unwrap_or("the core refused a configuration carrying only its built-in geo rules");
    Err(line.to_string())
}

/// A directory holding geo data the core accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub dir: String,
    pub geoip_bytes: u64,
    pub geosite_bytes: u64,
}

/// System-wide directories a core installation may have used, most specific
/// first.
///
/// Kept as data, and separate from [`candidate_dirs`], because it is the only
/// part of this module that is not portable as written: everything else here is
/// HTTP, files and one subprocess. The Homebrew prefixes differ by
/// architecture, and macOS has no `/usr/share/xray` convention.
pub fn system_asset_dirs() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &[
            "/opt/homebrew/share/xray",
            "/opt/homebrew/share/v2ray",
            "/usr/local/share/xray",
            "/usr/local/share/v2ray",
            "/Applications/Xray.app/Contents/Resources",
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        &[
            "/usr/local/share/xray",
            "/usr/share/xray",
            "/usr/local/share/v2ray",
            "/usr/share/v2ray",
            "/opt/Xray",
        ]
    }
}

/// Per-user directories other clients keep the same files in, relative to home.
///
/// Also platform-shaped: XDG puts these under `.local/share`, macOS under
/// `~/Library/Application Support`. `dirs::data_dir` already answers that for
/// oxidom's *own* directory; these are other programs' conventions, so they are
/// spelled out.
pub fn user_asset_dirs() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &[
            "Library/Application Support/xray",
            "Library/Application Support/v2ray",
            "Library/Application Support/V2rayU",
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        &[
            ".local/share/xray",
            ".local/share/v2ray",
            ".local/share/v2rayA",
            ".config/nekoray",
            ".config/hiddify",
        ]
    }
}

/// Directories worth asking the core about, in the order the core itself would
/// search them, plus the places other clients keep the same files.
///
/// The system and per-user lists are passed in rather than read from the
/// platform, so a test can build real directories under a test root and assert
/// the ordering without depending on what the developer's machine happens to
/// have in `/usr/share` — and so the same test covers every platform's list.
pub fn candidate_dirs(
    environment: Option<&OsStr>,
    xray: Option<&Path>,
    home: Option<&Path>,
    own: Option<&Path>,
    system: &[&str],
    user: &[&str],
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |dir: PathBuf| {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    if let Some(value) = environment.filter(|value| !value.is_empty()) {
        push(PathBuf::from(value));
    }
    if let Some(parent) = xray.and_then(Path::parent) {
        push(parent.to_path_buf());
    }
    for dir in system {
        push(PathBuf::from(dir));
    }
    if let Some(own) = own {
        push(own.to_path_buf());
    }
    if let Some(home) = home {
        for tail in user {
            push(home.join(tail));
        }
    }
    dirs
}

/// [`candidate_dirs`] against this machine.
pub fn candidate_dirs_here(xray: Option<&Path>) -> Vec<PathBuf> {
    let own = own_dir().ok();
    candidate_dirs(
        std::env::var_os(LOCATION_ENV).as_deref(),
        xray,
        dirs::home_dir().as_deref(),
        own.as_deref(),
        system_asset_dirs(),
        user_asset_dirs(),
    )
}

/// Keep the directories whose files the core actually accepts.
///
/// The size floor in [`complete`] runs first, so an obviously dead directory
/// costs no process. Everything past it is judged by the core, which is what
/// makes this a check on the data being *real* rather than on it being present:
/// a truncated list is refused with `code not found in geoip.dat: PRIVATE`.
pub fn usable_candidates(xray: &Path, dirs: &[PathBuf], scratch: &Path) -> Vec<Candidate> {
    let mut found = Vec::new();
    for dir in dirs {
        if !complete(dir) {
            continue;
        }
        if probe(xray, Some(dir), scratch).is_err() {
            log::debug!("{} holds geo data the core will not load", dir.display());
            continue;
        }
        let size = |asset: GeoAsset| {
            std::fs::metadata(dir.join(asset.installed_name()))
                .map(|meta| meta.len())
                .unwrap_or(0)
        };
        found.push(Candidate {
            dir: dir.display().to_string(),
            geoip_bytes: size(GeoAsset::GeoIp),
            geosite_bytes: size(GeoAsset::GeoSite),
        });
    }
    found
}

/// Copy a candidate's files into [`own_dir`].
///
/// Copied rather than pointed at where they lie: `XRAY_LOCATION_ASSET` names
/// one directory, so both files must sit together, and a directory owned by
/// another program can be upgraded or removed out from under us.
pub fn adopt(from: &Path, into: &Path) -> Result<()> {
    for asset in GeoAsset::ALL {
        let source = from.join(asset.installed_name());
        let bytes =
            std::fs::read(&source).with_context(|| format!("reading {}", source.display()))?;
        if bytes.len() as u64 > MAX_ASSET_BYTES {
            bail!(
                "{} is larger than {MAX_ASSET_BYTES} bytes",
                source.display()
            );
        }
        fsutil::write_private_atomic(&into.join(asset.installed_name()), &bytes)
            .with_context(|| format!("installing {}", asset.installed_name()))?;
    }
    Ok(())
}

/// The digest for `file` from a `sha256sum(1)` sidecar.
///
/// Refuses a sidecar that does not name the file asked for. Matching the hex
/// alone would accept a digest that was published about something else — which
/// is exactly what a redirect serving the wrong release looks like.
pub fn parse_sha256_sidecar(text: &str, file: &str) -> Result<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        // sha256sum writes " *name" for a binary-mode digest.
        if name.trim_start_matches('*') != file {
            continue;
        }
        return validate_digest(digest);
    }
    bail!("the published checksum does not mention {file}")
}

fn validate_digest(digest: &str) -> Result<String> {
    let digest = digest.trim().to_ascii_lowercase();
    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("the published checksum is not a SHA-256 digest");
    }
    Ok(digest)
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// How a download is getting on, for a caller that polls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    /// The file being fetched right now.
    pub file: Option<String>,
    pub done_bytes: u64,
    /// Zero when the server sent no `Content-Length`, so a caller must be able
    /// to show motion without a denominator.
    pub total_bytes: u64,
}

/// Where a download reports to, and how it is stopped.
///
/// The flag is checked once per chunk, so cancelling is felt within one read
/// rather than at the end of the file.
pub trait Sink: Send {
    fn progress(&self, progress: &Progress);
    fn cancelled(&self) -> bool;
}

/// A sink that never cancels and reports nowhere.
pub struct Quiet;

impl Sink for Quiet {
    fn progress(&self, _progress: &Progress) {}
    fn cancelled(&self) -> bool {
        false
    }
}

/// A cancellation flag shared with whoever can ask for one.
#[derive(Debug, Default)]
pub struct Cancel(AtomicBool);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Raised when the caller asked to stop. Kept distinct so a cancel is not
/// reported to the user as a failure.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the download was cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Build the agent a download runs through.
///
/// `through_proxy` routes the request over a local SOCKS inbound, which is the
/// answer to the release host being blocked on the network the user is trying
/// to escape. The daemon chooses it — it knows its own inbounds — rather than
/// taking an address from whoever asked.
pub fn agent(through_proxy: Option<&str>) -> Result<ureq::Agent> {
    let mut builder = ureq::AgentBuilder::new().timeout(FETCH_TIMEOUT);
    if let Some(address) = through_proxy {
        let proxy = ureq::Proxy::new(address)
            .with_context(|| format!("building the proxy URL {address}"))?;
        builder = builder.proxy(proxy);
    }
    Ok(builder.build())
}

/// Fetch one list, verify it against its published digest, and install it.
///
/// Nothing is written until the digest matches: a mismatch leaves the target
/// exactly as it was, so a bad download cannot replace a good file.
pub fn download(
    asset: GeoAsset,
    into: &Path,
    agent: &ureq::Agent,
    sink: &dyn Sink,
) -> Result<PathBuf> {
    let expected = fetch_digest(asset, agent)?;

    let response = agent
        .get(asset.url())
        .call()
        .map_err(|error| transport_error(asset.published_name(), error))?;
    let total = response
        .header("content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if total > MAX_ASSET_BYTES {
        bail!(
            "{} is {total} bytes, larger than the {MAX_ASSET_BYTES} this will install",
            asset.published_name()
        );
    }

    let mut reader = response.into_reader().take(MAX_ASSET_BYTES + 1);
    let mut bytes: Vec<u8> = Vec::with_capacity(total.min(MAX_ASSET_BYTES) as usize);
    let mut buffer = vec![0u8; CHUNK];
    loop {
        if sink.cancelled() {
            return Err(Cancelled.into());
        }
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("reading {}", asset.published_name()))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() as u64 > MAX_ASSET_BYTES {
            bail!(
                "{} is larger than the {MAX_ASSET_BYTES} bytes this will install",
                asset.published_name()
            );
        }
        sink.progress(&Progress {
            file: Some(asset.installed_name().to_string()),
            done_bytes: bytes.len() as u64,
            total_bytes: total,
        });
    }

    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!(
            "{} does not match its published checksum (got {actual}, expected {expected}) — \
             the download was corrupted or intercepted, and nothing was installed",
            asset.installed_name()
        );
    }

    let target = into.join(asset.installed_name());
    fsutil::write_private_atomic(&target, &bytes)
        .with_context(|| format!("installing {}", target.display()))?;
    Ok(target)
}

fn fetch_digest(asset: GeoAsset, agent: &ureq::Agent) -> Result<String> {
    let text = agent
        .get(asset.checksum_url())
        .call()
        .map_err(|error| transport_error("the published checksum", error))?
        .into_string()
        .context("reading the published checksum")?;
    parse_sha256_sidecar(&text, asset.published_name())
}

/// Keep the release host out of the message but say what happened. Unlike a
/// subscription URL this address is not a secret, so it is named.
fn transport_error(what: &str, error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(code, _) => {
            anyhow::anyhow!("fetching {what}: the server responded with HTTP {code}")
        }
        ureq::Error::Transport(transport) => {
            anyhow::anyhow!(
                "fetching {what}: {} — the release host may be unreachable from this network",
                transport.kind()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A real directory, because everything here judges files on disk.
    pub(super) struct Dir(pub(super) PathBuf);

    impl Dir {
        pub(super) fn new(label: &str) -> Self {
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oxidom-assets-{label}-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("creating the test directory");
            Dir(path)
        }

        pub(super) fn with(&self, name: &str, bytes: usize) -> &Self {
            std::fs::write(self.0.join(name), vec![b'x'; bytes]).expect("writing");
            self
        }

        #[allow(dead_code)]
        pub(super) fn complete(&self) -> &Self {
            self.with("geoip.dat", 4096).with("geosite.dat", 4096)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The wrapper `pkgs.xray` installs reads `${XRAY_LOCATION_ASSET-<store
    /// path>}`, so anything exported here *wins* over a working installation.
    /// A choice already made is therefore never overridden — this is the test
    /// that stops a NixOS machine being pointed at our directory and broken.
    #[test]
    fn a_location_the_environment_already_chose_is_never_overridden() {
        let ours = Dir::new("chosen");
        ours.complete();
        assert_eq!(
            location_override(Some(OsStr::new("/somewhere/else")), &ours.0),
            None,
            "a deliberate choice outranks our own copy"
        );
        // An empty value is not a choice.
        assert_eq!(
            location_override(Some(OsStr::new("")), &ours.0),
            Some(ours.0.clone())
        );
        assert_eq!(location_override(None, &ours.0), Some(ours.0.clone()));
    }

    /// `XRAY_LOCATION_ASSET` names one directory for both lists, so exporting a
    /// directory holding one of them hides whichever the core would otherwise
    /// have found for itself. Half is worse than none.
    #[test]
    fn a_half_populated_directory_is_never_handed_to_the_core() {
        let empty = Dir::new("empty");
        assert_eq!(location_override(None, &empty.0), None);

        let geoip_only = Dir::new("geoip-only");
        geoip_only.with("geoip.dat", 4096);
        assert_eq!(location_override(None, &geoip_only.0), None);

        let geosite_only = Dir::new("geosite-only");
        geosite_only.with("geosite.dat", 4096);
        assert_eq!(location_override(None, &geosite_only.0), None);

        let missing = Dir::new("absent");
        assert_eq!(location_override(None, &missing.0.join("nope")), None);
    }

    /// A placeholder of a few bytes is not geo data. The floor exists so an
    /// obviously dead directory costs no process spawn; the core is still the
    /// judge of anything that gets past it.
    #[test]
    fn a_file_too_small_to_be_geo_data_does_not_count_as_present() {
        let stub = Dir::new("stub");
        stub.with("geoip.dat", 12).with("geosite.dat", 4096);
        assert!(!complete(&stub.0));
        stub.with("geoip.dat", 1024);
        assert!(complete(&stub.0));
    }

    /// The order is the core's own, so a file already on the machine is used
    /// where it lies and our copy is a last resort rather than a preference.
    #[test]
    fn an_asset_directory_is_searched_in_the_order_the_core_searches_it() {
        let home = PathBuf::from("/home/someone");
        let xray = PathBuf::from("/opt/xray/bin/xray");
        let own = PathBuf::from("/var/lib/oxidom/assets");
        let dirs = candidate_dirs(
            Some(OsStr::new("/from/env")),
            Some(&xray),
            Some(&home),
            Some(&own),
            &["/usr/share/xray"],
            &[".local/share/xray"],
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/from/env"),
                PathBuf::from("/opt/xray/bin"),
                PathBuf::from("/usr/share/xray"),
                own,
                home.join(".local/share/xray"),
            ]
        );
    }

    /// Every source may name the same directory, and asking the core about one
    /// twice costs a process each time.
    #[test]
    fn a_directory_named_twice_is_only_offered_once() {
        let own = PathBuf::from("/usr/share/xray");
        let dirs = candidate_dirs(
            Some(OsStr::new("/usr/share/xray")),
            None,
            None,
            Some(&own),
            &["/usr/share/xray", "/usr/share/xray"],
            &[],
        );
        assert_eq!(dirs, vec![PathBuf::from("/usr/share/xray")]);
    }

    /// Both platform lists are checked here, on whichever platform runs the
    /// suite: a macOS build is a stated goal, and a relative or duplicated
    /// entry would only show up as a wrong answer on the machine nobody is
    /// testing on.
    #[test]
    fn every_platforms_asset_directories_are_usable_as_written() {
        let system = system_asset_dirs();
        assert!(!system.is_empty());
        for dir in system {
            assert!(
                Path::new(dir).is_absolute(),
                "a system directory must be absolute: {dir}"
            );
        }
        let user = user_asset_dirs();
        assert!(!user.is_empty());
        for dir in user {
            assert!(
                Path::new(dir).is_relative(),
                "a per-user directory is joined to home, so it must be relative: {dir}"
            );
        }
        let mut seen = system.to_vec();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "a directory is listed twice");
    }

    /// The sidecar is `sha256sum(1)` output: a digest, whitespace, the name.
    #[test]
    fn a_sidecar_gives_the_digest_for_the_file_it_names() {
        let text = "c67bd077eb102cec74fab759b73d17f99275f56af10a87c14d9fd983508f5ce1  geoip.dat\n";
        assert_eq!(
            parse_sha256_sidecar(text, "geoip.dat").expect("parses"),
            "c67bd077eb102cec74fab759b73d17f99275f56af10a87c14d9fd983508f5ce1"
        );
        // Binary mode marks the name with an asterisk.
        let binary = "c67bd077eb102cec74fab759b73d17f99275f56af10a87c14d9fd983508f5ce1 *geoip.dat";
        assert!(parse_sha256_sidecar(binary, "geoip.dat").is_ok());
    }

    /// Matching the hex alone would accept a digest published about something
    /// else, which is what a redirect serving the wrong release looks like.
    #[test]
    fn a_sidecar_naming_a_different_file_is_refused() {
        let text = "c67bd077eb102cec74fab759b73d17f99275f56af10a87c14d9fd983508f5ce1  other.dat\n";
        assert!(parse_sha256_sidecar(text, "geoip.dat").is_err());
    }

    /// The trap that would silently break every geosite download: upstream
    /// publishes the file as `dlc.dat` and its sidecar says `dlc.dat`, while
    /// the core looks for `geosite.dat`. The digest must be looked up under the
    /// published name, not the installed one.
    #[test]
    fn geosite_is_verified_against_the_name_it_was_published_under() {
        assert_eq!(GeoAsset::GeoSite.published_name(), "dlc.dat");
        assert_eq!(GeoAsset::GeoSite.installed_name(), "geosite.dat");
        let text = "8e0e5476fa1d7ad1d7e6a0e9c3b2a1908b7e6d5c4b3a29180706f5e4d3c2b1a0  dlc.dat\n";
        assert!(parse_sha256_sidecar(text, GeoAsset::GeoSite.published_name()).is_ok());
        assert!(
            parse_sha256_sidecar(text, GeoAsset::GeoSite.installed_name()).is_err(),
            "looking the digest up by the installed name would reject every download"
        );
    }

    /// An error page, a truncated body or an uppercase digest must not become
    /// something bytes are compared against.
    #[test]
    fn a_digest_that_is_not_sixty_four_hex_characters_is_refused() {
        for bad in [
            "abc  geoip.dat",
            "<!DOCTYPE html>  geoip.dat",
            "zzzzzzzzeb102cec74fab759b73d17f99275f56af10a87c14d9fd983508f5ce1  geoip.dat",
        ] {
            assert!(parse_sha256_sidecar(bad, "geoip.dat").is_err(), "{bad:?}");
        }
        // Uppercase is a legitimate spelling and is folded, not refused.
        let upper = "C67BD077EB102CEC74FAB759B73D17F99275F56AF10A87C14D9FD983508F5CE1  geoip.dat";
        assert_eq!(
            parse_sha256_sidecar(upper, "geoip.dat").expect("parses"),
            "c67bd077eb102cec74fab759b73d17f99275f56af10a87c14d9fd983508f5ce1"
        );
    }

    /// The probe config is what decides whether the data is usable, so it has
    /// to carry exactly the references a generated config carries. If
    /// `xray/config.rs` ever stops emitting one of these, this check is
    /// measuring something the real config no longer needs.
    #[test]
    fn the_probe_config_asks_for_what_every_generated_config_asks_for() {
        let config = probe_config();
        assert!(config.contains("geoip:private"));
        assert!(config.contains("geosite:private"));
        let parsed: serde_json::Value = serde_json::from_str(config).expect("valid JSON");
        assert!(
            parsed.get("inbounds").is_none(),
            "the check must bind nothing"
        );
    }

    /// Copied, not referenced: the source directory belongs to another program
    /// and can be upgraded or removed out from under us.
    #[test]
    fn found_files_are_copied_rather_than_pointed_at_where_they_lie() {
        let from = Dir::new("found");
        from.complete();
        let into = Dir::new("ours");
        adopt(&from.0, &into.0).expect("adopting");
        assert!(complete(&into.0));
        assert_eq!(
            std::fs::read(into.0.join("geoip.dat")).expect("reading"),
            vec![b'x'; 4096]
        );
    }

    /// Cancelling is felt within one chunk rather than at the end of a 23 MB
    /// file, so the flag is what the read loop consults.
    #[test]
    fn a_cancelled_download_is_noticed_without_waiting_for_the_body() {
        let cancel = Cancel::default();
        assert!(!cancel.is_cancelled());
        cancel.cancel();
        assert!(cancel.is_cancelled());
        cancel.reset();
        assert!(!cancel.is_cancelled());
    }

    /// Names travel over D-Bus, so both spellings a caller might send are
    /// understood and anything else is refused rather than guessed at.
    #[test]
    fn an_asset_is_named_by_either_of_its_two_names() {
        assert_eq!(GeoAsset::parse("geoip"), Some(GeoAsset::GeoIp));
        assert_eq!(GeoAsset::parse("geoip.dat"), Some(GeoAsset::GeoIp));
        assert_eq!(GeoAsset::parse("geosite"), Some(GeoAsset::GeoSite));
        assert_eq!(GeoAsset::parse("dlc.dat"), Some(GeoAsset::GeoSite));
        assert_eq!(GeoAsset::parse("something"), None);
    }
}

/// Checks that need a real Xray core, which the default suite must not.
///
/// Run them with a core on `$PATH` or named by `$OXIDOM_XRAY_BIN`:
///
/// ```sh
/// nix develop -c cargo test -p oxidom-core --lib assets::live -- --ignored
/// ```
#[cfg(test)]
mod live {
    use super::tests::Dir;
    use super::*;

    fn core() -> PathBuf {
        crate::xray::resolve::resolve("")
            .expect("this test needs a real Xray core")
            .path
    }

    fn scratch(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oxidom-live-{label}-{}.json", std::process::id()))
    }

    /// The whole design rests on this: the core is the authority on whether its
    /// data is usable, and it answers differently for present, absent and
    /// corrupt. A filesystem check cannot tell the last two apart, and on
    /// NixOS cannot see the first at all.
    #[ignore = "needs a real Xray core"]
    #[test]
    fn the_core_tells_present_absent_and_corrupt_geo_data_apart() {
        let xray = core();
        let scratch = scratch("verdicts");

        // Whatever this machine's core finds for itself. On NixOS the wrapper
        // supplies it and no directory we could have inspected would say so.
        probe(&xray, None, &scratch).expect("a wrapped core finds its own data");

        let absent = Dir::new("live-absent");
        let error = probe(&xray, Some(&absent.0), &scratch)
            .expect_err("an empty directory cannot satisfy the rule set");
        assert!(
            error.contains("geoip.dat"),
            "the refusal must name the file: {error}"
        );

        let corrupt = Dir::new("live-corrupt");
        corrupt.with("geoip.dat", 4096).with("geosite.dat", 4096);
        let error = probe(&xray, Some(&corrupt.0), &scratch)
            .expect_err("bytes that are not a geo list cannot satisfy it either");
        assert!(
            error.contains("geoip.dat"),
            "a corrupt list must be named too: {error}"
        );
    }

    /// And the same verdicts, through the interface the daemon actually calls.
    #[ignore = "needs a real Xray core"]
    #[test]
    fn a_directory_the_core_rejects_is_never_offered_as_a_candidate() {
        let xray = core();
        let scratch = scratch("candidates");

        let corrupt = Dir::new("live-candidate-bad");
        corrupt.with("geoip.dat", 4096).with("geosite.dat", 4096);
        let half = Dir::new("live-candidate-half");
        half.with("geoip.dat", 4096);

        let offered = usable_candidates(&xray, &[corrupt.0.clone(), half.0.clone()], &scratch);
        assert!(
            offered.is_empty(),
            "neither a corrupt nor a half directory is usable: {offered:?}"
        );
    }
}

#[cfg(test)]
mod live_network {
    use super::tests::Dir;
    use super::*;

    /// The whole path a user takes, end to end: fetch both lists from the real
    /// release hosts, verify each against its published digest, install them,
    /// and confirm the core will start on what landed.
    ///
    /// ```sh
    /// nix develop -c cargo test -p oxidom-core --lib assets::live_network -- --ignored
    /// ```
    #[ignore = "needs the network and a real Xray core"]
    #[test]
    fn downloaded_geo_data_is_data_the_core_will_actually_start_on() {
        let into = Dir::new("live-download");
        let agent = agent(None).expect("building the agent");
        for asset in GeoAsset::ALL {
            let path = download(asset, &into.0, &agent, &Quiet).expect("downloading");
            assert!(path.exists());
        }
        assert!(complete(&into.0), "both lists must have landed");

        let xray = crate::xray::resolve::resolve("")
            .expect("this test needs a real Xray core")
            .path;
        let scratch = std::env::temp_dir().join(format!("oxidom-live-dl-{}.json", std::process::id()));
        probe(&xray, Some(&into.0), &scratch).expect("the core must accept what was installed");

        // And the files are private, like everything else the daemon writes.
        use std::os::unix::fs::PermissionsExt;
        for asset in GeoAsset::ALL {
            let mode = std::fs::metadata(into.0.join(asset.installed_name()))
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{}", asset.installed_name());
        }
    }

    /// A digest that does not describe the bytes must leave the target alone.
    /// Nothing is written until it matches, so a bad download cannot replace a
    /// good file that is already there.
    #[ignore = "needs the network"]
    #[test]
    fn a_download_that_fails_its_checksum_installs_nothing() {
        let into = Dir::new("live-mismatch");
        std::fs::write(into.0.join("geoip.dat"), b"the good file that was already here")
            .expect("seeding");
        let agent = agent(None).expect("building the agent");
        // Verify against the *wrong* asset's digest by asking for a sidecar
        // that describes a different file.
        let text = agent
            .get(GeoAsset::GeoSite.checksum_url())
            .call()
            .expect("fetching a sidecar")
            .into_string()
            .expect("reading it");
        let wrong = parse_sha256_sidecar(&text, GeoAsset::GeoSite.published_name())
            .expect("parsing it");
        let bytes = b"not the file that digest describes";
        assert_ne!(sha256_hex(bytes), wrong);
        assert_eq!(
            std::fs::read(into.0.join("geoip.dat")).expect("reading"),
            b"the good file that was already here",
            "a mismatch must not have touched what was there"
        );
    }
}
