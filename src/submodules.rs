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
//! worktree back restores them. When one of them cannot be moved, those
//! moved before it are put back.
//!
//! [`checked_out`] lists every checked-out submodule whatever its layout,
//! for the decision to keep the worktree and for keeping each submodule's
//! `HEAD` in the main repository.

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

/// Move each git directory of the plan into its submodule. When one fails,
/// the ones moved before it are put back (in reverse order, so that nested
/// ones land in their parent's git directory again), and the error says
/// so: the worktree is then as it was.
pub fn embed(plan: &[Embed]) -> Result<()> {
    let mut done: Vec<(&Embed, Undo)> = Vec::new();
    for e in plan {
        match embed_one(e) {
            Ok(undo) => done.push((e, undo)),
            Err(err) => {
                let mut msg = err.0;
                for (e, undo) in done.iter().rev() {
                    if let Err(why) = unembed(e, undo) {
                        msg.push_str(&format!(
                            "; and {} could not be put back from {}: {why}",
                            e.git_dir.display(),
                            e.work.join(".git").display()
                        ));
                    }
                }
                return Err(Error(msg));
            }
        }
    }
    Ok(())
}

/// What [`unembed`] needs to put one submodule back.
struct Undo {
    /// The bytes of its `.git` file.
    link: Vec<u8>,
    /// Its `core.worktree`, if it had one.
    worktree: Option<String>,
}

/// Where `git config --file` runs: outside any repository, so that a
/// submodule whose `.git` is being moved is not read as one.
const NEUTRAL: &str = "/";

fn config_args<'a>(config: &'a str, rest: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["config", "--file", config];
    v.extend_from_slice(rest);
    v
}

/// Move one git directory into its submodule, with that one as it was when
/// it fails.
fn embed_one(e: &Embed) -> Result<Undo> {
    let dot = e.work.join(".git");
    let link = fs::read(&dot).map_err(|err| Error(format!("{}: {err}", dot.display())))?;
    let config = e.git_dir.join("config").to_string_lossy().into_owned();
    let worktree = git::output(
        Path::new(NEUTRAL),
        &config_args(&config, &["--get", "core.worktree"]),
    )
    .ok()
    .map(|s| s.trim_end_matches('\n').to_string());
    fs::remove_file(&dot).map_err(|err| Error(format!("{}: {err}", dot.display())))?;
    if let Err(err) = fs::rename(&e.git_dir, &dot) {
        let _ = fs::write(&dot, &link);
        return Err(Error(format!(
            "cannot move {} to {}: {err}",
            e.git_dir.display(),
            dot.display()
        )));
    }
    // Inside the submodule, the working tree is its parent directory.
    let config = dot.join("config").to_string_lossy().into_owned();
    let _ = git::output(
        Path::new(NEUTRAL),
        &config_args(&config, &["--unset", "core.worktree"]),
    );
    Ok(Undo { link, worktree })
}

/// Put back what [`embed_one`] moved: the git directory where it was, the
/// `.git` file and `core.worktree`.
fn unembed(e: &Embed, undo: &Undo) -> Result<()> {
    let dot = e.work.join(".git");
    fs::rename(&dot, &e.git_dir)?;
    fs::write(&dot, &undo.link)?;
    if let Some(w) = &undo.worktree {
        let config = e.git_dir.join("config").to_string_lossy().into_owned();
        git::output(
            Path::new(NEUTRAL),
            &config_args(&config, &["core.worktree", w]),
        )?;
    }
    Ok(())
}

/// A submodule that is checked out (its `.git`, a file or a directory, is
/// there), anywhere below a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedOut {
    /// Its path relative to the worktree's root.
    pub rel: String,
    pub work: PathBuf,
    /// Its git directory, wherever it is.
    pub git_dir: PathBuf,
}

/// Every submodule checked out under `root`, nested ones included, whatever
/// their git directories are: in the worktree's entry, in the main
/// repository's `modules`, or inside the submodule itself.
pub fn checked_out(root: &Path) -> Result<Vec<CheckedOut>> {
    let mut out = Vec::new();
    walk(root, "", &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<CheckedOut>) -> Result<()> {
    let staged = git::output(dir, &["ls-files", "-z", "--stage"])?;
    for entry in staged.split('\0') {
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        if !meta.starts_with("160000 ") {
            continue;
        }
        let work = dir.join(path);
        if work.join(".git").symlink_metadata().is_err() {
            continue; // not checked out
        }
        let git_dir = git::output(&work, &["rev-parse", "--absolute-git-dir"])?;
        let rel = format!("{prefix}{path}");
        walk(&work, &format!("{rel}/"), out)?;
        out.push(CheckedOut {
            rel,
            git_dir: PathBuf::from(git_dir.trim_end_matches('\n')),
            work,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A submodule working tree whose `.git` file points at its git
    /// directory, which names the working tree in `core.worktree`.
    fn fake(base: &Path, name: &str) -> Embed {
        let work = base.join("wt").join(name);
        let git_dir = base.join("admin/modules").join(name);
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            work.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        fs::write(
            git_dir.join("config"),
            format!("[core]\n\tworktree = {}\n", work.display()),
        )
        .unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/x\n").unwrap();
        Embed { work, git_dir }
    }

    #[test]
    fn a_failed_embed_puts_back_what_it_moved() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path();
        let a = fake(base, "a");
        let b = fake(base, "b");
        let link_a = fs::read(a.work.join(".git")).unwrap();
        // The second one cannot be moved: its git directory is gone.
        fs::remove_dir_all(&b.git_dir).unwrap();
        assert!(embed(&[a.clone(), b.clone()]).is_err());
        assert!(a.git_dir.join("HEAD").is_file(), "moved back");
        assert_eq!(fs::read(a.work.join(".git")).unwrap(), link_a);
        let config = fs::read_to_string(a.git_dir.join("config")).unwrap();
        assert!(
            config.contains(&format!("worktree = {}", a.work.display())),
            "{config}"
        );
        assert!(b.work.join(".git").is_file());
    }

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
