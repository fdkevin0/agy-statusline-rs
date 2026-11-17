//! Automatic AGY `settings.json` configuration for `agy-statusline`.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::db::resolve_home_dir;

/// Returns the default AGY settings path (`<home>/.gemini/antigravity-cli/settings.json`).
pub fn default_settings_path() -> io::Result<PathBuf> {
    let home = resolve_home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "neither HOME nor USERPROFILE environment variable is set",
        )
    })?;
    Ok(settings_path_for_home(&home))
}

/// Returns `<home>/.gemini/antigravity-cli/settings.json` for a given home directory.
pub fn settings_path_for_home(home: &Path) -> PathBuf {
    home.join(".gemini")
        .join("antigravity-cli")
        .join("settings.json")
}

/// Builds the `statusLine` JSON object pointing to `command_path`.
pub fn build_statusline_entry(command_path: &Path) -> Value {
    json!({
        "type": "command",
        "command": command_path.to_string_lossy(),
        "stack_with_default": true
    })
}

/// Updates `settings_path` in-place (preserving existing top-level keys) to
/// configure `statusLine` for `command_path`.
///
/// # Errors
///
/// Returns an error if an existing `settings.json` cannot be read, is not a
/// valid JSON object, or cannot be written back.
pub fn apply_statusline_config(settings_path: &Path, command_path: &Path) -> io::Result<()> {
    let mut root: Map<String, Value> = match fs::read_to_string(settings_path) {
        Ok(content) if content.trim().is_empty() => Map::new(),
        Ok(content) => {
            let parsed: Value = serde_json::from_str(&content).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid JSON in {}: {e}", settings_path.display()),
                )
            })?;
            match parsed {
                Value::Object(map) => map,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} must contain a top-level JSON object", settings_path.display()),
                    ));
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(e),
    };

    root.insert(
        "statusLine".to_string(),
        build_statusline_entry(command_path),
    );

    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut formatted = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|e| io::Error::other(format!("failed to serialize settings: {e}")))?;
    formatted.push('\n');

    let tmp_path = settings_path.with_extension("json.tmp");
    fs::write(&tmp_path, formatted.as_bytes())?;
    fs::rename(&tmp_path, settings_path)?;
    Ok(())
}

/// Runs the `config` CLI subcommand, prompting unless `assume_yes` is `true`.
pub fn run_config_command<R: BufRead, W: Write>(
    assume_yes: bool,
    settings_path: &Path,
    command_path: &Path,
    stdin: &mut R,
    out: &mut W,
) -> io::Result<()> {
    let entry = build_statusline_entry(command_path);
    let preview = serde_json::to_string_pretty(&entry)
        .map_err(|e| io::Error::other(format!("failed to format preview: {e}")))?;

    if !assume_yes {
        writeln!(out, "Target: {}", settings_path.display())?;
        writeln!(out, "\"statusLine\": {preview}")?;
        write!(out, "Apply this configuration? [y/N] ")?;
        out.flush()?;

        let mut answer = String::new();
        stdin.read_line(&mut answer)?;
        let trimmed = answer.trim();
        if !trimmed.eq_ignore_ascii_case("y") && !trimmed.eq_ignore_ascii_case("yes") {
            writeln!(out, "Aborted.")?;
            return Ok(());
        }
    }

    apply_statusline_config(settings_path, command_path)?;
    writeln!(
        out,
        "Configured statusLine in {} -> {}",
        settings_path.display(),
        command_path.display()
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agy_statusline_cfg_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_new_settings_with_yes() {
        let home = temp_home("new");
        let settings = settings_path_for_home(&home);
        let cmd = PathBuf::from("/usr/local/bin/agy-statusline");
        let mut stdin = Cursor::new(b"");
        let mut out = Vec::new();

        run_config_command(true, &settings, &cmd, &mut stdin, &mut out).unwrap();

        let written: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(written["statusLine"]["type"], "command");
        assert_eq!(written["statusLine"]["command"], "/usr/local/bin/agy-statusline");
        assert_eq!(written["statusLine"]["stack_with_default"], true);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn preserves_existing_keys_and_aborts_on_no() {
        let home = temp_home("preserve");
        let settings = settings_path_for_home(&home);
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, r#"{"theme": "dark"}"#).unwrap();

        let cmd = PathBuf::from("/opt/agy-statusline");
        let mut out = Vec::new();
        run_config_command(false, &settings, &cmd, &mut Cursor::new(b"n\n"), &mut out).unwrap();
        let after_abort: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert!(after_abort.get("statusLine").is_none());

        out.clear();
        run_config_command(false, &settings, &cmd, &mut Cursor::new(b"yes\n"), &mut out).unwrap();
        let after_yes: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(after_yes["theme"], "dark");
        assert_eq!(after_yes["statusLine"]["command"], "/opt/agy-statusline");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn rejects_invalid_existing_json() {
        let home = temp_home("invalid");
        let settings = settings_path_for_home(&home);
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{not json").unwrap();

        let err = apply_statusline_config(&settings, Path::new("/bin/agy-statusline")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(&home);
    }
}
