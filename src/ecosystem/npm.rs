//! npm: one project per tracked `package-lock.json`.
//!
//! `node_modules` is made present by the first of these that applies:
//!
//! 1. **Cache.** `<cache>/<repo>/<project>/<key>` holds a `node_modules`
//!    from an earlier `npm ci` with the same key. Cloned in one
//!    `clonefile(2)` call.
//! 2. **Another worktree** of the same repository (the main checkout
//!    included) whose lockfile is byte-identical, whose installed tree
//!    matches that lockfile ([`tree_match`]), and whose `node -v` is the
//!    same. A checkout that pulled a new lockfile but did not reinstall
//!    fails the tree check and is not used.
//! 3. `npm ci --prefer-offline --no-audit --no-fund`. The result is cloned
//!    into the cache.
//!
//! The key is the first 16 hex digits of the lockfile's sha256 plus the
//! `node -v` that the project directory resolves (version managers pick
//! it per directory). The Node version is part of the key and of the
//! worktree test because native addons are built for one Node ABI.
//!
//! After a clone, `node_modules/.cache` (tool caches that embed absolute
//! paths) is deleted. If the tree contains a symlink with an absolute
//! target, which could point into the source worktree, the clone is
//! dropped and `npm ci` runs instead. Dependencies' own install scripts
//! wrote their output inside `node_modules`, so the clone carries it; the
//! project's own `preinstall` / `install` / `postinstall` / `prepare`
//! (git hooks setup, code generation) may write outside it, so the ones
//! present in its `package.json` are run again in the clone.
//!
//! Cache entries unused for [`CACHE_TTL_DAYS`] days are removed. Clones
//! share blocks with their source, so an entry costs almost no disk while
//! a worktree made from it exists.
//!
//! Only macOS can clone directories; elsewhere steps 1 and 2 are skipped.

pub mod tree_match;

use super::{Ctx, Ecosystem};
use crate::paths::{hex, project_slug};
use crate::{clone, trash, util};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

pub const CACHE_TTL_DAYS: u64 = 7;
const LOCKFILE: &str = "package-lock.json";
const HIDDEN_LOCKFILE: &str = "node_modules/.package-lock.json";
const OWN_LIFECYCLE: [&str; 4] = ["preinstall", "install", "postinstall", "prepare"];

pub struct Npm;

impl Ecosystem for Npm {
    fn manifest(&self) -> &'static str {
        LOCKFILE
    }

    fn is_installed(&self, project: &Path) -> bool {
        project.join(HIDDEN_LOCKFILE).is_file()
            // npm creates no node_modules at all for a lockfile without
            // dependencies; there is nothing to install.
            || fs::read(project.join(LOCKFILE)).is_ok_and(|l| !has_dependencies(&l))
    }

    fn install(&self, ctx: &Ctx, dir: &str) -> Result<String, String> {
        install(ctx, dir)
    }
}

/// The cache key of a lockfile under a Node version.
pub fn cache_key(lockfile: &[u8], node_version: &str) -> String {
    let digest = hex(&Sha256::digest(lockfile));
    format!("{}-{}", &digest[..16], node_version.trim())
}

/// Whether the lockfile lists anything to install.
pub fn has_dependencies(lockfile: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(lockfile) else {
        return true; // let npm report it
    };
    let non_empty = |k: &str| {
        v.get(k)
            .and_then(|p| p.as_object())
            .is_some_and(|m| m.keys().any(|k| !k.is_empty()))
    };
    non_empty("packages") || non_empty("dependencies")
}

/// The checks a worktree must pass to lend its `node_modules`, each one
/// more expensive than the one before.
pub trait DonorProbe {
    fn is_dir(&self, project: &Path) -> bool;
    /// The candidate's lockfile is byte-identical to ours.
    fn same_lockfile(&self, project: &Path) -> bool;
    /// The candidate's installed tree matches its lockfile.
    fn tree_matches(&self, project: &Path) -> bool;
    fn node_version(&self, project: &Path) -> Option<String>;
}

/// The `node_modules` to clone from, if any worktree other than `root`
/// qualifies. Candidates are tried in order (the main checkout first) and
/// the checks run lazily, so `node -v` is only spawned for a candidate that
/// passed everything else.
pub fn find_donor(
    root: &Path,
    worktrees: &[PathBuf],
    dir: &str,
    node_version: &str,
    probe: &dyn DonorProbe,
) -> Option<PathBuf> {
    worktrees
        .iter()
        .filter(|w| w.as_path() != root)
        .map(|w| super::project_path(w, dir))
        .find(|p| {
            probe.is_dir(p)
                && probe.same_lockfile(p)
                && probe.tree_matches(p)
                && probe.node_version(p).as_deref() == Some(node_version)
        })
        .map(|p| p.join("node_modules"))
}

struct FsProbe<'a> {
    lockfile: &'a [u8],
}

impl DonorProbe for FsProbe<'_> {
    fn is_dir(&self, project: &Path) -> bool {
        project.is_dir()
    }
    fn same_lockfile(&self, project: &Path) -> bool {
        fs::read(project.join(LOCKFILE)).is_ok_and(|b| b == self.lockfile)
    }
    fn tree_matches(&self, project: &Path) -> bool {
        match fs::read(project.join(HIDDEN_LOCKFILE)) {
            Ok(hidden) => tree_match::tree_matches(self.lockfile, &hidden).is_ok(),
            Err(_) => false,
        }
    }
    fn node_version(&self, project: &Path) -> Option<String> {
        node_version(project).ok()
    }
}

fn node_version(project: &Path) -> Result<String, String> {
    let out = Command::new("node")
        .arg("-v")
        .current_dir(project)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run node: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "node -v failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn install(ctx: &Ctx, dir: &str) -> Result<String, String> {
    let start = Instant::now();
    let target = super::project_path(&ctx.root, dir);
    let node_modules = target.join("node_modules");
    if node_modules.symlink_metadata().is_ok() {
        return Ok(format!("skip   {dir} (node_modules already present)"));
    }
    let lockfile = fs::read(target.join(LOCKFILE)).map_err(|e| format!("{LOCKFILE}: {e}"))?;
    if !has_dependencies(&lockfile) {
        return Ok(format!("skip   {dir} (no dependencies)"));
    }
    let nodev = node_version(&target)?;
    let entry = ctx
        .cache_repo
        .join(project_slug(dir))
        .join(cache_key(&lockfile, &nodev));
    let mut lines = Vec::new();

    if clone::SUPPORTED {
        let from_cache = entry.is_dir();
        let src = if from_cache {
            touch(&entry);
            Some(entry.clone())
        } else {
            find_donor(
                &ctx.root,
                &ctx.worktrees,
                dir,
                &nodev,
                &FsProbe {
                    lockfile: &lockfile,
                },
            )
        };
        if let Some(src) = src {
            match clone_from(&src, &node_modules) {
                Ok(()) => {
                    run_own_lifecycle(&target)?;
                    if !from_cache {
                        if let Err(e) = cache_store(&node_modules, &entry) {
                            lines.push(format!("note   {dir}: cache not saved: {e}"));
                        }
                    }
                    lines.push(format!(
                        "clone  {dir} from {} ({})",
                        util::tilde(&src),
                        util::elapsed(start)
                    ));
                    return Ok(lines.join("\n"));
                }
                Err(why) => lines.push(format!("note   {dir}: {why}, falling back to npm ci")),
            }
        }
    }

    npm_ci(&target)?;
    if clone::SUPPORTED && node_modules.is_dir() {
        if let Err(e) = cache_store(&node_modules, &entry) {
            lines.push(format!("note   {dir}: cache not saved: {e}"));
        }
        if let Some(parent) = entry.parent() {
            cache_prune(parent, Duration::from_secs(CACHE_TTL_DAYS * 86_400));
        }
    }
    lines.push(format!("npm ci {dir} ({})", util::elapsed(start)));
    Ok(lines.join("\n"))
}

/// Clone `src` to `node_modules` and make it safe to use there. On error
/// nothing is left at `node_modules`.
fn clone_from(src: &Path, node_modules: &Path) -> Result<(), String> {
    if let Err(e) = clone::clone_dir(src, node_modules) {
        if node_modules.symlink_metadata().is_ok() {
            let _ = trash::trash(node_modules);
        }
        return Err(format!("clone failed ({e})"));
    }
    let _ = fs::remove_dir_all(node_modules.join(".cache"));
    if has_absolute_symlink(node_modules) {
        let _ = trash::trash(node_modules);
        return Err("absolute symlink in node_modules".into());
    }
    Ok(())
}

/// Whether any symlink under `dir` (not followed) has an absolute target.
pub fn has_absolute_symlink(dir: &Path) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                if fs::read_link(e.path()).is_ok_and(|t| t.is_absolute()) {
                    return true;
                }
            } else if ft.is_dir() {
                stack.push(e.path());
            }
        }
    }
    false
}

/// The project's own lifecycle scripts that exist in its `package.json`,
/// in npm's order. (`npm run` costs a few hundred milliseconds to start,
/// so absent ones are not run.)
pub fn own_lifecycle_scripts(package_json: &[u8]) -> Vec<&'static str> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(package_json) else {
        return Vec::new();
    };
    let scripts = v.get("scripts");
    OWN_LIFECYCLE
        .into_iter()
        .filter(|s| {
            scripts
                .and_then(|m| m.get(*s))
                .and_then(|x| x.as_str())
                .is_some_and(|x| !x.is_empty())
        })
        .collect()
}

fn run_own_lifecycle(project: &Path) -> Result<(), String> {
    let Ok(pkg) = fs::read(project.join("package.json")) else {
        return Ok(());
    };
    for script in own_lifecycle_scripts(&pkg) {
        let out = Command::new("npm")
            .args(["run", script])
            .current_dir(project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("cannot run npm: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "npm run {script} failed after clone:\n{}",
                util::tail(&String::from_utf8_lossy(&out.stderr), 30)
            ));
        }
    }
    Ok(())
}

/// `npm ci`. Its output is shown only when it fails.
fn npm_ci(project: &Path) -> Result<(), String> {
    let out = Command::new("npm")
        .args(["ci", "--prefer-offline", "--no-audit", "--no-fund"])
        .current_dir(project)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run npm: {e}"))?;
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

/// Clone `node_modules` into the cache at `entry`, unless it is there
/// already. Built under a temporary name and renamed, so a concurrent run
/// never sees a half-made entry.
fn cache_store(node_modules: &Path, entry: &Path) -> Result<(), String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if entry.exists() {
        return Ok(());
    }
    let parent = entry.parent().ok_or("no parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let name = entry.file_name().ok_or("no name")?.to_string_lossy();
    let tmp = parent.join(format!(
        "{name}.tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    clone::clone_dir(node_modules, &tmp).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(tmp.join(".cache"));
    touch(&tmp);
    if fs::rename(&tmp, entry).is_err() {
        let _ = trash::trash(&tmp);
    }
    Ok(())
}

/// Remove cache entries in `dir` not used for longer than `ttl`.
fn cache_prune(dir: &Path, ttl: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        if age > ttl {
            let _ = trash::trash(&e.path());
        }
    }
}

/// Set a directory's modification time to now: the cache's "last used".
fn touch(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: a valid NUL-terminated path; a null `times` means "now".
        unsafe {
            libc::utimes(c.as_ptr(), std::ptr::null());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[test]
    fn cache_key_is_lockfile_hash_and_node_version() {
        let k = cache_key(b"{}", "v20.11.1\n");
        // sha256("{}") = 44136fa355b3678a...
        assert_eq!(k, "44136fa355b3678a-v20.11.1");
        assert_ne!(k, cache_key(b"{}", "v22.0.0"));
        assert_ne!(k, cache_key(b"{ }", "v20.11.1"));
    }

    #[test]
    fn dependencies_are_detected_in_either_lockfile_format() {
        assert!(!has_dependencies(br#"{"packages":{"":{"name":"x"}}}"#));
        assert!(!has_dependencies(br#"{"lockfileVersion":3}"#));
        assert!(has_dependencies(
            br#"{"packages":{"":{},"node_modules/a":{}}}"#
        ));
        assert!(has_dependencies(br#"{"dependencies":{"a":{}}}"#));
        assert!(has_dependencies(b"not json"));
    }

    #[test]
    fn own_lifecycle_scripts_in_npm_order_and_only_if_present() {
        let pkg = br#"{"scripts":{"prepare":"husky","test":"x","preinstall":"a","install":""}}"#;
        assert_eq!(own_lifecycle_scripts(pkg), vec!["preinstall", "prepare"]);
        assert!(own_lifecycle_scripts(br#"{"name":"x"}"#).is_empty());
        assert!(own_lifecycle_scripts(b"nope").is_empty());
    }

    #[test]
    fn finds_absolute_symlinks_without_following_links() {
        let t = tempfile::tempdir().unwrap();
        let nm = t.path().join("node_modules");
        fs::create_dir_all(nm.join("a/b")).unwrap();
        std::os::unix::fs::symlink("../a", nm.join("rel")).unwrap();
        assert!(!has_absolute_symlink(&nm));
        std::os::unix::fs::symlink("/tmp", nm.join("a/b/abs")).unwrap();
        assert!(has_absolute_symlink(&nm));
    }

    /// A probe over a table of candidate facts that records what was asked.
    #[derive(Default)]
    struct Fake {
        facts: HashMap<PathBuf, (bool, bool, bool, Option<&'static str>)>,
        node_calls: RefCell<Vec<PathBuf>>,
    }

    impl DonorProbe for Fake {
        fn is_dir(&self, p: &Path) -> bool {
            self.facts.get(p).is_some_and(|f| f.0)
        }
        fn same_lockfile(&self, p: &Path) -> bool {
            self.facts[p].1
        }
        fn tree_matches(&self, p: &Path) -> bool {
            self.facts[p].2
        }
        fn node_version(&self, p: &Path) -> Option<String> {
            self.node_calls.borrow_mut().push(p.to_path_buf());
            self.facts[p].3.map(str::to_string)
        }
    }

    fn wt(n: &str) -> PathBuf {
        PathBuf::from("/r").join(n)
    }

    #[test]
    fn donor_must_pass_every_check() {
        let worktrees = vec![wt("main"), wt("a"), wt("b"), wt("c"), wt("d"), wt("me")];
        let mut f = Fake::default();
        f.facts
            .insert(wt("main/app"), (true, true, false, Some("v20"))); // stale tree
        f.facts
            .insert(wt("a/app"), (true, false, true, Some("v20"))); // other lockfile
        f.facts.insert(wt("b/app"), (true, true, true, Some("v22"))); // other node
        f.facts
            .insert(wt("c/app"), (false, true, true, Some("v20"))); // no project dir
        f.facts.insert(wt("d/app"), (true, true, true, Some("v20")));
        f.facts
            .insert(wt("me/app"), (true, true, true, Some("v20")));
        let got = find_donor(&wt("me"), &worktrees, "app", "v20", &f);
        assert_eq!(got, Some(wt("d/app/node_modules")));
        // `node -v` only for candidates that passed the cheaper checks.
        assert_eq!(*f.node_calls.borrow(), vec![wt("b/app"), wt("d/app")]);
    }

    #[test]
    fn donor_is_never_the_target_itself_and_prefers_the_first() {
        let worktrees = vec![wt("main"), wt("me"), wt("x")];
        let mut f = Fake::default();
        for w in ["main", "me", "x"] {
            f.facts.insert(wt(w), (true, true, true, Some("v20")));
        }
        assert_eq!(
            find_donor(&wt("me"), &worktrees, ".", "v20", &f),
            Some(wt("main/node_modules"))
        );
        let only_me = vec![wt("me")];
        assert_eq!(find_donor(&wt("me"), &only_me, ".", "v20", &f), None);
    }

    #[test]
    fn prune_removes_only_entries_older_than_the_ttl() {
        let t = tempfile::tempdir().unwrap();
        let cache = t.path().join("cache");
        std::env::set_var("FAST_WORKTREE_CACHE", &cache);
        let proj = cache.join("repo/app");
        fs::create_dir_all(proj.join("old")).unwrap();
        fs::create_dir_all(proj.join("new")).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        touch(&proj.join("new"));
        cache_prune(&proj, Duration::from_secs(1));
        assert!(!proj.join("old").exists());
        assert!(proj.join("new").exists());
    }
}
