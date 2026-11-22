//! Configuration, AGY hook, storage, and live preview diagnostics (`doctor`).

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::apply::read_settings_object;
use crate::db::{is_safe_sqlite_file, is_valid_uuid};
use crate::layout::try_load_layout_config;
use crate::paths::{resolve_agy_cli_dir, xdg_data_dir};
use crate::procutil::{run_command_stdout_bounded, BoundedCommandError};
use crate::render::write_statusline;
use crate::stats::ARCHIVE_DB_FILENAME;
use crate::statusline::{sample_preview_git, sample_preview_metadata, sample_preview_payload};

const AGY_VERSION_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_AGY_VERSION_BYTES: u64 = 4096;

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

fn format_version_output(bytes: &[u8]) -> String {
    let raw = String::from_utf8_lossy(bytes);
    let stripped = strip_ansi_escapes::strip_str(&raw);
    stripped
        .lines()
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
        })
        .map(|line| line.trim().to_string())
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| "unknown (empty output)".to_string())
}

fn probe_agy_version(path_env: Option<&OsStr>, timeout: Duration) -> String {
    let Some(paths) = path_env else {
        return "not found".to_string();
    };
    let Ok(agy_bin) = which::which_in("agy", Some(paths), ".") else {
        return "not found".to_string();
    };

    let mut cmd = Command::new(&agy_bin);
    cmd.arg("--version");
    match run_command_stdout_bounded(&mut cmd, timeout, MAX_AGY_VERSION_BYTES) {
        Ok(bytes) => format_version_output(&bytes),
        Err(BoundedCommandError::TimedOut) => "unavailable (timed out)".to_string(),
        Err(BoundedCommandError::NonZero(status)) => format!("unavailable ({status})"),
        Err(BoundedCommandError::Io(e)) => format!("unavailable ({e})"),
    }
}

fn inspect_conversations_dir(conv_dir: &Path) -> String {
    let meta = match fs::symlink_metadata(conv_dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return "not found".to_string(),
        Err(e) => return format!("error: {e}"),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        return "symlink (ignored)".to_string();
    }
    if !ft.is_dir() {
        return "not a directory".to_string();
    }
    let entries = match fs::read_dir(conv_dir) {
        Ok(e) => e,
        Err(e) => return format!("unreadable: {e}"),
    };
    let mut db_count = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(stem) = name_str.strip_suffix(".db") else {
            continue;
        };
        if !is_valid_uuid(stem) {
            continue;
        }
        if entry
            .file_type()
            .is_ok_and(|ft| !ft.is_symlink() && ft.is_file())
            && is_safe_sqlite_file(&entry.path())
        {
            db_count += 1;
        }
    }
    let noun = if db_count == 1 { "file" } else { "files" };
    format!("found, {db_count} session .db {noun}")
}

fn format_byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if (bytes as f64) < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else if (bytes as f64) < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB)
    }
}

fn inspect_archive_db(archive_path: &Path) -> String {
    match fs::metadata(archive_path) {
        Ok(meta) if meta.is_file() => format!("found, {}", format_byte_size(meta.len())),
        Ok(_) => "not a file".to_string(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => "not found".to_string(),
        Err(e) => format!("error: {e}"),
    }
}

fn run_doctor_with<W: Write>(
    config_path: &Path,
    settings_path: &Path,
    conv_dir: Option<&Path>,
    data_dir: Option<&Path>,
    path_env: Option<&OsStr>,
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
    writeln!(
        out,
        "AGY CLI:  {}",
        probe_agy_version(path_env, AGY_VERSION_TIMEOUT)
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
                let cmd_exists =
                    cmd_opt.is_some_and(|c| command_executable_exists_in_path(c, path_env));
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

    match conv_dir {
        Some(dir) => {
            let state = inspect_conversations_dir(dir);
            writeln!(out, "Sessions: {} [{state}]", dir.display())?;
        }
        None => {
            writeln!(out, "Sessions: unavailable [AGY CLI directory unresolved]")?;
        }
    }

    match data_dir {
        Some(dir) => {
            let archive_path = dir.join(ARCHIVE_DB_FILENAME);
            let state = inspect_archive_db(&archive_path);
            writeln!(out, "Archive:  {} [{state}]", archive_path.display())?;
        }
        None => {
            writeln!(
                out,
                "Archive:  unavailable [neither XDG_DATA_HOME nor HOME is set]"
            )?;
        }
    }

    writeln!(out, "Preview:")?;
    let sample = sample_preview_payload(100);
    let meta = sample_preview_metadata();
    let git = sample_preview_git();
    write_statusline(&sample, &meta, &git, &cfg, out)?;
    Ok(())
}

/// Inspects XDG config, AGY `settings.json` wiring, local AGY/archive storage,
/// optional `agy --version`, and prints a live sample preview.
pub fn run_doctor<W: Write>(
    config_path: &Path,
    settings_path: &Path,
    no_color: bool,
    out: &mut W,
) -> io::Result<()> {
    let conv_dir: Option<PathBuf> = settings_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("conversations"))
        .or_else(|| resolve_agy_cli_dir().map(|d| d.join("conversations")));
    let data_dir = xdg_data_dir();
    let path_env = env::var_os("PATH");
    run_doctor_with(
        config_path,
        settings_path,
        conv_dir.as_deref(),
        data_dir.as_deref(),
        path_env.as_deref(),
        no_color,
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let conv_dir = settings.parent().unwrap().join("conversations");
        let data_dir = home.path().join("data");
        apply_statusline_config(&settings, Path::new("/nonexistent/agy-statusline"), false).unwrap();

        let mut out = Vec::new();
        run_doctor_with(
            &cfg_path,
            &settings,
            Some(&conv_dir),
            Some(&data_dir),
            None,
            true,
            &mut out,
        )
        .unwrap();
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

        // Also verify run_doctor_with reports bin_exists=true for command-with-args.
        let cfg_path = home.path().join("config.json");
        let settings = agy_settings_path_for_home(home.path());
        let conv_dir = settings.parent().unwrap().join("conversations");
        let data_dir = home.path().join("data");
        apply_statusline_config(&settings, Path::new(&with_args), false).unwrap();
        let mut out = Vec::new();
        run_doctor_with(
            &cfg_path,
            &settings,
            Some(&conv_dir),
            Some(&data_dir),
            Some(path_env.as_os_str()),
            true,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("bin_exists=true"));
    }

    #[test]
    fn inspect_conversations_and_version_output_handle_edge_cases() {
        let dir = tempfile::tempdir().unwrap();
        let file_as_dir = dir.path().join("conversations");
        fs::write(&file_as_dir, b"not a dir").unwrap();
        assert_eq!(inspect_conversations_dir(&file_as_dir), "not a directory");

        assert_eq!(
            format_version_output(b"\n\x1b[32m1.5.14\x07\x1b[0m\nignored line\n"),
            "1.5.14"
        );
        assert_eq!(format_version_output(b"   \n\t\r\n"), "unknown (empty output)");
    }

    #[cfg(unix)]
    #[test]
    fn inspect_conversations_ignores_symlinked_dir_and_symlinked_dbs() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_conv = dir.path().join("real_conv");
        let external = dir.path().join("external.db");
        fs::create_dir_all(&real_conv).unwrap();
        fs::write(&external, b"db").unwrap();
        fs::write(
            real_conv.join("11111111-1111-4111-8111-111111111111.db"),
            b"real",
        )
        .unwrap();
        symlink(
            &external,
            real_conv.join("22222222-2222-4222-8222-222222222222.db"),
        )
        .unwrap();
        // Real .db with symlinked -wal sidecar is also excluded, matching db/stats policy.
        fs::write(
            real_conv.join("33333333-3333-4333-8333-333333333333.db"),
            b"real-with-sym-wal",
        )
        .unwrap();
        symlink(
            &external,
            real_conv.join("33333333-3333-4333-8333-333333333333.db-wal"),
        )
        .unwrap();
        assert_eq!(
            inspect_conversations_dir(&real_conv),
            "found, 1 session .db file"
        );

        let sym_conv = dir.path().join("sym_conv");
        symlink(&real_conv, &sym_conv).unwrap();
        assert_eq!(inspect_conversations_dir(&sym_conv), "symlink (ignored)");
    }
}
