//! Automatic AGY `settings.json` integration and diagnostics (`apply` / `doctor`).

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{self, Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::db::SessionMetadata;
use crate::fsutil::atomic_write_preserving_mode;
use crate::git::GitInfo;
use crate::layout::try_load_layout_config;
use crate::render::write_statusline;
use crate::tui::sample_preview_payload;

/// Resolves the preferred binary path to register in AGY `settings.json`.
///
/// Preserves the launcher path for explicit invocations and bare names on `PATH`.
pub fn resolve_invoked_binary_path() -> io::Result<PathBuf> {
    let exe = env::current_exe()?;
    let (Some(arg0), Ok(target)) = (env::args_os().next(), exe.canonicalize()) else {
        return Ok(exe);
    };
    let arg0 = Path::new(&arg0);
    let launcher = |candidate: PathBuf| {
        let with_exe = (cfg!(windows) && candidate.extension().is_none())
            .then(|| candidate.with_extension("exe"));
        std::iter::once(candidate)
            .chain(with_exe)
            .filter_map(|candidate| path::absolute(candidate).ok())
            .find(|candidate| candidate.canonicalize().is_ok_and(|p| p == target))
    };

    let resolved = if arg0.components().count() > 1 {
        launcher(arg0.to_owned())
    } else {
        env::var_os("PATH")
            .and_then(|path| env::split_paths(&path).find_map(|dir| launcher(dir.join(arg0))))
    };
    Ok(resolved.unwrap_or(exe))
}

/// Builds the `statusLine` JSON object pointing to `command_path`.
pub fn build_statusline_entry(command_path: &Path, stack_with_default: bool) -> Value {
    json!({
        "type": "command",
        "command": command_path.to_string_lossy(),
        "enabled": true,
        "stack_with_default": stack_with_default
    })
}

fn read_settings_object(settings_path: &Path, allow_missing: bool) -> io::Result<Map<String, Value>> {
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
pub fn apply_statusline_config(
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

fn command_executable_exists(cmd: &str) -> bool {
    command_executable_exists_in_path(cmd, env::var_os("PATH").as_deref())
}

fn command_executable_exists_in_path(cmd: &str, path_env: Option<&std::ffi::OsStr>) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return false;
    }
    if Path::new(trimmed).is_file() {
        return true;
    }
    let Some(exe) = trimmed.split_whitespace().next() else {
        return false;
    };
    let exe_path = Path::new(exe);
    if exe_path.components().count() > 1 {
        return exe_path.is_file();
    }
    let Some(paths) = path_env else {
        return false;
    };
    env::split_paths(paths).any(|dir| {
        let candidate = dir.join(exe);
        if candidate.is_file() {
            return true;
        }
        cfg!(windows)
            && candidate.extension().is_none()
            && candidate.with_extension("exe").is_file()
    })
}

/// Inspects XDG config, AGY `settings.json` wiring, and prints a live sample preview.
pub fn run_doctor<W: Write>(
    config_path: &Path,
    settings_path: &Path,
    no_color: bool,
    out: &mut W,
) -> io::Result<()> {
    writeln!(out, "agy-statusline v{}", env!("CARGO_PKG_VERSION"))?;

    let (mut cfg, cfg_state) = match try_load_layout_config(config_path) {
        Ok(cfg) => (cfg, "custom file".into()),
        Err(e) => {
            let state = match e.kind() {
                io::ErrorKind::NotFound => "not found (using built-in defaults)".into(),
                io::ErrorKind::InvalidData => {
                    format!("invalid config: {e} (using built-in defaults)")
                }
                _ => format!("unreadable config: {e} (using built-in defaults)"),
            };
            (Default::default(), state)
        }
    };
    cfg.no_color = no_color;
    writeln!(out, "Config:   {} [{cfg_state}]", config_path.display())?;
    writeln!(
        out,
        "Active:   style={} · theme={} · glyphs={} · stack={}",
        cfg.style.name(),
        cfg.theme.name(),
        cfg.glyph_mode.name(),
        cfg.stack_with_default
    )?;
    writeln!(
        out,
        "NO_COLOR: {}",
        if no_color { "active" } else { "unset" }
    )?;

    match read_settings_object(settings_path, false) {
        Ok(root) => match root.get("statusLine").and_then(Value::as_object) {
            Some(sl) => {
                let cmd_opt = sl.get("command").and_then(Value::as_str);
                let cmd = cmd_opt.unwrap_or("<missing>");
                let enabled = sl
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let stack = sl
                    .get("stack_with_default")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let cmd_exists = cmd_opt.is_some_and(command_executable_exists);
                writeln!(
                    out,
                    "AGY Hook: {} [enabled={enabled}, stack={stack}, bin_exists={cmd_exists}]",
                    settings_path.display()
                )?;
                writeln!(out, "Command:  {cmd}")?;
            }
            None => {
                writeln!(
                    out,
                    "AGY Hook: {} [missing statusLine entry — run `agy-statusline apply`]",
                    settings_path.display()
                )?;
            }
        },
        Err(e) => {
            writeln!(out, "AGY Hook: {} [error: {e}]", settings_path.display())?;
        }
    }

    writeln!(out, "Preview:")?;
    let sample = sample_preview_payload(100);
    let meta = SessionMetadata {
        speed: Some(64.0),
        median_speed: Some(66.0),
        last_turn_speed: Some(72.0),
        e2e_speed: Some(46.0),
        ttft_secs: Some(1.4),
        thinking_tokens: Some(4_200),
        max_context_tokens: Some(512_000),
        ..Default::default()
    };
    let git = GitInfo {
        branch: Some("main".to_string()),
        worktree: Some("feat-wt".to_string()),
        added: 42,
        deleted: 11,
    };
    write_statusline(&sample, &meta, &git, &cfg, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::agy_settings_path_for_home;
    use std::io::Cursor;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agy_statusline_apply_{}_{}",
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
        let settings = agy_settings_path_for_home(&home);
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
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn preserves_existing_keys_padding_and_supports_disable() {
        let home = temp_home("preserve");
        let settings = agy_settings_path_for_home(&home);
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
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn rejects_invalid_existing_json() {
        let home = temp_home("invalid");
        let settings = agy_settings_path_for_home(&home);
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{not json").unwrap();

        let err =
            apply_statusline_config(&settings, Path::new("/bin/agy-statusline"), false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn doctor_reports_config_settings_and_preview() {
        let home = temp_home("doctor");
        let cfg_path = home.join("config.json");
        let settings = agy_settings_path_for_home(&home);
        apply_statusline_config(&settings, Path::new("/nonexistent/agy-statusline"), false).unwrap();

        let mut out = Vec::new();
        run_doctor(&cfg_path, &settings, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("NO_COLOR: active"));
        assert!(text.contains("enabled=true"));
        assert!(text.contains("bin_exists=false"));
        assert!(text.contains("Preview:"));
        assert!(!text.contains('\x1b'));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn command_executable_lookup_handles_args_spaces_and_bare_path_names() {
        let home = temp_home("cmd_lookup");
        let bin_dir = home.join("bin");
        let space_dir = home.join("dir with spaces");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&space_dir).unwrap();

        let bin_path = bin_dir.join("agy-statusline-fake");
        let spaced_path = space_dir.join("agy-statusline-spaced");
        fs::write(&bin_path, b"").unwrap();
        fs::write(&spaced_path, b"").unwrap();

        let path_env = env::join_paths([&bin_dir]).unwrap();

        // Explicit path with CLI arguments.
        let with_args = format!("{} --no-color --style minimal", bin_path.display());
        assert!(command_executable_exists_in_path(&with_args, None));

        // Direct path containing spaces.
        assert!(command_executable_exists_in_path(
            &spaced_path.to_string_lossy(),
            None
        ));

        // Bare command resolved via PATH (with and without args).
        assert!(command_executable_exists_in_path(
            "agy-statusline-fake",
            Some(path_env.as_os_str())
        ));
        assert!(command_executable_exists_in_path(
            "agy-statusline-fake --no-color",
            Some(path_env.as_os_str())
        ));

        // Missing bare command and missing explicit path.
        assert!(!command_executable_exists_in_path(
            "definitely-nonexistent-bin",
            Some(path_env.as_os_str())
        ));
        assert!(!command_executable_exists_in_path(
            "/nonexistent/agy-statusline --no-color",
            Some(path_env.as_os_str())
        ));

        // Also verify run_doctor reports bin_exists=true for command-with-args.
        let cfg_path = home.join("config.json");
        let settings = agy_settings_path_for_home(&home);
        apply_statusline_config(&settings, Path::new(&with_args), false).unwrap();
        let mut out = Vec::new();
        run_doctor(&cfg_path, &settings, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("bin_exists=true"));

        let _ = fs::remove_dir_all(&home);
    }
}
