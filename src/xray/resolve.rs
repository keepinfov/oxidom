//! Locating the Xray core.
//!
//! The daemon can run as a system service with a minimal environment and no
//! login `PATH`, so "it works in my shell" proves nothing. Resolve explicitly
//! and, on failure, say what was tried and where it came from — a bare
//! `No such file or directory` leaves the user with nothing to act on.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Default command looked up on `PATH` when nothing else is configured.
const DEFAULT_COMMAND: &str = "xray";

/// Env var set by the nix wrapper (`flake.nix`, `--set-default`).
const ENV_VAR: &str = "OXIDOM_XRAY_BIN";

/// How many `PATH` entries an error message lists before summarizing; a nix
/// `PATH` is kilobytes long and dumping it whole helps nobody.
const LISTED_DIRS: usize = 4;

/// Where the binary request came from, so an error can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XraySource {
    Config,
    Environment,
    Path,
}

impl XraySource {
    pub fn label(self) -> &'static str {
        match self {
            XraySource::Config => "Settings › Xray binary",
            XraySource::Environment => "$OXIDOM_XRAY_BIN",
            XraySource::Path => "$PATH",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedXray {
    pub path: PathBuf,
    pub source: XraySource,
}

/// Resolve the Xray binary. Priority: configured value > `$OXIDOM_XRAY_BIN` >
/// `xray` on `$PATH`. An empty or whitespace-only `configured` counts as unset.
pub fn resolve(configured: &str) -> Result<ResolvedXray> {
    let (request, source) = request(configured, std::env::var(ENV_VAR).ok());
    resolve_request(&request, source, std::env::var_os("PATH").as_deref())
}

/// Split out from [`resolve`] so tests can exercise the priority order without
/// mutating the process environment (`env::set_var` is `unsafe` in edition 2024).
fn request(configured: &str, environment: Option<String>) -> (String, XraySource) {
    let configured = configured.trim();
    if !configured.is_empty() {
        return (configured.to_string(), XraySource::Config);
    }
    if let Some(value) = environment {
        let value = value.trim();
        if !value.is_empty() {
            return (value.to_string(), XraySource::Environment);
        }
    }
    (DEFAULT_COMMAND.to_string(), XraySource::Path)
}

/// A request containing a separator is a path and is checked directly; a bare
/// name is looked up in `search_path`.
fn resolve_request(
    request: &str,
    source: XraySource,
    search_path: Option<&OsStr>,
) -> Result<ResolvedXray> {
    if request.contains('/') {
        return resolve_path(Path::new(request), source);
    }

    let dirs: Vec<PathBuf> = search_path
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    if dirs.is_empty() {
        bail!(
            "cannot look up `{request}` because $PATH is empty — set an absolute path in Settings, \
             or point {ENV_VAR} at the binary"
        );
    }
    for dir in &dirs {
        let candidate = dir.join(request);
        if is_executable(&candidate) {
            return Ok(ResolvedXray {
                path: candidate,
                source,
            });
        }
    }
    bail!(
        "`{request}` was not found on $PATH (looked in {}) — install xray, or set its full path \
         in Settings",
        summarize(&dirs)
    )
}

fn resolve_path(path: &Path, source: XraySource) -> Result<ResolvedXray> {
    let label = source.label();
    // `metadata` follows symlinks, which is what we want: a nix store path
    // reached through a profile link must still count as executable.
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "the Xray binary from {label} does not exist: {}",
                path.display()
            )
        }
        Err(error) => bail!(
            "cannot read the Xray binary from {label} ({}): {error}",
            path.display()
        ),
    };
    if metadata.is_dir() {
        bail!(
            "the Xray binary from {label} is a directory, not a program: {}",
            path.display()
        );
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        bail!(
            "the Xray binary from {label} is not executable (mode {:04o}): {}",
            metadata.permissions().mode() & 0o7777,
            path.display()
        );
    }
    Ok(ResolvedXray {
        path: path.to_path_buf(),
        source,
    })
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn summarize(dirs: &[PathBuf]) -> String {
    let listed: Vec<String> = dirs
        .iter()
        .take(LISTED_DIRS)
        .map(|dir| dir.display().to_string())
        .collect();
    let rest = dirs.len().saturating_sub(LISTED_DIRS);
    if rest == 0 {
        listed.join(", ")
    } else {
        format!("{}, and {rest} more", listed.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oxidom-resolve-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn config_wins_over_environment_and_path() {
        let (from_config, source) = request("  /opt/xray  ", Some("/env/xray".to_string()));
        assert_eq!(from_config, "/opt/xray");
        assert_eq!(source, XraySource::Config);
    }

    #[test]
    fn blank_config_falls_through_to_environment_then_path() {
        let (from_env, source) = request("   ", Some("/env/xray".to_string()));
        assert_eq!(from_env, "/env/xray");
        assert_eq!(source, XraySource::Environment);

        let (blank_env, source) = request("", Some("  ".to_string()));
        assert_eq!(blank_env, DEFAULT_COMMAND);
        assert_eq!(source, XraySource::Path);

        let (no_env, source) = request("", None);
        assert_eq!(no_env, DEFAULT_COMMAND);
        assert_eq!(source, XraySource::Path);
    }

    #[test]
    fn bare_name_is_found_on_the_search_path() {
        let dir = scratch("path-hit");
        let expected = write_executable(&dir, "xray");
        let search = std::env::join_paths(["/nonexistent-oxidom", dir.to_str().unwrap()]).unwrap();

        let resolved = resolve_request("xray", XraySource::Path, Some(search.as_os_str())).unwrap();
        assert_eq!(resolved.path, expected);
        assert_eq!(resolved.source, XraySource::Path);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_binary_names_its_source_and_path() {
        let error = resolve_request("/nonexistent-oxidom/xray", XraySource::Config, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Settings › Xray binary"), "{error}");
        assert!(error.contains("/nonexistent-oxidom/xray"), "{error}");
        assert!(error.contains("does not exist"), "{error}");
    }

    #[test]
    fn non_executable_file_reports_its_mode() {
        let dir = scratch("not-exec");
        let path = dir.join("xray");
        std::fs::write(&path, b"not a program").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let error = resolve_request(path.to_str().unwrap(), XraySource::Environment, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("$OXIDOM_XRAY_BIN"), "{error}");
        assert!(error.contains("not executable"), "{error}");
        assert!(error.contains("0644"), "{error}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_search_path_is_called_out_explicitly() {
        let error = resolve_request("xray", XraySource::Path, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("$PATH is empty"), "{error}");
    }

    #[test]
    fn unfound_command_summarizes_a_long_search_path() {
        let dirs: Vec<PathBuf> = (0..7)
            .map(|i| PathBuf::from(format!("/no/dir{i}")))
            .collect();
        let search = std::env::join_paths(&dirs).unwrap();

        let error = resolve_request("xray", XraySource::Path, Some(search.as_os_str()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("/no/dir0"), "{error}");
        assert!(error.contains("and 3 more"), "{error}");
        assert!(!error.contains("/no/dir6"), "{error}");
    }
}
