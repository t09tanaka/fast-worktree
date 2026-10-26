//! `fast-worktree remove`: unregister a worktree at once and delete its
//! files in the background.
//!
//! Deleting a worktree with `node_modules` takes seconds per project;
//! renaming it into the trash takes milliseconds. Like `git worktree
//! remove`, it refuses a worktree with uncommitted or untracked changes
//! (ignored files are fine) and a locked one, and also one that has another
//! registered worktree inside it. Only this worktree's entry is removed
//! from git; `git worktree prune` would also drop worktrees that are merely
//! out of sight, such as one on a volume that is not mounted.

use crate::error::{bail, Error, Result};
use crate::hooks::{self, Stage};
use crate::{git, pending, pool, trash, util};
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
    /// Another registered worktree lives below this one (in an ignored
    /// directory, so `status` does not show it).
    pub contains_worktree: bool,
    /// `git status` printed something, or failed: see [`is_dirty`].
    pub dirty: bool,
    pub has_submodules: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NotWorktreeRoot,
    MainCheckout,
    Locked,
    ContainsWorktree,
    Dirty,
    Submodules,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::NotWorktreeRoot => "not a worktree root",
            Refusal::MainCheckout => "refusing to remove the main checkout",
            Refusal::Locked => "worktree is locked",
            Refusal::ContainsWorktree => "another worktree is inside it; remove that one first",
            Refusal::Dirty => "uncommitted or untracked changes",
            Refusal::Submodules => "worktree has submodules; use git worktree remove",
        })
    }
}

/// The arguments of the `git status` that decides [`Facts::dirty`]: every
/// untracked file counts whatever `status.showUntrackedFiles` says, and so
/// does every change in a submodule whatever `diff.ignoreSubmodules` says.
pub const STATUS_ARGS: [&str; 4] = [
    "status",
    "--porcelain",
    "--untracked-files=all",
    "--ignore-submodules=none",
];

/// Dirty when `git status` printed anything, and when it could not run:
/// what cannot be checked is not deleted.
pub fn is_dirty(status: &Result<String>) -> bool {
    status.as_ref().map_or(true, |s| !s.is_empty())
}

/// The first reason not to remove, in the order they are checked.
pub fn refusal(f: &Facts) -> Option<Refusal> {
    if !f.is_worktree_root {
        Some(Refusal::NotWorktreeRoot)
    } else if f.is_main_checkout {
        Some(Refusal::MainCheckout)
    } else if f.locked {
        Some(Refusal::Locked)
    } else if f.contains_worktree {
        Some(Refusal::ContainsWorktree)
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
    let real = |w: &git::Worktree| w.path.canonicalize().unwrap_or_else(|_| w.path.clone());
    let me = worktrees.iter().find(|w| real(w) == path);
    let inner = worktrees
        .iter()
        .map(real)
        .find(|p| p != &path && p.starts_with(&path));
    let facts = Facts {
        is_worktree_root: toplevel == path,
        is_main_checkout: path == main,
        locked: me.is_some_and(|w| w.locked),
        contains_worktree: inner.is_some(),
        dirty: is_dirty(&git::output(&path, &STATUS_ARGS)),
        has_submodules: git::output(&path, &["submodule", "status"])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
    };
    if let Some(r) = refusal(&facts) {
        if r == Refusal::Locked && pool::is_pool_path(&path) {
            bail!(
                "{r}: {} is a spare of the pool; `fast-worktree pool clear` removes the spares",
                path.display()
            );
        }
        match (r, &inner) {
            (Refusal::ContainsWorktree, Some(i)) => {
                bail!("{r}: {} is inside {}", i.display(), path.display())
            }
            _ => bail!("{r}: {}", path.display()),
        }
    }
    let branch = me.and_then(|w| w.branch.clone());
    // Decide about the branch before anything is removed: `git branch -d`
    // failing afterwards would leave the worktree gone and the branch kept.
    if opts.delete_branch {
        if let Some(b) = &branch {
            if let Some(reference) = git::unmerged_into(&main, b) {
                bail!(
                    "branch {b} is not merged into {reference}, so `git branch -d` would keep it; \
                     nothing was removed. Merge it first, or run remove without --delete-branch"
                );
            }
        }
    }
    // Taken while the worktree is in place: after the move, git can no
    // longer tell which entry was this worktree's.
    let admin = git::admin_dir(&path)?;
    if let Err(why) = trash::can_move(&path) {
        bail!(
            "cannot move {} away: {why}; nothing was removed",
            path.display()
        );
    }

    let cfg = hooks::Config::load(&path)?;
    // A background install that `hook create` left still writing into it
    // would bring it back after the move.
    pending::report_stop(&pending::stop(&path, pending::STOP_GRACE));
    let failed = hooks::run(&cfg, Stage::PreRemove, &path, &main);
    if failed > 0 {
        eprintln!("fast-worktree: {failed} pre_remove hook(s) failed; removing anyway");
    }

    if std::env::current_dir().is_ok_and(|d| d.starts_with(&path)) {
        std::env::set_current_dir(&main)?;
    }
    trash::sweep();
    let dest = match trash::trash(&path) {
        Ok(d) => d,
        Err(e) => bail!(
            "the pre_remove hooks ran, but {} could not be moved away ({e}): \
             the worktree is still there and still registered",
            path.display()
        ),
    };
    let registered = git::unregister(&admin);
    println!(
        "removed {} (moved to {}, deleting in background)",
        util::tilde(&path),
        util::tilde(&dest)
    );
    if let Err(e) = registered {
        eprintln!(
            "fast-worktree: the worktree is gone but git still lists it ({e}); \
             `git worktree prune` removes the entry"
        );
        return Ok(1);
    }

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
                    contains_worktree: true,
                    ..ok()
                },
                Refusal::ContainsWorktree,
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
    fn a_status_that_fails_counts_as_dirty() {
        assert!(!is_dirty(&Ok(String::new())));
        assert!(is_dirty(&Ok("?? notes.txt\n".into())));
        assert!(is_dirty(&Err(Error("git status: fatal".into()))));
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
