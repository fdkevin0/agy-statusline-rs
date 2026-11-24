//! Centralized XDG and AGY CLI path resolution.

use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn non_empty_path(val: Option<OsString>) -> Option<PathBuf> {
    val.filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn resolve_home_dir_from(home: Option<OsString>, userprofile: Option<OsString>) -> Option<PathBuf> {
    non_empty_path(home).or_else(|| non_empty_path(userprofile))
}

/// Expands a leading `~` or `~/` (and `~\` on Windows) against `home`.
///
/// Leaves empty strings, `~user/...` paths, and inputs with `home == None` unchanged.
pub fn expand_tilde(raw: &str, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from(raw);
    };
    if raw == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home.join(rest.trim_start_matches('/'));
    }
    #[cfg(windows)]
    if let Some(rest) = raw.strip_prefix("~\\") {
        return home.join(rest.trim_start_matches(['/', '\\']));
    }
    PathBuf::from(raw)
}

fn expand_tilde_os(raw: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    let raw = raw.filter(|v| !v.is_empty())?;
    match raw.to_str() {
        Some(s) => Some(expand_tilde(s, home)),
        None => Some(PathBuf::from(raw)),
    }
}

fn classify_data_dir_source(path: &Path) -> &'static str {
    match path.file_name().and_then(OsStr::to_str) {
        Some("antigravity") => "desktop",
        _ => "cli",
    }
}

fn push_unique_root(
    roots: &mut Vec<(&'static str, PathBuf)>,
    seen: &mut HashSet<PathBuf>,
    source: &'static str,
    path: PathBuf,
) {
    let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if seen.insert(key) {
        roots.push((source, path));
    }
}

/// Resolves all official AGY CLI, Desktop, and explicit custom data roots from explicit inputs,
/// deduplicating canonical/identical paths while preserving first-seen source priority
/// (`"cli"` before `"desktop"`).
pub fn resolve_agy_roots_from(
    home: Option<&Path>,
    antigravity_data_dir: Option<&OsStr>,
    antigravity_cli_home: Option<&OsStr>,
) -> Vec<(&'static str, PathBuf)> {
    let mut roots: Vec<(&'static str, PathBuf)> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut has_explicit_cli_home = false;

    if let Some(path) = expand_tilde_os(antigravity_cli_home, home) {
        has_explicit_cli_home = true;
        push_unique_root(&mut roots, &mut seen, "cli", path);
    }

    if let Some(raw) = antigravity_data_dir.filter(|v| !v.is_empty()) {
        match raw.to_str() {
            Some(s) => {
                for entry in s.split(',') {
                    let trimmed = entry.trim();
                    if !trimmed.is_empty() {
                        let path = expand_tilde(trimmed, home);
                        let source = classify_data_dir_source(&path);
                        push_unique_root(&mut roots, &mut seen, source, path);
                    }
                }
            }
            None => {
                let path = PathBuf::from(raw);
                let source = classify_data_dir_source(&path);
                push_unique_root(&mut roots, &mut seen, source, path);
            }
        }
    }

    if let Some(h) = home {
        let gemini = h.join(".gemini");
        if !has_explicit_cli_home {
            push_unique_root(&mut roots, &mut seen, "cli", gemini.join("antigravity-cli"));
        }
        push_unique_root(&mut roots, &mut seen, "desktop", gemini.join("antigravity"));
    }

    roots
}

/// Resolves all AGY data roots from `HOME`/`USERPROFILE`, `ANTIGRAVITY_CLI_HOME`,
/// and `ANTIGRAVITY_DATA_DIR`.
pub fn resolve_agy_roots() -> Vec<(&'static str, PathBuf)> {
    let resolved_home = resolve_home_dir();
    let data_dir = env::var_os("ANTIGRAVITY_DATA_DIR");
    let cli_home = env::var_os("ANTIGRAVITY_CLI_HOME");
    resolve_agy_roots_from(
        resolved_home.as_deref(),
        data_dir.as_deref(),
        cli_home.as_deref(),
    )
}

fn resolve_agy_cli_dir_from(
    antigravity_cli_home: Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    non_empty_path(antigravity_cli_home)
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
            "neither ANTIGRAVITY_CLI_HOME, HOME, nor USERPROFILE environment variable is set",
        )
    })?;
    Ok(dir.join("settings.json"))
}

/// Resolves the user's home directory from `HOME` or `USERPROFILE`.
pub fn resolve_home_dir() -> Option<PathBuf> {
    resolve_home_dir_from(env::var_os("HOME"), env::var_os("USERPROFILE"))
}

/// Resolves the AGY CLI data directory (`ANTIGRAVITY_CLI_HOME`
/// or `<home>/.gemini/antigravity-cli`).
pub fn resolve_agy_cli_dir() -> Option<PathBuf> {
    resolve_agy_cli_dir_from(env::var_os("ANTIGRAVITY_CLI_HOME"), resolve_home_dir())
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
/// Honors `ANTIGRAVITY_CLI_HOME` or `<home>/.gemini/antigravity-cli`.
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
            resolve_agy_cli_dir_from(os("/custom/cli"), home.clone()),
            Some(PathBuf::from("/custom/cli"))
        );
        assert_eq!(
            resolve_agy_cli_dir_from(os(""), home),
            Some(PathBuf::from("/home/alice/.gemini/antigravity-cli"))
        );
        assert_eq!(resolve_agy_cli_dir_from(None, None), None);
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

    #[test]
    fn expands_tilde_only_for_exact_or_separator_prefix() {
        let home = Path::new("/home/alice");
        assert_eq!(expand_tilde("~", Some(home)), PathBuf::from("/home/alice"));
        assert_eq!(
            expand_tilde("~/sub/dir", Some(home)),
            PathBuf::from("/home/alice/sub/dir")
        );
        assert_eq!(expand_tilde("~/", Some(home)), PathBuf::from("/home/alice"));
        assert_eq!(
            expand_tilde("~bob/sub", Some(home)),
            PathBuf::from("~bob/sub")
        );
        assert_eq!(expand_tilde("", Some(home)), PathBuf::from(""));
        assert_eq!(
            expand_tilde("/abs/path", Some(home)),
            PathBuf::from("/abs/path")
        );
        assert_eq!(expand_tilde("~/sub/dir", None), PathBuf::from("~/sub/dir"));
    }

    #[test]
    fn resolve_agy_roots_from_expands_env_deduplicates_and_discovers_official_roots() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        let data_cli = home.join("extra-data");
        let data_desk = home.join("extra").join("antigravity");
        fs::create_dir_all(&data_cli).unwrap();
        fs::create_dir_all(&data_desk).unwrap();

        let data_dir_val = OsStr::new(" ~/extra-data , , ~/extra/antigravity , ~/extra-data ");
        let roots = resolve_agy_roots_from(Some(home), Some(data_dir_val), None);
        assert_eq!(
            roots,
            vec![
                ("cli", data_cli.clone()),
                ("desktop", data_desk.clone()),
                ("cli", home.join(".gemini").join("antigravity-cli")),
                ("desktop", home.join(".gemini").join("antigravity")),
            ]
        );

        // Explicit ANTIGRAVITY_CLI_HOME replaces the default `.gemini/antigravity-cli`
        // and wins `"cli"` priority over a matching desktop data dir.
        let shared_ag = home.join("shared").join("antigravity");
        fs::create_dir_all(&shared_ag).unwrap();

        let explicit_roots = resolve_agy_roots_from(
            Some(home),
            Some(OsStr::new("~/shared/antigravity")),
            Some(shared_ag.as_os_str()),
        );
        assert_eq!(
            explicit_roots,
            vec![
                ("cli", shared_ag),
                ("desktop", home.join(".gemini").join("antigravity")),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_agy_roots_from_deduplicates_canonical_targets_with_cli_priority() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // Symlinked `.gemini/antigravity` pointing to `.gemini/antigravity-cli` deduplicates
        // to the canonical target while preserving `"cli"` source priority.
        let cli_dir = home.join(".gemini").join("antigravity-cli");
        let desk_dir = home.join(".gemini").join("antigravity");
        fs::create_dir_all(&cli_dir).unwrap();
        symlink(&cli_dir, &desk_dir).unwrap();

        let roots = resolve_agy_roots_from(Some(home), None, None);
        assert_eq!(roots, vec![("cli", cli_dir)]);
    }
}
