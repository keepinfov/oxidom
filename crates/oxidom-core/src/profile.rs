//! Named connection profiles stored below the daemon's config directory.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::engine::Engine;
use crate::{fsutil, paths};

const MAX_NAME_LEN: usize = 32;

/// Top-level CLI words cannot also be profile-first profile names: the
/// normalizer deliberately gives a verb in the first position precedence.
pub const RESERVED_NAMES: &[&str] = &[
    "up",
    "connect-profile",
    "down",
    "disconnect",
    "status",
    "ip",
    "list",
    "ping",
    "alias",
    "profile",
    "connect",
    "daemon",
    "gui",
    "run",
    "env",
    "tun",
];

static WARNED_RESERVED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Profile {
    pub description: String,
    pub select: ProfileSelect,
    pub proxy: ProfileProxy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProfileSelect {
    pub server: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileProxy {
    pub socks_port: u16,
    pub http_port: u16,
}

impl Default for ProfileProxy {
    fn default() -> Self {
        let config = Config::default();
        ProfileProxy {
            socks_port: config.socks_port,
            http_port: config.http_port,
        }
    }
}

impl Profile {
    pub fn validate(&self) -> Result<()> {
        if self.proxy.socks_port == 0 || self.proxy.http_port == 0 {
            bail!("profile ports must be between 1 and 65535");
        }
        if self.proxy.socks_port == self.proxy.http_port {
            bail!("the profile's SOCKS and HTTP inbounds cannot share a port");
        }
        Ok(())
    }

    pub fn from_toml(body: &str) -> Result<Self> {
        toml::from_str(body).context("parsing profile")
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serializing profile")
    }
}

/// Parse an editor setting without involving a shell. Editor variables often
/// include flags (`code --wait`), while executing the string through a shell
/// would turn a local preference into an avoidable command-injection surface.
pub fn editor_command(raw: &str) -> Result<Vec<String>> {
    let arguments = shell_words::split(raw).context("parsing the editor command")?;
    if arguments.is_empty() {
        bail!("the editor command is empty");
    }
    Ok(arguments)
}

/// Profile names are also systemd instance names, so accepting path syntax or
/// case variants here would make the on-disk and unit identities disagree.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=MAX_NAME_LEN).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

pub fn is_reserved(name: &str) -> bool {
    RESERVED_NAMES.contains(&name)
}

fn warn_reserved(name: &str) {
    let warned = WARNED_RESERVED.get_or_init(|| Mutex::new(HashSet::new()));
    if crate::sync::lock(warned).insert(name.to_string()) {
        log::warn!(
            "profile {name:?} uses a reserved CLI command name; it remains readable but cannot \
             be saved under that name"
        );
    }
}

pub fn list() -> Result<Vec<String>> {
    let directory = paths::profiles_dir()?;
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", directory.display()));
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("inspecting {}", entry.path().display()))?
            .is_file()
        {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if valid_name(name) {
            if is_reserved(name) {
                warn_reserved(name);
            }
            names.push(name.to_string());
        }
    }
    names.sort_unstable();
    Ok(names)
}

pub fn load(name: &str) -> Result<Profile> {
    let path = profile_path(name)?;
    if is_reserved(name) {
        warn_reserved(name);
    }
    let body = fs::read_to_string(&path)
        .with_context(|| format!("reading profile {name:?} from {}", path.display()))?;
    Profile::from_toml(&body).with_context(|| format!("in profile {name:?}"))
}

pub fn save(name: &str, profile: &Profile) -> Result<()> {
    if is_reserved(name) {
        bail!("profile name {name:?} is reserved by the oxidom CLI");
    }
    let path = profile_path(name)?;
    profile.validate()?;
    let body = profile.to_toml()?;
    fsutil::write_private_atomic(&path, body.as_bytes())
        .with_context(|| format!("writing profile {name:?}"))
}

pub fn remove(name: &str) -> Result<bool> {
    let path = profile_path(name)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("removing profile {name:?} from {}", path.display()))
        }
    }
}

/// Seed one useful profile on first start, but never treat startup as an
/// opportunity to rewrite a profile the user has already edited.
pub fn ensure_default(engine: &Engine) -> Result<()> {
    if !list()?.is_empty() {
        return Ok(());
    }
    let server = engine
        .active_server_id()
        .as_deref()
        .and_then(|active_id| engine.all_servers().find(|server| server.id == active_id))
        .and_then(|server| server.alias.clone())
        .unwrap_or_default();
    let profile = Profile {
        select: ProfileSelect { server },
        proxy: ProfileProxy {
            socks_port: engine.registry.config.socks_port,
            http_port: engine.registry.config.http_port,
        },
        ..Profile::default()
    };
    save("default", &profile)
}

fn profile_path(name: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!(
            "profile name must be 1-32 lowercase letters, digits, underscores, or hyphens, \
             and start with a letter or digit"
        );
    }
    Ok(paths::profiles_dir()?.join(format!("{name}.toml")))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::Result;

    use super::{Profile, is_reserved, list, valid_name};

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TestRoot {
        path: std::path::PathBuf,
    }

    impl TestRoot {
        fn install(label: &str) -> Self {
            let suffix = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oxidom-profile-test-{label}-{}-{suffix}",
                std::process::id()
            ));
            crate::paths::set_test_root(Some(path.clone()));
            Self { path }
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            crate::paths::set_test_root(None);
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn profile_toml_round_trip_ignores_unknown_keys() -> Result<()> {
        let input = r#"
description = "work"
future_key = "ignored"

[select]
server = "ch-trojan"
pool = "future"

[proxy]
socks_port = 12080
http_port = 12081
"#;
        let profile: Profile = toml::from_str(input)?;
        assert_eq!(profile.description, "work");
        assert_eq!(profile.select.server, "ch-trojan");
        assert_eq!(profile.proxy.socks_port, 12080);
        assert_eq!(profile.proxy.http_port, 12081);

        let encoded = toml::to_string_pretty(&profile)?;
        assert_eq!(toml::from_str::<Profile>(&encoded)?, profile);
        Ok(())
    }

    #[test]
    fn profile_names_are_portable_unit_names() {
        for accepted in ["default", "work-2", "a"] {
            assert!(valid_name(accepted), "{accepted:?}");
        }
        for rejected in [
            "../evil",
            "/etc/passwd",
            "Work",
            "",
            "a23456789012345678901234567890123",
        ] {
            assert!(!valid_name(rejected), "{rejected:?}");
        }
    }

    #[test]
    fn command_names_are_reserved_for_argv_normalization() {
        for reserved in super::RESERVED_NAMES {
            assert!(is_reserved(reserved), "{reserved:?}");
        }
        for allowed in ["default", "work", "home-2"] {
            assert!(!is_reserved(allowed), "{allowed:?}");
        }
    }

    #[test]
    fn reserved_legacy_profiles_stay_visible_and_readable() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("reserved-readable");
        let path = crate::paths::profiles_dir()?.join("status.toml");
        let profile = Profile::default();
        crate::fsutil::write_private_atomic(&path, profile.to_toml()?.as_bytes())?;

        assert_eq!(list()?, vec!["status".to_string()]);
        assert_eq!(super::load("status")?, profile);
        assert!(super::save("status", &profile).is_err());
        Ok(())
    }

    #[test]
    fn editor_settings_accept_flags_and_quoted_arguments() -> Result<()> {
        assert_eq!(
            super::editor_command(r#"code --wait "--profile=Work Tree""#)?,
            ["code", "--wait", "--profile=Work Tree"]
        );
        Ok(())
    }

    #[test]
    fn a_missing_profiles_directory_is_an_empty_list() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("missing-list");

        assert!(list()?.is_empty());
        Ok(())
    }

    #[test]
    fn the_default_profile_is_seeded_once_and_never_overwritten() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("ensure-default");
        let engine = crate::engine::Engine::load();

        super::ensure_default(&engine)?;
        assert_eq!(list()?, vec!["default".to_string()]);

        // The user's edits have to survive every later daemon start, so a
        // second call must not reach the file at all.
        let mut edited = super::load("default")?;
        edited.description = "mine".to_string();
        edited.proxy.socks_port = 21080;
        super::save("default", &edited)?;
        super::ensure_default(&engine)?;

        assert_eq!(list()?, vec!["default".to_string()]);
        assert_eq!(super::load("default")?, edited);
        Ok(())
    }
}
