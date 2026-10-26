//! Thin wrappers around the `git` CLI.

use crate::error::{bail, Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn command(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).stdin(Stdio::null());
    c
}

/// Run git in `dir` and return stdout. A non-zero exit becomes an error
/// carrying git's stderr.
pub fn output(dir: &Path, args: &[&str]) -> Result<String> {
    let out = command(dir)
        .args(args)
        .output()
        .map_err(|e| Error(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git {}: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run git in `dir` with inherited stdout/stderr (for commands whose
/// output the user should see, like `git worktree add`).
pub fn run(dir: &Path, args: &[&str]) -> Result<()> {
    let status = command(dir)
        .args(args)
        .status()
        .map_err(|e| Error(format!("cannot run git: {e}")))?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    let s = output(dir, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(s.trim_end_matches('\n')))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Short branch name, `None` for a detached HEAD or a bare entry.
    pub branch: Option<String>,
    pub locked: bool,
}

/// Registered worktrees. The main checkout comes first.
pub fn worktrees(dir: &Path) -> Result<Vec<Worktree>> {
    Ok(parse_worktrees(&output(
        dir,
        &["worktree", "list", "--porcelain"],
    )?))
}

pub fn parse_worktrees(porcelain: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in porcelain.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            out.extend(cur.take());
            cur = Some(Worktree {
                path: PathBuf::from(p),
                branch: None,
                locked: false,
            });
        } else if let Some(w) = cur.as_mut() {
            if let Some(b) = line.strip_prefix("branch ") {
                w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            } else if line == "locked" || line.starts_with("locked ") {
                w.locked = true;
            }
        }
    }
    out.extend(cur);
    out
}

/// Tracked files under `root` matching the pathspecs, relative to `root`.
pub fn tracked(root: &Path, pathspecs: &[&str]) -> Result<Vec<String>> {
    let mut args = vec!["ls-files", "-z", "--"];
    args.extend_from_slice(pathspecs);
    let s = output(root, &args)?;
    Ok(s.split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .collect())
}

/// The remote's default branch, from `refs/remotes/origin/HEAD`. When that
/// ref is not set (a repository that was not cloned, or one whose remote
/// changed its default), ask the remote once with `git remote set-head
/// origin -a` and read it again. Never guesses `main`.
pub fn origin_default_branch(dir: &Path) -> Result<String> {
    let read = || {
        output(
            dir,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
        .ok()
        .and_then(|s| {
            s.trim()
                .strip_prefix("origin/")
                .filter(|b| !b.is_empty())
                .map(str::to_string)
        })
    };
    if let Some(b) = read() {
        return Ok(b);
    }
    output(dir, &["remote", "set-head", "origin", "-a"])
        .map_err(|e| Error(format!("cannot resolve origin's default branch: {e}")))?;
    read().ok_or_else(|| Error("cannot resolve origin's default branch".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worktree_porcelain() {
        let s = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
                 worktree /repo/.worktrees/a\nHEAD def\nbranch refs/heads/feature/a\nlocked\n\n\
                 worktree /repo/.worktrees/b\nHEAD 123\ndetached\nlocked reason here\n\n";
        let w = parse_worktrees(s);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].path, PathBuf::from("/repo"));
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert!(!w[0].locked);
        assert_eq!(w[1].branch.as_deref(), Some("feature/a"));
        assert!(w[1].locked);
        assert_eq!(w[2].branch, None);
        assert!(w[2].locked);
    }
}
