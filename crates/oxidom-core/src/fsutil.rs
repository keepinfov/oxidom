//! Shared filesystem helpers for every persister: atomic replace-on-write,
//! private permissions (the state files hold server credentials), and
//! quarantine of unparseable files so a corrupt read never turns into a
//! silent overwrite of the user's data on the next save.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Persist each new directory entry as well as the eventual file replacement.
pub fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if path.is_dir() {
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_private_dir(parent)?;
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("creating directory {}", path.display()));
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    for directory in [path, parent] {
        fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("syncing directory {}", directory.display()))?;
    }
    Ok(())
}

/// Write `bytes` to `path` atomically (temp file + rename) with 0600
/// permissions, creating the parent directory as 0700 when missing.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    create_private_dir(parent)?;
    let directory = fs::File::open(parent)
        .with_context(|| format!("opening directory {}", parent.display()))?;

    let (tmp, mut file) = loop {
        let tmp = parent.join(format!(
            ".oxidom-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => break (tmp, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating a temporary file for {}", path.display()));
            }
        }
    };
    let result: Result<()> = (|| {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting temporary permissions for {}", path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        #[cfg(test)]
        let sync_result = tests::before_sync(&tmp).and_then(|()| file.sync_all());
        #[cfg(not(test))]
        let sync_result = file.sync_all();
        sync_result.with_context(|| format!("syncing {}", path.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    directory
        .sync_all()
        .with_context(|| format!("syncing directory {}", parent.display()))?;
    Ok(())
}

/// Move an unparseable file aside as `<name>.corrupt-<unix-ts>` and return the
/// new path. The caller then falls back to defaults without destroying data
/// the user may want to inspect or recover.
pub fn quarantine(path: &Path) -> Option<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let file_name = path.file_name()?.to_string_lossy().into_owned();
    let target = path.with_file_name(format!("{file_name}.corrupt-{timestamp}"));
    fs::rename(path, &target).ok()?;
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::{quarantine, write_private_atomic};
    use std::cell::RefCell;
    use std::fs;
    use std::io;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    type SyncHook = Box<dyn Fn(&Path) -> io::Result<()>>;
    thread_local! {
        static BEFORE_SYNC: RefCell<Option<SyncHook>> = RefCell::new(None);
    }

    pub(super) fn before_sync(path: &Path) -> io::Result<()> {
        BEFORE_SYNC.with_borrow(|hook| hook.as_ref().map_or(Ok(()), |hook| hook(path)))
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "oxidom-fsutil-test-{}-{}",
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
    fn a_failed_file_sync_preserves_the_destination_and_cleans_its_temp() {
        let dir = TestDir::new();
        let path = dir.0.join("state.toml");
        fs::write(&path, b"original state").unwrap();
        BEFORE_SYNC.with_borrow_mut(|hook| {
            *hook = Some(Box::new(|_| Err(io::Error::other("injected sync failure"))));
        });
        let result = write_private_atomic(&path, b"replacement state");
        BEFORE_SYNC.with_borrow_mut(|hook| *hook = None);
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original state");
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_writers_publish_complete_files_without_temp_collisions() {
        let dir = TestDir::new();
        let path = dir.0.join("state.toml");
        let barrier = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let writers: Vec<_> = [b"first state".as_slice(), b"second state".as_slice()]
                .into_iter()
                .map(|bytes| {
                    let path = &path;
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        BEFORE_SYNC.with_borrow_mut(|hook| {
                            *hook = Some(Box::new(move |_| {
                                barrier.wait();
                                Ok(())
                            }));
                        });
                        write_private_atomic(path, bytes)
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap().unwrap();
            }
        });
        let published = fs::read(&path).unwrap();
        assert!(published == b"first state" || published == b"second state");
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn a_stale_legacy_temp_is_neither_truncated_nor_removed() {
        let dir = TestDir::new();
        let path = dir.0.join("state.toml");
        let legacy = path.with_extension("tmp");
        fs::write(&legacy, b"stale temp sentinel").unwrap();
        write_private_atomic(&path, b"new state").unwrap();
        assert_eq!(fs::read(&legacy).unwrap(), b"stale temp sentinel");
        assert_eq!(fs::read(&path).unwrap(), b"new state");
    }

    #[test]
    fn a_legacy_temp_symlink_is_not_followed_or_removed() {
        let dir = TestDir::new();
        let path = dir.0.join("state.toml");
        let target = dir.0.join("sentinel");
        fs::write(&target, b"unrelated sentinel").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let legacy = path.with_extension("tmp");
        symlink(&target, &legacy).unwrap();
        write_private_atomic(&path, b"new state").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"unrelated sentinel");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read_link(&legacy).unwrap(), target);
        assert!(!fs::symlink_metadata(&path).unwrap().is_symlink());
        assert_eq!(fs::read(&path).unwrap(), b"new state");
    }

    #[test]
    fn a_failed_rename_cleans_only_the_writers_own_temp() {
        let dir = TestDir::new();
        let path = dir.0.join("state.toml");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("sentinel"), b"preserved").unwrap();
        let unrelated = dir.0.join("other.tmp");
        fs::write(&unrelated, b"unrelated temp").unwrap();
        assert!(write_private_atomic(&path, b"new state").is_err());
        assert_eq!(fs::read(path.join("sentinel")).unwrap(), b"preserved");
        assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated temp");
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
    }

    #[test]
    fn new_directories_and_in_progress_and_published_files_are_private() {
        let dir = TestDir::new();
        let parent = dir.0.join("nested").join("private");
        let path = parent.join("state.toml");
        BEFORE_SYNC.with_borrow_mut(|hook| {
            *hook = Some(Box::new(|temp| {
                assert_eq!(fs::metadata(temp)?.permissions().mode() & 0o777, 0o600);
                Ok(())
            }));
        });
        let result = write_private_atomic(&path, b"private state");
        BEFORE_SYNC.with_borrow_mut(|hook| *hook = None);
        result.unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(parent.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private_atomic(&path, b"replacement state").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn atomic_write_creates_parent_and_sets_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("oxidom-fsutil-{}", std::process::id()));
        let path = dir.join("nested").join("secrets.json");
        write_private_atomic(&path, b"first").unwrap();
        write_private_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn quarantine_renames_the_corrupt_file_out_of_the_way() {
        let dir = std::env::temp_dir().join(format!("oxidom-quarantine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.toml");
        std::fs::write(&path, b"not toml {{{").unwrap();
        let moved = quarantine(&path).unwrap();
        assert!(!path.exists());
        assert!(moved.exists());
        assert!(
            moved
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("state.toml.corrupt-")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
