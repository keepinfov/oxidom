use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

pub const PROFILE_SUBCOMMANDS: &[&str] = &["up", "down", "status", "ip", "run", "env", "tun"];

#[derive(Parser, Debug)]
#[command(
    name = "oxidom",
    version,
    about = "oxided freedom — a native Xray client",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Connect using a named profile.
    #[command(visible_alias = "connect-profile")]
    Up {
        /// Profile name.
        #[arg(default_value = "default")]
        profile: String,
    },
    /// Disconnect the active tunnel.
    #[command(visible_alias = "disconnect")]
    Down {
        /// Profile to stop. Omitted, the tunnel is stopped whoever owns it.
        profile: Option<String>,
    },
    /// Connect directly to one server, without a profile.
    Connect {
        /// Exact alias/id or a unique alias/name substring.
        handle: String,
    },
    /// Show the active connection.
    Status {
        /// Profile to inspect. Omitted, all sessions are listed.
        profile: Option<String>,
        /// Print the stable machine-readable schema.
        #[arg(long)]
        json: bool,
    },
    /// Print the active server endpoint or observed public egress address.
    Ip {
        /// Session whose server to inspect (defaults to `default`).
        profile: Option<String>,
        /// Observe the public address through the active SOCKS tunnel.
        #[arg(long)]
        egress: bool,
        /// Ignore the 60-second egress cache.
        #[arg(long, requires = "egress")]
        fresh: bool,
    },
    /// Print shell exports for one session's local proxies.
    Env {
        /// Session whose proxy variables to print (defaults to `default`).
        profile: Option<String>,
    },
    /// Inspect or remove one session's persistent TUN interface.
    Tun {
        /// Session whose interface to inspect (defaults to `default`).
        #[arg(default_value = "default")]
        profile: String,
        /// Stop interface routing and remove the device if oxidom created it.
        #[arg(long)]
        down: bool,
    },
    /// List daemon objects.
    List {
        /// Object type to list.
        #[arg(value_enum, default_value_t = ListTarget::Servers)]
        target: ListTarget,
        /// Print the stable machine-readable schema.
        #[arg(long)]
        json: bool,
    },
    /// Measure one server and print milliseconds.
    Ping {
        /// Exact alias/id or a unique alias/name substring.
        handle: String,
    },
    /// Assign a stable human handle to a server.
    Alias {
        /// Existing exact alias/id or a unique alias/name substring.
        handle: String,
        /// New globally unique alias.
        new: String,
    },
    /// Show a server's certificate and, with --trust, accept it from now on.
    ///
    /// Xray 26 removed `allowInsecure`, so a server with a self-signed
    /// certificate is unreachable until its certificate is pinned. This is how
    /// to look at one before deciding.
    Trust {
        /// Existing exact alias/id or a unique alias/name substring.
        handle: String,
        /// Pin the certificate shown, instead of only printing it.
        #[arg(long)]
        trust: bool,
    },
    /// Manage stored servers.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Manage named connection profiles.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Inspect the advanced Xray core settings a session would be built with.
    Core {
        #[command(subcommand)]
        command: CoreCommand,
    },
    /// Launch the graphical interface.
    Gui {
        /// Start without showing the window (for autostart; the tray/daemon
        /// keep working, activating the app again presents the window).
        #[arg(long)]
        background: bool,
        /// Stay in the foreground and log at debug level.
        #[arg(long)]
        debug: bool,
    },
    /// Run the headless daemon that owns the tunnel and serves D-Bus.
    Daemon {
        /// Serve on the system bus (systemd service) instead of the session bus.
        #[arg(long)]
        system: bool,
        /// Override the local SOCKS inbound port from config.toml.
        #[arg(long)]
        socks_port: Option<u16>,
        /// Override the local HTTP inbound port from config.toml.
        #[arg(long)]
        http_port: Option<u16>,
    },
    /// Run one process in the routing domain of a profile interface.
    Run {
        /// Profile whose routing domain should carry the command.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Split a command string without invoking a shell.
        #[arg(short = 'c', value_name = "COMMAND", conflicts_with = "args")]
        command: Option<String>,
        /// The command and arguments to run, e.g. `oxidom run -- curl https://ifconfig.me`.
        #[arg(trailing_var_arg = true, required_unless_present = "command")]
        args: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ListTarget {
    Servers,
    Profiles,
    Subscriptions,
    Sessions,
}

#[derive(Subcommand, Debug)]
pub enum ProfileCommand {
    /// List profiles.
    List {
        /// Print the stable machine-readable schema.
        #[arg(long)]
        json: bool,
    },
    /// Print one profile as TOML.
    Show { name: String },
    /// Create an empty profile using the current proxy ports.
    New { name: String },
    /// Edit one profile with $EDITOR, $VISUAL, or vi.
    Edit { name: String },
    /// Inspect or explicitly configure sequential failover for a saved profile.
    Failover(FailoverArgs),
    /// Remove one profile.
    Rm { name: String },
}

#[derive(clap::Args, Debug)]
pub struct FailoverArgs {
    pub name: String,
    /// Authorize switching within the fixed ordered list on the next saved Up.
    #[arg(long, conflicts_with = "disable")]
    pub enable: bool,
    /// Revoke future automatic starts, retaining the configured list and limits.
    #[arg(long)]
    pub disable: bool,
    /// Ordered exact canonical IDs, comma-separated; aliases and rules are refused.
    #[arg(long, value_delimiter = ',', value_name = "ID,ID")]
    pub members: Option<Vec<String>>,
    /// Maximum automatic starts per recovery episode (1..64; default 3).
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=64))]
    pub max_attempts: Option<u8>,
    /// Minimum seconds between automatic starts (1..3600; default 30).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=3600))]
    pub cooldown_secs: Option<u16>,
    /// Print saved authorization and this activation's progress as JSON.
    #[arg(long)]
    pub json: bool,
}

impl FailoverArgs {
    pub fn policy_payload(&self, loaded: &oxidom_core::profile::Profile) -> anyhow::Result<String> {
        let mut profile = loaded.clone();
        self.apply(&mut profile)?;
        Ok(serde_json::to_string(&profile.failover)?)
    }

    pub fn changes_policy(&self) -> bool {
        self.enable
            || self.disable
            || self.members.is_some()
            || self.max_attempts.is_some()
            || self.cooldown_secs.is_some()
    }

    pub fn apply(&self, profile: &mut oxidom_core::profile::Profile) -> anyhow::Result<()> {
        if !self.changes_policy() {
            return Ok(());
        }
        let mut policy = profile.failover.clone().unwrap_or_default();
        if self.enable || self.disable {
            policy.enabled = self.enable;
        }
        if let Some(members) = &self.members {
            policy.members = members.clone();
        }
        if let Some(max_attempts) = self.max_attempts {
            policy.max_attempts = usize::from(max_attempts);
        }
        if let Some(cooldown_secs) = self.cooldown_secs {
            policy.cooldown_secs = u64::from(cooldown_secs);
        }
        policy.validate()?;
        profile.failover = Some(policy);
        profile.validate(&self.name)
    }
}

#[derive(Subcommand, Debug)]
pub enum ServerCommand {
    /// Create a server by hand, field by field.
    ///
    /// Without --file, opens $EDITOR, $VISUAL, or vi on a commented TOML
    /// template for the chosen protocol; every field names the JSON key it
    /// fills. The daemon validates the draft through the same path that
    /// generates an Xray outbound, and creates nothing when validation fails.
    Add {
        /// Protocol the template is written for.
        #[arg(long, value_enum, default_value_t = DraftProtocol::Vless)]
        protocol: DraftProtocol,
        /// Read the draft as TOML from this file instead ('-' for stdin).
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

/// The protocols a draft can describe, as CLI words. Mirrors
/// `oxidom_core::model::Protocol`; the mapping is one match away because a
/// clap `ValueEnum` cannot be derived on a foreign type.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum DraftProtocol {
    Vless,
    Vmess,
    Trojan,
    Shadowsocks,
    Socks,
    Http,
    Hysteria2,
}

impl DraftProtocol {
    pub fn to_model(self) -> oxidom_core::model::Protocol {
        use oxidom_core::model::Protocol;
        match self {
            DraftProtocol::Vless => Protocol::Vless,
            DraftProtocol::Vmess => Protocol::Vmess,
            DraftProtocol::Trojan => Protocol::Trojan,
            DraftProtocol::Shadowsocks => Protocol::Shadowsocks,
            DraftProtocol::Socks => Protocol::Socks,
            DraftProtocol::Http => Protocol::Http,
            DraftProtocol::Hysteria2 => Protocol::Hysteria2,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum CoreCommand {
    /// Read one oldest-first page of persistent daemon diagnostics as JSON.
    Diagnostics {
        /// Opaque next_cursor from an earlier page.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum records (0 means 64; capped at 256).
        #[arg(long, default_value_t = 0)]
        limit: u32,
        /// Read this journal directly, without contacting or starting a daemon.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Export one diagnostic page as a local, reviewable problem report.
    Report {
        /// Opaque next_cursor from an earlier page.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum records (0 means 64; capped at 256).
        #[arg(long, default_value_t = 0)]
        limit: u32,
        /// Read this journal directly, without contacting or starting a daemon.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Atomically save a private report instead of printing to stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Print each resolved setting and the level it came from.
    Show {
        /// Profile whose `[core]` is folded over the machine-wide settings.
        #[arg(default_value = "default")]
        profile: String,
        /// Print the stable machine-readable schema.
        #[arg(long)]
        json: bool,
    },
}

/// Accept both `oxidom up work` and `oxidom work up`.
///
/// Verb-first is canonical: it is the form documented in AGENTS.md and used
/// by `oxidom@.service`. Profile-first is a shell-history-friendly synonym.
pub fn normalize(mut args: Vec<OsString>) -> Vec<OsString> {
    if args.len() < 3 {
        return args;
    }
    if args[1].as_os_str().as_bytes().starts_with(b"-") {
        return args;
    }

    let command = Cli::command();
    let known = command.get_subcommands().any(|subcommand| {
        args[1] == subcommand.get_name()
            || subcommand.get_all_aliases().any(|alias| args[1] == alias)
    });
    if known {
        return args;
    }

    if let Some(verb) = args[2].to_str()
        && PROFILE_SUBCOMMANDS.contains(&verb)
    {
        if verb == "run" {
            let profile = args.remove(1);
            args.insert(2, OsString::from("--profile"));
            args.insert(3, profile);
        } else {
            args.swap(1, 2);
        }
    }
    args
}

fn gui_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("OXIDOM_GUI_BIN").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(sibling) = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|parent| parent.join("oxidom-gui")))
        .filter(|path| path.is_file())
    {
        return sibling;
    }
    PathBuf::from("oxidom-gui")
}

pub fn run_gui(background: bool, debug: bool) -> Result<()> {
    let executable = gui_binary();
    let mut command = std::process::Command::new(&executable);
    if background {
        command.arg("--background");
    }
    if debug {
        command.arg("--debug");
    }
    let error = command.exec();
    Err(error).with_context(|| {
        format!(
            "launching the graphical interface with {} (set OXIDOM_GUI_BIN to override it)",
            executable.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn failover_args(args: &[&str]) -> FailoverArgs {
        let cli = parse(args).unwrap();
        let Command::Profile {
            command: ProfileCommand::Failover(args),
        } = cli.command
        else {
            panic!("failover command expected")
        };
        args
    }

    #[test]
    fn failover_requires_explicit_enable_and_preserves_exact_member_order() {
        let mut profile = oxidom_core::profile::Profile::default();
        profile.select.server = "primary-alias".into();
        let args = failover_args(&[
            "oxidom",
            "profile",
            "failover",
            "work",
            "--members",
            "canonical-b,canonical-a",
            "--max-attempts",
            "2",
            "--cooldown-secs",
            "45",
        ]);
        args.apply(&mut profile).unwrap();
        let policy = profile.failover.as_ref().unwrap();
        assert!(!policy.enabled);
        assert_eq!(policy.members, ["canonical-b", "canonical-a"]);
        assert_eq!((policy.max_attempts, policy.cooldown_secs), (2, 45));
        let enable = failover_args(&["oxidom", "profile", "failover", "work", "--enable"]);
        enable.apply(&mut profile).unwrap();
        assert!(profile.failover.as_ref().unwrap().enabled);
        let disable = failover_args(&["oxidom", "profile", "failover", "work", "--disable"]);
        disable.apply(&mut profile).unwrap();
        let policy = profile.failover.unwrap();
        assert!(!policy.enabled);
        assert_eq!(policy.members, ["canonical-b", "canonical-a"]);
        assert_eq!((policy.max_attempts, policy.cooldown_secs), (2, 45));
    }

    #[test]
    fn inspecting_failover_does_not_create_or_enable_a_policy() {
        let args = failover_args(&["oxidom", "profile", "failover", "work", "--json"]);
        let mut profile = oxidom_core::profile::Profile::default();
        args.apply(&mut profile).unwrap();
        assert!(!args.changes_policy());
        assert!(profile.failover.is_none());
    }

    #[test]
    fn failover_controls_refuse_ambiguous_switches_and_unbounded_limits() {
        for flags in [
            vec!["--enable", "--disable"],
            vec!["--max-attempts", "0"],
            vec!["--max-attempts", "65"],
            vec!["--cooldown-secs", "0"],
            vec!["--cooldown-secs", "3601"],
        ] {
            let mut args = vec!["oxidom", "profile", "failover", "work"];
            args.extend(flags);
            assert!(parse(&args).is_err());
        }
        for members in ["a", "a,a", "a,"] {
            let args = failover_args(&[
                "oxidom",
                "profile",
                "failover",
                "work",
                "--enable",
                "--members",
                members,
            ]);
            let mut profile = oxidom_core::profile::Profile::default();
            profile.select.server = "a".into();
            assert!(args.apply(&mut profile).is_err());
        }
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(normalize(args.iter().map(OsString::from).collect()))
    }

    #[test]
    fn diagnostic_commands_keep_file_cursor_limit_and_output_explicit() {
        let parsed = parse(&[
            "oxidom",
            "core",
            "diagnostics",
            "--file",
            "journal",
            "--cursor",
            "position",
            "--limit",
            "1",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Command::Core {
            command: CoreCommand::Diagnostics { cursor: Some(cursor), limit: 1, file: Some(file) }
        } if cursor == "position" && file == std::path::Path::new("journal")));
        assert!(matches!(
            parse(&["oxidom", "core", "report", "--output", "report.txt"])
                .unwrap()
                .command,
            Command::Core {
                command: CoreCommand::Report {
                    cursor: None,
                    limit: 0,
                    file: None,
                    output: Some(_)
                }
            }
        ));
        assert!(parse(&["oxidom", "core", "diagnostics", "--output", "report.txt"]).is_err());
        assert!(parse(&["oxidom", "core", "report", "--limit", "-1"]).is_err());
    }

    #[test]
    fn verb_first_and_profile_first_parse_identically() {
        for (verb_first, profile_first) in [
            (&["oxidom", "up", "work"][..], &["oxidom", "work", "up"][..]),
            (
                &["oxidom", "down", "work"][..],
                &["oxidom", "work", "down"][..],
            ),
            (
                &["oxidom", "status", "work", "--json"][..],
                &["oxidom", "work", "status", "--json"][..],
            ),
            (
                &["oxidom", "env", "work"][..],
                &["oxidom", "work", "env"][..],
            ),
            (
                &["oxidom", "tun", "work", "--down"][..],
                &["oxidom", "work", "tun", "--down"][..],
            ),
        ] {
            assert_eq!(
                format!("{:?}", parse(verb_first).unwrap().command),
                format!("{:?}", parse(profile_first).unwrap().command)
            );
        }
    }

    #[test]
    fn an_existing_verb_or_flag_is_never_swapped() {
        let list = normalize(
            ["oxidom", "list", "servers"]
                .into_iter()
                .map(OsString::from)
                .collect(),
        );
        assert_eq!(list, ["oxidom", "list", "servers"]);

        let version = normalize(
            ["oxidom", "--version", "work"]
                .into_iter()
                .map(OsString::from)
                .collect(),
        );
        assert_eq!(version, ["oxidom", "--version", "work"]);
    }

    #[test]
    fn profile_first_run_preserves_the_separator_and_tail() {
        let cli = parse(&["oxidom", "work", "run", "--", "curl", "x"]).unwrap();
        let Command::Run {
            profile,
            command,
            args,
        } = cli.command
        else {
            panic!("run did not parse");
        };
        assert_eq!(profile, "work");
        assert_eq!(command, None);
        assert_eq!(args, ["curl", "x"]);
    }

    #[test]
    fn profile_first_run_command_string_is_not_a_shell_subcommand() {
        let cli = parse(&["oxidom", "work", "run", "-c", "printf '%s' one"]).unwrap();
        let Command::Run {
            profile,
            command,
            args,
        } = cli.command
        else {
            panic!("run did not parse");
        };
        assert_eq!(profile, "work");
        assert_eq!(command.as_deref(), Some("printf '%s' one"));
        assert!(args.is_empty());
    }

    #[test]
    fn an_unknown_pair_passes_through_to_clap() {
        let input: Vec<OsString> = ["oxidom", "foo", "bar"]
            .into_iter()
            .map(OsString::from)
            .collect();
        assert_eq!(normalize(input.clone()), input);
        assert!(Cli::try_parse_from(input).is_err());
    }

    #[test]
    fn reserved_profile_names_track_clap_commands_and_aliases() {
        let mut clap_names = BTreeSet::new();
        let command = Cli::command();
        for subcommand in command.get_subcommands() {
            clap_names.insert(subcommand.get_name());
            clap_names.extend(subcommand.get_all_aliases());
        }
        // `tun` is reserved ahead of phase 4b so adding the command cannot
        // make an existing profile ambiguous overnight.
        clap_names.insert("tun");

        let reserved: BTreeSet<&str> = oxidom_core::profile::RESERVED_NAMES
            .iter()
            .copied()
            .collect();
        assert_eq!(reserved, clap_names);
    }
}
