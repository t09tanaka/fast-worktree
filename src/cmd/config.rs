//! `fast-worktree config init | check`: write and check the `[[clone]]`
//! entries of `.fast-worktree.toml`.
//!
//! `init` adds a `[[clone]]` for each tracked lockfile that a preset knows
//! and that has none yet, keeping what the file says (comments included).
//! `check` is for a pre-commit hook or CI: one `git ls-files` and a look at
//! which files exist, then exit 1 when a tracked lockfile is in no
//! `[[clone]]`, a `[[clone]]` names a file that is not there, or the file
//! has a mistake in it. Without any `[[clone]]` (targets are detected) it
//! does nothing and exits 0.

use crate::ecosystem::target::{self, join, Target};
use crate::error::{bail, Error, Result};
use crate::hooks::{self, FILE};
use crate::util::outln;
use crate::{git, util};
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
    let had_clone = !cfg.clone.is_empty();
    let listed = listed(&cfg);
    let missing: Vec<(&'static str, String)> = target::tracked_lockfiles(&root)?
        .into_iter()
        .filter(|(p, dir)| root.join(join(dir, p.manifest())).is_file())
        .filter(|(p, dir)| !listed.iter().any(|(n, d)| n == p.name() && d == dir))
        .map(|(p, dir)| (p.name(), dir))
        .collect();
    if missing.is_empty() {
        outln!("nothing to add: every tracked lockfile is in a [[clone]] of {FILE}");
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
        }
    }
}

/// What is wrong with the config `text` of the worktree `root`, given its
/// tracked lockfiles (`(preset name, dir)`) and whether a file exists.
/// `None` when it has no `[[clone]]`: there is nothing to check.
pub fn problems(
    text: &str,
    tracked: &[(&str, String)],
    exists: impl Fn(&str) -> bool,
) -> Option<Vec<Problem>> {
    let (cfg, warnings) = match hooks::parse(text) {
        Ok(p) => p,
        Err(e) => return Some(vec![Problem::Invalid(e)]),
    };
    if cfg.clone.is_empty() {
        return None;
    }
    let mut out: Vec<Problem> = warnings
        .into_iter()
        .map(|w| Problem::Invalid(w.trim_end_matches(", ignored").to_string()))
        .collect();
    if let Err(errors) = target::resolve(&cfg.clone) {
        out.extend(errors.into_iter().map(Problem::Invalid));
    }
    let listed = listed(&cfg);
    for (preset, dir) in tracked {
        if !listed.iter().any(|(n, d)| n == preset && d == dir) {
            let lockfile = crate::ecosystem::preset(preset)
                .map_or_else(String::new, |p| join(dir, p.manifest()));
            out.push(Problem::Unlisted {
                lockfile,
                preset: preset.to_string(),
            });
        }
    }
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
        for f in files.into_iter().filter(|f| !exists(f)) {
            out.push(Problem::Missing { entry: i, file: f });
        }
    }
    Some(out)
}

/// `config check`.
pub fn check() -> Result<i32> {
    let root = root_here()?;
    let text = read(&root.join(FILE))?;
    // Without a [[clone]] there is nothing to compare: not even git runs.
    if !text.contains("clone") {
        return Ok(0);
    }
    let tracked: Vec<(&str, String)> = target::tracked_lockfiles(&root)?
        .into_iter()
        .map(|(p, d)| (p.name(), d))
        .collect();
    let Some(found) = problems(&text, &tracked, |f| root.join(f).is_file()) else {
        return Ok(0);
    };
    if found.is_empty() {
        outln!(
            "config ok: every tracked lockfile is in a [[clone]], and every file they name exists"
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

    #[test]
    fn nothing_to_check_without_a_clone() {
        assert_eq!(problems("", &tracked(&[("npm", "api")]), |_| true), None);
        assert_eq!(
            problems("[[post_add]]\nrun = \"x\"\n", &tracked(&[]), |_| true),
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
        let got = problems(text, &t, |f| {
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
            problems(text, &tracked(&[("npm", "api")]), |_| true),
            Some(vec![])
        );
        let bad = problems("[[clone]\n", &tracked(&[]), |_| true).unwrap();
        assert!(matches!(&bad[..], [Problem::Invalid(_)]), "{bad:?}");
        // A key that replaces the lockfile does not make the lockfile optional.
        let text = "[[clone]]\npreset = \"npm\"\ndir = \"api\"\nkey = [\"api/.npmrc\"]\n";
        let got = problems(text, &tracked(&[("npm", "api")]), |f| f == "api/.npmrc").unwrap();
        assert_eq!(
            got,
            vec![Problem::Missing {
                entry: 0,
                file: "api/package-lock.json".into()
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
