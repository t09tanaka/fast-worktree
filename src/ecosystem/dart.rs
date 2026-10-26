//! Dart and Flutter: one project per tracked `pubspec.lock`.
//!
//! A package that the parent's pubspec pulls in as a path dependency (or a
//! pub workspace member) has no lockfile of its own, so it is not a
//! project: the parent's `pub get` resolves it.
//!
//! Dependencies are present when `.dart_tool/package_config.json` exists.
//! They are made present with `flutter pub get --offline` (then without
//! `--offline` if that fails) when `flutter` is installed, else with `dart
//! pub get`. With neither, the project is skipped with a note. Packages
//! live in pub's own shared cache, so there is nothing to clone.
//!
//! `flutter pub get` also writes absolute paths of the project into
//! ignored files (`ios/Flutter/Generated.xcconfig`,
//! `.flutter-plugins-dependencies`...), and does not write them again
//! while they are current. In a spare from the pool, moved to where it is
//! used, [`rewrite_moved_paths`] replaces the path where the spare was made
//! with the new one in those files.

use super::{Ctx, Mode, Plan, Preset, Report, Target};
use crate::paths::hex;
use crate::util;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const LOCKFILE: &str = "pubspec.lock";
const PACKAGE_CONFIG: &str = ".dart_tool/package_config.json";
const KEY_FILE: &str = ".dart_tool/fast-worktree-key";

pub struct Dart;

impl Preset for Dart {
    fn name(&self) -> &'static str {
        "dart"
    }

    fn label(&self) -> &'static str {
        "pub"
    }

    fn manifest(&self) -> &'static str {
        LOCKFILE
    }

    fn managed(&self) -> &'static str {
        ".dart_tool"
    }

    fn is_installed(&self, root: &Path, t: &Target) -> bool {
        super::project_path(root, &t.dir)
            .join(PACKAGE_CONFIG)
            .is_file()
    }

    fn install(&self, ctx: &Ctx, t: &Target, mode: Mode) -> Result<Report, String> {
        let project = super::project_path(&ctx.root, &t.dir);
        let key = target_key(ctx, t)?;
        let tools = Tools::find();
        let get = match &t.install {
            Some(cmd) => Get::Command {
                cmd,
                root: &ctx.root,
                main: &ctx.main,
            },
            None => Get::Pub(&tools),
        };
        let report = install(&project, &t.dir, mode, &get, &key)?;
        if report.kept || report.missing || !project.join(PACKAGE_CONFIG).is_file() {
            return Ok(report);
        }
        if let Some(v) = &t.verify {
            super::run_quietly(&ctx.root.join(&t.path), v, &ctx.root, &ctx.main)
                .map_err(|e| format!("verify failed after the install: {e}"))?;
        }
        for a in &t.after {
            super::run_quietly(&project, a, &ctx.root, &ctx.main)?;
        }
        Ok(report)
    }

    fn plan(&self, ctx: &Ctx, t: &Target, mode: Mode, fresh: bool) -> Plan {
        let project = super::project_path(&ctx.root, &t.dir);
        if !fresh && project.join(PACKAGE_CONFIG).is_file() {
            if !mode.reinstalls_changed() {
                return Plan::new("present (.dart_tool)", vec![]);
            }
            let recorded = fs::read_to_string(project.join(KEY_FILE)).unwrap_or_default();
            if target_key(ctx, t).is_ok_and(|k| recorded.trim() == k) {
                return Plan::new("present (up to date)", vec![]);
            }
        }
        let how = match &t.install {
            Some(c) => c.clone(),
            None => match Tools::find() {
                Tools {
                    flutter: Some(_), ..
                } => "flutter pub get".to_string(),
                Tools { dart: Some(_), .. } => "dart pub get".to_string(),
                _ => return Plan::new("nothing (neither flutter nor dart is installed)", vec![]),
            },
        };
        let why = vec!["clone: pub keeps packages in a cache of its own".to_string()];
        if mode.clone_only() {
            return Plan::new(format!("later, in the background ({how})"), why);
        }
        Plan::new(format!("install ({how})"), why)
    }

    fn rewrite_moved(&self, root: &Path, t: &Target, from: &[PathBuf]) -> usize {
        let project = super::project_path(root, &t.dir);
        let mut n = rewrite_moved_paths(&project, from, root);
        if t.rewrite_paths {
            n += super::rewrite_tree(&root.join(&t.path), from, root);
        }
        n
    }
}

/// What a target is installed from, recorded in `.dart_tool`: the
/// lockfile's hash, as before `[[clone]]`, unless its `key` or
/// `key_commands` say more.
fn target_key(ctx: &Ctx, t: &Target) -> Result<String, String> {
    let project = super::project_path(&ctx.root, &t.dir);
    if t.key_is_lockfile_only() && t.key_commands.is_empty() {
        let lockfile = fs::read(project.join(LOCKFILE)).map_err(|e| format!("{LOCKFILE}: {e}"))?;
        return Ok(key(&lockfile));
    }
    let files = super::key_digest(&ctx.root, &t.key)?;
    let outputs = super::command_outputs(&project, &t.key_commands, &ctx.root, &ctx.main)?;
    let mut h = Sha256::new();
    h.update(files);
    h.update(super::outputs_digest(&outputs));
    Ok(hex(&h.finalize())[..16].to_string())
}

/// Where in a project `pub get` and the Flutter tools write files that
/// hold the project's absolute path (pathspecs relative to the project).
const GENERATED: [&str; 6] = [
    ".dart_tool",
    "ios/Flutter",
    "macos/Flutter/ephemeral",
    "linux/flutter/ephemeral",
    "windows/flutter/ephemeral",
    ".flutter-plugins*",
];

/// The worktree of `project` moved from one of `from` (the paths where it
/// was made, as written into files) to `to`: in the ignored files under
/// [`GENERATED`], replace them with `to`. Files with a NUL byte (binary)
/// and symlinks are left alone. Returns how many files were rewritten.
pub fn rewrite_moved_paths(project: &Path, from: &[PathBuf], to: &Path) -> usize {
    let Ok(listed) = crate::git::output(
        project,
        &[
            &[
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--",
            ][..],
            &GENERATED[..],
        ]
        .concat(),
    ) else {
        return 0;
    };
    // Longest first: one may end with another (`/private/var/...` and
    // `/var/...`).
    let mut from: Vec<Vec<u8>> = from
        .iter()
        .map(|p| p.as_os_str().as_encoded_bytes().to_vec())
        .filter(|p| !p.is_empty())
        .collect();
    from.sort_by_key(|p| std::cmp::Reverse(p.len()));
    from.dedup();
    let to = to.as_os_str().as_encoded_bytes();
    let mut n = 0;
    for rel in listed.split('\0').filter(|r| !r.is_empty()) {
        let file = project.join(rel);
        if !fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let Ok(bytes) = fs::read(&file) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let mut text = bytes.clone();
        for f in &from {
            text = super::replace_all(&text, f, to);
        }
        if text != bytes && fs::write(&file, &text).is_ok() {
            n += 1;
        }
    }
    n
}

/// The SDK commands available, found once per install.
pub struct Tools {
    pub flutter: Option<PathBuf>,
    pub dart: Option<PathBuf>,
}

impl Tools {
    fn find() -> Tools {
        Tools {
            flutter: util::find_tool("flutter"),
            dart: util::find_tool("dart"),
        }
    }
}

fn key(lockfile: &[u8]) -> String {
    hex(&Sha256::digest(lockfile))[..16].to_string()
}

/// How `pub get` is done.
enum Get<'a> {
    /// `flutter pub get`, else `dart pub get`, with what is installed.
    Pub(&'a Tools),
    /// The `[[clone]]`'s own `install`, in the project directory.
    Command {
        cmd: &'a str,
        root: &'a Path,
        main: &'a Path,
    },
}

fn install(project: &Path, dir: &str, mode: Mode, get: &Get, key: &str) -> Result<Report, String> {
    let start = Instant::now();
    if project.join(PACKAGE_CONFIG).is_file() {
        if !mode.reinstalls_changed() {
            return Ok(Report::kept(format!(
                "skip   {dir} (.dart_tool already present)"
            )));
        }
        if fs::read_to_string(project.join(KEY_FILE)).is_ok_and(|k| k.trim() == key) {
            return Ok(Report::kept(format!("skip   {dir} (up to date)")));
        }
    }
    if mode.clone_only() {
        return Ok(Report::missing(""));
    }
    let how = match get {
        Get::Command { cmd, root, main } => {
            super::run_quietly(project, cmd, root, main)?;
            "install"
        }
        Get::Pub(Tools {
            flutter: Some(flutter),
            ..
        }) => match pub_get(flutter, project, true) {
            Ok(()) => "flutter",
            Err(_) => {
                pub_get(flutter, project, false)?;
                "flutter, online"
            }
        },
        Get::Pub(Tools {
            dart: Some(dart), ..
        }) => {
            pub_get(dart, project, false)?;
            "dart"
        }
        Get::Pub(_) => {
            return Ok(Report::done(format!(
                "note   {dir}: neither flutter nor dart is installed, skipped"
            )));
        }
    };
    let _ = fs::write(project.join(KEY_FILE), format!("{key}\n"));
    Ok(Report::done(format!(
        "pub get {dir} ({how}, {})",
        util::elapsed(start)
    )))
}

/// `<tool> pub get [--offline]`. Its output is shown only when it fails.
fn pub_get(tool: &Path, project: &Path, offline: bool) -> Result<(), String> {
    let mut c = Command::new(tool);
    c.args(["pub", "get"]);
    if offline {
        c.arg("--offline");
    }
    let out = c
        .current_dir(project)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", tool.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Err(util::tail(&log, 30))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sdk_is_a_note_not_a_failure() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join(LOCKFILE), "packages: {}\n").unwrap();
        let none = Tools {
            flutter: None,
            dart: None,
        };
        let k = key(b"packages: {}\n");
        let r = install(t.path(), "app", Mode::Missing, &Get::Pub(&none), &k).unwrap();
        assert_eq!(
            r.lines,
            "note   app: neither flutter nor dart is installed, skipped"
        );
        assert!(!r.missing);
        let r = install(t.path(), "app", Mode::CloneOnly, &Get::Pub(&none), &k).unwrap();
        assert!(r.missing, "pub get is never run from the hook");
    }

    #[test]
    fn present_is_skipped_unless_the_lockfile_changed() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        fs::write(p.join(LOCKFILE), "a").unwrap();
        fs::create_dir(p.join(".dart_tool")).unwrap();
        fs::write(p.join(PACKAGE_CONFIG), "{}").unwrap();
        let none = Tools {
            flutter: None,
            dart: None,
        };
        let get = Get::Pub(&none);
        let r = install(p, "app", Mode::Missing, &get, &key(b"a")).unwrap();
        assert_eq!(r.lines, "skip   app (.dart_tool already present)");
        fs::write(p.join(KEY_FILE), format!("{}\n", key(b"a"))).unwrap();
        let r = install(p, "app", Mode::Changed, &get, &key(b"a")).unwrap();
        assert_eq!(r.lines, "skip   app (up to date)");
        let r = install(p, "app", Mode::Changed, &get, &key(b"b")).unwrap();
        assert!(r.lines.contains("neither flutter nor dart"), "{}", r.lines);
    }
}
