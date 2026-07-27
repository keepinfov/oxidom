//! Locating the nftables helper.

use anyhow::Result;

use crate::resolve::BinarySpec;
pub use crate::resolve::Resolved;

pub const NFT: BinarySpec = BinarySpec {
    what: "nft",
    default_command: "nft",
    env_var: "OXIDOM_NFT_BIN",
    config_label: "Settings › nft binary",
};

pub fn resolve(configured: &str) -> Result<Resolved> {
    crate::resolve::resolve(&NFT, configured)
}

#[cfg(test)]
mod tests {
    use crate::resolve::BinarySource;

    use super::NFT;

    #[test]
    fn diagnostics_name_nft_settings() {
        let error = crate::resolve::resolve_request(
            &NFT,
            "/nonexistent-oxidom/nft",
            BinarySource::Config,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Settings › nft binary"), "{error}");
        assert!(error.contains("nft binary"), "{error}");
    }
}
