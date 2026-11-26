//! Pure-Rust Git branch/worktree probing and bounded, cached diff statistics.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use crate::sys::fs::{
    atomic_write, is_real_dir, is_safe_cache_target, modified_timestamp,
    read_bounded_file_to_string, read_bounded_regular_file,
};
use crate::sys::process::run_command_stdout_bounded;
use crate::sys::time::{unix_now_secs, SECS_PER_DAY, SECS_PER_HOUR};

const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_millis(250);
const DEFAULT_DIFF_CACHE_TTL_SECS: u64 = 3;
const MAX_GIT_METADATA_FILE_BYTES: u64 = 8 * 1024;
const MAX_DIFF_CACHE_FILE_BYTES: u64 = 256;
const DIFF_CACHE_STALE_SECS: u64 = SECS_PER_DAY as u64;
const DIFF_CACHE_PRUNE_INTERVAL_SECS: u64 = SECS_PER_HOUR;
const MAX_DIFF_CACHE_FILES: usize = 32;
const MAX_DIFF_PRUNE_SCAN: usize = 128;

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
fn resolve_git_repo(dir: &Path) -> Option<(PathBuf, Option<String>)> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let dot_git = d.join(".git");
        if dot_git.is_dir() {
            return Some((dot_git, None));
        }
        if dot_git.is_file() {
            let content =
                read_bounded_file_to_string(&dot_git, MAX_GIT_METADATA_FILE_BYTES).ok()?;
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

/// Reads the current branch or detached `@<short_sha>` directly from `<gitdir>/HEAD`.
fn read_head_branch(gitdir: &Path) -> Option<String> {
    let raw =
        read_bounded_file_to_string(&gitdir.join("HEAD"), MAX_GIT_METADATA_FILE_BYTES).ok()?;
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

fn is_diff_cache_filename(name: &str) -> bool {
    name.len() == 25
        && name.starts_with("diff_")
        && name.ends_with(".txt")
        && name[5..21].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Opportunistically removes stale or excess `diff_*.txt` cache files on cold/hourly misses
/// without scanning the directory on hot cache hits or normal 3s TTL refreshes.
fn prune_stale_diff_caches(cache_dir: &Path, current_file: &str, now_secs: u64) {
    if !is_real_dir(cache_dir) {
        return;
    }
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    let mut active: Vec<(i64, PathBuf)> = Vec::new();
    for entry in entries.flatten().take(MAX_DIFF_PRUNE_SCAN) {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str == current_file || !is_diff_cache_filename(name_str) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let ft = meta.file_type();
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let (mtime_secs, _) = modified_timestamp(&meta);
        if mtime_secs >= 0 && now_secs.saturating_sub(mtime_secs as u64) >= DIFF_CACHE_STALE_SECS {
            let _ = fs::remove_file(&path);
        } else {
            active.push((mtime_secs, path));
        }
    }
    let max_others = MAX_DIFF_CACHE_FILES.saturating_sub(1);
    if active.len() > max_others {
        active.sort_by_key(|(mtime, _)| *mtime);
        let remove_count = active.len() - max_others;
        for (_, path) in active.into_iter().take(remove_count) {
            let _ = fs::remove_file(path);
        }
    }
}

/// Reads or refreshes cached diff stats keyed by repo path, `.git/index` mtime, and `ttl_secs`.
fn probe_diff_cached<F>(
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
    let file_name = format!("diff_{hash:016x}.txt");
    let cache_path = cache_dir.join(&file_name);
    let idx_mtime = index_mtime_nanos(gitdir);

    let mut previous_ts: Option<u64> = None;
    if let Ok((bytes, _)) = read_bounded_regular_file(&cache_path, MAX_DIFF_CACHE_FILE_BYTES) {
        if let Ok(cached) = std::str::from_utf8(&bytes) {
            let mut parts = cached.split_whitespace();
            if let (Some(m), Some(t), Some(first_val)) = (
                parts.next().and_then(|s| s.parse::<u128>().ok()),
                parts.next().and_then(|s| s.parse::<u64>().ok()),
                parts.next(),
            ) {
                previous_ts = Some(t);
                if m == idx_mtime && now_secs.saturating_sub(t) < ttl_secs {
                    if first_val == "timeout" {
                        return (0, 0);
                    }
                    if let (Ok(a), Some(d)) = (
                        first_val.parse::<u64>(),
                        parts.next().and_then(|s| s.parse::<u64>().ok()),
                    ) {
                        return (a, d);
                    }
                }
            }
        }
    }

    let should_prune =
        previous_ts.is_none_or(|t| now_secs.saturating_sub(t) >= DIFF_CACHE_PRUNE_INTERVAL_SECS);
    let (payload, result) = match runner() {
        Some((added, deleted)) => (
            format!("{idx_mtime} {now_secs} {added} {deleted}\n"),
            (added, deleted),
        ),
        None => (format!("{idx_mtime} {now_secs} timeout\n"), (0, 0)),
    };
    if is_safe_cache_target(&cache_path) {
        let _ = atomic_write(&cache_path, payload.as_bytes());
        if should_prune {
            prune_stale_diff_caches(cache_dir, &file_name, now_secs);
        }
    }
    result
}

fn parse_numstat(bytes: &[u8]) -> (u64, u64) {
    let text = String::from_utf8_lossy(bytes);
    let mut added = 0u64;
    let mut deleted = 0u64;
    for line in text.lines() {
        let mut parts = line.split('\t');
        let a = parts
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let d = parts
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        added = added.saturating_add(a);
        deleted = deleted.saturating_add(d);
    }
    (added, deleted)
}

fn run_git_diff_bounded(dir: &Path, timeout: Duration) -> Option<(u64, u64)> {
    let mut cmd = Command::new("git");
    cmd.args(["--no-optional-locks", "-C"])
        .arg(dir)
        .args(["diff", "--numstat", "HEAD"]);
    let out = run_command_stdout_bounded(&mut cmd, timeout, 2 * 1024 * 1024).ok()?;
    if !out.truncated && out.status.is_some_and(|s| s.success()) {
        Some(parse_numstat(&out.stdout))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn detects_linked_worktree_from_dot_git_file() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("nested/sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            dir.path().join(".git"),
            "gitdir: /repo/.git/worktrees/feat-statusline\n",
        )
        .unwrap();

        assert_eq!(
            resolve_git_repo(&sub).and_then(|(_, wt)| wt),
            Some("feat-statusline".to_string())
        );
    }

    #[test]
    fn resolves_relative_worktree_and_head_branch_or_detached_sha() {
        let dir = tempfile::tempdir().unwrap();
        let wt_gitdir = dir.path().join("main_repo/.git/worktrees/wt-rel");
        let wt_dir = dir.path().join("wt_checkout");
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
    }

    #[test]
    fn main_worktree_dot_git_dir_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();

        assert_eq!(resolve_git_repo(dir.path()).and_then(|(_, wt)| wt), None);
    }

    #[test]
    fn probe_diff_cached_respects_ttl_and_index_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let gitdir = root.join(".git");
        let cache_dir = root.join("cache");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(gitdir.join("index"), b"v1").unwrap();

        let calls = Cell::new(0usize);
        let first = probe_diff_cached(root, &gitdir, &cache_dir, 100, 3, || {
            calls.set(calls.get() + 1);
            Some((12, 4))
        });
        assert_eq!(first, (12, 4));
        assert_eq!(calls.get(), 1);

        // Cache hit within TTL does not invoke runner.
        let hit = probe_diff_cached(root, &gitdir, &cache_dir, 102, 3, || {
            calls.set(calls.get() + 1);
            Some((99, 99))
        });
        assert_eq!(hit, (12, 4));
        assert_eq!(calls.get(), 1);

        // TTL expiry refreshes value.
        let expired = probe_diff_cached(root, &gitdir, &cache_dir, 103, 3, || {
            calls.set(calls.get() + 1);
            Some((20, 5))
        });
        assert_eq!(expired, (20, 5));
        assert_eq!(calls.get(), 2);

        // Changing .git/index mtime invalidates cache even within TTL.
        let idx_file = fs::OpenOptions::new()
            .write(true)
            .open(gitdir.join("index"))
            .unwrap();
        idx_file
            .set_modified(UNIX_EPOCH + Duration::from_secs(500))
            .unwrap();
        let invalidated = probe_diff_cached(root, &gitdir, &cache_dir, 104, 3, || {
            calls.set(calls.get() + 1);
            Some((7, 1))
        });
        assert_eq!(invalidated, (7, 1));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn probe_diff_cached_negative_caches_timeout_within_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let gitdir = root.join(".git");
        let cache_dir = root.join("cache");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(gitdir.join("index"), b"v1").unwrap();

        let calls = Cell::new(0usize);
        let timed_out = probe_diff_cached(root, &gitdir, &cache_dir, 200, 3, || {
            calls.set(calls.get() + 1);
            None
        });
        assert_eq!(timed_out, (0, 0));
        assert_eq!(calls.get(), 1);

        // Within TTL, cached failure avoids re-running the probe.
        let cached_fail = probe_diff_cached(root, &gitdir, &cache_dir, 202, 3, || {
            calls.set(calls.get() + 1);
            Some((50, 10))
        });
        assert_eq!(cached_fail, (0, 0));
        assert_eq!(calls.get(), 1);

        // After TTL expiry, probe runs again and succeeds.
        let recovered = probe_diff_cached(root, &gitdir, &cache_dir, 203, 3, || {
            calls.set(calls.get() + 1);
            Some((50, 10))
        });
        assert_eq!(recovered, (50, 10));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn probe_diff_cached_prunes_stale_diff_files_and_preserves_other_cache_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let gitdir = root.join(".git");
        let cache_dir = root.join("cache");
        fs::create_dir_all(&gitdir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(gitdir.join("index"), b"v1").unwrap();

        let now_secs = 200_000u64;
        let stale_diff = cache_dir.join("diff_0000000000000001.txt");
        let fresh_diff = cache_dir.join("diff_0000000000000002.txt");
        let unrelated = cache_dir.join("quota_snapshot.json");
        fs::write(&stale_diff, b"0 100 1 1\n").unwrap();
        fs::write(&fresh_diff, b"0 199990 2 2\n").unwrap();
        fs::write(&unrelated, b"{}").unwrap();

        fs::File::options()
            .write(true)
            .open(&stale_diff)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(100))
            .unwrap();
        fs::File::options()
            .write(true)
            .open(&fresh_diff)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(now_secs - 60))
            .unwrap();
        fs::File::options()
            .write(true)
            .open(&unrelated)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(100))
            .unwrap();

        let got = probe_diff_cached(root, &gitdir, &cache_dir, now_secs, 3, || Some((3, 1)));
        assert_eq!(got, (3, 1));
        assert!(!stale_diff.exists());
        assert!(fresh_diff.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn oversized_dot_git_and_head_files_degrade_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("oversized_wt");
        fs::create_dir_all(&wt_dir).unwrap();
        let oversized_dot_git = format!(
            "gitdir: /repo/.git/worktrees/feat{}\n",
            " ".repeat(MAX_GIT_METADATA_FILE_BYTES as usize)
        );
        fs::write(wt_dir.join(".git"), oversized_dot_git).unwrap();
        assert_eq!(resolve_git_repo(&wt_dir), None);

        let repo_git = dir.path().join("repo/.git");
        fs::create_dir_all(&repo_git).unwrap();
        let oversized_head = format!(
            "ref: refs/heads/main{}\n",
            " ".repeat(MAX_GIT_METADATA_FILE_BYTES as usize)
        );
        fs::write(repo_git.join("HEAD"), oversized_head).unwrap();
        assert_eq!(read_head_branch(&repo_git), None);
    }

    #[cfg(unix)]
    #[test]
    fn probe_diff_cached_skips_writes_into_symlinked_cache_dir_or_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let gitdir = root.join(".git");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(gitdir.join("index"), b"v1").unwrap();

        let real_cache = dir.path().join("real_cache");
        fs::create_dir_all(&real_cache).unwrap();
        let sym_cache = dir.path().join("sym_cache");
        symlink(&real_cache, &sym_cache).unwrap();

        let got = probe_diff_cached(&root, &gitdir, &sym_cache, 100, 3, || Some((4, 2)));
        assert_eq!(got, (4, 2));
        assert_eq!(fs::read_dir(&real_cache).unwrap().count(), 0);

        let hash = fnv1a_64(root.to_string_lossy().as_bytes());
        let file_name = format!("diff_{hash:016x}.txt");
        let sentinel = dir.path().join("sentinel.txt");
        fs::write(&sentinel, b"sentinel").unwrap();
        symlink(&sentinel, real_cache.join(&file_name)).unwrap();

        let got2 = probe_diff_cached(&root, &gitdir, &real_cache, 100, 3, || Some((6, 1)));
        assert_eq!(got2, (6, 1));
        assert_eq!(fs::read(&sentinel).unwrap(), b"sentinel");
    }
}
