//! `fast-worktree hook create|remove|print-config` for Claude Code's
//! `WorktreeCreate` and `WorktreeRemove` hooks, and `fast-worktree wait`.
//!
//! `hook create` reads `{"name": ..., "cwd": ...}` on stdin, creates the
//! worktree where and as Claude Code would (`<main>/.claude/worktrees/<name>`
//! on branch `worktree-<name>`, from origin's default branch), and prints
//! its path as the last line of stdout; everything else goes to stderr. It
//! fails only when the worktree cannot be created: dependencies that fail
//! are warnings. A hook holds up the session, so what a clone cannot
//! provide is installed in the background ([`pending`]).
//!
//! `hook remove` reads `{"worktree_path": ...}` and always deletes the
//! directory when it is a linked worktree of the repository: Claude Code
//! asked the user first, or was told to discard the changes. It refuses,
//! before anything changes, a worktree the user locked, one that cannot be
//! moved, and one whose submodule git directories cannot go with it. A
//! background install still running in it is stopped first. A worktree
//! with changes, with submodules or with another worktree inside is kept in
//! the trash for a day; a `HEAD` that would be lost is kept in
//! `refs/fast-worktree/kept/`. Only its own entry is removed from git. The
//! branch `worktree-<name>` is deleted only when its commit is on another
//! ref.

use super::{add, install, remove};
use crate::error::{bail, Error, Result};
use crate::hooks::{self, Config, Stage};
use crate::{git, pending, pool, submodules, trash, util};
use serde_json::Value;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long `hook create` lets the fetch of the default branch take (it
/// holds up the session; Claude Code caps its own fetch at 5 seconds).
const FETCH_LIMIT: Duration = Duration::from_secs(5);

/// How long `hook remove` keeps a worktree that had changes.
pub const KEEP_SECS: u64 = 24 * 3_600;

/// Send everything this process (and its children) writes to stdout to
/// stderr instead. Returns the original stdout.
fn stdout_to_stderr() -> Result<File> {
    io::stdout().flush()?;
    // SAFETY: plain descriptor duplication; the duplicate is owned by the
    // returned File.
    unsafe {
        // Close-on-exec: the background processes started later must not
        // hold the hook's stdout open, or Claude Code waits for them.
        let saved = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 0);
        if saved < 0 || libc::dup2(2, 1) < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(File::from_raw_fd(saved))
    }
}

fn read_input() -> Result<Value> {
    let mut s = String::new();
    io::stdin().read_to_string(&mut s)?;
    serde_json::from_str(&s).map_err(|e| Error(format!("hook input is not JSON: {e}")))
}

fn field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A worktree name as a single path component.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\0')
        && !name.starts_with('-')
}

/// `hook create`.
pub fn create() -> i32 {
    let mut out = match stdout_to_stderr() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("fast-worktree: {e}");
            return 1;
        }
    };
    match create_worktree() {
        Ok(path) => {
            let _ = io::stdout().flush();
            if writeln!(out, "{}", path.display()).is_err() {
                return 1;
            }
            0
        }
        Err(e) => {
            eprintln!("fast-worktree: {e}");
            1
        }
    }
}

fn create_worktree() -> Result<PathBuf> {
    let input = read_input()?;
    let Some(name) = field(&input, "name") else {
        bail!("hook input has no `name`");
    };
    if !valid_name(name) {
        bail!("`{name}` is not a usable worktree name");
    }
    let cwd = match field(&input, "cwd") {
        Some(c) => PathBuf::from(c),
        None => std::env::current_dir()?,
    };
    let main = install::main_checkout(&git::toplevel(&cwd)?)?;
    std::env::set_current_dir(&main)?;
    // Claude Code refuses a path through a symlink inside the repository.
    let parent = main.join(".claude/worktrees");
    for p in [main.join(".claude"), parent.clone()] {
        if p.symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            bail!(
                "{} is a symlink; not creating a worktree through it",
                p.display()
            );
        }
    }
    let path = parent.join(name);
    if path.symlink_metadata().is_ok() {
        // `claude --worktree <name>` with a name in use opens that worktree.
        if is_linked_worktree(&main, &path) {
            eprintln!("fast-worktree: {} exists, using it", util::tilde(&path));
            return Ok(path);
        }
        bail!(
            "{} exists and is not a worktree of this repository",
            path.display()
        );
    }
    std::fs::create_dir_all(&parent)?;
    let branch_name = format!("worktree-{name}");
    let branch = if git::branch_exists(&main, &branch_name) {
        eprintln!("fast-worktree: branch {branch_name} exists, checking it out");
        add::Branch::Existing(branch_name)
    } else {
        add::Branch::New {
            name: branch_name,
            start: fresh_start(&main),
        }
    };
    if !add::create(&main, &path, &branch, true)? {
        eprintln!(
            "fast-worktree: warning: the worktree was created, but setting it up failed (see above); \
             `fast-worktree install` in it tries again"
        );
    }
    Ok(path)
}

/// Where a new branch starts, as Claude Code does it: origin's default
/// branch, fetched; the cached ref when the fetch fails; `HEAD` without an
/// origin.
fn fresh_start(main: &Path) -> String {
    match git::origin_default_branch(main) {
        Ok(base) => {
            if let Err(e) = git::fetch_quietly(main, &base, Some(FETCH_LIMIT)) {
                eprintln!("fast-worktree: fetch failed, using origin/{base} as last fetched: {e}");
            }
            format!("origin/{base}")
        }
        Err(_) => {
            eprintln!("fast-worktree: no origin default branch, starting from HEAD");
            "HEAD".to_string()
        }
    }
}

fn is_linked_worktree(main: &Path, path: &Path) -> bool {
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    real != main
        && git::worktrees(main).is_ok_and(|w| {
            w.iter()
                .any(|w| w.path.canonicalize().is_ok_and(|p| p == real))
        })
}

/// `hook remove`.
pub fn remove() -> i32 {
    if let Err(e) = stdout_to_stderr() {
        eprintln!("fast-worktree: {e}");
        return 1;
    }
    match remove_worktree() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("fast-worktree: {e}");
            1
        }
    }
}

fn remove_worktree() -> Result<()> {
    let input = read_input()?;
    let Some(given) = field(&input, "worktree_path") else {
        bail!("hook input has no `worktree_path`");
    };
    let given = PathBuf::from(given);
    if given.symlink_metadata().is_err() {
        eprintln!("fast-worktree: {} is already gone", given.display());
        let dir = match field(&input, "cwd") {
            Some(c) => PathBuf::from(c),
            None => std::env::current_dir()?,
        };
        forget(&dir, &given);
        return Ok(());
    }
    let path = given
        .canonicalize()
        .map_err(|e| Error(format!("{}: {e}", given.display())))?;
    let top = git::toplevel(&path).map_err(|_| {
        Error(format!(
            "{}: not a git worktree; not removing it",
            path.display()
        ))
    })?;
    if top.canonicalize().unwrap_or(top) != path {
        bail!("{}: not a worktree root; not removing it", path.display());
    }
    let worktrees = git::worktrees(&path)?;
    let Some(first) = worktrees.first() else {
        bail!("no worktrees registered");
    };
    let main = first
        .path
        .canonicalize()
        .unwrap_or_else(|_| first.path.clone());
    if path == main {
        bail!("{}: refusing to remove the main checkout", path.display());
    }
    let real = |w: &git::Worktree| w.path.canonicalize().unwrap_or_else(|_| w.path.clone());
    let Some(me) = worktrees.iter().find(|w| real(w) == path) else {
        bail!(
            "{}: not a registered worktree; not removing it",
            path.display()
        );
    };
    if pool::is_pool_path(&path) {
        bail!(
            "{}: a spare of the pool; `fast-worktree pool clear` removes spares",
            path.display()
        );
    }

    // Everything that can refuse comes before anything that changes a
    // thing, the pre_remove hooks included. The entry is taken while the
    // worktree is in place: after the move git can no longer tell which
    // one it was.
    let admin = git::admin_dir(&path)?;
    if let Ok(reason) = std::fs::read_to_string(admin.join("locked")) {
        let reason = reason.trim();
        if reason != pool::LOCK_REASON {
            let why = if reason.is_empty() {
                String::new()
            } else {
                format!(" ({reason})")
            };
            bail!(
                "{} is locked{why}; nothing was removed. `git worktree unlock` it first",
                path.display()
            );
        }
    }
    if let Err(why) = trash::can_move(&path) {
        bail!(
            "cannot move {} away: {why}; nothing was removed",
            path.display()
        );
    }
    let subs =
        submodules::plan(&path, &admin).map_err(|e| Error(format!("{e}; nothing was removed")))?;
    let inner: Vec<PathBuf> = worktrees
        .iter()
        .map(real)
        .filter(|p| p != &path && p.starts_with(&path))
        .collect();

    // A background install still writing into it would bring it back.
    if pending::stop(&path, STOP_GRACE) {
        eprintln!("fast-worktree: stopped the background install");
    }
    let head = git::output(&path, &["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .ok();
    let dirty = remove::is_dirty(&git::output(&path, &remove::STATUS_ARGS));
    let keep = dirty || !subs.is_empty() || !inner.is_empty();

    let cfg = Config::load(&path).unwrap_or_else(|e| {
        eprintln!("fast-worktree: {e}");
        Config::default()
    });
    let failed = hooks::run(&cfg, Stage::PreRemove, &path, &main);
    if failed > 0 {
        eprintln!("fast-worktree: {failed} pre_remove hook(s) failed; removing anyway");
    }
    if std::env::current_dir().is_ok_and(|d| d.starts_with(&path)) {
        std::env::set_current_dir(&main)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A commit only HEAD knows of would go with the entry (and to gc).
    let detached_alone =
        me.branch.is_none() && head.as_deref().is_some_and(|h| !on_any_ref(&main, h));
    let kept_ref = match &head {
        Some(h) if keep || detached_alone => keep_head(&main, &name, h),
        _ => None,
    };
    submodules::embed(&subs).map_err(|e| {
        Error(format!(
            "{e}; the worktree is still there and still registered"
        ))
    })?;
    trash::sweep();
    let moved = if keep {
        trash::keep(&path, KEEP_SECS)
    } else {
        trash::trash(&path)
    };
    let dest = moved.map_err(|e| {
        Error(format!(
            "the pre_remove hooks ran, but {} could not be moved away ({e}): \
             the worktree is still there and still registered",
            path.display()
        ))
    })?;
    // Its lock, if the pool left one, goes with the entry.
    let unregistered = git::unregister(&admin);
    eprintln!(
        "removed {} (moved to {})",
        util::tilde(&path),
        util::tilde(&dest)
    );
    if let Err(e) = unregistered {
        eprintln!("fast-worktree: git still lists it ({e})");
    }
    for i in &inner {
        let now = dest.join(i.strip_prefix(&path).unwrap_or(i));
        let _ = git::output(&main, &["worktree", "repair", &now.to_string_lossy()]);
        eprintln!(
            "fast-worktree: the worktree {} was inside it; it is in {} now, \
             deleted with it after 24 hours unless moved: git worktree move {} <new path>",
            util::tilde(i),
            util::tilde(&now),
            sh_path(&now)
        );
    }

    let own = format!("worktree-{name}");
    let kept_branch = if git::branch_exists(&main, &own) {
        settle_branch(&main, &own)
    } else {
        None
    };
    if keep {
        let from = kept_branch
            .clone()
            .or_else(|| me.branch.clone().filter(|b| git::branch_exists(&main, b)))
            .or(kept_ref.clone())
            .or(head.clone())
            .unwrap_or_else(|| "HEAD".into());
        let why = if dirty {
            "it had uncommitted changes"
        } else if !subs.is_empty() {
            "it has submodules"
        } else {
            "another worktree was inside it"
        };
        eprintln!(
            "fast-worktree: {why}; it is kept in {} for at least 24 hours",
            util::tilde(&dest)
        );
        if !subs.is_empty() {
            eprintln!(
                "fast-worktree: its submodules keep their git directories inside them \
                 (`git submodule absorbgitdirs` moves them back after a restore)"
            );
        }
        eprintln!(
            "to restore them: {}",
            restore_command(&main, &path, &from, &dest)
        );
    }
    if let (Some(r), Some(h)) = (&kept_ref, &head) {
        eprintln!(
            "fast-worktree: its HEAD {} is kept as {r} (`git update-ref -d {r}` drops it)",
            &h[..h.len().min(12)]
        );
    }
    if let Some(b) = me.branch.as_ref().filter(|b| **b != own) {
        eprintln!("fast-worktree: kept branch {b} (not created by `hook create`)");
    }
    Ok(())
}

/// How long `hook remove` gives a background install to stop.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// The registration of a worktree whose directory is already gone: drop
/// that entry, and only that one, when it can be told apart.
fn forget(dir: &Path, path: &Path) {
    match git::find_admin(dir, path) {
        Ok(Some(admin)) if admin.join("locked").exists() => {
            eprintln!("fast-worktree: its entry is locked; left registered");
        }
        Ok(Some(admin)) => match git::unregister(&admin) {
            Ok(()) => eprintln!("fast-worktree: unregistered it"),
            Err(e) => eprintln!("fast-worktree: {e}"),
        },
        Ok(None) => eprintln!(
            "fast-worktree: git lists no single worktree at {}; nothing unregistered",
            path.display()
        ),
        Err(e) => eprintln!("fast-worktree: {e}; nothing unregistered"),
    }
}

/// Keep `head` in `refs/fast-worktree/kept/<name>-<UTC date and time>`, so
/// that neither gc nor the removal of the entry loses it. Returns the ref.
fn keep_head(main: &Path, name: &str, head: &str) -> Option<String> {
    let base = format!("{KEPT_REFS}{name}-{}", compact_now());
    let base = if git::succeeds(main, &["check-ref-format", &base]) {
        base
    } else {
        format!("{KEPT_REFS}worktree-{}", compact_now())
    };
    for n in 1..10 {
        let r = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        // The empty old value: only when the ref does not exist yet.
        if git::succeeds(main, &["update-ref", &r, head, ""]) {
            return Some(r);
        }
    }
    eprintln!("fast-worktree: could not keep HEAD {head} in a ref");
    None
}

const KEPT_REFS: &str = "refs/fast-worktree/kept/";

/// `YYYYMMDD-HHMMSS` (UTC), for a ref name.
fn compact_now() -> String {
    let now = std::time::SystemTime::now();
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!(
        "{}-{:02}{:02}{:02}",
        crate::cache::date(now).replace('-', ""),
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

/// The command that puts a kept worktree back: a worktree at `path` on
/// `from` again, then the kept files over it. Only the top `.git` (the
/// file pointing at the old entry) is left out: a submodule's `.git` is its
/// git directory.
fn restore_command(main: &Path, path: &Path, from: &str, kept: &Path) -> String {
    format!(
        "git -C {} worktree add {} {} && rsync -a --exclude=/.git {}/ {}/",
        sh_path(main),
        sh_path(path),
        sh(from),
        sh_path(kept),
        sh_path(path)
    )
}

/// Delete `branch` when its commit is on its upstream, a remote branch or
/// the default branch. Returns the branch when it was kept.
fn settle_branch(main: &Path, branch: &str) -> Option<String> {
    let Ok(sha) = git::output(main, &["rev-parse", &format!("refs/heads/{branch}")]) else {
        return Some(branch.to_string());
    };
    let sha = sha.trim();
    if reachable_elsewhere(main, branch, sha) {
        match git::output(main, &["branch", "-D", branch]) {
            Ok(_) => {
                eprintln!("deleted branch {branch}");
                None
            }
            Err(e) => {
                eprintln!("fast-worktree: kept branch {branch}: {e}");
                Some(branch.to_string())
            }
        }
    } else {
        eprintln!(
            "fast-worktree: kept branch {branch}: commit {} is not on its upstream, a remote branch or the default branch",
            &sha[..sha.len().min(12)]
        );
        Some(branch.to_string())
    }
}

fn reachable_elsewhere(main: &Path, branch: &str, sha: &str) -> bool {
    let contains = |r: &str| git::succeeds(main, &["merge-base", "--is-ancestor", sha, r]);
    let upstream = git::output(
        main,
        &[
            "rev-parse",
            "--symbolic-full-name",
            &format!("refs/heads/{branch}@{{upstream}}"),
        ],
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| s.starts_with("refs/"));
    if upstream.is_some_and(|u| contains(&u)) {
        return true;
    }
    let on_remote = git::output(
        main,
        &[
            "for-each-ref",
            "--contains",
            sha,
            "--format=%(refname)",
            "refs/remotes/",
        ],
    )
    .is_ok_and(|s| !s.trim().is_empty());
    if on_remote {
        return true;
    }
    git::origin_default_branch(main).is_ok_and(|d| contains(&format!("refs/heads/{d}")))
}

fn on_any_ref(main: &Path, sha: &str) -> bool {
    git::output(
        main,
        &["for-each-ref", "--contains", sha, "--format=%(refname)"],
    )
    .is_ok_and(|s| !s.trim().is_empty())
}

/// A path or ref quoted for a POSIX shell when it needs it.
fn sh(p: impl AsRef<std::ffi::OsStr>) -> String {
    let s = p.as_ref().to_string_lossy();
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+@:".contains(c))
    {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// [`sh`] with the home directory written as `~/`, outside the quotes so
/// that the shell still expands it.
fn sh_path(p: &Path) -> String {
    let home = util::home();
    match p.strip_prefix(&home) {
        Ok(rest) if home != Path::new("/") && !rest.as_os_str().is_empty() => {
            format!("~/{}", sh(rest))
        }
        _ => sh(p),
    }
}

/// The hooks to paste into a Claude Code settings file.
pub const CONFIG_JSON: &str = r#"{
  "hooks": {
    "WorktreeCreate": [
      { "hooks": [{ "type": "command", "command": "fast-worktree hook create" }] }
    ],
    "WorktreeRemove": [
      { "hooks": [{ "type": "command", "command": "fast-worktree hook remove" }] }
    ]
  }
}"#;

/// `hook print-config`.
pub fn print_config() -> i32 {
    println!("{CONFIG_JSON}");
    0
}

/// `wait [<path>]`: wait for the background install `hook create` started.
pub fn wait(path: Option<&Path>) -> Result<i32> {
    let root = git::toplevel(path.unwrap_or(Path::new(".")))?;
    let mut told = false;
    loop {
        match pending::state(&root) {
            pending::State::None => {
                println!("nothing is being installed in the background");
                return Ok(0);
            }
            pending::State::Running => {
                if !told {
                    eprintln!(
                        "waiting for the background install in {}",
                        util::tilde(&root)
                    );
                    told = true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            pending::State::Done(true) => {
                println!("background install done");
                return Ok(0);
            }
            pending::State::Done(false) => {
                println!("background install failed:");
                print_log(&root);
                return Ok(1);
            }
            pending::State::Interrupted => {
                pending::clear_marker(&root);
                println!(
                    "the background install stopped before it finished; \
                     `fast-worktree install --changed` finishes it"
                );
                print_log(&root);
                return Ok(1);
            }
        }
    }
}

fn print_log(root: &Path) {
    let Some(p) = pending::log_path(root) else {
        return;
    };
    if let Ok(log) = std::fs::read_to_string(&p) {
        for l in util::tail(&log, 30).lines() {
            println!("  {l}");
        }
    }
    println!("(log: {})", util::tilde(&p));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_one_path_component() {
        for ok in ["feature-auth", "bold-oak-a3f2", "a.b", "pr-12"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "-x", "a\0b"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh("/a/b-c_d.e"), "/a/b-c_d.e");
        assert_eq!(sh("/a b"), "'/a b'");
        assert_eq!(sh("it's"), "'it'\\''s'");
        assert_eq!(sh("~x"), "'~x'");
        let home = util::home();
        if home != Path::new("/") {
            assert_eq!(sh_path(&home.join("a b")), "~/'a b'");
            assert_eq!(sh_path(&home.join("a/b")), "~/a/b");
        }
        assert_eq!(
            sh_path(Path::new("/nonexistent-root/x")),
            "/nonexistent-root/x"
        );
    }

    #[test]
    fn the_restore_command_leaves_out_only_the_top_git_file() {
        let c = restore_command(
            Path::new("/nonexistent-root/repo"),
            Path::new("/nonexistent-root/repo/.claude/worktrees/a b"),
            "refs/fast-worktree/kept/a-20261026-120000",
            Path::new("/nonexistent-root/trash/keep-1-a"),
        );
        assert_eq!(
            c,
            "git -C /nonexistent-root/repo worktree add '/nonexistent-root/repo/.claude/worktrees/a b' \
             refs/fast-worktree/kept/a-20261026-120000 && rsync -a --exclude=/.git \
             /nonexistent-root/trash/keep-1-a/ '/nonexistent-root/repo/.claude/worktrees/a b'/"
        );
    }

    #[test]
    fn kept_refs_are_named_after_the_time() {
        let t = compact_now();
        assert_eq!(t.len(), "20261026-120000".len(), "{t}");
        assert!(t.chars().enumerate().all(|(i, c)| if i == 8 {
            c == '-'
        } else {
            c.is_ascii_digit()
        }));
    }

    #[test]
    fn the_config_has_both_events() {
        let v: Value = serde_json::from_str(CONFIG_JSON).unwrap();
        assert_eq!(
            v["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"],
            "fast-worktree hook create"
        );
        assert_eq!(
            v["hooks"]["WorktreeRemove"][0]["hooks"][0]["type"],
            "command"
        );
    }
}
