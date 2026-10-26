//! `.worktreeinclude`: gitignored files to copy into a new worktree.
//!
//! The file sits in the main checkout's root and uses `.gitignore` syntax,
//! the format Claude Code reads for its own worktrees. A file is copied
//! when it matches a pattern **and** git ignores it in the main checkout;
//! tracked files and untracked files that git does not ignore never are.
//! A file that already exists in the new worktree is left alone.
//!
//! Wholly ignored directories (such as `node_modules/`) are not walked
//! unless a pattern can reach into them: the directory itself matches, an
//! anchored pattern's leading components match the directory's path, or,
//! for a pattern that starts with `**/` (or has no slash, which means the
//! same), the first name after `**/` is one of the names in the
//! directory's path.

use crate::{clone, git};
use globset::GlobBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::fs;
use std::path::{Path, PathBuf};

pub const FILE: &str = ".worktreeinclude";

/// What [`copy`] did, as paths relative to the worktree root.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Copied {
    pub copied: Vec<String>,
    /// Matched, but the new worktree already had something there.
    pub kept: Vec<String>,
}

/// Copy the files `<main>/.worktreeinclude` selects from `main` to `dst`.
pub fn copy(main: &Path, dst: &Path) -> Result<Copied, String> {
    let text = match fs::read_to_string(main.join(FILE)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Copied::default()),
        Err(e) => return Err(format!("{FILE}: {e}")),
    };
    let rules = Rules::parse(main, &text)?;
    let listing = git::output(
        main,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    )
    .map_err(|e| e.to_string())?;
    let mut out = Copied::default();
    for c in candidates(&listing, &rules, main) {
        put(&main.join(&c), &dst.join(&c), &c, &mut out)?;
    }
    Ok(out)
}

/// Copy `src` to `to` unless something is there. A directory that exists
/// on both sides is merged entry by entry, so its other files still come.
fn put(src: &Path, to: &Path, rel: &Path, out: &mut Copied) -> Result<(), String> {
    let name = rel.to_string_lossy().into_owned();
    match to.symlink_metadata() {
        Ok(m) if m.is_dir() && src.symlink_metadata().is_ok_and(|s| s.is_dir()) => {
            let entries = fs::read_dir(src).map_err(|e| format!("{name}: {e}"))?;
            for e in entries.flatten() {
                let n = e.file_name();
                put(&e.path(), &to.join(&n), &rel.join(&n), out)?;
            }
        }
        Ok(_) => out.kept.push(name),
        Err(_) => {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("{name}: {e}"))?;
            }
            clone::copy_tree(src, to).map_err(|e| format!("{name}: {e}"))?;
            out.copied.push(name);
        }
    }
    Ok(())
}

/// The paths (files, or directories copied whole) to copy, relative to
/// `main`, from `git ls-files -z --others --ignored --exclude-standard
/// --directory`. In that listing a directory ends with `/`; one that git
/// did not descend into (no entry below it) is ignored as a whole.
fn candidates(listing: &str, rules: &Rules, main: &Path) -> Vec<PathBuf> {
    let entries: Vec<&str> = listing.split('\0').filter(|e| !e.is_empty()).collect();
    // Every directory that has an entry below it, as `a/b/`.
    let mut walked = std::collections::HashSet::new();
    for e in &entries {
        let body = e.strip_suffix('/').unwrap_or(e);
        for (i, _) in body.match_indices('/') {
            walked.insert(&e[..=i]);
        }
    }
    let mut out = Vec::new();
    for e in &entries {
        if let Some(d) = e.strip_suffix('/') {
            if !walked.contains(e) {
                collect_dir(main, Path::new(d), rules, &mut out);
            }
        } else if rules.matches(Path::new(e), false) {
            out.push(PathBuf::from(e));
        }
    }
    out.sort();
    out
}

/// Inside a wholly ignored directory: take it whole if it matches, else
/// look inside only where a pattern can reach.
fn collect_dir(main: &Path, rel: &Path, rules: &Rules, out: &mut Vec<PathBuf>) {
    if rules.matches(rel, true) {
        out.push(rel.to_path_buf());
        return;
    }
    if !rules.may_reach_into(rel) {
        return;
    }
    let Ok(entries) = fs::read_dir(main.join(rel)) else {
        return;
    };
    for e in entries.flatten() {
        let child = rel.join(e.file_name());
        match e.file_type() {
            Ok(t) if t.is_dir() => collect_dir(main, &child, rules, out),
            Ok(_) if rules.matches(&child, false) => out.push(child),
            _ => {}
        }
    }
}

struct Rules {
    matcher: Gitignore,
    /// The positive patterns, split into path components, for
    /// [`Rules::may_reach_into`]. A pattern without a slash is stored as
    /// `**/<pattern>`.
    patterns: Vec<Vec<String>>,
}

impl Rules {
    fn parse(root: &Path, text: &str) -> Result<Rules, String> {
        let mut b = GitignoreBuilder::new(root);
        let mut patterns = Vec::new();
        for line in text.lines() {
            b.add_line(None, line).map_err(|e| format!("{FILE}: {e}"))?;
            let l = line.trim_end();
            if l.is_empty() || l.starts_with('#') || l.starts_with('!') {
                continue;
            }
            let l = l.strip_prefix('\\').unwrap_or(l);
            let body = l.trim_end_matches('/');
            let anchored = body.contains('/');
            let body = body.trim_start_matches('/');
            let mut comps: Vec<String> = body.split('/').map(str::to_string).collect();
            if !anchored {
                comps.insert(0, "**".into());
            }
            patterns.push(comps);
        }
        let matcher = b.build().map_err(|e| format!("{FILE}: {e}"))?;
        Ok(Rules { matcher, patterns })
    }

    fn matches(&self, rel: &Path, is_dir: bool) -> bool {
        self.matcher
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
    }

    /// Whether some pattern could match a path below the directory `rel`.
    fn may_reach_into(&self, rel: &Path) -> bool {
        let names: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        self.patterns.iter().any(|p| reaches(p, &names))
    }
}

fn reaches(pattern: &[String], dir: &[String]) -> bool {
    if pattern.first().is_some_and(|c| c == "**") {
        // `**/name/...`: only through a directory called `name`.
        return match pattern.get(1) {
            None => true,
            Some(first) => dir.iter().any(|d| glob_eq(first, d)),
        };
    }
    for (i, d) in dir.iter().enumerate() {
        match pattern.get(i) {
            // The pattern is no deeper than the directory: had it matched,
            // the directory would have matched.
            None => return false,
            Some(c) if c == "**" => return true,
            Some(c) if !glob_eq(c, d) => return false,
            Some(_) => {}
        }
    }
    // The directory is a prefix of the pattern.
    pattern.len() > dir.len()
}

/// Whether one path component matches one glob component.
fn glob_eq(glob: &str, name: &str) -> bool {
    GlobBuilder::new(glob)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map(|g| g.compile_matcher().is_match(name))
        .unwrap_or(glob == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comps(p: &str) -> Vec<String> {
        p.split('/').map(str::to_string).collect()
    }

    #[test]
    fn reach_follows_the_leading_components() {
        let r = |p: &str, d: &str| reaches(&comps(p), &comps(d));
        assert!(r("vendor/**/config.json", "vendor"));
        assert!(r("vendor/**/config.json", "vendor/a/b"));
        assert!(!r("vendor/**/config.json", "node_modules"));
        assert!(r("config/secrets.json", "config"));
        assert!(!r("config/secrets.json", "config/deeper"));
        assert!(r("pack*/x", "packages"));
        assert!(r("**/.claude/skills/*.md", ".claude"));
        assert!(r("**/.claude/skills/*.md", "a/.claude/skills"));
        assert!(!r("**/.claude/skills/*.md", "node_modules"));
        assert!(!r("**/.env", "node_modules"), "a slashless .env");
        assert!(r("**/.env", "x/.env"), "the name is in the path");
    }

    fn rules(text: &str) -> Rules {
        Rules::parse(Path::new("/r"), text).unwrap()
    }

    #[test]
    fn parsed_patterns() {
        let r = rules("# c\n\n.env\n/top.txt\nconfig/secrets.json\nbuild/\n!keep\n");
        assert_eq!(
            r.patterns,
            vec![
                comps("**/.env"),
                comps("top.txt"),
                comps("config/secrets.json"),
                comps("**/build"),
            ]
        );
        assert!(r.matches(Path::new(".env"), false));
        assert!(r.matches(Path::new("api/.env"), false));
        assert!(r.matches(Path::new("top.txt"), false));
        assert!(!r.matches(Path::new("a/top.txt"), false), "anchored");
        assert!(
            r.matches(Path::new("x/build/out.js"), false),
            "inside a dir"
        );
        assert!(!r.matches(Path::new("keep"), false));
    }

    #[test]
    fn candidates_skip_unreachable_ignored_dirs() {
        let t = tempfile::tempdir().unwrap();
        let main = t.path();
        for d in ["node_modules/pkg", "vendor/a", "cfg"] {
            fs::create_dir_all(main.join(d)).unwrap();
        }
        for f in [
            "node_modules/pkg/.env",
            "vendor/a/config.json",
            "vendor/a/other.json",
            "cfg/x",
        ] {
            fs::write(main.join(f), "").unwrap();
        }
        let r = rules(".env\nvendor/**/config.json\ncfg/\n");
        // As git lists them: `.env` and `api/.env` are files; the three
        // directories are ignored whole.
        let listing = ".env\0api/.env\0api/x.log\0node_modules/\0vendor/\0cfg/\0";
        let got = candidates(listing, &r, main);
        assert_eq!(
            got,
            vec![
                PathBuf::from(".env"),
                PathBuf::from("api/.env"),
                PathBuf::from("cfg"),
                PathBuf::from("vendor/a/config.json"),
            ]
        );
        // A directory git descended into is not taken whole.
        let listing = "deep/\0deep/x/\0deep/x/.env\0";
        assert_eq!(
            candidates(listing, &r, main),
            vec![PathBuf::from("deep/x/.env")]
        );
    }
}
