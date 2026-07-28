//! Atomic nftables ownership for per-profile cgroup marks.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::run::CgroupSlice;

pub mod resolve;

pub struct Nft {
    binary: String,
}

impl Nft {
    pub fn new(binary: String) -> Self {
        Self { binary }
    }

    pub fn install(&self, profile: &str, slice: &CgroupSlice, mark: u32) -> Result<()> {
        self.apply(&install_ruleset(profile, slice, mark)?)
    }

    pub fn remove(&self, profile: &str) -> Result<()> {
        self.apply(&remove_ruleset(profile)?)
    }

    fn apply(&self, ruleset: &str) -> Result<()> {
        let resolved = resolve::resolve(&self.binary)?;
        let mut child = Command::new(&resolved.path)
            .args(["-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning nft ({})", resolved.path.display()))?;
        child
            .stdin
            .take()
            .context("nft stdin was not piped")?
            .write_all(ruleset.as_bytes())
            .context("writing the atomic nft ruleset")?;
        let output = child
            .wait_with_output()
            .context("waiting for nft to apply the ruleset")?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else if !stdout.trim().is_empty() {
            stdout.trim()
        } else {
            "no diagnostic output"
        };
        bail!(
            "nft ({}) exited with {} while updating table inet oxidom: {detail}",
            resolved.path.display(),
            output.status
        )
    }
}

/// nft identifiers are bare words: quoting one is a syntax error, not extra
/// safety. `profile::valid_name` admits only `[a-z0-9_-]`, which is exactly
/// what the nft scanner accepts as a name, so the guard below is what lets the
/// result be interpolated unquoted.
fn chain_name(profile: &str) -> Result<String> {
    if !crate::profile::valid_name(profile) {
        bail!("invalid profile name {profile:?}");
    }
    Ok(format!("profile_{profile}"))
}

/// nft reads a quoted string literally: it has no escape sequences to undo.
/// Doubling a backslash therefore does not protect the value, it corrupts it —
/// a cgroup path containing systemd's `\x2d` became `\\x2d` and no longer named
/// any directory. A quote or a newline genuinely cannot be carried, so rather
/// than mangle one this refuses. `profile::valid_name` already makes that
/// unreachable for every value oxidom builds; the guard is what keeps it so.
fn quote(value: &str) -> Result<String> {
    if value.contains(['"', '\n']) {
        bail!("nft strings cannot carry quotes or newlines, refusing to emit {value:?}");
    }
    Ok(format!("\"{value}\""))
}

/// `add` is idempotent for an existing table/chain. Flushing only our
/// deterministic chain before re-adding its one rule makes retries and daemon
/// recovery safe while leaving every foreign nftables object untouched.
pub fn install_ruleset(profile: &str, slice: &CgroupSlice, mark: u32) -> Result<String> {
    let chain = chain_name(profile)?;
    Ok(format!(
        "add table inet oxidom\n\
         add chain inet oxidom {chain} {{ type route hook output priority mangle; policy accept; }}\n\
         flush chain inet oxidom {chain}\n\
         add rule inet oxidom {chain} socket cgroupv2 level {} {} meta mark set {mark:#x} comment {}\n",
        slice.level,
        quote(&slice.path)?,
        quote(&format!("oxidom profile {profile}"))?
    ))
}

/// Create-and-empty before `destroy` makes removal idempotent even after a
/// partial start or a previous cleanup. The table itself is intentionally
/// retained as oxidom's private container for other live profile chains.
pub fn remove_ruleset(profile: &str) -> Result<String> {
    let chain = chain_name(profile)?;
    Ok(format!(
        "add table inet oxidom\n\
         add chain inet oxidom {chain} {{ type route hook output priority mangle; policy accept; }}\n\
         flush chain inet oxidom {chain}\n\
         destroy chain inet oxidom {chain}\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_one_atomic_profile_rule_in_a_route_chain() {
        let slice = crate::run::user_slice("work", 1000).unwrap();
        let ruleset = install_ruleset("work", &slice, 0x6f21).unwrap();
        assert_eq!(
            ruleset,
            "add table inet oxidom\n\
             add chain inet oxidom profile_work { type route hook output priority mangle; policy accept; }\n\
             flush chain inet oxidom profile_work\n\
             add rule inet oxidom profile_work socket cgroupv2 level 4 \
             \"user.slice/user-1000.slice/user@1000.service/oxidom\\x2dwork.slice\" meta mark set \
             0x6f21 comment \"oxidom profile work\"\n"
        );
        assert_eq!(ruleset.matches("socket cgroupv2").count(), 1);
    }

    /// The cgroup path systemd creates carries `\x2d`, and nft resolves the
    /// string it is given with no unescaping at all. Doubling that backslash
    /// silently pointed the rule at a directory that does not exist, so every
    /// `oxidom run` command left the tunnel unmarked.
    #[test]
    fn a_backslash_reaches_nft_exactly_as_written() {
        assert_eq!(
            quote("oxidom\\x2dwork.slice").unwrap(),
            "\"oxidom\\x2dwork.slice\""
        );
    }

    #[test]
    fn unrepresentable_nft_strings_are_refused_rather_than_mangled() {
        assert!(quote("a\"b").is_err());
        assert!(quote("a\nb").is_err());
    }

    #[test]
    fn removal_only_destroys_the_profiles_own_chain() {
        let ruleset = remove_ruleset("work").unwrap();
        assert!(ruleset.contains("destroy chain inet oxidom profile_work"));
        assert!(!ruleset.contains("profile_home"));
    }

    /// A chain name is an nft identifier, and identifiers are bare words —
    /// quoting one is a syntax error that rejected the whole ruleset, so no
    /// profile could ever be marked. A dash needs no quoting to survive.
    #[test]
    fn chain_identifiers_are_never_quoted() {
        let slice = crate::run::user_slice("client-work", 1000).unwrap();
        let ruleset = install_ruleset("client-work", &slice, 0x6f21).unwrap();
        assert!(ruleset.contains("chain inet oxidom profile_client-work "));
        assert!(
            !ruleset.contains("\"profile_client-work\""),
            "chain identifiers must not be quoted: {ruleset}"
        );
    }

    #[test]
    fn invalid_profile_never_reaches_ruleset_text() {
        let slice = CgroupSlice {
            path: "user.slice/injected".to_string(),
            level: 2,
        };
        assert!(install_ruleset("bad name", &slice, 1).is_err());
        assert!(remove_ruleset("bad name").is_err());
    }
}
