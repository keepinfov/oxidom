//! Which command installs an Xray core on this machine.
//!
//! oxidom does not install one itself. Downloading and running a binary is the
//! one thing a program that carries other people's traffic should not do
//! casually: it needs a signature to check, a place to put it, and privileges
//! to get there. Naming the command the distribution already has is the honest
//! half of that, and it is the half people actually asked for.

/// The command that installs an Xray core, for the distribution described by
/// an `os-release` file, or `None` where the distribution packages none.
///
/// Only Nix and the AUR package a core at all; everywhere else the answer is a
/// release download, which the caller words for itself rather than pretending
/// a package manager will do it.
pub fn xray_install_command(os_release: &str) -> Option<&'static str> {
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
        return Some("nix profile install nixpkgs#xray");
    }
    if family("arch") {
        return Some("yay -S xray-bin");
    }
    None
}

/// Read this machine's `os-release` and answer for it.
pub fn xray_install_command_here() -> Option<&'static str> {
    // /usr/lib is the vendor copy; /etc wins when both exist, per os-release(5).
    let text = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok()?;
    xray_install_command(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_and_its_family_are_pointed_at_the_aur() {
        let arch = "NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\n";
        assert_eq!(xray_install_command(arch), Some("yay -S xray-bin"));

        // EndeavourOS names itself, and says what it is through ID_LIKE.
        let endeavour = "NAME=\"EndeavourOS\"\nID=endeavouros\nID_LIKE=\"arch\"\n";
        assert_eq!(xray_install_command(endeavour), Some("yay -S xray-bin"));
    }

    #[test]
    fn nixos_installs_from_nixpkgs() {
        let nixos = "NAME=NixOS\nID=nixos\nVERSION=\"25.11 (Xantusia)\"\n";
        assert_eq!(
            xray_install_command(nixos),
            Some("nix profile install nixpkgs#xray")
        );
    }

    /// Debian, Ubuntu and Fedora package no Xray core at all. Inventing an
    /// `apt install xray` that fails is worse than saying nothing, because the
    /// user would trust it over the documentation.
    #[test]
    fn a_distribution_that_packages_no_core_gets_no_command() {
        for text in [
            "ID=ubuntu\nID_LIKE=debian\n",
            "ID=debian\n",
            "ID=fedora\nID_LIKE=\"rhel centos\"\n",
            "",
            "malformed without any equals sign\n",
        ] {
            assert_eq!(xray_install_command(text), None, "{text:?}");
        }
    }

    /// Quoting in os-release is optional and inconsistent between distros.
    #[test]
    fn quoted_and_bare_values_read_the_same() {
        assert_eq!(
            xray_install_command("ID=\"arch\"\n"),
            xray_install_command("ID=arch\n")
        );
    }
}
