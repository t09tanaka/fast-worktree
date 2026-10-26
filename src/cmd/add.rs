//! `fast-worktree add`: `git worktree add` (or a spare from the pool), the
//! `.worktreeinclude` files, then `install`, in one command.

use super::install;
use crate::ecosystem::{self, Mode, Project};
use crate::error::Result;
use crate::hooks::Config;
use crate::{git, include, pending, pool, trash, util};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Opts {
    pub path: PathBuf,
    pub branch: String,
    pub start_point: Option<String>,
}

pub fn run(opts: Opts) -> Result<i32> {
    let here = Path::new(".");
    let start = match opts.start_point {
        Some(s) => s,
        None => {
            // Start from the remote's default branch as it is now, not from
            // whatever local branch happens to be checked out.
            let base = git::origin_default_branch(here)?;
            git::run(here, &["fetch", "-q", "origin", &base])?;
            format!("origin/{base}")
        }
    };
    let branch = Branch::New {
        name: opts.branch,
        start,
    };
    let ok = create(here, &opts.path, &branch, false)?;
    Ok(if ok { 0 } else { 1 })
}

/// The branch the new worktree is on.
pub enum Branch {
    /// `-b <name> <start>`.
    New { name: String, start: String },
    /// An existing local branch, checked out as it is.
    Existing(String),
}

/// Create the worktree at `path`, then copy the `.worktreeinclude` files,
/// install and run the `post_add` hooks. `here` is any directory of the
/// repository. Only creating the worktree itself can fail with an error;
/// what fails after that is printed, and the result is `false`. `true`
/// means everything succeeded or was left to the background.
///
/// With `defer`, nothing that downloads or builds runs here: what a clone
/// cannot provide is installed by a detached `install --changed`, which
/// then runs the `post_add` hooks (see [`pending`]).
pub fn create(here: &Path, path: &Path, branch: &Branch, defer: bool) -> Result<bool> {
    let start = Instant::now();
    let main = install::main_checkout(here)?;
    // Relative to the current directory, also for the git commands the
    // pool runs in the main checkout.
    let path = &std::env::current_dir()?.join(path);
    let spare = from_pool(&main, path, branch);
    if spare.is_none() {
        let p = path.to_string_lossy().into_owned();
        let mut args = vec!["worktree", "add", &p];
        match branch {
            Branch::New { name, start } => args.extend(["-b", name, start]),
            Branch::Existing(name) => args.push(name),
        }
        // A post-checkout hook that calls `fast-worktree check
        // --post-checkout` would clone dependencies here before the full
        // install below; it is told to do nothing.
        git::run_with_env(here, &args, &[("FAST_WORKTREE_SKIP", "1")])?;
        println!("worktree added in {}", util::elapsed(start));
    } else {
        println!("worktree from the pool in {}", util::elapsed(start));
    }
    let root = path.canonicalize()?;
    Ok(match prepare(&root, spare.as_deref(), defer) {
        Ok(ok) => ok,
        Err(e) => {
            eprintln!("fast-worktree: {e}");
            false
        }
    })
}

/// Take a spare for `path` when the pool is on, and start refilling it.
/// Returns the commit the spare was made at, with the worktree now at
/// `path` on `branch`; `None` when the worktree is still to be made.
fn from_pool(main: &Path, path: &Path, branch: &Branch) -> Option<String> {
    let (size, _) = pool::size(main);
    if size == 0 {
        return None;
    }
    if path.symlink_metadata().is_ok() {
        println!(
            "note   pool: {} exists, so no spare is moved there",
            path.display()
        );
        return None;
    }
    let taken = pool::take(main, path);
    pool::spawn_fill(main);
    let commit = match taken {
        pool::Taken::Spare { commit } => commit,
        pool::Taken::Empty => {
            pool::record_miss(main);
            println!("note   pool: no spare ready, creating the worktree");
            return None;
        }
        pool::Taken::Failed(why) => {
            println!("note   pool: a spare could not be moved ({why}), creating the worktree");
            return None;
        }
    };
    let p = path.to_string_lossy();
    let mut args = vec!["-C", &p, "switch", "-q"];
    match branch {
        Branch::New { name, start } => args.extend(["-c", name, start]),
        Branch::Existing(name) => args.push(name),
    }
    let switched = std::process::Command::new("git")
        .args(&args)
        .env("FAST_WORKTREE_SKIP", "1")
        .stdin(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if switched {
        return Some(commit);
    }
    // The worktree is gone again; `create` makes it the usual way.
    println!("note   pool: git switch failed in the spare, creating the worktree");
    let _ = trash::trash(path);
    let _ = git::output(main, &["worktree", "prune"]);
    None
}

/// Everything after the worktree exists. Returns whether it all succeeded.
fn prepare(root: &Path, spare: Option<&str>, defer: bool) -> Result<bool> {
    copy_included(root);
    let cfg = Config::load(root)?;
    let mode = match (spare.is_some(), defer) {
        (false, false) => Mode::Missing,
        (false, true) => Mode::CloneOnly,
        (true, false) => Mode::Changed,
        (true, true) => Mode::ChangedCloneOnly,
    };
    let mut done = install::install_deps(root, &[], &cfg, mode)?;
    if let Some(commit) = spare {
        let failed = rerun_changed(root, commit, &done.kept, &cfg);
        done.ok &= failed.is_empty();
        if defer {
            done.missing.extend(failed);
        }
    }
    if !done.missing.is_empty() {
        let dirs: Vec<String> = done.missing.iter().map(|p| p.dir.clone()).collect();
        // The background install runs the post_add hooks when it is done.
        return match pending::spawn(root, &dirs) {
            Ok(()) => {
                println!("later  started; `fast-worktree wait` waits for it");
                Ok(done.ok)
            }
            Err(e) => {
                eprintln!("fast-worktree: cannot start the background install: {e}");
                Ok(false)
            }
        };
    }
    let hooks_ok = install::run_post_add(root, &cfg)?;
    Ok(done.ok && hooks_ok)
}

/// A spare's dependencies were installed at `commit`. Run the own install
/// scripts again for the projects among `kept` whose tracked files changed
/// since. Returns the projects where that failed.
fn rerun_changed(root: &Path, commit: &str, kept: &[Project], cfg: &Config) -> Vec<Project> {
    let changed = match git::output(root, &["diff", "--name-only", "-z", commit, "HEAD"]) {
        Ok(s) => s,
        Err(e) => {
            println!("note   pool: cannot list the files changed since the spare: {e}");
            return Vec::new();
        }
    };
    let files: Vec<&str> = changed.split('\0').filter(|f| !f.is_empty()).collect();
    // A file belongs to the deepest project of all, kept or not.
    let all = ecosystem::discover(root).unwrap_or_default();
    let dirs: Vec<&str> = all.iter().map(|p| p.dir.as_str()).collect();
    let mut failed = Vec::new();
    for p in kept {
        if !files
            .iter()
            .any(|f| owner(f, &dirs) == Some(p.dir.as_str()))
        {
            continue;
        }
        match p
            .ecosystem
            .rerun_own_scripts(root, &p.dir, &cfg.skip_lifecycle)
        {
            Ok(notes) => {
                for n in notes {
                    println!("{n}");
                }
            }
            Err(log) => {
                install::print_result(p, &Err::<crate::ecosystem::Report, _>(log));
                failed.push(p.clone());
            }
        }
    }
    failed
}

/// The project directory among `dirs` that `file` belongs to: the deepest
/// one containing it, `.` for the rest.
fn owner<'a>(file: &str, dirs: &[&'a str]) -> Option<&'a str> {
    dirs.iter()
        .copied()
        .filter(|d| *d == "." || file.starts_with(&format!("{d}/")))
        .max_by_key(|d| if *d == "." { 0 } else { d.len() + 1 })
}

/// Copy what the main checkout's `.worktreeinclude` selects. A failure is
/// reported and does not stop `add`.
fn copy_included(root: &Path) {
    let main = match install::main_checkout(root) {
        Ok(m) => m,
        Err(e) => {
            println!("note   {}: {e}", include::FILE);
            return;
        }
    };
    match include::copy(&main, root) {
        Ok(c) => {
            for p in c.copied {
                println!("copy   {p}");
            }
            for p in c.kept {
                println!("keep   {p} (already in the worktree)");
            }
        }
        Err(e) => println!("note   {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_belongs_to_the_deepest_project() {
        let dirs = [".", "web", "web/admin", "api"];
        assert_eq!(owner("web/a.js", &dirs), Some("web"));
        assert_eq!(owner("web/admin/x", &dirs), Some("web/admin"));
        assert_eq!(owner("webby/x", &dirs), Some("."));
        assert_eq!(owner("README.md", &dirs), Some("."));
        assert_eq!(owner("README.md", &["api"]), None);
        assert_eq!(owner("api/x", &["api"]), Some("api"));
    }
}
