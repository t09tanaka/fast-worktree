//! Repository settings from `.fast-worktree.toml` at the worktree root.
//!
//! ```toml
//! [[post_add]]          # after add / install, in the worktree root, in order
//! run = "make setup"
//! requires = "make"     # optional: skipped silently when not installed
//!
//! [[pre_remove]]        # after remove's safety checks, before the move
//! run = "docker compose -p \"$(basename \"$FAST_WORKTREE_PATH\")\" down -v"
//! requires = "docker"
//!
//! [[skip_lifecycle]]    # after a clone, skip a project's own script when
//! dir = "api"           # every pair of files is byte-identical
//! script = "postinstall"
//! if_equal = [["schema.json", "node_modules/.codegen/schema.json"]]
//! ```
//!
//! fast-worktree itself knows nothing about the tools a repository uses;
//! anything repository-specific goes here.

use crate::error::{Error, Result};
use crate::util;
use serde::Deserialize;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const FILE: &str = ".fast-worktree.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub post_add: Vec<Hook>,
    #[serde(default)]
    pub pre_remove: Vec<Hook>,
    #[serde(default)]
    pub skip_lifecycle: Vec<SkipLifecycle>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Hook {
    pub run: String,
    pub requires: Option<String>,
}

/// Do not run the project's own lifecycle `script` after a clone when each
/// pair of files (relative to the project directory) is byte-identical. A
/// missing file never counts as identical, so the script runs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SkipLifecycle {
    #[serde(default = "root_dir")]
    pub dir: String,
    pub script: String,
    pub if_equal: Vec<[String; 2]>,
}

fn root_dir() -> String {
    ".".into()
}

impl SkipLifecycle {
    /// Whether this rule is about `script` of the project at `dir`.
    pub fn applies_to(&self, dir: &str, script: &str) -> bool {
        self.script == script && normalize_dir(&self.dir) == normalize_dir(dir)
    }

    /// Whether every pair under `project` is byte-identical.
    pub fn inputs_unchanged(&self, project: &Path) -> bool {
        !self.if_equal.is_empty()
            && self.if_equal.iter().all(|[a, b]| {
                match (
                    std::fs::read(project.join(a)),
                    std::fs::read(project.join(b)),
                ) {
                    (Ok(x), Ok(y)) => x == y,
                    _ => false,
                }
            })
    }
}

/// `./a/b/` and `a/b` name the same project; `""` and `./` are the root.
fn normalize_dir(d: &str) -> &str {
    let d = d.trim_start_matches("./").trim_end_matches('/');
    if d.is_empty() {
        "."
    } else {
        d
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    PostAdd,
    PreRemove,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Stage::PostAdd => "post_add",
            Stage::PreRemove => "pre_remove",
        }
    }
}

impl Config {
    /// Read `<root>/.fast-worktree.toml`. A missing file is an empty
    /// config; unknown keys are reported on stderr and ignored.
    pub fn load(root: &Path) -> Result<Config> {
        let path = root.join(FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(Error(format!("{}: {e}", path.display()))),
        };
        let (cfg, warnings) = parse(&text).map_err(|e| Error(format!("{FILE}: {e}")))?;
        for w in warnings {
            eprintln!("fast-worktree: {FILE}: {w}");
        }
        Ok(cfg)
    }

    pub fn hooks(&self, stage: Stage) -> &[Hook] {
        match stage {
            Stage::PostAdd => &self.post_add,
            Stage::PreRemove => &self.pre_remove,
        }
    }
}

/// The keys each array of tables accepts.
fn allowed_keys(section: &str) -> Option<&'static [&'static str]> {
    match section {
        "post_add" | "pre_remove" => Some(&["run", "requires"]),
        "skip_lifecycle" => Some(&["dir", "script", "if_equal"]),
        _ => None,
    }
}

/// Parse the config, returning it with warnings about unknown keys.
pub fn parse(text: &str) -> std::result::Result<(Config, Vec<String>), String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let mut warnings = Vec::new();
    let mut known = toml::Table::new();
    for (k, v) in table {
        let Some(allowed) = allowed_keys(&k) else {
            warnings.push(format!("unknown key `{k}`, ignored"));
            continue;
        };
        let Some(list) = v.as_array() else {
            return Err(format!("`{k}` must be an array of tables ([[{k}]])"));
        };
        let mut kept = Vec::new();
        for (i, item) in list.iter().enumerate() {
            let Some(t) = item.as_table() else {
                return Err(format!("`{k}` must be an array of tables ([[{k}]])"));
            };
            let mut t = t.clone();
            let unknown: Vec<String> = t
                .keys()
                .filter(|x| !allowed.contains(&x.as_str()))
                .cloned()
                .collect();
            for u in unknown {
                warnings.push(format!("unknown key `{u}` in {k}[{i}], ignored"));
                t.remove(&u);
            }
            kept.push(toml::Value::Table(t));
        }
        known.insert(k, toml::Value::Array(kept));
    }
    let cfg = Config::deserialize(toml::Value::Table(known)).map_err(|e| e.to_string())?;
    if let Some(i) = cfg
        .skip_lifecycle
        .iter()
        .position(|r| r.if_equal.is_empty())
    {
        return Err(format!(
            "skip_lifecycle[{i}]: `if_equal` must list at least one pair of files"
        ));
    }
    Ok((cfg, warnings))
}

/// Run the hooks of `stage` in `worktree`. Prints a line per hook and lets
/// the hook's own output through. Returns the number of hooks that failed.
pub fn run(cfg: &Config, stage: Stage, worktree: &Path, main_checkout: &Path) -> usize {
    let mut failed = 0;
    for hook in cfg.hooks(stage) {
        let extra_path = match &hook.requires {
            None => None,
            Some(tool) => match util::find_tool(tool) {
                None => continue,
                Some(found) => found.parent().map(Path::to_path_buf),
            },
        };
        println!("hook   {}: {}", stage.name(), hook.run);
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(&hook.run)
            .current_dir(worktree)
            .stdin(Stdio::null())
            .env("FAST_WORKTREE_PATH", worktree)
            .env("FAST_WORKTREE_MAIN", main_checkout);
        if let Some(p) = path_with(extra_path) {
            c.env("PATH", p);
        }
        let ok = match c.status() {
            Ok(s) if s.success() => true,
            Ok(s) => {
                eprintln!("FAILED hook {}: {} ({s})", stage.name(), hook.run);
                false
            }
            Err(e) => {
                eprintln!("FAILED hook {}: {} ({e})", stage.name(), hook.run);
                false
            }
        };
        if !ok {
            failed += 1;
        }
    }
    failed
}

/// `PATH` with `dir` appended when it is not already on it, so a tool that
/// `requires` found in `~/.cargo/bin` is also found by the hook's shell.
fn path_with(dir: Option<PathBuf>) -> Option<std::ffi::OsString> {
    let dir = dir?;
    let current = env::var_os("PATH").unwrap_or_default();
    if env::split_paths(&current).any(|p| p == dir) {
        return None;
    }
    let mut parts: Vec<PathBuf> = env::split_paths(&current).collect();
    parts.push(dir);
    env::join_paths(parts).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_stages_in_order() {
        let (c, w) = parse(
            r#"
[[post_add]]
run = "a"
[[post_add]]
run = "b"
requires = "tool"
[[pre_remove]]
run = "c"
"#,
        )
        .unwrap();
        assert!(w.is_empty());
        assert_eq!(
            c.post_add,
            vec![
                Hook {
                    run: "a".into(),
                    requires: None
                },
                Hook {
                    run: "b".into(),
                    requires: Some("tool".into())
                },
            ]
        );
        assert_eq!(c.pre_remove.len(), 1);
    }

    #[test]
    fn unknown_keys_warn_and_are_ignored() {
        let (c, w) = parse(
            r#"
future = 1
[[post_add]]
run = "a"
timeout = 5
"#,
        )
        .unwrap();
        assert_eq!(c.post_add.len(), 1);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w.iter().any(|x| x.contains("`future`")));
        assert!(w.iter().any(|x| x.contains("`timeout` in post_add[0]")));
    }

    #[test]
    fn empty_and_malformed() {
        assert_eq!(parse("").unwrap().0, Config::default());
        assert!(parse("post_add = 1").is_err());
        assert!(
            parse("[[post_add]]\nrequires = \"x\"").is_err(),
            "run is required"
        );
        assert!(parse("not toml =").is_err());
    }

    #[test]
    fn parses_skip_lifecycle() {
        let (c, w) = parse(
            r#"
[[skip_lifecycle]]
dir = "backend"
script = "postinstall"
if_equal = [["a", "node_modules/a"], ["b", "node_modules/b"]]
[[skip_lifecycle]]
script = "prepare"
if_equal = [["x", "y"]]
when = "never"
"#,
        )
        .unwrap();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("`when` in skip_lifecycle[1]"));
        assert_eq!(c.skip_lifecycle.len(), 2);
        assert_eq!(
            c.skip_lifecycle[0].if_equal[1],
            ["b".to_string(), "node_modules/b".to_string()]
        );
        assert_eq!(c.skip_lifecycle[1].dir, ".", "dir defaults to the root");
        assert!(c.skip_lifecycle[0].applies_to("./backend/", "postinstall"));
        assert!(!c.skip_lifecycle[0].applies_to("backend", "prepare"));
        assert!(!c.skip_lifecycle[0].applies_to("backend/sub", "postinstall"));
        assert!(c.skip_lifecycle[1].applies_to(".", "prepare"));
        assert!(c.skip_lifecycle[1].applies_to("", "prepare"));
    }

    #[test]
    fn skip_lifecycle_rejects_bad_pairs() {
        let bad = |s: &str| parse(&format!("[[skip_lifecycle]]\nscript = \"p\"\n{s}\n")).is_err();
        assert!(bad("if_equal = []"), "at least one pair");
        assert!(bad(r#"if_equal = [["a"]]"#), "a pair has two files");
        assert!(bad(r#"if_equal = [["a", "b", "c"]]"#));
        assert!(bad(""), "if_equal is required");
        assert!(!bad(r#"if_equal = [["a", "b"]]"#));
        assert!(
            parse("[[skip_lifecycle]]\nif_equal = [[\"a\", \"b\"]]").is_err(),
            "script is required"
        );
    }

    #[test]
    fn inputs_unchanged_needs_every_pair_present_and_equal() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        for (f, body) in [("a", "1"), ("a2", "1"), ("b", "2"), ("b2", "3")] {
            std::fs::write(p.join(f), body).unwrap();
        }
        let rule = |pairs: &[[&str; 2]]| SkipLifecycle {
            dir: ".".into(),
            script: "postinstall".into(),
            if_equal: pairs
                .iter()
                .map(|[x, y]| [x.to_string(), y.to_string()])
                .collect(),
        };
        assert!(rule(&[["a", "a2"]]).inputs_unchanged(p));
        assert!(!rule(&[["a", "a2"], ["b", "b2"]]).inputs_unchanged(p));
        assert!(!rule(&[["a", "missing"]]).inputs_unchanged(p));
        assert!(
            !rule(&[["missing", "also-missing"]]).inputs_unchanged(p),
            "absent files are not equal"
        );
        assert!(!rule(&[]).inputs_unchanged(p));
    }

    #[test]
    fn missing_file_is_empty() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(t.path()).unwrap(), Config::default());
    }
}
