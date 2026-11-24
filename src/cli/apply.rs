//! Interactive `apply` CLI command delegating pure `settings.json` I/O to [`crate::agy::settings`].

use std::io::{self, BufRead, Write};
use std::path::Path;

use serde_json::{Map, Value};

use crate::agy::settings::{apply_statusline_config, set_statusline_fields};
pub use crate::agy::settings::{resolve_invoked_binary_path, set_statusline_enabled};

/// Runs the `apply` CLI subcommand, prompting unless `assume_yes` is `true`.
pub fn run_apply_command<R: BufRead, W: Write>(
    assume_yes: bool,
    stack_with_default: bool,
    settings_path: &Path,
    command_path: &Path,
    stdin: &mut R,
    out: &mut W,
) -> io::Result<()> {
    if !assume_yes {
        let mut preview_map = Map::new();
        set_statusline_fields(&mut preview_map, command_path, stack_with_default);
        let preview = serde_json::to_string_pretty(&Value::Object(preview_map))
            .map_err(|e| io::Error::other(format!("failed to format preview: {e}")))?;
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
    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;

    use super::*;
    use crate::agy::paths::agy_settings_path_for_home;

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
        assert_eq!(
            after_disable["statusLine"]["command"],
            "/opt/agy-statusline"
        );
    }

    #[test]
    fn rejects_invalid_existing_json() {
        let home = tempfile::tempdir().unwrap();
        let settings = agy_settings_path_for_home(home.path());
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{not json").unwrap();

        let err = apply_statusline_config(&settings, Path::new("/bin/agy-statusline"), false)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
