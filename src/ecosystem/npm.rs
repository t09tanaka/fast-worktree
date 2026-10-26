//! npm: one project per tracked `package-lock.json`.
//!
//! `node_modules` is made present by the first of these that applies:
//!
//! 1. **Cache.** `<cache>/<repo>/<project>/<key>` holds a `node_modules`
//!    from an earlier install with the same key. Cloned in one
//!    `clonefile(2)` call.
//! 2. **Another worktree** of the same repository (the main checkout
//!    included) whose lockfile is byte-identical, whose installed tree
//!    matches that lockfile ([`tree_match`]), and whose `node -v` is the
//!    same. A checkout that pulled a new lockfile but did not reinstall
//!    fails the tree check and is not used.
//! 3. **Delta.** The project's most recently used cache entry for the same
//!    Node version (an older lockfile) is cloned and `npm install
//!    --prefer-offline --no-audit --no-fund` brings it up to date. The
//!    result is used only if `package-lock.json` is byte-identical
//!    afterwards and the installed tree matches it; otherwise it is thrown
//!    away and step 4 runs.
//! 4. `npm ci --prefer-offline --no-audit --no-fund`.
//!
//! What steps 2 to 4 produce is cloned into the cache.
//!
//! Only a tree whose `node_modules/.package-lock.json` matches the lockfile
//! goes into the cache (for a `lockfileVersion` 1 lockfile, which has no
//! `packages` to compare, a tree with a `.package-lock.json`). A
//! `node_modules` without `.package-lock.json` (an install that did not
//! finish) does not count as installed: it is moved to the trash and
//! installed again, unless another package manager made it ([`foreign`]):
//! then it is left alone with a note, and nothing is installed.
//!
//! The key is the first 16 hex digits of the lockfile's sha256 plus the
//! `node -v` that the project directory resolves (version managers pick
//! it per directory). The Node version is part of the key and of the
//! worktree test because native addons are built for one Node ABI. It is
//! also written to `node_modules/.fast-worktree-key`, so `install
//! --changed` can tell an outdated tree.
//!
//! After a clone, `node_modules/.cache` (tool caches that embed absolute
//! paths) is deleted. A tree with a symlink whose target is absolute, which
//! could point into the source worktree, is neither cached nor cloned from
//! another worktree; `npm ci` runs instead. Cache entries are checked once,
//! when stored ([`cache`]). Dependencies' own install scripts wrote their
//! output inside `node_modules`, so the clone carries it; the project's own
//! `preinstall` / `install` / `postinstall` / `prepare` (git hooks setup,
//! code generation) may write outside it, so the ones present in its
//! `package.json` are run again in the clone, except where a
//! `skip_lifecycle` rule says their inputs are unchanged. If one fails,
//! the clone is thrown away so that the next `install` does not skip a
//! broken tree.
//!
//! Only macOS can clone directories; elsewhere steps 1 to 3 are skipped.

pub mod tree_match;

use super::{Ctx, Ecosystem, Mode, Report};
use crate::hooks::SkipLifecycle;
use crate::paths::{hex, project_slug};
use crate::{cache, clone, trash, util};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LOCKFILE: &str = "package-lock.json";
const HIDDEN_LOCKFILE: &str = "node_modules/.package-lock.json";
const KEY_FILE: &str = "node_modules/.fast-worktree-key";
const OWN_LIFECYCLE: [&str; 4] = ["preinstall", "install", "postinstall", "prepare"];
const NPM_FLAGS: [&str; 3] = ["--prefer-offline", "--no-audit", "--no-fund"];

pub struct Npm;

impl Ecosystem for Npm {
    fn name(&self) -> &'static str {
        "npm"
    }

    fn manifest(&self) -> &'static str {
        LOCKFILE
    }

    fn is_installed(&self, project: &Path) -> bool {
        project.join(HIDDEN_LOCKFILE).is_file()
            // npm creates no node_modules at all for a lockfile without
            // dependencies; there is nothing to install.
            || fs::read(project.join(LOCKFILE)).is_ok_and(|l| !has_dependencies(&l))
    }

    fn install(&self, ctx: &Ctx, dir: &str, mode: Mode) -> Result<Report, String> {
        install(ctx, dir, mode)
    }

    fn rerun_own_scripts(
        &self,
        root: &Path,
        dir: &str,
        rules: &[SkipLifecycle],
    ) -> Result<Vec<String>, String> {
        let project = super::project_path(root, dir);
        if !project.join("node_modules").is_dir() {
            return Ok(Vec::new());
        }
        match run_own_lifecycle(&project, dir, rules) {
            Ok((mut notes, ran)) => {
                if !ran.is_empty() {
                    notes.push(format!("rerun  {dir}: {}", ran.join(", ")));
                }
                Ok(notes)
            }
            Err(e) => {
                discard(&project.join("node_modules"));
                Err(e)
            }
        }
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
        installed_tree_matches(project, self.lockfile)
    }
    fn node_version(&self, project: &Path) -> Option<String> {
        node_version(project).ok()
    }
}

/// Whether `project/node_modules` is the tree `lockfile` asks for.
fn installed_tree_matches(project: &Path, lockfile: &[u8]) -> bool {
    fs::read(project.join(HIDDEN_LOCKFILE))
        .is_ok_and(|hidden| tree_match::tree_matches(lockfile, &hidden).is_ok())
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

/// Where a clone came from.
enum Source {
    Cache(PathBuf),
    Worktree(PathBuf),
}

fn install(ctx: &Ctx, dir: &str, mode: Mode) -> Result<Report, String> {
    let start = Instant::now();
    let target = super::project_path(&ctx.root, dir);
    let node_modules = target.join("node_modules");
    let mut lines = Vec::new();
    let mut present = node_modules.symlink_metadata().is_ok();
    if present {
        if let Some(by) = foreign(&target) {
            return Ok(Report::kept(format!(
                "note   {dir}: node_modules was installed by {by}; left as it is"
            )));
        }
    }
    if present && !mode.reinstalls_changed() {
        if Npm.is_installed(&target) {
            return Ok(Report::kept(format!(
                "skip   {dir} (node_modules already present)"
            )));
        }
        // Left by an install that did not finish, or made by hand: npm
        // writes `.package-lock.json` last.
        let note = format!("note   {dir}: node_modules has no .package-lock.json");
        if mode.clone_only() {
            return Ok(Report::missing(note));
        }
        trash::trash(&node_modules).map_err(|e| format!("cannot move node_modules away: {e}"))?;
        lines.push(format!("{note}, installing again"));
        present = false;
    }
    let lockfile = fs::read(target.join(LOCKFILE)).map_err(|e| format!("{LOCKFILE}: {e}"))?;
    if !has_dependencies(&lockfile) {
        return Ok(Report::kept(format!("skip   {dir} (no dependencies)")));
    }
    if mode.clone_only() && !clone::SUPPORTED && !present {
        return Ok(Report::missing(""));
    }
    let nodev = node_version(&target)?;
    let key = cache_key(&lockfile, &nodev);
    if present {
        if is_current(&target, &lockfile, &key) {
            write_key(&target, &key);
            return Ok(Report::kept(format!("skip   {dir} (up to date)")));
        }
        trash::trash(&node_modules).map_err(|e| format!("cannot move node_modules away: {e}"))?;
        lines.push(format!(
            "note   {dir}: lockfile or Node version changed, installing again"
        ));
        if mode.clone_only() && !clone::SUPPORTED {
            return Ok(Report::missing(lines.join("\n")));
        }
    }
    let project_cache = ctx.cache_repo.join(project_slug(dir));
    let entry = project_cache.join(&key);

    if clone::SUPPORTED {
        let src = if entry.is_dir() {
            cache::touch(&entry);
            Some(Source::Cache(entry.clone()))
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
            .map(Source::Worktree)
        };
        if let Some(src) = src {
            let (path, checked) = match &src {
                Source::Cache(p) => (p, cache::is_checked(p)),
                Source::Worktree(p) => (p, false),
            };
            match clone_from(path, &node_modules, checked) {
                Ok(()) => {
                    match &src {
                        // An entry from before markers existed, now checked.
                        Source::Cache(p) if !checked => cache::mark_checked(p),
                        Source::Cache(_) => {}
                        // Stored before the project's own scripts touch it:
                        // this is the donor's tree, just scanned.
                        Source::Worktree(_) => {
                            save(&node_modules, &entry, true, &lockfile, dir, &mut lines);
                        }
                    }
                    lines.extend(after_clone(ctx, &target, dir, &key)?);
                    lines.push(format!(
                        "clone  {dir} from {} ({})",
                        util::tilde(path),
                        util::elapsed(start)
                    ));
                    return Ok(Report::done(lines.join("\n")));
                }
                Err(why) => lines.push(format!("note   {dir}: {why}, not cloned")),
            }
        }
    }

    if mode.clone_only() {
        return Ok(Report::missing(lines.join("\n")));
    }

    if clone::SUPPORTED {
        if let Some(base) = cache::latest(&project_cache, &nodev, &entry) {
            cache::touch(&base);
            match delta(&base, &target, &lockfile) {
                Ok(()) => {
                    write_key(&target, &key);
                    save(&node_modules, &entry, false, &lockfile, dir, &mut lines);
                    cache::prune(
                        &project_cache,
                        Duration::from_secs(cache::TTL_DAYS * 86_400),
                    );
                    lines.push(format!(
                        "delta  {dir} from {} ({})",
                        util::tilde(&base),
                        util::elapsed(start)
                    ));
                    return Ok(Report::done(lines.join("\n")));
                }
                Err(why) => lines.push(format!("note   {dir}: {why}, falling back to npm ci")),
            }
        }
    }

    if let Err(log) = npm_ci(&target) {
        discard(&node_modules);
        return Err(log);
    }
    write_key(&target, &key);
    if clone::SUPPORTED && node_modules.is_dir() {
        save(&node_modules, &entry, false, &lockfile, dir, &mut lines);
        cache::prune(
            &project_cache,
            Duration::from_secs(cache::TTL_DAYS * 86_400),
        );
    }
    lines.push(format!("npm ci {dir} ({})", util::elapsed(start)));
    Ok(Report::done(lines.join("\n")))
}

/// The package manager other than npm that made `project/node_modules`,
/// from what it leaves there: such a tree has no `.package-lock.json`, and
/// moving it away and running `npm ci` would replace it with a different
/// one.
fn foreign(project: &Path) -> Option<&'static str> {
    let nm = project.join("node_modules");
    let marks = [
        (".yarn-integrity", "yarn"),
        (".yarn-state.yml", "yarn"),
        (".modules.yaml", "pnpm"),
        (".bun", "bun"),
    ];
    if let Some((_, by)) = marks
        .iter()
        .find(|(m, _)| nm.join(m).symlink_metadata().is_ok())
    {
        return Some(by);
    }
    // bun's default (hoisted) layout leaves nothing of its own in
    // node_modules; its lockfile beside a tree npm did not record is the
    // sign.
    let bun_lock = ["bun.lock", "bun.lockb"]
        .iter()
        .any(|l| project.join(l).is_file());
    (bun_lock && !project.join(HIDDEN_LOCKFILE).exists()).then_some("bun")
}

/// Whether the lockfile has a `packages` map (`lockfileVersion` 2 and 3),
/// the part [`tree_match`] compares. Version 1 has only `dependencies`.
fn has_packages(lockfile: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(lockfile)
        .is_ok_and(|v| v.get("packages").is_some_and(|p| p.is_object()))
}

/// Whether the installed tree is the one this lockfile and Node version
/// ask for. Trees installed by fast-worktree carry their key: a different
/// Node version means installing again, a different lockfile only if the
/// tree does not match it (someone ran `npm install` there). For trees
/// without a key, only the lockfile can be compared.
fn is_current(project: &Path, lockfile: &[u8], key: &str) -> bool {
    let node = |k: &str| k.split_once('-').map(|(_, n)| n.to_string());
    match fs::read_to_string(project.join(KEY_FILE)) {
        Ok(k) if k.trim() == key => true,
        Ok(k) if node(k.trim()) != node(key) => false,
        _ => installed_tree_matches(project, lockfile),
    }
}

fn write_key(project: &Path, key: &str) {
    let _ = fs::write(project.join(KEY_FILE), format!("{key}\n"));
}

/// Store in the cache, noting a failure among `lines`. Only a tree whose
/// `.package-lock.json` matches `lockfile` is stored: anything else (an
/// install that wrote part of the tree) would be cloned into every later
/// worktree, and marked as checked. A version 1 lockfile has no `packages`
/// to compare with (npm records the tree in the newer format): there, a
/// `.package-lock.json` is enough.
fn save(
    node_modules: &Path,
    entry: &Path,
    checked: bool,
    lockfile: &[u8],
    dir: &str,
    lines: &mut Vec<String>,
) {
    let stored = match fs::read(node_modules.join(".package-lock.json")) {
        Err(_) => Err("node_modules has no .package-lock.json".to_string()),
        Ok(hidden)
            if has_packages(lockfile) && tree_match::tree_matches(lockfile, &hidden).is_err() =>
        {
            Err("node_modules does not match package-lock.json".to_string())
        }
        Ok(_) => cache::store(node_modules, entry, checked),
    };
    if let Err(e) = stored {
        lines.push(format!("note   {dir}: cache not saved: {e}"));
    }
}

/// Move a half-made `node_modules` away, so that the next `install` does
/// not skip it as present.
fn discard(node_modules: &Path) {
    if node_modules.symlink_metadata().is_ok() {
        let _ = trash::trash(node_modules);
    }
}

/// The project's own lifecycle, then the key. A failure discards the clone.
fn after_clone(ctx: &Ctx, target: &Path, dir: &str, key: &str) -> Result<Vec<String>, String> {
    match run_own_lifecycle(target, dir, &ctx.skip_lifecycle) {
        Ok((notes, _)) => {
            write_key(target, key);
            Ok(notes)
        }
        Err(e) => {
            discard(&target.join("node_modules"));
            Err(e)
        }
    }
}

/// Clone `src` to `node_modules` and make it safe to use there. `checked`
/// skips the symlink scan for a cache entry that was scanned when stored.
/// On error nothing is left at `node_modules`.
fn clone_from(src: &Path, node_modules: &Path, checked: bool) -> Result<(), String> {
    if let Err(e) = clone::clone_dir(src, node_modules) {
        discard(node_modules);
        return Err(format!("clone failed ({e})"));
    }
    let _ = fs::remove_dir_all(node_modules.join(".cache"));
    if !checked && clone::has_absolute_symlink(node_modules) {
        discard(node_modules);
        return Err("absolute symlink in node_modules".into());
    }
    Ok(())
}

/// Clone `base` (a cache entry for an older lockfile) into the project and
/// bring it up to date with `npm install`. On error the tree is discarded
/// and `package-lock.json` is as it was.
fn delta(base: &Path, project: &Path, lockfile: &[u8]) -> Result<(), String> {
    let node_modules = project.join("node_modules");
    clone_from(base, &node_modules, cache::is_checked(base))?;
    let out = Command::new("npm")
        .arg("install")
        .args(NPM_FLAGS)
        .current_dir(project)
        .stdin(Stdio::null())
        .output();
    let lock_path = project.join(LOCKFILE);
    let rewritten = fs::read(&lock_path).map_or(true, |now| now != lockfile);
    let why = match out {
        Err(e) => Some(format!("cannot run npm: {e}")),
        Ok(o) if !o.status.success() => Some("npm install failed".to_string()),
        Ok(_) if rewritten => Some("npm install changed package-lock.json".to_string()),
        Ok(_) if !installed_tree_matches(project, lockfile) => {
            Some("npm install left a tree that differs from package-lock.json".to_string())
        }
        Ok(_) => None,
    };
    let Some(why) = why else {
        return Ok(());
    };
    if rewritten {
        let _ = fs::write(&lock_path, lockfile);
    }
    discard(&node_modules);
    Err(why)
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

/// Run the project's own lifecycle scripts, except those a rule skips.
/// Returns a note per skipped script, and the scripts that ran.
fn run_own_lifecycle(
    project: &Path,
    dir: &str,
    rules: &[SkipLifecycle],
) -> Result<(Vec<String>, Vec<&'static str>), String> {
    let Ok(pkg) = fs::read(project.join("package.json")) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut notes = Vec::new();
    let mut ran = Vec::new();
    for script in own_lifecycle_scripts(&pkg) {
        if rules
            .iter()
            .any(|r| r.applies_to(dir, script) && r.inputs_unchanged(project))
        {
            notes.push(format!("note   {dir}: {script} skipped, inputs unchanged"));
            continue;
        }
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
        ran.push(script);
    }
    Ok((notes, ran))
}

/// `npm ci`. Its output is shown only when it fails.
fn npm_ci(project: &Path) -> Result<(), String> {
    let out = Command::new("npm")
        .arg("ci")
        .args(NPM_FLAGS)
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
    fn a_tree_is_current_by_its_key_else_by_the_lockfile() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        let lock = br#"{"packages":{"node_modules/a":{"version":"1"}}}"#;
        fs::create_dir(p.join("node_modules")).unwrap();
        assert!(!is_current(p, lock, "aaaa-v20"), "nothing recorded");
        fs::write(p.join(HIDDEN_LOCKFILE), lock).unwrap();
        assert!(is_current(p, lock, "aaaa-v20"), "no key: the tree decides");
        write_key(p, "aaaa-v20");
        assert!(is_current(p, lock, "aaaa-v20"));
        assert!(!is_current(p, lock, "aaaa-v22"), "another Node version");
        assert!(
            is_current(p, lock, "bbbb-v20"),
            "another lockfile, but the tree already matches it"
        );
        fs::write(p.join(HIDDEN_LOCKFILE), b"{\"packages\":{}}").unwrap();
        assert!(!is_current(p, lock, "bbbb-v20"));
    }

    #[test]
    fn a_tree_from_a_version_1_lockfile_is_saved_when_npm_recorded_it() {
        let t = tempfile::tempdir().unwrap();
        let nm = t.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        // npm 7+ records the tree in the new format, whatever the lockfile.
        fs::write(
            nm.join(".package-lock.json"),
            br#"{"lockfileVersion":3,"packages":{"node_modules/a":{"version":"1.0.0"}}}"#,
        )
        .unwrap();
        let v1 = br#"{"lockfileVersion":1,"dependencies":{"a":{"version":"1.0.0"}}}"#;
        let mut lines = Vec::new();
        save(&nm, &t.path().join("entry"), true, v1, "app", &mut lines);
        assert!(
            !lines.iter().any(|l| l.contains("package-lock.json")),
            "{lines:?}"
        );
        // Without the record it is still not saved.
        fs::remove_file(nm.join(".package-lock.json")).unwrap();
        let mut lines = Vec::new();
        save(&nm, &t.path().join("entry2"), true, v1, "app", &mut lines);
        assert!(
            lines.iter().any(|l| l.contains("no .package-lock.json")),
            "{lines:?}"
        );
    }

    #[test]
    fn skipped_lifecycle_scripts_are_not_run() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        fs::write(p.join("package.json"), r#"{"scripts":{"postinstall":"x"}}"#).unwrap();
        fs::write(p.join("in"), "same").unwrap();
        fs::write(p.join("out"), "same").unwrap();
        let rule = SkipLifecycle {
            dir: "app".into(),
            script: "postinstall".into(),
            if_equal: vec![["in".into(), "out".into()]],
        };
        // No npm is spawned: the only script is skipped.
        let (notes, ran) = run_own_lifecycle(p, "app", std::slice::from_ref(&rule)).unwrap();
        assert!(ran.is_empty());
        assert_eq!(
            notes,
            vec!["note   app: postinstall skipped, inputs unchanged"]
        );
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
}
