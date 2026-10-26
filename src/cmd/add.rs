//! `fast-worktree add`: `git worktree add`, then `install`, in one command.

use super::install;
use crate::error::Result;
use crate::{git, util};
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
    git::run(
        here,
        &["worktree", "add", &path, "-b", &opts.branch, &start_point],
    )?;
    println!("worktree added in {}", util::elapsed(start));
    let root = opts.path.canonicalize()?;
    let deps_ok = install::install_deps(&root, &[])?;
    let hooks_ok = install::run_post_add(&root)?;
    Ok(if deps_ok && hooks_ok { 0 } else { 1 })
}
