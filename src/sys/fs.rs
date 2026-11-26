//! Symlink-safe bounded file reads, metadata helpers, and atomic file writes.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Error returned by symlink-safe, size-bounded regular-file inspection and reads.
#[derive(Debug)]
pub(crate) enum BoundedReadError {
    /// Target file (or its parent directory) does not exist.
    Missing,
    /// Target file or its immediate parent directory is a symlink.
    Symlink,
    /// Target is not a regular file (or its immediate parent is not a directory).
    NotAFile,
    /// Regular file size exceeded the caller-supplied byte cap.
    Oversized(u64),
    /// Underlying filesystem I/O error.
    Io(io::Error),
}

/// Returns `fs::canonicalize(path)` when resolvable, falling back to `path` unchanged.
pub(crate) fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Returns `true` when `path` is an existing non-symlink directory.
pub(crate) fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| {
        let ft = meta.file_type();
        !ft.is_symlink() && ft.is_dir()
    })
}

/// Returns `true` when `path` is absent or an existing non-symlink regular file.
pub(crate) fn is_absent_or_real_file(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            !ft.is_symlink() && ft.is_file()
        }
        Err(err) => err.kind() == io::ErrorKind::NotFound,
    }
}

/// Returns `true` when `path`'s immediate parent (if any) is absent or a non-symlink directory,
/// and `path` itself is absent or a non-symlink regular file.
pub(crate) fn is_safe_cache_target(path: &Path) -> bool {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        match fs::symlink_metadata(parent) {
            Ok(meta) => {
                let ft = meta.file_type();
                if ft.is_symlink() || !ft.is_dir() {
                    return false;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    is_absent_or_real_file(path)
}

/// Inspects `path` via `symlink_metadata`, rejecting symlinked immediate parent directories,
/// symlinked files, and non-regular files.
pub(crate) fn safe_regular_metadata(path: &Path) -> Result<fs::Metadata, BoundedReadError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        match fs::symlink_metadata(parent) {
            Ok(meta) => {
                let ft = meta.file_type();
                if ft.is_symlink() {
                    return Err(BoundedReadError::Symlink);
                }
                if !ft.is_dir() {
                    return Err(BoundedReadError::NotAFile);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(BoundedReadError::Missing);
            }
            Err(err) => return Err(BoundedReadError::Io(err)),
        }
    }

    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(BoundedReadError::Missing),
        Err(err) => return Err(BoundedReadError::Io(err)),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Err(BoundedReadError::Symlink);
    }
    if !ft.is_file() {
        return Err(BoundedReadError::NotAFile);
    }
    Ok(meta)
}

/// Reads a non-symlink regular file under a non-symlink parent directory up to `max_bytes`,
/// enforcing the size cap both via `symlink_metadata` and a bounded `Read::take(max_bytes + 1)`.
pub(crate) fn read_bounded_regular_file(
    path: &Path,
    max_bytes: u64,
) -> Result<(Vec<u8>, fs::Metadata), BoundedReadError> {
    let meta = safe_regular_metadata(path)?;
    if meta.len() > max_bytes {
        return Err(BoundedReadError::Oversized(meta.len()));
    }
    let file = fs::File::open(path).map_err(BoundedReadError::Io)?;
    let cap = meta.len().min(max_bytes).min(64 * 1024) as usize;
    let mut bytes = Vec::with_capacity(cap);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(BoundedReadError::Io)?;
    if (bytes.len() as u64) > max_bytes {
        return Err(BoundedReadError::Oversized(bytes.len() as u64));
    }
    Ok((bytes, meta))
}

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
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_write_impl(path, bytes, None)
}

/// Atomically writes `bytes` to `path`, preserving existing file permissions and
/// replacing the canonical target when `path` is a symlink.
pub(crate) fn atomic_write_preserving_mode(path: &Path, bytes: &[u8]) -> io::Result<()> {
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

    #[test]
    fn read_bounded_regular_file_enforces_size_missing_and_non_file_guards() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.json");
        fs::write(&file, b"12345").unwrap();

        let (bytes, meta) = read_bounded_regular_file(&file, 5).unwrap();
        assert_eq!(bytes, b"12345");
        assert_eq!(meta.len(), 5);

        assert!(matches!(
            read_bounded_regular_file(&file, 4),
            Err(BoundedReadError::Oversized(5))
        ));
        assert!(matches!(
            read_bounded_regular_file(&dir.path().join("absent.json"), 64),
            Err(BoundedReadError::Missing)
        ));
        let sub = dir.path().join("subdir");
        fs::create_dir(&sub).unwrap();
        assert!(matches!(
            read_bounded_regular_file(&sub, 64),
            Err(BoundedReadError::NotAFile)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn safe_regular_metadata_and_read_reject_symlinked_file_and_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let real_file = real_dir.join("cache.json");
        fs::write(&real_file, b"{}").unwrap();

        let link_file = real_dir.join("link.json");
        symlink(&real_file, &link_file).unwrap();
        assert!(matches!(
            safe_regular_metadata(&link_file),
            Err(BoundedReadError::Symlink)
        ));
        assert!(matches!(
            read_bounded_regular_file(&link_file, 64),
            Err(BoundedReadError::Symlink)
        ));
        assert!(!is_safe_cache_target(&link_file));

        let link_dir = dir.path().join("sym_dir");
        symlink(&real_dir, &link_dir).unwrap();
        let via_link_dir = link_dir.join("cache.json");
        assert!(matches!(
            safe_regular_metadata(&via_link_dir),
            Err(BoundedReadError::Symlink)
        ));
        assert!(matches!(
            read_bounded_regular_file(&via_link_dir, 64),
            Err(BoundedReadError::Symlink)
        ));
        assert!(!is_safe_cache_target(&via_link_dir));
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
