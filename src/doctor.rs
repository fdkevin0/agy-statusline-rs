//! Configuration, AGY hook, and live preview diagnostics (`doctor`).

use std::env;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;

use serde_json::Value;

use crate::apply::read_settings_object;
use crate::layout::try_load_layout_config;
use crate::render::write_statusline;
use crate::statusline::{sample_preview_git, sample_preview_metadata, sample_preview_payload};

fn command_executable_exists(cmd: &str) -> bool {
    command_executable_exists_in_path(cmd, env::var_os("PATH").as_deref())
}

fn command_executable_exists_in_path(cmd: &str, path_env: Option<&OsStr>) -> bool {
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
    path_env.is_some_and(|paths| which::which_in(exe, Some(paths), ".").is_ok())
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
    let meta = sample_preview_metadata();
    let git = sample_preview_git();
    write_statusline(&sample, &meta, &git, &cfg, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::apply::apply_statusline_config;
    use crate::paths::agy_settings_path_for_home;

    #[cfg(windows)]
    const FAKE_BIN_NAME: &str = "agy-statusline-fake.exe";
    #[cfg(not(windows))]
    const FAKE_BIN_NAME: &str = "agy-statusline-fake";

    #[test]
    fn doctor_reports_config_settings_and_preview() {
        let home = tempfile::tempdir().unwrap();
        let cfg_path = home.path().join("config.json");
        let settings = agy_settings_path_for_home(home.path());
        apply_statusline_config(&settings, Path::new("/nonexistent/agy-statusline"), false).unwrap();

        let mut out = Vec::new();
        run_doctor(&cfg_path, &settings, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("NO_COLOR: active"));
        assert!(text.contains("enabled=true"));
        assert!(text.contains("bin_exists=false"));
        assert!(text.contains("Preview:"));
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn command_executable_lookup_handles_args_spaces_and_bare_path_names() {
        let home = tempfile::tempdir().unwrap();
        let bin_dir = home.path().join("bin");
        let space_dir = home.path().join("dir with spaces");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&space_dir).unwrap();

        let bin_path = bin_dir.join(FAKE_BIN_NAME);
        let spaced_path = space_dir.join("agy-statusline-spaced");
        fs::write(&bin_path, b"").unwrap();
        fs::write(&spaced_path, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755)).unwrap();
        }

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
        let cfg_path = home.path().join("config.json");
        let settings = agy_settings_path_for_home(home.path());
        apply_statusline_config(&settings, Path::new(&with_args), false).unwrap();
        let mut out = Vec::new();
        run_doctor(&cfg_path, &settings, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("bin_exists=true"));
    }
}
