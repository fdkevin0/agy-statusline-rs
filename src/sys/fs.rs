//! Atomic file write helpers with optional permission and symlink target preservation.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Extracts a file's modification time as signed Unix `(secs, nanos)`.
pub(crate) fn modified_timestamp(meta: &fs::Metadata) -> (i64, u32) {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => {
                let d = e.duration();
                (-(d.as_secs() as i64), d.subsec_nanos())
            }
        },
        Err(_) => (0, 0),
    }
}

fn atomic_write_impl(
    target: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> io::Result<()> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    if let Some(perms) = permissions {
        tmp.as_file().set_permissions(perms)?;
    }
    tmp.as_file().sync_all()?;
    tmp.persist(target).map_err(|e| e.error)?;
    Ok(())
}

/// Atomically writes `bytes` to `path`, creating parent directories as needed.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_write_impl(path, bytes, None)
}

/// Atomically writes `bytes` to `path`, preserving existing file permissions and
/// replacing the canonical target when `path` is a symlink.
pub fn atomic_write_preserving_mode(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (target, permissions) = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = fs::canonicalize(path)?;
            let perms = fs::metadata(&target)?.permissions();
            (target, Some(perms))
        }
        Ok(meta) => (path.to_path_buf(), Some(meta.permissions())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (path.to_path_buf(), None),
        Err(e) => return Err(e),
    };
    atomic_write_impl(&target, bytes, permissions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_parents_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("config.json");

        atomic_write(&target, b"first").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"first");

        atomic_write(&target, b"second").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserving_mode_keeps_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("settings.json");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

        atomic_write_preserving_mode(&target, b"new").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserving_mode_replaces_symlink_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir_all(&dotfiles).unwrap();
        let real_target = dotfiles.join("settings.json");
        fs::write(&real_target, b"initial").unwrap();
        fs::set_permissions(&real_target, fs::Permissions::from_mode(0o640)).unwrap();

        let link_path = dir.path().join("linked_settings.json");
        symlink(&real_target, &link_path).unwrap();

        atomic_write_preserving_mode(&link_path, b"updated").unwrap();

        assert!(fs::symlink_metadata(&link_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&real_target).unwrap(), b"updated");
        assert_eq!(fs::read(&link_path).unwrap(), b"updated");
        let mode = fs::metadata(&real_target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }
}
