//! Tier 2: settings updates and diagnostics through the installed CLI entry point.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_agy-statusline");

fn isolated_command(program: impl AsRef<Path>, root: &Path) -> Command {
    let mut cmd = Command::new(program.as_ref());
    cmd.current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("ANTIGRAVITY_CLI_HOME", root.join("agy"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("NO_COLOR", "1");
    cmd
}

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

    let mut cmd = isolated_command(BIN, root.path());
    stdout(cmd.args(["apply", "--yes"]).output().unwrap());
    assert_eq!(mode() & 0o077, 0);

    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    stdout(cmd.output().unwrap());
    assert_eq!(mode(), 0o600);

    fs::set_permissions(&settings, fs::Permissions::from_mode(0o640)).unwrap();
    stdout(
        isolated_command(BIN, root.path())
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
    let bin = root.path().canonicalize().unwrap().join("bin");
    fs::create_dir(&bin).unwrap();
    let path_launcher = bin.join("agy-statusline");
    let relative_launcher = bin.join("statusline");
    symlink(BIN, &path_launcher).unwrap();
    symlink(BIN, &relative_launcher).unwrap();
    let settings = root.path().join("agy/settings.json");
    let read = || serde_json::from_slice::<Value>(&fs::read(&settings).unwrap()).unwrap();

    stdout(
        isolated_command(&path_launcher, root.path())
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
        isolated_command(&relative_launcher, root.path())
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
    let old = dir.join("v1");
    let launcher = dir.join("launcher");
    fs::copy(BIN, &old).unwrap();
    symlink(&old, &launcher).unwrap();

    let mut tui = isolated_command("script", &dir)
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
        serde_json::from_slice(&fs::read(dir.join("agy/settings.json")).unwrap()).unwrap();
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
            isolated_command(BIN, root.path())
                .arg("doctor")
                .output()
                .unwrap(),
        )
    };
    let cache = root.path().join("cache/agy-statusline");
    fs::create_dir_all(&cache).unwrap();
    let fetched_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(
        cache.join("models_dev_pricing.json"),
        serde_json::json!({"fetched_at": fetched_at, "models": {}}).to_string(),
    )
    .unwrap();
    let input = root.path().join("payload.json");
    fs::write(&input, r#"{"cost":{"total_usd":1.0}}"#).unwrap();
    let render = || {
        stdout(
            isolated_command(BIN, root.path())
                .stdin(fs::File::open(&input).unwrap())
                .output()
                .unwrap(),
        )
    };

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
