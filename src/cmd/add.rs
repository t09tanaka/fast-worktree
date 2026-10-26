//! `fast-worktree add`: `git worktree add`, the `.worktreeinclude` files,
//! then `install`, in one command.

use super::install;
use crate::ecosystem::Mode;
use crate::error::Result;
use crate::hooks::Config;
use crate::{git, include, util};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Opts {
    pub path: PathBuf,
    pub branch: String,
    pub start_point: Option<String>,
}

pub fn run(opts: Opts) -> Result<i32> {
    let here = Path::new(".");
    let start_point = match opts.start_point {
        Some(s) => s,
        None => {
            // Start from the remote's default branch as it is now, not from
            // whatever local branch happens to be checked out.
            let base = git::origin_default_branch(here)?;
            git::run(here, &["fetch", "-q", "origin", &base])?;
            format!("origin/{base}")
        }
    };
    let start = Instant::now();
    let path = opts.path.to_string_lossy().into_owned();
    // A post-checkout hook that calls `fast-worktree check --post-checkout`
    // would clone dependencies here before the full install below; it is
    // told to do nothing.
    git::run_with_env(
        here,
        &["worktree", "add", &path, "-b", &opts.branch, &start_point],
        &[("FAST_WORKTREE_SKIP", "1")],
    )?;
    println!("worktree added in {}", util::elapsed(start));
    let root = opts.path.canonicalize()?;
    copy_included(&root);
    let cfg = Config::load(&root)?;
    let deps_ok = install::install_deps(&root, &[], &cfg, Mode::Missing)?;
    let hooks_ok = install::run_post_add(&root, &cfg)?;
    Ok(if deps_ok && hooks_ok { 0 } else { 1 })
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
