//! How to install an Xray core on this machine.
//!
//! oxidom does not install one itself. Downloading and running a binary is the
//! one thing a program that carries other people's traffic should not do
//! casually: it needs a signature to check, a place to put it, and privileges
//! to get there. Xray publishes no signature — only a digest served from the
//! same host as the file — so the first of those cannot be satisfied, and the
//! honest answer is to tell someone exactly what to fetch rather than to fetch
//! it for them.
//!
//! "Exactly" is the whole point. Pointing at a releases page was technically
//! true and practically useless: that page carries eighty assets, and choosing
//! between `Xray-linux-64.zip` and `Xray-linux-arm64-v8a.zip` — then knowing
//! the archive holds a bare binary and nothing else — is where people came
//! unstuck. This module answers with a package manager command where one
//! exists, and otherwise with the release built for *this* machine.
//!
//! Note the geo data is a separate question, handled in
//! [`crate::xray::assets`]: the archive named here contains the core alone.

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

fn download_recipe(asset: &str) -> CoreInstall {
    let url = format!("https://github.com/XTLS/Xray-core/releases/latest/download/{asset}");
    // `unzip xray` extracts the one member: the archive also carries README and
    // LICENSE files nobody needs on their PATH.
    let commands = format!(
        "curl -LO {url}\n\
         unzip -o {asset} xray\n\
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
pub fn xray_install(os_release: &str, os: &str, arch: &str) -> CoreInstall {
    let Some(asset) = release_asset(os, arch) else {
        return CoreInstall::Unsupported {
            arch: arch.to_string(),
        };
    };

    // Homebrew is the only packaged answer on macOS, and it is not tied to an
    // os-release file.
    if os == "macos" {
        return CoreInstall::Package {
            command: "brew install xray",
            note: None,
        };
    }

    let mut id = String::new();
    let mut id_like = String::new();
    for line in os_release.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_ascii_lowercase();
        match key.trim() {
            "ID" => id = value,
            "ID_LIKE" => id_like = value,
            _ => {}
        }
    }
    // ID_LIKE carries the family, so EndeavourOS and Manjaro answer as Arch
    // without being listed one by one.
    let family = |name: &str| id == name || id_like.split_whitespace().any(|part| part == name);

    if family("nixos") {
        return CoreInstall::Package {
            command: "nix profile install nixpkgs#xray",
            note: None,
        };
    }
    if family("arch") {
        return CoreInstall::Package {
            command: "yay -S xray-bin",
            note: Some("from the AUR; no official repository carries a core"),
        };
    }
    if family("alpine") {
        return CoreInstall::Package {
            command: "apk add xray",
            note: Some("packaged on edge; a stable release may not have it yet"),
        };
    }
    if family("gentoo") {
        return CoreInstall::Package {
            command: "eselect repository enable guru && emerge net-proxy/xray",
            note: Some("from the GURU overlay, not the main tree"),
        };
    }
    // Everyone else, which is most people: Debian, Ubuntu, Fedora, openSUSE,
    // RHEL and their derivatives package nothing. Inventing an `apt install
    // xray` that fails would be trusted over the documentation.
    download_recipe(asset)
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

    fn linux(os_release: &str) -> CoreInstall {
        xray_install(os_release, "linux", "x86_64")
    }

    #[test]
    fn arch_and_its_family_are_pointed_at_the_aur() {
        let arch = "NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\n";
        assert!(matches!(
            linux(arch),
            CoreInstall::Package {
                command: "yay -S xray-bin",
                ..
            }
        ));

        // EndeavourOS names itself, and says what it is through ID_LIKE.
        let endeavour = "NAME=\"EndeavourOS\"\nID=endeavouros\nID_LIKE=\"arch\"\n";
        assert!(matches!(
            linux(endeavour),
            CoreInstall::Package {
                command: "yay -S xray-bin",
                ..
            }
        ));
    }

    #[test]
    fn nixos_installs_from_nixpkgs() {
        let nixos = "NAME=NixOS\nID=nixos\nVERSION=\"25.11 (Xantusia)\"\n";
        assert!(matches!(
            linux(nixos),
            CoreInstall::Package {
                command: "nix profile install nixpkgs#xray",
                ..
            }
        ));
    }

    /// A package that exists but not where the user is looking is a half
    /// answer, and the half that is missing is the one that wastes their time.
    #[test]
    fn a_package_outside_the_default_setup_says_so() {
        for (id, fragment) in [("alpine", "edge"), ("gentoo", "GURU"), ("arch", "AUR")] {
            let CoreInstall::Package { note, .. } = linux(&format!("ID={id}\n")) else {
                panic!("{id} packages a core");
            };
            let note = note.unwrap_or_else(|| panic!("{id} needs a caveat"));
            assert!(note.contains(fragment), "{id}: {note}");
        }
    }

    /// Debian, Ubuntu, Fedora, openSUSE and RHEL package no core — which is
    /// most people. They used to be handed a releases page carrying eighty
    /// assets; they now get the one built for this machine, and the commands
    /// that end with a core on `$PATH`.
    #[test]
    fn a_distribution_that_packages_no_core_gets_the_exact_release_for_this_machine() {
        for text in [
            "ID=ubuntu\nID_LIKE=debian\n",
            "ID=debian\n",
            "ID=fedora\nID_LIKE=\"rhel centos\"\n",
            "ID=opensuse-tumbleweed\n",
            "",
            "malformed without any equals sign\n",
        ] {
            let CoreInstall::Download { url, commands } = linux(text) else {
                panic!("{text:?} packages no core, so it must get a download");
            };
            assert!(url.ends_with("/Xray-linux-64.zip"), "{url}");
            assert!(
                !url.ends_with("/releases"),
                "a page of eighty assets is not an answer: {url}"
            );
            assert!(commands.contains("install -Dm755"), "{commands}");
            assert!(
                commands.contains("xray version"),
                "the recipe ends by proving it worked: {commands}"
            );
        }
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
        assert_eq!(release_asset("macos", "x86_64"), Some("Xray-macos-64.zip"));
        assert_eq!(
            release_asset("macos", "aarch64"),
            Some("Xray-macos-arm64-v8a.zip")
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

    /// macOS has one packaged answer and it does not come from os-release.
    #[test]
    fn macos_is_answered_by_homebrew_whatever_the_os_release_says() {
        assert!(matches!(
            xray_install("", "macos", "aarch64"),
            CoreInstall::Package {
                command: "brew install xray",
                ..
            }
        ));
    }

    /// Copy has to leave something that finishes the job. A URL on the
    /// clipboard still leaves the unzip, the install and the mode to guess at.
    #[test]
    fn copying_a_download_yields_the_commands_and_not_just_the_link() {
        let answer = linux("ID=debian\n");
        assert!(answer.clipboard().contains("curl -LO"));
        assert!(answer.clipboard().contains("sudo install"));
        assert_ne!(answer.clipboard(), answer.summary());
        // A package manager answer is one line, and the two agree.
        let packaged = linux("ID=nixos\n");
        assert_eq!(packaged.clipboard(), packaged.summary());
    }

    /// Quoting in os-release is optional and inconsistent between distros.
    #[test]
    fn quoted_and_bare_values_read_the_same() {
        assert_eq!(linux("ID=\"arch\"\n"), linux("ID=arch\n"));
    }
}
