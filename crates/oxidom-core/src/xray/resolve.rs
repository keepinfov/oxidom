//! Locating the Xray core.

use anyhow::Result;

pub use crate::resolve::BinarySource as XraySource;
use crate::resolve::BinarySpec;
pub use crate::resolve::Resolved as ResolvedXray;

pub const XRAY: BinarySpec = BinarySpec {
    what: "Xray",
    default_command: "xray",
    env_var: "OXIDOM_XRAY_BIN",
    config_label: "Settings › Xray binary",
};

/// Resolve a matching explicit core, or install and resolve the managed pinned
/// release before accepting an environment or `PATH` fallback.
pub fn resolve(configured: &str) -> Result<ResolvedXray> {
    if !configured.trim().is_empty() {
        let resolved = crate::resolve::resolve(&XRAY, configured)?;
        crate::xray::managed::require_version(&resolved.path)?;
        return Ok(resolved);
    }

    match crate::xray::managed::ensure_installed() {
        Ok(path) => Ok(ResolvedXray {
            path,
            // The generic resolver has no managed source. RuntimeInfo suppresses
            // this placeholder for the managed path rather than lying that it
            // came from $PATH.
            source: crate::resolve::BinarySource::Path,
        }),
        Err(managed_error) => {
            // Offline systems can continue with an already-installed exact
            // release. Anything else remains refused: generated config behavior
            // is pinned to `managed::VERSION`, not to a range of releases.
            let fallback = crate::resolve::resolve(&XRAY, configured).and_then(|resolved| {
                crate::xray::managed::require_version(&resolved.path)?;
                Ok(resolved)
            });
            match fallback {
                Ok(resolved) => {
                    log::warn!(
                        "could not install managed Xray {} ({managed_error:#}); using the matching core at {}",
                        crate::xray::managed::VERSION,
                        resolved.path.display()
                    );
                    Ok(resolved)
                }
                Err(fallback_error) => Err(anyhow::anyhow!(
                    "could not install managed Xray {}: {managed_error:#}; fallback core unavailable: {fallback_error:#}",
                    crate::xray::managed::VERSION
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::BinarySource;

    #[test]
    fn missing_configured_binary_names_the_xray_setting() {
        let error = crate::resolve::resolve_request(
            &XRAY,
            "/nonexistent-oxidom/xray",
            BinarySource::Config,
            None,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "the Xray binary from Settings › Xray binary does not exist: \
             /nonexistent-oxidom/xray"
        );
    }

    #[test]
    fn invalid_environment_binary_names_the_xray_variable() {
        let error = crate::resolve::resolve_request(
            &XRAY,
            "/nonexistent-oxidom/xray",
            BinarySource::Environment,
            None,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "the Xray binary from $OXIDOM_XRAY_BIN does not exist: /nonexistent-oxidom/xray"
        );
    }

    #[test]
    fn path_error_text_is_unchanged() {
        let search = std::env::join_paths(["/no/dir0", "/no/dir1"]).unwrap();
        let error = crate::resolve::resolve_request(
            &XRAY,
            "xray",
            BinarySource::Path,
            Some(search.as_os_str()),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "`xray` was not found on $PATH (looked in /no/dir0, /no/dir1) — install xray, or \
             set its full path in Settings"
        );
    }
}
