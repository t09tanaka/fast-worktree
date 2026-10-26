//! Package ecosystems. Each one finds its projects from tracked lockfiles
//! and knows how to make a project's dependencies present. Another
//! ecosystem is a new module plus a line in [`all`].

pub mod dart;
pub mod npm;

use crate::error::Result;
use crate::git;
use crate::hooks::SkipLifecycle;
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
    /// `skip_lifecycle` rules from `.fast-worktree.toml`.
    pub skip_lifecycle: Vec<SkipLifecycle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Make missing dependencies present by any means; leave present ones.
    Missing,
    /// Like `Missing`, and also install again where the lockfile or the
    /// Node version changed since the last install.
    Changed,
    /// Only what a clone (cache or another worktree) can provide: nothing
    /// that downloads or builds. For the `post-checkout` hook.
    CloneOnly,
    /// `Changed` limited to what `CloneOnly` may do: an outdated tree is
    /// moved away and replaced by a clone when there is one, else reported
    /// missing. For `hook create` with a spare from the pool.
    ChangedCloneOnly,
}

impl Mode {
    /// Whether present dependencies are checked against the lockfile.
    pub fn reinstalls_changed(self) -> bool {
        matches!(self, Mode::Changed | Mode::ChangedCloneOnly)
    }

    /// Whether only clones are allowed: nothing that downloads or builds.
    pub fn clone_only(self) -> bool {
        matches!(self, Mode::CloneOnly | Mode::ChangedCloneOnly)
    }
}

/// The result of one project's install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Lines to print.
    pub lines: String,
    /// `CloneOnly` found nothing to clone: the dependencies are still
    /// missing.
    pub missing: bool,
    /// The dependencies already there were left as they were.
    pub kept: bool,
}

impl Report {
    pub fn done(lines: impl Into<String>) -> Report {
        Report {
            lines: lines.into(),
            missing: false,
            kept: false,
        }
    }

    /// Nothing was installed: what was there is still current.
    pub fn kept(lines: impl Into<String>) -> Report {
        Report {
            kept: true,
            ..Report::done(lines)
        }
    }

    /// A clone-only install could not make the dependencies present.
    pub fn missing(lines: impl Into<String>) -> Report {
        Report {
            missing: true,
            ..Report::done(lines)
        }
    }
}

pub trait Ecosystem: Sync {
    /// Short name shown next to a project, such as `npm`.
    fn name(&self) -> &'static str;

    /// The lockfile whose tracked copies mark a project directory.
    fn manifest(&self) -> &'static str;

    /// Whether the project's dependencies look present, for `check`. Must
    /// be cheap: no subprocesses.
    fn is_installed(&self, project: &Path) -> bool;

    /// Make the dependencies of `ctx.root/<dir>` present. Returns what to
    /// print, or on failure the log to show.
    fn install(&self, ctx: &Ctx, dir: &str, mode: Mode) -> std::result::Result<Report, String>;

    /// Run the project's own install steps again on dependencies that are
    /// already present, because the worktree moved (a spare from the pool:
    /// what those steps wrote by absolute path points into the pool) and
    /// its tracked files may have changed. Returns notes to print. On
    /// failure the dependencies are moved away, so the next `install`
    /// installs them again.
    fn rerun_own_scripts(
        &self,
        _root: &Path,
        _dir: &str,
        _rules: &[SkipLifecycle],
    ) -> std::result::Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

pub fn all() -> Vec<&'static dyn Ecosystem> {
    vec![&npm::Npm, &dart::Dart]
}

/// `root/<dir>`, with `.` meaning `root` itself.
pub fn project_path(root: &Path, dir: &str) -> PathBuf {
    if dir == "." {
        root.to_path_buf()
    } else {
        root.join(dir)
    }
}

#[derive(Clone)]
pub struct Project {
    pub ecosystem: &'static dyn Ecosystem,
    /// Relative to the worktree root; `.` for the root itself.
    pub dir: String,
}

impl Project {
    /// `api (npm)`: how a project is named in `check`'s output.
    pub fn label(&self) -> String {
        format!("{} ({})", self.dir, self.ecosystem.name())
    }
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
        // A sparse checkout can track a lockfile that is not on disk.
        if !root.join(&f).is_file() {
            continue;
        }
        projects.push(Project {
            ecosystem: *eco,
            dir,
        });
    }
    projects.sort_by(|a, b| {
        (a.dir.as_str(), a.ecosystem.name()).cmp(&(b.dir.as_str(), b.ecosystem.name()))
    });
    projects.dedup_by(|a, b| a.dir == b.dir && a.ecosystem.name() == b.ecosystem.name());
    Ok(projects)
}
