//! Atomic file replacement for the on-disk stores (#470).
//!
//! `identity::save_store` and `network::save_store` both wrote their store with
//! a direct `std::fs::write(path, json)`. That is not atomic: a crash, power
//! loss, or disk-full error partway through the write leaves the file truncated
//! or corrupted, and the next `load_store` then fails to parse — losing every
//! identity or network the user had stored, with no way to recover it.
//!
//! The fix is the standard one: write the new contents to a temporary file in
//! the same directory, then `rename` it over the destination. `rename` within a
//! filesystem is atomic, so a reader (or a crash) sees either the complete old
//! file or the complete new one — never a partial write.
//!
//! The temporary file must live in the same directory as the destination: a
//! `rename` across filesystems is not atomic and fails outright, and the
//! system temp directory is frequently on a different mount than `~/.config`.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::{ForgeError, Result};

/// Write `contents` to `path` atomically, creating parent directories as needed.
///
/// On success the destination holds exactly `contents`. On failure the previous
/// contents are left untouched.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    write_atomic_inner(path, contents, |tmp| File::create(tmp))
}

/// Write `contents` atomically, creating the temporary file with `mode`.
///
/// The mode is applied before any contents are written.
#[cfg(unix)]
pub fn write_atomic_with_mode(path: &Path, contents: &str, mode: u32) -> Result<()> {
    write_atomic_inner(path, contents, |tmp| {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(tmp)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        Ok(file)
    })
}

fn write_atomic_inner(
    path: &Path,
    contents: &str,
    create_temp: impl FnOnce(&Path) -> std::io::Result<File>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(ForgeError::io(format!("creating {}", parent.display())))?;
    }

    let tmp = temp_path(path);
    // Scope the write so the file is closed (flushed to the OS) before the
    // rename. `rename` on an open file is fine on Unix but not on Windows.
    {
        let mut file = create_temp(&tmp)
            .map_err(ForgeError::io(format!("creating {}", tmp.display())))?;
        file.write_all(contents.as_bytes())
            .map_err(ForgeError::io(format!("writing {}", tmp.display())))?;
        // Without this, a crash after `rename` can leave a zero-length file:
        // the rename is durable but the data behind it may not be.
        file.sync_all()
            .map_err(ForgeError::io(format!("flushing {}", tmp.display())))?;
    }

    std::fs::rename(&tmp, path).map_err(|e| {
        // Do not leave the temp file behind for the next run to trip over.
        let _ = std::fs::remove_file(&tmp);
        ForgeError::io(format!(
            "replacing {} with the newly written store",
            path.display()
        ))(e)
    })
}

/// The temporary path to write before renaming onto `path`.
///
/// A sibling of the destination, so the rename stays within one filesystem, and
/// suffixed with the process id so two concurrent invocations cannot write to
/// the same temporary file.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "soroban-forge-atomic-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_the_contents() {
        let dir = temp_dir("write");
        let path = dir.join("store.json");

        write_atomic(&path, "{\"a\":1}").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_parent_directories() {
        let dir = temp_dir("parents");
        let path = dir.join("nested").join("deeper").join("store.json");

        write_atomic(&path, "{}").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaces_existing_contents_completely() {
        // A longer previous value must not survive as a tail of the new one,
        // which is exactly what a non-atomic truncate-and-write can produce.
        let dir = temp_dir("replace");
        let path = dir.join("store.json");
        write_atomic(&path, "{\"identities\":{\"a\":\"secret-a\"}}").unwrap();

        write_atomic(&path, "{}").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaves_no_temporary_file_behind() {
        let dir = temp_dir("cleanup");
        let path = dir.join("store.json");

        write_atomic(&path, "{}").unwrap();

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["store.json".to_string()], "stray temp file left: {entries:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_leaves_the_previous_file_untouched() {
        // Simulate the failure the issue describes: the write step fails before
        // the rename. Pointing at a destination whose parent cannot be created
        // (a path component that is a file) reproduces it without needing to
        // interrupt a real write.
        let dir = temp_dir("failure");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();

        let path = blocker.join("store.json");
        let result = write_atomic(&path, "{\"new\":true}");

        assert!(result.is_err(), "writing under a file must fail");
        // The blocking file is intact, and no partial store was created.
        assert_eq!(std::fs::read_to_string(&blocker).unwrap(), "not a directory");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
