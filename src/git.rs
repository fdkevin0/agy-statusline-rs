//! Git worktree detection and working-tree diff line statistics.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Git metadata probed from the active workspace directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitInfo {
    /// Linked worktree name when inside `git worktree` (e.g. `"feat-auth"`), `None` for main repo.
    pub worktree: Option<String>,
    /// Inserted lines in working tree vs `HEAD`.
    pub added: u64,
    /// Deleted lines in working tree vs `HEAD`.
    pub deleted: u64,
}

/// Trait abstracting Git probes for deterministic offline testing.
pub trait GitProbe {
    /// Probes worktree name and (optionally) diff line stats for `dir`.
    fn probe(&self, dir: &Path, include_diff: bool) -> GitInfo;
}

/// Production [`GitProbe`] using pure-Rust `.git` worktree detection and `git diff --numstat`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGitProbe;

impl GitProbe for SystemGitProbe {
    fn probe(&self, dir: &Path, include_diff: bool) -> GitInfo {
        let worktree = detect_worktree_name(dir);
        let (added, deleted) = if include_diff {
            read_git_diff_stats(dir).unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        GitInfo {
            worktree,
            added,
            deleted,
        }
    }
}

/// Detects whether `dir` (or an ancestor) is a linked `git worktree` by inspecting `.git`.
///
/// A linked worktree has a `.git` regular file containing `gitdir: .../worktrees/<name>`.
pub fn detect_worktree_name(dir: &Path) -> Option<String> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let dot_git = d.join(".git");
        if dot_git.is_dir() {
            return None;
        }
        if dot_git.is_file() {
            let content = fs::read_to_string(&dot_git).ok()?;
            let gitdir = content.trim().strip_prefix("gitdir:")?.trim();
            let path = PathBuf::from(gitdir);
            let mut comps = path.components().rev();
            let wt_name = comps.next()?.as_os_str().to_str()?.to_string();
            let parent = comps.next()?.as_os_str().to_str()?;
            if parent == "worktrees" && !wt_name.is_empty() {
                return Some(wt_name);
            }
            return None;
        }
        cur = d.parent();
    }
    None
}

fn read_git_diff_stats(dir: &Path) -> Option<(u64, u64)> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["diff", "--numstat", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut added = 0u64;
    let mut deleted = 0u64;
    for line in text.lines() {
        let mut parts = line.split('\t');
        let a = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let d = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        added = added.saturating_add(a);
        deleted = deleted.saturating_add(d);
    }
    Some((added, deleted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_linked_worktree_from_dot_git_file() {
        let dir = std::env::temp_dir().join(format!("agy_wt_test_{}", std::process::id()));
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
    fn main_worktree_dot_git_dir_returns_none() {
        let dir = std::env::temp_dir().join(format!("agy_wt_main_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();

        assert_eq!(detect_worktree_name(&dir), None);

        let _ = fs::remove_dir_all(&dir);
    }
}
