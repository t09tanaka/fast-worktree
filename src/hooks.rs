//! Repository hooks from `.fast-worktree.toml` at the worktree root.
//!
//! ```toml
//! [[post_add]]          # after add / install, in the worktree root, in order
//! run = "make setup"
//! requires = "make"     # optional: skipped silently when not installed
//!
//! [[pre_remove]]        # after remove's safety checks, before the move
//! run = "docker compose -p \"$(basename \"$FAST_WORKTREE_PATH\")\" down -v"
//! requires = "docker"
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
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Hook {
    pub run: String,
    pub requires: Option<String>,
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

/// Parse the config, returning it with warnings about unknown keys.
pub fn parse(text: &str) -> std::result::Result<(Config, Vec<String>), String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let mut warnings = Vec::new();
    let mut known = toml::Table::new();
    for (k, v) in table {
        match k.as_str() {
            "post_add" | "pre_remove" => {
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
                        .filter(|x| !matches!(x.as_str(), "run" | "requires"))
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
            _ => warnings.push(format!("unknown key `{k}`, ignored")),
        }
    }
    let cfg = Config::deserialize(toml::Value::Table(known)).map_err(|e| e.to_string())?;
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
    fn missing_file_is_empty() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(t.path()).unwrap(), Config::default());
    }
}
