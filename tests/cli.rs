//! End-to-end tests for the `fast-worktree` binary, without the network.
//!
//! Each test builds its own world in a temp directory: an upstream
//! repository (default branch `trunk`, so nothing can assume `main`), a
//! clone of it as the main checkout, and fake `npm` / `node` at the front
//! of `PATH`. The fake `npm ci` writes `node_modules/.package-lock.json`
//! from the lockfile, as the real one does; every `npm` call is logged.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const FAKE_NPM: &str = r#"#!/bin/sh
echo "$1 $2 $(pwd -P)" >> "$FAKE_NPM_LOG"
case "$1" in
  ci)
    if [ -n "$FAKE_NPM_FAIL" ]; then echo "npm ERR! simulated failure" >&2; exit 1; fi
    mkdir -p node_modules/.cache node_modules/dep
    cp package-lock.json node_modules/.package-lock.json
    echo cached > node_modules/.cache/tool
    echo dep > node_modules/dep/index.js
    ;;
  run) ;;
  *) echo "fake npm: unexpected $*" >&2; exit 1 ;;
esac
"#;

const FAKE_NODE: &str = "#!/bin/sh\necho v20.0.0\n";

const HOOKS: &str = r#"
[[post_add]]
run = 'echo "post_add $FAST_WORKTREE_PATH|$FAST_WORKTREE_MAIN" >> "$HOOK_LOG"'

[[post_add]]
run = 'echo never >> "$HOOK_LOG"'
requires = "no-such-tool-anywhere"

[[pre_remove]]
run = 'echo "pre_remove $FAST_WORKTREE_PATH|$FAST_WORKTREE_MAIN" >> "$HOOK_LOG"; exit 3'
"#;

fn lockfile(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","lockfileVersion":3,"requires":true,"packages":{{"":{{"name":"{name}"}},"node_modules/dep":{{"version":"1.0.0"}}}}}}"#
    )
}

struct World {
    _tmp: TempDir,
    /// Real path of the temp dir (macOS puts it behind a symlink).
    base: PathBuf,
    cache: PathBuf,
}

fn write_exe(p: &Path, body: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

impl World {
    fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let w = World {
            cache: base.join("cache"),
            base,
            _tmp: tmp,
        };
        fs::create_dir_all(w.home()).unwrap();
        write_exe(&w.base.join("bin/npm"), FAKE_NPM);
        write_exe(&w.base.join("bin/node"), FAKE_NODE);

        let up = w.base.join("upstream");
        fs::create_dir_all(&up).unwrap();
        w.git(&up, &["init", "-q", "-b", "trunk"]);
        for (dir, scripts) in [
            ("app", r#","scripts":{"postinstall":"echo hi","test":"x"}"#),
            ("lib", ""),
        ] {
            let d = up.join(dir);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("package.json"),
                format!(r#"{{"name":"{dir}","version":"1.0.0"{scripts}}}"#),
            )
            .unwrap();
            fs::write(d.join("package-lock.json"), lockfile(dir)).unwrap();
        }
        fs::write(up.join(".gitignore"), "node_modules/\n").unwrap();
        fs::write(up.join(".fast-worktree.toml"), HOOKS).unwrap();
        w.git(&up, &["add", "-A"]);
        w.git(&up, &["commit", "-q", "-m", "init"]);
        w.git(&w.base, &["clone", "-q", "upstream", "main"]);
        w
    }

    fn home(&self) -> PathBuf {
        self.base.join("home")
    }
    fn main(&self) -> PathBuf {
        self.base.join("main")
    }
    fn npm_log(&self) -> String {
        fs::read_to_string(self.base.join("npm.log")).unwrap_or_default()
    }
    fn hook_log(&self) -> String {
        fs::read_to_string(self.base.join("hook.log")).unwrap_or_default()
    }

    fn env(&self, c: &mut Command) {
        c.env_clear()
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.base.join("bin").display()
                ),
            )
            .env("HOME", self.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("FAST_WORKTREE_CACHE", &self.cache)
            .env("FAKE_NPM_LOG", self.base.join("npm.log"))
            .env("HOOK_LOG", self.base.join("hook.log"));
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut c = Command::new("git");
        self.env(&mut c);
        let out = c.args(args).current_dir(dir).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn fw(&self, dir: &Path, args: &[&str]) -> Run {
        self.fw_with(dir, args, &[])
    }

    fn fw_with(&self, dir: &Path, args: &[&str], extra: &[(&str, &str)]) -> Run {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fast-worktree"));
        self.env(&mut c);
        for (k, v) in extra {
            c.env(k, v);
        }
        Run(c.args(args).current_dir(dir).output().unwrap())
    }

    fn count(&self, needle: &str) -> usize {
        self.npm_log()
            .lines()
            .filter(|l| l.starts_with(needle))
            .count()
    }
}

struct Run(Output);

impl Run {
    fn ok(&self) -> bool {
        self.0.status.success()
    }
    fn out(&self) -> String {
        String::from_utf8_lossy(&self.0.stdout).into_owned()
    }
    fn err(&self) -> String {
        String::from_utf8_lossy(&self.0.stderr).into_owned()
    }
    #[track_caller]
    fn assert_ok(&self) -> &Self {
        assert!(
            self.ok(),
            "stdout:\n{}\nstderr:\n{}",
            self.out(),
            self.err()
        );
        self
    }
    #[track_caller]
    fn assert_fails(&self) -> &Self {
        assert!(!self.ok(), "expected failure; stdout:\n{}", self.out());
        self
    }
}

#[test]
fn add_install_check_remove() {
    let w = World::new();
    let main = w.main();

    // First worktree: nothing to clone from, so npm ci for both projects.
    let r = w.fw(&main, &["add", "../wt1", "-b", "feat1"]);
    r.assert_ok();
    let wt1 = w.base.join("wt1");
    assert!(r.out().contains("worktree added in"), "{}", r.out());
    assert!(r.out().contains("npm ci app ("), "{}", r.out());
    assert!(r.out().contains("npm ci lib ("), "{}", r.out());
    assert!(r.out().contains("deps done in"));
    assert_eq!(w.count("ci "), 2);
    assert!(wt1.join("app/node_modules/.package-lock.json").is_file());
    // The new branch starts from origin's default branch (`trunk`).
    assert_eq!(
        w.git(&wt1, &["rev-parse", "HEAD"]),
        w.git(&main, &["rev-parse", "origin/trunk"])
    );
    // post_add ran with both paths; the one whose tool is missing did not.
    let hooks = w.hook_log();
    assert!(
        hooks.contains(&format!("post_add {}|{}", wt1.display(), main.display())),
        "{hooks}"
    );
    assert!(!hooks.contains("never"));

    // Second worktree: on macOS, cloned from the cache with no npm ci; the
    // project's own postinstall runs again in the clone.
    let r = w.fw(&main, &["add", "../wt2", "-b", "feat2", "origin/trunk"]);
    r.assert_ok();
    let wt2 = w.base.join("wt2");
    if cfg!(target_os = "macos") {
        assert!(r.out().contains("clone  app from "), "{}", r.out());
        assert!(r.out().contains(&w.cache.display().to_string()));
        assert_eq!(w.count("ci "), 2, "{}", w.npm_log());
        assert!(w
            .npm_log()
            .contains(&format!("run postinstall {}", wt2.join("app").display())));
        assert_eq!(w.count("run "), 1, "only the project that has one");
        assert!(!wt2.join("app/node_modules/.cache").exists());
        assert!(wt2.join("app/node_modules/dep/index.js").is_file());
    } else {
        assert!(r.out().contains("npm ci app ("), "{}", r.out());
        assert_eq!(w.count("ci "), 4);
    }

    // check: lists exactly what is missing and always exits 0.
    let r = w.fw(&wt2, &["check"]);
    r.assert_ok();
    assert_eq!(r.out(), "deps ok (2 projects)\n");
    assert_eq!(w.fw(&wt2, &["check", "--quiet"]).out(), "");
    fs::remove_dir_all(wt2.join("lib/node_modules")).unwrap();
    let r = w.fw(&wt2.join("app"), &["check", "--quiet"]);
    r.assert_ok();
    assert!(r.out().contains("missing deps: lib\n"), "{}", r.out());
    assert!(!r.out().contains("missing deps: app"));
    let r = w.fw(&w.base, &["check"]);
    r.assert_ok();
    assert_eq!(r.out(), "", "outside a repository: silent");

    // install: skips what is present, restores what is not.
    let r = w.fw(&wt2, &["install"]);
    r.assert_ok();
    assert!(r
        .out()
        .contains("skip   app (node_modules already present)"));
    assert!(
        r.out().contains("clone  lib from ") || r.out().contains("npm ci lib ("),
        "{}",
        r.out()
    );
    assert_eq!(w.fw(&wt2, &["check", "--quiet"]).out(), "");

    // remove: refuses untracked changes, then removes; pre_remove failing
    // does not stop it.
    fs::write(wt2.join("notes.txt"), "wip").unwrap();
    let r = w.fw(&main, &["remove", "../wt2"]);
    r.assert_fails();
    assert!(
        r.err().contains("uncommitted or untracked changes"),
        "{}",
        r.err()
    );
    assert!(wt2.exists());
    fs::remove_file(wt2.join("notes.txt")).unwrap();
    let r = w.fw(&main, &["remove", "../wt2"]);
    r.assert_ok();
    assert!(r.out().contains("deleting in background"));
    assert!(r.err().contains("pre_remove hook(s) failed"), "{}", r.err());
    assert!(!wt2.exists());
    assert!(!w.git(&main, &["worktree", "list"]).contains("wt2"));
    assert!(w
        .hook_log()
        .contains(&format!("pre_remove {}|{}", wt2.display(), main.display())));
    // The branch stays without --delete-branch.
    assert!(w
        .git(&main, &["branch", "--list", "feat2"])
        .contains("feat2"));

    // Refusals: the main checkout, a subdirectory, a locked worktree.
    let r = w.fw(&main, &["remove", "."]);
    r.assert_fails();
    assert!(r.err().contains("main checkout"), "{}", r.err());
    let r = w.fw(&main, &["remove", "../wt1/app"]);
    r.assert_fails();
    assert!(r.err().contains("not a worktree root"), "{}", r.err());
    w.git(&main, &["worktree", "lock", "../wt1"]);
    let r = w.fw(&main, &["remove", "../wt1"]);
    r.assert_fails();
    assert!(r.err().contains("locked"), "{}", r.err());
    w.git(&main, &["worktree", "unlock", "../wt1"]);

    // --delete-branch, run from inside the worktree being removed.
    let r = w.fw(&wt1, &["remove", ".", "--delete-branch"]);
    r.assert_ok();
    assert!(r.out().contains("deleted branch feat1"), "{}", r.out());
    assert!(!wt1.exists());
    assert!(!w
        .git(&main, &["branch", "--list", "feat1"])
        .contains("feat1"));
}

#[test]
fn add_resolves_the_default_branch_when_origin_head_is_unset() {
    let w = World::new();
    let main = w.main();
    w.git(&main, &["remote", "set-head", "origin", "-d"]);
    let r = w.fw(&main, &["add", "../wt", "-b", "x"]);
    r.assert_ok();
    assert_eq!(
        w.git(
            &main,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]
        )
        .trim(),
        "origin/trunk"
    );
}

#[test]
fn npm_ci_failure_is_reported_per_project() {
    let w = World::new();
    let r = w.fw_with(
        &w.main(),
        &["add", "../wt", "-b", "x"],
        &[("FAKE_NPM_FAIL", "1")],
    );
    r.assert_fails();
    assert!(r.out().contains("FAILED app:"), "{}", r.out());
    assert!(
        r.out().contains("  npm ERR! simulated failure"),
        "{}",
        r.out()
    );
    // The worktree itself was created and can be fixed with `install`.
    let wt = w.base.join("wt");
    assert!(wt.join("app/package.json").is_file());
    w.fw(&wt, &["install", "app"]).assert_ok();
    assert!(wt.join("app/node_modules/.package-lock.json").is_file());
}

#[test]
fn install_only_named_projects() {
    let w = World::new();
    let main = w.main();
    let r = w.fw(&main, &["install", "--root", ".", "lib"]);
    r.assert_ok();
    assert!(r.out().contains("lib"), "{}", r.out());
    assert!(!r.out().contains(" app"), "{}", r.out());
    assert!(main.join("lib/node_modules").is_dir());
    assert!(!main.join("app/node_modules").exists());
    let r = w.fw(&main, &["install", "nope"]);
    r.assert_fails();
    assert!(r.err().contains("no tracked project manifest in nope"));
}

#[test]
fn post_add_failure_fails_the_command() {
    let w = World::new();
    let main = w.main();
    fs::write(
        main.join(".fast-worktree.toml"),
        "[[post_add]]\nrun = \"exit 7\"\nfuture = true\n",
    )
    .unwrap();
    let r = w.fw(&main, &["install"]);
    r.assert_fails();
    assert!(
        r.err().contains("FAILED hook post_add: exit 7"),
        "{}",
        r.err()
    );
    assert!(r.err().contains("unknown key `future`"), "{}", r.err());
}

#[test]
fn requires_is_found_in_cargo_bin_and_put_on_path() {
    let w = World::new();
    let main = w.main();
    write_exe(
        &w.home().join(".cargo/bin/mytool"),
        "#!/bin/sh\necho mytool ran >> \"$HOOK_LOG\"\n",
    );
    fs::write(
        main.join(".fast-worktree.toml"),
        "[[post_add]]\nrun = \"mytool\"\nrequires = \"mytool\"\n",
    )
    .unwrap();
    w.fw(&main, &["install"]).assert_ok();
    assert!(w.hook_log().contains("mytool ran"));
}

#[test]
fn no_config_and_no_lockfiles() {
    let w = World::new();
    let main = w.main();
    fs::remove_file(main.join(".fast-worktree.toml")).unwrap();
    for d in ["app", "lib"] {
        fs::remove_file(main.join(d).join("package-lock.json")).unwrap();
    }
    w.git(&main, &["commit", "-qam", "drop lockfiles"]);
    let r = w.fw(&main, &["install"]);
    r.assert_ok();
    assert!(r.out().contains("no package-lock.json tracked"));
    assert_eq!(w.hook_log(), "");
    assert_eq!(w.fw(&main, &["check"]).out(), "");
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    #[test]
    fn clones_from_a_matching_worktree_but_not_a_stale_one() {
        let w = World::new();
        let main = w.main();
        w.fw(&main, &["add", "../wt1", "-b", "a"]).assert_ok();
        let wt1 = w.base.join("wt1");
        // Make wt1's lib tree stale: its record no longer matches the lockfile.
        fs::write(
            wt1.join("lib/node_modules/.package-lock.json"),
            r#"{"packages":{"node_modules/dep":{"version":"0.9.0"}}}"#,
        )
        .unwrap();
        let r = w.fw_with(
            &main,
            &["add", "../wt2", "-b", "b"],
            &[(
                "FAST_WORKTREE_CACHE",
                w.base.join("cache2").to_str().unwrap(),
            )],
        );
        r.assert_ok();
        assert!(
            r.out().contains(&format!(
                "clone  app from {}",
                wt1.join("app/node_modules").display()
            )),
            "{}",
            r.out()
        );
        assert!(r.out().contains("npm ci lib ("), "{}", r.out());
        // The clone was saved to the cache, without .cache.
        let entries: Vec<_> = walk_dirs(&w.base.join("cache2"))
            .into_iter()
            .filter(|p| p.join(".package-lock.json").is_file())
            .collect();
        assert_eq!(entries.len(), 2, "{entries:?}");
        for e in entries {
            assert!(!e.join(".cache").exists());
            let name = e.file_name().unwrap().to_string_lossy().into_owned();
            assert!(name.ends_with("-v20.0.0"), "{name}");
            assert_eq!(name.len(), 16 + "-v20.0.0".len());
        }
    }

    #[test]
    fn an_absolute_symlink_falls_back_to_npm_ci() {
        let w = World::new();
        let main = w.main();
        w.fw(&main, &["add", "../wt1", "-b", "a"]).assert_ok();
        let wt1 = w.base.join("wt1");
        std::os::unix::fs::symlink(&wt1, wt1.join("app/node_modules/dep/abs")).unwrap();
        let r = w.fw_with(
            &main,
            &["add", "../wt2", "-b", "b"],
            &[(
                "FAST_WORKTREE_CACHE",
                w.base.join("cache2").to_str().unwrap(),
            )],
        );
        r.assert_ok();
        assert!(
            r.out()
                .contains("note   app: absolute symlink in node_modules, falling back to npm ci"),
            "{}",
            r.out()
        );
        assert!(r.out().contains("npm ci app ("), "{}", r.out());
        assert!(!w.base.join("wt2/app/node_modules/dep/abs").exists());
    }

    fn walk_dirs(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                    out.push(e.path());
                }
            }
        }
        out
    }
}
