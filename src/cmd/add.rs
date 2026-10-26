//! `fast-worktree add`: `git worktree add` (or a spare from the pool), the
//! `.worktreeinclude` files, then `install`, in one command.

use super::install;
use crate::ecosystem::{self, dart, Mode, Project};
use crate::error::{bail, Result};
use crate::hooks::Config;
use crate::util::{errln, outln};
use crate::{git, include, paths, pending, pool, trash, util};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct Opts {
    pub path: PathBuf,
    pub branch: String,
    pub start_point: Option<String>,
    /// How long the fetch of origin's default branch may take.
    pub fetch_limit: Duration,
}

pub fn run(opts: Opts) -> Result<i32> {
    let here = Path::new(".");
    let start = match opts.start_point {
        Some(s) => s,
        None => fetched_start(here, opts.fetch_limit)?,
    };
    let branch = Branch::New {
        name: opts.branch,
        start,
    };
    let ok = create(here, &opts.path, &branch, false)?;
    Ok(if ok { 0 } else { 1 })
}

/// Where a new branch starts without `<start-point>`: the remote's default
/// branch as it is now, not whatever local branch happens to be checked
/// out. The fetch never prompts and is stopped after `limit`, as in `hook
/// create` and `pool fill`; when it fails, the branch starts from
/// `origin/<default>` as last fetched, and only when there is none is that
/// an error.
fn fetched_start(here: &Path, limit: Duration) -> Result<String> {
    let base = git::origin_default_branch(here)?;
    let start = format!("origin/{base}");
    if let Err(e) = git::fetch_quietly(here, &base, Some(limit)) {
        let r = format!("refs/remotes/{start}");
        if !git::succeeds(here, &["rev-parse", "--verify", "--quiet", &r]) {
            bail!("{e}, and there is no {start} to start from");
        }
        util::warn(&format!(
            "fast-worktree: {e}; starting from {start} as last fetched\n"
        ));
    }
    Ok(start)
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
///
/// When the pool is on, a refill starts at the end, whether a spare was
/// taken or not; it waits for the background install, if there is one.
pub fn create(here: &Path, path: &Path, branch: &Branch, defer: bool) -> Result<bool> {
    let start = Instant::now();
    let main = install::main_checkout(here)?;
    // Relative to the current directory, also for the git commands the
    // pool runs in the main checkout.
    let path = &std::env::current_dir()?.join(path);
    let pool_on = pool::size(&main).0 > 0;
    let spare = if pool_on {
        from_pool(&main, path, branch)
    } else {
        None
    };
    if spare.is_none() {
        let p = path.to_string_lossy().into_owned();
        // `--quiet`: no "Preparing worktree" lines and, above all, no
        // "Updating files: N%" progress, which git prints on a terminal for
        // a large checkout. Errors are still printed.
        let mut args = vec!["worktree", "add", "--quiet", &p];
        match branch {
            Branch::New { name, start } => args.extend(["-b", name, start]),
            Branch::Existing(name) => args.push(name),
        }
        // A post-checkout hook that calls `fast-worktree check
        // --post-checkout` would clone dependencies here before the full
        // install below; it is told to do nothing.
        if let Err(e) = git::run_with_env(here, &args, &[("FAST_WORKTREE_SKIP", "1")]) {
            if pool_on {
                pool::spawn_fill(&main, None);
            }
            return Err(e);
        }
        outln!("worktree added in {}", util::elapsed(start));
    } else {
        outln!("worktree from the pool in {}", util::elapsed(start));
    }
    let root = path.canonicalize()?;
    let ok = match prepare(&root, spare.as_ref(), defer) {
        Ok(ok) => ok,
        Err(e) => {
            errln!("fast-worktree: {e}");
            false
        }
    };
    if pool_on {
        pool::spawn_fill(&main, Some(&root));
    }
    Ok(ok)
}

/// A spare taken from the pool, now the worktree.
struct Spare {
    /// Where it was made (see [`pool::Taken::Spare`]).
    made_at: Vec<PathBuf>,
}

/// Take a spare for `path`. Returns it, with the worktree now at `path` on
/// `branch`; `None` when the worktree is still to be made.
fn from_pool(main: &Path, path: &Path, branch: &Branch) -> Option<Spare> {
    if path.symlink_metadata().is_ok() {
        outln!(
            "note   pool: {} exists, so no spare is moved there",
            path.display()
        );
        return None;
    }
    let made_at = match pool::take(main, path) {
        pool::Taken::Spare { made_at } => made_at,
        pool::Taken::Empty => {
            pool::record_miss(main);
            outln!("note   pool: no spare ready, creating the worktree");
            return None;
        }
        pool::Taken::Failed(why) => {
            outln!("note   pool: a spare could not be moved ({why}), creating the worktree");
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
        return Some(Spare { made_at });
    }
    // The worktree is gone again; `create` makes it the usual way.
    outln!("note   pool: git switch failed in the spare, creating the worktree");
    let admin = git::admin_dir(path);
    if trash::trash(path).is_ok() {
        if let Err(e) = admin.and_then(|a| git::unregister(&a)) {
            outln!("note   pool: {e}");
        }
    }
    None
}

/// Everything after the worktree exists. Returns whether it all succeeded.
fn prepare(root: &Path, spare: Option<&Spare>, defer: bool) -> Result<bool> {
    copy_included(root);
    if let Some(s) = spare {
        rewrite_moved(root, &s.made_at);
    }
    let cfg = Config::load(root)?;
    let mode = match (spare.is_some(), defer) {
        (false, false) => Mode::Missing,
        (false, true) => Mode::CloneOnly,
        (true, false) => Mode::Changed,
        (true, true) => Mode::ChangedCloneOnly,
    };
    let mut done = install::install_deps(root, &[], &cfg, mode)?;
    if spare.is_some() {
        let failed = rerun_moved(root, &done.kept, &cfg);
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
                outln!("later  started; `fast-worktree wait` waits for it");
                Ok(done.ok)
            }
            Err(e) => {
                errln!("fast-worktree: cannot start the background install: {e}");
                Ok(false)
            }
        };
    }
    let hooks_ok = install::run_post_add(root, &cfg)?;
    Ok(done.ok && hooks_ok)
}

/// In the files Flutter generated in a spare's Dart projects, replace the
/// path where the spare was made (`made_at`) with `root`.
fn rewrite_moved(root: &Path, made_at: &[PathBuf]) {
    let Ok(projects) = ecosystem::discover(root) else {
        return;
    };
    for p in projects.iter().filter(|p| p.ecosystem.name() == "pub") {
        let project = ecosystem::project_path(root, &p.dir);
        let n = dart::rewrite_moved_paths(&project, made_at, root);
        if n > 0 {
            outln!(
                "paths  {}: {n} generated file(s) pointed to where the spare was made, rewritten",
                p.dir
            );
        }
    }
}

/// A spare's dependencies were installed where the pool made it, and
/// whatever its projects' own install steps wrote by absolute path points
/// there. Run those steps again for the projects among `kept`, here.
/// Returns the projects where that failed.
fn rerun_moved(root: &Path, kept: &[Project], cfg: &Config) -> Vec<Project> {
    let cache_repo = match install::main_checkout(root) {
        Ok(main) => paths::cache_root().join(paths::repo_key(&main)),
        Err(e) => {
            outln!("note   {e}");
            return kept.to_vec();
        }
    };
    let mut failed = Vec::new();
    for p in kept {
        match p
            .ecosystem
            .rerun_own_scripts(root, &p.dir, &cfg.skip_lifecycle, &cache_repo)
        {
            Ok(notes) => {
                for n in notes {
                    outln!("{n}");
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

/// Copy what the main checkout's `.worktreeinclude` selects. A failure is
/// reported and does not stop `add`.
fn copy_included(root: &Path) {
    let main = match install::main_checkout(root) {
        Ok(m) => m,
        Err(e) => {
            outln!("note   {}: {e}", include::FILE);
            return;
        }
    };
    match include::copy(&main, root) {
        Ok(c) => {
            for p in c.copied {
                outln!("copy   {p}");
            }
            for p in c.kept {
                outln!("keep   {p} (already in the worktree)");
            }
        }
        Err(e) => outln!("note   {e}"),
    }
}
