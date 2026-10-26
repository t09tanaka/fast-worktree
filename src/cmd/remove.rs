//! `fast-worktree remove`: unregister a worktree at once and delete its
//! files in the background.
//!
//! Deleting a worktree with `node_modules` takes seconds per project;
//! renaming it into the trash takes milliseconds. Like `git worktree
//! remove`, it refuses a worktree with uncommitted or untracked changes
//! (ignored files are fine) and a locked one.

use crate::error::{bail, Error, Result};
use crate::hooks::{self, Stage};
use crate::{git, trash, util};
use std::fmt;
use std::path::{Path, PathBuf};

pub struct Opts {
    pub path: PathBuf,
    pub delete_branch: bool,
}

/// What `remove` looks at before deciding.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub is_worktree_root: bool,
    pub is_main_checkout: bool,
    pub locked: bool,
    /// `git status --porcelain` printed something.
    pub dirty: bool,
    pub has_submodules: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NotWorktreeRoot,
    MainCheckout,
    Locked,
    Dirty,
    Submodules,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::NotWorktreeRoot => "not a worktree root",
            Refusal::MainCheckout => "refusing to remove the main checkout",
            Refusal::Locked => "worktree is locked",
            Refusal::Dirty => "uncommitted or untracked changes",
            Refusal::Submodules => "worktree has submodules; use git worktree remove",
        })
    }
}

/// The first reason not to remove, in the order they are checked.
pub fn refusal(f: &Facts) -> Option<Refusal> {
    if !f.is_worktree_root {
        Some(Refusal::NotWorktreeRoot)
    } else if f.is_main_checkout {
        Some(Refusal::MainCheckout)
    } else if f.locked {
        Some(Refusal::Locked)
    } else if f.dirty {
        Some(Refusal::Dirty)
    } else if f.has_submodules {
        Some(Refusal::Submodules)
    } else {
        None
    }
}

pub fn run(opts: Opts) -> Result<i32> {
    let path = opts
        .path
        .canonicalize()
        .map_err(|e| Error(format!("{}: {e}", opts.path.display())))?;
    let toplevel = git::toplevel(&path)?;
    let toplevel = toplevel.canonicalize().unwrap_or(toplevel);
    let worktrees = git::worktrees(&path)?;
    let Some(main) = worktrees.first() else {
        bail!("no worktrees registered");
    };
    let main = main
        .path
        .canonicalize()
        .unwrap_or_else(|_| main.path.clone());
    let me = worktrees
        .iter()
        .find(|w| w.path.canonicalize().is_ok_and(|p| p == path));
    let facts = Facts {
        is_worktree_root: toplevel == path,
        is_main_checkout: path == main,
        locked: me.is_some_and(|w| w.locked),
        dirty: !git::output(&path, &["status", "--porcelain"])?.is_empty(),
        has_submodules: git::output(&path, &["submodule", "status"])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
    };
    if let Some(r) = refusal(&facts) {
        bail!("{r}: {}", path.display());
    }
    let branch = me.and_then(|w| w.branch.clone());

    let cfg = hooks::Config::load(&path)?;
    let failed = hooks::run(&cfg, Stage::PreRemove, &path, &main);
    if failed > 0 {
        eprintln!("fast-worktree: {failed} pre_remove hook(s) failed; removing anyway");
    }

    if std::env::current_dir().is_ok_and(|d| d.starts_with(&path)) {
        std::env::set_current_dir(&main)?;
    }
    trash::sweep();
    let dest = trash::trash(&path)?;
    git::output(&main, &["worktree", "prune"])?;
    println!(
        "removed {} (moved to {}, deleting in background)",
        util::tilde(&path),
        util::tilde(&dest)
    );

    if opts.delete_branch {
        return Ok(delete_branch(&main, branch.as_deref()));
    }
    Ok(0)
}

/// `git branch -d`: a branch with commits that are not merged is kept.
fn delete_branch(main: &Path, branch: Option<&str>) -> i32 {
    let Some(b) = branch else {
        eprintln!("fast-worktree: the worktree had a detached HEAD; no branch to delete");
        return 0;
    };
    match git::output(main, &["branch", "-d", b]) {
        Ok(_) => {
            println!("deleted branch {b}");
            0
        }
        Err(e) => {
            eprintln!("fast-worktree: kept branch {b}: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> Facts {
        Facts {
            is_worktree_root: true,
            ..Facts::default()
        }
    }

    #[test]
    fn a_clean_linked_worktree_may_be_removed() {
        assert_eq!(refusal(&ok()), None);
    }

    #[test]
    fn each_condition_refuses() {
        let cases = [
            (
                Facts {
                    is_worktree_root: false,
                    ..ok()
                },
                Refusal::NotWorktreeRoot,
            ),
            (
                Facts {
                    is_main_checkout: true,
                    ..ok()
                },
                Refusal::MainCheckout,
            ),
            (
                Facts {
                    locked: true,
                    ..ok()
                },
                Refusal::Locked,
            ),
            (
                Facts {
                    dirty: true,
                    ..ok()
                },
                Refusal::Dirty,
            ),
            (
                Facts {
                    has_submodules: true,
                    ..ok()
                },
                Refusal::Submodules,
            ),
        ];
        for (f, want) in cases {
            assert_eq!(refusal(&f), Some(want), "{f:?}");
        }
    }

    #[test]
    fn the_main_checkout_is_refused_before_anything_else_about_it() {
        let f = Facts {
            is_main_checkout: true,
            dirty: true,
            locked: true,
            ..ok()
        };
        assert_eq!(refusal(&f), Some(Refusal::MainCheckout));
        let f = Facts {
            is_worktree_root: false,
            is_main_checkout: true,
            ..ok()
        };
        assert_eq!(refusal(&f), Some(Refusal::NotWorktreeRoot));
    }
}
