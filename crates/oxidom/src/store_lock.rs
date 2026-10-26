use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::Context;

/// Keep the data directory exclusively owned until the daemon exits.
pub(super) struct StoreLock {
    _file: File,
}

impl StoreLock {
    /// Only the lease owner initializes identity, before any recovery side effects.
    pub(super) fn daemon_info(
        &self,
        data_dir: &Path,
        config_dir: &Path,
    ) -> anyhow::Result<oxidom_core::ipc::DaemonInfo> {
        use ring::digest::{Context as Digest, SHA256};

        oxidom_core::fsutil::create_private_dir(config_dir)?;
        let path = data_dir.join("store.identity");
        let token = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(mut file) => {
                let metadata = file.metadata()?;
                anyhow::ensure!(
                    metadata.is_file()
                        && metadata.uid() == nix::unistd::Uid::effective().as_raw()
                        && metadata.nlink() == 1
                        && metadata.mode() & 0o7777 == 0o600
                        && metadata.len() == 65,
                    "store identity must be a private regular file"
                );
                let mut bytes = Vec::new();
                (&mut file).take(66).read_to_end(&mut bytes)?;
                anyhow::ensure!(
                    bytes.len() == 65
                        && bytes[64] == b'\n'
                        && bytes[..64]
                            .iter()
                            .all(|byte| { byte.is_ascii_digit() || (b'a'..=b'f').contains(byte) }),
                    "invalid store identity; refusing to replace it"
                );
                file.sync_all()?;
                File::open(data_dir)?.sync_all()?;
                bytes
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::ensure!(
                    matches!(fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                    "store identity is not an absent regular file"
                );
                let bytes = format!("{}\n", random_token()?).into_bytes();
                oxidom_core::fsutil::write_private_atomic(&path, &bytes)?;
                bytes
            }
            Err(error) => return Err(error).context("opening store identity"),
        };
        let mut digest = Digest::new(&SHA256);
        digest.update(b"oxidom-store-v1\0");
        digest.update(&token);
        // Include both canonical roots: pairing the same data with different
        // settings/profiles must not authorize transparent client recovery.
        for root in [data_dir, config_dir] {
            let root = root.canonicalize()?;
            let bytes = root.as_os_str().as_bytes();
            digest.update(&(bytes.len() as u64).to_be_bytes());
            digest.update(bytes);
        }
        Ok(oxidom_core::ipc::DaemonInfo {
            version: 1,
            incarnation: random_token()?,
            store_id: hex(digest.finish().as_ref()),
            capabilities: vec!["probe_cancel".into(), "geo_download".into()],
        })
    }

    pub(super) fn acquire(data_dir: &Path) -> anyhow::Result<Self> {
        oxidom_core::fsutil::create_private_dir(data_dir)?;
        let lock_path = data_dir.join("store.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&lock_path)
            .with_context(|| format!("opening store lock {}", lock_path.display()))?;
        file.try_lock().with_context(|| {
            format!(
                "locking {}: another daemon may already own the data directory",
                lock_path.display()
            )
        })?;
        fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", data_dir.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", lock_path.display()))?;
        // Never unlink this file: waiters must always lock the same inode.
        Ok(Self { _file: file })
    }
}

fn random_token() -> anyhow::Result<String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("could not generate daemon identity"))?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::StoreLock;
    use std::fs;
    use std::io;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "oxidom-store-lock-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("creating test directory: {error}"),
                }
            }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn independent_acquisitions_conflict_until_the_owner_is_dropped() {
        let dir = TestDir::new();
        let owner = StoreLock::acquire(&dir.0).unwrap();
        assert!(StoreLock::acquire(&dir.0).is_err());
        drop(owner);
        let next_owner = StoreLock::acquire(&dir.0).unwrap();
        assert!(StoreLock::acquire(&dir.0).is_err());
        drop(next_owner);
    }

    #[test]
    fn rejected_acquisition_preserves_resource_and_lock_sentinels() {
        let dir = TestDir::new();
        let resource = dir.0.join("state.toml");
        fs::write(&resource, b"resource sentinel").unwrap();
        let owner = StoreLock::acquire(&dir.0).unwrap();
        let lock_path = dir.0.join("store.lock");
        fs::write(&lock_path, b"lock sentinel").unwrap();
        let inode = fs::metadata(&lock_path).unwrap().ino();
        assert!(StoreLock::acquire(&dir.0).is_err());
        assert_eq!(fs::read(&resource).unwrap(), b"resource sentinel");
        assert_eq!(fs::read(&lock_path).unwrap(), b"lock sentinel");
        assert_eq!(fs::metadata(&lock_path).unwrap().ino(), inode);
        drop(owner);
        assert_eq!(fs::metadata(&lock_path).unwrap().ino(), inode);
        let _next_owner = StoreLock::acquire(&dir.0).unwrap();
        assert_eq!(fs::metadata(&lock_path).unwrap().ino(), inode);
        assert_eq!(fs::read(&lock_path).unwrap(), b"lock sentinel");
    }

    #[test]
    fn the_data_directory_and_lock_file_are_private() {
        let dir = TestDir::new();
        let data_dir = dir.0.join("nested").join("data");
        let lock_path = data_dir.join("store.lock");
        let owner = StoreLock::acquire(&data_dir).unwrap();
        assert_eq!(
            fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(data_dir.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(owner);
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();
        let _owner = StoreLock::acquire(&data_dir).unwrap();
        assert_eq!(
            fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn store_continuity_survives_restarts_but_not_changed_root_pairing() {
        let dir = TestDir::new();
        let data = dir.0.join("data");
        let config = dir.0.join("config");
        let owner = StoreLock::acquire(&data).unwrap();
        let first = owner.daemon_info(&data, &config).unwrap();
        first.validate().unwrap();
        assert_eq!(
            fs::metadata(data.join("store.identity"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(owner);
        let owner = StoreLock::acquire(&data).unwrap();
        let next = owner.daemon_info(&data, &config).unwrap();
        assert_eq!(first.store_id, next.store_id);
        assert_ne!(first.incarnation, next.incarnation);
        let changed = owner
            .daemon_info(&data, &dir.0.join("other-config"))
            .unwrap();
        assert_ne!(first.store_id, changed.store_id);
        let other_data = dir.0.join("other-data");
        let other = StoreLock::acquire(&other_data).unwrap();
        assert_ne!(
            first.store_id,
            other.daemon_info(&other_data, &config).unwrap().store_id
        );
    }

    #[test]
    fn store_identity_hash_pins_literal_seed_root_order_and_canonical_aliases() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;
        let dir = TestDir::new();
        let data = dir.0.join("data");
        let config = dir.0.join("config");
        let owner = StoreLock::acquire(&data).unwrap();
        let seed = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
        oxidom_core::fsutil::write_private_atomic(&data.join("store.identity"), seed).unwrap();
        let info = owner.daemon_info(&data, &config).unwrap();
        let mut literal = b"oxidom-store-v1\0".to_vec();
        literal.extend_from_slice(seed);
        for root in [&data, &config] {
            let root = root.canonicalize().unwrap();
            let bytes = root.as_os_str().as_bytes();
            literal.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            literal.extend_from_slice(bytes);
        }
        assert_eq!(
            info.store_id,
            super::hex(ring::digest::digest(&ring::digest::SHA256, &literal).as_ref())
        );
        let data_alias = dir.0.join("data-alias");
        let config_alias = dir.0.join("config-alias");
        symlink(&data, &data_alias).unwrap();
        symlink(&config, &config_alias).unwrap();
        assert_eq!(
            info.store_id,
            owner
                .daemon_info(&data_alias, &config_alias)
                .unwrap()
                .store_id
        );
        let copied = dir.0.join("copied");
        let copied_owner = StoreLock::acquire(&copied).unwrap();
        oxidom_core::fsutil::write_private_atomic(&copied.join("store.identity"), seed).unwrap();
        assert_ne!(
            info.store_id,
            copied_owner.daemon_info(&copied, &config).unwrap().store_id
        );
    }

    #[test]
    fn malformed_or_unsafe_identity_is_never_regenerated() {
        use std::os::unix::fs::symlink;
        let dir = TestDir::new();
        let owner = StoreLock::acquire(&dir.0).unwrap();
        let path = dir.0.join("store.identity");
        fs::write(&path, b"broken\n").unwrap();
        assert!(owner.daemon_info(&dir.0, &dir.0).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"broken\n");
        fs::remove_file(&path).unwrap();
        let target = dir.0.join("sentinel");
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, &path).unwrap();
        assert!(owner.daemon_info(&dir.0, &dir.0).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        fs::remove_file(&path).unwrap();
        let info = owner.daemon_info(&dir.0, &dir.0).unwrap();
        fs::hard_link(&path, dir.0.join("alias")).unwrap();
        assert!(owner.daemon_info(&dir.0, &dir.0).is_err());
        fs::remove_file(dir.0.join("alias")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(owner.daemon_info(&dir.0, &dir.0).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            info.store_id,
            owner.daemon_info(&dir.0, &dir.0).unwrap().store_id
        );
    }
}
