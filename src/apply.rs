//! Automatic AGY `settings.json` integration (`apply`).

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{self, Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::fsutil::atomic_write_preserving_mode;

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

/// Builds the `statusLine` JSON object pointing to `command_path`.
fn build_statusline_entry(command_path: &Path, stack_with_default: bool) -> Value {
    json!({
        "type": "command",
        "command": command_path.to_string_lossy(),
        "enabled": true,
        "stack_with_default": stack_with_default
    })
}

pub(crate) fn read_settings_object(
    settings_path: &Path,
    allow_missing: bool,
) -> io::Result<Map<String, Value>> {
    match fs::read_to_string(settings_path) {
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
///
/// # Errors
///
/// Returns an error if an existing `settings.json` cannot be read, is not a
/// valid JSON object, or cannot be written back.
pub(crate) fn apply_statusline_config(
    settings_path: &Path,
    command_path: &Path,
    stack_with_default: bool,
) -> io::Result<()> {
    let mut root = read_settings_object(settings_path, true)?;
    let mut sl_map = match root.get("statusLine") {
        Some(Value::Object(existing)) => existing.clone(),
        _ => Map::new(),
    };
    sl_map.insert("type".to_string(), Value::String("command".to_string()));
    sl_map.insert(
        "command".to_string(),
        Value::String(command_path.to_string_lossy().into_owned()),
    );
    sl_map.insert("enabled".to_string(), Value::Bool(true));
    sl_map.insert(
        "stack_with_default".to_string(),
        Value::Bool(stack_with_default),
    );
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

/// Runs the `apply` CLI subcommand, prompting unless `assume_yes` is `true`.
pub fn run_apply_command<R: BufRead, W: Write>(
    assume_yes: bool,
    stack_with_default: bool,
    settings_path: &Path,
    command_path: &Path,
    stdin: &mut R,
    out: &mut W,
) -> io::Result<()> {
    let entry = build_statusline_entry(command_path, stack_with_default);
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

    apply_statusline_config(settings_path, command_path, stack_with_default)?;
    writeln!(
        out,
        "Configured statusLine (stack_with_default={stack_with_default}) in {} -> {}",
        settings_path.display(),
        command_path.display()
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::agy_settings_path_for_home;
    use std::io::Cursor;

    #[test]
    fn creates_new_settings_with_yes() {
        let home = tempfile::tempdir().unwrap();
        let settings = agy_settings_path_for_home(home.path());
        let cmd = PathBuf::from("/usr/local/bin/agy-statusline");
        let mut stdin = Cursor::new(b"");
        let mut out = Vec::new();

        run_apply_command(true, false, &settings, &cmd, &mut stdin, &mut out).unwrap();

        let written: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(written["statusLine"]["type"], "command");
        assert_eq!(
            written["statusLine"]["command"],
            "/usr/local/bin/agy-statusline"
        );
        assert_eq!(written["statusLine"]["enabled"], true);
        assert_eq!(written["statusLine"]["stack_with_default"], false);
    }

    #[test]
    fn preserves_existing_keys_padding_and_supports_disable() {
        let home = tempfile::tempdir().unwrap();
        let settings = agy_settings_path_for_home(home.path());
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            r#"{"theme": "dark", "statusLine": {"padding": 1, "custom": "keep"}}"#,
        )
        .unwrap();

        let cmd = PathBuf::from("/opt/agy-statusline");
        let mut out = Vec::new();
        run_apply_command(
            false,
            true,
            &settings,
            &cmd,
            &mut Cursor::new(b"n\n"),
            &mut out,
        )
        .unwrap();
        let after_abort: Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(after_abort["statusLine"]["padding"], 1);
        assert!(after_abort["statusLine"].get("command").is_none());

        out.clear();
        run_apply_command(
            false,
            true,
            &settings,
            &cmd,
            &mut Cursor::new(b"yes\n"),
            &mut out,
        )
        .unwrap();
        let after_yes: Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(after_yes["theme"], "dark");
        assert_eq!(after_yes["statusLine"]["padding"], 1);
        assert_eq!(after_yes["statusLine"]["custom"], "keep");
        assert_eq!(after_yes["statusLine"]["command"], "/opt/agy-statusline");
        assert_eq!(after_yes["statusLine"]["enabled"], true);
        assert_eq!(after_yes["statusLine"]["stack_with_default"], true);

        set_statusline_enabled(&settings, false).unwrap();
        let after_disable: Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(after_disable["statusLine"]["enabled"], false);
        assert_eq!(after_disable["statusLine"]["padding"], 1);
        assert_eq!(after_disable["statusLine"]["command"], "/opt/agy-statusline");
    }

    #[test]
    fn rejects_invalid_existing_json() {
        let home = tempfile::tempdir().unwrap();
        let settings = agy_settings_path_for_home(home.path());
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{not json").unwrap();

        let err =
            apply_statusline_config(&settings, Path::new("/bin/agy-statusline"), false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
