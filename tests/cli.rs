//! End-to-end tests for the `fast-worktree` binary, without the network.
//!
//! Each test builds its own world in a temp directory: an upstream
//! repository (default branch `trunk`, so nothing can assume `main`), a
//! clone of it as the main checkout, and fake `npm` / `node` at the front
//! of `PATH` (`flutter` / `dart` too, for the tests that want them). The
//! fake `npm ci` and `npm install` write `node_modules/.package-lock.json`
//! from the lockfile, as the real ones do; every call is logged.

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
    # An install that exits 0 but leaves no record of the tree.
    if [ -n "$FAKE_NPM_PARTIAL" ]; then mkdir -p node_modules/dep; echo dep > node_modules/dep/index.js; exit 0; fi
    mkdir -p node_modules/.cache node_modules/dep
    cp package-lock.json node_modules/.package-lock.json
    echo cached > node_modules/.cache/tool
    echo dep > node_modules/dep/index.js
    if [ -n "$FAKE_NPM_ABS_LINK" ]; then ln -s "$(pwd -P)" node_modules/dep/abs; fi
    ;;
  install)
    [ -d node_modules ] || { echo "fake npm: install without a tree" >&2; exit 1; }
    if [ -n "$FAKE_NPM_INSTALL_REWRITES" ]; then echo '{"rewritten":1}' > package-lock.json; fi
    if [ -z "$FAKE_NPM_INSTALL_STALE" ]; then cp package-lock.json node_modules/.package-lock.json; fi
    echo updated > node_modules/dep/index.js
    ;;
  run)
    if [ -n "$FAKE_NPM_RUN_FAIL" ]; then echo "script failed" >&2; exit 1; fi
    ;;
  *) echo "fake npm: unexpected $*" >&2; exit 1 ;;
esac
"#;

/// `flutter` or `dart`: `pub get` writes `.dart_tool/package_config.json`;
/// `--offline` fails when `FAKE_PUB_OFFLINE_FAIL` is set.
const FAKE_PUB: &str = r#"#!/bin/sh
echo "$(basename "$0") $* $(pwd -P)" >> "$FAKE_PUB_LOG"
case "$*" in
  *--offline*) if [ -n "$FAKE_PUB_OFFLINE_FAIL" ]; then echo "offline: no cache" >&2; exit 1; fi ;;
esac
mkdir -p .dart_tool && echo '{}' > .dart_tool/package_config.json
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
    lockfile_v(name, "1.0.0")
}

/// A lockfile whose one dependency is at `version`.
fn lockfile_v(name: &str, version: &str) -> String {
    format!(
        r#"{{"name":"{name}","lockfileVersion":3,"requires":true,"packages":{{"":{{"name":"{name}"}},"node_modules/dep":{{"version":"{version}"}}}}}}"#
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
            .env("FAKE_PUB_LOG", self.base.join("pub.log"))
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
    assert!(r.out().contains("missing deps: lib (npm)\n"), "{}", r.out());
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
    assert!(r.err().contains("no tracked lockfile in nope"));
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
    assert!(r.out().contains("no lockfile tracked"));
    assert_eq!(w.hook_log(), "");
    assert_eq!(w.fw(&main, &["check"]).out(), "");
}

#[test]
fn a_lockfile_at_the_repository_root_is_a_project() {
    let w = World::new();
    let main = w.main();
    fs::write(main.join("package.json"), r#"{"name":"root"}"#).unwrap();
    fs::write(main.join("package-lock.json"), lockfile("root")).unwrap();
    w.git(&main, &["add", "-A"]);
    w.git(&main, &["commit", "-qm", "root project"]);

    let r = w.fw(&main, &["check", "--quiet"]);
    assert!(r.out().contains("missing deps: . (npm)\n"), "{}", r.out());
    let r = w.fw(&main, &["install", "."]);
    r.assert_ok();
    assert!(r.out().contains("npm ci . ("), "{}", r.out());
    assert!(main.join("node_modules/.package-lock.json").is_file());
    assert!(!r.out().contains(" app"), "only the named project");

    // A worktree of the same commit gets the root project from the cache
    // (macOS) or from npm ci.
    let r = w.fw(&main, &["add", "../wt", "-b", "x", "HEAD"]);
    r.assert_ok();
    if cfg!(target_os = "macos") {
        // The repository's cache directory (`<name>-<hash>`).
        let from_cache = fs::read_dir(&w.cache)
            .unwrap()
            .flatten()
            .find(|e| e.file_name() != "trash")
            .unwrap()
            .path();
        assert!(
            r.out()
                .contains(&format!("clone  . from {}/_root/", from_cache.display())),
            "{}",
            r.out()
        );
    } else {
        assert!(r.out().contains("npm ci . ("), "{}", r.out());
    }
    assert!(w.base.join("wt/node_modules/.package-lock.json").is_file());
}

/// The main checkout plus `wt`, a plain `git worktree add` (no installs).
fn with_worktree(w: &World, rel: &str) -> PathBuf {
    let main = w.main();
    let name = rel.rsplit('/').next().unwrap().to_string();
    w.git(&main, &["worktree", "add", "-q", rel, "-b", &name]);
    main.join(rel).canonicalize().unwrap()
}

fn registered(w: &World) -> String {
    w.git(&w.main(), &["worktree", "list", "--porcelain"])
}

#[test]
fn remove_sees_untracked_files_whatever_the_status_config_says() {
    let w = World::new();
    let wt = with_worktree(&w, "../wt");
    w.git(&wt, &["config", "status.showUntrackedFiles", "no"]);
    fs::write(wt.join("notes.txt"), "wip").unwrap();
    assert_eq!(
        w.git(&wt, &["status", "--porcelain"]),
        "",
        "hidden by the config"
    );
    let r = w.fw(&w.main(), &["remove", "../wt"]);
    r.assert_fails();
    assert!(
        r.err().contains("uncommitted or untracked changes"),
        "{}",
        r.err()
    );
    assert!(wt.join("notes.txt").is_file());
}

#[test]
fn remove_unregisters_only_its_own_worktree() {
    let w = World::new();
    let main = w.main();
    // A worktree on a volume that is not mounted right now: its directory
    // is out of sight, so `git worktree prune` would drop it.
    fs::create_dir_all(w.base.join("vol")).unwrap();
    let ext = with_worktree(&w, "../vol/ext");
    let wt = with_worktree(&w, "../wt");
    fs::rename(w.base.join("vol"), w.base.join("vol-unmounted")).unwrap();
    assert!(registered(&w).contains("prunable"), "{}", registered(&w));

    let r = w.fw(&main, &["remove", "../wt"]);
    r.assert_ok();
    assert!(!wt.exists());
    let list = registered(&w);
    assert!(
        !list.contains(&format!("worktree {}\n", wt.display())),
        "{list}"
    );
    assert!(
        list.contains(&format!("worktree {}\n", ext.display())),
        "{list}"
    );

    // Mounted again, it is still a working worktree.
    fs::rename(w.base.join("vol-unmounted"), w.base.join("vol")).unwrap();
    assert_eq!(w.git(&ext, &["status", "--porcelain"]), "");
    assert!(!registered(&w).contains("prunable"), "{}", registered(&w));
}

#[test]
fn remove_refuses_a_worktree_with_another_one_inside() {
    let w = World::new();
    let main = w.main();
    // Ignored for every worktree, so `status` does not show what is inside.
    fs::write(main.join(".git/info/exclude"), ".claude/\n").unwrap();
    let wt = with_worktree(&w, "../wt");
    fs::create_dir_all(wt.join(".claude/worktrees")).unwrap();
    w.git(
        &wt,
        &[
            "worktree",
            "add",
            "-q",
            ".claude/worktrees/inner",
            "-b",
            "inner",
        ],
    );
    let inner = wt.join(".claude/worktrees/inner");
    assert_eq!(w.git(&wt, &["status", "--porcelain"]), "");

    let r = w.fw(&main, &["remove", "../wt"]);
    r.assert_fails();
    assert!(
        r.err().contains("another worktree is inside it"),
        "{}",
        r.err()
    );
    assert!(
        r.err().contains(&inner.display().to_string()),
        "{}",
        r.err()
    );
    assert!(inner.join(".git").exists());
    assert!(
        !w.hook_log().contains("pre_remove"),
        "refused before the hooks"
    );

    // Inner first, then the outer one.
    w.fw(&main, &["remove", &inner.to_string_lossy()])
        .assert_ok();
    w.fw(&main, &["remove", "../wt"]).assert_ok();
    assert!(!wt.exists());
}

#[test]
fn remove_checks_that_the_worktree_can_be_moved_before_the_hooks() {
    // SAFETY: no arguments.
    if unsafe { libc::geteuid() } == 0 {
        return; // root can rename anywhere
    }
    let w = World::new();
    fs::create_dir_all(w.base.join("ro")).unwrap();
    let wt = with_worktree(&w, "../ro/wt");
    let ro = w.base.join("ro");
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    let r = w.fw(&w.main(), &["remove", "../ro/wt"]);
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    r.assert_fails();
    assert!(r.err().contains("nothing was removed"), "{}", r.err());
    assert!(r.err().contains("not writable"), "{}", r.err());
    assert!(!w.hook_log().contains("pre_remove"), "{}", w.hook_log());
    assert!(wt.join(".git").is_file());
    assert!(registered(&w).contains(&wt.display().to_string()));
}

#[test]
fn install_redoes_a_node_modules_without_its_record() {
    let w = World::new();
    let main = w.main();
    w.fw(&main, &["add", "../wt", "-b", "x"]).assert_ok();
    let wt = w.base.join("wt");
    // An install interrupted before npm wrote `.package-lock.json`.
    fs::remove_file(wt.join("lib/node_modules/.package-lock.json")).unwrap();
    let r = w.fw(&wt, &["install"]);
    r.assert_ok();
    assert!(
        r.out()
            .contains("note   lib: node_modules has no .package-lock.json, installing again"),
        "{}",
        r.out()
    );
    assert!(r
        .out()
        .contains("skip   app (node_modules already present)"));
    assert!(wt.join("lib/node_modules/.package-lock.json").is_file());
    assert_eq!(w.fw(&wt, &["check", "--quiet"]).out(), "");
}

#[test]
fn worktreeinclude_never_copies_another_repository() {
    let w = World::new();
    let main = w.main();
    fs::write(main.join(".git/info/exclude"), ".claude/\n").unwrap();
    fs::write(main.join(".worktreeinclude"), ".claude/\n").unwrap();
    fs::create_dir_all(main.join(".claude/worktrees")).unwrap();
    fs::write(main.join(".claude/settings.local.json"), "{}\n").unwrap();
    w.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            ".claude/worktrees/other",
            "-b",
            "other",
        ],
    );
    w.git(&main, &["init", "-q", ".claude/clone"]);
    fs::write(main.join(".claude/clone/x.txt"), "x\n").unwrap();

    // The new worktree is inside the directory it copies from, too.
    let r = w.fw(
        &main,
        &["add", ".claude/worktrees/new", "-b", "new", "HEAD"],
    );
    r.assert_ok();
    let new = main.join(".claude/worktrees/new");
    assert_eq!(
        fs::read_to_string(new.join(".claude/settings.local.json")).unwrap(),
        "{}\n"
    );
    assert!(
        r.out().contains("copy   .claude/settings.local.json\n"),
        "{}",
        r.out()
    );
    assert!(!new.join(".claude/worktrees/other").exists(), "{}", r.out());
    assert!(!new.join(".claude/worktrees/new").exists(), "{}", r.out());
    assert!(!new.join(".claude/clone").exists(), "{}", r.out());
}

#[test]
fn install_changed_reinstalls_when_a_git_dependency_moves() {
    let w = World::new();
    let main = w.main();
    let lock = |sha: &str| {
        format!(
            r#"{{"name":"lib","lockfileVersion":3,"requires":true,"packages":{{"":{{"name":"lib"}},"node_modules/dep":{{"version":"1.0.0","resolved":"git+ssh://git@example.com/dep.git#{sha}"}}}}}}"#
        )
    };
    w.commit(
        &main,
        &[("lib/package-lock.json", &lock("aaa"))],
        "a git dependency",
    );
    w.fw(&main, &["add", "../wt", "-b", "x", "HEAD"])
        .assert_ok();
    let wt = w.base.join("wt");
    // Another commit of the dependency: same version, no integrity.
    fs::write(wt.join("lib/package-lock.json"), lock("bbb")).unwrap();
    let r = w.fw(&wt, &["install", "--changed", "lib"]);
    r.assert_ok();
    assert!(!r.out().contains("up to date"), "{}", r.out());
    assert!(
        r.out()
            .contains("note   lib: lockfile or Node version changed, installing again"),
        "{}",
        r.out()
    );
    let hidden = fs::read_to_string(wt.join("lib/node_modules/.package-lock.json")).unwrap();
    assert!(hidden.contains("#bbb"), "{hidden}");
}

#[test]
fn check_exits_0_when_its_stdout_is_closed() {
    let w = World::new();
    // Nothing installed in the main checkout: both modes have lines to print.
    let zeros = "0000000000000000000000000000000000000000";
    for args in [
        vec!["check"],
        vec!["check", "--post-checkout", "1111", "2222", "1"],
        vec!["check", "--post-checkout", zeros, "2222", "1"],
    ] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let mut c = Command::new(env!("CARGO_BIN_EXE_fast-worktree"));
        w.env(&mut c);
        let out = c
            .args(&args)
            .current_dir(w.main())
            .stdout(writer)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{args:?}");
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    #[test]
    fn a_tree_without_its_record_is_not_cached() {
        let w = World::new();
        let main = w.main();
        let r = w.fw_with(
            &main,
            &["add", "../wt1", "-b", "a"],
            &[("FAKE_NPM_PARTIAL", "1")],
        );
        r.assert_ok();
        assert!(
            r.out()
                .contains("note   app: cache not saved: node_modules has no .package-lock.json"),
            "{}",
            r.out()
        );
        let entries: Vec<_> = walk_dirs(&w.cache)
            .into_iter()
            .filter(|p| p.join("dep").is_dir())
            .collect();
        assert!(entries.is_empty(), "{entries:?}");
        // The next worktree neither clones the cache nor wt1's tree.
        let r = w.fw(&main, &["add", "../wt2", "-b", "b"]);
        r.assert_ok();
        assert!(r.out().contains("npm ci app ("), "{}", r.out());
        assert!(r.out().contains("npm ci lib ("), "{}", r.out());
    }

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
                .contains("note   app: absolute symlink in node_modules, not cloned"),
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

// ---- v0.2 ----

impl World {
    /// Write files under `dir` and commit everything.
    fn commit(&self, dir: &Path, files: &[(&str, &str)], msg: &str) {
        for (f, body) in files {
            let p = dir.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        self.git(dir, &["add", "-A"]);
        self.git(dir, &["commit", "-qm", msg]);
    }

    /// Run git without asserting success, capturing its output (hooks
    /// included).
    fn git_run(&self, dir: &Path, args: &[&str], extra: &[(&str, &str)]) -> Run {
        let mut c = Command::new("git");
        self.env(&mut c);
        for (k, v) in extra {
            c.env(k, v);
        }
        Run(c.args(args).current_dir(dir).output().unwrap())
    }

    /// `PATH` with `dirs` (under the world) between the fake npm and the
    /// system.
    fn path_with(&self, dirs: &[&str]) -> String {
        let mut parts = vec![self.base.join("bin").display().to_string()];
        parts.extend(dirs.iter().map(|d| self.base.join(d).display().to_string()));
        parts.push("/usr/bin:/bin:/usr/sbin:/sbin".into());
        parts.join(":")
    }

    /// The cache entries of one project directory (slug) of the repository.
    #[cfg(target_os = "macos")]
    fn entries_in(&self, cache: &Path, slug: &str) -> Vec<PathBuf> {
        let Some(repo) = fs::read_dir(cache)
            .into_iter()
            .flatten()
            .flatten()
            .find(|e| e.file_name() != "trash")
        else {
            return Vec::new();
        };
        let mut v: Vec<PathBuf> = fs::read_dir(repo.path().join(slug))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        v.sort();
        v
    }
}

impl Run {
    /// stdout and stderr together.
    fn all(&self) -> String {
        format!("{}{}", self.out(), self.err())
    }
}

#[track_caller]
fn status_is_clean(w: &World, dir: &Path) {
    assert_eq!(
        w.git(dir, &["status", "--porcelain"]),
        "",
        "{}",
        dir.display()
    );
}

#[test]
fn worktreeinclude_copies_only_ignored_matches() {
    let w = World::new();
    let main = w.main();
    w.commit(
        &main,
        &[
            (
                ".gitignore",
                "node_modules/\n.env\n.env.local\nconfig/\nlogs/\n",
            ),
            (
                ".worktreeinclude",
                "# copied into new worktrees\n.env*\nconfig/\n",
            ),
            (".env.example", "committed\n"),
        ],
        "include",
    );
    // A branch that tracks a file the main checkout has as ignored.
    w.git(&main, &["switch", "-qc", "with-local"]);
    fs::write(main.join(".env.local"), "from branch\n").unwrap();
    fs::create_dir_all(main.join("config")).unwrap();
    fs::write(main.join("config/app.json"), "from branch\n").unwrap();
    w.git(&main, &["add", "-f", ".env.local", "config/app.json"]);
    w.git(&main, &["commit", "-qm", "track .env.local"]);
    w.git(&main, &["switch", "-q", "trunk"]);
    for (f, body) in [
        (".env", "main env\n"),
        (".env.local", "main local\n"),
        (".env.draft", "untracked, not ignored\n"),
        (".env.example", "modified, tracked\n"),
        ("app/.env", "app env\n"),
        ("config/db.json", "{}\n"),
        ("logs/.env.old", "in a directory no pattern reaches\n"),
        ("node_modules/pkg/.env", "a dependency's own\n"),
    ] {
        let p = main.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    let r = w.fw(&main, &["add", "../wt", "-b", "x", "HEAD"]);
    r.assert_ok();
    let wt = w.base.join("wt");
    for line in [
        "copy   .env\n",
        "copy   .env.local\n",
        "copy   app/.env\n",
        "copy   config\n",
    ] {
        assert!(r.out().contains(line), "{line}: {}", r.out());
    }
    let read = |p: &str| fs::read_to_string(wt.join(p)).unwrap();
    assert_eq!(read(".env"), "main env\n");
    assert_eq!(read(".env.local"), "main local\n");
    assert_eq!(read("app/.env"), "app env\n");
    assert_eq!(read("config/db.json"), "{}\n");
    assert_eq!(read(".env.example"), "committed\n", "tracked: not copied");
    assert!(!wt.join(".env.draft").exists(), "not ignored");
    assert!(!wt.join("logs").exists());
    assert!(!wt.join("node_modules").exists());
    status_is_clean(&w, &wt);

    // What the new worktree already has is kept.
    let r = w.fw(&main, &["add", "../wt2", "-b", "y", "with-local"]);
    r.assert_ok();
    assert!(
        r.out()
            .contains("keep   .env.local (already in the worktree)"),
        "{}",
        r.out()
    );
    // A directory both sides have is merged: its other files still come.
    assert!(r.out().contains("copy   config/db.json\n"), "{}", r.out());
    let wt2 = w.base.join("wt2");
    assert_eq!(
        fs::read_to_string(wt2.join(".env.local")).unwrap(),
        "from branch\n"
    );
    assert_eq!(
        fs::read_to_string(wt2.join("config/app.json")).unwrap(),
        "from branch\n"
    );
    assert_eq!(
        fs::read_to_string(wt2.join("config/db.json")).unwrap(),
        "{}\n"
    );
    assert_eq!(fs::read_to_string(wt2.join(".env")).unwrap(), "main env\n");
    status_is_clean(&w, &wt2);
}

#[test]
fn flutter_and_dart_projects() {
    let w = World::new();
    let main = w.main();
    w.commit(
        &main,
        &[
            (".gitignore", "node_modules/\n.dart_tool/\n"),
            ("mobile/pubspec.yaml", "name: mobile\n"),
            ("mobile/pubspec.lock", "packages: {}\n"),
            // A path dependency of mobile: no lockfile, not a project.
            ("mobile/pkg/pubspec.yaml", "name: pkg\n"),
        ],
        "mobile",
    );
    write_exe(&w.base.join("flutter-bin/flutter"), FAKE_PUB);
    write_exe(&w.base.join("dart-bin/dart"), FAKE_PUB);
    let flutter = w.path_with(&["flutter-bin", "dart-bin"]);
    let dart = w.path_with(&["dart-bin"]);
    let mobile = main.join("mobile");
    let pub_log = || fs::read_to_string(w.base.join("pub.log")).unwrap_or_default();

    let r = w.fw(&main, &["check", "--quiet"]);
    assert!(
        r.out().contains("missing deps: mobile (pub)\n"),
        "{}",
        r.out()
    );
    assert!(!r.out().contains("pkg"));

    // No SDK: a note, not a failure.
    let r = w.fw(&main, &["install", "mobile"]);
    r.assert_ok();
    assert!(
        r.out()
            .contains("note   mobile: neither flutter nor dart is installed, skipped"),
        "{}",
        r.out()
    );

    // flutter, offline first.
    let r = w.fw_with(&main, &["install", "mobile"], &[("PATH", &flutter)]);
    r.assert_ok();
    assert!(r.out().contains("pub get mobile (flutter, "), "{}", r.out());
    assert_eq!(
        pub_log(),
        format!("flutter pub get --offline {}\n", mobile.display())
    );
    assert!(mobile.join(".dart_tool/package_config.json").is_file());
    assert!(!mobile.join("pkg/.dart_tool").exists());
    assert!(!w.fw(&main, &["check", "--quiet"]).out().contains("mobile"));
    let r = w.fw_with(&main, &["install", "mobile"], &[("PATH", &flutter)]);
    assert!(r
        .out()
        .contains("skip   mobile (.dart_tool already present)"));

    // The offline attempt fails: online.
    fs::remove_dir_all(mobile.join(".dart_tool")).unwrap();
    let r = w.fw_with(
        &main,
        &["install", "mobile"],
        &[("PATH", &flutter), ("FAKE_PUB_OFFLINE_FAIL", "1")],
    );
    r.assert_ok();
    assert!(
        r.out().contains("pub get mobile (flutter, online, "),
        "{}",
        r.out()
    );
    assert!(pub_log().ends_with(&format!(
        "flutter pub get --offline {0}\nflutter pub get {0}\n",
        mobile.display()
    )));

    // dart only.
    fs::remove_dir_all(mobile.join(".dart_tool")).unwrap();
    let r = w.fw_with(&main, &["install", "mobile"], &[("PATH", &dart)]);
    r.assert_ok();
    assert!(r.out().contains("pub get mobile (dart, "), "{}", r.out());
    assert!(pub_log().ends_with(&format!("dart pub get {}\n", mobile.display())));

    // --changed: a new lockfile is fetched again, once.
    w.commit(
        &main,
        &[("mobile/pubspec.lock", "packages: {a: 1}\n")],
        "bump",
    );
    let r = w.fw_with(
        &main,
        &["install", "--changed", "mobile"],
        &[("PATH", &dart)],
    );
    assert!(r.out().contains("pub get mobile (dart, "), "{}", r.out());
    let r = w.fw_with(
        &main,
        &["install", "--changed", "mobile"],
        &[("PATH", &dart)],
    );
    assert!(
        r.out().contains("skip   mobile (up to date)"),
        "{}",
        r.out()
    );
}

#[test]
fn post_checkout_hook_clones_only_into_new_worktrees() {
    let w = World::new();
    let main = w.main();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_fast-worktree"),
        w.base.join("bin/fast-worktree"),
    )
    .unwrap();
    // The hook as the README shows it.
    write_exe(
        &main.join(".git/hooks/post-checkout"),
        "#!/bin/sh\ncommand -v fast-worktree >/dev/null 2>&1 && fast-worktree check --post-checkout \"$@\" || true\n",
    );

    // `add` tells the hook to stand aside and installs once.
    let r = w.fw(&main, &["add", "../wt1", "-b", "a"]);
    r.assert_ok();
    assert_eq!(w.count("ci "), 2, "{}", w.npm_log());
    assert!(!r.all().contains("missing deps"), "{}", r.all());

    // A plain `git worktree add`: clones what it can, never installs.
    let r = w.git_run(
        &main,
        &["worktree", "add", "../wt2", "-b", "b", "origin/trunk"],
        &[],
    );
    r.assert_ok();
    let wt2 = w.base.join("wt2");
    if cfg!(target_os = "macos") {
        assert!(r.all().contains("clone  app from "), "{}", r.all());
        assert!(wt2.join("app/node_modules/.package-lock.json").is_file());
        assert!(wt2.join("lib/node_modules/.package-lock.json").is_file());
        assert!(!r.all().contains("missing deps"), "{}", r.all());
    } else {
        assert!(r.all().contains("missing deps: app (npm)"), "{}", r.all());
        assert!(!wt2.join("app/node_modules").exists());
    }
    assert_eq!(w.count("ci "), 2, "{}", w.npm_log());
    assert_eq!(w.count("install "), 0, "{}", w.npm_log());

    // `git switch`: only lists.
    let _ = fs::remove_dir_all(wt2.join("lib/node_modules"));
    let npm_calls = w.npm_log().lines().count();
    let r = w.git_run(&wt2, &["switch", "-qc", "c"], &[]);
    r.assert_ok();
    assert!(r.all().contains("missing deps: lib (npm)"), "{}", r.all());
    assert!(!wt2.join("lib/node_modules").exists());
    assert_eq!(w.npm_log().lines().count(), npm_calls, "{}", w.npm_log());

    // CI, FAST_WORKTREE_SKIP: nothing at all.
    for (k, v) in [("CI", "true"), ("FAST_WORKTREE_SKIP", "1")] {
        let path = format!("../wt-{k}");
        let r = w.git_run(
            &main,
            &["worktree", "add", &path, "-b", k, "origin/trunk"],
            &[(k, v)],
        );
        r.assert_ok();
        assert!(!r.all().contains("deps"), "{k}: {}", r.all());
        assert!(!w.base.join(format!("wt-{k}/app/node_modules")).exists());
    }

    // A lockfile nothing can be cloned for: listed, not installed.
    w.commit(
        &main,
        &[("app/package-lock.json", &lockfile_v("app", "2.0.0"))],
        "bump",
    );
    let r = w.git_run(
        &main,
        &["worktree", "add", "../wt3", "-b", "d", "HEAD"],
        &[],
    );
    r.assert_ok();
    assert!(r.all().contains("missing deps: app (npm)"), "{}", r.all());
    assert!(
        r.all().contains("run `fast-worktree install`"),
        "{}",
        r.all()
    );
    assert!(!w.base.join("wt3/app/node_modules").exists());
    assert_eq!(w.count("ci "), 2, "{}", w.npm_log());
    assert_eq!(w.count("install "), 0, "{}", w.npm_log());

    // Called directly, outside a repository, it still exits 0.
    w.fw(&w.base, &["check", "--post-checkout", "0", "0", "1"])
        .assert_ok();
}

#[test]
fn cache_ls_and_prune() {
    let w = World::new();
    let main = w.main();
    let r = w.fw(&main, &["cache", "ls"]);
    r.assert_ok();
    assert!(r.out().starts_with("cache is empty"), "{}", r.out());

    let old = w.cache.join("app-1234abcd/_root/aaaaaaaaaaaaaaaa-v20.0.0");
    let new = w
        .cache
        .join("app-1234abcd/packages__web/bbbbbbbbbbbbbbbb-v20.0.0");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    fs::create_dir_all(w.cache.join("trash/1-1-1-x")).unwrap();
    let old_marker = w
        .cache
        .join("app-1234abcd/_root/aaaaaaaaaaaaaaaa-v20.0.0.ok");
    fs::write(&old_marker, "").unwrap();
    // Noon, so that the UTC date is the same in every time zone.
    let t = Command::new("touch")
        .args(["-t", "202001011200"])
        .arg(&old)
        .status()
        .unwrap();
    assert!(t.success());

    let r = w.fw(&main, &["cache", "ls"]);
    r.assert_ok();
    let out = r.out();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[0].starts_with("REPO "), "{out}");
    assert!(
        lines[1].starts_with("app-1234abcd  .             aaaaaaaaaaaaaaaa-v20.0.0  2020-01-01 ("),
        "{out}"
    );
    assert!(lines[2].contains("  packages/web  "), "{out}");
    assert!(lines[3].starts_with("2 entries in "), "{out}");

    let r = w.fw(&main, &["cache", "prune"]);
    r.assert_ok();
    assert!(
        r.out()
            .contains("pruned app-1234abcd/_root/aaaaaaaaaaaaaaaa-v20.0.0\n"),
        "{}",
        r.out()
    );
    assert!(r
        .out()
        .contains("pruned 1 entry unused for more than 7 days"));
    assert!(!old.exists());
    assert!(!old_marker.exists());
    assert!(new.exists());

    let r = w.fw(&main, &["cache", "prune", "--older-than", "0"]);
    r.assert_ok();
    assert!(r.out().contains("pruned 1 entry "), "{}", r.out());
    assert!(w
        .fw(&main, &["cache", "ls"])
        .out()
        .starts_with("cache is empty"));
}

#[test]
fn remove_delete_branch_refuses_an_unmerged_branch_before_removing() {
    let w = World::new();
    let main = w.main();
    w.fw(&main, &["add", "../wt", "-b", "feat"]).assert_ok();
    let wt = w.base.join("wt");
    w.commit(&wt, &[("new.txt", "x")], "work");
    let r = w.fw(&main, &["remove", "../wt", "--delete-branch"]);
    r.assert_fails();
    assert!(
        r.err()
            .contains("branch feat is not merged into origin/trunk"),
        "{}",
        r.err()
    );
    assert!(r.err().contains("nothing was removed"), "{}", r.err());
    assert!(wt.join("new.txt").is_file());
    assert!(!w.hook_log().contains("pre_remove"), "refused before hooks");
    w.fw(&main, &["remove", "../wt"]).assert_ok();
    assert!(!wt.exists());
    // git agrees: `git branch -d` keeps it.
    assert!(!w.git_run(&main, &["branch", "-d", "feat"], &[]).ok());

    // Without an upstream, HEAD decides, as for `git branch -d`.
    w.fw(&main, &["add", "../wt2", "-b", "local", "HEAD"])
        .assert_ok();
    let wt2 = w.base.join("wt2");
    w.commit(&wt2, &[("new.txt", "x")], "work");
    let r = w.fw(&main, &["remove", "../wt2", "--delete-branch"]);
    r.assert_fails();
    assert!(
        r.err().contains("branch local is not merged into HEAD"),
        "{}",
        r.err()
    );
    assert!(wt2.exists());
    w.fw(&main, &["remove", "../wt2"]).assert_ok();
    assert!(!w.git_run(&main, &["branch", "-d", "local"], &[]).ok());
}

#[test]
fn install_changed_reinstalls_only_what_changed() {
    let w = World::new();
    let main = w.main();
    w.fw(&main, &["add", "../wt", "-b", "a"]).assert_ok();
    let wt = w.base.join("wt");
    let bumped = lockfile_v("app", "1.1.0");
    w.commit(&wt, &[("app/package-lock.json", &bumped)], "bump");

    let r = w.fw(&wt, &["install"]);
    assert!(r
        .out()
        .contains("skip   app (node_modules already present)"));
    let r = w.fw(&wt, &["install", "--changed"]);
    r.assert_ok();
    assert!(
        r.out()
            .contains("note   app: lockfile or Node version changed, installing again"),
        "{}",
        r.out()
    );
    if cfg!(target_os = "macos") {
        assert!(r.out().contains("delta  app from "), "{}", r.out());
    } else {
        assert!(r.out().contains("npm ci app ("), "{}", r.out());
    }
    assert!(r.out().contains("skip   lib (up to date)"), "{}", r.out());
    assert_eq!(
        fs::read_to_string(wt.join("app/node_modules/.package-lock.json")).unwrap(),
        bumped
    );
    let r = w.fw(&wt, &["install", "--changed"]);
    assert!(r.out().contains("skip   app (up to date)"), "{}", r.out());
}

#[cfg(target_os = "macos")]
mod macos_v02 {
    use super::*;

    #[test]
    fn a_changed_lockfile_is_installed_on_top_of_the_latest_entry() {
        let w = World::new();
        let main = w.main();
        w.fw(&main, &["add", "../wt1", "-b", "a"]).assert_ok();
        let v11 = lockfile_v("app", "1.1.0");
        w.commit(&main, &[("app/package-lock.json", &v11)], "bump app");

        let r = w.fw(&main, &["add", "../wt2", "-b", "b", "HEAD"]);
        r.assert_ok();
        let wt2 = w.base.join("wt2");
        assert!(
            r.out()
                .contains(&format!("delta  app from {}/", w.cache.display())),
            "{}",
            r.out()
        );
        assert!(r.out().contains("clone  lib from "), "{}", r.out());
        assert_eq!(w.count("ci "), 2, "{}", w.npm_log());
        assert!(w.npm_log().contains(&format!(
            "install --prefer-offline {}",
            wt2.join("app").display()
        )));
        assert_eq!(
            fs::read_to_string(wt2.join("app/node_modules/.package-lock.json")).unwrap(),
            v11
        );
        assert_eq!(
            fs::read_to_string(wt2.join("app/node_modules/dep/index.js")).unwrap(),
            "updated\n"
        );
        let entries = w.entries_in(&w.cache, "app");
        assert_eq!(entries.len(), 2, "{entries:?}");
        status_is_clean(&w, &wt2);

        // The result was cached: the next worktree clones it.
        let r = w.fw(&main, &["add", "../wt3", "-b", "c", "HEAD"]);
        assert!(r.out().contains("clone  app from "), "{}", r.out());
        assert_eq!(w.count("install "), 1);

        // npm install rewrote the lockfile: npm ci, and the lockfile is back.
        let v12 = lockfile_v("app", "1.2.0");
        w.commit(&main, &[("app/package-lock.json", &v12)], "bump again");
        let r = w.fw_with(
            &main,
            &["add", "../wt4", "-b", "d", "HEAD"],
            &[("FAKE_NPM_INSTALL_REWRITES", "1")],
        );
        r.assert_ok();
        assert!(
            r.out().contains(
                "note   app: npm install changed package-lock.json, falling back to npm ci"
            ),
            "{}",
            r.out()
        );
        assert!(r.out().contains("npm ci app ("), "{}", r.out());
        let wt4 = w.base.join("wt4");
        assert_eq!(
            fs::read_to_string(wt4.join("app/package-lock.json")).unwrap(),
            v12
        );
        status_is_clean(&w, &wt4);
        assert_eq!(w.count("ci "), 3);

        // npm install left a tree that does not match: npm ci.
        let v13 = lockfile_v("app", "1.3.0");
        w.commit(&main, &[("app/package-lock.json", &v13)], "and again");
        let r = w.fw_with(
            &main,
            &["add", "../wt5", "-b", "e", "HEAD"],
            &[("FAKE_NPM_INSTALL_STALE", "1")],
        );
        r.assert_ok();
        assert!(
            r.out().contains(
                "note   app: npm install left a tree that differs from package-lock.json, \
                 falling back to npm ci"
            ),
            "{}",
            r.out()
        );
        assert_eq!(w.count("ci "), 4);
        assert_eq!(
            fs::read_to_string(w.base.join("wt5/app/node_modules/.package-lock.json")).unwrap(),
            v13
        );
    }

    #[test]
    fn skip_lifecycle_when_the_inputs_are_unchanged() {
        let w = World::new();
        let main = w.main();
        let cfg = format!(
            "{HOOKS}\n[[skip_lifecycle]]\ndir = \"app\"\nscript = \"postinstall\"\n\
             if_equal = [[\"gen-input.txt\", \"node_modules/dep/index.js\"]]\n"
        );
        // The fake npm ci writes "dep\n" to node_modules/dep/index.js.
        w.commit(
            &main,
            &[
                (".fast-worktree.toml", &cfg),
                ("app/gen-input.txt", "dep\n"),
            ],
            "skip rule",
        );
        w.fw(&main, &["add", "../wt1", "-b", "a", "HEAD"])
            .assert_ok();
        let r = w.fw(&main, &["add", "../wt2", "-b", "b", "HEAD"]);
        r.assert_ok();
        assert!(
            r.out()
                .contains("note   app: postinstall skipped, inputs unchanged"),
            "{}",
            r.out()
        );
        assert!(r.out().contains("clone  app from "), "{}", r.out());
        assert_eq!(w.count("run "), 0, "{}", w.npm_log());

        w.commit(
            &main,
            &[("app/gen-input.txt", "changed\n")],
            "input changed",
        );
        let r = w.fw(&main, &["add", "../wt3", "-b", "c", "HEAD"]);
        r.assert_ok();
        assert!(!r.out().contains("skipped"), "{}", r.out());
        assert_eq!(w.count("run postinstall"), 1, "{}", w.npm_log());
    }

    #[test]
    fn cache_entries_are_scanned_once_when_stored() {
        let w = World::new();
        let main = w.main();
        w.fw(&main, &["add", "../wt1", "-b", "a"]).assert_ok();
        let entry = w.entries_in(&w.cache, "app").pop().unwrap();
        let marker = PathBuf::from(format!("{}.ok", entry.display()));
        assert!(marker.is_file());
        let lib = &w.entries_in(&w.cache, "lib")[0];
        assert!(PathBuf::from(format!("{}.ok", lib.display())).is_file());

        // With the marker, a clone does not walk the tree: a symlink planted
        // in the entry afterwards comes through.
        std::os::unix::fs::symlink(&w.base, entry.join("dep/abs")).unwrap();
        let r = w.fw(&main, &["add", "../wt2", "-b", "b"]);
        r.assert_ok();
        assert!(r.out().contains("clone  app from "), "{}", r.out());
        assert!(w
            .base
            .join("wt2/app/node_modules/dep/abs")
            .symlink_metadata()
            .is_ok());

        // Without it, the tree is walked and the clone refused.
        fs::remove_file(&marker).unwrap();
        let r = w.fw(&main, &["add", "../wt3", "-b", "c"]);
        r.assert_ok();
        assert!(
            r.out()
                .contains("note   app: absolute symlink in node_modules, not cloned"),
            "{}",
            r.out()
        );
        assert!(r.out().contains("npm ci app ("), "{}", r.out());
        assert!(w
            .base
            .join("wt3/app/node_modules/dep/abs")
            .symlink_metadata()
            .is_err());

        // A tree with an absolute symlink is not cached.
        let cache2 = w.base.join("cache2");
        w.commit(
            &main,
            &[("app/package-lock.json", &lockfile_v("app", "3.0.0"))],
            "bump",
        );
        let r = w.fw_with(
            &main,
            &["add", "../wt4", "-b", "d", "HEAD"],
            &[
                ("FAST_WORKTREE_CACHE", cache2.to_str().unwrap()),
                ("FAKE_NPM_ABS_LINK", "1"),
            ],
        );
        r.assert_ok();
        assert!(
            r.out()
                .contains("note   app: cache not saved: absolute symlink in node_modules"),
            "{}",
            r.out()
        );
        assert!(w.entries_in(&cache2, "app").is_empty());
    }

    #[test]
    fn a_failing_lifecycle_script_discards_the_clone() {
        let w = World::new();
        let main = w.main();
        w.fw(&main, &["add", "../wt1", "-b", "a"]).assert_ok();
        let r = w.fw_with(
            &main,
            &["add", "../wt2", "-b", "b"],
            &[("FAKE_NPM_RUN_FAIL", "1")],
        );
        r.assert_fails();
        assert!(r.out().contains("FAILED app:"), "{}", r.out());
        assert!(
            r.out().contains("npm run postinstall failed after clone"),
            "{}",
            r.out()
        );
        let wt2 = w.base.join("wt2");
        assert!(!wt2.join("app/node_modules").exists());
        assert!(wt2.join("lib/node_modules").is_dir());
        let r = w.fw(&wt2, &["install"]);
        r.assert_ok();
        assert!(r.out().contains("clone  app from "), "{}", r.out());
    }
}
