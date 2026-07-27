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

/// Resolve the Xray binary through the shared config → environment → `PATH`
/// policy while preserving the established Xray diagnostics.
pub fn resolve(configured: &str) -> Result<ResolvedXray> {
    crate::resolve::resolve(&XRAY, configured)
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
