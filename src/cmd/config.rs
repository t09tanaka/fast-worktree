//! `fast-worktree config init | check`: write and check the `[[clone]]`
//! entries of `.fast-worktree.toml`.
//!
//! `init` adds a `[[clone]]` for each tracked lockfile that a preset knows
//! and that has none yet, nor an `[[ignore]]`, keeping what the file says
//! (comments included). An entry that does not resolve stops it, so that
//! it never adds a second one for the same project. `check` is for a
//! pre-commit hook or CI: a few `git ls-files` and a look at which files
//! exist, then exit 1 when the file has a mistake in it (it does not parse,
//! an unknown key or section, an unknown preset, a `path` that does not fit
//! its preset, holds tracked files or another target, or leaves the
//! worktree through a symlink), a tracked lockfile is in no `[[clone]]` and
//! no `[[ignore]]`, a `[[clone]]` names a file that is not there or not
//! tracked, or an `[[ignore]]` names a directory without a tracked
//! lockfile. Without any `[[clone]]` (targets are detected) the lockfiles
//! are not compared, but the rest is checked, and it says so. With
//! `--staged`, the file and which files exist come from the index, so that
//! a pre-commit hook checks what is committed, not what is on disk; what
//! `install` would refuse (symlinks, tracked files under a `path`) is still
//! looked at on disk.

use crate::ecosystem::target::{self, join, Target};
use crate::error::{bail, Error, Result};
use crate::hooks::{self, FILE};
use crate::util::outln;
use crate::{git, util};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn root_here() -> Result<PathBuf> {
    let root = git::toplevel(Path::new("."))?;
    Ok(root.canonicalize().unwrap_or(root))
}

fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(Error(format!("{}: {e}", path.display()))),
    }
}

/// The `(preset, dir)` of each `[[clone]]` with a preset that resolves.
fn listed(cfg: &hooks::Config) -> Vec<(String, String)> {
    cfg.clone
        .iter()
        .enumerate()
        .filter_map(|(i, e)| target::resolve_entry(i, e).ok())
        .filter(Target::has_preset)
        .map(|t| (t.preset.name().to_string(), t.dir))
        .collect()
}

/// `config init [--dry-run]`.
pub fn init(dry_run: bool) -> Result<i32> {
    let root = root_here()?;
    let path = root.join(FILE);
    let text = read(&path)?;
    let (cfg, _) = hooks::parse(&text).map_err(|e| Error(format!("{FILE}: {e}")))?;
    // An entry that does not resolve names no project: one for its
    // project would be added next to it.
    if let Err(errors) = target::resolve(&cfg.clone) {
        bail!(
            "{FILE}: {}; nothing was added (fix it first: `fast-worktree config check` lists every problem)",
            errors.join("; ")
        );
    }
    let had_clone = !cfg.clone.is_empty();
    let listed = listed(&cfg);
    let missing: Vec<(&'static str, String)> = target::tracked_lockfiles(&root)?
        .into_iter()
        .filter(|(p, dir)| root.join(join(dir, p.manifest())).is_file())
        .filter(|(p, dir)| !listed.iter().any(|(n, d)| n == p.name() && d == dir))
        .filter(|(p, dir)| !cfg.ignore.iter().any(|i| i.covers(p.name(), dir)))
        .map(|(p, dir)| (p.name(), dir))
        .collect();
    if missing.is_empty() {
        outln!(
            "nothing to add: every tracked lockfile is in a [[clone]] or an [[ignore]] of {FILE}"
        );
        return Ok(0);
    }
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| Error(format!("{FILE}: {e}")))?;
    let added = add_entries(&mut doc, &missing)?;
    if dry_run {
        util::say(&added);
        outln!("(dry run: {FILE} was not written)");
        return Ok(0);
    }
    std::fs::write(&path, doc.to_string()).map_err(|e| Error(format!("{FILE}: {e}")))?;
    for (preset, dir) in &missing {
        outln!("add    [[clone]] preset = \"{preset}\", dir = \"{dir}\"");
    }
    if !had_clone {
        outln!(
            "note   with [[clone]] in {FILE}, only what it lists is installed; \
             `fast-worktree config check` finds lockfiles it leaves out"
        );
    }
    Ok(0)
}

/// Append a `[[clone]]` per `(preset, dir)` to `doc`. Returns the added
/// entries as TOML.
fn add_entries(doc: &mut toml_edit::DocumentMut, add: &[(&str, String)]) -> Result<String> {
    use toml_edit::{value, ArrayOfTables, Item, Table};
    if !doc.contains_key("clone") {
        doc.insert("clone", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let Some(list) = doc["clone"].as_array_of_tables_mut() else {
        bail!("{FILE}: `clone` must be an array of tables ([[clone]])");
    };
    let mut shown = toml_edit::DocumentMut::new();
    let mut shown_list = ArrayOfTables::new();
    for (preset, dir) in add {
        let mut t = Table::new();
        t["preset"] = value(*preset);
        t["dir"] = value(dir.as_str());
        list.push(t.clone());
        shown_list.push(t);
    }
    shown.insert("clone", Item::ArrayOfTables(shown_list));
    Ok(shown.to_string())
}

/// One thing `config check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// A mistake in the file: an unknown key, a preset that does not exist,
    /// a `path` that does not fit its preset...
    Invalid(String),
    /// A tracked lockfile of a preset that no `[[clone]]` names.
    Unlisted { lockfile: String, preset: String },
    /// A file a `[[clone]]` needs that is not there.
    Missing { entry: usize, file: String },
    /// A `key` file that git does not track: a new worktree has another
    /// one, or none.
    Untracked { entry: usize, file: String },
    /// An `[[ignore]]` whose directory has no tracked lockfile (of its
    /// preset, when it names one).
    IgnoredMissing { entry: usize, what: String },
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::Invalid(why) => write!(f, "invalid    {why}"),
            Problem::Unlisted { lockfile, preset } => write!(
                f,
                "unlisted   {lockfile} ({preset}) is in no [[clone]]; \
                 `fast-worktree config init` adds it"
            ),
            Problem::Missing { entry, file } => {
                write!(f, "missing    clone[{entry}]: {file} does not exist")
            }
            Problem::Untracked { entry, file } => write!(
                f,
                "untracked  clone[{entry}]: {file} is not tracked by git, so another worktree \
                 has its own or none"
            ),
            Problem::IgnoredMissing { entry, what } => {
                write!(f, "missing    ignore[{entry}]: no tracked {what}")
            }
        }
    }
}

/// What [`target::check_fs`] finds wrong with targets, named so.
pub type FsCheck<'a> = &'a dyn Fn(&[Target], &[String]) -> Vec<String>;

/// What `problems` asks of the worktree.
pub struct Disk<'a> {
    /// Whether a file exists.
    pub exists: &'a dyn Fn(&str) -> bool,
    /// Which of these files git tracks.
    pub tracked: &'a dyn Fn(&[String]) -> Vec<String>,
    /// What [`target::check_fs`] finds wrong with these targets, named so.
    pub fs: FsCheck<'a>,
}

/// What is wrong with the config `text` of a worktree, given its tracked
/// lockfiles (`(preset name, dir)`) and what `disk` says. `None` when
/// there is nothing to check: no `[[clone]]`, and nothing wrong with the
/// rest.
pub fn problems(text: &str, tracked: &[(&str, String)], disk: &Disk) -> Option<Vec<Problem>> {
    let (cfg, warnings) = match hooks::parse(text) {
        Ok(p) => p,
        Err(e) => return Some(vec![Problem::Invalid(e)]),
    };
    let mut out: Vec<Problem> = warnings
        .into_iter()
        .map(|w| Problem::Invalid(w.trim_end_matches(", ignored").to_string()))
        .collect();
    if cfg.clone.is_empty() {
        // Detected: what install would refuse, and [[ignore]] entries.
        let found: Vec<Target> = tracked
            .iter()
            .filter(|(p, d)| !cfg.ignore.iter().any(|i| i.covers(p, d)))
            .filter_map(|(p, d)| crate::ecosystem::preset(p).map(|p| Target::with_defaults(p, d)))
            .filter(|t| (disk.exists)(&join(&t.dir, t.preset.manifest())))
            .collect();
        let names: Vec<String> = found.iter().map(Target::label).collect();
        out.extend((disk.fs)(&found, &names).into_iter().map(Problem::Invalid));
        out.extend(ignored_missing(&cfg, tracked));
        return (!out.is_empty()).then_some(out);
    }
    match target::resolve(&cfg.clone) {
        Err(errors) => out.extend(errors.into_iter().map(Problem::Invalid)),
        Ok(found) => {
            let names: Vec<String> = (0..found.len()).map(|i| format!("clone[{i}]")).collect();
            out.extend((disk.fs)(&found, &names).into_iter().map(Problem::Invalid));
        }
    }
    let listed = listed(&cfg);
    for (i, ig) in cfg.ignore.iter().enumerate() {
        if let Some((_, d)) = listed.iter().find(|(n, d)| ig.covers(n, d)) {
            out.push(Problem::Invalid(format!(
                "ignore[{i}]: `{d}` is also in a [[clone]]; leave it in one of them"
            )));
        }
    }
    for (preset, dir) in tracked {
        if cfg.ignore.iter().any(|i| i.covers(preset, dir)) {
            continue;
        }
        if !listed.iter().any(|(n, d)| n == preset && d == dir) {
            let lockfile = crate::ecosystem::preset(preset)
                .map_or_else(String::new, |p| join(dir, p.manifest()));
            out.push(Problem::Unlisted {
                lockfile,
                preset: preset.to_string(),
            });
        }
    }
    let mut needed: Vec<(usize, String)> = Vec::new();
    for (i, e) in cfg.clone.iter().enumerate() {
        let Ok(t) = target::resolve_entry(i, e) else {
            continue;
        };
        let mut files = t.key.clone();
        if t.has_preset() {
            let lockfile = join(&t.dir, t.preset.manifest());
            if !files.contains(&lockfile) {
                files.insert(0, lockfile);
            }
        }
        needed.extend(files.into_iter().map(|f| (i, f)));
    }
    let present: Vec<String> = needed
        .iter()
        .filter(|(_, f)| (disk.exists)(f))
        .map(|(_, f)| f.clone())
        .collect();
    let in_git = (disk.tracked)(&present);
    for (i, f) in needed {
        if !(disk.exists)(&f) {
            out.push(Problem::Missing { entry: i, file: f });
        } else if !in_git.contains(&f) {
            out.push(Problem::Untracked { entry: i, file: f });
        }
    }
    out.extend(ignored_missing(&cfg, tracked));
    Some(out)
}

/// The `[[ignore]]` entries whose directory has no tracked lockfile (of
/// their preset).
fn ignored_missing(cfg: &hooks::Config, tracked: &[(&str, String)]) -> Vec<Problem> {
    let mut out = Vec::new();
    for (i, ig) in cfg.ignore.iter().enumerate() {
        if !tracked.iter().any(|(p, d)| ig.covers(p, d)) {
            let dir = target::normalize(&ig.dir);
            let what = match ig.preset.as_deref().and_then(crate::ecosystem::preset) {
                Some(p) => join(&dir, p.manifest()),
                None => format!("lockfile in {dir}"),
            };
            out.push(Problem::IgnoredMissing { entry: i, what });
        }
    }
    out
}

/// Every path in the index.
fn index_files(root: &Path) -> Result<HashSet<String>> {
    Ok(git::tracked(root, &[])?.into_iter().collect())
}

/// `config check [--staged]`.
pub fn check(staged: bool) -> Result<i32> {
    let root = root_here()?;
    let index = if staged {
        Some(index_files(&root)?)
    } else {
        None
    };
    let text = match &index {
        Some(files) if !files.contains(FILE) => String::new(),
        Some(_) => git::output(&root, &["cat-file", "blob", &format!(":{FILE}")])?,
        None => read(&root.join(FILE))?,
    };
    let place = if staged { " in the index" } else { "" };
    if text.trim().is_empty() {
        outln!(
            "nothing to check: no {FILE}{place}, so every tracked lockfile is a project \
             (`fast-worktree config init` lists them in [[clone]] entries)"
        );
        return Ok(0);
    }
    let tracked: Vec<(&str, String)> = target::tracked_lockfiles(&root)?
        .into_iter()
        .map(|(p, d)| (p.name(), d))
        .collect();
    let exists = |f: &str| match &index {
        Some(files) => files.contains(f),
        None => root.join(f).is_file(),
    };
    // One `git ls-files`, by where each file really is (a file through a
    // symlink is one `check_fs` reports). With the index, what exists is
    // tracked.
    let in_git = |files: &[String]| -> Vec<String> {
        if let Some(index) = &index {
            return files
                .iter()
                .filter(|f| index.contains(*f))
                .cloned()
                .collect();
        }
        let real: Vec<(String, String)> = files
            .iter()
            .filter_map(|f| target::real_rel(&root, f).ok().map(|r| (f.clone(), r)))
            .collect();
        if real.is_empty() {
            return Vec::new();
        }
        let specs: Vec<String> = real.iter().map(|(_, r)| format!(":(literal){r}")).collect();
        let refs: Vec<&str> = specs.iter().map(String::as_str).collect();
        let listed = git::tracked(&root, &refs).unwrap_or_default();
        real.into_iter()
            .filter(|(_, r)| listed.contains(r))
            .map(|(f, _)| f)
            .collect()
    };
    let fs_check = |t: &[Target], names: &[String]| target::check_fs(&root, t, names);
    let disk = Disk {
        exists: &exists,
        tracked: &in_git,
        fs: &fs_check,
    };
    let Some(found) = problems(&text, &tracked, &disk) else {
        outln!(
            "config ok: no [[clone]] in {FILE}{place}, so every tracked lockfile is a project \
             and none is reported as unlisted (`fast-worktree config init` lists them)"
        );
        return Ok(0);
    };
    if found.is_empty() {
        outln!(
            "config ok: every tracked lockfile is in a [[clone]] or an [[ignore]], \
             and every file they name is tracked"
        );
        return Ok(0);
    }
    for p in &found {
        outln!("{p}");
    }
    outln!(
        "{} {} in {FILE}",
        found.len(),
        if found.len() == 1 {
            "problem"
        } else {
            "problems"
        }
    );
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(v: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        v.iter().map(|(p, d)| (*p, d.to_string())).collect()
    }

    /// Every file exists and is tracked, and nothing is wrong on disk,
    /// unless `exists` says otherwise.
    fn problems_with(
        text: &str,
        t: &[(&str, String)],
        exists: impl Fn(&str) -> bool,
    ) -> Option<Vec<Problem>> {
        let all = |f: &[String]| f.to_vec();
        let none = |_: &[Target], _: &[String]| Vec::new();
        problems(
            text,
            t,
            &Disk {
                exists: &exists,
                tracked: &all,
                fs: &none,
            },
        )
    }

    #[test]
    fn nothing_to_check_without_a_clone() {
        assert_eq!(
            problems_with("", &tracked(&[("npm", "api")]), |_| true),
            None
        );
        assert_eq!(
            problems_with("[[post_add]]\nrun = \"x\"\n", &tracked(&[]), |_| true),
            None
        );
    }

    #[test]
    fn unlisted_lockfiles_missing_files_and_mistakes() {
        let text = r#"
[[clone]]
preset = "npm"
dir = "api"

[[clone]]
preset = "npm"
dir = "web"
paht = "x"

[[clone]]
path = "ios/Pods"
key = ["ios/Podfile.lock"]

[[clone]]
preset = "dart"
dir = "mobile"
path = "app/.dart_tool"
"#;
        let t = tracked(&[
            ("npm", "api"),
            ("npm", "web"),
            ("npm", "lambda"),
            ("dart", "mobile"),
        ]);
        let got = problems_with(text, &t, |f| {
            f != "ios/Podfile.lock" && f != "web/package-lock.json"
        })
        .unwrap();
        let shown: Vec<String> = got.iter().map(|p| p.to_string()).collect();
        assert_eq!(
            shown,
            vec![
                "invalid    unknown key `paht` in clone[1]",
                "invalid    clone[3]: path `app/.dart_tool` does not match preset `dart` \
                 with dir `mobile`: it would be `mobile/.dart_tool`",
                "unlisted   lambda/package-lock.json (npm) is in no [[clone]]; \
                 `fast-worktree config init` adds it",
                "unlisted   mobile/pubspec.lock (dart) is in no [[clone]]; \
                 `fast-worktree config init` adds it",
                "missing    clone[1]: web/package-lock.json does not exist",
                "missing    clone[2]: ios/Podfile.lock does not exist",
            ]
        );
    }

    #[test]
    fn a_clean_config_and_one_that_does_not_parse() {
        let text = "[[clone]]\npreset = \"npm\"\ndir = \"api\"\n";
        assert_eq!(
            problems_with(text, &tracked(&[("npm", "api")]), |_| true),
            Some(vec![])
        );
        let bad = problems_with("[[clone]\n", &tracked(&[]), |_| true).unwrap();
        assert!(matches!(&bad[..], [Problem::Invalid(_)]), "{bad:?}");
        // A key that replaces the lockfile does not make the lockfile optional.
        let text = "[[clone]]\npreset = \"npm\"\ndir = \"api\"\nkey = [\"api/.npmrc\"]\n";
        let got = problems_with(text, &tracked(&[("npm", "api")]), |f| f == "api/.npmrc").unwrap();
        assert_eq!(
            got,
            vec![Problem::Missing {
                entry: 0,
                file: "api/package-lock.json".into()
            }]
        );
    }

    #[test]
    fn ignored_projects_are_not_unlisted_and_must_exist() {
        let text = r#"
[[clone]]
preset = "npm"
dir = "api"

[[ignore]]
dir = "docs-site"
reason = "built in CI only"

[[ignore]]
dir = "web"
preset = "bundler"
reason = "the Ruby side is not used here"

[[ignore]]
dir = "gone"
reason = "removed"

[[ignore]]
dir = "web"
preset = "dart"
reason = "no Flutter"

[[ignore]]
dir = "api"
reason = "listed too"
"#;
        let t = tracked(&[
            ("npm", "api"),
            ("npm", "docs-site"),
            ("bundler", "docs-site"),
            ("npm", "web"),
            ("bundler", "web"),
        ]);
        let shown: Vec<String> = problems_with(text, &t, |_| true)
            .unwrap()
            .iter()
            .map(|p| p.to_string())
            .collect();
        assert_eq!(
            shown,
            vec![
                "invalid    ignore[4]: `api` is also in a [[clone]]; leave it in one of them",
                "unlisted   web/package-lock.json (npm) is in no [[clone]]; \
                 `fast-worktree config init` adds it",
                "missing    ignore[2]: no tracked lockfile in gone",
                "missing    ignore[3]: no tracked web/pubspec.lock",
            ]
        );
    }

    #[test]
    fn mistakes_outside_clone_and_untracked_key_files() {
        let t = tracked(&[("npm", "api"), ("npm", "web")]);
        let shown = |text: &str| -> Vec<String> {
            problems_with(text, &t, |_| true)
                .unwrap_or_default()
                .iter()
                .map(|p| p.to_string())
                .collect()
        };
        // A misspelled section is no [[clone]], and is a mistake.
        assert_eq!(
            shown("[[clones]]\npreset = \"npm\"\ndir = \"api\"\n"),
            vec!["invalid    unknown key `clones`"]
        );
        assert_eq!(
            shown("[[Clone]]\npreset = \"npm\"\ndir = \"api\"\n"),
            vec!["invalid    unknown key `Clone`"]
        );
        let e = shown("[[ignore]]\ndir = \"web\"\n");
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(
            e[0].starts_with("invalid    ignore[0]: `reason` is required"),
            "{e:?}"
        );
        let e = shown("[pool]\nsize = \"two\"\n");
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].starts_with("invalid    "), "{e:?}");
        // Without [[clone]], a clean file has nothing to say.
        assert_eq!(problems_with("[pool]\nsize = 1\n", &t, |_| true), None);
        // What install would refuse, also when detected.
        let refused = |_: &[Target], names: &[String]| {
            names.iter().map(|n| format!("{n}: path refused")).collect()
        };
        let all = |f: &[String]| f.to_vec();
        let yes = |_: &str| true;
        let disk = Disk {
            exists: &yes,
            tracked: &all,
            fs: &refused,
        };
        let got: Vec<String> = problems("[pool]\nsize = 1\n", &t, &disk)
            .unwrap()
            .iter()
            .map(|p| p.to_string())
            .collect();
        assert_eq!(
            got,
            vec![
                "invalid    api (npm): path refused",
                "invalid    web (npm): path refused"
            ]
        );
        // A key file that exists here but is not tracked.
        let text = "[[clone]]\npreset = \"npm\"\ndir = \"api\"\n[[clone]]\npreset = \"npm\"\n\
                    dir = \"web\"\nkey = [\"web/package-lock.json\", \"web/.npmrc\"]\n";
        let only_locks = |f: &[String]| -> Vec<String> {
            f.iter().filter(|f| f.ends_with(".json")).cloned().collect()
        };
        let none = |_: &[Target], _: &[String]| Vec::new();
        let disk = Disk {
            exists: &yes,
            tracked: &only_locks,
            fs: &none,
        };
        assert_eq!(
            problems(text, &t, &disk).unwrap(),
            vec![Problem::Untracked {
                entry: 1,
                file: "web/.npmrc".into()
            }]
        );
    }

    #[test]
    fn init_appends_entries_and_keeps_the_rest() {
        let text = "# hooks first\n[[post_add]]\nrun = \"make\" # setup\n\n[pool]\nsize = 1\n";
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        let shown = add_entries(
            &mut doc,
            &[("npm", "api".to_string()), ("dart", ".".to_string())],
        )
        .unwrap();
        assert_eq!(
            shown,
            "[[clone]]\npreset = \"npm\"\ndir = \"api\"\n\n[[clone]]\npreset = \"dart\"\ndir = \".\"\n"
        );
        let out = doc.to_string();
        assert!(out.starts_with(text), "{out}");
        let (cfg, w) = hooks::parse(&out).unwrap();
        assert!(w.is_empty());
        assert_eq!(cfg.clone.len(), 2);
        assert_eq!(cfg.pool.unwrap().size, Some(1));
        let mut bad: toml_edit::DocumentMut = "clone = 1\n".parse().unwrap();
        assert!(add_entries(&mut bad, &[("npm", "a".into())]).is_err());
    }
}
