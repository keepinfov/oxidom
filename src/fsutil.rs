//! Shared filesystem helpers for every persister: atomic replace-on-write,
//! private permissions (the state files hold server credentials), and
//! quarantine of unparseable files so a corrupt read never turns into a
//! silent overwrite of the user's data on the next save.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Write `bytes` to `path` atomically (temp file + rename) with 0600
/// permissions, creating the parent directory as 0700 when missing.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    if !parent.exists() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }

    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        let _ = file.sync_all();
    }
    // Tighten a pre-existing temp file created before the mode applied.
    let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
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
