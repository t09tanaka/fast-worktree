//! Package ecosystems. Each one finds its projects from tracked manifest
//! files and knows how to make a project's dependencies present. v0.1 has
//! npm only; another ecosystem is a new module plus a line in [`all`].

pub mod npm;

use crate::error::Result;
use crate::git;
use std::path::{Path, PathBuf};

/// What an install needs to know about its surroundings.
pub struct Ctx {
    /// The worktree being installed into (real path).
    pub root: PathBuf,
    /// Every registered worktree (real paths), the main checkout first.
    /// Candidates to clone from.
    pub worktrees: Vec<PathBuf>,
    /// This repository's directory in the cache.
    pub cache_repo: PathBuf,
}

pub trait Ecosystem: Sync {
    /// The file whose tracked copies mark a project directory.
    fn manifest(&self) -> &'static str;

    /// Whether the project's dependencies look present, for `check`. Must
    /// be cheap: no subprocesses.
    fn is_installed(&self, project: &Path) -> bool;

    /// Make the dependencies of `ctx.root/<dir>` present. Returns the lines
    /// to print, or on failure the log to show.
    fn install(&self, ctx: &Ctx, dir: &str) -> std::result::Result<String, String>;
}

pub fn all() -> Vec<&'static dyn Ecosystem> {
    vec![&npm::Npm]
}

/// `root/<dir>`, with `.` meaning `root` itself.
pub fn project_path(root: &Path, dir: &str) -> PathBuf {
    if dir == "." {
        root.to_path_buf()
    } else {
        root.join(dir)
    }
}

pub struct Project {
    pub ecosystem: &'static dyn Ecosystem,
    /// Relative to the worktree root; `.` for the root itself.
    pub dir: String,
}

/// Tracked projects of every ecosystem, in path order, from one `git
/// ls-files` call.
pub fn discover(root: &Path) -> Result<Vec<Project>> {
    let ecos = all();
    let specs: Vec<String> = ecos
        .iter()
        .map(|e| format!(":(glob)**/{}", e.manifest()))
        .collect();
    let spec_refs: Vec<&str> = specs.iter().map(String::as_str).collect();
    let files = git::tracked(root, &spec_refs)?;
    let mut projects: Vec<Project> = Vec::new();
    for f in files {
        let (dir, base) = match f.rsplit_once('/') {
            Some((d, b)) => (d.to_string(), b),
            None => (".".to_string(), f.as_str()),
        };
        let Some(eco) = ecos.iter().find(|e| e.manifest() == base) else {
            continue;
        };
        // A sparse checkout can track a manifest that is not on disk.
        if !root.join(&f).is_file() {
            continue;
        }
        projects.push(Project {
            ecosystem: *eco,
            dir,
        });
    }
    projects.sort_by(|a, b| a.dir.cmp(&b.dir));
    projects.dedup_by(|a, b| a.dir == b.dir && a.ecosystem.manifest() == b.ecosystem.manifest());
    Ok(projects)
}
