//! Centralized XDG and AGY CLI path resolution.

use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::db::is_real_dir;

fn non_empty_path(val: Option<OsString>) -> Option<PathBuf> {
    val.filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn resolve_home_dir_from(home: Option<OsString>, userprofile: Option<OsString>) -> Option<PathBuf> {
    non_empty_path(home).or_else(|| non_empty_path(userprofile))
}

fn resolve_effective_home(
    explicit: Option<&Path>,
    env_home: Option<OsString>,
    env_userprofile: Option<OsString>,
) -> Option<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| resolve_home_dir_from(env_home, env_userprofile))
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
        Some("antigravity" | "antigravity-ide") => "desktop",
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

/// Resolves all AGY CLI, Desktop, custom, and isolated ACP data roots from explicit inputs,
/// deduplicating canonical/identical paths while preserving first-seen source priority
/// (`"cli"` before `"desktop"`).
pub fn resolve_agy_roots_from(
    home: Option<&Path>,
    antigravity_data_dir: Option<&OsStr>,
    antigravity_cli_home: Option<&OsStr>,
    agy_home: Option<&OsStr>,
    antigravity_home: Option<&OsStr>,
) -> Vec<(&'static str, PathBuf)> {
    let mut roots: Vec<(&'static str, PathBuf)> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut has_explicit_cli_home = false;

    for env_val in [antigravity_cli_home, agy_home, antigravity_home] {
        if let Some(path) = expand_tilde_os(env_val, home) {
            has_explicit_cli_home = true;
            push_unique_root(&mut roots, &mut seen, "cli", path);
        }
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
        push_unique_root(
            &mut roots,
            &mut seen,
            "desktop",
            gemini.join("antigravity-ide"),
        );
        push_unique_root(
            &mut roots,
            &mut seen,
            "cli",
            gemini.join("antigravity-backup"),
        );
        push_unique_root(
            &mut roots,
            &mut seen,
            "desktop",
            h.join(".config").join("antigravity"),
        );

        // Bounded single-level checks for isolated ACP roots without following symlinks.
        let t3_base = h
            .join(".t3")
            .join("userdata")
            .join("providers")
            .join("antigravity");
        if is_real_dir(&t3_base) {
            if let Ok(entries) = fs::read_dir(&t3_base) {
                let mut provider_dirs: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| is_real_dir(p))
                    .collect();
                provider_dirs.sort();
                for dir in provider_dirs {
                    let acp = dir.join("antigravity-acp");
                    if is_real_dir(&acp) {
                        push_unique_root(&mut roots, &mut seen, "cli", acp);
                    }
                }
            }
        }

        let pi_base = h
            .join(".pi")
            .join("agent")
            .join("cache")
            .join("antigravity-acp");
        let pi_agy = pi_base.join("agy");
        if is_real_dir(&pi_base) && is_real_dir(&pi_agy) {
            push_unique_root(&mut roots, &mut seen, "cli", pi_agy);
        }
    }

    roots
}

/// Returns the parent directory prefixes under `home` that host ephemeral isolated ACP roots
/// (T3 and pi), including canonicalized variants so previously archived roots remain matched
/// after their temporary subdirectories are removed from disk.
pub(crate) fn ephemeral_acp_root_prefixes(home: &Path) -> Vec<PathBuf> {
    let mut prefixes = Vec::with_capacity(4);
    let canonical_home = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let rels = [
        Path::new(".t3")
            .join("userdata")
            .join("providers")
            .join("antigravity"),
        Path::new(".pi")
            .join("agent")
            .join("cache")
            .join("antigravity-acp"),
    ];
    for rel in &rels {
        for candidate in [canonical_home.join(rel), home.join(rel)] {
            if let Ok(canon) = fs::canonicalize(&candidate) {
                if !prefixes.contains(&canon) {
                    prefixes.push(canon);
                }
            }
            if !prefixes.contains(&candidate) {
                prefixes.push(candidate);
            }
        }
    }
    prefixes
}

/// Resolves the effective home directory, never letting ambient `HOME`/`USERPROFILE`
/// preempt an explicit `Some(home)` override.
pub(crate) fn resolve_effective_home_env(home: Option<&Path>) -> Option<PathBuf> {
    resolve_effective_home(home, env::var_os("HOME"), env::var_os("USERPROFILE"))
}

/// Resolves all AGY data roots for `home` (falling back to `HOME`/`USERPROFILE` only when
/// `home` is `None`) together with `ANTIGRAVITY_CLI_HOME`, `AGY_HOME`, `ANTIGRAVITY_HOME`,
/// and `ANTIGRAVITY_DATA_DIR`.
pub fn resolve_agy_roots(home: Option<&Path>) -> Vec<(&'static str, PathBuf)> {
    let resolved_home = resolve_effective_home_env(home);
    let data_dir = env::var_os("ANTIGRAVITY_DATA_DIR");
    let cli_home = env::var_os("ANTIGRAVITY_CLI_HOME");
    let agy_home = env::var_os("AGY_HOME");
    let ag_home = env::var_os("ANTIGRAVITY_HOME");
    resolve_agy_roots_from(
        resolved_home.as_deref(),
        data_dir.as_deref(),
        cli_home.as_deref(),
        agy_home.as_deref(),
        ag_home.as_deref(),
    )
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

fn xdg_data_dir_from(xdg_data_home: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(xdg) = non_empty_path(xdg_data_home) {
        return Some(xdg.join("agy-statusline"));
    }
    home.map(|h| h.join(".local").join("share").join("agy-statusline"))
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

/// Resolves `$XDG_DATA_HOME/agy-statusline` or `<home>/.local/share/agy-statusline`.
pub fn xdg_data_dir() -> Option<PathBuf> {
    xdg_data_dir_from(env::var_os("XDG_DATA_HOME"), resolve_home_dir())
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
    fn resolves_xdg_config_cache_and_data_dirs() {
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
            xdg_cache_dir_from(os(""), home.clone()),
            Some(PathBuf::from("/home/alice/.cache/agy-statusline"))
        );
        assert_eq!(xdg_cache_dir_from(None, None), None);

        assert_eq!(
            xdg_data_dir_from(os("/xdg/data"), home.clone()),
            Some(PathBuf::from("/xdg/data/agy-statusline"))
        );
        assert_eq!(
            xdg_data_dir_from(os(""), home),
            Some(PathBuf::from("/home/alice/.local/share/agy-statusline"))
        );
        assert_eq!(xdg_data_dir_from(None, None), None);
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
    fn resolve_effective_home_never_lets_ambient_env_preempt_explicit_home() {
        let explicit = Path::new("/isolated/home");
        assert_eq!(
            resolve_effective_home(Some(explicit), os("/ambient/home"), os("/ambient/user")),
            Some(PathBuf::from("/isolated/home"))
        );
        assert_eq!(
            resolve_effective_home(None, os("/ambient/home"), os("/ambient/user")),
            Some(PathBuf::from("/ambient/home"))
        );
        assert_eq!(
            resolve_effective_home(None, os(""), os("/ambient/user")),
            Some(PathBuf::from("/ambient/user"))
        );
        assert_eq!(resolve_effective_home(None, None, None), None);
    }

    #[test]
    fn resolve_agy_roots_from_expands_env_deduplicates_and_discovers_multi_roots() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        let data_cli = home.join("extra-data");
        let data_desk = home.join("extra").join("antigravity");
        let data_ide = home.join("extra").join("antigravity-ide");
        let t3_b = home
            .join(".t3")
            .join("userdata")
            .join("providers")
            .join("antigravity")
            .join("proj-b")
            .join("antigravity-acp");
        let t3_a = home
            .join(".t3")
            .join("userdata")
            .join("providers")
            .join("antigravity")
            .join("proj-a")
            .join("antigravity-acp");
        let t3_no_acp = home
            .join(".t3")
            .join("userdata")
            .join("providers")
            .join("antigravity")
            .join("proj-empty");
        let pi_agy = home
            .join(".pi")
            .join("agent")
            .join("cache")
            .join("antigravity-acp")
            .join("agy");

        fs::create_dir_all(&data_cli).unwrap();
        fs::create_dir_all(&data_desk).unwrap();
        fs::create_dir_all(&data_ide).unwrap();
        fs::create_dir_all(&t3_b).unwrap();
        fs::create_dir_all(&t3_a).unwrap();
        fs::create_dir_all(&t3_no_acp).unwrap();
        fs::create_dir_all(&pi_agy).unwrap();

        let data_dir_val = OsStr::new(
            " ~/extra-data , , ~/extra/antigravity , ~/extra/antigravity-ide , ~/extra-data ",
        );
        let roots = resolve_agy_roots_from(Some(home), Some(data_dir_val), None, None, None);
        assert_eq!(
            roots,
            vec![
                ("cli", data_cli.clone()),
                ("desktop", data_desk.clone()),
                ("desktop", data_ide.clone()),
                ("cli", home.join(".gemini").join("antigravity-cli")),
                ("desktop", home.join(".gemini").join("antigravity")),
                ("desktop", home.join(".gemini").join("antigravity-ide")),
                ("cli", home.join(".gemini").join("antigravity-backup")),
                ("desktop", home.join(".config").join("antigravity")),
                ("cli", t3_a.clone()),
                ("cli", t3_b.clone()),
                ("cli", pi_agy.clone()),
            ]
        );

        // Explicit CLI env vars (including ANTIGRAVITY_HOME) replace the default
        // `.gemini/antigravity-cli` and win `"cli"` priority over matching desktop data dirs.
        let shared_ag = home.join("shared").join("antigravity");
        let ag_home = home.join("ag-home");
        fs::create_dir_all(&shared_ag).unwrap();
        fs::create_dir_all(&ag_home).unwrap();

        let explicit_roots = resolve_agy_roots_from(
            Some(home),
            Some(OsStr::new("~/shared/antigravity")),
            Some(shared_ag.as_os_str()),
            None,
            Some(OsStr::new("~/ag-home")),
        );
        assert_eq!(explicit_roots[0], ("cli", shared_ag));
        assert_eq!(explicit_roots[1], ("cli", ag_home));
        assert!(!explicit_roots
            .iter()
            .any(|(_, p)| p == &home.join(".gemini").join("antigravity-cli")));
        assert!(explicit_roots.contains(&("desktop", home.join(".gemini").join("antigravity"))));
        assert!(explicit_roots.contains(&("desktop", home.join(".gemini").join("antigravity-ide"))));
        assert!(explicit_roots.contains(&("cli", home.join(".gemini").join("antigravity-backup"))));
        assert!(explicit_roots.contains(&("desktop", home.join(".config").join("antigravity"))));
        assert!(explicit_roots.contains(&("cli", t3_a)));
        assert!(explicit_roots.contains(&("cli", t3_b)));
        assert!(explicit_roots.contains(&("cli", pi_agy)));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_agy_roots_from_ignores_symlinked_isolated_dirs_and_deduplicates_canonical_targets() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let external = dir.path().join("external");
        let ext_provider = external.join("ext-provider");
        let ext_acp = ext_provider.join("antigravity-acp");
        let ext_pi_agy = external.join("ext-pi-agy");
        fs::create_dir_all(&ext_acp).unwrap();
        fs::create_dir_all(&ext_pi_agy).unwrap();

        // Symlinked `.gemini/antigravity` pointing to `.gemini/antigravity-cli` deduplicates
        // to the canonical target while preserving `"cli"` source priority.
        let cli_dir = home.join(".gemini").join("antigravity-cli");
        let desk_dir = home.join(".gemini").join("antigravity");
        fs::create_dir_all(&cli_dir).unwrap();
        symlink(&cli_dir, &desk_dir).unwrap();

        // Symlinked T3 provider subdir and symlinked `antigravity-acp` leaf are both ignored.
        let t3_base = home
            .join(".t3")
            .join("userdata")
            .join("providers")
            .join("antigravity");
        let real_provider = t3_base.join("real-provider");
        fs::create_dir_all(&real_provider).unwrap();
        symlink(&ext_provider, t3_base.join("sym-provider")).unwrap();
        symlink(&ext_acp, real_provider.join("antigravity-acp")).unwrap();

        // Symlinked pi `agy` leaf is ignored.
        let pi_base = home
            .join(".pi")
            .join("agent")
            .join("cache")
            .join("antigravity-acp");
        fs::create_dir_all(&pi_base).unwrap();
        symlink(&ext_pi_agy, pi_base.join("agy")).unwrap();

        let roots = resolve_agy_roots_from(Some(home), None, None, None, None);
        assert!(roots.contains(&("cli", cli_dir)));
        assert!(!roots.iter().any(|(_, p)| p == &desk_dir));
        assert!(!roots.iter().any(|(_, p)| p.starts_with(&t3_base)));
        assert!(!roots.iter().any(|(_, p)| p.starts_with(&pi_base)));
    }
}
