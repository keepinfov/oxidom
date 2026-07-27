//! Per-process routing through a profile's cgroup-marked systemd scope.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::ipc::InterfaceInfo;

/// Private hand-off from the outer CLI to the copy running inside the scope.
///
/// The scoped copy validates its actual cgroup before it replaces itself with
/// the target. A public hidden subcommand would still become a reserved profile
/// name, so an inherited variable keeps this implementation detail out of the
/// CLI grammar.
pub const SCOPED_CGROUP_ENV: &str = "OXIDOM_SCOPED_CGROUP";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CgroupSlice {
    pub path: String,
    pub level: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ScopedRequest {
    profile: String,
    uid: u32,
}

/// Path of the user slice for one profile, and the ancestor level nftables
/// needs for `socket cgroupv2`.
pub fn user_slice(profile: &str, uid: u32) -> Result<CgroupSlice> {
    // systemd names the cgroup directory after the unit verbatim, escapes and
    // all, so the path has to be built from the same string `--slice` receives.
    let unit = slice_unit(profile)?;
    let path = format!("user.slice/user-{uid}.slice/user@{uid}.service/{unit}");
    let level = u32::try_from(path.split('/').count()).context("cgroup slice path is too deep")?;
    Ok(CgroupSlice { path, level })
}

pub fn slice_unit(profile: &str) -> Result<String> {
    if !crate::profile::valid_name(profile) {
        bail!("invalid profile name {profile:?}");
    }
    // Literal dashes are hierarchy separators in a systemd slice name, and the
    // nesting they cause is not cosmetic. Unescaped, profile `work` would sit
    // at `oxidom.slice/oxidom-work.slice` while profile `work-eu` would sit
    // *below it*, at `oxidom.slice/oxidom-work.slice/oxidom-work-eu.slice` — so
    // the ancestor match written for `work` would also claim every process of
    // `work-eu` and push its traffic into the wrong tunnel.
    //
    // Escaping keeps each profile in one flat directory at a fixed depth. The
    // escape survives into the cgroup path verbatim: systemd really does create
    // a directory named `oxidom\x2dwork.slice`. Verified on systemd 258.
    Ok(format!(
        "oxidom\\x2d{}.slice",
        profile.replace('-', "\\x2d")
    ))
}

/// Extract the unified cgroup-v2 path from `/proc/<pid>/cgroup`.
pub fn parse_cgroup_v2(contents: &str) -> Result<String> {
    let path = contents
        .lines()
        .find_map(|line| {
            let (hierarchy, rest) = line.split_once(':')?;
            let (controllers, path) = rest.split_once(':')?;
            (hierarchy == "0" && controllers.is_empty()).then_some(path)
        })
        .context("/proc/self/cgroup has no unified cgroup-v2 entry")?
        .trim();
    if path.is_empty() {
        bail!("the unified cgroup-v2 entry has no path");
    }
    Ok(path.trim_start_matches('/').to_string())
}

pub fn verify_cgroup(actual: &str, expected: &CgroupSlice) -> Result<()> {
    let actual = actual.trim_start_matches('/');
    if actual == expected.path
        || actual
            .strip_prefix(&expected.path)
            .is_some_and(|tail| tail.starts_with('/'))
    {
        return Ok(());
    }
    bail!(
        "systemd put `oxidom run` in cgroup {actual:?}, expected it below {:?}; refusing to run \
         the command outside the selected profile",
        expected.path
    )
}

pub fn validate_interface<'a>(
    profile: &str,
    interface: Option<&'a InterfaceInfo>,
    interface_enabled: bool,
) -> Result<&'a InterfaceInfo> {
    if !interface_enabled {
        bail!(
            "profile `{profile}` has no network interface. Use `oxidom env {profile}` for \
             programs that honor proxy environment variables"
        );
    }
    let interface = interface.context(
        "the profile asks for an interface, but its running session has none; bring the profile \
         down and up again",
    )?;
    if !interface.up {
        bail!(
            "profile `{profile}` has an interface, but it is not up; bring the profile down and \
             up again"
        );
    }
    Ok(interface)
}

/// Build argv for either the ordinary `-- cmd ...` form or `-c "cmd ..."`.
/// `-c` is deliberately shell-word parsing only: no shell is started.
pub fn command_argv(args: &[String], command: Option<&str>) -> Result<Vec<String>> {
    let argv = match command {
        Some(command) => {
            shell_words::split(command).context("parsing the `oxidom run -c` command string")?
        }
        None => args.to_vec(),
    };
    if argv.is_empty() {
        bail!("no command given to `oxidom run`");
    }
    Ok(argv)
}

pub fn user_manager_socket(uid: u32, runtime_dir: Option<&OsStr>) -> PathBuf {
    runtime_dir
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")))
        .join("systemd/private")
}

fn require_user_manager(uid: u32) -> Result<()> {
    let socket = user_manager_socket(uid, std::env::var_os("XDG_RUNTIME_DIR").as_deref());
    require_user_manager_at(uid, &socket)
}

fn require_user_manager_at(uid: u32, socket: &Path) -> Result<()> {
    let available = std::fs::metadata(socket)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false);
    if !available {
        bail!(
            "no systemd --user manager is available for uid {uid} (expected its control socket at \
             {}); `oxidom run` requires a user systemd session",
            socket.display()
        );
    }
    Ok(())
}

fn scope_arguments(profile: &str, executable: &Path, argv: &[String]) -> Result<Vec<OsString>> {
    let mut arguments = vec![
        OsString::from("--user"),
        OsString::from("--scope"),
        OsString::from("--quiet"),
        OsString::from("--collect"),
        OsString::from("--expand-environment=no"),
        OsString::from(format!("--slice={}", slice_unit(profile)?)),
        OsString::from("--"),
        executable.as_os_str().to_os_string(),
    ];
    arguments.extend(argv.iter().map(OsString::from));
    Ok(arguments)
}

pub fn run_in_scope(profile: &str, uid: u32, argv: &[String]) -> Result<ExitStatus> {
    require_user_manager(uid)?;
    let executable = std::env::current_exe().context("locating the current oxidom executable")?;
    let arguments = scope_arguments(profile, &executable, argv)?;
    Command::new("systemd-run")
        .args(arguments)
        .env(
            SCOPED_CGROUP_ENV,
            serde_json::to_string(&ScopedRequest {
                profile: profile.to_string(),
                uid,
            })
            .context("serializing the scoped run request")?,
        )
        .status()
        .context(
            "starting a transient systemd --user scope; ensure systemd-run is installed and the \
             user manager is running",
        )
}

/// Validate the scope and replace this private wrapper with the target. This
/// runs before logging or any worker thread exists, so `exec` preserves the
/// target's terminal signal behavior exactly.
pub fn exec_scoped(request_json: &str, argv: &[OsString]) -> Result<()> {
    let request: ScopedRequest =
        serde_json::from_str(request_json).context("reading the scoped run request")?;
    let expected = user_slice(&request.profile, request.uid)?;
    if argv.is_empty() {
        bail!("the scoped `oxidom run` wrapper received no command");
    }
    let contents =
        std::fs::read_to_string("/proc/self/cgroup").context("reading /proc/self/cgroup")?;
    let actual = parse_cgroup_v2(&contents)?;
    verify_cgroup(&actual, &expected)?;

    // nft resolves a cgroup path to its inode while parsing the ruleset, so
    // the first rule cannot be installed until systemd has created this
    // scope. No target socket exists yet: marking still completes before exec.
    let daemon = crate::client::DaemonClient::connect_existing()
        .context("reaching the oxidom daemon from inside the systemd scope")?;
    let installed = daemon.mark_cgroup(&request.profile, request.uid)?;
    if installed != expected {
        bail!(
            "the daemon installed a mark for cgroup {:?}, but this command entered {:?}; \
             refusing to run outside the selected profile",
            installed.path,
            expected.path
        );
    }

    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).env_remove(SCOPED_CGROUP_ENV);
    let error = command.exec();
    Err(error).with_context(|| format!("executing {}", argv[0].to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_slice_level_is_derived_from_its_components() {
        let slice = user_slice("work", 1000).unwrap();
        assert_eq!(
            slice,
            CgroupSlice {
                path: "user.slice/user-1000.slice/user@1000.service/oxidom\\x2dwork.slice"
                    .to_string(),
                level: 4,
            }
        );
        assert_eq!(
            slice.level as usize,
            slice
                .path
                .split('/')
                .filter(|part| !part.is_empty())
                .count()
        );
    }

    /// Escaping is what stops one profile's rule from claiming another's
    /// traffic: unescaped, `work` would be an ancestor slice of `work-eu`, and
    /// an ancestor match is exactly what `socket cgroupv2 level N` performs.
    #[test]
    fn related_profile_names_stay_flat_and_never_nest() {
        let work = user_slice("work", 1000).unwrap();
        let work_eu = user_slice("work-eu", 1000).unwrap();
        assert_eq!(
            work_eu.path,
            "user.slice/user-1000.slice/user@1000.service/oxidom\\x2dwork\\x2deu.slice"
        );
        assert_eq!(
            slice_unit("work-eu").unwrap(),
            "oxidom\\x2dwork\\x2deu.slice"
        );
        assert_eq!(work.level, work_eu.level, "every profile sits at one depth");
        assert!(
            !work_eu.path.starts_with(&format!("{}/", work.path)),
            "{} must not contain {}",
            work.path,
            work_eu.path
        );
    }

    /// `user_slice` is the path nft matches and `slice_unit` is what systemd is
    /// asked for. They drifted apart once, and the only symptom was `oxidom
    /// run` refusing every command it was given.
    #[test]
    fn the_matched_path_ends_in_the_unit_systemd_is_asked_for() {
        for profile in ["work", "work-eu", "default"] {
            let slice = user_slice(profile, 1000).unwrap();
            let unit = slice_unit(profile).unwrap();
            assert_eq!(
                slice.path,
                format!("user.slice/user-1000.slice/user@1000.service/{unit}")
            );
            assert_eq!(slice.level, 4);
        }
    }

    #[test]
    fn cgroup_v2_parser_ignores_legacy_entries() {
        let contents = "7:devices:/legacy\n0::/user.slice/user-1000.slice/app.scope\n";
        assert_eq!(
            parse_cgroup_v2(contents).unwrap(),
            "user.slice/user-1000.slice/app.scope"
        );
        assert!(parse_cgroup_v2("7:devices:/legacy\n").is_err());
    }

    #[test]
    fn actual_scope_must_be_the_expected_slice_or_its_child() {
        let expected = user_slice("work", 1000).unwrap();
        // Shape taken verbatim from `systemd-run --user --scope` on systemd 258.
        verify_cgroup(
            "/user.slice/user-1000.slice/user@1000.service/oxidom\\x2dwork.slice/run-p1-i1.scope",
            &expected,
        )
        .unwrap();
        assert!(
            verify_cgroup(
                "user.slice/user-1000.slice/user@1000.service/oxidom\\x2dhome.slice/run-1.scope",
                &expected,
            )
            .unwrap_err()
            .to_string()
            .contains("refusing")
        );
        // The unescaped spelling is a different directory, not a synonym.
        assert!(
            verify_cgroup(
                "user.slice/user-1000.slice/user@1000.service/oxidom-work.slice/run-1.scope",
                &expected,
            )
            .is_err()
        );
        assert!(
            verify_cgroup(
                "user.slice/user-1000.slice/user@1000.service/\
                 oxidom\\x2dwork.slice-escape/run.scope",
                &expected,
            )
            .is_err()
        );
    }

    #[test]
    fn command_string_is_split_without_a_shell() {
        assert_eq!(
            command_argv(&[], Some("printf '%s %s' one two")).unwrap(),
            ["printf", "%s %s", "one", "two"]
        );
        assert!(command_argv(&[], Some("")).is_err());
        assert_eq!(
            command_argv(&["printf".into(), "$HOME".into()], None).unwrap(),
            ["printf", "$HOME"]
        );
    }

    #[test]
    fn scope_command_disables_systemd_environment_expansion() {
        let arguments = scope_arguments(
            "work",
            Path::new("/usr/bin/oxidom"),
            &["echo".into(), "${HOME}".into()],
        )
        .unwrap();
        assert_eq!(
            arguments,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--expand-environment=no",
                "--slice=oxidom\\x2dwork.slice",
                "--",
                "/usr/bin/oxidom",
                "echo",
                "${HOME}",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn proxy_only_refusal_keeps_the_b2_wording() {
        let error = validate_interface("work", None, false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "profile `work` has no network interface. Use `oxidom env work` for programs that \
             honor proxy environment variables"
        );
    }

    #[test]
    fn missing_user_manager_is_distinct_from_a_missing_interface() {
        let path =
            std::env::temp_dir().join(format!("oxidom-no-systemd-manager-{}", std::process::id()));
        let error = require_user_manager_at(1000, &path)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no systemd --user manager"), "{error}");
        assert!(error.contains("requires a user systemd session"), "{error}");
        assert!(!error.contains("network interface"), "{error}");
    }
}
