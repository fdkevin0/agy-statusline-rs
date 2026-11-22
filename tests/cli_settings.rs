#![forbid(unsafe_code)]
//! Tier 2: settings updates and diagnostics through the installed CLI entry point.

mod common;

use std::fs;
use std::process::Output;

#[cfg(unix)]
use serde_json::Value;

#[cfg(unix)]
use common::{isolated_cmd, BIN};
use common::{isolated_agy_cmd, run_statusline_stdin};

fn stdout(output: Output) -> String {
    assert!(output.status.success(), "{:?}", output);
    String::from_utf8(output.stdout).unwrap()
}

#[cfg(unix)]
#[test]
fn tier2_settings_updates_preserve_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let agy = root.path().join("agy");
    fs::create_dir(&agy).unwrap();
    let settings = agy.join("settings.json");
    let mode = || fs::metadata(&settings).unwrap().permissions().mode() & 0o777;

    let mut cmd = isolated_agy_cmd(root.path());
    cmd.env("ANTIGRAVITY_CLI_HOME", &agy).env("NO_COLOR", "1");
    stdout(cmd.args(["apply", "--yes"]).output().unwrap());
    assert_eq!(mode() & 0o077, 0);

    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    stdout(cmd.output().unwrap());
    assert_eq!(mode(), 0o600);

    fs::set_permissions(&settings, fs::Permissions::from_mode(0o640)).unwrap();
    stdout(
        isolated_agy_cmd(root.path())
            .env("ANTIGRAVITY_CLI_HOME", &agy)
            .env("NO_COLOR", "1")
            .args(["apply", "--disable"])
            .output()
            .unwrap(),
    );
    assert_eq!(mode(), 0o640);
}

#[cfg(unix)]
#[test]
fn tier2_apply_preserves_path_and_explicit_relative_launchers() {
    use std::os::unix::fs::symlink;
    use std::os::unix::process::CommandExt;

    let root = tempfile::tempdir().unwrap();
    let agy = root.path().join("agy");
    let bin = root.path().canonicalize().unwrap().join("bin");
    fs::create_dir(&bin).unwrap();
    let path_launcher = bin.join("agy-statusline");
    let relative_launcher = bin.join("statusline");
    symlink(BIN, &path_launcher).unwrap();
    symlink(BIN, &relative_launcher).unwrap();
    let settings = agy.join("settings.json");
    let read = || serde_json::from_slice::<Value>(&fs::read(&settings).unwrap()).unwrap();

    stdout(
        isolated_cmd(&path_launcher, root.path())
            .env("ANTIGRAVITY_CLI_HOME", &agy)
            .env("NO_COLOR", "1")
            .arg0("agy-statusline")
            .env("PATH", "missing:bin")
            .args(["apply", "--yes"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        read()["statusLine"]["command"],
        path_launcher.to_str().unwrap()
    );

    stdout(
        isolated_cmd(&relative_launcher, root.path())
            .env("ANTIGRAVITY_CLI_HOME", &agy)
            .env("NO_COLOR", "1")
            .arg0("./bin/statusline")
            .args(["apply", "--yes"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        read()["statusLine"]["command"],
        relative_launcher.to_str().unwrap()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn tier2_tui_save_preserves_launcher_across_upgrade() {
    use std::io::{Read, Write};
    use std::os::unix::fs::symlink;
    use std::process::Stdio;

    let root = tempfile::tempdir().unwrap();
    let dir = root.path().canonicalize().unwrap();
    let agy = dir.join("agy");
    let old = dir.join("v1");
    let launcher = dir.join("launcher");
    fs::copy(BIN, &old).unwrap();
    symlink(&old, &launcher).unwrap();

    let mut tui = isolated_cmd("script", &dir)
        .env("ANTIGRAVITY_CLI_HOME", &agy)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TERM", "xterm-256color")
        .env("SHELL", "/bin/sh")
        .args([
            "-qec",
            "stty rows 24 cols 100 && exec ./launcher config",
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // The first terminal write signals that the editor has entered raw mode.
    tui.stdout.as_mut().unwrap().read_exact(&mut [0]).unwrap();
    let next = dir.join("next");
    symlink(BIN, &next).unwrap();
    fs::rename(next, &launcher).unwrap();
    fs::remove_file(old).unwrap();
    tui.stdin.as_mut().unwrap().write_all(b"s").unwrap();
    let output = tui.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let settings: Value =
        serde_json::from_slice(&fs::read(agy.join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        settings["statusLine"]["command"],
        launcher.to_str().unwrap()
    );
}

#[test]
fn tier2_doctor_reports_config_states_and_render_uses_defaults_on_error() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config/agy-statusline/config.json");
    let doctor = || {
        stdout(
            isolated_agy_cmd(root.path())
                .env("NO_COLOR", "1")
                .arg("doctor")
                .output()
                .unwrap(),
        )
    };
    let render = || run_statusline_stdin(root.path(), &[], r#"{"cost":{"total_usd":1.0}}"#);

    assert!(doctor().contains("[not found (using built-in defaults)]"));
    let default_render = render();
    assert!(default_render.contains("$1.00"));

    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "{broken").unwrap();
    let invalid = doctor();
    assert!(invalid.contains("[invalid config:"));
    assert!(invalid.contains("using built-in defaults"));
    assert!(invalid.contains("Preview:"));
    assert_eq!(render(), default_render);

    fs::remove_file(&config).unwrap();
    fs::create_dir(&config).unwrap();
    assert!(doctor().contains("[unreadable config:"));

    fs::remove_dir(&config).unwrap();
    fs::write(&config, r#"{"stack_with_default":true}"#).unwrap();
    let valid = doctor();
    assert!(valid.contains("[custom file]"));
    assert!(valid.contains("stack=true"));
}

#[test]
fn tier2_apply_and_doctor_honor_antigravity_cli_home_and_agy_home() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let cli_home = base.join("custom_cli_home");
    let agy_home = base.join("custom_agy_home");

    // ANTIGRAVITY_CLI_HOME takes precedence over AGY_HOME and HOME.
    stdout(
        isolated_agy_cmd(base)
            .env("ANTIGRAVITY_CLI_HOME", &cli_home)
            .env("AGY_HOME", &agy_home)
            .env("NO_COLOR", "1")
            .args(["apply", "--yes"])
            .output()
            .unwrap(),
    );
    assert!(cli_home.join("settings.json").is_file());
    assert!(!agy_home.join("settings.json").exists());
    assert!(!base.join(".gemini/antigravity-cli/settings.json").exists());

    // AGY_HOME is used when ANTIGRAVITY_CLI_HOME is unset.
    stdout(
        isolated_agy_cmd(base)
            .env_remove("ANTIGRAVITY_CLI_HOME")
            .env("AGY_HOME", &agy_home)
            .env("NO_COLOR", "1")
            .args(["apply", "--yes"])
            .output()
            .unwrap(),
    );
    assert!(agy_home.join("settings.json").is_file());

    let doctor_text = stdout(
        isolated_agy_cmd(base)
            .env_remove("ANTIGRAVITY_CLI_HOME")
            .env("AGY_HOME", &agy_home)
            .env("NO_COLOR", "1")
            .args(["doctor"])
            .output()
            .unwrap(),
    );
    assert!(
        doctor_text.contains(&agy_home.join("settings.json").display().to_string()),
        "doctor stdout: {doctor_text}"
    );
    assert!(doctor_text.contains("enabled=true"));
}

#[test]
fn tier2_doctor_reports_conversations_archive_and_agy_cli_status() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let empty = stdout(
        isolated_agy_cmd(root)
            .env("NO_COLOR", "1")
            .arg("doctor")
            .output()
            .unwrap(),
    );
    assert!(empty.contains("AGY CLI:  not found"), "doctor stdout:\n{empty}");
    assert!(
        empty.contains("conversations [not found]"),
        "doctor stdout:\n{empty}"
    );
    assert!(
        empty.contains("stats_history.db [not found]"),
        "doctor stdout:\n{empty}"
    );

    let conv_dir = root.join(".gemini/antigravity-cli/conversations");
    fs::create_dir_all(&conv_dir).unwrap();
    fs::write(
        conv_dir.join("11111111-1111-4111-8111-111111111111.db"),
        b"db",
    )
    .unwrap();
    fs::write(
        conv_dir.join("11111111-1111-4111-8111-111111111111.db-wal"),
        b"wal",
    )
    .unwrap();
    fs::write(conv_dir.join("not-a-uuid.db"), b"junk").unwrap();

    let archive_path = root.join("data/agy-statusline/stats_history.db");
    fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
    fs::write(&archive_path, vec![0u8; 2048]).unwrap();

    let populated = stdout(
        isolated_agy_cmd(root)
            .env("NO_COLOR", "1")
            .arg("doctor")
            .output()
            .unwrap(),
    );
    assert!(
        populated.contains("conversations [found, 1 session .db file]"),
        "doctor stdout:\n{populated}"
    );
    assert!(
        populated.contains("stats_history.db [found, 2.0 KiB]"),
        "doctor stdout:\n{populated}"
    );
}

#[cfg(unix)]
#[test]
fn tier2_doctor_probes_agy_version_and_handles_nonzero_exit() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let agy_bin = bin_dir.join("agy");

    // Symlink the real compiled binary (supports `--version`) instead of a shell stub.
    symlink(BIN, &agy_bin).unwrap();
    let ok_out = stdout(
        isolated_agy_cmd(root)
            .env("NO_COLOR", "1")
            .env("PATH", &bin_dir)
            .arg("doctor")
            .output()
            .unwrap(),
    );
    assert!(
        ok_out.contains(&format!(
            "AGY CLI:  agy-statusline {}",
            env!("CARGO_PKG_VERSION")
        )),
        "doctor stdout:\n{ok_out}"
    );

    // Symlink the real system `false` utility to verify non-zero exit never fails `doctor`.
    fs::remove_file(&agy_bin).unwrap();
    symlink(which::which("false").unwrap(), &agy_bin).unwrap();
    let err_out = stdout(
        isolated_agy_cmd(root)
            .env("NO_COLOR", "1")
            .env("PATH", &bin_dir)
            .arg("doctor")
            .output()
            .unwrap(),
    );
    assert!(
        err_out.contains("AGY CLI:  unavailable (exit"),
        "doctor stdout:\n{err_out}"
    );
    assert!(err_out.contains("Preview:"), "doctor stdout:\n{err_out}");
}
