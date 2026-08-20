//! Which versions are running, and how they got here.
//!
//! Five questions the bug form asks — the application's version, how it was
//! installed, which daemon answered, what `xray version` says, and the
//! distribution and desktop — and every one of them is something the machine
//! already knows. Asking a reporter to go and look them up is how they arrive
//! wrong: `oxidom --version` is the version of whichever binary is first on
//! `$PATH`, which on a machine with both a package and a build is not the one
//! that just misbehaved.
//!
//! Everything here is a plain value over plain inputs, so the graphical
//! client's About window and anything else that needs the same block — a
//! problem report, `oxidom status` — assemble it the same way rather than each
//! writing its own. Nothing in this module talks to the daemon; the daemon's
//! and the core's versions arrive through [`crate::ipc::RuntimeInfo`] and are
//! passed in.

use std::path::Path;

use crate::client::DaemonSource;

/// How oxidom itself got onto this machine.
///
/// A judgement from the path the process was started as, not a fact: nothing
/// records the answer at install time, and the packaging formats that could be
/// asked directly — dpkg, rpm — would each need querying and would still be
/// silent about the others. The path is what every one of them differs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Install {
    /// Somewhere under `/usr`, which belongs to a package manager. The apt and
    /// rpm repositories, a downloaded `.deb` or `.rpm`, and the AUR all land
    /// here and are indistinguishable once installed, so this says the honest
    /// thing rather than picking one of the four.
    Package,
    /// The self-contained image, which sets `$APPIMAGE` to its own path.
    AppImage,
    /// A Flatpak sandbox, which sets `$FLATPAK_ID`.
    Flatpak,
    /// The Nix store, whose paths are immutable and per-build.
    Nix,
    /// `cargo install`, which writes into the Cargo home's `bin`.
    Cargo,
    /// A build tree or a hand `install` into `/usr/local` — either way, a
    /// binary this machine produced rather than received.
    Source,
    #[default]
    Unknown,
}

impl Install {
    /// The phrase the About window shows and the copied block carries. Worded
    /// to match the bug form's dropdown, so a reporter can pick the matching
    /// entry without having to translate.
    pub fn label(self) -> &'static str {
        match self {
            Install::Package => "a distribution package (.deb, .rpm or AUR)",
            Install::AppImage => "AppImage",
            Install::Flatpak => "Flatpak",
            Install::Nix => "Nix or NixOS",
            Install::Cargo => "cargo install",
            Install::Source => "built from source",
            Install::Unknown => "unknown",
        }
    }
}

/// Judge an installation from the path a binary was started as and the two
/// environment variables the sandboxed formats set.
///
/// The environment wins over the path because both of those formats put their
/// binaries somewhere that would otherwise be read as a package: an AppImage
/// mounts itself under `/tmp`, and a Flatpak's `/app/bin` is `/usr`-shaped
/// from the inside.
pub fn install_from(exe: &Path, appimage: Option<&str>, flatpak: Option<&str>) -> Install {
    if flatpak.is_some_and(|id| !id.is_empty()) {
        return Install::Flatpak;
    }
    if appimage.is_some_and(|path| !path.is_empty()) {
        return Install::AppImage;
    }
    let path = exe.to_string_lossy();
    if path.starts_with("/nix/store/") {
        return Install::Nix;
    }
    // A build tree, whichever profile. Checked before the prefixes below
    // because a checkout can live anywhere, including under /usr/local.
    if path.contains("/target/debug/") || path.contains("/target/release/") {
        return Install::Source;
    }
    if path.contains("/.cargo/bin/") {
        return Install::Cargo;
    }
    // /usr/local is the prefix `make install` and a hand `install -D` use, and
    // it is deliberately not the package manager's, so it is checked before
    // the general /usr.
    if path.starts_with("/usr/local/") {
        return Install::Source;
    }
    if path.starts_with("/usr/") || path.starts_with("/bin/") || path.starts_with("/sbin/") {
        return Install::Package;
    }
    Install::Unknown
}

/// The answer for the process that is asking.
pub fn install_here() -> Install {
    let exe = std::env::current_exe().unwrap_or_default();
    install_from(
        &exe,
        std::env::var("APPIMAGE").ok().as_deref(),
        std::env::var("FLATPAK_ID").ok().as_deref(),
    )
}

/// What a distribution calls itself, out of an `os-release` file.
///
/// `PRETTY_NAME` is the one field os-release(5) says is meant for showing a
/// person, and it already carries the release — "Fedora Linux 42 (Workstation
/// Edition)". The fallbacks exist because a minimal image may carry only
/// `NAME` and `VERSION_ID`, and a container image sometimes only `ID`.
pub fn distribution(os_release: &str) -> Option<String> {
    let mut pretty = None;
    let mut name = None;
    let mut version = None;
    let mut id = None;
    for line in os_release.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "PRETTY_NAME" => pretty = Some(value.to_string()),
            "NAME" => name = Some(value.to_string()),
            "VERSION_ID" => version = Some(value.to_string()),
            "ID" => id = Some(value.to_string()),
            _ => {}
        }
    }
    if let Some(pretty) = pretty {
        return Some(pretty);
    }
    match (name, version) {
        (Some(name), Some(version)) => Some(format!("{name} {version}")),
        (Some(name), None) => Some(name),
        (None, _) => id,
    }
}

/// The distribution this process is running on.
pub fn distribution_here() -> Option<String> {
    // /usr/lib is the vendor copy; /etc wins when both exist, per os-release(5).
    let text = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok()?;
    distribution(&text)
}

/// The desktop and the display protocol, in the one phrase the bug form asks
/// for them in.
///
/// `XDG_CURRENT_DESKTOP` is a colon-separated list — Ubuntu's GNOME reports
/// `ubuntu:GNOME` — and it is kept whole. The prefix is the part that explains
/// a session's patched shell, so dropping it would throw away the informative
/// half.
pub fn desktop(current_desktop: Option<&str>, session_type: Option<&str>) -> Option<String> {
    let clean = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    match (clean(current_desktop), clean(session_type)) {
        (Some(desktop), Some(session)) => Some(format!("{desktop}, {session}")),
        (Some(desktop), None) => Some(desktop),
        (None, Some(session)) => Some(session),
        (None, None) => None,
    }
}

/// The desktop this process is running under.
pub fn desktop_here() -> Option<String> {
    desktop(
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
    )
}

/// The core's name and version, out of what `xray version` printed.
///
/// The core answers with a paragraph — `Xray 26.3.27 (Xray, Penetrator of the
/// Great Firewall) Custom (go1.24.0 linux/amd64)`, then a blank line and a
/// tagline — and only the head of the first line is wanted. Cutting at the
/// first parenthesis leaves the name and the version, which is the shape the
/// bug form asks for, and leaves the same shape for a v2ray core, whose first
/// line differs only in the name.
pub fn core_version(stdout: &str) -> Option<String> {
    let first = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let head = match first.split_once('(') {
        Some((head, _)) => head.trim(),
        None => first,
    };
    if head.is_empty() {
        return None;
    }
    Some(head.to_string())
}

/// Everything the About window shows and the bug form asks for.
///
/// `None` on a version means the question was asked and not answered, which is
/// a different thing from a version of "unknown" and is why these are not
/// strings with a placeholder baked in: only the renderer should choose the
/// words for silence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Versions {
    /// The version of the binary assembling this block — the graphical client
    /// when the About window asks, the CLI when it does.
    pub app: String,
    /// The daemon's, as it reported it. `None` from a daemon that predates the
    /// field, which is itself the answer: see [`Versions::skew`].
    pub daemon: Option<String>,
    /// What `xray version` said, or `None` when no core resolved or it could
    /// not be run.
    pub core: Option<String>,
    /// Which daemon answered. `None` before one has.
    pub source: Option<DaemonSource>,
    pub install: Install,
    pub distribution: Option<String>,
    pub desktop: Option<String>,
}

impl Versions {
    /// The block for the machine this is running on, given the version of the
    /// calling binary and whatever the daemon has said so far.
    pub fn here(
        app: &str,
        daemon: Option<&str>,
        core: Option<&str>,
        source: Option<DaemonSource>,
    ) -> Self {
        Versions {
            app: app.to_string(),
            daemon: daemon.map(str::to_string),
            core: core.map(str::to_string),
            source,
            install: install_here(),
            distribution: distribution_here(),
            desktop: desktop_here(),
        }
    }

    /// How to describe the daemon that answered.
    ///
    /// A daemon this process started is a session daemon — it holds the same
    /// database as one that was already running — but saying so plainly
    /// matters when a system daemon was expected and did not appear, because
    /// then the servers are not the ones the user is looking for.
    pub fn daemon_kind(&self) -> &'static str {
        match self.source {
            Some(DaemonSource::System) => "the system daemon",
            Some(DaemonSource::Session) => "the session daemon",
            Some(DaemonSource::Spawned) => "a session daemon this window started",
            None => "none has answered",
        }
    }

    /// The sentence to show when the daemon is not this build, and nothing
    /// when it is.
    ///
    /// A missing version is not missing information: the field is answered by
    /// every daemon from the release that introduced it, so silence places the
    /// daemon before that release and it is provably the older of the two.
    /// Saying so is the whole point — the symptom a user meets otherwise is a
    /// control that is not there, with nothing to connect it to.
    pub fn skew(&self) -> Option<String> {
        match self.daemon.as_deref() {
            None => Some(
                "The daemon is older than this window — old enough that it cannot say which \
                 version it is. Some controls will be missing until it is restarted."
                    .to_string(),
            ),
            Some(daemon) if daemon == self.app => None,
            Some(daemon) => Some(match (triple(daemon), triple(&self.app)) {
                (Some(daemon_parts), Some(app_parts)) if daemon_parts < app_parts => format!(
                    "The daemon is {daemon}, older than this window. Some controls will be \
                     missing until it is restarted."
                ),
                (Some(daemon_parts), Some(app_parts)) if daemon_parts > app_parts => format!(
                    "The daemon is {daemon}, newer than this window. It may do things this \
                     window has no controls for."
                ),
                // Either version is not three numbers — a git build, a distribution's
                // own suffix — so which is older cannot be decided, and guessing it
                // would be worse than reporting the pair.
                _ => format!("The daemon is {daemon}, and this window is {}.", self.app),
            }),
        }
    }

    /// The labelled lines, in the bug form's order, with the words this block
    /// uses for a question that was not answered.
    pub fn rows(&self) -> Vec<(&'static str, String)> {
        let unknown = |value: &Option<String>| value.clone().unwrap_or_else(|| "unknown".into());
        vec![
            ("Version", format!("oxidom {}", self.app)),
            (
                "Daemon",
                match self.daemon.as_deref() {
                    Some(version) => format!("{version} ({})", self.daemon_kind()),
                    None => format!("too old to say ({})", self.daemon_kind()),
                },
            ),
            (
                "Xray core",
                self.core.clone().unwrap_or_else(|| "none".into()),
            ),
            ("How it was installed", self.install.label().to_string()),
            ("Distribution", unknown(&self.distribution)),
            ("Desktop", unknown(&self.desktop)),
        ]
    }

    /// What the Copy button puts on the clipboard: the same lines, plus the
    /// skew sentence where there is one, so that pasting into the bug form
    /// answers it rather than needing to be read and retyped.
    pub fn clipboard(&self) -> String {
        let mut text = String::new();
        for (label, value) in self.rows() {
            text.push_str(label);
            text.push_str(": ");
            text.push_str(&value);
            text.push('\n');
        }
        if let Some(skew) = self.skew() {
            text.push('\n');
            text.push_str(&skew);
            text.push('\n');
        }
        text
    }
}

/// A version as three numbers, for the one comparison [`Versions::skew`] makes.
///
/// `None` for anything that is not exactly three of them — `0.2.0-rc1`,
/// `0.2.0+deb`, a git description — because those order by rules this does not
/// know, and a wrong "older than" is worse than declining to say.
fn triple(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(app: &str, daemon: Option<&str>) -> Versions {
        Versions {
            app: app.to_string(),
            daemon: daemon.map(str::to_string),
            core: Some("Xray 26.3.27".to_string()),
            source: Some(DaemonSource::System),
            install: Install::Package,
            distribution: Some("Fedora Linux 42 (Workstation Edition)".to_string()),
            desktop: Some("GNOME, wayland".to_string()),
        }
    }

    /// The bytes are the core's, copied from what it prints. The tail is a
    /// build string and a tagline, and carrying either into a one-line row
    /// would push the version itself off the end of it.
    #[test]
    fn the_core_version_is_the_head_of_the_first_line() {
        let printed = "Xray 26.3.27 (Xray, Penetrator of the Great Firewall) Custom (go1.24.0 linux/amd64)\nA unified platform for anti-censorship.\n";
        assert_eq!(core_version(printed).as_deref(), Some("Xray 26.3.27"));
    }

    /// A v2ray core answers in the same shape under a different name, so the
    /// name is read rather than assumed.
    #[test]
    fn a_core_under_another_name_is_reported_under_that_name() {
        let printed = "V2Ray 5.16.1 (V2Fly, a community-driven edition of V2Ray.) Custom (go1.22.5 linux/amd64)\n";
        assert_eq!(core_version(printed).as_deref(), Some("V2Ray 5.16.1"));
    }

    /// A first line with no parenthesis at all is still a version, and cutting
    /// at a character that is not there must not empty it.
    #[test]
    fn a_first_line_without_a_build_string_survives_whole() {
        assert_eq!(core_version("Xray 1.8.4\n").as_deref(), Some("Xray 1.8.4"));
    }

    #[test]
    fn a_core_that_printed_nothing_has_no_version() {
        assert_eq!(core_version(""), None);
        assert_eq!(core_version("\n  \n"), None);
        // A line that is nothing but a build string leaves no head to report.
        assert_eq!(core_version("(go1.24.0 linux/amd64)\n"), None);
    }

    /// `PRETTY_NAME` is the field os-release(5) sets aside for showing a
    /// person, and it already carries the release.
    #[test]
    fn a_distribution_is_named_by_its_pretty_name_where_it_has_one() {
        let fedora = "NAME=\"Fedora Linux\"\nVERSION_ID=42\nID=fedora\nPRETTY_NAME=\"Fedora Linux 42 (Workstation Edition)\"\n";
        assert_eq!(
            distribution(fedora).as_deref(),
            Some("Fedora Linux 42 (Workstation Edition)")
        );
    }

    /// A minimal image may carry no pretty name, and the pair below is then
    /// the same sentence assembled by hand rather than nothing at all.
    #[test]
    fn a_distribution_without_a_pretty_name_falls_back_to_what_it_has() {
        assert_eq!(
            distribution("NAME=\"Alpine Linux\"\nVERSION_ID=3.20.3\nID=alpine\n").as_deref(),
            Some("Alpine Linux 3.20.3")
        );
        assert_eq!(
            distribution("NAME=\"Alpine Linux\"\nID=alpine\n").as_deref(),
            Some("Alpine Linux")
        );
        assert_eq!(distribution("ID=alpine\n").as_deref(), Some("alpine"));
        assert_eq!(distribution(""), None);
        // An empty value is not an answer, and must not be reported as one.
        assert_eq!(
            distribution("PRETTY_NAME=\"\"\nID=alpine\n").as_deref(),
            Some("alpine")
        );
    }

    /// Both sandboxed formats put their binaries somewhere that reads as a
    /// package from the inside, so the environment they set has to win.
    #[test]
    fn a_sandboxed_install_is_named_by_its_environment_not_its_path() {
        let flatpak = Path::new("/app/bin/oxidom-gui");
        assert_eq!(
            install_from(flatpak, None, Some("dev.keepinfov.oxidom")),
            Install::Flatpak
        );
        let appimage = Path::new("/tmp/.mount_oxidoAbCdEf/usr/bin/oxidom-gui");
        assert_eq!(
            install_from(appimage, Some("/home/someone/oxidom.AppImage"), None),
            Install::AppImage
        );
        // An empty variable is not a sandbox; the path decides again.
        assert_eq!(
            install_from(Path::new("/usr/bin/oxidom-gui"), Some(""), Some("")),
            Install::Package
        );
    }

    #[test]
    fn an_install_is_judged_from_the_prefix_it_sits_under() {
        let judge = |path: &str| install_from(Path::new(path), None, None);
        assert_eq!(judge("/usr/bin/oxidom-gui"), Install::Package);
        assert_eq!(judge("/bin/oxidom"), Install::Package);
        assert_eq!(
            judge("/nix/store/3q9k1pz-oxidom-0.2.0/bin/oxidom-gui"),
            Install::Nix
        );
        assert_eq!(judge("/usr/local/bin/oxidom-gui"), Install::Source);
        assert_eq!(judge("/home/someone/.cargo/bin/oxidom"), Install::Cargo);
        assert_eq!(
            judge("/home/someone/oxidom/target/release/oxidom-gui"),
            Install::Source
        );
        assert_eq!(
            judge("/home/someone/oxidom/target/debug/oxidom-gui"),
            Install::Source
        );
        assert_eq!(judge("/opt/vendor/oxidom-gui"), Install::Unknown);
    }

    /// A checkout can live anywhere, `/usr/local` included, and a build tree
    /// under one is still a build tree.
    #[test]
    fn a_build_tree_is_a_build_tree_wherever_it_sits() {
        assert_eq!(
            install_from(
                Path::new("/usr/local/src/oxidom/target/release/oxidom"),
                None,
                None
            ),
            Install::Source
        );
    }

    #[test]
    fn the_desktop_is_one_phrase_or_whichever_half_of_it_exists() {
        assert_eq!(
            desktop(Some("ubuntu:GNOME"), Some("wayland")).as_deref(),
            Some("ubuntu:GNOME, wayland")
        );
        assert_eq!(desktop(Some("KDE"), None).as_deref(), Some("KDE"));
        assert_eq!(desktop(None, Some("x11")).as_deref(), Some("x11"));
        assert_eq!(desktop(None, None), None);
        assert_eq!(desktop(Some("  "), Some("")), None);
    }

    /// The field exists in every daemon from the release that introduced it,
    /// so its absence places the daemon before that release. That is a fact,
    /// not a gap, and the window says it rather than leaving the user to infer
    /// it from a control that is not there.
    #[test]
    fn a_daemon_too_old_to_name_itself_is_reported_as_the_older_of_the_two() {
        let skew = block("0.2.0", None).skew().expect("a sentence");
        assert!(skew.contains("older than this window"), "{skew}");
        assert!(skew.contains("restarted"), "{skew}");
    }

    #[test]
    fn a_daemon_of_the_same_version_gets_no_sentence() {
        assert_eq!(block("0.2.0", Some("0.2.0")).skew(), None);
    }

    #[test]
    fn a_numbered_daemon_is_placed_on_the_side_of_this_window_it_is_on() {
        let older = block("0.2.0", Some("0.1.0")).skew().expect("a sentence");
        assert!(older.contains("0.1.0"), "{older}");
        assert!(older.contains("older than this window"), "{older}");

        let newer = block("0.2.0", Some("0.3.0")).skew().expect("a sentence");
        assert!(newer.contains("newer than this window"), "{newer}");
    }

    /// A git build or a distribution's suffix orders by rules this does not
    /// know, so the pair is reported and neither is called the older.
    #[test]
    fn a_version_that_is_not_three_numbers_is_reported_rather_than_ordered() {
        let skew = block("0.2.0", Some("0.2.0-rc1"))
            .skew()
            .expect("a sentence");
        assert!(skew.contains("0.2.0-rc1"), "{skew}");
        assert!(!skew.contains("older"), "{skew}");
        assert!(!skew.contains("newer"), "{skew}");
        assert_eq!(triple("0.2.0"), Some((0, 2, 0)));
        assert_eq!(triple("0.2"), None);
        assert_eq!(triple("0.2.0.1"), None);
        assert_eq!(triple("0.2.0+deb"), None);
    }

    /// The whole block, as a reader of the bug form meets it. Written out
    /// rather than assembled, so that a change to any row's wording has to be
    /// made here too and cannot pass unnoticed.
    #[test]
    fn the_copied_block_answers_the_form_it_is_pasted_into() {
        assert_eq!(
            block("0.2.0", Some("0.2.0")).clipboard(),
            "Version: oxidom 0.2.0\n\
             Daemon: 0.2.0 (the system daemon)\n\
             Xray core: Xray 26.3.27\n\
             How it was installed: a distribution package (.deb, .rpm or AUR)\n\
             Distribution: Fedora Linux 42 (Workstation Edition)\n\
             Desktop: GNOME, wayland\n"
        );
    }

    /// Every question the block asks has a word for going unanswered. A blank
    /// row reads as a bug in the window rather than as a fact about the
    /// machine, and a reporter pasting one has told nobody anything.
    #[test]
    fn nothing_the_block_could_not_learn_is_left_blank() {
        let nothing = Versions {
            app: "0.2.0".to_string(),
            daemon: None,
            core: None,
            source: None,
            install: Install::Unknown,
            distribution: None,
            desktop: None,
        };
        for (label, value) in nothing.rows() {
            assert!(!value.trim().is_empty(), "{label} came out blank");
        }
        assert_eq!(
            nothing.clipboard(),
            "Version: oxidom 0.2.0\n\
             Daemon: too old to say (none has answered)\n\
             Xray core: none\n\
             How it was installed: unknown\n\
             Distribution: unknown\n\
             Desktop: unknown\n\
             \n\
             The daemon is older than this window — old enough that it cannot say which \
             version it is. Some controls will be missing until it is restarted.\n"
        );
    }

    #[test]
    fn each_daemon_a_window_can_reach_is_named_differently() {
        let kinds: Vec<&str> = [
            DaemonSource::System,
            DaemonSource::Session,
            DaemonSource::Spawned,
        ]
        .into_iter()
        .map(|source| {
            Versions {
                source: Some(source),
                ..block("0.2.0", Some("0.2.0"))
            }
            .daemon_kind()
        })
        .collect();
        assert_eq!(
            kinds,
            [
                "the system daemon",
                "the session daemon",
                "a session daemon this window started"
            ]
        );
    }
}
