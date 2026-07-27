mod cli;
mod daemon;

use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, ToSocketAddrs};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use oxidom_core::cli_json::{ProfileOutput, ServerOutput, StatusOutput, SubscriptionOutput};
use oxidom_core::client::DaemonClient;
use oxidom_core::handle::{self, HandleMatch};
use oxidom_core::ipc::{PROBE_STATE_VERSION, ProbeFailure, ProbeRoute, ProbeState, ProfileEntry};
use oxidom_core::model::{Server, Subscription};
use oxidom_core::profile::{Profile, ProfileProxy};
use serde::Serialize;

use crate::cli::{Cli, Command, ListTarget, ProfileCommand};

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
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
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
        Command::Status { json } => status(json),
        Command::Ip { egress, fresh } => ip(egress, fresh),
        Command::List { target, json } => list(target, json),
        Command::Ping { handle } => ping(&handle),
        Command::Alias { handle, new } => set_alias(&handle, &new),
        Command::Profile { command } => profile_command(command),
        Command::Gui { background } => cli::run_gui(background).map_err(Failure::error),
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
        Command::Run { args } => oxidom_core::netns::run(&args).map_err(Failure::error),
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
    Ok(())
}

fn down(profile: Option<&str>) -> CliResult {
    let client = existing_client()?;
    let stopped = client.down(profile.unwrap_or("")).map_err(Failure::error)?;
    if !stopped {
        // Stopping is idempotent on purpose. `oxidom@work`'s ExecStop runs
        // whenever that unit stops, including long after `home` took the
        // tunnel over; failing here would leave the unit in a failed state for
        // having correctly done nothing. The post-condition asked for — this
        // profile is not running — already holds.
        eprintln!(
            "profile {:?} does not own the active tunnel; nothing to stop",
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

fn status(json: bool) -> CliResult {
    let client = existing_client()?;
    let (status, server) = connected_server(&client)?;
    let config = client.settings().map_err(Failure::error)?;
    let probes = client.probe_state().map_err(Failure::error)?;
    let latency_ms = current_latency(&probes, &server.id);
    let output = StatusOutput::new(&status, Some(&server), &config, latency_ms);

    if json {
        print_json(&output)
    } else {
        let handle = server.alias.as_deref().unwrap_or(server.id.as_str());
        let latency = latency_ms
            .map(|value| format!("{value} ms"))
            .unwrap_or_else(|| "—".to_string());
        print_line(format!(
            "{}  {}  {}  socks {}  {}",
            output.state, handle, server.name, output.socks_port, latency
        ))
    }
}

fn ip(egress: bool, fresh: bool) -> CliResult {
    let client = existing_client()?;
    let (_, server) = connected_server(&client)?;
    let address = if egress {
        let config = client.settings().map_err(Failure::error)?;
        oxidom_core::egress::address(&server.id, config.socks_port, fresh)
            .map_err(Failure::error)?
    } else {
        endpoint_ip(&server)?
    };
    print_line(address)
}

fn list(target: ListTarget, json: bool) -> CliResult {
    let client = existing_client()?;
    match target {
        ListTarget::Servers => list_servers(&client, json),
        ListTarget::Profiles => list_profiles(&client, json),
        ListTarget::Subscriptions => list_subscriptions(&client, json),
    }
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
            return print_probe_result(&probes, server);
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

fn print_probe_result(probes: &ProbeState, server: &Server) -> CliResult {
    let reading = probes.readings.get(&server.id).ok_or_else(|| {
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
    let reason = match reading.failure {
        Some(ProbeFailure::Unreachable) => "server is unreachable",
        Some(ProbeFailure::Timeout) => "probe timed out",
        Some(ProbeFailure::NoNetwork) => "no network connection",
        Some(ProbeFailure::Unknown) => "probe could not run on this machine",
        None => "daemon returned an invalid probe reading",
    };
    Err(Failure::message(format!(
        "{}: {reason}",
        display_handle(server)
    )))
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
    profile.validate().map_err(Failure::error)?;
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

fn connected_server(client: &DaemonClient) -> CliResult<(oxidom_core::ipc::StatusInfo, Server)> {
    let status = client.status().map_err(Failure::error)?;
    if status.state != "connected" {
        return Err(Failure::not_connected());
    }
    let active_id = status
        .active_id
        .as_deref()
        .ok_or_else(|| Failure::message("daemon reports connected without an active server"))?;
    let subscriptions = client.subscriptions().map_err(Failure::error)?;
    let server = subscriptions
        .iter()
        .flat_map(|subscription| &subscription.servers)
        .find(|server| server.id == active_id)
        .cloned()
        .ok_or_else(|| Failure::message("the active server is no longer in the daemon store"))?;
    Ok((status, server))
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

fn current_latency(probes: &ProbeState, server_id: &str) -> Option<u32> {
    if probes.version < PROBE_STATE_VERSION
        || probes.running.iter().any(|id| id == server_id)
        || probes.queued.iter().any(|id| id == server_id)
    {
        return None;
    }
    probes.readings.get(server_id).and_then(|reading| {
        (reading.route == ProbeRoute::Proxied && reading.failure.is_none())
            .then_some(reading.value)
            .flatten()
    })
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
}
