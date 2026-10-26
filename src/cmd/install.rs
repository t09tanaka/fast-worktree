//! `fast-worktree install`: make every tracked project's dependencies present.

use crate::ecosystem::{self, Ctx, Mode, Project, Report};
use crate::error::{bail, Result};
use crate::hooks::{self, Config, Stage};
use crate::{git, paths, trash, util};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Opts {
    pub root: Option<PathBuf>,
    pub projects: Vec<String>,
    pub changed: bool,
}

pub fn run(opts: Opts) -> Result<i32> {
    let root = git::toplevel(opts.root.as_deref().unwrap_or(Path::new(".")))?;
    let cfg = Config::load(&root)?;
    let mode = if opts.changed {
        Mode::Changed
    } else {
        Mode::Missing
    };
    let deps_ok = install_deps(&root, &opts.projects, &cfg, mode)?;
    let hooks_ok = run_post_add(&root, &cfg)?;
    Ok(if deps_ok && hooks_ok { 0 } else { 1 })
}

/// Real path of the main checkout of the repository at `root`.
pub fn main_checkout(root: &Path) -> Result<PathBuf> {
    let wts = git::worktrees(root)?;
    let Some(main) = wts.first() else {
        bail!("no worktrees registered");
    };
    Ok(main
        .path
        .canonicalize()
        .unwrap_or_else(|_| main.path.clone()))
}

/// Install into every project under `root` (or only `only`, given relative
/// to `root` or to the current directory). Prints one line per project.
/// Returns whether all succeeded.
pub fn install_deps(root: &Path, only: &[String], cfg: &Config, mode: Mode) -> Result<bool> {
    let start = Instant::now();
    let root = root.canonicalize()?;
    let mut projects = ecosystem::discover(&root)?;
    if !only.is_empty() {
        let wanted: Vec<String> = only
            .iter()
            .map(|p| relative_project(&root, p))
            .collect::<Result<_>>()?;
        for w in &wanted {
            if !projects.iter().any(|p| &p.dir == w) {
                bail!("no tracked lockfile in {w}");
            }
        }
        projects.retain(|p| wanted.contains(&p.dir));
    }
    if projects.is_empty() {
        println!("no lockfile tracked");
        return Ok(true);
    }
    let results = install_projects(&root, &projects, cfg, mode)?;
    let mut ok = true;
    for (p, r) in projects.iter().zip(results) {
        ok &= print_result(p, &r);
    }
    println!("deps done in {}", util::elapsed(start));
    Ok(ok)
}

/// Install `projects` of the worktree `root` in parallel, one result per
/// project in the same order. Nothing is printed. This is the entry point
/// for callers that pick the projects themselves; with [`Mode::Changed`],
/// projects whose dependencies are present and up to date are left alone.
pub fn install_projects(
    root: &Path,
    projects: &[Project],
    cfg: &Config,
    mode: Mode,
) -> Result<Vec<std::result::Result<Report, String>>> {
    trash::sweep();
    let root = root.canonicalize()?;
    let worktrees: Vec<PathBuf> = git::worktrees(&root)?
        .into_iter()
        .map(|w| w.path.canonicalize().unwrap_or(w.path))
        .collect();
    let Some(main) = worktrees.first().cloned() else {
        bail!("no worktrees registered");
    };
    let ctx = Ctx {
        root,
        worktrees,
        cache_repo: paths::cache_root().join(paths::repo_key(&main)),
        skip_lifecycle: cfg.skip_lifecycle.clone(),
    };
    Ok(std::thread::scope(|s| {
        let handles: Vec<_> = projects
            .iter()
            .map(|p| {
                let ctx = &ctx;
                s.spawn(move || p.ecosystem.install(ctx, &p.dir, mode))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err("install thread panicked".into()))
            })
            .collect()
    }))
}

/// Print one project's result. Returns whether it succeeded.
pub fn print_result(p: &Project, r: &std::result::Result<Report, String>) -> bool {
    match r {
        Ok(report) => {
            if !report.lines.is_empty() {
                println!("{}", report.lines);
            }
            true
        }
        Err(log) => {
            println!("FAILED {}:", p.dir);
            for l in util::tail(log, 30).lines() {
                println!("  {l}");
            }
            false
        }
    }
}

/// A project argument as a directory relative to `root` (`.` for the root).
fn relative_project(root: &Path, arg: &str) -> Result<String> {
    let p = Path::new(arg);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        // Relative to the current directory when that resolves inside the
        // root, else relative to the root.
        match std::env::current_dir()?.join(p).canonicalize() {
            Ok(c) if c.starts_with(root) => c,
            _ => root.join(p),
        }
    };
    let abs = abs.canonicalize().unwrap_or(abs);
    let Ok(rel) = abs.strip_prefix(root) else {
        bail!("{arg} is outside {}", root.display());
    };
    let rel = rel.to_string_lossy().trim_end_matches('/').to_string();
    Ok(if rel.is_empty() { ".".into() } else { rel })
}

/// Run the `post_add` hooks of `root`. Returns whether all succeeded.
pub fn run_post_add(root: &Path, cfg: &Config) -> Result<bool> {
    if cfg.post_add.is_empty() {
        return Ok(true);
    }
    let root = root.canonicalize()?;
    let main = main_checkout(&root)?;
    Ok(hooks::run(cfg, Stage::PostAdd, &root, &main) == 0)
}
