//! An install that `hook create` left to a background process.
//!
//! A hook holds up the start of the session, so `hook create` clones what
//! can be cloned and hands the rest (`npm ci`, incremental installs, `pub
//! get`) to a detached `fast-worktree install --changed`. While it runs, a
//! marker says so; when it ends, a result file says how it went. `wait`
//! waits for the marker to go away, `check` mentions it.
//!
//! The files live in the worktree's own git directory
//! (`<repo>/.git/worktrees/<name>/`), not in the worktree: a file there
//! would show up in `git status` and make the worktree look changed. Finding
//! that directory reads the worktree's `.git` file; no git process runs.

use crate::error::Result;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const MARKER: &str = "fast-worktree-installing";
const LOG: &str = "fast-worktree-install.log";
const RESULT: &str = "fast-worktree-install.result";
/// A marker without a pid (the background process has not started yet) is
/// taken for a running install for this long.
const START_GRACE_SECS: u64 = 60;

/// The worktree's git directory: `<root>/.git` itself, or the directory a
/// `.git` file points to.
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    let meta = fs::symlink_metadata(&dot).ok()?;
    if meta.is_dir() {
        return Some(dot);
    }
    let text = fs::read_to_string(&dot).ok()?;
    let p = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    Some(if p.is_absolute() { p } else { root.join(p) })
}

/// Where the background install of `root` writes its output.
pub fn log_path(root: &Path) -> Option<PathBuf> {
    git_dir(root).map(|d| d.join(LOG))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Nothing was started in the background.
    None,
    Running,
    /// The marker is there but its process is gone.
    Interrupted,
    /// Finished: whether it succeeded.
    Done(bool),
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Decide from the marker's text (`<unix seconds> [<pid>]`).
fn marker_state(text: &str, now: u64) -> State {
    let mut parts = text.split_whitespace();
    let started = parts.next().and_then(|s| s.parse::<u64>().ok());
    match parts.next().and_then(|s| s.parse::<i32>().ok()) {
        Some(pid) if alive(pid) => State::Running,
        Some(_) => State::Interrupted,
        None => match started {
            Some(t) if t + START_GRACE_SECS >= now => State::Running,
            _ => State::Interrupted,
        },
    }
}

pub fn state(root: &Path) -> State {
    let Some(dir) = git_dir(root) else {
        return State::None;
    };
    if let Ok(text) = fs::read_to_string(dir.join(MARKER)) {
        return marker_state(&text, now_secs());
    }
    match fs::read_to_string(dir.join(RESULT)) {
        Ok(r) => State::Done(r.trim() == "ok"),
        Err(_) => State::None,
    }
}

/// Forget an interrupted install's marker.
pub fn clear_marker(root: &Path) {
    if let Some(dir) = git_dir(root) {
        let _ = fs::remove_file(dir.join(MARKER));
    }
}

/// Start `fast-worktree install --changed` for `dirs` of `root` in a
/// detached process, with the marker in place before it starts.
pub fn spawn(root: &Path, dirs: &[String]) -> io::Result<()> {
    let dir =
        git_dir(root).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no git directory"))?;
    let _ = fs::remove_file(dir.join(RESULT));
    fs::write(dir.join(MARKER), format!("{}\n", now_secs()))?;
    let log = fs::File::create(dir.join(LOG))?;
    let exe = std::env::current_exe()?;
    let mut c = Command::new(exe);
    c.arg("install")
        .arg("--root")
        .arg(root)
        .args(["--changed", "--deferred"])
        .args(dirs)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0);
    for v in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_PREFIX"] {
        c.env_remove(v);
    }
    match c.spawn() {
        Ok(_) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(dir.join(MARKER));
            Err(e)
        }
    }
}

/// Run the background install: put this process in the marker, run `f`,
/// record how it went and remove the marker. Returns `f`'s exit code.
pub fn run_deferred(root: &Path, f: impl FnOnce() -> Result<i32>) -> i32 {
    let dir = git_dir(root);
    if let Some(d) = &dir {
        let _ = fs::write(
            d.join(MARKER),
            format!("{} {}\n", now_secs(), std::process::id()),
        );
    }
    let code = match f() {
        Ok(c) => c,
        Err(e) => {
            println!("fast-worktree: {e}");
            1
        }
    };
    if let Some(d) = &dir {
        let _ = fs::write(d.join(RESULT), if code == 0 { "ok\n" } else { "failed\n" });
        let _ = fs::remove_file(d.join(MARKER));
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_git_dir_of_a_linked_worktree() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        fs::write(p.join(".git"), "gitdir: /repo/.git/worktrees/x\n").unwrap();
        assert_eq!(git_dir(p), Some(PathBuf::from("/repo/.git/worktrees/x")));
        fs::write(p.join(".git"), "gitdir: ../g\n").unwrap();
        assert_eq!(git_dir(p), Some(p.join("../g")));
        fs::remove_file(p.join(".git")).unwrap();
        assert_eq!(git_dir(p), None);
        fs::create_dir(p.join(".git")).unwrap();
        assert_eq!(git_dir(p), Some(p.join(".git")));
    }

    #[test]
    fn a_marker_is_running_while_its_process_lives() {
        let me = std::process::id();
        assert_eq!(marker_state(&format!("100 {me}"), 200), State::Running);
        // Not started yet: running for a grace period.
        assert_eq!(marker_state("100", 150), State::Running);
        assert_eq!(
            marker_state("100", 100 + START_GRACE_SECS + 1),
            State::Interrupted
        );
        assert_eq!(marker_state("", 0), State::Interrupted);
    }

    #[test]
    fn the_deferred_run_records_its_result() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(state(root), State::None);
        let code = run_deferred(root, || {
            assert_eq!(state(root), State::Running);
            Ok(1)
        });
        assert_eq!(code, 1);
        assert_eq!(state(root), State::Done(false));
        run_deferred(root, || Ok(0));
        assert_eq!(state(root), State::Done(true));
    }
}
