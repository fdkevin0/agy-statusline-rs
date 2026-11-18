//! Pure-Rust Git branch/worktree probing and bounded, cached diff statistics.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};

use crate::timeutil::unix_now_secs;

const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_millis(250);
const DEFAULT_DIFF_CACHE_TTL_SECS: u64 = 3;

/// Git metadata probed from the active workspace directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitInfo {
    /// Branch name or `@<short_sha>` resolved from `.git/HEAD`.
    pub branch: Option<String>,
    /// Linked worktree name when inside `git worktree` (e.g. `"feat-auth"`), `None` for main repo.
    pub worktree: Option<String>,
    /// Inserted lines in working tree vs `HEAD`.
    pub added: u64,
    /// Deleted lines in working tree vs `HEAD`.
    pub deleted: u64,
}

/// Trait abstracting Git probes for deterministic offline testing.
pub trait GitProbe {
    /// Probes branch, worktree name, and (optionally) diff line stats for `dir`.
    fn probe(&self, dir: &Path, include_diff: bool) -> GitInfo;
}

/// Production [`GitProbe`] using pure-Rust `.git/HEAD` + worktree detection and bounded `git diff`.
#[derive(Debug, Clone, Default)]
pub struct SystemGitProbe {
    /// Optional directory for caching `git diff --numstat` results across rapid state ticks.
    pub cache_dir: Option<PathBuf>,
}

impl SystemGitProbe {
    /// Creates a probe configured with the default per-user diff cache directory.
    pub fn with_default_cache() -> Self {
        let base = env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir);
        Self {
            cache_dir: Some(base.join("agy-statusline")),
        }
    }
}

impl GitProbe for SystemGitProbe {
    fn probe(&self, dir: &Path, include_diff: bool) -> GitInfo {
        let Some((gitdir, worktree)) = resolve_git_repo(dir) else {
            return GitInfo::default();
        };
        let branch = read_head_branch(&gitdir);
        let (added, deleted) = if include_diff {
            if let Some(cache_dir) = &self.cache_dir {
                probe_diff_cached(
                    dir,
                    &gitdir,
                    cache_dir,
                    unix_now_secs(),
                    DEFAULT_DIFF_CACHE_TTL_SECS,
                    || run_git_diff_bounded(dir, DEFAULT_GIT_TIMEOUT),
                )
            } else {
                run_git_diff_bounded(dir, DEFAULT_GIT_TIMEOUT).unwrap_or((0, 0))
            }
        } else {
            (0, 0)
        };
        GitInfo {
            branch,
            worktree,
            added,
            deleted,
        }
    }
}

/// Resolves the `.git` directory path and optional linked worktree name for `dir`.
pub fn resolve_git_repo(dir: &Path) -> Option<(PathBuf, Option<String>)> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let dot_git = d.join(".git");
        if dot_git.is_dir() {
            return Some((dot_git, None));
        }
        if dot_git.is_file() {
            let content = fs::read_to_string(&dot_git).ok()?;
            let gitdir_str = content.trim().strip_prefix("gitdir:")?.trim();
            if gitdir_str.is_empty() {
                return None;
            }
            let raw_path = PathBuf::from(gitdir_str);
            let gitdir = if raw_path.is_relative() {
                d.join(raw_path)
            } else {
                raw_path
            };
            let worktree = extract_worktree_name(&gitdir);
            return Some((gitdir, worktree));
        }
        cur = d.parent();
    }
    None
}

fn extract_worktree_name(gitdir: &Path) -> Option<String> {
    let mut comps = gitdir.components().rev();
    let wt_name = comps.next()?.as_os_str().to_str()?.to_string();
    let parent = comps.next()?.as_os_str().to_str()?;
    if parent == "worktrees" && !wt_name.is_empty() {
        Some(wt_name)
    } else {
        None
    }
}

/// Detects whether `dir` (or an ancestor) is a linked `git worktree` by inspecting `.git`.
///
/// A linked worktree has a `.git` regular file containing `gitdir: .../worktrees/<name>`.
pub fn detect_worktree_name(dir: &Path) -> Option<String> {
    resolve_git_repo(dir).and_then(|(_, wt)| wt)
}

/// Reads the current branch or detached `@<short_sha>` directly from `<gitdir>/HEAD`.
pub fn read_head_branch(gitdir: &Path) -> Option<String> {
    let raw = fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = raw.trim();
    if let Some(rest) = head.strip_prefix("ref:") {
        let target = rest.trim();
        let branch = target
            .strip_prefix("refs/heads/")
            .or_else(|| target.strip_prefix("refs/"))
            .unwrap_or(target);
        return (!branch.is_empty()).then(|| branch.to_string());
    }
    if head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(format!("@{}", &head[..7]));
    }
    None
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn index_mtime_nanos(gitdir: &Path) -> u128 {
    fs::metadata(gitdir.join("index"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

/// Reads or refreshes cached diff stats keyed by repo path, `.git/index` mtime, and `ttl_secs`.
pub fn probe_diff_cached<F>(
    dir: &Path,
    gitdir: &Path,
    cache_dir: &Path,
    now_secs: u64,
    ttl_secs: u64,
    runner: F,
) -> (u64, u64)
where
    F: FnOnce() -> Option<(u64, u64)>,
{
    let hash = fnv1a_64(dir.to_string_lossy().as_bytes());
    let cache_path = cache_dir.join(format!("diff_{hash:016x}.txt"));
    let idx_mtime = index_mtime_nanos(gitdir);

    if let Ok(cached) = fs::read_to_string(&cache_path) {
        let mut parts = cached.split_whitespace();
        if let (Some(m), Some(t), Some(a), Some(d)) = (
            parts.next().and_then(|s| s.parse::<u128>().ok()),
            parts.next().and_then(|s| s.parse::<u64>().ok()),
            parts.next().and_then(|s| s.parse::<u64>().ok()),
            parts.next().and_then(|s| s.parse::<u64>().ok()),
        ) {
            if m == idx_mtime && now_secs.saturating_sub(t) < ttl_secs {
                return (a, d);
            }
        }
    }

    let Some((added, deleted)) = runner() else {
        return (0, 0);
    };
    if fs::create_dir_all(cache_dir).is_ok() {
        let tmp = cache_dir.join(format!("diff_{hash:016x}.{}.tmp", std::process::id()));
        let payload = format!("{idx_mtime} {now_secs} {added} {deleted}\n");
        if fs::write(&tmp, payload.as_bytes()).is_ok() {
            let _ = fs::rename(&tmp, &cache_path);
        }
    }
    (added, deleted)
}

fn parse_numstat(bytes: &[u8]) -> (u64, u64) {
    let text = String::from_utf8_lossy(bytes);
    let mut added = 0u64;
    let mut deleted = 0u64;
    for line in text.lines() {
        let mut parts = line.split('\t');
        let a = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let d = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        added = added.saturating_add(a);
        deleted = deleted.saturating_add(d);
    }
    (added, deleted)
}

fn run_git_diff_bounded(dir: &Path, timeout: Duration) -> Option<(u64, u64)> {
    let mut child = Command::new("git")
        .args(["--no-optional-locks", "-C"])
        .arg(dir)
        .args(["diff", "--numstat", "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = stdout.take(2 * 1024 * 1024).read_to_end(&mut buf).map(|_| buf);
        let _ = tx.send(res);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(bytes)) => {
            let status = child.wait().ok()?;
            status.success().then(|| parse_numstat(&bytes))
        }
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn detects_linked_worktree_from_dot_git_file() {
        let dir = env::temp_dir().join(format!("agy_wt_test_{}", std::process::id()));
        let sub = dir.join("nested/sub");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            dir.join(".git"),
            "gitdir: /repo/.git/worktrees/feat-statusline\n",
        )
        .unwrap();

        assert_eq!(
            detect_worktree_name(&sub),
            Some("feat-statusline".to_string())
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_relative_worktree_and_head_branch_or_detached_sha() {
        let dir = env::temp_dir().join(format!("agy_wt_rel_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let wt_gitdir = dir.join("main_repo/.git/worktrees/wt-rel");
        let wt_dir = dir.join("wt_checkout");
        fs::create_dir_all(&wt_gitdir).unwrap();
        fs::create_dir_all(&wt_dir).unwrap();
        fs::write(
            wt_dir.join(".git"),
            "gitdir: ../main_repo/.git/worktrees/wt-rel\n",
        )
        .unwrap();
        fs::write(wt_gitdir.join("HEAD"), "ref: refs/heads/feat/awesome\n").unwrap();

        let (resolved, wt) = resolve_git_repo(&wt_dir).unwrap();
        assert_eq!(wt.as_deref(), Some("wt-rel"));
        assert_eq!(read_head_branch(&resolved).as_deref(), Some("feat/awesome"));

        fs::write(
            wt_gitdir.join("HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(read_head_branch(&resolved).as_deref(), Some("@0123456"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn main_worktree_dot_git_dir_returns_none() {
        let dir = env::temp_dir().join(format!("agy_wt_main_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();

        assert_eq!(detect_worktree_name(&dir), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_diff_cached_respects_ttl_and_index_mtime() {
        let root = env::temp_dir().join(format!("agy_diff_cache_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let gitdir = root.join(".git");
        let cache_dir = root.join("cache");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(gitdir.join("index"), b"v1").unwrap();

        let calls = Cell::new(0usize);
        let first = probe_diff_cached(&root, &gitdir, &cache_dir, 100, 3, || {
            calls.set(calls.get() + 1);
            Some((12, 4))
        });
        assert_eq!(first, (12, 4));
        assert_eq!(calls.get(), 1);

        // Cache hit within TTL does not invoke runner.
        let hit = probe_diff_cached(&root, &gitdir, &cache_dir, 102, 3, || {
            calls.set(calls.get() + 1);
            Some((99, 99))
        });
        assert_eq!(hit, (12, 4));
        assert_eq!(calls.get(), 1);

        // TTL expiry refreshes value.
        let expired = probe_diff_cached(&root, &gitdir, &cache_dir, 103, 3, || {
            calls.set(calls.get() + 1);
            Some((20, 5))
        });
        assert_eq!(expired, (20, 5));
        assert_eq!(calls.get(), 2);

        // Changing .git/index mtime invalidates cache even within TTL.
        let idx_file = fs::OpenOptions::new().write(true).open(gitdir.join("index")).unwrap();
        idx_file
            .set_modified(UNIX_EPOCH + Duration::from_secs(500))
            .unwrap();
        let invalidated = probe_diff_cached(&root, &gitdir, &cache_dir, 104, 3, || {
            calls.set(calls.get() + 1);
            Some((7, 1))
        });
        assert_eq!(invalidated, (7, 1));
        assert_eq!(calls.get(), 3);

        let _ = fs::remove_dir_all(&root);
    }
}
