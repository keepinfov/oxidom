//! App-facing orchestration API. The GUI (Phase 2) drives everything through
//! this type; it should not call the lower-level modules directly.

use std::collections::HashMap;

use anyhow::{Result, anyhow};

use crate::config::Config;
use crate::model::{Server, Subscription};
use crate::state::{self, State, store};
use crate::xray::core::{Status, XrayCore};
use crate::{alias, link, probe, subscription};

/// Fixed id of the local group that holds servers imported by share-link,
/// not tied to any subscription URL. It is a sentinel rather than a hash, so
/// the identity migration must leave it alone.
pub const LOCAL_ID: &str = "local";

pub struct Engine {
    pub config: Config,
    pub state: State,
    pub subscriptions: Vec<Subscription>,
    pub core: XrayCore,
    /// Non-fatal problems found while loading (e.g. a quarantined corrupt
    /// subscriptions file). The GUI surfaces these once at startup.
    pub load_warnings: Vec<String>,
}

impl Engine {
    pub fn load() -> Self {
        let config = Config::load();
        let core = XrayCore::new(
            config.socks_port,
            config.http_port,
            config.xray_binary.clone(),
        );
        let state = State::load();
        let (subscriptions, store_warning) = store::load();
        let mut engine = Engine {
            state,
            subscriptions,
            core,
            config,
            load_warnings: store_warning.into_iter().collect(),
        };
        engine.migrate_identities();
        engine.recover();
        engine
    }

    fn migrate_identities(&mut self) {
        let active_before = self.state.active_server_id.clone();
        let mut server_ids = HashMap::new();
        let mut seen_ids: HashMap<String, (String, String)> = HashMap::new();
        let mut identities_changed = false;
        let mut renamed_servers = 0usize;

        for subscription in &mut self.subscriptions {
            // The local group is keyed by a sentinel, not by its (empty) URL.
            // Rehashing it would orphan every share-link the user imported.
            if subscription.id != LOCAL_ID {
                let new_subscription_id = Server::stable_id(&subscription.url);
                if subscription.id != new_subscription_id {
                    subscription.id = new_subscription_id;
                    identities_changed = true;
                }
            }

            let mut migrated = Vec::with_capacity(subscription.servers.len());
            for mut server in std::mem::take(&mut subscription.servers) {
                let old_id = server.id.clone();
                let new_id = Server::stable_id(&server.identity_string());
                if old_id != new_id {
                    server_ids
                        .entry(old_id.clone())
                        .or_insert_with(|| new_id.clone());
                    server.id.clone_from(&new_id);
                    identities_changed = true;
                    renamed_servers += 1;
                }

                if let Some((first_old_id, first_name)) = seen_ids.get(&new_id)
                    && first_old_id != &old_id
                {
                    log::warn!(
                        "dropping server {:?}: its migrated id collides with earlier server {:?}",
                        server.name,
                        first_name
                    );
                    identities_changed = true;
                    continue;
                }
                seen_ids
                    .entry(new_id)
                    .or_insert_with(|| (old_id, server.name.clone()));
                migrated.push(server);
            }
            subscription.servers = migrated;
        }

        let aliases_complete = self
            .all_servers()
            .all(|server| server.alias.as_ref().is_some());
        if !identities_changed && aliases_complete {
            return;
        }

        if let Some(active_id) = self.state.active_server_id.as_mut()
            && let Some(new_id) = server_ids.get(active_id)
        {
            active_id.clone_from(new_id);
        }
        alias::assign(&mut self.subscriptions);

        if let Err(error) = store::save(&self.subscriptions) {
            log::warn!("could not persist migrated subscription identities: {error:#}");
        }
        if let Err(error) = self.state.save() {
            log::warn!("could not persist migrated active server identity: {error:#}");
        }

        let active_preserved = active_before.is_none()
            || self
                .state
                .active_server_id
                .as_deref()
                .is_some_and(|active_id| self.all_servers().any(|server| server.id == active_id));
        log::info!(
            "identity migration renamed {renamed_servers} servers; active server preserved: \
             {active_preserved}"
        );
    }

    /// Undo each resource a crashed previous instance could have left behind.
    fn recover(&mut self) {
        self.recover_stale_core();
        // Phase 4 adds the TUN device and the routes we added here.
    }

    fn recover_stale_core(&mut self) {
        if let Some(pid) = self.state.xray_pid.take() {
            if kill_stale_xray(pid) {
                log::info!("stopped orphaned xray process {pid} from a previous run");
            } else {
                log::warn!("could not confirm that orphaned xray process {pid} stopped");
            }
            if let Err(error) = self.state.save() {
                log::warn!("could not persist stale-core recovery state: {error:#}");
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        self.config.save()?;
        self.state.save()?;
        store::save(&self.subscriptions)?;
        Ok(())
    }

    /// Flat iterator over every known server across all subscriptions.
    pub fn all_servers(&self) -> impl Iterator<Item = &Server> {
        self.subscriptions.iter().flat_map(|s| s.servers.iter())
    }

    pub fn find_server(&self, id: &str) -> Option<Server> {
        self.all_servers().find(|s| s.id == id).cloned()
    }

    pub fn add_subscription(
        &mut self,
        url: String,
        name: Option<String>,
        send_hwid: bool,
    ) -> Result<()> {
        let mut sub = Subscription::new(url, name);
        sub.send_hwid = send_hwid;
        let hwid = if sub.send_hwid {
            state::hwid().ok()
        } else {
            None
        };
        subscription::refresh(
            &mut sub,
            &self.config.subscription_user_agent,
            hwid.as_deref(),
        )?;
        if let Some(existing) = self
            .subscriptions
            .iter_mut()
            .find(|existing| existing.id == sub.id)
        {
            *existing = sub;
        } else {
            self.subscriptions.push(sub);
        }
        alias::assign(&mut self.subscriptions);
        store::save(&self.subscriptions)?;
        Ok(())
    }

    pub fn refresh(&mut self, sub_id: &str) -> Result<()> {
        let ua = self.config.subscription_user_agent.clone();
        let sub = self
            .subscriptions
            .iter_mut()
            .find(|s| s.id == sub_id)
            .ok_or_else(|| anyhow!("subscription not found"))?;
        // Generate the device id only for a subscription that opted in: the
        // file is itself a per-install identifier, so an opt-out user must not
        // end up with one sitting on disk.
        let hwid = if sub.send_hwid {
            state::hwid().ok()
        } else {
            None
        };
        subscription::refresh(sub, &ua, hwid.as_deref())?;
        alias::assign(&mut self.subscriptions);
        self.disconnect_if_active_gone();
        store::save(&self.subscriptions)?;
        Ok(())
    }

    /// Refresh every URL-backed subscription (skips the local share-link group,
    /// which has an empty URL). Collects per-subscription errors and still saves
    /// whatever succeeded; returns an error summarizing any failures.
    pub fn refresh_all(&mut self) -> Result<()> {
        // Only touch the hwid file when something actually opted in; reading it
        // creates it. See the note in `refresh`.
        let hwid_val = self
            .subscriptions
            .iter()
            .any(|s| s.send_hwid && !s.url.is_empty())
            .then(|| state::hwid().ok())
            .flatten();
        let ua = self.config.subscription_user_agent.clone();
        let ids: Vec<String> = self
            .subscriptions
            .iter()
            .filter(|s| !s.url.is_empty())
            .map(|s| s.id.clone())
            .collect();
        let mut errors = Vec::new();
        for id in ids {
            if let Some(sub) = self.subscriptions.iter_mut().find(|s| s.id == id) {
                let hwid = if sub.send_hwid {
                    hwid_val.as_deref()
                } else {
                    None
                };
                if let Err(error) = subscription::refresh(sub, &ua, hwid) {
                    errors.push(format!("{}: {error:#}", sub.name));
                }
            }
        }
        alias::assign(&mut self.subscriptions);
        self.disconnect_if_active_gone();
        store::save(&self.subscriptions)?;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(errors.join("; ")))
        }
    }

    /// Remove a subscription. Returns true when the removal took the active
    /// server with it and the tunnel was therefore shut down.
    pub fn remove_subscription(&mut self, sub_id: &str) -> Result<bool> {
        let disconnected = self.disconnect_if_active_within(|server_id, subs| {
            subs.iter()
                .find(|s| s.id == sub_id)
                .is_some_and(|s| s.servers.iter().any(|server| server.id == server_id))
        });
        self.subscriptions.retain(|s| s.id != sub_id);
        store::save(&self.subscriptions)?;
        Ok(disconnected)
    }

    /// Import one or more share-links into the local "My servers" group.
    /// Returns how many new servers were added (duplicates are skipped) and
    /// how many lines used an unsupported scheme.
    pub fn import_links(&mut self, text: &str) -> Result<(usize, usize)> {
        let (parsed, unsupported) = link::parse_links_counting(text);
        if parsed.is_empty() {
            if unsupported > 0 {
                return Err(anyhow!(
                    "none of the links use a supported scheme ({})",
                    link::supported_scheme_list()
                ));
            }
            return Err(anyhow!("no valid share-links found"));
        }
        let idx = match self.subscriptions.iter().position(|s| s.id == LOCAL_ID) {
            Some(idx) => idx,
            None => {
                let mut sub = Subscription::new(String::new(), Some("My servers".to_string()));
                sub.id = LOCAL_ID.to_string();
                self.subscriptions.insert(0, sub);
                0
            }
        };
        let mut added = 0;
        for server in parsed {
            if !self.subscriptions[idx]
                .servers
                .iter()
                .any(|s| s.id == server.id)
            {
                self.subscriptions[idx].servers.push(server);
                added += 1;
            }
        }
        alias::assign(&mut self.subscriptions);
        store::save(&self.subscriptions)?;
        Ok((added, unsupported))
    }

    /// Remove a single server from the local group, dropping the group when it
    /// becomes empty. Only local servers are removable; subscription servers
    /// would just reappear on refresh. Returns true when the removed server
    /// was the active one and the tunnel was shut down.
    pub fn remove_server(&mut self, server_id: &str) -> Result<bool> {
        let disconnected = self.disconnect_if_active_within(|active_id, _| active_id == server_id);
        if let Some(sub) = self.subscriptions.iter_mut().find(|s| s.id == LOCAL_ID) {
            sub.servers.retain(|s| s.id != server_id);
        }
        self.subscriptions
            .retain(|s| !(s.id == LOCAL_ID && s.servers.is_empty()));
        store::save(&self.subscriptions)?;
        Ok(disconnected)
    }

    /// Disconnect when a refresh took the active server away with it — the
    /// panel rotated its credentials, renumbered it, or dropped it entirely.
    /// Same invariant as a deletion: the tunnel must not keep running through
    /// a server the user can no longer see, select or manage.
    fn disconnect_if_active_gone(&mut self) -> bool {
        let gone = self.disconnect_if_active_within(|active_id, subs| {
            !subs
                .iter()
                .any(|s| s.servers.iter().any(|server| server.id == active_id))
        });
        if gone {
            self.core
                .note("the active server is no longer in its subscription — disconnected");
        }
        gone
    }

    /// Disconnect when the tunnel is running and the active server matches
    /// `covers(active_id, &subscriptions)`. Never leave xray proxying through
    /// a server the user just deleted.
    fn disconnect_if_active_within(
        &mut self,
        covers: impl Fn(&str, &[Subscription]) -> bool,
    ) -> bool {
        // `Error` counts as active: a crashed core still leaves the server
        // recorded as the active one, and deleting it must clear that.
        let active = match (&self.state.active_server_id, self.core.status()) {
            (Some(id), Status::Connected | Status::Connecting | Status::Error(_)) => id.clone(),
            _ => return false,
        };
        if covers(&active, &self.subscriptions) {
            self.disconnect();
            true
        } else {
            false
        }
    }

    pub fn connect(&mut self, server_id: &str) -> Result<()> {
        let server = self
            .find_server(server_id)
            .ok_or_else(|| anyhow!("server not found"))?;
        self.core.connect(&server)?;
        self.state.active_server_id = Some(server_id.to_string());
        self.state.xray_pid = self.core.child_pid();
        if let Err(error) = self.state.save() {
            log::warn!("could not persist the active Xray process: {error:#}");
        }
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.core.disconnect();
        self.state.active_server_id = None;
        self.state.xray_pid = None;
        if let Err(error) = self.state.save() {
            log::warn!("could not persist the disconnected state: {error:#}");
        }
    }

    pub fn status(&self) -> Status {
        self.core.status()
    }

    /// Probe one server with the configured latency method, measured against
    /// that server rather than through the tunnel.
    pub fn probe(&self, server: &Server) -> probe::ProbeOutcome {
        probe::measure(server, &self.config, probe::Route::Direct)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Stop the child before the struct fields drop so the recovery flag
        // can be persisted as clean in the same pass.
        self.core.disconnect();
        if self.state.xray_pid.is_some() {
            self.state.xray_pid = None;
            if let Err(error) = self.state.save() {
                log::warn!("could not clear the Xray recovery PID on shutdown: {error:#}");
            }
        }
    }
}

/// Kill a leftover xray process from a previous run, but only after verifying
/// the PID still belongs to our core — PIDs get recycled.
fn kill_stale_xray(pid: u32) -> bool {
    if !is_our_xray(pid) {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        log::warn!("refusing to signal stale PID {pid}: it is not an oxidom Xray core");
        return false;
    }
    let Ok(raw_pid) = i32::try_from(pid) else {
        log::warn!("stale Xray PID {pid} is outside the platform PID range");
        return false;
    };
    let process = nix::unistd::Pid::from_raw(raw_pid);
    match nix::sys::signal::kill(process, nix::sys::signal::Signal::SIGTERM) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => return true,
        Err(error) => {
            log::warn!("could not send SIGTERM to stale Xray PID {pid}: {error}");
            return false;
        }
    }
    if wait_until_gone(process, std::time::Duration::from_secs(2)) {
        return true;
    }

    log::warn!("stale Xray PID {pid} ignored SIGTERM; sending SIGKILL");
    match nix::sys::signal::kill(process, nix::sys::signal::Signal::SIGKILL) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => return true,
        Err(error) => {
            log::warn!("could not send SIGKILL to stale Xray PID {pid}: {error}");
            return false;
        }
    }
    wait_until_gone(process, std::time::Duration::from_secs(2))
}

fn wait_until_gone(pid: nix::unistd::Pid, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match nix::sys::signal::kill(pid, None) {
            Err(nix::errno::Errno::ESRCH) => return true,
            Err(error) => {
                log::warn!("could not inspect stale Xray PID {pid}: {error}");
                return false;
            }
            Ok(()) if std::time::Instant::now() >= deadline => return false,
            Ok(()) => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
}

/// Does this PID belong to a core oxidom started?
///
/// The binary name is user-configurable (`xray_binary`, `$OXIDOM_XRAY_BIN`), so
/// insisting on `comm == "xray"` would skip a core installed as, say,
/// `xray-linux-amd64` and leave its tunnel up with no way to stop it. The
/// generated config path is the reliable marker: nothing else is run against
/// it. The name check stays as a fallback for when the data dir has moved.
fn is_our_xray(pid: u32) -> bool {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let cmdline = String::from_utf8_lossy(&raw);
    if let Ok(config) = XrayCore::config_path()
        && cmdline
            .split('\0')
            .any(|arg| !arg.is_empty() && std::path::Path::new(arg) == config)
    {
        return true;
    }
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|comm| comm.trim().starts_with("xray"))
}

#[cfg(test)]
mod tests {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::{Context, Result, anyhow};

    use super::{Engine, LOCAL_ID};
    use crate::link::parse_link;
    use crate::model::{Server, Subscription};
    use crate::state::{State, store};

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TestRoot {
        path: std::path::PathBuf,
    }

    impl TestRoot {
        fn install(label: &str) -> Result<Self> {
            let suffix = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oxidom-core-test-{label}-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path)
                .with_context(|| format!("creating test root {}", path.display()))?;
            crate::paths::set_test_root(Some(path.clone()));
            Ok(Self { path })
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            crate::paths::set_test_root(None);
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn old_stable_id(seed: &str) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        seed.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    fn old_subscription(url: &str, links: &[&str]) -> Subscription {
        let mut subscription = Subscription::new(url.to_string(), Some("Test".to_string()));
        subscription.id = old_stable_id(url);
        subscription.servers = links
            .iter()
            .map(|link| {
                let mut server = parse_link(link).unwrap();
                server.id = old_stable_id(&server.identity_string());
                server.alias = None;
                server
            })
            .collect();
        subscription
    }

    #[test]
    fn migration_leaves_the_local_group_keyed_by_its_sentinel() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("identity-local-group")?;
        let mut local = old_subscription("", &["trojan://one@one.example:443#Imported"]);
        local.id = LOCAL_ID.to_string();
        store::save(&[local])?;
        State::default().save()?;

        let engine = Engine::load();

        let group = engine
            .subscriptions
            .first()
            .ok_or_else(|| anyhow!("the local group disappeared"))?;
        assert_eq!(group.id, LOCAL_ID);
        assert_eq!(group.servers.len(), 1);
        Ok(())
    }

    #[test]
    fn migration_preserves_the_active_server_and_server_count() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("identity-migration")?;
        let subscription = old_subscription(
            "https://subscription.example/token",
            &[
                "trojan://one@one.example:443#One",
                "vless://two@two.example:443#Two",
            ],
        );
        let old_active = subscription.servers[1].id.clone();
        let expected_active = Server::stable_id(&subscription.servers[1].identity_string());
        let server_count = subscription.servers.len();
        store::save(&[subscription])?;
        State {
            active_server_id: Some(old_active),
            active_profile: None,
            xray_pid: None,
        }
        .save()?;

        let engine = Engine::load();

        assert_eq!(engine.all_servers().count(), server_count);
        assert_eq!(
            engine.state.active_server_id.as_deref(),
            Some(expected_active.as_str())
        );
        assert!(
            engine
                .all_servers()
                .all(|server| server.alias.as_ref().is_some_and(|alias| !alias.is_empty()))
        );
        Ok(())
    }

    #[test]
    fn completed_migration_does_not_rewrite_the_store() -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("identity-no-rewrite")?;
        let mut subscription = old_subscription(
            "https://subscription.example/stable",
            &["trojan://one@one.example:443#One"],
        );
        subscription.id = Server::stable_id(&subscription.url);
        subscription.servers[0].id = Server::stable_id(&subscription.servers[0].identity_string());
        subscription.servers[0].alias = Some("one".to_string());
        store::save(&[subscription])?;
        State::default().save()?;
        let subscriptions_path = crate::paths::subscriptions_file()?;
        let state_path = crate::paths::state_file()?;
        let subscriptions_inode = std::fs::metadata(&subscriptions_path)?.ino();
        let state_inode = std::fs::metadata(&state_path)?.ino();

        let _engine = Engine::load();

        assert_eq!(
            std::fs::metadata(&subscriptions_path)?.ino(),
            subscriptions_inode
        );
        assert_eq!(std::fs::metadata(&state_path)?.ino(), state_inode);
        Ok(())
    }

    #[test]
    fn migration_keeps_the_first_server_when_old_ids_collapse() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("identity-collision")?;
        let mut subscription = old_subscription(
            "https://subscription.example/collision",
            &["trojan://one@one.example:443#First"],
        );
        let mut duplicate = subscription.servers[0].clone();
        duplicate.id = "different-old-id".to_string();
        duplicate.name = "Second".to_string();
        subscription.servers.push(duplicate);
        store::save(&[subscription])?;
        State {
            active_server_id: Some("different-old-id".to_string()),
            active_profile: None,
            xray_pid: None,
        }
        .save()?;

        let engine = Engine::load();

        assert_eq!(engine.all_servers().count(), 1);
        assert_eq!(
            engine
                .all_servers()
                .next()
                .map(|server| server.name.as_str()),
            Some("First")
        );
        assert_eq!(
            engine.state.active_server_id,
            engine.all_servers().next().map(|server| server.id.clone())
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires OXIDOM_TEST_SUBSCRIPTIONS pointing at a real cache"]
    fn migrates_a_real_subscription_cache_without_losing_state() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let source = std::env::var_os("OXIDOM_TEST_SUBSCRIPTIONS")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow!("OXIDOM_TEST_SUBSCRIPTIONS is not set"))?;
        let subscriptions: Vec<Subscription> = serde_json::from_str(
            &std::fs::read_to_string(&source)
                .with_context(|| format!("reading {}", source.display()))?,
        )
        .with_context(|| format!("parsing {}", source.display()))?;
        let state_path = source
            .parent()
            .ok_or_else(|| anyhow!("subscription path has no parent"))?
            .join("state.toml");
        let state: State = toml::from_str(
            &std::fs::read_to_string(&state_path)
                .with_context(|| format!("reading {}", state_path.display()))?,
        )
        .with_context(|| format!("parsing {}", state_path.display()))?;
        let active_before = state
            .active_server_id
            .clone()
            .ok_or_else(|| anyhow!("the real state has no active server"))?;
        if !subscriptions
            .iter()
            .flat_map(|subscription| subscription.servers.iter())
            .any(|server| server.id == active_before)
        {
            return Err(anyhow!("the real active server is absent before migration"));
        }
        let server_count = subscriptions
            .iter()
            .map(|subscription| subscription.servers.len())
            .sum::<usize>();

        let _root = TestRoot::install("real-identity-migration")?;
        store::save(&subscriptions)?;
        state.save()?;
        let engine = Engine::load();

        assert_eq!(engine.all_servers().count(), server_count);
        let active_after = engine
            .state
            .active_server_id
            .as_deref()
            .ok_or_else(|| anyhow!("migration cleared the active server"))?;
        assert!(
            engine.all_servers().any(|server| server.id == active_after),
            "migrated active server is absent"
        );
        Ok(())
    }
}
