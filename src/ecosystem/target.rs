//! What a worktree gets: the `[[clone]]` entries of `.fast-worktree.toml`,
//! or, when there is none, the projects found from the tracked lockfiles.
//!
//! ```toml
//! [[clone]]
//! preset = "npm"          # path = "api/node_modules", key = ["api/package-lock.json"],
//! dir = "api"             # key_commands = ["node -v"], npm's own install and checks
//!
//! [[clone]]               # no preset: everything is written out
//! path = "mobile/ios/Pods"
//! key = ["mobile/ios/Podfile.lock"]
//! key_commands = ["pod --version"]
//! install = "pod install"
//! verify = "test -f Manifest.lock"
//! rewrite_paths = true
//! after = ["echo done"]
//! ```
//!
//! A preset fills in every field it has a default for; a field that is
//! written replaces that default. With a preset, `path` must be the
//! preset's own directory under `dir` (`<dir>/node_modules` for npm); it
//! may be given instead of `dir`. Without a preset, `path` and `key` are
//! required.
//!
//! When there is at least one `[[clone]]`, only what they list is handled:
//! a tracked lockfile that no `[[clone]]` names is left alone (`config
//! check` reports it). Without any, every tracked lockfile of a preset is a
//! target with that preset's defaults ([`discover`]), as before `[[clone]]`
//! existed.
//!
//! What fast-worktree moves away, clones over and rewrites must be its own
//! and in the worktree. So [`targets`] also refuses ([`check_fs`]) a `dir`,
//! `path` or `key` that leads out of the worktree through a symlink and a
//! `path` that holds tracked files, and [`resolve`] a `path` that holds
//! another target's `path` or `dir`. The presets check `path` again
//! ([`inside`]) right before they move it away, clone into it, rewrite it
//! or give it to the cache: a symlink can appear in between.

use super::{generic, Preset};
use crate::error::{Error, Result};
use crate::git;
use crate::hooks::{CloneEntry, Config};
use std::path::{Component, Path};

/// One thing to make present in a worktree: a `[[clone]]` with its
/// preset's defaults filled in, or a project found by [`discover`].
#[derive(Clone)]
pub struct Target {
    /// The preset that makes it present ([`generic::Generic`] without one).
    pub preset: &'static dyn Preset,
    /// How it is named on the command line and in the output: the project
    /// directory for a preset, `path` without one.
    pub name: String,
    /// Where its commands run, relative to the worktree root (`.` for the
    /// root): `dir`, else the parent of `path`.
    pub dir: String,
    /// The directory it provides, relative to the worktree root.
    pub path: String,
    /// Files whose contents make up its key, relative to the worktree root.
    pub key: Vec<String>,
    /// Commands whose output is part of the key, run in `dir`.
    pub key_commands: Vec<String>,
    /// The command that makes `path` when nothing can be cloned. `None`:
    /// the preset's own install (or, without a preset, none).
    pub install: Option<String>,
    /// A command that must exit 0 in `path` after a clone.
    pub verify: Option<String>,
    /// After a clone, replace the source worktree's path with this one in
    /// the text files under `path`.
    pub rewrite_paths: bool,
    /// Commands run in `dir` after a clone or an install.
    pub after: Vec<String>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("preset", &self.preset.name())
            .field("name", &self.name)
            .field("dir", &self.dir)
            .field("path", &self.path)
            .field("key", &self.key)
            .field("key_commands", &self.key_commands)
            .field("install", &self.install)
            .field("verify", &self.verify)
            .field("rewrite_paths", &self.rewrite_paths)
            .field("after", &self.after)
            .finish()
    }
}

impl Target {
    /// `api (npm)`: how a target is named in `check`'s output.
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.preset.label())
    }

    /// A target of `preset` in `dir` with nothing but the preset's
    /// defaults: what auto-detection finds.
    pub fn with_defaults(preset: &'static dyn Preset, dir: &str) -> Target {
        Target {
            preset,
            name: dir.to_string(),
            dir: dir.to_string(),
            path: join(dir, preset.managed()),
            key: vec![join(dir, preset.manifest())],
            key_commands: preset
                .default_key_commands()
                .iter()
                .map(|c| c.to_string())
                .collect(),
            install: None,
            verify: None,
            rewrite_paths: false,
            after: Vec::new(),
        }
    }

    /// Whether this target is made by a preset (not a preset-less
    /// `[[clone]]`).
    pub fn has_preset(&self) -> bool {
        // Only the preset-less one marks no project directory.
        !self.preset.manifest().is_empty()
    }

    /// The key as written by default for a preset: its lockfile alone.
    pub fn key_is_lockfile_only(&self) -> bool {
        self.key.len() == 1 && self.key[0] == join(&self.dir, self.preset.manifest())
    }
}

/// `dir/name`, with `.` meaning the root.
pub fn join(dir: &str, name: &str) -> String {
    if dir == "." || dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// `./a/b/` and `a/b` are the same path; `""` and `./` are the root.
pub fn normalize(p: &str) -> String {
    let mut s = p.trim();
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    let s = s.trim_end_matches('/');
    if s.is_empty() || s == "." {
        ".".to_string()
    } else {
        s.to_string()
    }
}

/// Why a path written in `[[clone]]` cannot be used: absolute, or going up
/// out of the worktree.
fn bad_path(p: &str) -> Option<&'static str> {
    let path = Path::new(p);
    if path.is_absolute() {
        return Some("must be relative to the worktree root");
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Some("must not leave the worktree (`..`)");
    }
    None
}

/// The targets the `[[clone]]` entries describe, or every problem found in
/// them (each starting with `clone[<i>]`).
pub fn resolve(entries: &[CloneEntry]) -> std::result::Result<Vec<Target>, Vec<String>> {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        match resolve_entry(i, e) {
            Ok(t) => out.push((i, t)),
            Err(why) => errors.extend(why),
        }
    }
    for (a, (i, t)) in out.iter().enumerate() {
        if let Some((j, _)) = out[..a].iter().find(|(_, u)| u.path == t.path) {
            errors.push(format!(
                "clone[{i}]: path `{}` is also clone[{j}]'s",
                t.path
            ));
        }
    }
    for (i, t) in &out {
        for (j, u) in out.iter().filter(|(j, _)| j != i) {
            if u.path != t.path && under(&u.path, &t.path) {
                errors.push(format!(
                    "clone[{i}]: path `{}` holds clone[{j}]'s path `{}`; \
                     moving one away would take the other",
                    t.path, u.path
                ));
            } else if u.dir != "." && under(&u.dir, &t.path) {
                errors.push(format!(
                    "clone[{i}]: path `{}` holds clone[{j}]'s dir `{}`; \
                     moving it away would take that project",
                    t.path, u.dir
                ));
            }
        }
    }
    if errors.is_empty() {
        Ok(out.into_iter().map(|(_, t)| t).collect())
    } else {
        Err(errors)
    }
}

/// The target of the `[[clone]]` at index `i`, or its problems, each
/// starting with `clone[<i>]`. Only [`resolve`] tells two entries with the
/// same `path` apart.
pub fn resolve_entry(i: usize, e: &CloneEntry) -> std::result::Result<Target, Vec<String>> {
    resolve_one(e).map_err(|why| {
        why.into_iter()
            .map(|w| format!("clone[{i}]: {w}"))
            .collect()
    })
}

fn resolve_one(e: &CloneEntry) -> std::result::Result<Target, Vec<String>> {
    let mut errors = Vec::new();
    let mut check = |what: &str, p: &str| {
        if let Some(why) = bad_path(p) {
            errors.push(format!("{what} `{p}` {why}"));
        }
    };
    for (what, p) in [("dir", &e.dir), ("path", &e.path)] {
        if let Some(p) = p {
            check(what, p);
        }
    }
    for k in e.key.iter().flatten() {
        check("key", k);
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let dir = e.dir.as_deref().map(normalize);
    let path = e.path.as_deref().map(normalize);
    let mut t = match e.preset.as_deref() {
        Some(name) => {
            let Some(preset) = super::preset(name) else {
                let known: Vec<&str> = super::presets().iter().map(|p| p.name()).collect();
                return Err(vec![format!(
                    "unknown preset `{name}` (known: {})",
                    known.join(", ")
                )]);
            };
            let managed = preset.managed();
            let dir = match (dir, &path) {
                (Some(d), Some(p)) if *p != join(&d, managed) => {
                    return Err(vec![format!(
                        "path `{p}` does not match preset `{name}` with dir `{d}`: \
                         it would be `{}`",
                        join(&d, managed)
                    )]);
                }
                (Some(d), _) => d,
                (None, Some(p)) => match p.strip_suffix(managed) {
                    Some("") => ".".to_string(),
                    Some(parent) if parent.ends_with('/') => {
                        normalize(parent.trim_end_matches('/'))
                    }
                    _ => {
                        return Err(vec![format!(
                            "path `{p}` does not match preset `{name}`: \
                             it is `<dir>/{managed}`"
                        )]);
                    }
                },
                (None, None) => ".".to_string(),
            };
            Target::with_defaults(preset, &dir)
        }
        None => {
            let Some(path) = path else {
                return Err(vec![
                    "without a preset, `path` (the directory to clone) is required".into(),
                ]);
            };
            if path == "." {
                return Err(vec!["path must not be the worktree root".into()]);
            }
            if e.key.as_ref().is_none_or(|k| k.is_empty()) {
                errors.push(
                    "without a preset, `key` (the files whose contents decide what can be \
                     reused) must list at least one file"
                        .into(),
                );
                return Err(errors);
            }
            let dir = dir.unwrap_or_else(|| match path.rsplit_once('/') {
                Some((parent, _)) => parent.to_string(),
                None => ".".to_string(),
            });
            Target {
                preset: &generic::Generic,
                name: path.clone(),
                dir,
                path,
                key: Vec::new(),
                key_commands: Vec::new(),
                install: None,
                verify: None,
                rewrite_paths: false,
                after: Vec::new(),
            }
        }
    };
    if let Some(k) = &e.key {
        t.key = k.iter().map(|k| normalize(k)).collect();
    }
    if let Some(c) = &e.key_commands {
        t.key_commands = c.clone();
    }
    if e.install.is_some() {
        t.install = e.install.clone();
    }
    if e.verify.is_some() {
        t.verify = e.verify.clone();
    }
    if let Some(r) = e.rewrite_paths {
        t.rewrite_paths = r;
    }
    if let Some(a) = &e.after {
        t.after = a.clone();
    }
    Ok(t)
}

/// Whether the relative path `p` is `dir` or below it (`.` holds
/// everything).
pub fn under(p: &str, dir: &str) -> bool {
    dir == "." || p == dir || p.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

/// Where `rel` (relative to the worktree `root`) really is, relative to the
/// real path of `root`: each component is looked at in turn
/// (`symlink_metadata`), a symlink is followed, and what does not exist yet
/// is taken as written below the deepest part that does. An error, saying
/// why, when that leads out of the worktree or to its root.
pub fn real_rel(root: &Path, rel: &str) -> std::result::Result<String, String> {
    let real_root = root
        .canonicalize()
        .map_err(|e| format!("cannot be checked: {}: {e}", root.display()))?;
    let mut cur = real_root.clone();
    let mut missing = false;
    for c in Path::new(rel).components() {
        let name = match c {
            Component::CurDir => continue,
            Component::Normal(n) => n,
            _ => return Err("must stay in the worktree".into()),
        };
        cur.push(name);
        if missing {
            continue;
        }
        match cur.symlink_metadata() {
            Ok(m) if m.file_type().is_symlink() => {
                let to = cur.canonicalize().map_err(|_| {
                    format!(
                        "goes through the symlink {}, which points nowhere",
                        cur.display()
                    )
                })?;
                if !to.starts_with(&real_root) {
                    return Err(format!(
                        "goes through the symlink {} to {}, outside the worktree",
                        cur.display(),
                        to.display()
                    ));
                }
                cur = to;
            }
            Ok(_) => {}
            Err(_) => missing = true,
        }
    }
    match cur.strip_prefix(&real_root) {
        Ok(r) if r.as_os_str().is_empty() => Err("is the worktree root".into()),
        Ok(r) => Ok(r.to_string_lossy().into_owned()),
        Err(_) => Err("is outside the worktree".into()),
    }
}

/// [`real_rel`] of a target's `path`, for its preset to check right before
/// it moves it away, clones into it, rewrites it or gives it to the cache.
pub fn inside(root: &Path, path: &str) -> std::result::Result<(), String> {
    real_rel(root, path)
        .map(|_| ())
        .map_err(|why| format!("path `{path}` {why}; not touching it"))
}

/// What is wrong with `targets` in the worktree `root` on disk, each line
/// starting with the target's name in `names`: a `dir`, `path` or `key`
/// that leads out of the worktree through a symlink ([`real_rel`]), and a
/// `path` that holds tracked files (one `git ls-files`), which moving it
/// away would take with it.
pub fn check_fs(root: &Path, targets: &[Target], names: &[String]) -> Vec<String> {
    check_each(root, targets, names)
        .into_iter()
        .map(|(_, e)| e)
        .collect()
}

/// [`check_fs`], each problem with the index in `targets` of the target it
/// refuses: `None` for one that is no single target's (the tracked files
/// cannot be listed).
fn check_each(root: &Path, targets: &[Target], names: &[String]) -> Vec<(Option<usize>, String)> {
    let mut errors = Vec::new();
    let mut paths: Vec<(usize, String)> = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let name = &names[i];
        if t.dir != "." {
            if let Err(why) = real_rel(root, &t.dir) {
                errors.push((Some(i), format!("{name}: dir `{}` {why}", t.dir)));
            }
        }
        match real_rel(root, &t.path) {
            Ok(real) => paths.push((i, real)),
            Err(why) => errors.push((Some(i), format!("{name}: path `{}` {why}", t.path))),
        }
        for k in &t.key {
            if let Err(why) = real_rel(root, k) {
                errors.push((Some(i), format!("{name}: key `{k}` {why}")));
            }
        }
    }
    if paths.is_empty() {
        return errors;
    }
    let specs: Vec<String> = paths
        .iter()
        .map(|(_, p)| format!(":(literal){p}"))
        .collect();
    let spec_refs: Vec<&str> = specs.iter().map(String::as_str).collect();
    match git::tracked(root, &spec_refs) {
        Ok(files) => {
            for (i, p) in &paths {
                if let Some(f) = files.iter().find(|f| under(f, p)) {
                    errors.push((
                        Some(*i),
                        format!(
                            "{}: path `{}` holds tracked files ({f}), \
                             and fast-worktree moves the whole directory away",
                            names[*i], targets[*i].path
                        ),
                    ));
                }
            }
        }
        Err(e) => errors.push((None, format!("cannot list the tracked files: {e}"))),
    }
    errors
}

/// Print `line` on stderr, once per process: [`targets`] runs more than
/// once in one command.
fn warn_once(line: String) {
    static SAID: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if !said.contains(&line) {
        crate::util::warn(&line);
        said.push(line);
    }
}

/// Whether the directory `rel` of the worktree `root` holds tracked files.
/// One that cannot be told counts as holding some.
pub fn holds_tracked(root: &Path, rel: &str) -> bool {
    git::tracked(root, &[&format!(":(literal){rel}")]).map_or(true, |f| !f.is_empty())
}

/// What the worktree at `root` gets: the `[[clone]]` entries of `cfg`, or,
/// without any, the projects [`discover`] finds, but those `[[ignore]]`
/// leaves out. A target that would have fast-worktree touch what is not
/// its own ([`check_fs`]) is an error when a `[[clone]]` names it; one that
/// was detected is left out, with a warning on stderr, and the others are
/// still handled.
pub fn targets(root: &Path, cfg: &Config) -> Result<Vec<Target>> {
    let file = crate::hooks::FILE;
    if cfg.clone.is_empty() {
        let mut found = discover(root)?;
        found.retain(|t| !cfg.ignore.iter().any(|i| i.covers(t.preset.name(), &t.dir)));
        let names: Vec<String> = found.iter().map(Target::label).collect();
        let problems = check_each(root, &found, &names);
        if let Some((_, e)) = problems.iter().find(|(i, _)| i.is_none()) {
            return Err(Error(e.clone()));
        }
        for (_, e) in &problems {
            warn_once(format!(
                "fast-worktree: {e}; left out (an [[ignore]] in {file} leaves it out without this warning)\n"
            ));
        }
        let refused: Vec<usize> = problems.iter().filter_map(|(i, _)| *i).collect();
        return Ok(found
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !refused.contains(i))
            .map(|(_, t)| t)
            .collect());
    }
    let found = resolve(&cfg.clone).map_err(|e| Error(format!("{file}: {}", e.join("; "))))?;
    let names: Vec<String> = (0..found.len()).map(|i| format!("clone[{i}]")).collect();
    let errors = check_fs(root, &found, &names);
    if !errors.is_empty() {
        return Err(Error(format!("{file}: {}", errors.join("; "))));
    }
    Ok(found)
}

/// The tracked lockfiles of every preset, relative to `root`, from one
/// `git ls-files` call: `(preset, project dir)` in path order.
pub fn tracked_lockfiles(root: &Path) -> Result<Vec<(&'static dyn Preset, String)>> {
    let presets = super::presets();
    let specs: Vec<String> = presets
        .iter()
        .map(|p| format!(":(glob)**/{}", p.manifest()))
        .collect();
    let spec_refs: Vec<&str> = specs.iter().map(String::as_str).collect();
    let files = git::tracked(root, &spec_refs)?;
    let mut out: Vec<(&'static dyn Preset, String)> = Vec::new();
    for f in files {
        let (dir, base) = match f.rsplit_once('/') {
            Some((d, b)) => (d.to_string(), b),
            None => (".".to_string(), f.as_str()),
        };
        let Some(preset) = presets.iter().find(|p| p.manifest() == base) else {
            continue;
        };
        out.push((*preset, dir));
    }
    out.sort_by(|a, b| (a.1.as_str(), a.0.label()).cmp(&(b.1.as_str(), b.0.label())));
    out.dedup_by(|a, b| a.1 == b.1 && a.0.name() == b.0.name());
    Ok(out)
}

/// Tracked projects of every preset, in path order, from one `git
/// ls-files` call, each with its preset's defaults.
pub fn discover(root: &Path) -> Result<Vec<Target>> {
    Ok(tracked_lockfiles(root)?
        .into_iter()
        // A sparse checkout can track a lockfile that is not on disk.
        .filter(|(p, dir)| root.join(join(dir, p.manifest())).is_file())
        .map(|(p, dir)| Target::with_defaults(p, &dir))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(toml: &str) -> CloneEntry {
        let cfg = crate::hooks::parse(&format!("[[clone]]\n{toml}\n"))
            .unwrap()
            .0;
        cfg.clone.into_iter().next().unwrap()
    }

    fn one(toml: &str) -> std::result::Result<Target, Vec<String>> {
        resolve(&[entry(toml)]).map(|mut v| v.remove(0))
    }

    #[test]
    fn a_preset_fills_in_its_defaults() {
        let t = one("preset = \"npm\"\ndir = \"api\"").unwrap();
        assert_eq!(t.preset.name(), "npm");
        assert_eq!(t.name, "api");
        assert_eq!(t.dir, "api");
        assert_eq!(t.path, "api/node_modules");
        assert_eq!(t.key, vec!["api/package-lock.json"]);
        assert_eq!(t.key_commands, vec!["node -v"]);
        assert_eq!(t.install, None);
        assert!(t.key_is_lockfile_only());
        assert!(t.has_preset());
        let t = one("preset = \"dart\"\ndir = \"./mobile/\"").unwrap();
        assert_eq!(t.path, "mobile/.dart_tool");
        assert_eq!(t.key, vec!["mobile/pubspec.lock"]);
        assert!(t.key_commands.is_empty());
        assert_eq!(t.label(), "mobile (pub)");
        let t = one("preset = \"npm\"").unwrap();
        assert_eq!((t.dir.as_str(), t.path.as_str()), (".", "node_modules"));
        assert_eq!(t.key, vec!["package-lock.json"]);
    }

    #[test]
    fn every_default_can_be_replaced() {
        let t = one(r#"preset = "npm"
dir = "web"
key = ["web/package-lock.json", "web/.npmrc"]
key_commands = ["node -v", "npm -v"]
install = "npm ci --legacy-peer-deps"
verify = "test -d .bin"
rewrite_paths = true
after = ["echo a", "echo b"]"#)
        .unwrap();
        assert_eq!(t.key, vec!["web/package-lock.json", "web/.npmrc"]);
        assert!(!t.key_is_lockfile_only());
        assert_eq!(t.key_commands, vec!["node -v", "npm -v"]);
        assert_eq!(t.install.as_deref(), Some("npm ci --legacy-peer-deps"));
        assert_eq!(t.verify.as_deref(), Some("test -d .bin"));
        assert!(t.rewrite_paths);
        assert_eq!(t.after, vec!["echo a", "echo b"]);
        let t = one("preset = \"npm\"\ndir = \"web\"\nkey_commands = []").unwrap();
        assert!(t.key_commands.is_empty());
    }

    #[test]
    fn a_preset_path_names_its_dir_or_must_agree_with_it() {
        let t = one("preset = \"npm\"\npath = \"api/node_modules\"").unwrap();
        assert_eq!(t.dir, "api");
        let t = one("preset = \"npm\"\npath = \"node_modules\"").unwrap();
        assert_eq!(t.dir, ".");
        let t = one("preset = \"npm\"\ndir = \"api\"\npath = \"./api/node_modules/\"").unwrap();
        assert_eq!(t.path, "api/node_modules");
        let e = one("preset = \"npm\"\ndir = \"api\"\npath = \"web/node_modules\"").unwrap_err();
        assert!(
            e[0].contains("does not match preset `npm` with dir `api`"),
            "{e:?}"
        );
        let e = one("preset = \"npm\"\npath = \"api/vendor\"").unwrap_err();
        assert!(e[0].contains("it is `<dir>/node_modules`"), "{e:?}");
        let e = one("preset = \"npm\"\npath = \"apinode_modules\"").unwrap_err();
        assert!(e[0].contains("does not match"), "{e:?}");
        let e = one("preset = \"dart\"\npath = \"mobile/node_modules\"").unwrap_err();
        assert!(e[0].contains("<dir>/.dart_tool"), "{e:?}");
    }

    #[test]
    fn without_a_preset_path_and_key_are_required() {
        let t = one(r#"path = "mobile/ios/Pods"
key = ["mobile/ios/Podfile.lock"]
install = "pod install""#)
        .unwrap();
        assert!(!t.has_preset());
        assert_eq!(t.name, "mobile/ios/Pods");
        assert_eq!(t.dir, "mobile/ios", "the parent of path");
        assert_eq!(t.label(), "mobile/ios/Pods (clone)");
        assert!(t.key_commands.is_empty() && t.after.is_empty() && !t.rewrite_paths);
        let t = one("path = \"build\"\nkey = [\"a\"]\ndir = \"tools\"").unwrap();
        assert_eq!((t.dir.as_str(), t.install.as_deref()), ("tools", None));
        let t = one("path = \"build\"\nkey = [\"a\"]").unwrap();
        assert_eq!(t.dir, ".");
        assert!(one("key = [\"a\"]").unwrap_err()[0].contains("`path`"));
        assert!(one("path = \"x\"").unwrap_err()[0].contains("`key`"));
        assert!(one("path = \"x\"\nkey = []").unwrap_err()[0].contains("`key`"));
        assert!(one("path = \".\"\nkey = [\"a\"]").unwrap_err()[0].contains("root"));
    }

    #[test]
    fn paths_stay_inside_the_worktree_and_presets_must_exist() {
        let e = one("path = \"/abs\"\nkey = [\"../up\"]").unwrap_err();
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(e[0].contains("path `/abs` must be relative"), "{e:?}");
        assert!(e[1].contains("key `../up` must not leave"), "{e:?}");
        let e = one("preset = \"bower\"").unwrap_err();
        assert_eq!(
            e,
            vec!["clone[0]: unknown preset `bower` (known: npm, dart, bundler)"]
        );
    }

    #[test]
    fn every_problem_is_named_by_its_entry() {
        let entries = vec![
            entry("preset = \"npm\"\ndir = \"a\""),
            entry("path = \"x\""),
            entry("preset = \"npm\"\npath = \"a/node_modules\""),
        ];
        let e = resolve(&entries).unwrap_err();
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(e[0].starts_with("clone[1]: "), "{e:?}");
        assert!(
            e[1].starts_with("clone[2]: path `a/node_modules` is also clone[0]'s"),
            "{e:?}"
        );
    }

    #[test]
    fn listed_targets_are_all_there_is() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::write(root.join("web/package-lock.json"), "{}").unwrap();
        git(&["add", "web/package-lock.json"]);
        // With [[clone]] entries nothing is detected.
        let cfg = crate::hooks::parse("[[clone]]\npreset = \"npm\"\ndir = \"api\"\n")
            .unwrap()
            .0;
        let got = targets(root, &cfg).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "api");
        let bad = crate::hooks::parse("[[clone]]\npreset = \"none\"\n")
            .unwrap()
            .0;
        let e = targets(root, &bad).unwrap_err().0;
        assert!(
            e.starts_with(".fast-worktree.toml: clone[0]: unknown preset"),
            "{e}"
        );
        // Without any, they are detected.
        let got = targets(root, &Config::default()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "web");
        assert!(targets(Path::new("/nonexistent-root"), &Config::default()).is_err());
    }

    #[test]
    fn a_detected_target_that_is_refused_leaves_the_others() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        for f in [
            "web/package-lock.json",
            "api/package-lock.json",
            // Moving api/node_modules away would take this with it.
            "api/node_modules/patched/index.js",
        ] {
            std::fs::create_dir_all(root.join(f).parent().unwrap()).unwrap();
            std::fs::write(root.join(f), "{}").unwrap();
            git(&["add", "-f", f]);
        }
        let got = targets(root, &Config::default()).unwrap();
        let names: Vec<&str> = got.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["web"]);
        // Named by a [[clone]], it stops everything, as before.
        let cfg = crate::hooks::parse(
            "[[clone]]\npreset = \"npm\"\ndir = \"api\"\n\n[[clone]]\npreset = \"npm\"\ndir = \"web\"\n",
        )
        .unwrap()
        .0;
        let e = targets(root, &cfg).unwrap_err().0;
        assert!(
            e.starts_with(
                ".fast-worktree.toml: clone[0]: path `api/node_modules` holds tracked files"
            ),
            "{e}"
        );
    }

    #[test]
    fn a_path_that_holds_another_targets_path_or_dir_is_refused() {
        let entries = vec![
            entry("preset = \"npm\"\ndir = \"app\""),
            entry("path = \"app/node_modules/.cache/x\"\nkey = [\"k\"]"),
            entry("path = \"vendor\"\nkey = [\"k\"]"),
            entry("preset = \"bundler\"\ndir = \"vendor/gemsite\""),
            entry("path = \"out/x\"\nkey = [\"k\"]\ndir = \"tools/gen\""),
            entry("path = \"tools\"\nkey = [\"k\"]"),
        ];
        let e = resolve(&entries).unwrap_err();
        assert_eq!(e.len(), 3, "{e:?}");
        assert!(
            e[0].starts_with(
                "clone[0]: path `app/node_modules` holds clone[1]'s path `app/node_modules/.cache/x`"
            ),
            "{e:?}"
        );
        assert!(
            e[1].starts_with("clone[2]: path `vendor` holds clone[3]'s path"),
            "{e:?}"
        );
        assert!(
            e[2].starts_with("clone[5]: path `tools` holds clone[4]'s dir `tools/gen`"),
            "{e:?}"
        );
        // A path under another target's dir is how a project's own
        // directories are listed.
        let ok = vec![
            entry("preset = \"npm\"\ndir = \"mobile\""),
            entry("path = \"mobile/ios/Pods\"\nkey = [\"k\"]"),
            entry("path = \"build\"\nkey = [\"k\"]"),
        ];
        assert!(resolve(&ok).is_ok());
        assert!(under("a/b", "a") && under("a", "a") && under("x", "."));
        assert!(!under("ab", "a") && !under("a", "a/b"));
    }

    #[test]
    fn real_paths_stay_in_the_worktree() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let root = base.join("wt");
        std::fs::create_dir_all(root.join("ios/real")).unwrap();
        std::fs::create_dir_all(base.join("outside/Pods")).unwrap();
        let link = |to: &str, at: &str| std::os::unix::fs::symlink(to, root.join(at)).unwrap();
        link("../outside", "out");
        link("ios/real", "in");
        link("..", "up");
        link("nowhere", "dangling");
        link(".", "self");
        assert_eq!(real_rel(&root, "ios/Pods").unwrap(), "ios/Pods");
        assert_eq!(real_rel(&root, "./ios/real/x/y").unwrap(), "ios/real/x/y");
        assert_eq!(real_rel(&root, "in/Pods").unwrap(), "ios/real/Pods");
        assert_eq!(real_rel(&root, "in").unwrap(), "ios/real");
        assert_eq!(real_rel(&root, "self/ios").unwrap(), "ios");
        for bad in ["out/Pods", "out", "up/wt/ios", "out/none/deeper"] {
            let e = real_rel(&root, bad).unwrap_err();
            assert!(e.contains("outside the worktree"), "{bad}: {e}");
        }
        assert!(real_rel(&root, "dangling/x")
            .unwrap_err()
            .contains("points nowhere"));
        assert!(real_rel(&root, "self").unwrap_err().contains("root"));
        assert!(real_rel(&root, "../wt/ios").is_err());
        assert!(inside(&root, "out/Pods")
            .unwrap_err()
            .starts_with("path `out/Pods` goes through the symlink "));
        assert!(inside(&root, "ios/Pods").is_ok());
    }

    #[test]
    fn normalized_paths() {
        assert_eq!(normalize("./a/b/"), "a/b");
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("./"), ".");
        assert_eq!(normalize("a"), "a");
        assert_eq!(join(".", "x"), "x");
        assert_eq!(join("a", "x"), "a/x");
    }
}
