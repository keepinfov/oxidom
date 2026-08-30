//! The Xray release oxidom has tested, and its private on-demand installation.
//!
//! The release archive is checked against a digest embedded in this source,
//! rather than one fetched beside it. A response that can replace both an
//! archive and a sidecar can make a corrupt archive look verified.

use std::io::{Cursor, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::{fsutil, paths};

/// The only Xray release whose generated configuration is supported.
pub const VERSION: &str = "26.3.27";

/// Release downloads are serialized because an install consists of several
/// files in one directory. A second caller observes the complete installation,
/// never a half-extracted one.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

const MAX_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    pub asset: &'static str,
    pub sha256: &'static str,
}

/// The supported releases, keyed by Rust's platform names. The application is
/// Linux-only, so no macOS archive is installed behind the user's back.
pub fn release_for(os: &str, arch: &str) -> Option<Release> {
    match (os, arch) {
        ("linux", "x86_64") => Some(Release {
            asset: "Xray-linux-64.zip",
            sha256: "23cd9af937744d97776ee35ecad4972cf4b2109d1e0fe6be9930467608f7c8ae",
        }),
        ("linux", "aarch64") => Some(Release {
            asset: "Xray-linux-arm64-v8a.zip",
            sha256: "4d30283ae614e3057f730f67cd088a42be6fdf91f8639d82cb69e48cde80413c",
        }),
        _ => None,
    }
}

pub fn release_here() -> Result<Release> {
    release_for(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        anyhow::anyhow!(
            "oxidom manages Xray {VERSION} only on Linux x86_64 and aarch64; set the Xray binary in Settings to a matching core on this platform"
        )
    })
}

pub fn install_dir() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("xray").join(VERSION))
}

pub fn binary_path() -> Result<PathBuf> {
    Ok(install_dir()?.join("xray"))
}

pub fn is_managed(path: &Path) -> bool {
    binary_path().is_ok_and(|managed| path == managed)
}

/// Return the installed core, downloading the pinned release if necessary.
pub fn ensure_installed() -> Result<PathBuf> {
    let _guard = crate::sync::lock(&INSTALL_LOCK);
    let binary = binary_path()?;
    if is_complete_install(&binary) {
        match require_version(&binary) {
            Ok(()) => return Ok(binary),
            Err(error) => {
                // This path belongs to the managed release, so an interrupted
                // or replaced install is repaired from the pinned archive.
                log::warn!(
                    "managed Xray {} is invalid; reinstalling the pinned release: {error:#}",
                    VERSION
                );
            }
        }
    }

    let release = release_here()?;
    let url = format!(
        "https://github.com/XTLS/Xray-core/releases/download/v{VERSION}/{}",
        release.asset
    );
    let agent = ureq::AgentBuilder::new().timeout(FETCH_TIMEOUT).build();
    let response = agent
        .get(&url)
        .call()
        .map_err(|error| download_error(release.asset, error))?;
    let total = response
        .header("content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if total > MAX_ARCHIVE_BYTES {
        bail!(
            "{} is {total} bytes, larger than the {MAX_ARCHIVE_BYTES} byte Xray release limit",
            release.asset
        );
    }
    let mut bytes = Vec::with_capacity(total.min(MAX_ARCHIVE_BYTES) as usize);
    response
        .into_reader()
        .take(MAX_ARCHIVE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", release.asset))?;
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        bail!(
            "{} is larger than the {MAX_ARCHIVE_BYTES} byte Xray release limit",
            release.asset
        );
    }
    let actual = sha256_hex(&bytes);
    if actual != release.sha256 {
        bail!(
            "{} does not match oxidom's pinned SHA-256 (got {actual}, expected {})",
            release.asset,
            release.sha256
        );
    }
    install_archive(&bytes, &install_dir()?)?;
    require_version(&binary)?;
    Ok(binary)
}

/// Refuse an arbitrary core even when it is executable: generated configuration
/// semantics are tied to the release above, not merely to an `xray` command.
pub fn require_version(binary: &Path) -> Result<()> {
    let output = std::process::Command::new(binary)
        .arg("version")
        .output()
        .with_context(|| format!("running {} version", binary.display()))?;
    if !output.status.success() {
        bail!("{} version exited with {}", binary.display(), output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if version_matches(&text) {
        return Ok(());
    }
    let found =
        crate::versions::core_version(&text).unwrap_or_else(|| "no version output".to_string());
    bail!(
        "{} is {found}; oxidom requires Xray {VERSION} because its generated config is pinned to that release",
        binary.display()
    );
}

pub fn version_matches(output: &str) -> bool {
    matches!(
        crate::versions::core_version(output).as_deref(),
        Some(version) if version == format!("Xray {VERSION}")
    )
}

fn install_archive(bytes: &[u8], dir: &Path) -> Result<()> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).context("reading the Xray archive")?;
    let binary = archive
        .by_name("xray")
        .context("the Xray archive has no xray executable")?;
    if binary.is_dir() || binary.size() == 0 || binary.size() > MAX_ARCHIVE_BYTES {
        bail!("the Xray archive contains an invalid xray executable");
    }
    drop(binary);
    for name in ["xray", "geoip.dat", "geosite.dat"] {
        let entry = archive
            .by_name(name)
            .with_context(|| format!("the Xray archive has no {name}"))?;
        if entry.is_dir() || entry.size() > MAX_ARCHIVE_BYTES {
            bail!("the Xray archive contains an invalid {name}");
        }
        let mut content = Vec::with_capacity(entry.size() as usize);
        entry
            .take(MAX_ARCHIVE_BYTES + 1)
            .read_to_end(&mut content)
            .with_context(|| format!("extracting {name} from the Xray archive"))?;
        if content.len() as u64 > MAX_ARCHIVE_BYTES {
            bail!("{name} is too large in the Xray archive");
        }
        let target = dir.join(name);
        fsutil::write_private_atomic(&target, &content)
            .with_context(|| format!("installing {}", target.display()))?;
        if name == "xray" {
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("making {} executable", target.display()))?;
        }
    }
    Ok(())
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn is_complete_install(binary: &Path) -> bool {
    is_executable(binary)
        && ["geoip.dat", "geosite.dat"].iter().all(|name| {
            std::fs::metadata(binary.with_file_name(name))
                .map(|meta| meta.is_file() && meta.len() >= 1024)
                .unwrap_or(false)
        })
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn download_error(asset: &str, error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(code, _) => anyhow::anyhow!(
            "fetching pinned Xray {VERSION} ({asset}): the release host responded with HTTP {code}"
        ),
        ureq::Error::Transport(transport) => anyhow::anyhow!(
            "fetching pinned Xray {VERSION} ({asset}): {}",
            transport.kind()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version_script(version: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "oxidom-managed-version-{}-{}",
            std::process::id(),
            version.replace(' ', "-")
        ));
        std::fs::write(&path, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn supported_platforms_have_the_source_pinned_release() {
        assert_eq!(
            release_for("linux", "x86_64"),
            Some(Release {
                asset: "Xray-linux-64.zip",
                sha256: "23cd9af937744d97776ee35ecad4972cf4b2109d1e0fe6be9930467608f7c8ae",
            })
        );
        assert_eq!(
            release_for("linux", "aarch64").map(|release| release.asset),
            Some("Xray-linux-arm64-v8a.zip")
        );
        assert_eq!(release_for("macos", "aarch64"), None);
    }

    #[test]
    fn only_the_pinned_xray_version_is_accepted() {
        assert!(version_matches(
            "Xray 26.3.27 (Xray, Penetrates Everything.)"
        ));
        assert!(!version_matches(
            "Xray 26.3.28 (Xray, Penetrates Everything.)"
        ));
        assert!(!version_matches("v2ray 26.3.27"));
    }

    #[test]
    fn an_external_core_must_report_the_pinned_version() {
        let matching = version_script("Xray 26.3.27 (test core)");
        require_version(&matching).expect("the pinned core is accepted");
        let other = version_script("Xray 26.3.28 (test core)");
        let error = require_version(&other).unwrap_err().to_string();
        assert!(error.contains("requires Xray 26.3.27"), "{error}");
        std::fs::remove_file(matching).ok();
        std::fs::remove_file(other).ok();
    }
}
