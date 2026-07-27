//! Locating external helper binaries with actionable diagnostics.
//!
//! A system daemon commonly has a much smaller environment than an
//! interactive shell. Every helper therefore follows the same explicit
//! config → environment → `PATH` resolution policy.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// How many `PATH` entries an error message lists before summarizing; a nix
/// `PATH` is kilobytes long and dumping it whole helps nobody.
const LISTED_DIRS: usize = 4;

/// Where the binary request came from, so an error can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinarySource {
    Config,
    Environment,
    Path,
}

/// Static policy for one external binary.
pub struct BinarySpec {
    /// Human-readable name used in errors.
    pub what: &'static str,
    /// Bare command searched for on `PATH` as the final fallback.
    pub default_command: &'static str,
    /// Environment variable used between config and `PATH`.
    pub env_var: &'static str,
    /// Human-readable config field used in errors.
    pub config_label: &'static str,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub path: PathBuf,
    pub source: BinarySource,
}

impl BinarySource {
    pub fn label(self, spec: &BinarySpec) -> String {
        match self {
            BinarySource::Config => spec.config_label.to_string(),
            BinarySource::Environment => format!("${}", spec.env_var),
            BinarySource::Path => "$PATH".to_string(),
        }
    }
}

/// Resolve `spec`. Priority: configured value, its environment variable, then
/// its default command on `PATH`. Whitespace-only values count as unset.
pub fn resolve(spec: &BinarySpec, configured: &str) -> Result<Resolved> {
    let (request, source) = request(spec, configured, std::env::var(spec.env_var).ok());
    resolve_request(spec, &request, source, std::env::var_os("PATH").as_deref())
}

/// Kept separate so unit tests and thin binary-specific wrappers can exercise
/// diagnostics without mutating the process environment.
pub(crate) fn request(
    spec: &BinarySpec,
    configured: &str,
    environment: Option<String>,
) -> (String, BinarySource) {
    let configured = configured.trim();
    if !configured.is_empty() {
        return (configured.to_string(), BinarySource::Config);
    }
    if let Some(value) = environment {
        let value = value.trim();
        if !value.is_empty() {
            return (value.to_string(), BinarySource::Environment);
        }
    }
    (spec.default_command.to_string(), BinarySource::Path)
}

/// A request containing a separator is a path and is checked directly; a bare
/// name is looked up in `search_path`.
pub(crate) fn resolve_request(
    spec: &BinarySpec,
    request: &str,
    source: BinarySource,
    search_path: Option<&OsStr>,
) -> Result<Resolved> {
    if request.contains('/') {
        return resolve_path(spec, Path::new(request), source);
    }

    let dirs: Vec<PathBuf> = search_path
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    if dirs.is_empty() {
        bail!(
            "cannot look up `{request}` because $PATH is empty — set an absolute path in Settings, \
             or point {} at the binary",
            spec.env_var
        );
    }
    for dir in &dirs {
        let candidate = dir.join(request);
        if is_executable(&candidate) {
            return Ok(Resolved {
                path: candidate,
                source,
            });
        }
    }
    bail!(
        "`{request}` was not found on $PATH (looked in {}) — install {}, or set its full path in \
         Settings",
        summarize(&dirs),
        spec.default_command
    )
}

fn resolve_path(spec: &BinarySpec, path: &Path, source: BinarySource) -> Result<Resolved> {
    let label = source.label(spec);
    // `metadata` follows symlinks, which is what we want: a nix store path
    // reached through a profile link must still count as executable.
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "the {} binary from {label} does not exist: {}",
                spec.what,
                path.display()
            )
        }
        Err(error) => bail!(
            "cannot read the {} binary from {label} ({}): {error}",
            spec.what,
            path.display()
        ),
    };
    if metadata.is_dir() {
        bail!(
            "the {} binary from {label} is a directory, not a program: {}",
            spec.what,
            path.display()
        );
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        bail!(
            "the {} binary from {label} is not executable (mode {:04o}): {}",
            spec.what,
            metadata.permissions().mode() & 0o7777,
            path.display()
        );
    }
    Ok(Resolved {
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

    const TEST: BinarySpec = BinarySpec {
        what: "test helper",
        default_command: "test-helper",
        env_var: "OXIDOM_TEST_HELPER_BIN",
        config_label: "Settings › test helper binary",
    };

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
    fn source_json_shape_is_frozen() {
        assert_eq!(
            serde_json::to_string(&[
                BinarySource::Config,
                BinarySource::Environment,
                BinarySource::Path
            ])
            .unwrap(),
            r#"["config","environment","path"]"#
        );
    }

    #[test]
    fn runtime_info_json_shape_is_unchanged() {
        let info = crate::ipc::RuntimeInfo {
            xray_path: Some("/nix/store/xray/bin/xray".to_string()),
            xray_source: Some(BinarySource::Config),
            ..crate::ipc::RuntimeInfo::default()
        };
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"xray_path":"/nix/store/xray/bin/xray","xray_error":null,"xray_source":"config","socks_port_locked":false,"http_port_locked":false,"socks_port":0,"http_port":0}"#
        );
    }

    #[test]
    fn config_wins_over_environment_and_path() {
        let (from_config, source) =
            request(&TEST, "  /opt/helper  ", Some("/env/helper".to_string()));
        assert_eq!(from_config, "/opt/helper");
        assert_eq!(source, BinarySource::Config);
    }

    #[test]
    fn blank_config_falls_through_to_environment_then_path() {
        let (from_env, source) = request(&TEST, "   ", Some("/env/helper".to_string()));
        assert_eq!(from_env, "/env/helper");
        assert_eq!(source, BinarySource::Environment);

        let (blank_env, source) = request(&TEST, "", Some("  ".to_string()));
        assert_eq!(blank_env, TEST.default_command);
        assert_eq!(source, BinarySource::Path);

        let (no_env, source) = request(&TEST, "", None);
        assert_eq!(no_env, TEST.default_command);
        assert_eq!(source, BinarySource::Path);
    }

    #[test]
    fn bare_name_is_found_on_the_search_path() {
        let dir = scratch("path-hit");
        let expected = write_executable(&dir, TEST.default_command);
        let search = std::env::join_paths(["/nonexistent-oxidom", dir.to_str().unwrap()]).unwrap();

        let resolved = resolve_request(
            &TEST,
            TEST.default_command,
            BinarySource::Path,
            Some(search.as_os_str()),
        )
        .unwrap();
        assert_eq!(resolved.path, expected);
        assert_eq!(resolved.source, BinarySource::Path);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_binary_names_its_source_and_path() {
        let error = resolve_request(
            &TEST,
            "/nonexistent-oxidom/helper",
            BinarySource::Config,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(TEST.config_label), "{error}");
        assert!(error.contains("/nonexistent-oxidom/helper"), "{error}");
        assert!(error.contains("does not exist"), "{error}");
    }

    #[test]
    fn non_executable_file_reports_its_mode() {
        let dir = scratch("not-exec");
        let path = dir.join(TEST.default_command);
        std::fs::write(&path, b"not a program").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let error = resolve_request(
            &TEST,
            path.to_str().unwrap(),
            BinarySource::Environment,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(TEST.env_var), "{error}");
        assert!(error.contains("not executable"), "{error}");
        assert!(error.contains("0644"), "{error}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_search_path_is_called_out_explicitly() {
        let error = resolve_request(&TEST, TEST.default_command, BinarySource::Path, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("$PATH is empty"), "{error}");
        assert!(error.contains(TEST.env_var), "{error}");
    }

    #[test]
    fn unfound_command_summarizes_a_long_search_path() {
        let dirs: Vec<PathBuf> = (0..7)
            .map(|i| PathBuf::from(format!("/no/dir{i}")))
            .collect();
        let search = std::env::join_paths(&dirs).unwrap();

        let error = resolve_request(
            &TEST,
            TEST.default_command,
            BinarySource::Path,
            Some(search.as_os_str()),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("/no/dir0"), "{error}");
        assert!(error.contains("and 3 more"), "{error}");
        assert!(!error.contains("/no/dir6"), "{error}");
    }
}
