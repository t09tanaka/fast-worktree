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
//!
//! The background process holds an exclusive `flock` on a lock file there
//! while it runs, and leads a session and process group of its own, so that
//! `hook remove` can tell that it is still running and stop it together
//! with everything it started ([`stop`]).

use crate::error::Result;
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MARKER: &str = "fast-worktree-installing";
const LOG: &str = "fast-worktree-install.log";
const RESULT: &str = "fast-worktree-install.result";
const LOCK: &str = "fast-worktree-install.lock";
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
    decide(
        || fs::read_to_string(dir.join(MARKER)).ok(),
        || fs::read_to_string(dir.join(RESULT)).ok(),
        now_secs(),
    )
}

/// The state from the marker and the result file, each read when asked.
/// The background process writes its result and then removes the marker
/// before it exits, so a marker whose process is gone may only mean that it
/// finished after the marker was read: the marker is read again, and when it
/// is gone, the result decides.
fn decide(
    mut marker: impl FnMut() -> Option<String>,
    result: impl FnOnce() -> Option<String>,
    now: u64,
) -> State {
    if let Some(text) = marker() {
        let s = marker_state(&text, now);
        if s != State::Interrupted || marker().is_some() {
            return s;
        }
    }
    match result() {
        Some(r) => State::Done(r.trim() == "ok"),
        None => State::None,
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
        .stderr(log);
    detach(&mut c);
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

/// Start the child in a session of its own (`setsid`): it leads a new
/// process group, so that a signal to the group reaches everything it
/// starts, and it has no controlling terminal, so that nothing it runs
/// stops on reading one (`SIGTTIN`) or waits at a prompt.
pub fn detach(c: &mut Command) {
    // SAFETY: setsid is async-signal-safe; nothing else runs between fork
    // and exec.
    unsafe {
        c.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// What an attempt at the lock found.
enum Flock {
    /// Taken: nobody else holds it. It goes when the file is closed.
    Taken(#[allow(dead_code)] fs::File),
    /// Another process holds it (`EWOULDBLOCK`).
    Held,
    /// It could not be tried (the file cannot be opened, or the file
    /// system has no locks: `ENOLCK`, `ENOTSUP`...), so it says nothing.
    Unknown(io::Error),
}

/// An exclusive `flock` on `path`, without waiting.
fn try_flock(path: &Path) -> Flock {
    let f = match fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => return Flock::Unknown(e),
    };
    // SAFETY: a valid open descriptor; the lock goes when it is closed.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Flock::Taken(f);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Flock::Held
    } else {
        Flock::Unknown(e)
    }
}

/// Run the background install: hold the lock, put this process in the
/// marker, run `f`, record how it went and remove the marker. Returns `f`'s
/// exit code.
pub fn run_deferred(root: &Path, f: impl FnOnce() -> Result<i32>) -> i32 {
    let dir = git_dir(root);
    // Taken before the pid goes into the marker: whoever finds a pid there
    // also finds the lock held for as long as this process lives.
    let _lock = dir.as_ref().map(|d| try_flock(&d.join(LOCK)));
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

/// How long [`stop`] waits for a background install that was started but
/// has not written its pid yet.
const PID_WAIT: Duration = Duration::from_secs(5);

/// How long `remove` and `hook remove` give a background install to stop.
pub const STOP_GRACE: Duration = Duration::from_secs(3);

/// What [`stop`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// No background install was running.
    NotRunning,
    /// One was running, and it is gone now.
    Stopped,
    /// The marker names a process, but the lock could not be tried, so
    /// nothing tells whether that process is the install: it was left
    /// alone. Says why.
    Unknown(String),
}

/// Stop the background install of `root`, if one is running, together with
/// everything it started: `SIGTERM` to its process group, then `SIGKILL`
/// to what is left after `grace`. Returns once the group is gone.
///
/// The pid in the marker is trusted only when the lock is held by another
/// process: the marker of an install that was killed may name a pid that
/// is someone else's now. When the lock cannot be tried at all, nothing is
/// signalled; it waits up to `grace` for the marker to go away instead.
pub fn stop(root: &Path, grace: Duration) -> Stop {
    let Some(dir) = git_dir(root) else {
        return Stop::NotRunning;
    };
    let start = Instant::now();
    let pid = loop {
        let Ok(text) = fs::read_to_string(dir.join(MARKER)) else {
            return Stop::NotRunning;
        };
        if let Some(pid) = marker_pid(&text) {
            break pid;
        }
        if marker_state(&text, now_secs()) != State::Running || start.elapsed() > PID_WAIT {
            return Stop::NotRunning;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    match try_flock(&dir.join(LOCK)) {
        Flock::Taken(_) => Stop::NotRunning,
        Flock::Held => {
            signal_group(pid, libc::SIGTERM);
            if !group_gone_within(pid, grace) {
                signal_group(pid, libc::SIGKILL);
                group_gone_within(pid, Duration::from_secs(2));
            }
            Stop::Stopped
        }
        Flock::Unknown(e) => {
            let start = Instant::now();
            while start.elapsed() < grace {
                if !dir.join(MARKER).exists() {
                    return Stop::NotRunning;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Stop::Unknown(format!(
                "cannot check the lock of the background install (pid {pid}): {e}; \
                 it was not stopped"
            ))
        }
    }
}

/// Print what [`stop`] did, for `remove` and `hook remove`.
pub fn report_stop(s: &Stop) {
    match s {
        Stop::NotRunning => {}
        Stop::Stopped => eprintln!("fast-worktree: stopped the background install"),
        Stop::Unknown(why) => eprintln!("fast-worktree: warning: {why}"),
    }
}

/// The pid in a marker, `<unix seconds> <pid>`.
fn marker_pid(text: &str) -> Option<i32> {
    text.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|p| *p > 1)
}

fn signal_group(pgid: i32, sig: i32) {
    // SAFETY: sends a signal; no memory is involved.
    unsafe {
        libc::kill(-pgid, sig);
    }
}

fn group_gone_within(pgid: i32, limit: Duration) -> bool {
    let start = Instant::now();
    loop {
        // SAFETY: signal 0 only checks that the group has members.
        let rc = unsafe { libc::kill(-pgid, 0) };
        if rc != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        if start.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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
    fn a_detached_child_leads_its_own_session() {
        let mut c = Command::new("sleep");
        c.arg("5");
        detach(&mut c);
        let mut child = c.spawn().unwrap();
        let pid = child.id() as i32;
        // SAFETY: plain queries about processes.
        let (theirs, ours) = unsafe { (libc::getsid(pid), libc::getsid(0)) };
        let err = io::Error::last_os_error().raw_os_error();
        let _ = child.kill();
        let _ = child.wait();
        assert_ne!(theirs, ours, "still in our session");
        // Some systems do not tell the session of a process in another one
        // (EPERM): that, too, says it is not ours.
        assert!(theirs == pid || err == Some(libc::EPERM), "{theirs} {pid}");
    }

    #[test]
    fn stop_trusts_a_pid_only_while_the_lock_is_held() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let dir = root.join(".git");
        fs::create_dir(&dir).unwrap();
        assert_eq!(
            stop(root, Duration::from_secs(1)),
            Stop::NotRunning,
            "no marker"
        );
        // A live process that is not ours, named by a stale marker.
        let mut other = Command::new("sleep").arg("5").spawn().unwrap();
        fs::write(dir.join(MARKER), format!("1 {}\n", other.id())).unwrap();
        assert_eq!(stop(root, Duration::from_secs(1)), Stop::NotRunning);
        assert!(other.try_wait().unwrap().is_none(), "left alone");
        let _ = other.kill();
        let _ = other.wait();
        assert_eq!(marker_pid("1 42\n"), Some(42));
        assert_eq!(marker_pid("1\n"), None);
        assert_eq!(marker_pid("1 1\n"), None);
    }

    #[test]
    fn stop_kills_nothing_when_the_lock_cannot_be_checked() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let dir = root.join(".git");
        fs::create_dir(&dir).unwrap();
        // A lock that cannot be opened says nothing about the pid.
        fs::create_dir(dir.join(LOCK)).unwrap();
        // In a group of its own, as the background install is: a signal
        // to the group would reach it.
        let mut c = Command::new("sleep");
        c.arg("5");
        detach(&mut c);
        let mut other = c.spawn().unwrap();
        fs::write(dir.join(MARKER), format!("1 {}\n", other.id())).unwrap();
        let s = stop(root, Duration::from_millis(200));
        assert!(matches!(s, Stop::Unknown(_)), "{s:?}");
        let alive = other.try_wait().unwrap().is_none();
        let _ = other.kill();
        let _ = other.wait();
        assert!(alive, "killed a process the lock did not vouch for");
    }

    #[test]
    fn a_marker_that_goes_away_while_its_pid_is_checked_is_a_finished_install() {
        // The process wrote its result, removed the marker and exited
        // between the read of the marker and the check of its pid.
        let dead = {
            let mut c = Command::new("true").spawn().unwrap();
            c.wait().unwrap();
            c.id()
        };
        let mut reads = vec![None, Some(format!("1 {dead}\n"))];
        let s = decide(|| reads.pop().flatten(), || Some("ok\n".into()), 2);
        assert_eq!(s, State::Done(true));
        // Still there on the second read: it was interrupted.
        let marker = format!("1 {dead}\n");
        let s = decide(|| Some(marker.clone()), || Some("ok\n".into()), 2);
        assert_eq!(s, State::Interrupted);
        // Gone, and no result either: nothing to wait for.
        let mut reads = vec![None, Some(marker.clone())];
        assert_eq!(decide(|| reads.pop().flatten(), || None, 2), State::None);
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
