//! The submodules of a linked worktree, before `hook remove` moves it into
//! the trash.
//!
//! A linked worktree keeps the git directories of its submodules in its
//! administrative directory (`<repo>/.git/worktrees/<id>/modules/<name>`),
//! and each submodule's `.git` is a file pointing there. Unregistering the
//! worktree deletes that directory, and with it the submodules' commits
//! that were never pushed. So before that, each submodule's git directory
//! is moved into the submodule itself, as `<submodule>/.git`: the layout a
//! submodule had before `git submodule absorbgitdirs`, which git still
//! reads. The submodules then travel with the worktree, and copying the
//! worktree back restores them.

use crate::error::{bail, Error, Result};
use crate::git;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// One submodule whose git directory is to move into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Embed {
    /// The submodule's working tree.
    pub work: PathBuf,
    /// Its git directory, under `<admin>/modules`.
    pub git_dir: PathBuf,
}

/// What [`embed`] would move for the worktree at `root` whose
/// administrative directory is `admin`, nested submodules first. Empty when
/// it has no submodule git directories. An error, before anything is
/// changed, when one of them cannot be carried along: it has no working
/// tree (`git submodule deinit`), or it is on another volume.
pub fn plan(root: &Path, admin: &Path) -> Result<Vec<Embed>> {
    let modules = admin.join("modules");
    if !modules.is_dir() {
        return Ok(Vec::new());
    }
    let modules = modules.canonicalize()?;
    let mut out = Vec::new();
    collect(root, &modules, &mut out)?;
    let mut found = Vec::new();
    git_dirs(&modules, &mut found);
    if let Some(left) = found
        .iter()
        .find(|g| !out.iter().any(|e| g.starts_with(&e.git_dir)))
    {
        bail!(
            "the submodule git directory {} has no working tree in the worktree, \
             so it cannot be kept with it",
            left.display()
        );
    }
    let dev = |p: &Path| p.metadata().map(|m| m.dev()).ok();
    if !out.is_empty() && dev(&modules) != dev(root) {
        bail!(
            "the submodules' git directories ({}) are on another volume than the worktree",
            modules.display()
        );
    }
    Ok(out)
}

/// The initialized submodules under `dir` whose git directory is under
/// `modules`, deepest first.
fn collect(dir: &Path, modules: &Path, out: &mut Vec<Embed>) -> Result<()> {
    let staged = git::output(dir, &["ls-files", "-z", "--stage"])?;
    for entry in staged.split('\0') {
        // `<mode> <object> <stage>\t<path>`; a submodule is mode 160000.
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        if !meta.starts_with("160000 ") {
            continue;
        }
        let work = dir.join(path);
        if !work.join(".git").is_file() {
            continue; // not initialized, or its git directory is inside it
        }
        let git_dir = git::output(&work, &["rev-parse", "--absolute-git-dir"])?;
        let git_dir = PathBuf::from(git_dir.trim_end_matches('\n'));
        let git_dir = git_dir.canonicalize().unwrap_or(git_dir);
        if !git_dir.starts_with(modules) {
            continue; // not in the worktree's entry: unregistering keeps it
        }
        collect(&work, modules, out)?;
        if !out.iter().any(|e| e.git_dir == git_dir) {
            out.push(Embed { work, git_dir });
        }
    }
    Ok(())
}

/// The git directories (a directory with a `HEAD`) under `dir`.
fn git_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if p.join("HEAD").is_file() {
            out.push(p.clone());
            git_dirs(&p.join("modules"), out);
        } else {
            git_dirs(&p, out);
        }
    }
}

/// Move each git directory of the plan into its submodule. Stops at the
/// first one that fails, with that one as it was.
pub fn embed(plan: &[Embed]) -> Result<()> {
    for e in plan {
        let dot = e.work.join(".git");
        let link = fs::read(&dot)?;
        fs::remove_file(&dot)?;
        if let Err(err) = fs::rename(&e.git_dir, &dot) {
            let _ = fs::write(&dot, &link);
            return Err(Error(format!(
                "cannot move {} to {}: {err}",
                e.git_dir.display(),
                dot.display()
            )));
        }
        // Inside the submodule, the working tree is its parent directory.
        let config = dot.join("config");
        let _ = git::output(
            &e.work,
            &[
                "config",
                "--file",
                &config.to_string_lossy(),
                "--unset",
                "core.worktree",
            ],
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_dirs_are_found_with_the_nested_ones() {
        let t = tempfile::tempdir().unwrap();
        let m = t.path();
        for d in ["a", "a/modules/b", "group/c"] {
            fs::create_dir_all(m.join(d).join("objects")).unwrap();
            fs::write(m.join(d).join("HEAD"), "ref: refs/heads/x\n").unwrap();
        }
        let mut found = Vec::new();
        git_dirs(m, &mut found);
        found.sort();
        assert_eq!(
            found,
            vec![m.join("a"), m.join("a/modules/b"), m.join("group/c")]
        );
    }
}
