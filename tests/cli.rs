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
P=$(pwd -P)
echo "$1 $2 $P" >> "$FAKE_NPM_LOG"
# Fails while another call that set the same directory is running: the
# project's own lifecycle writing a file that only one writer may hold.
exclusive() {
  [ -n "$FAKE_NPM_EXCLUSIVE" ] || return 0
  mkdir "$FAKE_NPM_EXCLUSIVE" 2>/dev/null || { echo "fake npm: another lifecycle is running" >&2; exit 1; }
  sleep 0.5
  rmdir "$FAKE_NPM_EXCLUSIVE"
}
case "$1" in
  ci)
    exclusive
    if [ -n "$FAKE_NPM_SLEEP" ]; then sleep "$FAKE_NPM_SLEEP"; fi
    # Like npm, which resolves its prefix once: write by absolute path.
    if [ -n "$FAKE_NPM_ABS_WRITE" ]; then mkdir -p "$P/node_modules/late"; fi
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
    exclusive
    if [ -n "$FAKE_NPM_RUN_FAIL" ]; then echo "script failed" >&2; exit 1; fi
    # The first call fails as git does when another process holds the
    # repository's config.
    if [ -n "$FAKE_NPM_RUN_LOCKED_ONCE" ] && [ ! -e "$FAKE_NPM_RUN_LOCKED_ONCE" ]; then
      : > "$FAKE_NPM_RUN_LOCKED_ONCE"
      echo "error: could not lock config file .git/config: File exists" >&2; exit 1
    fi
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

/// Run `fast-worktree` with stdout, and with `close_stderr` stderr too, a
/// pipe whose reader is gone: what `fast-worktree pool status | head -1`
/// does once `head` has its line.
fn fw_closed(w: &World, dir: &Path, args: &[&str], close_stderr: bool) -> Output {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let mut c = Command::new(env!("CARGO_BIN_EXE_fast-worktree"));
    w.env(&mut c);
    c.args(args).current_dir(dir);
    if close_stderr {
        c.stderr(writer.try_clone().unwrap());
    } else {
        c.stderr(std::process::Stdio::piped());
    }
    c.stdout(writer)
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap()
}

#[test]
fn commands_do_not_panic_when_their_output_is_closed() {
    let w = World::new();
    let main = w.main();
    w.pool_size(1);
    // A spare and a cache entry, and no refill running in the background.
    w.fw(&main, &["pool", "fill"]).assert_ok();
    for args in [
        vec!["pool", "status"],
        vec!["cache", "ls"],
        vec!["kept", "ls"],
        vec!["hook", "print-config"],
        vec!["wait"],
        vec!["cache", "prune"],
        vec!["kept", "prune"],
    ] {
        let out = fw_closed(&w, &main, &args, false);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "{args:?}: {err}");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {err}");
    }
    // An error on a closed stderr is an exit code, not a panic (101).
    let out = fw_closed(&w, &main, &["remove", "../no-such-worktree"], true);
    assert_eq!(out.status.code(), Some(1));
    let out = fw_closed(&w, &main, &["pool", "clear"], true);
    assert_eq!(out.status.code(), Some(0));
    let r = w.fw(&main, &["pool", "status"]);
    assert!(r.out().starts_with("pool   0 of 1 ready"), "{}", r.out());
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

// ---- v0.2: the pool and the Claude Code hooks ----

impl World {
    /// Turn the pool on in the user-wide config.
    fn pool_size(&self, n: u32) {
        let p = self.home().join(".config/fast-worktree/config.toml");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, format!("[pool]\nsize = {n}\n")).unwrap();
    }

    /// `<cache>/<repo>/pool`.
    fn pool_dir(&self) -> PathBuf {
        let repo = fs::read_dir(&self.cache)
            .unwrap()
            .flatten()
            .find(|e| e.file_name() != "trash" && e.path().join("pool").is_dir())
            .expect("a pool directory")
            .path();
        repo.join("pool")
    }

    /// The ready spares.
    fn spares(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(self.pool_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("wt-"))
            .map(|e| e.path())
            .collect();
        v.sort();
        v
    }

    /// Run fast-worktree with `input` on stdin.
    fn fw_stdin(&self, dir: &Path, args: &[&str], input: &str, extra: &[(&str, &str)]) -> Run {
        use std::io::Write;
        let mut c = Command::new(env!("CARGO_BIN_EXE_fast-worktree"));
        self.env(&mut c);
        for (k, v) in extra {
            c.env(k, v);
        }
        let mut child = c
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        Run(child.wait_with_output().unwrap())
    }

    fn hook_create(&self, name: &str, extra: &[(&str, &str)]) -> Run {
        let input = format!(
            r#"{{"session_id":"s","cwd":"{}","hook_event_name":"WorktreeCreate","name":"{name}"}}"#,
            self.main().display()
        );
        self.fw_stdin(&self.main(), &["hook", "create"], &input, extra)
    }

    fn hook_remove(&self, path: &Path) -> Run {
        let input = format!(
            r#"{{"session_id":"s","cwd":"{}","hook_event_name":"WorktreeRemove","worktree_path":"{}"}}"#,
            self.main().display(),
            path.display()
        );
        self.fw_stdin(&self.main(), &["hook", "remove"], &input, &[])
    }
}

/// Wait up to 30 seconds for `cond`.
#[track_caller]
fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let start = std::time::Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Hold the pool's fill lock, so that the fills `add` starts exit at once.
struct HeldFillLock(#[allow(dead_code)] fs::File);

fn hold_fill_lock(pool: &Path) -> HeldFillLock {
    use std::os::unix::io::AsRawFd;
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(pool.join("fill.lock"))
        .unwrap();
    // SAFETY: a valid descriptor, unlocked when the file is closed.
    assert_eq!(
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    HeldFillLock(f)
}

/// The last non-empty line of `s`.
fn last_line(s: &str) -> &str {
    s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("")
}

#[test]
fn the_pool_is_off_by_default() {
    let w = World::new();
    let main = w.main();
    let r = w.fw(&main, &["pool", "status"]);
    r.assert_ok();
    assert!(r.out().starts_with("pool   off"), "{}", r.out());
    let r = w.fw(&main, &["pool", "fill"]);
    r.assert_ok();
    assert!(r.out().contains("the pool is off"), "{}", r.out());
    let r = w.fw(&main, &["add", "../wt", "-b", "x"]);
    r.assert_ok();
    assert!(!r.out().contains("pool"), "{}", r.out());
}

#[test]
fn add_takes_a_spare_and_starts_a_refill() {
    let w = World::new();
    let main = w.main();
    // The post-checkout hook stays out of the way while spares are made.
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_fast-worktree"),
        w.base.join("bin/fast-worktree"),
    )
    .unwrap();
    write_exe(
        &main.join(".git/hooks/post-checkout"),
        "#!/bin/sh\nfast-worktree check --post-checkout \"$@\" || true\n",
    );
    w.pool_size(1);
    let r = w.fw(&main, &["pool", "fill"]);
    r.assert_ok();
    assert!(r.out().contains("fill  done: 1 of 1 ready"), "{}", r.all());
    assert!(!r.all().contains("missing deps"), "{}", r.all());
    assert_eq!(w.count("ci "), 2, "{}", w.npm_log());
    let spare = w.spares().pop().unwrap();
    assert!(spare.join("app/node_modules/.package-lock.json").is_file());
    let list = w.git(&main, &["worktree", "list", "--porcelain"]);
    assert!(list.contains("locked fast-worktree pool"), "{list}");

    let r = w.fw(&main, &["pool", "status"]);
    r.assert_ok();
    assert!(r.out().contains("pool   1 of 1 ready"), "{}", r.out());
    assert!(r.out().contains("BEHIND origin/trunk"), "{}", r.out());
    assert!(r.out().contains("misses 0"), "{}", r.out());

    // The spare is moved into place; nothing is installed again.
    let r = w.fw(&main, &["add", "../wt", "-b", "feat"]);
    r.assert_ok();
    let wt = w.base.join("wt");
    assert!(
        r.out().contains("worktree from the pool in "),
        "{}",
        r.out()
    );
    assert!(r.out().contains("skip   app (up to date)"), "{}", r.out());
    assert!(!spare.exists());
    assert!(wt.join("app/node_modules/.package-lock.json").is_file());
    assert_eq!(w.git(&wt, &["branch", "--show-current"]).trim(), "feat");
    assert_eq!(
        w.git(&wt, &["rev-parse", "HEAD"]),
        w.git(&main, &["rev-parse", "origin/trunk"])
    );
    assert_eq!(
        w.git(&wt, &["rev-parse", "--abbrev-ref", "feat@{upstream}"])
            .trim(),
        "origin/trunk"
    );
    let list = w.git(&main, &["worktree", "list", "--porcelain"]);
    let entry = list
        .split("\n\n")
        .find(|e| e.contains(&format!("worktree {}", wt.display())))
        .unwrap_or_else(|| panic!("{list}"));
    assert!(!entry.contains("locked"), "{entry}");
    assert!(w
        .hook_log()
        .contains(&format!("post_add {}|{}", wt.display(), main.display())));
    assert!(!r.all().contains("missing deps"), "{}", r.all());
    status_is_clean(&w, &wt);

    // A refill started in the background.
    wait_until("the refill", || {
        fs::read_to_string(w.pool_dir().join("fill.log"))
            .is_ok_and(|l| l.contains("fill  done: 1 of 1 ready"))
    });
    let spare = w.spares().pop().unwrap();
    assert!(spare.join("lib/node_modules/.package-lock.json").is_file());
    if cfg!(target_os = "macos") {
        assert_eq!(w.count("ci "), 2, "cloned from the cache: {}", w.npm_log());
    }

    // `remove` refuses a spare; `pool clear` removes them.
    let r = w.fw(&main, &["remove", spare.to_str().unwrap()]);
    r.assert_fails();
    assert!(r.err().contains("worktree is locked"), "{}", r.err());
    assert!(r.err().contains("fast-worktree pool clear"), "{}", r.err());
    assert!(spare.is_dir());
    let r = w.fw(&main, &["pool", "clear"]);
    r.assert_ok();
    assert!(r.out().contains("removed 1 spare(s)"), "{}", r.out());
    assert!(w.spares().is_empty());
    assert!(!w
        .git(&main, &["worktree", "list"])
        .contains(&w.pool_dir().display().to_string()));
    // The cache commands do not take spares for entries.
    let r = w.fw(&main, &["cache", "ls"]);
    assert!(!r.out().contains("wt-"), "{}", r.out());
}

#[test]
fn two_adds_at_once_take_one_spare() {
    let w = World::new();
    let main = w.main();
    w.pool_size(1);
    w.fw(&main, &["pool", "fill"]).assert_ok();
    let _lock = hold_fill_lock(&w.pool_dir());
    let start = |name: &str| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fast-worktree"));
        w.env(&mut c);
        c.args(["add", &format!("../{name}"), "-b", name])
            .current_dir(&main)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (a, b) = (start("a"), start("b"));
    let runs = [
        Run(a.wait_with_output().unwrap()),
        Run(b.wait_with_output().unwrap()),
    ];
    for r in &runs {
        r.assert_ok();
    }
    let pooled = runs
        .iter()
        .filter(|r| r.out().contains("worktree from the pool"))
        .count();
    let missed = runs
        .iter()
        .filter(|r| {
            r.out()
                .contains("note   pool: no spare ready, creating the worktree")
        })
        .count();
    assert_eq!(
        (pooled, missed),
        (1, 1),
        "{}\n---\n{}",
        runs[0].all(),
        runs[1].all()
    );
    for n in ["a", "b"] {
        let wt = w.base.join(n);
        assert!(wt.join("app/node_modules/.package-lock.json").is_file());
        assert_eq!(w.git(&wt, &["branch", "--show-current"]).trim(), n);
    }
    let r = w.fw(&main, &["pool", "status"]);
    assert!(r.out().contains("pool   0 of 1 ready"), "{}", r.out());
    assert!(r.out().contains("misses 1 "), "{}", r.out());
    assert!(r.out().contains("fill   running"), "{}", r.out());
}

#[test]
fn a_spare_behind_its_start_reinstalls_only_what_changed() {
    let w = World::new();
    let main = w.main();
    w.pool_size(1);
    w.fw(&main, &["pool", "fill"]).assert_ok();
    let _lock = hold_fill_lock(&w.pool_dir());
    // Upstream moves on: lib's lockfile changes, and a file of app.
    let up = w.base.join("upstream");
    w.commit(
        &up,
        &[
            ("lib/package-lock.json", &lockfile_v("lib", "1.1.0")),
            ("app/src.txt", "new\n"),
        ],
        "move on",
    );
    let ci_before = w.count("ci ");
    // From a subdirectory: the path is relative to it.
    let r = w.fw(&main.join("app"), &["add", "../../wt", "-b", "x"]);
    r.assert_ok();
    let wt = w.base.join("wt");
    assert!(r.out().contains("worktree from the pool"), "{}", r.out());
    assert_eq!(
        w.git(&wt, &["rev-parse", "HEAD"]),
        w.git(&up, &["rev-parse", "HEAD"])
    );
    assert!(r.out().contains("skip   app (up to date)"), "{}", r.out());
    assert!(
        r.out()
            .contains("note   lib: lockfile or Node version changed, installing again"),
        "{}",
        r.out()
    );
    assert_eq!(
        fs::read_to_string(wt.join("lib/node_modules/.package-lock.json")).unwrap(),
        lockfile_v("lib", "1.1.0")
    );
    if cfg!(target_os = "macos") {
        assert!(r.out().contains("delta  lib from "), "{}", r.out());
        assert_eq!(w.count("ci "), ci_before);
    } else {
        assert!(r.out().contains("npm ci lib ("), "{}", r.out());
        assert_eq!(w.count("ci "), ci_before + 1);
    }
    // app's dependencies stayed, and its own postinstall ran again because
    // its files changed.
    assert!(
        w.npm_log()
            .contains(&format!("run postinstall {}", wt.join("app").display())),
        "{}",
        w.npm_log()
    );
    assert!(r.out().contains("rerun  app: postinstall"), "{}", r.out());
    status_is_clean(&w, &wt);
}

#[test]
fn hook_create_prints_the_worktree_path_last() {
    let w = World::new();
    let main = w.main();
    w.commit(
        &w.base.join("upstream"),
        &[
            (".gitignore", "node_modules/\n.env\n.claude/worktrees/\n"),
            (".worktreeinclude", ".env\n"),
        ],
        "include",
    );
    w.git(&main, &["pull", "-q"]);
    fs::write(main.join(".env"), "SECRET=1\n").unwrap();

    let r = w.hook_create("feat", &[("FAKE_NPM_SLEEP", "3")]);
    r.assert_ok();
    let path = PathBuf::from(last_line(&r.out()));
    assert_eq!(
        r.out().trim(),
        path.display().to_string(),
        "only the path on stdout"
    );
    assert!(path.is_absolute() && path.is_dir(), "{}", path.display());
    assert_eq!(path, main.join(".claude/worktrees/feat"));
    assert_eq!(path.canonicalize().unwrap(), path, "no symlink, no ..");
    assert!(r.err().contains("worktree added in "), "{}", r.err());
    assert_eq!(
        w.git(&path, &["branch", "--show-current"]).trim(),
        "worktree-feat"
    );
    assert_eq!(
        w.git(&path, &["rev-parse", "HEAD"]),
        w.git(&main, &["rev-parse", "origin/trunk"])
    );
    assert_eq!(fs::read_to_string(path.join(".env")).unwrap(), "SECRET=1\n");

    // npm ci was left to the background: `check` says so, `wait` waits.
    let r = w.fw(&path, &["check", "--quiet"]);
    r.assert_ok();
    assert!(
        r.out().contains("installing deps in the background"),
        "{}",
        r.out()
    );
    let r = w.fw(&main, &["wait", path.to_str().unwrap()]);
    r.assert_ok();
    assert!(r.out().contains("background install done"), "{}", r.out());
    assert!(path.join("app/node_modules/.package-lock.json").is_file());
    assert!(path.join("lib/node_modules/.package-lock.json").is_file());
    assert_eq!(w.fw(&path, &["check", "--quiet"]).out(), "");
    // The post_add hooks ran after the background install.
    assert!(w
        .hook_log()
        .contains(&format!("post_add {}|{}", path.display(), main.display())));
    status_is_clean(&w, &path);

    // The same name again: the existing worktree.
    let r = w.hook_create("feat", &[]);
    r.assert_ok();
    assert_eq!(last_line(&r.out()), path.display().to_string());

    // On macOS the next one clones everything and needs no background.
    let r = w.hook_create("second", &[]);
    r.assert_ok();
    let second = PathBuf::from(last_line(&r.out()));
    if cfg!(target_os = "macos") {
        assert!(r.err().contains("clone  app from "), "{}", r.err());
        assert!(!r.err().contains("later"), "{}", r.err());
        assert!(second.join("app/node_modules").is_dir());
    }
    w.fw(&second, &["wait"]).assert_ok();
    assert!(second.join("app/node_modules/.package-lock.json").is_file());

    // An invalid name or no repository fails.
    w.hook_create("../escape", &[]).assert_fails();
    let r = w.fw_stdin(
        &w.base,
        &["hook", "create"],
        &format!(r#"{{"cwd":"{}","name":"x"}}"#, w.base.display()),
        &[],
    );
    r.assert_fails();
    assert_eq!(r.out(), "");
}

#[test]
fn hook_create_succeeds_when_the_dependencies_fail() {
    let w = World::new();
    let r = w.hook_create("broken", &[("FAKE_NPM_FAIL", "1")]);
    r.assert_ok();
    let path = PathBuf::from(last_line(&r.out()));
    assert!(path.is_dir());
    let r = w.fw(&path, &["wait"]);
    r.assert_fails();
    assert!(r.out().contains("background install failed"), "{}", r.out());
    assert!(
        r.out().contains("npm ERR! simulated failure"),
        "{}",
        r.out()
    );
    // Nothing waits any more, and `install` can finish the job.
    let r = w.fw(&path, &["install"]);
    r.assert_ok();
    assert!(path.join("app/node_modules/.package-lock.json").is_file());

    // Only git failing fails the hook: a branch that is checked out
    // elsewhere cannot be checked out again.
    w.git(&w.main(), &["branch", "worktree-taken"]);
    w.git(
        &w.main(),
        &["worktree", "add", "-q", "../other", "worktree-taken"],
    );
    let r = w.hook_create("taken", &[]);
    r.assert_fails();
    assert_eq!(r.out(), "");
}

#[test]
fn hook_create_uses_a_spare() {
    let w = World::new();
    let main = w.main();
    w.pool_size(1);
    w.fw(&main, &["pool", "fill"]).assert_ok();
    let _lock = hold_fill_lock(&w.pool_dir());
    let r = w.hook_create("pooled", &[]);
    r.assert_ok();
    let path = PathBuf::from(last_line(&r.out()));
    assert!(r.err().contains("worktree from the pool"), "{}", r.err());
    assert!(
        !r.err().contains("later"),
        "nothing to do later: {}",
        r.err()
    );
    assert_eq!(
        w.git(&path, &["branch", "--show-current"]).trim(),
        "worktree-pooled"
    );
    assert!(path.join("app/node_modules/.package-lock.json").is_file());
    assert!(w.spares().is_empty());
}

#[test]
fn hook_remove_deletes_changed_worktrees_and_keeps_unmerged_branches() {
    let w = World::new();
    let main = w.main();
    let r = w.hook_create("dirty", &[]);
    r.assert_ok();
    let dirty = PathBuf::from(last_line(&r.out()));
    w.fw(&dirty, &["wait"]).assert_ok();
    fs::write(dirty.join("notes.txt"), "unsaved\n").unwrap();
    fs::write(dirty.join("app/package.json"), "{}\n").unwrap();

    let r = w.hook_remove(&dirty);
    r.assert_ok();
    assert_eq!(r.out(), "");
    assert!(!dirty.exists());
    assert!(!w.git(&main, &["worktree", "list"]).contains("dirty"));
    assert!(r.err().contains("for at least 24 hours"), "{}", r.err());
    assert!(r.err().contains("to restore them: git -C "), "{}", r.err());
    // The changes are in the trash, kept; the branch had nothing of its own.
    let kept = fs::read_dir(w.cache.join("trash"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("keep-"))
        .expect("a kept trash entry")
        .path();
    assert_eq!(
        fs::read_to_string(kept.join("notes.txt")).unwrap(),
        "unsaved\n"
    );
    assert!(
        r.err().contains("deleted branch worktree-dirty"),
        "{}",
        r.err()
    );
    assert!(!w
        .git_run(&main, &["rev-parse", "--verify", "worktree-dirty"], &[])
        .ok());
    assert!(w.hook_log().contains("pre_remove"), "{}", w.hook_log());

    // A commit that is on no other ref keeps its branch.
    let r = w.hook_create("work", &[]);
    let work = PathBuf::from(last_line(&r.out()));
    w.fw(&work, &["wait"]).assert_ok();
    w.commit(&work, &[("new.txt", "x")], "work");
    let r = w.hook_remove(&work);
    r.assert_ok();
    assert!(!work.exists());
    assert!(
        r.err().contains("kept branch worktree-work: commit "),
        "{}",
        r.err()
    );
    assert!(w
        .git_run(&main, &["rev-parse", "--verify", "worktree-work"], &[])
        .ok());
    // ...and the next `claude --worktree work` checks it out again.
    let r = w.hook_create("work", &[]);
    r.assert_ok();
    let again = PathBuf::from(last_line(&r.out()));
    assert!(again.join("new.txt").is_file());
    w.fw(&again, &["wait"]).assert_ok();

    // Already gone: fine. The main checkout, or a directory that is not a
    // worktree: refused, and nothing is deleted.
    w.hook_remove(&w.base.join("nowhere")).assert_ok();
    let r = w.hook_remove(&main);
    r.assert_fails();
    assert!(main.join(".git").exists());
    let plain = w.base.join("plain");
    fs::create_dir_all(&plain).unwrap();
    w.hook_remove(&plain).assert_fails();
    assert!(plain.is_dir());
}

#[test]
fn wait_and_print_config() {
    let w = World::new();
    let r = w.fw(&w.main(), &["wait"]);
    r.assert_ok();
    assert!(
        r.out().contains("nothing is being installed"),
        "{}",
        r.out()
    );
    let r = w.fw(&w.main(), &["hook", "print-config"]);
    r.assert_ok();
    let v: serde_json::Value = serde_json::from_str(&r.out()).unwrap();
    assert_eq!(
        v["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"],
        "fast-worktree hook create"
    );
    assert_eq!(
        v["hooks"]["WorktreeRemove"][0]["hooks"][0]["command"],
        "fast-worktree hook remove"
    );
}

// ---- v0.2: hook remove and the pool, after review ----

impl World {
    /// The `keep-` entries of the trash.
    fn kept(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(self.cache.join("trash"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("keep-"))
            .map(|e| e.path())
            .collect();
        v.sort();
        v
    }

    /// A worktree made by `hook create`, with its background install done.
    fn hook_worktree(&self, name: &str) -> PathBuf {
        let r = self.hook_create(name, &[]);
        r.assert_ok();
        let path = PathBuf::from(last_line(&r.out()));
        self.fw(&path, &["wait"]).assert_ok();
        path
    }

    /// `ext`, a worktree on a volume that is not mounted right now: `git
    /// worktree prune` would drop it. Returns its path.
    fn unmounted_worktree(&self) -> PathBuf {
        fs::create_dir_all(self.base.join("vol")).unwrap();
        let ext = with_worktree(self, "../vol/ext");
        fs::rename(self.base.join("vol"), self.base.join("vol-unmounted")).unwrap();
        assert!(
            registered(self).contains("prunable"),
            "{}",
            registered(self)
        );
        ext
    }

    /// Run a command line the way a user would paste it.
    fn sh(&self, line: &str) {
        let mut c = Command::new("sh");
        self.env(&mut c);
        let out = c
            .args(["-c", line])
            .current_dir(&self.base)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{line}\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[track_caller]
fn still_registered(w: &World, ext: &Path) {
    let list = registered(w);
    assert!(
        list.contains(&format!("worktree {}\n", ext.display())),
        "{list}"
    );
}

/// What follows `prefix` on the line of `s` that starts with it.
#[track_caller]
fn after<'a>(s: &'a str, prefix: &str) -> &'a str {
    s.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no line starting with {prefix:?} in:\n{s}"))
}

/// The `refs/fast-worktree/kept/...` named in `s`.
#[track_caller]
fn kept_ref(s: &str) -> String {
    let i = s
        .find("refs/fast-worktree/kept/")
        .unwrap_or_else(|| panic!("no kept ref in:\n{s}"));
    s[i..]
        .split(|c: char| c.is_whitespace() || "`),;".contains(c))
        .next()
        .unwrap()
        .to_string()
}

fn have_rsync() -> bool {
    Command::new("rsync")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn hook_remove_keeps_untracked_files_whatever_the_status_config_says() {
    let w = World::new();
    let wt = w.hook_worktree("hidden");
    w.git(&wt, &["config", "status.showUntrackedFiles", "no"]);
    fs::write(wt.join("notes.txt"), "wip\n").unwrap();
    assert_eq!(w.git(&wt, &["status", "--porcelain"]), "");
    let r = w.hook_remove(&wt);
    r.assert_ok();
    assert!(!wt.exists());
    assert!(r.err().contains("for at least 24 hours"), "{}", r.err());
    let kept = w.kept();
    assert_eq!(kept.len(), 1, "{}", r.err());
    assert_eq!(
        fs::read_to_string(kept[0].join("notes.txt")).unwrap(),
        "wip\n"
    );
}

#[test]
fn hook_remove_keeps_a_detached_head_in_a_ref() {
    let w = World::new();
    let main = w.main();
    // Clean, detached, with a commit on no branch.
    let det = w.hook_worktree("det");
    w.git(&det, &["checkout", "-q", "--detach"]);
    w.commit(&det, &[("lonely.txt", "x")], "lonely");
    let sha = w.git(&det, &["rev-parse", "HEAD"]).trim().to_string();
    let r = w.hook_remove(&det);
    r.assert_ok();
    assert!(!det.exists());
    let name = kept_ref(&r.err());
    assert!(
        name.starts_with("refs/fast-worktree/kept/det-"),
        "{}",
        r.err()
    );
    assert_eq!(w.git(&main, &["rev-parse", &name]).trim(), sha);

    // Dirty and detached: kept, and the restore command uses the ref.
    let det2 = w.hook_worktree("det2");
    w.git(&det2, &["checkout", "-q", "--detach"]);
    w.commit(&det2, &[("lonely2.txt", "y")], "lonely2");
    let sha2 = w.git(&det2, &["rev-parse", "HEAD"]).trim().to_string();
    fs::write(det2.join("notes.txt"), "wip\n").unwrap();
    let r = w.hook_remove(&det2);
    r.assert_ok();
    let restore = after(&r.err(), "to restore them: ").to_string();
    let name2 = kept_ref(&restore);
    assert!(
        name2.starts_with("refs/fast-worktree/kept/det2-"),
        "{restore}"
    );
    assert_eq!(w.git(&main, &["rev-parse", &name2]).trim(), sha2);

    // Nothing else holds those commits: gc keeps them for the refs.
    w.git(&main, &["reflog", "expire", "--expire=now", "--all"]);
    w.git(&main, &["gc", "-q", "--prune=now"]);
    w.git(&main, &["cat-file", "-e", &sha]);
    w.git(&main, &["cat-file", "-e", &sha2]);
    if have_rsync() {
        w.sh(&restore);
        assert_eq!(w.git(&det2, &["rev-parse", "HEAD"]).trim(), sha2);
        assert_eq!(fs::read_to_string(det2.join("notes.txt")).unwrap(), "wip\n");
    }
}

#[test]
fn hook_remove_saves_the_git_directories_of_submodules() {
    let w = World::new();
    let sub_up = w.base.join("subup");
    fs::create_dir_all(&sub_up).unwrap();
    w.git(&sub_up, &["init", "-q", "-b", "trunk"]);
    w.commit(&sub_up, &[("s.txt", "s")], "s");
    let add_sub = |wt: &Path| {
        w.git(
            wt,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &sub_up.to_string_lossy(),
                "sub",
            ],
        );
        w.git(wt, &["commit", "-qm", "add sub"]);
    };
    let wt = w.hook_worktree("subs");
    add_sub(&wt);
    // A commit in the submodule that is nowhere else, recorded in the
    // superproject: `status` is clean.
    w.commit(&wt.join("sub"), &[("mine.txt", "m")], "unpushed");
    let unpushed = w.git(&wt.join("sub"), &["rev-parse", "HEAD"]);
    w.git(&wt, &["commit", "-qam", "bump sub"]);
    assert_eq!(w.git(&wt, &["status", "--porcelain"]), "");

    let r = w.hook_remove(&wt);
    r.assert_ok();
    assert!(!wt.exists());
    assert!(r.err().contains("submodule"), "{}", r.err());
    // Its HEAD is in the main repository too, under the removal's group.
    let refs = w.git(
        &w.main(),
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/fast-worktree/",
        ],
    );
    assert!(
        refs.lines()
            .any(|l| l.starts_with("refs/fast-worktree/kept/subs-")
                && l.ends_with(&format!("/sub/sub {}", unpushed.trim()))),
        "{refs}"
    );
    let kept = w.kept();
    assert_eq!(kept.len(), 1, "{}", r.err());
    // The submodule's git directory went with it.
    assert!(kept[0].join("sub/.git").is_dir(), "{}", r.err());
    assert_eq!(
        w.git(&kept[0].join("sub"), &["rev-parse", "HEAD"]),
        unpushed
    );
    if have_rsync() {
        w.sh(after(&r.err(), "to restore them: "));
        assert_eq!(w.git(&wt.join("sub"), &["rev-parse", "HEAD"]), unpushed);
        assert!(w
            .git(&wt.join("sub"), &["log", "--oneline"])
            .contains("unpushed"));
        assert_eq!(w.git(&wt, &["status", "--porcelain"]), "");
    }

    // A submodule whose git directory has no working tree (deinit): it
    // cannot be carried along, so nothing is removed.
    let wt2 = w.hook_worktree("deinit");
    add_sub(&wt2);
    w.git(&wt2, &["submodule", "deinit", "-q", "-f", "sub"]);
    let r = w.hook_remove(&wt2);
    r.assert_fails();
    assert!(r.err().contains("nothing was removed"), "{}", r.err());
    assert!(
        !w.hook_log()
            .contains(&format!("pre_remove {}|", wt2.display())),
        "{}",
        w.hook_log()
    );
    assert!(wt2.join(".git").is_file());
    assert!(registered(&w).contains(&wt2.display().to_string()));
}

#[test]
fn hook_remove_unregisters_only_its_own_worktree() {
    let w = World::new();
    let ext = w.unmounted_worktree();
    let clean = w.hook_worktree("clean");
    w.hook_remove(&clean).assert_ok();
    assert!(!registered(&w).contains(&clean.display().to_string()));
    still_registered(&w, &ext);

    let dirty = w.hook_worktree("dirty");
    fs::write(dirty.join("notes.txt"), "x").unwrap();
    w.hook_remove(&dirty).assert_ok();
    assert!(!registered(&w).contains(&dirty.display().to_string()));
    still_registered(&w, &ext);

    // Already gone: only its own entry is dropped.
    let gone = w.hook_worktree("gone");
    fs::remove_dir_all(&gone).unwrap();
    let r = w.hook_remove(&gone);
    r.assert_ok();
    assert!(
        !registered(&w).contains(&gone.display().to_string()),
        "{}",
        r.err()
    );
    still_registered(&w, &ext);
}

#[test]
fn the_pool_unregisters_only_its_own_worktrees() {
    let w = World::new();
    let main = w.main();
    w.pool_size(1);
    w.fw(&main, &["pool", "fill"]).assert_ok();
    let ext = w.unmounted_worktree();
    let r = w.fw(&main, &["pool", "clear"]);
    r.assert_ok();
    assert!(r.out().contains("removed 1 spare(s)"), "{}", r.out());
    assert!(!registered(&w).contains(&w.pool_dir().display().to_string()));
    still_registered(&w, &ext);

    // A spare that cannot be switched to the branch is thrown away.
    w.fw(&main, &["pool", "fill"]).assert_ok();
    let _lock = hold_fill_lock(&w.pool_dir());
    w.git(&main, &["branch", "worktree-taken"]);
    w.git(
        &main,
        &["worktree", "add", "-q", "../other", "worktree-taken"],
    );
    let r = w.hook_create("taken", &[]);
    r.assert_fails();
    assert!(r.err().contains("git switch failed"), "{}", r.err());
    still_registered(&w, &ext);
    let list = registered(&w);
    assert!(
        !list.contains(&main.join(".claude/worktrees/taken").display().to_string()),
        "{list}"
    );
}

#[test]
fn hook_remove_keeps_a_worktree_with_another_one_inside() {
    let w = World::new();
    let main = w.main();
    fs::write(main.join(".git/info/exclude"), ".claude/\n").unwrap();
    let outer = w.hook_worktree("outer");
    w.git(
        &outer,
        &[
            "worktree",
            "add",
            "-q",
            ".claude/worktrees/inner",
            "-b",
            "inner",
        ],
    );
    let inner = outer.join(".claude/worktrees/inner");
    fs::write(inner.join("inner-notes.txt"), "keep me\n").unwrap();
    assert_eq!(w.git(&outer, &["status", "--porcelain"]), "");

    let r = w.hook_remove(&outer);
    r.assert_ok();
    assert!(!outer.exists());
    assert!(
        r.err().contains(&inner.display().to_string()),
        "{}",
        r.err()
    );
    let kept = w.kept();
    assert_eq!(kept.len(), 1, "{}", r.err());
    let moved = kept[0].join(".claude/worktrees/inner");
    assert_eq!(
        fs::read_to_string(moved.join("inner-notes.txt")).unwrap(),
        "keep me\n"
    );
    // Still a worktree, registered where it is now.
    let moved = moved.canonicalize().unwrap();
    still_registered(&w, &moved);
    assert_eq!(w.git(&moved, &["branch", "--show-current"]).trim(), "inner");
}

#[test]
fn hook_remove_stops_the_background_install_first() {
    let w = World::new();
    let r = w.hook_create(
        "race",
        &[("FAKE_NPM_SLEEP", "3"), ("FAKE_NPM_ABS_WRITE", "1")],
    );
    r.assert_ok();
    let path = PathBuf::from(last_line(&r.out()));
    assert!(r.err().contains("later"), "{}", r.err());
    wait_until("npm ci to start", || w.count("ci ") == 2);
    w.hook_remove(&path).assert_ok();
    assert!(!path.exists());
    // Long after the install would have written into it.
    std::thread::sleep(std::time::Duration::from_secs(4));
    assert!(!path.exists(), "the background install brought it back");
    let r = w.hook_create("race", &[]);
    r.assert_ok();
    assert_eq!(last_line(&r.out()), path.display().to_string());
}

#[test]
fn fetches_never_wait_for_an_ssh_prompt() {
    let w = World::new();
    let main = w.main();
    let log = w.base.join("ssh.log");
    write_exe(
        &w.base.join("bin/ssh"),
        &format!(
            "#!/bin/sh\necho \"$* prompt=$GIT_TERMINAL_PROMPT\" >> '{}'\nexit 255\n",
            log.display()
        ),
    );
    w.git(
        &main,
        &["remote", "set-url", "origin", "ssh://example.invalid/x.git"],
    );
    let ssh_log = || fs::read_to_string(&log).unwrap_or_default();

    w.hook_create("one", &[]).assert_ok();
    let l = ssh_log();
    assert!(l.contains("-o BatchMode=yes"), "{l}");
    assert!(l.contains("prompt=0"), "{l}");

    // The user's own ssh command is kept, with BatchMode added.
    w.git(&main, &["config", "core.sshCommand", "ssh -F /dev/null"]);
    let _ = fs::remove_file(&log);
    w.hook_create("two", &[]).assert_ok();
    let l = ssh_log();
    assert!(l.contains("-F /dev/null"), "{l}");
    assert!(l.contains("-o BatchMode=yes"), "{l}");

    let _ = fs::remove_file(&log);
    w.pool_size(1);
    let r = w.fw_with(
        &main,
        &["pool", "fill"],
        &[("GIT_SSH_COMMAND", "ssh -p 2222")],
    );
    r.assert_ok();
    let l = ssh_log();
    assert!(l.contains("-p 2222"), "{l}");
    assert!(!l.contains("-F /dev/null"), "the environment wins: {l}");
    assert!(l.contains("-o BatchMode=yes"), "{l}");
    assert!(l.contains("prompt=0"), "{l}");
}

#[test]
fn add_stops_a_fetch_that_hangs_and_starts_from_origin_as_last_fetched() {
    let w = World::new();
    let main = w.main();
    let log = w.base.join("ssh.log");
    // An ssh that never answers (for longer than the test waits).
    write_exe(
        &w.base.join("bin/ssh"),
        &format!(
            "#!/bin/sh\necho \"$* prompt=$GIT_TERMINAL_PROMPT\" >> '{}'\nsleep 20\nexit 255\n",
            log.display()
        ),
    );
    w.git(
        &main,
        &["remote", "set-url", "origin", "ssh://example.invalid/x.git"],
    );
    let start = std::time::Instant::now();
    let r = w.fw(&main, &["add", "../wt", "-b", "x", "--fetch-timeout", "1"]);
    r.assert_ok();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(15),
        "waited for the fetch: {:?}",
        start.elapsed()
    );
    assert!(
        r.err()
            .contains("took longer than 1s; starting from origin/trunk as last fetched"),
        "{}",
        r.err()
    );
    let l = fs::read_to_string(&log).unwrap_or_default();
    assert!(l.contains("-o BatchMode=yes"), "{l}");
    assert!(l.contains("prompt=0"), "{l}");
    assert_eq!(
        w.git(&w.base.join("wt"), &["rev-parse", "HEAD"]),
        w.git(&main, &["rev-parse", "origin/trunk"])
    );

    // Nothing fetched before either: nothing to start from.
    w.git(&main, &["update-ref", "-d", "refs/remotes/origin/trunk"]);
    let r = w.fw(&main, &["add", "../wt2", "-b", "y", "--fetch-timeout", "1"]);
    r.assert_fails();
    assert!(
        r.err()
            .contains("and there is no origin/trunk to start from"),
        "{}",
        r.err()
    );
    assert!(!w.base.join("wt2").exists());
}

#[test]
fn a_spare_is_checked_for_absolute_symlinks_and_its_scripts_run_again_after_the_move() {
    let w = World::new();
    let main = w.main();
    w.pool_size(1);
    let r = w.fw_with(&main, &["pool", "fill"], &[("FAKE_NPM_ABS_LINK", "1")]);
    r.assert_ok();
    assert!(r.out().contains("absolute symlink"), "{}", r.out());
    let spare = w.spares().pop().unwrap();
    assert!(spare
        .join("app/node_modules/dep/abs")
        .symlink_metadata()
        .is_err());
    let lock = hold_fill_lock(&w.pool_dir());
    let r = w.fw(&main, &["add", "../wt", "-b", "feat"]);
    r.assert_ok();
    let wt = w.base.join("wt");
    assert!(r.out().contains("worktree from the pool"), "{}", r.out());
    assert!(wt.join("app/node_modules/.package-lock.json").is_file());
    assert!(
        wt.join("app/node_modules/dep/abs")
            .symlink_metadata()
            .is_err(),
        "a link into where the spare was made"
    );
    drop(lock);

    // Nothing changed since the spare was made: app's own postinstall
    // runs again anyway, in the worktree where it now is.
    let w = World::new();
    let main = w.main();
    w.pool_size(1);
    w.fw(&main, &["pool", "fill"]).assert_ok();
    let _lock = hold_fill_lock(&w.pool_dir());
    let r = w.fw(&main, &["add", "../wt", "-b", "feat"]);
    r.assert_ok();
    let wt = w.base.join("wt");
    assert!(r.out().contains("skip   app (up to date)"), "{}", r.out());
    assert!(
        w.npm_log()
            .contains(&format!("run postinstall {}", wt.join("app").display())),
        "{}",
        w.npm_log()
    );
}

/// `flutter pub get` that, like the real one, writes the project's absolute
/// path into generated files, and leaves them alone when they are there.
const FAKE_FLUTTER_GEN: &str = r#"#!/bin/sh
P=$(pwd -P)
echo "$(basename "$0") $* $P" >> "$FAKE_PUB_LOG"
mkdir -p .dart_tool ios/Flutter linux/flutter/ephemeral
echo '{}' > .dart_tool/package_config.json
[ -f ios/Flutter/Generated.xcconfig ] || printf 'FLUTTER_ROOT=/sdk\nFLUTTER_APPLICATION_PATH=%s\n' "$P" > ios/Flutter/Generated.xcconfig
[ -f ios/Flutter/flutter_export_environment.sh ] || { printf '#!/bin/sh\nexport "FLUTTER_APPLICATION_PATH=%s"\n' "$P" > ios/Flutter/flutter_export_environment.sh; chmod +x ios/Flutter/flutter_export_environment.sh; }
[ -f linux/flutter/ephemeral/flutter_native_integration.env ] || printf 'PROJECT=%s\n' "$P" > linux/flutter/ephemeral/flutter_native_integration.env
[ -f .flutter-plugins-dependencies ] || printf '{"root":"%s/","plugins":[]}' "$P" > .flutter-plugins-dependencies
[ -f .dart_tool/blob.bin ] || printf 'bin\000%s' "$P" > .dart_tool/blob.bin
"#;

#[test]
fn a_flutter_spare_has_the_paths_in_its_generated_files_rewritten_after_the_move() {
    let w = World::new();
    let main = w.main();
    let up = w.base.join("upstream");
    w.commit(
        &up,
        &[
            (
                ".gitignore",
                "node_modules/\n.dart_tool/\n.flutter-plugins*\n\
                 **/ios/Flutter/Generated.xcconfig\n**/ios/Flutter/flutter_export_environment.sh\n\
                 **/linux/flutter/ephemeral/\n",
            ),
            (
                "mobile/pubspec.yaml",
                "name: mobile\ndependencies:\n  flutter:\n    sdk: flutter\n",
            ),
            ("mobile/pubspec.lock", "packages: {}\n"),
            (
                "mobile/ios/Flutter/Debug.xcconfig",
                "#include \"Generated.xcconfig\"\n",
            ),
        ],
        "mobile",
    );
    w.git(&main, &["pull", "-q"]);
    write_exe(&w.base.join("flutter-bin/flutter"), FAKE_FLUTTER_GEN);
    let path = w.path_with(&["flutter-bin"]);
    w.pool_size(1);
    w.fw_with(&main, &["pool", "fill"], &[("PATH", &path)])
        .assert_ok();
    let spare = w.spares().pop().unwrap();
    let made = fs::read_to_string(spare.join("mobile/ios/Flutter/Generated.xcconfig")).unwrap();
    assert!(made.contains("/pool/.new-wt-"), "{made}");
    let _lock = hold_fill_lock(&w.pool_dir());
    let pub_calls = fs::read_to_string(w.base.join("pub.log")).unwrap();
    let r = w.fw_with(&main, &["add", "../wt", "-b", "feat"], &[("PATH", &path)]);
    r.assert_ok();
    assert!(r.out().contains("worktree from the pool"), "{}", r.out());
    let mobile = w.base.join("wt/mobile");
    for f in [
        "ios/Flutter/Generated.xcconfig",
        "ios/Flutter/flutter_export_environment.sh",
        "linux/flutter/ephemeral/flutter_native_integration.env",
        ".flutter-plugins-dependencies",
    ] {
        let text = fs::read_to_string(mobile.join(f)).unwrap();
        assert!(text.contains(&mobile.display().to_string()), "{f}: {text}");
        assert!(!text.contains("/pool/"), "{f}: {text}");
    }
    assert!(
        fs::metadata(mobile.join("ios/Flutter/flutter_export_environment.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100
            != 0
    );
    // A binary file is left as it is.
    let blob = fs::read(mobile.join(".dart_tool/blob.bin")).unwrap();
    assert!(String::from_utf8_lossy(&blob).contains("/pool/.new-wt-"));
    // `pub get` does not run again: it would not rewrite them.
    assert_eq!(
        fs::read_to_string(w.base.join("pub.log")).unwrap(),
        pub_calls
    );
    assert!(r.out().contains("paths  mobile: "), "{}", r.out());
    status_is_clean(&w, &w.base.join("wt"));
}

#[test]
fn hook_remove_refuses_a_worktree_the_user_locked() {
    let w = World::new();
    let main = w.main();
    let wt = w.hook_worktree("mine");
    w.git(
        &main,
        &[
            "worktree",
            "lock",
            "--reason",
            "on a trip",
            &wt.to_string_lossy(),
        ],
    );
    let r = w.hook_remove(&wt);
    r.assert_fails();
    assert!(r.err().contains("on a trip"), "{}", r.err());
    assert!(wt.join(".git").is_file());
    assert!(!w.hook_log().contains("pre_remove"), "{}", w.hook_log());
    assert!(registered(&w).contains("locked on a trip"));

    // Locked by the pool (a spare that was not unlocked): removed.
    let pooled = w.hook_worktree("pooled");
    w.git(
        &main,
        &[
            "worktree",
            "lock",
            "--reason",
            "fast-worktree pool",
            &pooled.to_string_lossy(),
        ],
    );
    w.hook_remove(&pooled).assert_ok();
    assert!(!pooled.exists());
    assert!(!registered(&w).contains(&pooled.display().to_string()));
}

#[test]
fn hook_remove_checks_that_the_worktree_can_be_moved_before_the_hooks() {
    // SAFETY: no arguments.
    if unsafe { libc::geteuid() } == 0 {
        return; // root can rename anywhere
    }
    let w = World::new();
    let wt = w.hook_worktree("stuck");
    let parent = wt.parent().unwrap().to_path_buf();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
    let r = w.hook_remove(&wt);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    r.assert_fails();
    assert!(r.err().contains("nothing was removed"), "{}", r.err());
    assert!(!w.hook_log().contains("pre_remove"), "{}", w.hook_log());
    assert!(wt.join(".git").is_file());
}

#[test]
fn check_exits_0_when_its_stdout_is_closed_during_a_background_install() {
    let w = World::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(
        w.main().join(".git/fast-worktree-installing"),
        format!("{now} {}\n", std::process::id()),
    )
    .unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let mut c = Command::new(env!("CARGO_BIN_EXE_fast-worktree"));
    w.env(&mut c);
    let out = c
        .arg("check")
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
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn remove_stops_the_background_install_first() {
    let w = World::new();
    let main = w.main();
    let r = w.hook_create(
        "race",
        &[("FAKE_NPM_SLEEP", "3"), ("FAKE_NPM_ABS_WRITE", "1")],
    );
    r.assert_ok();
    let path = PathBuf::from(last_line(&r.out()));
    wait_until("npm ci to start", || w.count("ci ") == 2);
    let r = w.fw(&main, &["remove", &path.to_string_lossy()]);
    r.assert_ok();
    assert!(
        r.err().contains("stopped the background install"),
        "{}",
        r.err()
    );
    assert!(!path.exists());
    std::thread::sleep(std::time::Duration::from_secs(4));
    assert!(!path.exists(), "the background install brought it back");
    let r = w.hook_create("race", &[]);
    r.assert_ok();
    assert_eq!(last_line(&r.out()), path.display().to_string());
}

/// A worktree whose `<admin>/gitdir` is relative to the admin directory,
/// as git 2.48 and later write it with `worktree.useRelativePaths`.
#[test]
fn remove_finds_the_entry_of_a_worktree_with_relative_paths() {
    let w = World::new();
    let main = w.main();
    w.git(
        &main,
        &[
            "-c",
            "worktree.useRelativePaths=true",
            "worktree",
            "add",
            "-q",
            "../rel",
            "-b",
            "rel",
        ],
    );
    let rel = w.base.join("rel");
    let gitdir = main.join(".git/worktrees/rel/gitdir");
    let text = fs::read_to_string(&gitdir).unwrap();
    if Path::new(text.trim()).is_absolute() {
        // A git that ignores the setting: write what a newer one would.
        fs::write(&gitdir, "../../../../rel/.git\n").unwrap();
    }
    let r = w.fw(&main, &["remove", &rel.to_string_lossy()]);
    r.assert_ok();
    assert!(!rel.exists());
    assert!(!main.join(".git/worktrees/rel").exists());
}

#[test]
fn install_leaves_the_node_modules_of_another_package_manager_alone() {
    let w = World::new();
    let main = w.main();
    fs::create_dir_all(main.join("app/node_modules")).unwrap();
    fs::write(main.join("app/node_modules/.yarn-integrity"), "{}").unwrap();
    fs::create_dir_all(main.join("lib/node_modules")).unwrap();
    fs::write(main.join("lib/node_modules/.modules.yaml"), "x: 1\n").unwrap();
    for args in [&["install"][..], &["install", "--changed"][..]] {
        let r = w.fw(&main, args);
        r.assert_ok();
        assert!(
            r.out()
                .contains("note   app: node_modules was installed by yarn"),
            "{}",
            r.out()
        );
        assert!(
            r.out()
                .contains("note   lib: node_modules was installed by pnpm"),
            "{}",
            r.out()
        );
        assert_eq!(w.count("ci "), 0, "{}", w.npm_log());
        assert!(main.join("app/node_modules/.yarn-integrity").is_file());
        assert!(main.join("lib/node_modules/.modules.yaml").is_file());
    }
}

/// A submodule whose git directory is inside it (cloned and added, or
/// left so by an earlier `hook remove`): the worktree is kept, and the
/// submodule's HEAD is kept in a ref of the main repository.
#[test]
fn hook_remove_keeps_a_submodule_with_its_git_directory_inside() {
    let w = World::new();
    let main = w.main();
    let sub_up = w.base.join("subup");
    fs::create_dir_all(&sub_up).unwrap();
    w.git(&sub_up, &["init", "-q", "-b", "trunk"]);
    w.commit(&sub_up, &[("s.txt", "s")], "s");
    let wt = w.hook_worktree("emb");
    w.git(&wt, &["clone", "-q", &sub_up.to_string_lossy(), "sub"]);
    w.commit(&wt.join("sub"), &[("mine.txt", "m")], "unpushed");
    let unpushed = w.git(&wt.join("sub"), &["rev-parse", "HEAD"]);
    let unpushed = unpushed.trim();
    w.git(&wt, &["add", "sub"]);
    w.git(&wt, &["commit", "-qm", "embedded"]);
    assert!(wt.join("sub/.git").is_dir());
    assert_eq!(w.git(&wt, &["status", "--porcelain"]), "");

    let r = w.hook_remove(&wt);
    r.assert_ok();
    assert!(!wt.exists());
    let kept = w.kept();
    assert_eq!(kept.len(), 1, "{}", r.err());
    assert!(kept[0].join("sub/.git").is_dir());
    let refs = w.git(
        &main,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/fast-worktree/",
        ],
    );
    let sub_ref = refs
        .lines()
        .find(|l| l.contains("/sub/sub "))
        .unwrap_or_else(|| panic!("no submodule ref in:\n{refs}\n{}", r.err()));
    assert!(
        sub_ref.starts_with("refs/fast-worktree/kept/emb-"),
        "{refs}"
    );
    assert!(sub_ref.ends_with(unpushed), "{refs}");
    assert!(r.err().contains("/sub/sub"), "{}", r.err());
    // The commit is in the main repository now, and gc keeps it.
    fs::remove_dir_all(&sub_up).unwrap();
    w.git(&main, &["reflog", "expire", "--expire=now", "--all"]);
    w.git(&main, &["gc", "-q", "--prune=now"]);
    w.git(&main, &["cat-file", "-e", unpushed]);
}

#[test]
fn hook_remove_keeps_no_ref_for_a_head_on_a_branch_that_stays() {
    let w = World::new();
    let main = w.main();
    let wt = w.hook_worktree("mine");
    w.commit(&wt, &[("new.txt", "x")], "own commit");
    fs::write(wt.join("notes.txt"), "wip\n").unwrap();
    let r = w.hook_remove(&wt);
    r.assert_ok();
    assert!(r.err().contains("kept branch worktree-mine"), "{}", r.err());
    assert!(!r.err().contains("refs/fast-worktree/"), "{}", r.err());
    assert_eq!(w.git(&main, &["for-each-ref", "refs/fast-worktree/"]), "");
    let restore = after(&r.err(), "to restore them: ").to_string();
    assert!(restore.contains(" worktree-mine && rsync"), "{restore}");
}

#[test]
fn hook_remove_keeps_the_detached_head_of_a_worktree_inside() {
    let w = World::new();
    let main = w.main();
    fs::write(main.join(".git/info/exclude"), ".claude/\n").unwrap();
    let outer = w.hook_worktree("outer");
    w.git(
        &outer,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            ".claude/worktrees/inner",
        ],
    );
    let inner = outer.join(".claude/worktrees/inner");
    w.commit(&inner, &[("lonely.txt", "x")], "lonely");
    let sha = w.git(&inner, &["rev-parse", "HEAD"]);
    let r = w.hook_remove(&outer);
    r.assert_ok();
    let refs = w.git(
        &main,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/fast-worktree/",
        ],
    );
    let line = refs
        .lines()
        .find(|l| l.starts_with("refs/fast-worktree/kept/inner-"))
        .unwrap_or_else(|| panic!("no ref for the inner worktree:\n{refs}\n{}", r.err()));
    assert!(line.ends_with(sha.trim()), "{refs}");
}

#[test]
fn kept_ls_and_prune() {
    let w = World::new();
    let main = w.main();
    let head = w.git(&main, &["rev-parse", "HEAD"]);
    let head = head.trim();
    let now = w.git(
        &main,
        &[
            "log",
            "-1",
            "--format=%cd",
            "--date=format-local:%Y%m%d-%H%M%S",
        ],
    );
    let fresh = format!("refs/fast-worktree/kept/new-1-{}/head", now.trim());
    for r in [
        "refs/fast-worktree/kept/old-20200101-000000/head",
        "refs/fast-worktree/kept/old-20200101-000000/sub/a/b",
        "refs/fast-worktree/kept/flat-20200102-030405-2",
        fresh.as_str(),
    ] {
        w.git(&main, &["update-ref", r, head]);
    }
    let r = w.fw(&main, &["kept", "ls"]);
    r.assert_ok();
    for want in [
        "old-20200101-000000/sub/a/b",
        "flat-20200102-030405-2",
        "new-1-",
        "2020-01-02",
    ] {
        assert!(r.out().contains(want), "{want}: {}", r.out());
    }
    let r = w.fw(&main, &["kept", "prune"]);
    r.assert_ok();
    assert_eq!(
        w.git(
            &main,
            &["for-each-ref", "--format=%(refname)", "refs/fast-worktree/"]
        ),
        format!("{fresh}\n"),
        "{}",
        r.out()
    );
    assert!(r.out().contains("pruned 3 refs"), "{}", r.out());
    w.fw(&main, &["kept", "prune", "--older-than", "0"])
        .assert_ok();
    assert_eq!(w.git(&main, &["for-each-ref", "refs/fast-worktree/"]), "");
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
}

#[test]
fn hook_remove_puts_the_submodules_back_when_one_cannot_be_moved() {
    if is_root() {
        return; // a read-only directory does not stop root
    }
    let w = World::new();
    let main = w.main();
    let sub_up = w.base.join("subup");
    fs::create_dir_all(&sub_up).unwrap();
    w.git(&sub_up, &["init", "-q", "-b", "trunk"]);
    w.commit(&sub_up, &[("s.txt", "s")], "s");
    let wt = w.hook_worktree("two");
    for name in ["sub", "sub2"] {
        w.git(
            &wt,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &sub_up.to_string_lossy(),
                name,
            ],
        );
    }
    w.git(&wt, &["commit", "-qm", "two subs"]);
    let admin = main.join(".git/worktrees/two");
    // The second one cannot be moved: its `.git` file cannot be replaced.
    let ro = |mode| fs::set_permissions(wt.join("sub2"), fs::Permissions::from_mode(mode));
    ro(0o555).unwrap();
    let r = w.hook_remove(&wt);
    ro(0o755).unwrap();
    r.assert_fails();
    assert!(r.err().contains("still registered"), "{}", r.err());
    for name in ["sub", "sub2"] {
        assert!(wt.join(name).join(".git").is_file(), "{name}: {}", r.err());
        assert!(
            admin.join("modules").join(name).join("HEAD").is_file(),
            "{name}"
        );
        assert_eq!(w.git(&wt.join(name), &["status", "--porcelain"]), "");
    }
    // Once it can be, it is.
    w.hook_remove(&wt).assert_ok();
    assert!(!wt.exists());
}

#[test]
fn the_projects_own_lifecycle_runs_one_at_a_time() {
    let w = World::new();
    let busy = w.base.join("busy");
    // Both projects install at once; their `npm ci` (which runs their own
    // lifecycle) must not.
    let r = w.fw_with(
        &w.main(),
        &["add", "../wt", "-b", "feat"],
        &[("FAKE_NPM_EXCLUSIVE", busy.to_str().unwrap())],
    );
    r.assert_ok();
    assert_eq!(w.count("ci "), 2, "{}", w.npm_log());
}

#[test]
fn a_lifecycle_script_that_finds_the_git_config_locked_is_run_again() {
    let w = World::new();
    let main = w.main();
    w.pool_size(1);
    w.fw(&main, &["pool", "fill"]).assert_ok();
    let _lock = hold_fill_lock(&w.pool_dir());
    let once = w.base.join("locked-once");
    let r = w.fw_with(
        &main,
        &["add", "../wt", "-b", "feat"],
        &[("FAKE_NPM_RUN_LOCKED_ONCE", once.to_str().unwrap())],
    );
    r.assert_ok();
    assert!(once.exists(), "{}", w.npm_log());
    assert!(r.out().contains("rerun  app: postinstall"), "{}", r.all());
    let app = w.base.join("wt/app");
    let runs = w
        .npm_log()
        .lines()
        .filter(|l| *l == format!("run postinstall {}", app.display()))
        .count();
    assert_eq!(runs, 2, "{}", w.npm_log());
}

/// A `post_add` hook that waits a little, then copies the pool's fill log
/// to `$SNAP_DIR/<worktree name>`: what a refill wrote while `add` ran.
const SNAPSHOT_FILL_LOG: &str = r#"
[[post_add]]
run = 'sleep 1; find "$FAST_WORKTREE_CACHE" -name fill.log -exec cat {} + > "$SNAP_DIR/$(basename "$FAST_WORKTREE_PATH")" 2>/dev/null; true'
"#;

#[test]
fn the_refill_starts_when_the_add_is_done() {
    let w = World::new();
    let main = w.main();
    w.commit(
        &w.base.join("upstream"),
        &[(".fast-worktree.toml", SNAPSHOT_FILL_LOG)],
        "hooks",
    );
    w.git(&main, &["pull", "-q"]);
    let snaps = w.base.join("snaps");
    fs::create_dir_all(&snaps).unwrap();
    let env = [("SNAP_DIR", snaps.to_str().unwrap())];
    w.pool_size(1);
    let fill_log = || fs::read_to_string(w.pool_dir().join("fill.log")).unwrap_or_default();

    // No spare ready: the refill still starts, after the add.
    let r = w.fw_with(&main, &["add", "../miss", "-b", "miss"], &env);
    r.assert_ok();
    assert!(r.out().contains("no spare ready"), "{}", r.out());
    assert_eq!(fs::read_to_string(snaps.join("miss")).unwrap(), "");
    wait_until("the refill", || {
        fill_log().contains("fill  done: 1 of 1 ready")
    });

    // A spare taken.
    let before = fill_log();
    let r = w.fw_with(&main, &["add", "../hit", "-b", "hit"], &env);
    r.assert_ok();
    assert!(r.out().contains("worktree from the pool"), "{}", r.out());
    assert_eq!(fs::read_to_string(snaps.join("hit")).unwrap(), before);
    wait_until("the second refill", || {
        fill_log().matches("fill  done: 1 of 1 ready").count() == 2
    });
}

#[test]
fn the_refill_waits_for_the_background_install_of_hook_create() {
    let w = World::new();
    w.pool_size(1);
    let r = w.hook_create("bg", &[("FAKE_NPM_SLEEP", "2")]);
    r.assert_ok();
    assert!(r.err().contains("later"), "{}", r.err());
    let path = PathBuf::from(last_line(&r.out()));
    let marker = w.main().join(".git/worktrees/bg/fast-worktree-installing");
    let fill_log = || fs::read_to_string(w.pool_dir().join("fill.log")).unwrap_or_default();
    wait_until("the refill to wait", || {
        fill_log().contains("waiting for the background install")
    });
    assert!(marker.exists());
    assert!(!fill_log().contains("fill  started"), "{}", fill_log());
    w.fw(&path, &["wait"]).assert_ok();
    wait_until("the refill", || {
        fill_log().contains("fill  done: 1 of 1 ready")
    });
}

#[test]
fn a_spare_is_made_without_the_repositorys_hooks_which_run_where_it_is_taken() {
    let w = World::new();
    let main = w.main();
    // A hook with a side effect, such as reserving something for the
    // worktree it runs in: here it records where it ran.
    write_exe(
        &w.base.join("githooks/post-checkout"),
        "#!/bin/sh\n[ -z \"$CHECKOUT_LOG\" ] || pwd -P >> \"$CHECKOUT_LOG\"\n",
    );
    w.git(
        &main,
        &[
            "config",
            "core.hooksPath",
            w.base.join("githooks").to_str().unwrap(),
        ],
    );
    let log = w.base.join("checkout.log");
    let env = [("CHECKOUT_LOG", log.to_str().unwrap())];
    w.pool_size(1);
    w.fw_with(&main, &["pool", "fill"], &env).assert_ok();
    assert_eq!(fs::read_to_string(&log).unwrap_or_default(), "");
    let _lock = hold_fill_lock(&w.pool_dir());
    let r = w.fw_with(&main, &["add", "../wt", "-b", "feat"], &env);
    r.assert_ok();
    assert!(r.out().contains("worktree from the pool"), "{}", r.out());
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("{}\n", w.base.join("wt").display())
    );
}

/// Whether `git --version` output is at least `major.minor`.
fn git_at_least(version: &str, major: u32, minor: u32) -> bool {
    let mut nums = version
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .split('.')
        .map(|n| n.parse::<u32>().unwrap_or(0));
    let v = (nums.next().unwrap_or(0), nums.next().unwrap_or(0));
    v >= (major, minor)
}

#[test]
fn git_versions_compare() {
    assert!(git_at_least("git version 2.56.0\n", 2, 48));
    assert!(git_at_least("git version 2.48.1 (Apple Git-1)", 2, 48));
    assert!(!git_at_least("git version 2.45.0", 2, 48));
    assert!(!git_at_least("git version 2.39.5 (Apple Git-154)", 2, 48));
}

#[test]
fn everything_works_with_relative_worktree_paths() {
    let w = World::new();
    let main = w.main();
    let version = w.git(&main, &["--version"]);
    if !git_at_least(&version, 2, 48) {
        eprintln!(
            "skipped: {} has no worktree.useRelativePaths",
            version.trim()
        );
        return;
    }
    w.git(&main, &["config", "worktree.useRelativePaths", "true"]);
    let relative = |wt: &Path| {
        let dot = fs::read_to_string(wt.join(".git")).unwrap();
        let gitdir = dot.trim().strip_prefix("gitdir: ").unwrap();
        assert!(Path::new(gitdir).is_relative(), "{}: {dot}", wt.display());
        let back = fs::read_to_string(wt.join(gitdir).join("gitdir")).unwrap();
        assert!(
            Path::new(back.trim()).is_relative(),
            "{}: {back}",
            wt.display()
        );
    };

    w.fw(&main, &["add", "../plain", "-b", "plain"]).assert_ok();
    let plain = w.base.join("plain");
    relative(&plain);
    w.fw(&main, &["remove", plain.to_str().unwrap()])
        .assert_ok();
    assert!(!plain.exists());

    let r = w.hook_create("h", &[]);
    r.assert_ok();
    let h = PathBuf::from(last_line(&r.out()));
    relative(&h);
    w.fw(&h, &["wait"]).assert_ok();
    w.hook_remove(&h).assert_ok();
    assert!(!h.exists());

    w.pool_size(2);
    w.fw(&main, &["pool", "fill"]).assert_ok();
    for s in w.spares() {
        relative(&s);
    }
    let _lock = hold_fill_lock(&w.pool_dir());
    let r = w.fw(&main, &["add", "../pooled", "-b", "pooled"]);
    r.assert_ok();
    assert!(r.out().contains("worktree from the pool"), "{}", r.out());
    let pooled = w.base.join("pooled");
    relative(&pooled);
    status_is_clean(&w, &pooled);
    w.fw(&main, &["remove", pooled.to_str().unwrap()])
        .assert_ok();
    let r = w.fw(&main, &["pool", "clear"]);
    r.assert_ok();
    assert!(r.out().contains("removed 1 spare(s)"), "{}", r.out());
    let list = w.git(&main, &["worktree", "list", "--porcelain"]);
    assert_eq!(list.matches("worktree ").count(), 1, "{list}");
    assert_eq!(w.git(&main, &["worktree", "prune", "-n", "-v"]), "");
}
