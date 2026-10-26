//! `fast-worktree check`: list projects whose dependencies are missing.
//!
//! Meant for a `post-checkout` hook, so it always exits 0. Plain `check`
//! spawns nothing but `git rev-parse` and one `git ls-files`, and stays
//! well under 100 ms.
//!
//! `check --post-checkout <prev> <new> <flag>` takes the hook's arguments.
//! Right after `git worktree add` or `git clone` (`<prev>` is all zeros and
//! `<flag>` is 1) it also clones what the cache or another worktree can
//! provide, but never runs anything that downloads or builds (`npm ci`,
//! `npm install`, `pub get`): those are listed as missing. On any other
//! checkout it only lists. With `CI` set, or `FAST_WORKTREE_SKIP=1`, it does
//! nothing. Its output is best effort: a closed stdout or stderr (a hook
//! whose output is piped into something that already exited) does not
//! change the exit code.

use super::install;
use crate::ecosystem::{target, Mode, Target};
use crate::hooks::Config;
use crate::{git, paths, pending, util};
use std::env;
use std::path::Path;

pub fn run(quiet: bool) -> i32 {
    let Ok(root) = git::toplevel(Path::new(".")) else {
        return 0;
    };
    let cfg = Config::load(&root).unwrap_or_default();
    let Ok(projects) = target::targets(&root, &cfg) else {
        return 0;
    };
    let missing: Vec<String> = missing(&root, &projects)
        .iter()
        .map(|p| p.label())
        .collect();
    if pending::state(&root) == pending::State::Running {
        util::say(&report_pending(&missing));
        return 0;
    }
    util::say(&report(&missing, projects.len(), quiet));
    0
}

/// While `hook create`'s background install runs.
pub fn report_pending(missing: &[String]) -> String {
    let mut s = String::new();
    for m in missing {
        s.push_str(&format!("missing deps: {m}\n"));
    }
    s.push_str("installing deps in the background; `fast-worktree wait` waits for it\n");
    s
}

/// Whether `dir` is in the cache: a spare of the pool being made.
fn in_cache(dir: &std::path::Path) -> bool {
    let root = paths::cache_root();
    let root = root.canonicalize().unwrap_or(root);
    dir.canonicalize().is_ok_and(|d| d.starts_with(&root))
}

fn missing(root: &Path, projects: &[Target]) -> Vec<Target> {
    projects
        .iter()
        .filter(|p| !p.preset.is_installed(root, p))
        .cloned()
        .collect()
}

/// What the `post-checkout` hook should do with its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    Nothing,
    /// A new worktree or clone: clone what can be cloned, list the rest.
    CloneMissing,
    /// Any other checkout: list what is missing.
    List,
}

pub fn hook_action(prev: &str, flag: &str, ci: bool, skip: bool) -> HookAction {
    if ci || skip {
        HookAction::Nothing
    } else if !prev.is_empty() && prev.bytes().all(|b| b == b'0') && flag == "1" {
        HookAction::CloneMissing
    } else {
        HookAction::List
    }
}

pub fn post_checkout(prev: &str, _new: &str, flag: &str) -> i32 {
    let action = hook_action(
        prev,
        flag,
        env::var_os("CI").is_some(),
        env::var("FAST_WORKTREE_SKIP").is_ok_and(|v| v == "1"),
    );
    // A hook inherits git's view of the repository (`git worktree add`
    // exports the new worktree's GIT_DIR); the working directory says it.
    for v in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_PREFIX"] {
        env::remove_var(v);
    }
    // A spare of the pool is installed by `pool fill` itself.
    if env::current_dir().is_ok_and(|d| in_cache(&d)) {
        return 0;
    }
    match action {
        HookAction::Nothing => 0,
        HookAction::List => run(true),
        HookAction::CloneMissing => {
            clone_missing();
            0
        }
    }
}

/// Clone what the cache or another worktree has for the missing projects,
/// and list what is still missing. Errors are printed, never returned.
fn clone_missing() {
    let Ok(root) = git::toplevel(Path::new(".")) else {
        return;
    };
    let cfg = Config::load(&root).unwrap_or_else(|e| {
        util::warn(&format!("fast-worktree: {e}\n"));
        Config::default()
    });
    let projects = match target::targets(&root, &cfg) {
        Ok(p) => p,
        Err(e) => {
            util::warn(&format!("fast-worktree: {e}\n"));
            return;
        }
    };
    let todo = missing(&root, &projects);
    if todo.is_empty() {
        return;
    }
    let results = match install::install_projects(&root, &todo, &cfg, Mode::CloneOnly) {
        Ok(r) => r,
        Err(e) => {
            util::warn(&format!("fast-worktree: {e}\n"));
            util::say(&report(&labels(&todo), projects.len(), true));
            return;
        }
    };
    let mut still = Vec::new();
    for (p, r) in todo.iter().zip(results) {
        match &r {
            Ok(rep) if rep.missing => {
                if !rep.lines.is_empty() {
                    util::say(&format!("{}\n", rep.lines));
                }
                still.push(p.label());
            }
            Ok(_) => {
                install::print_result(p, &r);
            }
            Err(_) => {
                install::print_result(p, &r);
                still.push(p.label());
            }
        }
    }
    util::say(&report(&still, projects.len(), true));
}

fn labels(projects: &[Target]) -> Vec<String> {
    projects.iter().map(Target::label).collect()
}

pub fn report(missing: &[String], total: usize, quiet: bool) -> String {
    if missing.is_empty() {
        return if quiet || total == 0 {
            String::new()
        } else {
            format!("deps ok ({total} projects)\n")
        };
    }
    let mut s = String::new();
    for m in missing {
        s.push_str(&format!("missing deps: {m}\n"));
    }
    s.push_str("run `fast-worktree install` to install them\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_prints_only_when_something_is_missing() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(report(&[], 3, true), "");
        assert_eq!(report(&[], 3, false), "deps ok (3 projects)\n");
        assert_eq!(report(&[], 0, false), "");
        let r = report(&m(&["api (npm)", ". (pub)"]), 3, true);
        assert!(r.starts_with("missing deps: api (npm)\nmissing deps: . (pub)\n"));
        assert_eq!(r, report(&m(&["api (npm)", ". (pub)"]), 3, false));
        let p = report_pending(&m(&["api (npm)"]));
        assert_eq!(
            p,
            "missing deps: api (npm)\ninstalling deps in the background; `fast-worktree wait` waits for it\n"
        );
    }

    #[test]
    fn hook_clones_only_on_a_fresh_checkout() {
        let zeros = "0000000000000000000000000000000000000000";
        let sha = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        assert_eq!(
            hook_action(zeros, "1", false, false),
            HookAction::CloneMissing
        );
        assert_eq!(
            hook_action("0000", "1", false, false),
            HookAction::CloneMissing
        );
        assert_eq!(
            hook_action(sha, "1", false, false),
            HookAction::List,
            "git switch"
        );
        assert_eq!(
            hook_action(zeros, "0", false, false),
            HookAction::List,
            "file checkout"
        );
        assert_eq!(hook_action("", "1", false, false), HookAction::List);
        assert_eq!(
            hook_action(zeros, "1", true, false),
            HookAction::Nothing,
            "CI"
        );
        assert_eq!(
            hook_action(sha, "1", false, true),
            HookAction::Nothing,
            "skip"
        );
    }
}
