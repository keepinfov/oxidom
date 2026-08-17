mod cli;
mod daemon;

use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, ToSocketAddrs};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use oxidom_core::cli_json::{
    CoreOutput, ProfileOutput, ServerOutput, SessionOutput, StatusOutput, SubscriptionOutput,
};
use oxidom_core::client::DaemonClient;
use oxidom_core::handle::{self, HandleMatch};
use oxidom_core::ipc::{
    LatencyReading, PROBE_STATE_VERSION, ProbeFailure, ProbeRoute, ProbeState, ProfileEntry,
    SessionInfo,
};
use oxidom_core::model::{Server, Subscription};
use oxidom_core::profile::{Profile, ProfileProxy};
use serde::Serialize;

use crate::cli::{Cli, Command, CoreCommand, ListTarget, ProfileCommand};

/// Command completed successfully.
const EXIT_SUCCESS: u8 = 0;
/// The command failed.
const EXIT_ERROR: u8 = 1;
/// The requested connection-dependent value is unavailable because no tunnel is connected.
const EXIT_NOT_CONNECTED: u8 = 3;
/// No existing daemon could be reached (or, for a writer, started).
const EXIT_DAEMON_UNAVAILABLE: u8 = 4;

const PROBE_DEADLINE: Duration = Duration::from_secs(30);
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureKind {
    Error,
    NotConnected,
    DaemonUnavailable,
    Child(u8),
}

struct Failure {
    kind: FailureKind,
    message: Option<String>,
}

type CliResult<T = ()> = Result<T, Failure>;

impl Failure {
    fn error(error: impl std::fmt::Display) -> Self {
        Failure {
            kind: FailureKind::Error,
            message: Some(format!("{error:#}")),
        }
    }

    fn message(message: impl Into<String>) -> Self {
        Failure {
            kind: FailureKind::Error,
            message: Some(message.into()),
        }
    }

    fn not_connected() -> Self {
        Failure {
            kind: FailureKind::NotConnected,
            message: None,
        }
    }

    fn daemon_unavailable(message: Option<String>) -> Self {
        Failure {
            kind: FailureKind::DaemonUnavailable,
            message,
        }
    }
}

fn exit_status(result: CliResult) -> ExitCode {
    match result {
        Ok(()) => ExitCode::from(EXIT_SUCCESS),
        Err(failure) => {
            if let Some(message) = failure.message {
                eprintln!("{message}");
            }
            ExitCode::from(exit_code_for(failure.kind))
        }
    }
}

fn exit_code_for(kind: FailureKind) -> u8 {
    match kind {
        FailureKind::Error => EXIT_ERROR,
        FailureKind::NotConnected => EXIT_NOT_CONNECTED,
        FailureKind::DaemonUnavailable => EXIT_DAEMON_UNAVAILABLE,
        FailureKind::Child(code) => code,
    }
}

fn main() -> ExitCode {
    if let Some(expected) = std::env::var_os(oxidom_core::run::SCOPED_CGROUP_ENV) {
        let expected = expected.to_string_lossy();
        let argv = std::env::args_os().skip(1).collect::<Vec<_>>();
        return match oxidom_core::run::exec_scoped(&expected, &argv) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error:#}");
                ExitCode::from(EXIT_ERROR)
            }
        };
    }
    let cli = Cli::parse_from(cli::normalize(std::env::args_os().collect()));
    // The daemon's log is its journal, and `journalctl -u oxidom` is the only
    // window into it. For every other subcommand the same lines are noise in
    // front of whatever the user actually asked for — "no daemon on the system
    // bus" is a step, not a problem. $RUST_LOG still overrides either way.
    let default_level = if matches!(cli.command, Command::Daemon { .. }) {
        "info"
    } else {
        "warn"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .init();
    exit_status(dispatch(cli))
}

fn dispatch(cli: Cli) -> CliResult {
    match cli.command {
        Command::Up { profile } => up(&profile),
        Command::Down { profile } => down(profile.as_deref()),
        Command::Connect { handle } => connect(&handle),
        Command::Status { profile, json } => status(profile.as_deref(), json),
        Command::Ip {
            profile,
            egress,
            fresh,
        } => ip(profile.as_deref(), egress, fresh),
        Command::Env { profile } => env(profile.as_deref()),
        Command::Tun { profile, down } => tun(&profile, down),
        Command::List { target, json } => list(target, json),
        Command::Ping { handle } => ping(&handle),
        Command::Alias { handle, new } => set_alias(&handle, &new),
        Command::Profile { command } => profile_command(command),
        Command::Core { command } => core_command(command),
        Command::Gui { background, debug } => {
            cli::run_gui(background, debug).map_err(Failure::error)
        }
        Command::Daemon {
            system,
            socks_port,
            http_port,
        } => daemon::run(daemon::DaemonOptions {
            system_bus: system,
            socks_port,
            http_port,
        })
        .map_err(Failure::error),
        Command::Run {
            profile,
            command,
            args,
        } => run(&profile, &args, command.as_deref()),
    }
}

fn existing_client() -> CliResult<DaemonClient> {
    DaemonClient::connect_existing().map_err(|_| Failure::daemon_unavailable(None))
}

fn spawning_client() -> CliResult<DaemonClient> {
    DaemonClient::connect_any(|_| {}).map_err(|error| {
        Failure::daemon_unavailable(Some(format!(
            "could not reach the oxidom daemon: {error:#}"
        )))
    })
}

fn up(profile: &str) -> CliResult {
    let client = spawning_client()?;
    let result = client.up_profile(profile).map_err(Failure::error)?;
    for ignored in result.ignored_ports {
        let normalized = ignored.to_ascii_lowercase();
        let name = normalized.strip_suffix(" port").unwrap_or(&normalized);
        eprintln!("{name} port is pinned by the unit, profile value ignored");
    }
    for warning in result.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

fn down(profile: Option<&str>) -> CliResult {
    let client = existing_client()?;
    let stopped = match profile {
        Some(profile) => client.down_profile(profile),
        None => client.down(""),
    }
    .map_err(Failure::error)?;
    if !stopped {
        // Stopping is idempotent on purpose. `oxidom@work`'s ExecStop runs
        // whenever that unit stops, including long after `home` took the
        // tunnel over; failing here would leave the unit in a failed state for
        // having correctly done nothing. The post-condition asked for — this
        // profile is not running — already holds.
        eprintln!(
            "profile {:?} is not up; nothing to stop",
            profile.unwrap_or_default()
        );
    }
    Ok(())
}

fn connect(handle: &str) -> CliResult {
    let client = spawning_client()?;
    let subscriptions = client.subscriptions().map_err(Failure::error)?;
    let server = resolve_server(&subscriptions, handle)?;
    client.connect_server(&server.id).map_err(Failure::error)
}

fn status(profile: Option<&str>, json: bool) -> CliResult {
    let client = existing_client()?;
    let Some(profile) = profile else {
        return print_sessions(&client, json);
    };
    let session = connected_session(&client, profile)?;
    let probes = client.probe_state().map_err(Failure::error)?;
    if session.selection.kind == "pool" {
        let latency_ms = current_pool_latency(&probes, &session.profile);
        let output = StatusOutput::new(&session, None, latency_ms);
        if json {
            return print_json(&output);
        }
        return print_text(pool_status(&session, latency_ms));
    }

    let server = servers_for_session(&client, &session)?
        .into_iter()
        .next()
        .ok_or_else(|| Failure::message("the active server is no longer in the daemon store"))?;
    let latency_ms = current_latency(&probes, &session.profile, &server.id);
    let output = StatusOutput::new(&session, Some(&server), latency_ms);

    if json {
        print_json(&output)
    } else {
        let handle = server.alias.as_deref().unwrap_or(server.id.as_str());
        let latency = latency_ms
            .map(|value| format!("{value} ms"))
            .unwrap_or_else(|| "—".to_string());
        print_line(format!(
            "{}  {}  {}  socks {}:{}  {}",
            output.state, handle, server.name, output.address, output.socks_port, latency
        ))
    }
}

fn ip(profile: Option<&str>, egress: bool, fresh: bool) -> CliResult {
    let client = existing_client()?;
    let session = connected_listed_session(&client, profile.unwrap_or("default"))?;
    let servers = servers_for_session(&client, &session)?;
    if egress {
        let bind = session
            .address
            .parse()
            .map_err(|error| Failure::message(format!("invalid session address: {error}")))?;
        let address = if session.selection.kind == "pool" {
            oxidom_core::egress::uncached_address(bind, session.socks_port)
        } else {
            let server = servers.first().ok_or_else(|| {
                Failure::message("the active server is no longer in the daemon store")
            })?;
            oxidom_core::egress::address(
                &session.profile,
                &server.id,
                bind,
                session.socks_port,
                fresh,
            )
        }
        .map_err(Failure::error)?;
        return print_line(address);
    }

    let addresses = servers
        .iter()
        .map(endpoint_ip)
        .collect::<CliResult<Vec<_>>>()?;
    let body = addresses
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    print_line(body)
}

fn env(profile: Option<&str>) -> CliResult {
    let client = existing_client()?;
    let session = connected_listed_session(&client, profile.unwrap_or("default"))?;
    print_text(session_environment(&session))
}

fn tun(profile: &str, down: bool) -> CliResult {
    let client = existing_client()?;
    if down {
        let removed = client.delete_interface(profile).map_err(Failure::error)?;
        if !removed {
            eprintln!("profile {profile:?} has no interface; nothing to remove");
        }
        return Ok(());
    }
    let session = client.session_status(profile).map_err(Failure::error)?;
    let interface = session.interface.ok_or_else(|| {
        Failure::message(format!(
            "profile {profile:?} has no interface; set [interface] enable = true and bring it up"
        ))
    })?;
    print_line(format!(
        "{}\t{}/32\tmtu {}\troutes {}\ttable {}\tmark {:#x}\t{}",
        interface.device,
        interface.address,
        interface.mtu,
        interface.routes,
        interface.table,
        interface.mark,
        if interface.up { "up" } else { "down" }
    ))
}

fn run(profile: &str, args: &[String], command: Option<&str>) -> CliResult {
    let argv = oxidom_core::run::command_argv(args, command).map_err(Failure::error)?;
    let client = existing_client()?;
    let configured = client.profile(profile).map_err(Failure::error)?;
    let session = client.session_status(profile).map_err(Failure::error)?;
    oxidom_core::run::validate_interface(
        profile,
        session.interface.as_ref(),
        configured.interface.enable,
    )
    .map_err(Failure::error)?;
    let uid = nix::unistd::Uid::effective().as_raw();
    let status = oxidom_core::run::run_in_scope(profile, uid, &argv).map_err(Failure::error)?;
    if status.success() {
        return Ok(());
    }
    let code = status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(i32::from(EXIT_ERROR))
        .clamp(1, 255) as u8;
    Err(Failure {
        kind: FailureKind::Child(code),
        message: None,
    })
}

fn session_environment(session: &SessionInfo) -> String {
    format!(
        "export ALL_PROXY=socks5h://{}:{}\n\
         export all_proxy=socks5h://{}:{}\n\
         export HTTP_PROXY=http://{}:{}\n\
         export http_proxy=http://{}:{}\n\
         export HTTPS_PROXY=http://{}:{}\n\
         export https_proxy=http://{}:{}\n\
         export NO_PROXY=localhost,127.0.0.0/8,::1\n\
         export no_proxy=localhost,127.0.0.0/8,::1\n",
        session.address,
        session.socks_port,
        session.address,
        session.socks_port,
        session.address,
        session.http_port,
        session.address,
        session.http_port,
        session.address,
        session.http_port,
        session.address,
        session.http_port,
    )
}

fn list(target: ListTarget, json: bool) -> CliResult {
    let client = existing_client()?;
    match target {
        ListTarget::Servers => list_servers(&client, json),
        ListTarget::Profiles => list_profiles(&client, json),
        ListTarget::Subscriptions => list_subscriptions(&client, json),
        ListTarget::Sessions => print_sessions(&client, json),
    }
}

fn print_sessions(client: &DaemonClient, json: bool) -> CliResult {
    let sessions = client.list_sessions().map_err(Failure::error)?;
    let probes = client.probe_state().map_err(Failure::error)?;
    let outputs = sessions
        .iter()
        .map(|session| {
            let latency = (session.state == "connected")
                .then(|| {
                    if session.selection.kind == "pool" {
                        current_pool_latency(&probes, &session.profile)
                    } else {
                        session.server_id.as_deref().and_then(|server_id| {
                            current_latency(&probes, &session.profile, server_id)
                        })
                    }
                })
                .flatten();
            SessionOutput::new(session, latency)
        })
        .collect::<Vec<_>>();
    if json {
        print_json(&outputs)
    } else {
        print_text(session_table(&outputs))
    }
}

fn session_table(sessions: &[SessionOutput]) -> String {
    let show_device = sessions.iter().any(|session| session.interface.is_some());
    let mut rows = Vec::with_capacity(sessions.len() + 1);
    let mut header = vec![
        "PROFILE".to_string(),
        "STATE".to_string(),
        "SERVER".to_string(),
        "ADDRESS".to_string(),
    ];
    if show_device {
        header.push("DEVICE".to_string());
    }
    header.push("LATENCY".to_string());
    rows.push(header);
    rows.extend(sessions.iter().map(|session| {
        let mut row = vec![
            session.profile.clone(),
            session.state.clone(),
            session
                .server_alias
                .clone()
                .or_else(|| session.server_id.clone())
                .or_else(|| {
                    session
                        .selection
                        .as_ref()
                        .map(|selection| format!("pool({})", selection.members.len()))
                })
                .unwrap_or_else(|| "—".to_string()),
            format!("{}:{}", session.address, session.socks_port),
        ];
        if show_device {
            row.push(
                session
                    .interface
                    .as_ref()
                    .map(|interface| interface.device.clone())
                    .unwrap_or_else(|| "—".to_string()),
            );
        }
        row.push(
            session
                .latency_ms
                .map(|latency| format!("{latency}ms"))
                .unwrap_or_else(|| "—".to_string()),
        );
        row
    }));
    let mut widths = vec![0usize; rows[0].len()];
    for row in &rows {
        for (column, value) in row.iter().enumerate() {
            widths[column] = widths[column].max(value.chars().count());
        }
    }
    let mut table = String::new();
    for row in rows {
        for (column, value) in row.into_iter().enumerate() {
            table.push_str(&value);
            if column < widths.len() - 1 {
                let padding = widths[column].saturating_sub(value.chars().count()) + 2;
                table.extend(std::iter::repeat_n(' ', padding));
            }
        }
        table.push('\n');
    }
    table
}

fn list_servers(client: &DaemonClient, json: bool) -> CliResult {
    let subscriptions = client.subscriptions().map_err(Failure::error)?;
    let servers = ServerOutput::all(&subscriptions);
    if json {
        return print_json(&servers);
    }
    for server in servers {
        let handle = server.alias.as_deref().unwrap_or(server.id.as_str());
        print_line(format!(
            "{handle}\t{}\t{}\t{}:{}\t{}",
            server.name, server.protocol, server.address, server.port, server.subscription
        ))?;
    }
    Ok(())
}

fn list_profiles(client: &DaemonClient, json: bool) -> CliResult {
    let profiles = client.list_profiles().map_err(Failure::error)?;
    if json {
        return print_json(&ProfileOutput::all(&profiles));
    }
    print_profiles(&profiles)
}

fn print_profiles(profiles: &[ProfileEntry]) -> CliResult {
    for profile in profiles {
        print_line(format!(
            "{}\t{}\tsocks {}\thttp {}\t{}",
            profile.name,
            profile.server,
            profile.socks_port,
            profile.http_port,
            profile.description
        ))?;
    }
    Ok(())
}

fn list_subscriptions(client: &DaemonClient, json: bool) -> CliResult {
    let subscriptions = client.subscriptions().map_err(Failure::error)?;
    if json {
        return print_json(&SubscriptionOutput::all(&subscriptions));
    }
    for subscription in subscriptions {
        print_line(format!(
            "{}\t{}\t{} servers",
            subscription.id,
            subscription.name,
            subscription.servers.len()
        ))?;
    }
    Ok(())
}

fn ping(handle: &str) -> CliResult {
    let client = existing_client()?;
    let subscriptions = client.subscriptions().map_err(Failure::error)?;
    let server = resolve_server(&subscriptions, handle)?;
    client.request_probe(&server.id).map_err(Failure::error)?;

    let deadline = Instant::now() + PROBE_DEADLINE;
    loop {
        let probes = client.probe_state().map_err(Failure::error)?;
        if probes.version < PROBE_STATE_VERSION {
            return Err(Failure::message(format!(
                "daemon probe schema {} is older than required version {PROBE_STATE_VERSION}",
                probes.version
            )));
        }
        let pending = probes.running.iter().any(|id| id == &server.id)
            || probes.queued.iter().any(|id| id == &server.id);
        if !pending {
            let sessions = client.list_sessions().map_err(Failure::error)?;
            return print_probe_result(&probes, &sessions, server);
        }
        if Instant::now() >= deadline {
            return Err(Failure::message(format!(
                "probe for {} did not finish within {} seconds",
                display_handle(server),
                PROBE_DEADLINE.as_secs()
            )));
        }
        std::thread::sleep(PROBE_POLL_INTERVAL);
    }
}

fn print_probe_result(probes: &ProbeState, sessions: &[SessionInfo], server: &Server) -> CliResult {
    let reading = probe_reading_for_server(probes, sessions, &server.id).ok_or_else(|| {
        Failure::message(format!(
            "daemon finished the probe for {} without a reading",
            display_handle(server)
        ))
    })?;
    if reading.failure.is_none()
        && let Some(value) = reading.value
    {
        return print_line(value);
    }
    // The detail, when the daemon sent one, says which local condition it was
    // — "the server's certificate was rejected" rather than the shrug that
    // sends people to replace a server that is fine.
    let reason = match (reading.failure, reading.detail) {
        (Some(ProbeFailure::Unknown), Some(detail)) => detail.message(),
        (Some(ProbeFailure::Unreachable), _) => "server is unreachable",
        (Some(ProbeFailure::Timeout), _) => "probe timed out",
        (Some(ProbeFailure::NoNetwork), _) => "no network connection",
        (Some(ProbeFailure::Unknown), None) => "probe could not run on this machine",
        (None, _) => "daemon returned an invalid probe reading",
    };
    Err(Failure::message(format!(
        "{}: {reason}",
        display_handle(server)
    )))
}

fn probe_reading_for_server<'a>(
    probes: &'a ProbeState,
    sessions: &[SessionInfo],
    server_id: &str,
) -> Option<&'a LatencyReading> {
    sessions
        .iter()
        .find(|session| {
            session.state == "connected" && session.server_id.as_deref() == Some(server_id)
        })
        .and_then(|session| probes.proxied.get(&session.profile))
        .or_else(|| probes.readings.get(server_id))
}

fn set_alias(handle: &str, new: &str) -> CliResult {
    let client = existing_client()?;
    let subscriptions = client.subscriptions().map_err(Failure::error)?;
    let server = resolve_server(&subscriptions, handle)?;
    client
        .set_server_alias(&server.id, new)
        .map_err(Failure::error)
}

fn profile_command(command: ProfileCommand) -> CliResult {
    let client = existing_client()?;
    match command {
        ProfileCommand::List { json } => list_profiles(&client, json),
        ProfileCommand::Show { name } => {
            let profile = client.profile(&name).map_err(Failure::error)?;
            print_text(profile.to_toml().map_err(Failure::error)?)
        }
        ProfileCommand::New { name } => new_profile(&client, &name),
        ProfileCommand::Edit { name } => edit_profile(&client, &name),
        ProfileCommand::Rm { name } => {
            if client.remove_profile(&name).map_err(Failure::error)? {
                Ok(())
            } else {
                Err(Failure::message(format!("profile {name:?} does not exist")))
            }
        }
    }
}

fn core_command(command: CoreCommand) -> CliResult {
    let client = existing_client()?;
    match command {
        CoreCommand::Show { profile, json } => {
            let global = client.settings().map_err(Failure::error)?;
            let overrides = client.profile(&profile).map_err(Failure::error)?;
            let output = CoreOutput::new(&profile, &global.core, &overrides.core);
            if json {
                return print_json(&output);
            }
            for setting in &output.settings {
                print_line(format!(
                    "{}\t{}\t{}",
                    setting.setting, setting.value, setting.origin
                ))?;
            }
            Ok(())
        }
    }
}

fn new_profile(client: &DaemonClient, name: &str) -> CliResult {
    if client
        .list_profiles()
        .map_err(Failure::error)?
        .iter()
        .any(|profile| profile.name == name)
    {
        return Err(Failure::message(format!("profile {name:?} already exists")));
    }
    let config = client.settings().map_err(Failure::error)?;
    let profile = Profile {
        proxy: ProfileProxy {
            socks_port: config.socks_port,
            http_port: config.http_port,
        },
        ..Profile::default()
    };
    client.save_profile(name, &profile).map_err(Failure::error)
}

fn edit_profile(client: &DaemonClient, name: &str) -> CliResult {
    let profile = client.profile(name).map_err(Failure::error)?;
    let original = profile.to_toml().map_err(Failure::error)?;
    let temporary = TemporaryProfile::create(original.as_bytes()).map_err(Failure::error)?;
    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|editor| !editor.is_empty())
        .or_else(|| {
            std::env::var("VISUAL")
                .ok()
                .filter(|editor| !editor.is_empty())
        })
        .unwrap_or_else(|| "vi".to_string());
    let arguments = oxidom_core::profile::editor_command(&editor).map_err(Failure::error)?;
    let status = std::process::Command::new(&arguments[0])
        .args(&arguments[1..])
        .arg(temporary.path())
        .status()
        .map_err(Failure::error)?;
    if !status.success() {
        return Err(Failure::message(format!(
            "editor exited with status {status}"
        )));
    }
    let edited = std::fs::read_to_string(temporary.path()).map_err(Failure::error)?;
    if edited == original {
        return Ok(());
    }
    let profile = Profile::from_toml(&edited).map_err(Failure::error)?;
    profile.validate(name).map_err(Failure::error)?;
    client.save_profile(name, &profile).map_err(Failure::error)
}

struct TemporaryProfile {
    path: PathBuf,
}

impl TemporaryProfile {
    fn create(body: &[u8]) -> std::io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        for attempt in 0..100_u8 {
            let path = std::env::temp_dir().join(format!(
                "oxidom-profile-{}-{nonce}-{attempt}.toml",
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut file) => {
                    let temporary = TemporaryProfile { path };
                    file.write_all(body)?;
                    return Ok(temporary);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique temporary profile",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryProfile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::debug!(
                "could not remove temporary profile {}: {error}",
                self.path.display()
            );
        }
    }
}

fn connected_session(client: &DaemonClient, profile: &str) -> CliResult<SessionInfo> {
    let session = client.session_status(profile).map_err(Failure::error)?;
    require_connected(session)
}

fn connected_listed_session(client: &DaemonClient, profile: &str) -> CliResult<SessionInfo> {
    let session = client
        .list_sessions()
        .map_err(Failure::error)?
        .into_iter()
        .find(|session| session.profile == profile)
        .ok_or_else(Failure::not_connected)?;
    require_connected(session)
}

fn require_connected(session: SessionInfo) -> CliResult<SessionInfo> {
    if session.state != "connected" {
        return Err(Failure::not_connected());
    }
    if session.selection.kind != "pool" && session.server_id.is_none() {
        return Err(Failure::message(
            "daemon reports connected without an active selection",
        ));
    }
    Ok(session)
}

fn servers_for_session(client: &DaemonClient, session: &SessionInfo) -> CliResult<Vec<Server>> {
    let subscriptions = client.subscriptions().map_err(Failure::error)?;
    let all = subscriptions
        .iter()
        .flat_map(|subscription| subscription.servers.iter())
        .collect::<Vec<_>>();
    let ids = if session.selection.kind == "pool" {
        session
            .selection
            .members
            .iter()
            .map(|member| member.server_id.as_str())
            .collect::<Vec<_>>()
    } else {
        session.server_id.iter().map(String::as_str).collect()
    };
    ids.into_iter()
        .map(|server_id| {
            all.iter()
                .find(|server| server.id == server_id)
                .map(|server| (*server).clone())
                .ok_or_else(|| {
                    Failure::message(format!(
                        "session member {server_id:?} is no longer in the daemon store"
                    ))
                })
        })
        .collect()
}

fn resolve_server<'a>(subscriptions: &'a [Subscription], needle: &str) -> CliResult<&'a Server> {
    match handle::resolve(
        subscriptions
            .iter()
            .flat_map(|subscription| subscription.servers.iter()),
        needle,
    ) {
        HandleMatch::One(server) => Ok(server),
        HandleMatch::None => Err(Failure::message(format!(
            "no server matches handle {needle:?}"
        ))),
        HandleMatch::Ambiguous(candidates) => {
            let list = candidates
                .iter()
                .map(|server| format!("  {}\t{}", display_handle(server), server.name))
                .collect::<Vec<_>>()
                .join("\n");
            Err(Failure::message(format!(
                "handle {needle:?} is ambiguous:\n{list}"
            )))
        }
    }
}

fn display_handle(server: &Server) -> &str {
    server.alias.as_deref().unwrap_or(server.id.as_str())
}

fn current_latency(probes: &ProbeState, profile: &str, server_id: &str) -> Option<u32> {
    if probes.version < PROBE_STATE_VERSION
        || probes.running.iter().any(|id| id == server_id)
        || probes.queued.iter().any(|id| id == server_id)
    {
        return None;
    }
    probes
        .proxied
        .get(profile)
        // Compatibility with an A2 daemon, which stored the active tunnel
        // reading under the server id before the additive `proxied` field.
        .or_else(|| {
            probes
                .readings
                .get(server_id)
                .filter(|reading| reading.route == ProbeRoute::Proxied)
        })
        .and_then(|reading| {
            (reading.route == ProbeRoute::Proxied && reading.failure.is_none())
                .then_some(reading.value)
                .flatten()
        })
}

fn current_pool_latency(probes: &ProbeState, profile: &str) -> Option<u32> {
    let label = format!("pool:{profile}");
    if probes.version < PROBE_STATE_VERSION
        || probes.running.iter().any(|id| id == &label)
        || probes.queued.iter().any(|id| id == &label)
    {
        return None;
    }
    probes.proxied.get(profile).and_then(|reading| {
        (reading.route == ProbeRoute::Proxied && reading.failure.is_none())
            .then_some(reading.value)
            .flatten()
    })
}

fn pool_status(session: &SessionInfo, latency_ms: Option<u32>) -> String {
    let selection = &session.selection;
    // Under a rotating strategy every connection may leave by a different node,
    // so "now → X" would be a lie; what is true and useful there is how many
    // nodes the core still considers eligible.
    let current = match selection.selecting.as_deref() {
        Some(handle) => format!("now → {handle}"),
        None => {
            let known = selection
                .members
                .iter()
                .filter(|member| member.in_rotation.is_some())
                .count();
            if known == 0 {
                "no live reading".to_string()
            } else {
                let live = selection
                    .members
                    .iter()
                    .filter(|member| member.in_rotation == Some(true))
                    .count();
                format!("{live}/{known} in rotation")
            }
        }
    };
    let stale = if selection.stale { ", stale" } else { "" };
    let latency = latency_ms
        .map(|value| format!("{value} ms"))
        .unwrap_or_else(|| "—".to_string());
    // An unnamed pool still reads fine; a named one saves the reader from
    // matching six hostnames against the group they had in mind.
    let label = if selection.name.is_empty() {
        "pool".to_string()
    } else {
        format!("pool {:?}", selection.name)
    };
    // Providers list one host many times, so the node count alone overstates
    // the spread. Said only when it differs: on a pool where every node is its
    // own host, "6 nodes on 6 exits" is one number twice.
    let nodes = match selection.endpoints {
        exits if exits > 0 && exits < selection.members.len() => format!(
            "{} nodes on {exits} exit{}",
            selection.members.len(),
            if exits == 1 { "" } else { "s" }
        ),
        _ => format!("{} nodes", selection.members.len()),
    };
    let mut output = format!(
        "{}  socks {}:{}  {latency}\nselection: {label} ({}, {nodes}, {current}{stale})\n",
        session.state, session.address, session.socks_port, selection.strategy,
    );
    for member in &selection.members {
        let health = match member.in_rotation {
            Some(true) => "✓",
            Some(false) => "✗",
            None => "?",
        };
        let handle = member.alias.as_deref().unwrap_or(&member.server_id);
        output.push_str(&format!("  {health} {handle}  {}\n", member.name));
    }
    output
}

fn endpoint_ip(server: &Server) -> CliResult<IpAddr> {
    if let Ok(ip) = server.address.parse() {
        return Ok(ip);
    }
    (server.address.as_str(), server.port)
        .to_socket_addrs()
        .map_err(Failure::error)?
        .next()
        .map(|address| address.ip())
        .ok_or_else(|| {
            Failure::message(format!(
                "endpoint {}:{} resolved to no addresses",
                server.address, server.port
            ))
        })
}

fn print_json(value: &impl Serialize) -> CliResult {
    let json = serde_json::to_string(value).map_err(Failure::error)?;
    print_line(json)
}

fn print_line(value: impl std::fmt::Display) -> CliResult {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{value}").map_err(Failure::error)
}

fn print_text(value: String) -> CliResult {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(value.as_bytes()).map_err(Failure::error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_map_to_the_stable_exit_codes() {
        assert_eq!(exit_code_for(FailureKind::Error), 1);
        assert_eq!(exit_code_for(FailureKind::NotConnected), 3);
        assert_eq!(exit_code_for(FailureKind::DaemonUnavailable), 4);
    }

    #[test]
    fn session_table_is_aligned_without_borders() {
        let sessions = [
            SessionOutput {
                profile: "default".to_string(),
                state: "connected".to_string(),
                server_id: Some("id-one".to_string()),
                server_alias: Some("ch-trojan".to_string()),
                server_name: Some("Swiss".to_string()),
                address: "127.0.0.1".to_string(),
                socks_port: 10808,
                http_port: 10809,
                latency_ms: Some(84),
                error: None,
                owns_system_proxy: false,
                interface: None,
                selection: None,
            },
            SessionOutput {
                profile: "work".to_string(),
                state: "connected".to_string(),
                server_id: Some("id-two".to_string()),
                server_alias: Some("nl-vless".to_string()),
                server_name: Some("Dutch".to_string()),
                address: "127.72.14.1".to_string(),
                socks_port: 10808,
                http_port: 10809,
                latency_ms: Some(112),
                error: None,
                owns_system_proxy: false,
                interface: None,
                selection: None,
            },
        ];

        assert_eq!(
            session_table(&sessions),
            "PROFILE  STATE      SERVER     ADDRESS            LATENCY\n\
             default  connected  ch-trojan  127.0.0.1:10808    84ms\n\
             work     connected  nl-vless   127.72.14.1:10808  112ms\n"
        );
    }

    #[test]
    fn session_table_adds_the_device_column_only_when_needed() {
        let sessions = [
            SessionOutput {
                profile: "default".to_string(),
                state: "connected".to_string(),
                server_id: Some("id-one".to_string()),
                server_alias: Some("ch".to_string()),
                server_name: None,
                address: "127.0.0.1".to_string(),
                socks_port: 10808,
                http_port: 10809,
                latency_ms: Some(84),
                error: None,
                owns_system_proxy: false,
                interface: Some(oxidom_core::ipc::InterfaceInfo {
                    device: "oxi-default".to_string(),
                    ..Default::default()
                }),
                selection: None,
            },
            SessionOutput {
                profile: "work".to_string(),
                state: "connected".to_string(),
                server_id: Some("id-two".to_string()),
                server_alias: Some("nl".to_string()),
                server_name: None,
                address: "127.72.14.1".to_string(),
                socks_port: 10808,
                http_port: 10809,
                latency_ms: None,
                error: None,
                owns_system_proxy: false,
                interface: None,
                selection: None,
            },
        ];

        assert_eq!(
            session_table(&sessions),
            "PROFILE  STATE      SERVER  ADDRESS            DEVICE       LATENCY\n\
             default  connected  ch      127.0.0.1:10808    oxi-default  84ms\n\
             work     connected  nl      127.72.14.1:10808  —            —\n"
        );
    }

    #[test]
    fn pool_sessions_use_an_explicit_pool_label_and_member_status() {
        let selection = oxidom_core::ipc::SelectionInfo {
            kind: "pool".to_string(),
            name: String::new(),
            strategy: "roundRobin".to_string(),
            members: vec![
                oxidom_core::ipc::PoolMember {
                    server_id: "id-one".to_string(),
                    alias: Some("ch-one".to_string()),
                    name: "Swiss".to_string(),
                    tag: "s-ch-one".to_string(),
                    in_rotation: Some(true),
                },
                oxidom_core::ipc::PoolMember {
                    server_id: "id-two".to_string(),
                    alias: None,
                    name: "Dutch".to_string(),
                    tag: "s-id-two".to_string(),
                    in_rotation: Some(false),
                },
            ],
            // Both members share one host, which is exactly the case the node
            // count alone hides.
            endpoints: 1,
            // roundRobin has no single current exit, so the daemon leaves this
            // unset and the line reports the rotation instead.
            selecting: None,
            stale: true,
        };
        let session = SessionInfo {
            profile: "spread".to_string(),
            state: "connected".to_string(),
            address: "127.72.14.1".to_string(),
            socks_port: 10808,
            selection: selection.clone(),
            ..SessionInfo::default()
        };
        let output = SessionOutput::new(&session, Some(84));

        assert_eq!(
            session_table(&[output]),
            "PROFILE  STATE      SERVER   ADDRESS            LATENCY\n\
             spread   connected  pool(2)  127.72.14.1:10808  84ms\n"
        );
        assert_eq!(
            pool_status(&session, Some(84)),
            concat!(
                "connected  socks 127.72.14.1:10808  84 ms\n",
                "selection: pool (roundRobin, 2 nodes on 1 exit, 1/2 in rotation, stale)\n",
                "  ✓ ch-one  Swiss\n",
                "  ✗ id-two  Dutch\n",
            )
        );
    }

    /// A picking strategy does name a current exit, and says nothing about the
    /// members it did not pick.
    #[test]
    fn a_least_ping_pool_prints_its_current_exit() {
        let session = SessionInfo {
            profile: "spread".to_string(),
            state: "connected".to_string(),
            address: "127.72.14.1".to_string(),
            socks_port: 10808,
            selection: oxidom_core::ipc::SelectionInfo {
                kind: "pool".to_string(),
                name: "Europe".to_string(),
                strategy: "leastPing".to_string(),
                members: vec![
                    oxidom_core::ipc::PoolMember {
                        server_id: "id-one".to_string(),
                        alias: Some("ch-one".to_string()),
                        name: "Swiss".to_string(),
                        tag: "s-ch-one".to_string(),
                        in_rotation: None,
                    },
                    oxidom_core::ipc::PoolMember {
                        server_id: "id-two".to_string(),
                        alias: Some("nl-two".to_string()),
                        name: "Dutch".to_string(),
                        tag: "s-nl-two".to_string(),
                        in_rotation: None,
                    },
                ],
                // Two nodes on two hosts: the exit count says nothing the node
                // count did not, and the line does not print it.
                endpoints: 2,
                selecting: Some("nl-two".to_string()),
                stale: false,
            },
            ..SessionInfo::default()
        };

        assert_eq!(
            pool_status(&session, None),
            concat!(
                "connected  socks 127.72.14.1:10808  —\n",
                // Named here, unnamed in the roundRobin case above: both forms
                // are output the parsers downstream have to survive.
                "selection: pool \"Europe\" (leastPing, 2 nodes, now → nl-two)\n",
                "  ? ch-one  Swiss\n",
                "  ? nl-two  Dutch\n",
            )
        );
    }

    #[test]
    fn env_uses_both_session_endpoints_and_remote_dns_socks() {
        let session = SessionInfo {
            address: "127.72.14.1".to_string(),
            socks_port: 10808,
            http_port: 10809,
            ..SessionInfo::default()
        };

        assert_eq!(
            session_environment(&session),
            "export ALL_PROXY=socks5h://127.72.14.1:10808\n\
             export all_proxy=socks5h://127.72.14.1:10808\n\
             export HTTP_PROXY=http://127.72.14.1:10809\n\
             export http_proxy=http://127.72.14.1:10809\n\
             export HTTPS_PROXY=http://127.72.14.1:10809\n\
             export https_proxy=http://127.72.14.1:10809\n\
             export NO_PROXY=localhost,127.0.0.0/8,::1\n\
             export no_proxy=localhost,127.0.0.0/8,::1\n"
        );
    }

    #[test]
    fn proxied_probe_results_are_selected_by_profile_before_direct_cache() {
        let direct = LatencyReading::ok(
            10,
            ProbeRoute::Direct,
            oxidom_core::config::LatencyMethod::Tcp,
        );
        let proxied = LatencyReading::ok(
            20,
            ProbeRoute::Proxied,
            oxidom_core::config::LatencyMethod::HttpGet,
        );
        let probes = ProbeState {
            version: PROBE_STATE_VERSION,
            readings: std::collections::HashMap::from([("same".to_string(), direct)]),
            proxied: std::collections::HashMap::from([("work".to_string(), proxied)]),
            ..ProbeState::default()
        };
        let sessions = [SessionInfo {
            profile: "work".to_string(),
            state: "connected".to_string(),
            server_id: Some("same".to_string()),
            ..SessionInfo::default()
        }];

        assert_eq!(
            probe_reading_for_server(&probes, &sessions, "same").map(|reading| reading.value),
            Some(Some(20))
        );

        let legacy = ProbeState {
            version: PROBE_STATE_VERSION,
            readings: std::collections::HashMap::from([("same".to_string(), proxied)]),
            ..ProbeState::default()
        };
        assert_eq!(current_latency(&legacy, "work", "same"), Some(20));
        assert_eq!(current_pool_latency(&probes, "work"), Some(20));

        let pending = ProbeState {
            version: PROBE_STATE_VERSION,
            running: vec!["pool:work".to_string()],
            proxied: probes.proxied,
            ..ProbeState::default()
        };
        assert_eq!(current_pool_latency(&pending, "work"), None);
    }
}
