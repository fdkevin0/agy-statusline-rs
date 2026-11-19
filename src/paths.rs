//! Centralized XDG and AGY CLI path resolution.

use std::env;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

fn non_empty_path(val: Option<OsString>) -> Option<PathBuf> {
    val.filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn resolve_home_dir_from(home: Option<OsString>, userprofile: Option<OsString>) -> Option<PathBuf> {
    non_empty_path(home).or_else(|| non_empty_path(userprofile))
}

fn resolve_agy_cli_dir_from(
    antigravity_cli_home: Option<OsString>,
    agy_home: Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    non_empty_path(antigravity_cli_home)
        .or_else(|| non_empty_path(agy_home))
        .or_else(|| home.map(|h| h.join(".gemini").join("antigravity-cli")))
}

fn xdg_config_dir_from(
    xdg_config_home: Option<OsString>,
    home: Option<PathBuf>,
) -> io::Result<PathBuf> {
    if let Some(xdg) = non_empty_path(xdg_config_home) {
        return Ok(xdg.join("agy-statusline"));
    }
    let home = home.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "neither XDG_CONFIG_HOME nor HOME/USERPROFILE is set",
        )
    })?;
    Ok(home.join(".config").join("agy-statusline"))
}

fn xdg_cache_dir_from(xdg_cache_home: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(xdg) = non_empty_path(xdg_cache_home) {
        return Some(xdg.join("agy-statusline"));
    }
    home.map(|h| h.join(".cache").join("agy-statusline"))
}

fn agy_settings_path_from(cli_dir: Option<PathBuf>) -> io::Result<PathBuf> {
    let dir = cli_dir.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "neither ANTIGRAVITY_CLI_HOME, AGY_HOME, HOME, nor USERPROFILE environment variable is set",
        )
    })?;
    Ok(dir.join("settings.json"))
}

/// Resolves the user's home directory from `HOME` or `USERPROFILE`.
pub fn resolve_home_dir() -> Option<PathBuf> {
    resolve_home_dir_from(env::var_os("HOME"), env::var_os("USERPROFILE"))
}

/// Resolves the AGY CLI data directory (`ANTIGRAVITY_CLI_HOME`, `AGY_HOME`,
/// or `<home>/.gemini/antigravity-cli`).
pub fn resolve_agy_cli_dir() -> Option<PathBuf> {
    resolve_agy_cli_dir_from(
        env::var_os("ANTIGRAVITY_CLI_HOME"),
        env::var_os("AGY_HOME"),
        resolve_home_dir(),
    )
}

/// Resolves `$XDG_CONFIG_HOME/agy-statusline` or `<home>/.config/agy-statusline`.
pub fn xdg_config_dir() -> io::Result<PathBuf> {
    xdg_config_dir_from(env::var_os("XDG_CONFIG_HOME"), resolve_home_dir())
}

/// Resolves `$XDG_CACHE_HOME/agy-statusline` or `<home>/.cache/agy-statusline`.
pub fn xdg_cache_dir() -> Option<PathBuf> {
    xdg_cache_dir_from(env::var_os("XDG_CACHE_HOME"), resolve_home_dir())
}

/// Returns the default AGY settings path (`<agy_cli_dir>/settings.json`).
///
/// Honors `ANTIGRAVITY_CLI_HOME`, `AGY_HOME`, or `<home>/.gemini/antigravity-cli`.
pub fn agy_settings_path() -> io::Result<PathBuf> {
    agy_settings_path_from(resolve_agy_cli_dir())
}

/// Returns `<home>/.gemini/antigravity-cli/settings.json` for a given home directory.
#[cfg(test)]
pub(crate) fn agy_settings_path_for_home(home: &std::path::Path) -> PathBuf {
    home.join(".gemini")
        .join("antigravity-cli")
        .join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn resolves_home_with_fallback_and_ignores_empty() {
        assert_eq!(
            resolve_home_dir_from(os("/home/alice"), os("/Users/alice")),
            Some(PathBuf::from("/home/alice"))
        );
        assert_eq!(
            resolve_home_dir_from(os(""), os("/Users/alice")),
            Some(PathBuf::from("/Users/alice"))
        );
        assert_eq!(resolve_home_dir_from(os(""), os("")), None);
        assert_eq!(resolve_home_dir_from(None, None), None);
    }

    #[test]
    fn resolves_agy_cli_dir_precedence() {
        let home = Some(PathBuf::from("/home/alice"));
        assert_eq!(
            resolve_agy_cli_dir_from(os("/custom/cli"), os("/custom/agy"), home.clone()),
            Some(PathBuf::from("/custom/cli"))
        );
        assert_eq!(
            resolve_agy_cli_dir_from(os(""), os("/custom/agy"), home.clone()),
            Some(PathBuf::from("/custom/agy"))
        );
        assert_eq!(
            resolve_agy_cli_dir_from(None, os(""), home),
            Some(PathBuf::from("/home/alice/.gemini/antigravity-cli"))
        );
        assert_eq!(resolve_agy_cli_dir_from(None, None, None), None);
    }

    #[test]
    fn resolves_xdg_config_and_cache_dirs() {
        let home = Some(PathBuf::from("/home/alice"));
        assert_eq!(
            xdg_config_dir_from(os("/xdg/cfg"), home.clone()).unwrap(),
            PathBuf::from("/xdg/cfg/agy-statusline")
        );
        assert_eq!(
            xdg_config_dir_from(os(""), home.clone()).unwrap(),
            PathBuf::from("/home/alice/.config/agy-statusline")
        );
        assert_eq!(
            xdg_config_dir_from(None, None).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        assert_eq!(
            xdg_cache_dir_from(os("/xdg/cache"), home.clone()),
            Some(PathBuf::from("/xdg/cache/agy-statusline"))
        );
        assert_eq!(
            xdg_cache_dir_from(os(""), home),
            Some(PathBuf::from("/home/alice/.cache/agy-statusline"))
        );
        assert_eq!(xdg_cache_dir_from(None, None), None);
    }
}
