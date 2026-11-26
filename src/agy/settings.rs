//! Pure AGY `settings.json` statusLine read/write helpers and launcher binary path resolution.

use std::env;
use std::io;
use std::path::{self, Path, PathBuf};

use serde_json::{Map, Value};

use crate::sys::fs::{atomic_write_preserving_mode, read_bounded_file_to_string};

/// Maximum size accepted when loading AGY `settings.json`.
const MAX_SETTINGS_FILE_BYTES: u64 = 1024 * 1024;

/// Resolves the preferred binary path to register in AGY `settings.json`.
///
/// Preserves the launcher path for explicit invocations and bare names on `PATH`.
pub fn resolve_invoked_binary_path() -> io::Result<PathBuf> {
    let exe = env::current_exe()?;
    let (Some(arg0), Ok(target)) = (env::args_os().next(), exe.canonicalize()) else {
        return Ok(exe);
    };
    let arg0 = Path::new(&arg0);
    let matches_target = |candidate: PathBuf| {
        path::absolute(candidate)
            .ok()
            .filter(|abs| abs.canonicalize().is_ok_and(|p| p == target))
    };

    let resolved = if arg0.components().count() > 1 {
        let with_exe =
            (cfg!(windows) && arg0.extension().is_none()).then(|| arg0.with_extension("exe"));
        std::iter::once(arg0.to_owned())
            .chain(with_exe)
            .find_map(matches_target)
    } else {
        which::which_all(arg0)
            .ok()
            .and_then(|mut iter| iter.find_map(matches_target))
    };
    Ok(resolved.unwrap_or(exe))
}

pub(crate) fn set_statusline_fields(
    map: &mut Map<String, Value>,
    command_path: &Path,
    stack_with_default: bool,
) {
    map.insert("type".to_string(), Value::String("command".to_string()));
    map.insert(
        "command".to_string(),
        Value::String(command_path.to_string_lossy().into_owned()),
    );
    map.insert("enabled".to_string(), Value::Bool(true));
    map.insert(
        "stack_with_default".to_string(),
        Value::Bool(stack_with_default),
    );
}

pub(crate) fn read_settings_object(
    settings_path: &Path,
    allow_missing: bool,
) -> io::Result<Map<String, Value>> {
    match read_bounded_file_to_string(settings_path, MAX_SETTINGS_FILE_BYTES) {
        Ok(content) if content.trim().is_empty() => Ok(Map::new()),
        Ok(content) => {
            let parsed: Value = serde_json::from_str(&content).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid JSON in {}: {e}", settings_path.display()),
                )
            })?;
            match parsed {
                Value::Object(map) => Ok(map),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} must contain a top-level JSON object",
                        settings_path.display()
                    ),
                )),
            }
        }
        Err(e) if allow_missing && e.kind() == io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e),
    }
}

fn write_settings_object(settings_path: &Path, root: Map<String, Value>) -> io::Result<()> {
    let mut formatted = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|e| io::Error::other(format!("failed to serialize settings: {e}")))?;
    formatted.push('\n');
    atomic_write_preserving_mode(settings_path, formatted.as_bytes())
}

/// Updates `settings_path` in-place (preserving existing top-level keys and custom
/// `statusLine` properties such as `padding`) to configure `statusLine` for `command_path`.
pub(crate) fn apply_statusline_config(
    settings_path: &Path,
    command_path: &Path,
    stack_with_default: bool,
) -> io::Result<()> {
    let mut root = read_settings_object(settings_path, true)?;
    let mut sl_map = match root.remove("statusLine") {
        Some(Value::Object(existing)) => existing,
        _ => Map::new(),
    };
    set_statusline_fields(&mut sl_map, command_path, stack_with_default);
    root.insert("statusLine".to_string(), Value::Object(sl_map));

    write_settings_object(settings_path, root)
}

/// Toggles `statusLine.enabled` inside `settings_path` while preserving all other fields.
pub fn set_statusline_enabled(settings_path: &Path, enabled: bool) -> io::Result<()> {
    let mut root = read_settings_object(settings_path, false)?;
    let Some(Value::Object(mut sl_map)) = root.remove("statusLine") else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no statusLine block found in {}", settings_path.display()),
        ));
    };
    sl_map.insert("enabled".to_string(), Value::Bool(enabled));
    root.insert("statusLine".to_string(), Value::Object(sl_map));
    write_settings_object(settings_path, root)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn oversized_settings_file_is_rejected_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let oversized = format!(
            "{}{}",
            " ".repeat(MAX_SETTINGS_FILE_BYTES as usize),
            r#"{"theme":"dark"}"#
        );
        fs::write(&path, &oversized).unwrap();

        assert_eq!(
            read_settings_object(&path, true).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            apply_statusline_config(&path, Path::new("/bin/agy-statusline"), false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), oversized.len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_settings_file_reads_and_updates_target_in_place() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir_all(&dotfiles).unwrap();
        let real = dotfiles.join("settings.json");
        fs::write(&real, r#"{"existing":true}"#).unwrap();

        let link = dir.path().join("settings.json");
        symlink(&real, &link).unwrap();

        apply_statusline_config(&link, Path::new("/bin/agy-statusline"), true).unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let obj = read_settings_object(&link, false).unwrap();
        assert_eq!(obj.get("existing"), Some(&Value::Bool(true)));
        assert_eq!(
            obj["statusLine"]["command"],
            Value::String("/bin/agy-statusline".to_string())
        );
        assert_eq!(obj["statusLine"]["stack_with_default"], Value::Bool(true));
    }
}
