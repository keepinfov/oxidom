//! Locating the tun2socks helper.

use anyhow::Result;

use crate::resolve::BinarySpec;
pub use crate::resolve::Resolved;

pub const TUN2SOCKS: BinarySpec = BinarySpec {
    what: "tun2socks",
    default_command: "tun2socks",
    env_var: "OXIDOM_TUN2SOCKS_BIN",
    config_label: "Settings › tun2socks binary",
};

pub fn resolve(configured: &str) -> Result<Resolved> {
    crate::resolve::resolve(&TUN2SOCKS, configured)
}

#[cfg(test)]
mod tests {
    use crate::resolve::BinarySource;

    use super::TUN2SOCKS;

    #[test]
    fn diagnostics_name_tun2socks_settings() {
        let error = crate::resolve::resolve_request(
            &TUN2SOCKS,
            "/nonexistent-oxidom/tun2socks",
            BinarySource::Config,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Settings › tun2socks binary"), "{error}");
        assert!(error.contains("tun2socks binary"), "{error}");
    }
}
