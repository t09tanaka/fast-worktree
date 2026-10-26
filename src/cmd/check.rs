//! `fast-worktree check`: list projects whose dependencies are missing.
//!
//! Meant for a `post-checkout` hook, so it always exits 0, never spawns
//! anything but one `git ls-files`, and stays well under 100 ms.

use crate::ecosystem;
use crate::git;
use std::path::Path;

pub fn run(quiet: bool) -> i32 {
    let Ok(root) = git::toplevel(Path::new(".")) else {
        return 0;
    };
    let Ok(projects) = ecosystem::discover(&root) else {
        return 0;
    };
    let missing: Vec<&str> = projects
        .iter()
        .filter(|p| {
            !p.ecosystem
                .is_installed(&ecosystem::project_path(&root, &p.dir))
        })
        .map(|p| p.dir.as_str())
        .collect();
    print!("{}", report(&missing, projects.len(), quiet));
    0
}

pub fn report(missing: &[&str], total: usize, quiet: bool) -> String {
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
        assert_eq!(report(&[], 3, true), "");
        assert_eq!(report(&[], 3, false), "deps ok (3 projects)\n");
        assert_eq!(report(&[], 0, false), "");
        let r = report(&["api", "."], 3, true);
        assert!(r.starts_with("missing deps: api\nmissing deps: .\n"));
        assert_eq!(r, report(&["api", "."], 3, false));
    }
}
