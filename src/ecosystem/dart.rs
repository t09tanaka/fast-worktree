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

use super::{Ctx, Ecosystem, Mode, Report};
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

impl Ecosystem for Dart {
    fn name(&self) -> &'static str {
        "pub"
    }

    fn manifest(&self) -> &'static str {
        LOCKFILE
    }

    fn is_installed(&self, project: &Path) -> bool {
        project.join(PACKAGE_CONFIG).is_file()
    }

    fn install(&self, ctx: &Ctx, dir: &str, mode: Mode) -> Result<Report, String> {
        install(
            &super::project_path(&ctx.root, dir),
            dir,
            mode,
            &Tools::find(),
        )
    }
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

fn install(project: &Path, dir: &str, mode: Mode, tools: &Tools) -> Result<Report, String> {
    let start = Instant::now();
    let lockfile = fs::read(project.join(LOCKFILE)).map_err(|e| format!("{LOCKFILE}: {e}"))?;
    let key = key(&lockfile);
    if project.join(PACKAGE_CONFIG).is_file() {
        if mode != Mode::Changed {
            return Ok(Report::done(format!(
                "skip   {dir} (.dart_tool already present)"
            )));
        }
        if fs::read_to_string(project.join(KEY_FILE)).is_ok_and(|k| k.trim() == key) {
            return Ok(Report::done(format!("skip   {dir} (up to date)")));
        }
    }
    if mode == Mode::CloneOnly {
        return Ok(Report {
            lines: String::new(),
            missing: true,
        });
    }
    let how = if let Some(flutter) = &tools.flutter {
        match pub_get(flutter, project, true) {
            Ok(()) => "flutter",
            Err(_) => {
                pub_get(flutter, project, false)?;
                "flutter, online"
            }
        }
    } else if let Some(dart) = &tools.dart {
        pub_get(dart, project, false)?;
        "dart"
    } else {
        return Ok(Report::done(format!(
            "note   {dir}: neither flutter nor dart is installed, skipped"
        )));
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
        let r = install(t.path(), "app", Mode::Missing, &none).unwrap();
        assert_eq!(
            r.lines,
            "note   app: neither flutter nor dart is installed, skipped"
        );
        assert!(!r.missing);
        let r = install(t.path(), "app", Mode::CloneOnly, &none).unwrap();
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
        let r = install(p, "app", Mode::Missing, &none).unwrap();
        assert_eq!(r.lines, "skip   app (.dart_tool already present)");
        fs::write(p.join(KEY_FILE), format!("{}\n", key(b"a"))).unwrap();
        let r = install(p, "app", Mode::Changed, &none).unwrap();
        assert_eq!(r.lines, "skip   app (up to date)");
        fs::write(p.join(LOCKFILE), "b").unwrap();
        let r = install(p, "app", Mode::Changed, &none).unwrap();
        assert!(r.lines.contains("neither flutter nor dart"), "{}", r.lines);
    }
}
