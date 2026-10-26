//! Bundler: one project per tracked `Gemfile.lock`.
//!
//! The gems are installed into one directory, `BUNDLE_PATH`, which is
//! reused whole the way a [`generic`] `[[clone]]` is: from the cache, else
//! from a registered worktree, else `bundle install`. Native extensions are
//! built for one Ruby ABI and platform, so `ruby -v` and `RUBY_PLATFORM` are
//! part of the key, with `Gemfile.lock`.
//!
//! The directory is the one Bundler itself uses, so that `bundle exec`
//! finds what was installed: `BUNDLE_PATH` from the project's
//! `.bundle/config`, else from the environment, else from
//! `~/.bundle/config` (Bundler's own order), else `<dir>/vendor/bundle`. A
//! relative one is relative to the project directory, as for Bundler. One
//! outside the worktree (gems shared by every checkout, or a symlink out of
//! it), and one that holds tracked files (`BUNDLE_PATH=vendor` next to a
//! tracked `vendor/javascript`), is not cloned and never moved away: there
//! `bundle check` decides, and `bundle install` runs when it fails.
//!
//! `.bundle/config` is often ignored by git, and without it Bundler looks
//! for the gems among the system's. So `bundle check` (after a clone) and
//! `bundle install` run with `BUNDLE_PATH` set to the directory.
//!
//! `bundle check` writes `Gemfile.lock` when it lacks the current platform
//! (or the Bundler version). Every check here is `bundle check --dry-run`,
//! which does not.
//!
//! [`generic`]: super::generic

use super::{generic, target, Ctx, Mode, Plan, Preset, Report, Target};
use crate::util;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

const LOCKFILE: &str = "Gemfile.lock";
const DEFAULT_PATH: &str = "vendor/bundle";

pub struct Bundler;

impl Preset for Bundler {
    fn name(&self) -> &'static str {
        "bundler"
    }

    fn manifest(&self) -> &'static str {
        LOCKFILE
    }

    fn managed(&self) -> &'static str {
        DEFAULT_PATH
    }

    fn default_key_commands(&self) -> &'static [&'static str] {
        &["ruby -v", "ruby -e 'print RUBY_PLATFORM'"]
    }

    fn is_installed(&self, root: &Path, t: &Target) -> bool {
        match locate(root, t) {
            // A `BUNDLE_PATH` of the project's own that fast-worktree did
            // not make (it may hold tracked files; telling needs git, and
            // this must be cheap): there when it is a directory, as
            // outside.
            Where::Inside(l)
                if l.path != t.path && !root.join(&l.path).join(generic::KEY_FILE).is_file() =>
            {
                root.join(&l.path).is_dir()
            }
            Where::Inside(l) => generic::is_installed(root, &l),
            Where::Outside(p, _) => p.is_dir(),
        }
    }

    fn install(&self, ctx: &Ctx, t: &Target, mode: Mode) -> Result<Report, String> {
        match locate_checked(&ctx.root, t) {
            Where::Inside(t) => generic::Generic.install(ctx, &t, mode),
            Where::Outside(p, why) => install_outside(ctx, t, &p, why, mode),
        }
    }

    fn plan(&self, ctx: &Ctx, t: &Target, mode: Mode, fresh: bool) -> Plan {
        match locate_checked(&ctx.root, t) {
            Where::Inside(t) => generic::Generic.plan(ctx, &t, mode, fresh),
            Where::Outside(p, where_) => {
                let why = vec![format!(
                    "clone: the gems are in {}, {where_}",
                    util::tilde(&p)
                )];
                let project = super::project_path(&ctx.root, &t.dir);
                let how = t.install.as_deref().unwrap_or("bundle install");
                if !fresh && super::succeeds(&project, CHECK, &ctx.root, &ctx.main) {
                    Plan::new("present (bundle check)", why)
                } else if mode.clone_only() {
                    Plan::new(format!("later, in the background ({how})"), why)
                } else {
                    Plan::new(format!("install ({how})"), why)
                }
            }
        }
    }

    fn rewrite_moved(&self, root: &Path, t: &Target, from: &[PathBuf]) -> usize {
        match locate_checked(root, t) {
            Where::Inside(t) if t.rewrite_paths && target::inside(root, &t.path).is_ok() => {
                super::rewrite_tree(&root.join(&t.path), from, root)
            }
            _ => 0,
        }
    }
}

/// `bundle check`, as every check here runs it: without writing
/// `Gemfile.lock`.
const CHECK: &str = "bundle check --dry-run";

/// Where the gems of a target go.
#[derive(Debug)]
enum Where {
    /// In the worktree, fast-worktree's to clone and move away: the target,
    /// with `path` set to the directory and Bundler's `install` and
    /// `verify` filled in where it has none.
    Inside(Target),
    /// Not fast-worktree's: where, and why (`outside the worktree`).
    Outside(PathBuf, &'static str),
}

const OUTSIDE: &str = "outside the worktree";
const TRACKED: &str = "a directory that holds tracked files";

/// [`locate`], and a `BUNDLE_PATH` in the worktree that holds tracked
/// files is not fast-worktree's either: moving it away would take them.
/// (The default `<dir>/vendor/bundle` is checked with the other targets,
/// [`target::check_fs`].) One `git ls-files`, when `BUNDLE_PATH` is set.
fn locate_checked(root: &Path, t: &Target) -> Where {
    match locate(root, t) {
        Where::Inside(l) if l.path != t.path && target::holds_tracked(root, &l.path) => {
            Where::Outside(root.join(&l.path), TRACKED)
        }
        w => w,
    }
}

/// Where the gems of `t` in the worktree `root` go ([`bundle_path`]).
fn locate(root: &Path, t: &Target) -> Where {
    locate_with(
        root,
        t,
        std::env::var("BUNDLE_PATH").ok().as_deref(),
        fs::read_to_string(util::home().join(".bundle/config"))
            .ok()
            .as_deref(),
    )
}

/// [`locate`], with the environment's `BUNDLE_PATH` and the text of
/// `~/.bundle/config` given.
fn locate_with(root: &Path, t: &Target, env: Option<&str>, global: Option<&str>) -> Where {
    let project = super::project_path(root, &t.dir);
    let configured = bundle_path(
        fs::read_to_string(project.join(".bundle/config"))
            .ok()
            .as_deref(),
        env,
        global,
    );
    let path = match configured {
        None => t.path.clone(),
        Some(p) => {
            let abs = match p.strip_prefix("~/") {
                Some(rest) => util::home().join(rest),
                None => project.join(&p),
            };
            match inside(root, &abs) {
                Some(rel) => rel,
                None => return Where::Outside(real(&abs), OUTSIDE),
            }
        }
    };
    // `install` runs in the project directory, `verify` in `path`.
    let rel_to_dir = match t.dir.as_str() {
        "." => quote(&path),
        d => match Path::new(&path).strip_prefix(d) {
            Ok(rel) => quote(&rel.to_string_lossy()),
            Err(_) => format!("\"$FAST_WORKTREE_PATH\"/{}", quote(&path)),
        },
    };
    let gemfile = match t.dir.as_str() {
        "." => "\"$FAST_WORKTREE_PATH\"/Gemfile".to_string(),
        d => format!("\"$FAST_WORKTREE_PATH\"/{}/Gemfile", quote(d)),
    };
    Where::Inside(Target {
        install: t
            .install
            .clone()
            .or_else(|| Some(format!("BUNDLE_PATH={rel_to_dir} bundle install"))),
        // In `path` (of this worktree, a cache entry or another worktree),
        // with the Gemfile of the worktree it is checked for.
        verify: t.verify.clone().or_else(|| {
            Some(format!(
                "BUNDLE_PATH=\"$PWD\" BUNDLE_GEMFILE={gemfile} {CHECK}"
            ))
        }),
        path,
        ..t.clone()
    })
}

/// `BUNDLE_PATH` as Bundler reads it: the project's `.bundle/config`
/// (`local`), else the environment (`env`), else `~/.bundle/config`
/// (`global`). `None` when none of them sets it.
fn bundle_path(local: Option<&str>, env: Option<&str>, global: Option<&str>) -> Option<String> {
    local
        .and_then(config_path)
        .or_else(|| {
            env.map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
        })
        .or_else(|| global.and_then(config_path))
}

/// The `BUNDLE_PATH` of a Bundler config file (YAML, one `KEY: value` per
/// line, the value quoted or not).
fn config_path(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let v = l.trim().strip_prefix("BUNDLE_PATH:")?.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(v);
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// `p` without `.` and `..` components (`..` takes away the one before),
/// without looking at the file system.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Where `p` really is: the deepest part of it that exists, with every
/// symlink resolved, and the rest as written.
fn real(p: &Path) -> PathBuf {
    let p = lexical(p);
    for base in p.ancestors() {
        if base.symlink_metadata().is_err() {
            continue;
        }
        let Ok(rest) = p.strip_prefix(base) else {
            break;
        };
        return match base.canonicalize() {
            Ok(b) if rest.as_os_str().is_empty() => b,
            Ok(b) => b.join(rest),
            Err(_) => p.clone(),
        };
    }
    p
}

/// Where `p` really is ([`real`]) relative to the real path of the
/// worktree `root`, when that is inside it and not the root itself: a path
/// that is in the worktree as written but goes out of it through a symlink
/// is outside.
fn inside(root: &Path, p: &Path) -> Option<String> {
    let real_root = root.canonicalize().ok()?;
    let p = real(p);
    let rel = p.strip_prefix(&real_root).ok()?;
    let rel = rel.to_string_lossy().trim_end_matches('/').to_string();
    (!rel.is_empty()).then_some(rel)
}

/// `s` quoted for a POSIX shell when it needs it.
fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+@:".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Gems that are not fast-worktree's ([`Where::Outside`]): nothing to
/// clone or move away. Present when `bundle check` passes in the project,
/// else `bundle install` (or the target's own).
fn install_outside(
    ctx: &Ctx,
    t: &Target,
    gems: &Path,
    where_: &str,
    mode: Mode,
) -> Result<Report, String> {
    let start = Instant::now();
    let dir = t.dir.as_str();
    let project = super::project_path(&ctx.root, dir);
    if super::succeeds(&project, CHECK, &ctx.root, &ctx.main) {
        return Ok(Report::kept(format!(
            "skip   {dir} (gems in {}, {where_})",
            util::tilde(gems)
        )));
    }
    if mode.clone_only() {
        return Ok(Report::missing(""));
    }
    let cmd = t.install.as_deref().unwrap_or("bundle install");
    super::run_quietly(&project, cmd, &ctx.root, &ctx.main)?;
    if let Some(v) = &t.verify {
        super::run_quietly(&project, v, &ctx.root, &ctx.main)
            .map_err(|e| format!("verify failed after the install: {e}"))?;
    }
    for a in &t.after {
        super::run_quietly(&project, a, &ctx.root, &ctx.main)?;
    }
    Ok(Report::done(format!(
        "install {dir} ({cmd}, {})",
        util::elapsed(start)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecosystem::target;

    #[test]
    fn bundle_path_is_read_in_bundlers_order() {
        let local = "---\nBUNDLE_WITHOUT: \"test\"\nBUNDLE_PATH: \"vendor/gems\"\n";
        let global = "BUNDLE_PATH: ~/.gems\n";
        assert_eq!(
            bundle_path(Some(local), Some("env"), Some(global)).as_deref(),
            Some("vendor/gems"),
            "the project's config wins over the environment"
        );
        assert_eq!(
            bundle_path(Some("BUNDLE_JOBS: 4\n"), Some("env"), Some(global)).as_deref(),
            Some("env")
        );
        assert_eq!(
            bundle_path(None, Some(" "), Some(global)).as_deref(),
            Some("~/.gems")
        );
        assert_eq!(bundle_path(None, None, None), None);
        assert_eq!(config_path("BUNDLE_PATH: 'a b'\n").as_deref(), Some("a b"));
        assert_eq!(config_path("BUNDLE_PATH: \"\"\n"), None);
    }

    fn tgt(dir: &str) -> Target {
        Target::with_defaults(&Bundler, dir)
    }

    #[test]
    fn the_defaults_and_where_the_gems_go() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let api = tgt("api");
        assert_eq!(api.path, "api/vendor/bundle");
        assert_eq!(api.key, vec!["api/Gemfile.lock"]);
        assert_eq!(
            api.key_commands,
            vec!["ruby -v", "ruby -e 'print RUBY_PLATFORM'"]
        );
        let Where::Inside(got) = locate_with(&root, &api, None, None) else {
            panic!("outside");
        };
        assert_eq!(got.path, "api/vendor/bundle");
        assert_eq!(
            got.install.as_deref(),
            Some("BUNDLE_PATH=vendor/bundle bundle install")
        );
        assert_eq!(
            got.verify.as_deref(),
            Some("BUNDLE_PATH=\"$PWD\" BUNDLE_GEMFILE=\"$FAST_WORKTREE_PATH\"/api/Gemfile bundle check --dry-run")
        );
        // The project's .bundle/config, relative to the project.
        fs::create_dir_all(root.join("api/.bundle")).unwrap();
        fs::write(
            root.join("api/.bundle/config"),
            "BUNDLE_PATH: \"../shared gems\"\n",
        )
        .unwrap();
        let Where::Inside(got) = locate_with(&root, &api, None, None) else {
            panic!("outside");
        };
        assert_eq!(got.path, "shared gems");
        assert_eq!(
            got.install.as_deref(),
            Some("BUNDLE_PATH=\"$FAST_WORKTREE_PATH\"/'shared gems' bundle install")
        );
        // Outside the worktree: shared, not cloned.
        let outside = |t: &Target| match locate_with(&root, t, None, None) {
            Where::Outside(p, _) => p,
            Where::Inside(t) => panic!("inside: {t:?}"),
        };
        fs::write(
            root.join("api/.bundle/config"),
            "BUNDLE_PATH: \"../../gems\"\n",
        )
        .unwrap();
        assert_eq!(outside(&api), root.parent().unwrap().join("gems"));
        fs::write(
            root.join("api/.bundle/config"),
            "BUNDLE_PATH: \"/opt/gems\"\n",
        )
        .unwrap();
        assert_eq!(outside(&api), real(Path::new("/opt/gems")));
        // In the worktree as written, out of it through a symlink: outside.
        let other = tempfile::tempdir().unwrap();
        let elsewhere = other.path().join("elsewhere");
        fs::create_dir_all(elsewhere.join("gems")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("api/link")).unwrap();
        fs::write(
            root.join("api/.bundle/config"),
            "BUNDLE_PATH: \"link/gems\"\n",
        )
        .unwrap();
        assert_eq!(
            outside(&api),
            elsewhere.canonicalize().unwrap().join("gems")
        );
        // Into the worktree through one: where it really is.
        fs::create_dir_all(root.join("api/real")).unwrap();
        std::os::unix::fs::symlink(root.join("api/real"), root.join("api/inner")).unwrap();
        fs::write(
            root.join("api/.bundle/config"),
            "BUNDLE_PATH: \"inner/gems\"\n",
        )
        .unwrap();
        let Where::Inside(got) = locate_with(&root, &api, None, None) else {
            panic!("outside");
        };
        assert_eq!(got.path, "api/real/gems");
        // The environment, then ~/.bundle/config, for the root project.
        let root_project = tgt(".");
        let Where::Inside(got) = locate_with(&root, &root_project, Some("gems"), None) else {
            panic!("outside");
        };
        assert_eq!(got.path, "gems");
        let global = format!("BUNDLE_PATH: \"{}/g\"\n", root.display());
        let Where::Inside(got) = locate_with(&root, &root_project, None, Some(&global)) else {
            panic!("outside");
        };
        assert_eq!(got.path, "g", "absolute, but in the worktree");
        // Written by hand: kept.
        let mut own = tgt(".");
        own.install = Some("bin/setup".into());
        own.verify = Some("true".into());
        let Where::Inside(got) = locate_with(&root, &own, None, None) else {
            panic!("outside");
        };
        assert_eq!(got.path, "vendor/bundle");
        assert_eq!(got.install.as_deref(), Some("bin/setup"));
        assert_eq!(got.verify.as_deref(), Some("true"));
    }

    #[test]
    fn a_clone_entry_takes_the_preset() {
        let cfg = crate::hooks::parse("[[clone]]\npreset = \"bundler\"\ndir = \"web\"\n")
            .unwrap()
            .0;
        let t = target::resolve(&cfg.clone).unwrap().remove(0);
        assert_eq!(t.preset.name(), "bundler");
        assert_eq!(t.path, "web/vendor/bundle");
        assert_eq!(t.label(), "web (bundler)");
        assert_eq!(lexical(Path::new("/a/./b/../c")), Path::new("/a/c"));
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("vendor/bundle"), "vendor/bundle");
    }
}
