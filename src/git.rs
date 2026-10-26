//! Thin wrappers around the `git` CLI.

use crate::error::{bail, Error, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn command(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).stdin(Stdio::null());
    c
}

/// Run git in `dir` and return stdout. A non-zero exit becomes an error
/// carrying git's stderr.
pub fn output(dir: &Path, args: &[&str]) -> Result<String> {
    output_with_env(dir, args, &[])
}

/// [`output`] with extra environment variables for git and its hooks.
pub fn output_with_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let out = command(dir)
        .args(args)
        .envs(env.iter().copied())
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
    run_with_env(dir, args, &[])
}

/// [`run`] with extra environment variables for git and its hooks.
pub fn run_with_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<()> {
    let status = command(dir)
        .args(args)
        .envs(env.iter().copied())
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

/// The administrative directory of the linked worktree whose root is
/// `path`: `<common dir>/worktrees/<id>`, where git keeps its `HEAD`,
/// index and the `gitdir` file pointing back at `path/.git`. Taken while
/// the worktree is still in place, for [`unregister`]. An error when `path`
/// is not a linked worktree (the main checkout has none).
pub fn admin_dir(path: &Path) -> Result<PathBuf> {
    let out = output(
        path,
        &[
            "rev-parse",
            "--absolute-git-dir",
            "--path-format=absolute",
            "--git-common-dir",
        ],
    )?;
    let mut lines = out.lines();
    let (Some(git_dir), Some(common)) = (lines.next(), lines.next()) else {
        bail!(
            "git rev-parse printed no git directory for {}",
            path.display()
        );
    };
    let real = |p: &str| {
        let p = PathBuf::from(p);
        p.canonicalize().unwrap_or(p)
    };
    let (admin, common) = (real(git_dir), real(common));
    let gitdir_file = fs::read_to_string(admin.join("gitdir")).unwrap_or_default();
    let points_back = Path::new(gitdir_file.trim_end_matches('\n'))
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .is_some_and(|p| p == path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    if admin.parent() != Some(common.join("worktrees").as_path()) || !points_back {
        bail!(
            "{}: cannot find its entry under {}",
            path.display(),
            common.join("worktrees").display()
        );
    }
    Ok(admin)
}

/// Unregister one worktree whose files have been moved away, by deleting
/// its administrative directory (from [`admin_dir`]). This is what `git
/// worktree prune` does for that entry, without touching the others: a
/// worktree on a volume that is not mounted right now also looks missing to
/// `prune`, and must keep its registration.
pub fn unregister(admin: &Path) -> Result<()> {
    let is_entry = admin
        .parent()
        .is_some_and(|p| p.file_name().is_some_and(|n| n == "worktrees"))
        && admin.join("gitdir").is_file();
    if !is_entry {
        bail!("{}: not a worktree entry; left alone", admin.display());
    }
    fs::remove_dir_all(admin).map_err(|e| Error(format!("{}: {e}", admin.display())))
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

/// Whether `git branch -d <branch>`, run in `dir`, would find the branch
/// merged: into its upstream when it has one that exists, else into `HEAD`.
/// Returns the reference it was compared with when it is not merged, and
/// `None` when it is merged or git cannot tell (then `git branch -d` itself
/// decides).
pub fn unmerged_into(dir: &Path, branch: &str) -> Option<String> {
    // `<branch>@{upstream}` resolves only when the upstream ref exists,
    // which is also when `git branch -d` compares with it.
    let upstream = output(
        dir,
        &[
            "rev-parse",
            "--symbolic-full-name",
            &format!("{branch}@{{upstream}}"),
        ],
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| s.starts_with("refs/"));
    let reference = upstream.unwrap_or_else(|| "HEAD".to_string());
    let status = command(dir)
        .args([
            "merge-base",
            "--is-ancestor",
            &format!("refs/heads/{branch}"),
            &reference,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    let short = reference
        .strip_prefix("refs/remotes/")
        .or_else(|| reference.strip_prefix("refs/heads/"))
        .unwrap_or(&reference)
        .to_string();
    (status.code() == Some(1)).then_some(short)
}

/// The administrative directory of the worktree registered at `path`, for
/// when `path` is gone or broken and [`admin_dir`] cannot ask git there:
/// the one entry of `<common dir>/worktrees/*` whose `gitdir` file points
/// at `path/.git` and that `git worktree list`, run in `dir` (any checkout
/// of the repository), lists at `path`. `None` unless exactly one matches.
pub fn find_admin(dir: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let common = output(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common.trim_end_matches('\n'));
    let listed = worktrees(dir)?;
    let same = |a: &Path| comparable(a) == comparable(path);
    if !listed.iter().any(|w| same(&w.path)) {
        return Ok(None);
    }
    let mut found = Vec::new();
    for e in fs::read_dir(common.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let admin = e.path();
        let Ok(text) = fs::read_to_string(admin.join("gitdir")) else {
            continue;
        };
        let gitdir = admin.join(text.trim_end_matches('\n'));
        if gitdir.file_name().is_some_and(|n| n == ".git") && gitdir.parent().is_some_and(same) {
            found.push(admin);
        }
    }
    Ok(match found.len() {
        1 => found.pop(),
        _ => None,
    })
}

/// `path` with symlinks resolved as far as it exists: a path that is gone
/// is its parent's real path plus its name (`/tmp` and `/private/tmp` are
/// one place on macOS).
fn comparable(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => comparable(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// Whether `git` in `dir` exits 0, with its output discarded.
pub fn succeeds(dir: &Path, args: &[&str]) -> bool {
    command(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether the local branch exists.
pub fn branch_exists(dir: &Path, branch: &str) -> bool {
    succeeds(
        dir,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

/// The ssh command for a fetch that must not wait for a password, a
/// passphrase or a host key confirmation: the one git would use
/// (`GIT_SSH_COMMAND`, else `core.sshCommand`, else `ssh`) with `-o
/// BatchMode=yes` added. `None` when only `GIT_SSH` names the program: it
/// takes no options, so it is left as it is.
pub fn batch_ssh_command(
    env_command: Option<&str>,
    config_command: Option<&str>,
    git_ssh: bool,
) -> Option<String> {
    fn nonempty(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|s| !s.is_empty())
    }
    let base = match (nonempty(env_command), nonempty(config_command)) {
        (Some(c), _) | (None, Some(c)) => c.to_string(),
        (None, None) if git_ssh => return None,
        (None, None) => "ssh".to_string(),
    };
    Some(format!("{base} -o BatchMode=yes"))
}

/// The environment for a fetch that never prompts: `GIT_TERMINAL_PROMPT=0`
/// and [`batch_ssh_command`].
fn batch_env(dir: &Path) -> Vec<(&'static str, String)> {
    let var = |k: &str| std::env::var(k).ok();
    let config = output(dir, &["config", "--get", "core.sshCommand"]).ok();
    let mut env = vec![("GIT_TERMINAL_PROMPT", "0".to_string())];
    if let Some(c) = batch_ssh_command(
        var("GIT_SSH_COMMAND").as_deref(),
        config.as_deref(),
        var("GIT_SSH").is_some_and(|s| !s.is_empty()),
    ) {
        env.push(("GIT_SSH_COMMAND", c));
    }
    env
}

/// `git fetch -q origin <branch>` that never waits for a password, a
/// passphrase or a host key confirmation ([`batch_env`]): it fails
/// instead. With `limit`, a fetch still running after that long is stopped
/// and counts as failed.
pub fn fetch_quietly(dir: &Path, branch: &str, limit: Option<Duration>) -> Result<()> {
    let args = ["fetch", "-q", "origin", branch];
    let env = batch_env(dir);
    let Some(limit) = limit else {
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        return output_with_env(dir, &args, &env).map(|_| ());
    };
    let mut child = command(dir)
        .args(args)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error(format!("cannot run git: {e}")))?;
    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(s) if s.success() => return Ok(()),
            Some(_) => bail!("git fetch origin {branch} failed"),
            None if start.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "git fetch origin {branch} took longer than {}s",
                    limit.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
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
    fn fetches_use_the_users_ssh_command_in_batch_mode() {
        let b = |e, c, s| batch_ssh_command(e, c, s);
        assert_eq!(
            b(None, None, false).as_deref(),
            Some("ssh -o BatchMode=yes")
        );
        assert_eq!(
            b(None, Some("ssh -i k\n"), false).as_deref(),
            Some("ssh -i k -o BatchMode=yes")
        );
        assert_eq!(
            b(Some("ssh -p 2"), Some("ssh -i k"), true).as_deref(),
            Some("ssh -p 2 -o BatchMode=yes"),
            "the environment wins, as in git"
        );
        assert_eq!(
            b(None, Some("ssh -i k"), true).as_deref(),
            Some("ssh -i k -o BatchMode=yes"),
            "core.sshCommand wins over GIT_SSH, as in git"
        );
        assert_eq!(b(Some(" "), None, true), None, "GIT_SSH takes no options");
    }

    #[test]
    fn a_missing_path_compares_by_its_real_parent() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().canonicalize().unwrap();
        assert_eq!(comparable(&t.path().join("gone/x")), real.join("gone/x"));
        assert_eq!(comparable(t.path()), real);
    }

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
