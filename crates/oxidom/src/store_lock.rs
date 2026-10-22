use std::fs::{self, File};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::Context;

/// Keep the data directory exclusively owned until the daemon exits.
pub(super) struct StoreLock {
    _file: File,
}

impl StoreLock {
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
}
