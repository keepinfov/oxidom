//! The manual fallback for the source-pinned managed Xray release.
//!
//! It is used only when automatic installation failed. Package-manager Xray
//! builds are not suggested: their version can differ from the one this release
//! validates, which would make the fallback unusable by design.

/// What to tell someone who has no core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreInstall {
    /// The distribution packages a core, so one command is the whole answer.
    Package {
        command: &'static str,
        /// A caveat where the package is real but not in the default setup —
        /// an overlay, or a branch the user may not be on.
        note: Option<&'static str>,
    },
    /// It does not. The release built for this machine, and what to do with it.
    Download {
        /// Direct link to the asset, not the releases page.
        url: String,
        /// A recipe that ends with a core on `$PATH`.
        commands: String,
    },
    /// No release is published for this architecture, so neither answer is
    /// honest. Saying so beats offering a download that cannot run.
    Unsupported { arch: String },
}

impl CoreInstall {
    /// The single line a narrow row can show.
    pub fn summary(&self) -> String {
        match self {
            CoreInstall::Package { command, .. } => command.to_string(),
            CoreInstall::Download { url, .. } => url.clone(),
            CoreInstall::Unsupported { arch } => {
                format!("No Xray release is published for {arch}")
            }
        }
    }

    /// What a Copy button puts on the clipboard: the commands where there are
    /// any, since a URL alone still leaves the work undone.
    pub fn clipboard(&self) -> String {
        match self {
            CoreInstall::Package { command, .. } => command.to_string(),
            CoreInstall::Download { commands, .. } => commands.clone(),
            CoreInstall::Unsupported { arch } => {
                format!("No Xray release is published for {arch}")
            }
        }
    }

    /// A page worth opening in a browser, when there is one.
    pub fn link(&self) -> Option<&str> {
        match self {
            CoreInstall::Download { url, .. } => Some(url),
            _ => None,
        }
    }
}

/// The release asset for an OS and architecture, as
/// [`std::env::consts`] spells them.
///
/// Only the builds a desktop or server plausibly runs are mapped. The rest of
/// the release — mips, loong64, s390x — exists, but a wrong guess here would
/// hand someone an archive that cannot execute, and `None` says so instead.
pub fn release_asset(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("Xray-linux-64.zip"),
        ("linux", "aarch64") => Some("Xray-linux-arm64-v8a.zip"),
        ("linux", "arm") => Some("Xray-linux-arm32-v7a.zip"),
        ("linux", "x86") => Some("Xray-linux-32.zip"),
        ("linux", "riscv64") => Some("Xray-linux-riscv64.zip"),
        ("linux", "powerpc64") => Some("Xray-linux-ppc64le.zip"),
        ("macos", "x86_64") => Some("Xray-macos-64.zip"),
        ("macos", "aarch64") => Some("Xray-macos-arm64-v8a.zip"),
        _ => None,
    }
}

/// Where a hand-installed core goes. On `$PATH` for every shell, and outside
/// `/usr`, which belongs to the package manager.
const INSTALL_PREFIX: &str = "/usr/local/bin";

fn download_recipe(asset: &str, sha256: &str) -> CoreInstall {
    let url = format!(
        "https://github.com/XTLS/Xray-core/releases/download/v{}/{}",
        crate::xray::managed::VERSION,
        asset
    );
    // `unzip xray` extracts the one member: the archive also carries README and
    // LICENSE files nobody needs on their PATH.
    let commands = format!(
        "curl -fLO {url}\n\
         echo '{sha256}  {asset}' | sha256sum -c -\n\
         unzip -o {asset} xray geoip.dat geosite.dat\n\
         sudo install -Dm755 xray {INSTALL_PREFIX}/xray\n\
         xray version"
    );
    CoreInstall::Download { url, commands }
}

/// What to tell the user, given an `os-release` file and this machine's target.
///
/// Which distributions genuinely package a core was checked rather than
/// assumed: Alpine, Arch (AUR), Nix, Gentoo's GURU overlay and Homebrew do.
/// **Debian, Ubuntu, Fedora, openSUSE and RHEL do not**, which is most people —
/// so the download path is the common one and gets the exact asset, not a
/// page to go hunting on.
pub fn xray_install(_os_release: &str, os: &str, arch: &str) -> CoreInstall {
    let Some(release) = crate::xray::managed::release_for(os, arch) else {
        return CoreInstall::Unsupported {
            arch: arch.to_string(),
        };
    };
    download_recipe(release.asset, release.sha256)
}

/// Answer for the machine this is running on.
pub fn xray_install_here() -> CoreInstall {
    // /usr/lib is the vendor copy; /etc wins when both exist, per os-release(5).
    let text = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .unwrap_or_default();
    xray_install(&text, std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manual_fallback_is_the_same_pinned_release() {
        let CoreInstall::Download { url, commands } = xray_install("ID=arch\n", "linux", "x86_64")
        else {
            panic!("the supported platform needs a manual release fallback");
        };
        assert!(url.contains("/v26.3.27/Xray-linux-64.zip"), "{url}");
        assert!(commands.contains("sha256sum -c"), "{commands}");
        assert!(commands.contains("geoip.dat geosite.dat"), "{commands}");
    }

    /// The asset has to match the machine, or the recipe hands someone an
    /// archive that cannot execute.
    #[test]
    fn each_machine_is_offered_the_build_that_runs_on_it() {
        assert_eq!(release_asset("linux", "x86_64"), Some("Xray-linux-64.zip"));
        assert_eq!(
            release_asset("linux", "aarch64"),
            Some("Xray-linux-arm64-v8a.zip")
        );
    }

    /// Where upstream publishes nothing, say so. An unsupported architecture
    /// offered a download would be sent to a 404, and told nothing useful.
    #[test]
    fn an_architecture_with_no_release_is_told_so_rather_than_guessed_at() {
        let answer = xray_install("ID=debian\n", "linux", "sparc64");
        assert_eq!(
            answer,
            CoreInstall::Unsupported {
                arch: "sparc64".to_string()
            }
        );
        assert!(answer.link().is_none(), "nothing to open");
        assert!(answer.summary().contains("sparc64"));
    }
}
