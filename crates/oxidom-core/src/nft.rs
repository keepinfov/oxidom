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

fn chain_name(profile: &str) -> Result<String> {
    if !crate::profile::valid_name(profile) {
        bail!("invalid profile name {profile:?}");
    }
    Ok(format!("profile_{profile}"))
}

fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// `add` is idempotent for an existing table/chain. Flushing only our
/// deterministic chain before re-adding its one rule makes retries and daemon
/// recovery safe while leaving every foreign nftables object untouched.
pub fn install_ruleset(profile: &str, slice: &CgroupSlice, mark: u32) -> Result<String> {
    let chain = quote(&chain_name(profile)?);
    Ok(format!(
        "add table inet oxidom\n\
         add chain inet oxidom {chain} {{ type route hook output priority mangle; policy accept; }}\n\
         flush chain inet oxidom {chain}\n\
         add rule inet oxidom {chain} socket cgroupv2 level {} {} meta mark set {mark:#x} comment {}\n",
        slice.level,
        quote(&slice.path),
        quote(&format!("oxidom profile {profile}"))
    ))
}

/// Create-and-empty before `destroy` makes removal idempotent even after a
/// partial start or a previous cleanup. The table itself is intentionally
/// retained as oxidom's private container for other live profile chains.
pub fn remove_ruleset(profile: &str) -> Result<String> {
    let chain = quote(&chain_name(profile)?);
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
             add chain inet oxidom \"profile_work\" { type route hook output priority mangle; policy accept; }\n\
             flush chain inet oxidom \"profile_work\"\n\
             add rule inet oxidom \"profile_work\" socket cgroupv2 level 4 \
             \"user.slice/user-1000.slice/user@1000.service/oxidom-work.slice\" meta mark set \
             0x6f21 comment \"oxidom profile work\"\n"
        );
        assert_eq!(ruleset.matches("socket cgroupv2").count(), 1);
    }

    #[test]
    fn quoted_nft_strings_cannot_break_out() {
        assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn removal_only_destroys_the_profiles_own_chain() {
        let ruleset = remove_ruleset("work").unwrap();
        assert!(ruleset.contains("destroy chain inet oxidom \"profile_work\""));
        assert!(!ruleset.contains("profile_home"));
    }

    #[test]
    fn hyphens_in_profile_names_stay_inside_a_quoted_chain_identifier() {
        let slice = crate::run::user_slice("client-work", 1000).unwrap();
        let ruleset = install_ruleset("client-work", &slice, 0x6f21).unwrap();
        assert!(ruleset.contains("chain inet oxidom \"profile_client-work\""));
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
