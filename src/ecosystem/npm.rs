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
//! Install scripts often write the repository's shared `.git/config` (a
//! project's `prepare` running `husky`, a dependency's `postinstall`
//! installing git hooks), so every run of them holds the repository's
//! [`super::lifecycle_lock`] and they run one at a time, and a run that
//! failed because git found a config file locked is tried again. The
//! downloads do not need the lock: `npm ci` and the `npm install` of step 3
//! run with `--ignore-scripts`, for every project at once, and then, under
//! the lock, `npm rebuild <names>` runs the install scripts of the
//! dependencies that have one (all of them after `npm ci`, the new or
//! changed ones after `npm install`) and `npm run <script>` the project's
//! own `preinstall`, `install`, `postinstall` and `prepare`, in npm's
//! order. (A plain `npm rebuild` would run the project's own scripts in
//! another order and leave out `prepare`.) Until those scripts are done,
//! `node_modules/.fast-worktree-scripts-pending` marks the tree as not
//! installed. A lockfile with a git dependency or a link (workspaces),
//! whose scripts npm runs in ways `npm rebuild` does not repeat, is
//! installed by `npm ci` / `npm install` with its scripts, under the lock,
//! as is a project whose `npm rebuild` failed (an optional dependency whose
//! build fails, which `npm ci` tolerates). With npm's `ignore-scripts` set,
//! no script runs.
//!
//! Only macOS can clone directories; elsewhere steps 1 to 3 are skipped.

pub mod tree_match;

use super::{lifecycle_lock, output_retrying_config_lock, Ctx, Mode, Plan, Preset, Report, Target};
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
/// Present while the install scripts of an install without them are still
/// to run.
const SCRIPTS_PENDING: &str = "node_modules/.fast-worktree-scripts-pending";
const OWN_LIFECYCLE: [&str; 4] = ["preinstall", "install", "postinstall", "prepare"];
const NPM_FLAGS: [&str; 3] = ["--prefer-offline", "--no-audit", "--no-fund"];

pub struct Npm;

impl Preset for Npm {
    fn name(&self) -> &'static str {
        "npm"
    }

    fn manifest(&self) -> &'static str {
        LOCKFILE
    }

    fn managed(&self) -> &'static str {
        "node_modules"
    }

    fn default_key_commands(&self) -> &'static [&'static str] {
        &["node -v"]
    }

    fn is_installed(&self, root: &Path, t: &Target) -> bool {
        is_installed(&super::project_path(root, &t.dir))
    }

    fn install(&self, ctx: &Ctx, t: &Target, mode: Mode) -> Result<Report, String> {
        install(ctx, t, mode)
    }

    fn plan(&self, ctx: &Ctx, t: &Target, mode: Mode, fresh: bool) -> Plan {
        plan(ctx, t, mode, fresh)
    }

    fn rerun_own_scripts(
        &self,
        root: &Path,
        t: &Target,
        rules: &[SkipLifecycle],
        cache_repo: &Path,
    ) -> Result<Vec<String>, String> {
        let dir = t.dir.as_str();
        let project = super::project_path(root, dir);
        if !project.join("node_modules").is_dir() {
            return Ok(Vec::new());
        }
        let start = Instant::now();
        let mut waited = Duration::ZERO;
        match run_scripts(&project, dir, &Scripts::own(rules), cache_repo, &mut waited) {
            Ok((mut notes, ran)) => {
                if !ran.is_empty() {
                    let mut line = format!("rerun  {dir}: {}", ran.join(", "));
                    if waited >= Duration::from_millis(50) {
                        line.push_str(&format!(" ({})", super::timing(start, waited)));
                    }
                    notes.push(line);
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

/// Whether the project's dependencies look present: `node_modules` with
/// npm's record of it, and no install scripts still to run.
fn is_installed(project: &Path) -> bool {
    (project.join(HIDDEN_LOCKFILE).is_file() && !scripts_pending(project))
        // npm creates no node_modules at all for a lockfile without
        // dependencies; there is nothing to install.
        || fs::read(project.join(LOCKFILE)).is_ok_and(|l| !has_dependencies(&l))
}

/// The cache key of a lockfile under a Node version.
pub fn cache_key(lockfile: &[u8], node_version: &str) -> String {
    let digest = hex(&Sha256::digest(lockfile));
    format!("{}-{}", &digest[..16], node_version.trim())
}

/// The part of a key that the `key_commands` make: their outputs joined
/// with `-` (by default just `node -v`, such as `v20.13.1`), `none`
/// without any, and a hash of them when they would not make a short
/// directory name.
pub fn fingerprint(outputs: &[String]) -> String {
    if outputs.is_empty() {
        return "none".to_string();
    }
    let joined = outputs
        .iter()
        .map(|o| o.trim())
        .collect::<Vec<_>>()
        .join("-");
    let plain = !joined.is_empty()
        && joined.len() <= 40
        && joined
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b));
    if plain {
        joined
    } else {
        format!("h{}", super::short(&super::outputs_digest(outputs), 12))
    }
}

/// The fingerprint and the key of a target: `<16 hex digits of the
/// lockfile's sha256>-<fingerprint>`, as [`cache_key`] makes them, when
/// `key` is the lockfile alone (the default); with more files in `key`, the
/// first part hashes them all.
fn target_key(ctx: &Ctx, t: &Target, lockfile: &[u8]) -> Result<(String, String), String> {
    let project = super::project_path(&ctx.root, &t.dir);
    let outputs = super::command_outputs(&project, &t.key_commands, &ctx.root, &ctx.main)?;
    let fp = fingerprint(&outputs);
    let key = if t.key_is_lockfile_only() {
        cache_key(lockfile, &fp)
    } else {
        format!(
            "{}-{fp}",
            super::short(&super::key_digest(&ctx.root, &t.key)?, 16)
        )
    };
    Ok((fp, key))
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
    /// The candidate's lockfile (and other `key` files) are byte-identical
    /// to ours.
    fn same_lockfile(&self, project: &Path) -> bool;
    /// The candidate's installed tree matches its lockfile.
    fn tree_matches(&self, project: &Path) -> bool;
    /// What the `key_commands` print there ([`fingerprint`]).
    fn fingerprint(&self, project: &Path) -> Option<String>;
}

/// The `node_modules` to clone from, if any worktree other than `root`
/// qualifies. Candidates are tried in order (the main checkout first) and
/// the checks run lazily, so `node -v` is only spawned for a candidate that
/// passed everything else.
pub fn find_donor(
    root: &Path,
    worktrees: &[PathBuf],
    dir: &str,
    fp: &str,
    probe: &dyn DonorProbe,
) -> Option<PathBuf> {
    explain_donor(root, worktrees, dir, fp, probe, &mut Vec::new())
}

/// [`find_donor`], noting in `why` why each candidate before the one found
/// was passed over.
pub fn explain_donor(
    root: &Path,
    worktrees: &[PathBuf],
    dir: &str,
    fp: &str,
    probe: &dyn DonorProbe,
    why: &mut Vec<String>,
) -> Option<PathBuf> {
    for w in worktrees.iter().filter(|w| w.as_path() != root) {
        let p = super::project_path(w, dir);
        let name = util::tilde(w);
        let no = if !probe.is_dir(&p) {
            format!("worktree {name}: no {dir}")
        } else if !probe.same_lockfile(&p) {
            format!("worktree {name}: {LOCKFILE} (or another key file) differs")
        } else if !probe.tree_matches(&p) {
            format!("worktree {name}: node_modules does not match {LOCKFILE}")
        } else {
            match probe.fingerprint(&p) {
                Some(f) if f == fp => return Some(p.join("node_modules")),
                Some(f) => format!("worktree {name}: key_commands print {f}, not {fp}"),
                None => format!("worktree {name}: key_commands failed there"),
            }
        };
        why.push(no);
    }
    None
}

struct FsProbe<'a> {
    lockfile: &'a [u8],
    t: &'a Target,
    /// Our worktree, for the other `key` files.
    root: &'a Path,
    main: &'a Path,
}

impl FsProbe<'_> {
    /// The worktree of a candidate project directory.
    fn worktree_of<'p>(&self, project: &'p Path) -> &'p Path {
        let depth = match self.t.dir.as_str() {
            "." => 0,
            d => Path::new(d).components().count(),
        };
        project.ancestors().nth(depth).unwrap_or(project)
    }
}

impl DonorProbe for FsProbe<'_> {
    fn is_dir(&self, project: &Path) -> bool {
        project.is_dir()
    }
    fn same_lockfile(&self, project: &Path) -> bool {
        fs::read(project.join(LOCKFILE)).is_ok_and(|b| b == self.lockfile)
            && (self.t.key_is_lockfile_only()
                || super::same_key_files(self.worktree_of(project), self.root, &self.t.key).is_ok())
    }
    fn tree_matches(&self, project: &Path) -> bool {
        !scripts_pending(project) && installed_tree_matches(project, self.lockfile)
    }
    fn fingerprint(&self, project: &Path) -> Option<String> {
        let wt = self.worktree_of(project);
        super::command_outputs(project, &self.t.key_commands, wt, self.main)
            .ok()
            .map(|o| fingerprint(&o))
    }
}

/// Whether `project/node_modules` is the tree `lockfile` asks for.
fn installed_tree_matches(project: &Path, lockfile: &[u8]) -> bool {
    fs::read(project.join(HIDDEN_LOCKFILE))
        .is_ok_and(|hidden| tree_match::tree_matches(lockfile, &hidden).is_ok())
}

/// Where a clone came from.
enum Source {
    Cache(PathBuf),
    Worktree(PathBuf),
}

fn install(ctx: &Ctx, t: &Target, mode: Mode) -> Result<Report, String> {
    let dir = t.dir.as_str();
    let start = Instant::now();
    // Time spent waiting for the lifecycle lock, shown apart.
    let mut waited = Duration::ZERO;
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
        if is_installed(&target) {
            return Ok(Report::kept(format!(
                "skip   {dir} (node_modules already present)"
            )));
        }
        // Left by an install that did not finish, or made by hand: npm
        // writes `.package-lock.json` last.
        let note = if scripts_pending(&target) {
            format!("note   {dir}: the install scripts in node_modules did not finish")
        } else {
            format!("note   {dir}: node_modules has no .package-lock.json")
        };
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
    let (fp, key) = target_key(ctx, t, &lockfile)?;
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
                &fp,
                &FsProbe {
                    lockfile: &lockfile,
                    t,
                    root: &ctx.root,
                    main: &ctx.main,
                },
            )
            .map(Source::Worktree)
        };
        if let Some(src) = src {
            let (path, checked) = match &src {
                Source::Cache(p) => (p, cache::is_checked(p)),
                Source::Worktree(p) => (p, false),
            };
            match clone_from(path, &node_modules, checked).and_then(|()| verify(ctx, t, "clone")) {
                Ok(()) => {
                    let made_in = match &src {
                        // An entry from before markers existed, now checked.
                        Source::Cache(p) => {
                            if !checked {
                                cache::mark_checked(p);
                            }
                            cache::source(p)
                        }
                        // Stored before the project's own scripts touch it:
                        // this is the donor's tree, just scanned.
                        Source::Worktree(p) => {
                            let donor = donor_root(p, dir);
                            save(
                                &node_modules,
                                &entry,
                                true,
                                &lockfile,
                                dir,
                                &donor,
                                &mut lines,
                            );
                            Some(donor)
                        }
                    };
                    rewrite_from(t, &ctx.root, made_in);
                    lines.extend(after_clone(ctx, &target, dir, &key, &mut waited)?);
                    run_after(ctx, t, &target)?;
                    lines.push(format!(
                        "clone  {dir} from {} ({})",
                        util::tilde(path),
                        super::timing(start, waited)
                    ));
                    return Ok(Report::done(lines.join("\n")));
                }
                Err(why) => {
                    discard(&node_modules);
                    lines.push(format!("note   {dir}: {why}, not cloned"));
                }
            }
        }
    }

    if mode.clone_only() {
        return Ok(Report::missing(lines.join("\n")));
    }

    if clone::SUPPORTED {
        if let Some(base) = cache::latest(&project_cache, &fp, &entry) {
            cache::touch(&base);
            match delta(&base, &target, &lockfile, &ctx.cache_repo, &mut waited) {
                Ok(()) => {
                    write_key(&target, &key);
                    rewrite_from(t, &ctx.root, cache::source(&base));
                    if let Err(e) = verify(ctx, t, "delta install") {
                        discard(&node_modules);
                        return Err(e);
                    }
                    save(
                        &node_modules,
                        &entry,
                        false,
                        &lockfile,
                        dir,
                        &ctx.root,
                        &mut lines,
                    );
                    cache::prune(
                        &project_cache,
                        Duration::from_secs(cache::TTL_DAYS * 86_400),
                    );
                    run_after(ctx, t, &target)?;
                    lines.push(format!(
                        "delta  {dir} from {} ({})",
                        util::tilde(&base),
                        super::timing(start, waited)
                    ));
                    return Ok(Report::done(lines.join("\n")));
                }
                Err(why) => lines.push(format!("note   {dir}: {why}, falling back to npm ci")),
            }
        }
    }

    let done = match &t.install {
        None => npm_ci(&target, dir, &lockfile, &ctx.cache_repo, &mut waited),
        Some(cmd) => own_install(ctx, &target, cmd, &mut waited),
    };
    match done.and_then(|notes| verify(ctx, t, "install").map(|()| notes)) {
        Ok(notes) => lines.extend(notes),
        Err(log) => {
            discard(&node_modules);
            return Err(log);
        }
    }
    write_key(&target, &key);
    if clone::SUPPORTED && node_modules.is_dir() {
        save(
            &node_modules,
            &entry,
            false,
            &lockfile,
            dir,
            &ctx.root,
            &mut lines,
        );
        cache::prune(
            &project_cache,
            Duration::from_secs(cache::TTL_DAYS * 86_400),
        );
    }
    run_after(ctx, t, &target)?;
    lines.push(match &t.install {
        None => format!("npm ci {dir} ({})", super::timing(start, waited)),
        Some(cmd) => format!("install {dir} ({cmd}, {})", super::timing(start, waited)),
    });
    Ok(Report::done(lines.join("\n")))
}

/// The worktree a donor's `node_modules` (`<worktree>/<dir>/node_modules`)
/// is in.
fn donor_root(node_modules: &Path, dir: &str) -> PathBuf {
    let depth = 1 + match dir {
        "." => 0,
        d => Path::new(d).components().count(),
    };
    node_modules
        .ancestors()
        .nth(depth)
        .unwrap_or(node_modules)
        .to_path_buf()
}

/// With `rewrite_paths`, replace the worktree a clone was made in with
/// this one in the text files of its `node_modules`.
fn rewrite_from(t: &Target, root: &Path, made_in: Option<PathBuf>) {
    if let (true, Some(from)) = (t.rewrite_paths, made_in) {
        super::rewrite_tree(&root.join(&t.path), &[from], root);
    }
}

/// `verify`, in `node_modules`, after `what`.
fn verify(ctx: &Ctx, t: &Target, what: &str) -> Result<(), String> {
    match &t.verify {
        Some(v) => super::run_quietly(&ctx.root.join(&t.path), v, &ctx.root, &ctx.main)
            .map_err(|e| format!("verify failed after the {what}: {e}")),
        None => Ok(()),
    }
}

/// The `after` commands, in the project directory.
fn run_after(ctx: &Ctx, t: &Target, project: &Path) -> Result<(), String> {
    for a in &t.after {
        super::run_quietly(project, a, &ctx.root, &ctx.main)?;
    }
    Ok(())
}

/// A `[[clone]]`'s own `install` in place of `npm ci`, holding the
/// [`lifecycle_lock`]: whatever it runs, it may run install scripts.
fn own_install(
    ctx: &Ctx,
    project: &Path,
    cmd: &str,
    waited: &mut Duration,
) -> Result<Vec<String>, String> {
    let lock = lifecycle_lock(&ctx.cache_repo);
    *waited += lock.waited;
    super::run_quietly(project, cmd, &ctx.root, &ctx.main)?;
    clear_scripts_pending(project);
    Ok(Vec::new())
}

/// What `install` would do, step by step, without doing it.
fn plan(ctx: &Ctx, t: &Target, mode: Mode, fresh: bool) -> Plan {
    let dir = t.dir.as_str();
    let target = super::project_path(&ctx.root, dir);
    let node_modules = target.join("node_modules");
    let mut why = Vec::new();
    let present = !fresh && node_modules.symlink_metadata().is_ok();
    if present {
        if let Some(by) = foreign(&target) {
            return Plan::new(format!("present (installed by {by}; left as it is)"), why);
        }
        if !mode.reinstalls_changed() {
            if is_installed(&target) {
                return Plan::new("present (node_modules)", why);
            }
            why.push("present: node_modules is not complete".to_string());
        }
    }
    let lockfile = match fs::read(target.join(LOCKFILE)) {
        Ok(l) => l,
        Err(e) => return Plan::new(format!("nothing: {LOCKFILE}: {e}"), why),
    };
    if !has_dependencies(&lockfile) {
        return Plan::new("nothing (no dependencies)", why);
    }
    let (fp, key) = match target_key(ctx, t, &lockfile) {
        Ok(k) => k,
        Err(e) => return Plan::new(format!("nothing: {e}"), why),
    };
    if present && mode.reinstalls_changed() {
        if is_current(&target, &lockfile, &key) {
            return Plan::new("present (up to date)", why);
        }
        why.push("present: lockfile or Node version changed".to_string());
    }
    let install = t.install.as_deref().unwrap_or("npm ci");
    let later = |why| Plan::new(format!("later, in the background ({install})"), why);
    if !clone::SUPPORTED {
        why.push("clone: needs macOS clonefile(2)".to_string());
        return if mode.clone_only() {
            later(why)
        } else {
            Plan::new(format!("install ({install})"), why)
        };
    }
    let project_cache = ctx.cache_repo.join(project_slug(dir));
    let entry = project_cache.join(&key);
    if entry.is_dir() {
        match &t.verify {
            Some(v) if !super::succeeds(&entry, v, &ctx.root, &ctx.main) => {
                why.push(format!("cache {}: verify failed", util::tilde(&entry)));
            }
            _ => return Plan::new(format!("cache {}", util::tilde(&entry)), why),
        }
    } else {
        why.push(format!("cache: no entry {key}"));
        let probe = FsProbe {
            lockfile: &lockfile,
            t,
            root: &ctx.root,
            main: &ctx.main,
        };
        let not = if fresh { Path::new("") } else { &ctx.root };
        if let Some(p) = explain_donor(not, &ctx.worktrees, dir, &fp, &probe, &mut why) {
            return Plan::new(format!("worktree {}", util::tilde(&p)), why);
        }
    }
    if mode.clone_only() {
        return later(why);
    }
    match cache::latest(&project_cache, &fp, &entry) {
        Some(base) => Plan::new(
            format!("delta from {} (npm install)", util::tilde(&base)),
            why,
        ),
        None => {
            why.push(format!("delta: no older cache entry for {fp}"));
            Plan::new(format!("install ({install})"), why)
        }
    }
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
    if scripts_pending(project) {
        return false;
    }
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
    made_in: &Path,
    lines: &mut Vec<String>,
) {
    let stored = match fs::read(node_modules.join(".package-lock.json")) {
        Err(_) => Err("node_modules has no .package-lock.json".to_string()),
        Ok(hidden)
            if has_packages(lockfile) && tree_match::tree_matches(lockfile, &hidden).is_err() =>
        {
            Err("node_modules does not match package-lock.json".to_string())
        }
        Ok(_) => cache::store(node_modules, entry, checked, made_in),
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
fn after_clone(
    ctx: &Ctx,
    target: &Path,
    dir: &str,
    key: &str,
    waited: &mut Duration,
) -> Result<Vec<String>, String> {
    let scripts = Scripts::own(&ctx.skip_lifecycle);
    match run_scripts(target, dir, &scripts, &ctx.cache_repo, waited) {
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
/// bring it up to date with `npm install`, then run the install scripts of
/// what changed. On error the tree is discarded and `package-lock.json` is
/// as it was.
fn delta(
    base: &Path,
    project: &Path,
    lockfile: &[u8],
    cache_repo: &Path,
    waited: &mut Duration,
) -> Result<(), String> {
    let node_modules = project.join("node_modules");
    clone_from(base, &node_modules, cache::is_checked(base))?;
    // Until the scripts have run, the tree is not installed.
    set_scripts_pending(project);
    let before = fs::read(project.join(HIDDEN_LOCKFILE)).unwrap_or_default();
    let split = scripts_can_wait(lockfile);
    let out = if split {
        npm_output(project, &["install", "--ignore-scripts"])
    } else {
        // `npm install` runs the project's own lifecycle too.
        let lock = lifecycle_lock(cache_repo);
        *waited += lock.waited;
        npm_output(project, &["install"])
    };
    let lock_path = project.join(LOCKFILE);
    let rewritten = fs::read(&lock_path).map_or(true, |now| now != lockfile);
    let mut why = match out {
        Err(e) => Some(format!("cannot run npm: {e}")),
        Ok(o) if !o.status.success() => Some("npm install failed".to_string()),
        Ok(_) if rewritten => Some("npm install changed package-lock.json".to_string()),
        Ok(_) if !installed_tree_matches(project, lockfile) => {
            Some("npm install left a tree that differs from package-lock.json".to_string())
        }
        Ok(_) => None,
    };
    if why.is_none() && split {
        let after = fs::read(project.join(HIDDEN_LOCKFILE)).unwrap_or_default();
        let scripts = Scripts::install(scripted_packages(&after, Some(&before)));
        if let Err(e) = run_install_scripts(project, &scripts, cache_repo, waited) {
            why = Some(format!("install scripts failed after npm install: {e}"));
        }
    }
    let Some(why) = why else {
        clear_scripts_pending(project);
        return Ok(());
    };
    if rewritten {
        let _ = fs::write(&lock_path, lockfile);
    }
    discard(&node_modules);
    Err(why)
}

/// `npm <args> --prefer-offline --no-audit --no-fund` in `project`, run
/// again when it found the git config locked.
fn npm_output(project: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    output_retrying_config_lock(|| {
        let mut c = Command::new("npm");
        c.args(args)
            .args(NPM_FLAGS)
            .current_dir(project)
            .stdin(Stdio::null());
        c
    })
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

/// Whether the install scripts of this lockfile can run after an install
/// without them (`--ignore-scripts`) and give the same tree: not when it
/// has a git dependency (npm prepares those while fetching them) or a link
/// (a workspace, whose scripts npm runs as part of the install), and not
/// for a lockfile without `packages` (version 1), whose records say
/// nothing about install scripts.
pub fn scripts_can_wait(lockfile: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(lockfile) else {
        return false;
    };
    let Some(packages) = v.get("packages").and_then(|p| p.as_object()) else {
        return false;
    };
    packages
        .iter()
        .filter(|(k, _)| !k.is_empty())
        .all(|(_, p)| {
            let link = p.get("link").and_then(|l| l.as_bool()) == Some(true);
            let git = p
                .get("resolved")
                .and_then(|r| r.as_str())
                .is_some_and(|r| r.starts_with("git") || r.starts_with("github:"));
            !link && !git
        })
}

/// The names of the installed packages with an install script
/// (`hasInstallScript` in `node_modules/.package-lock.json`), for `npm
/// rebuild`. With `before` (the record of the tree an `npm install`
/// started from), only those that are new or changed. A package installed
/// under an alias is named by both names; `npm rebuild` ignores a name it
/// does not find.
pub fn scripted_packages(hidden: &[u8], before: Option<&[u8]>) -> Vec<String> {
    let parse = |b: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(b)
            .ok()
            .and_then(|v| v.get("packages").cloned())
    };
    let Some(serde_json::Value::Object(now)) = parse(hidden) else {
        return Vec::new();
    };
    let then = before.and_then(parse);
    let same = |k: &str, p: &serde_json::Value| {
        let Some(old) = then.as_ref().and_then(|t| t.get(k)) else {
            return false;
        };
        ["version", "resolved", "integrity"]
            .iter()
            .all(|f| old.get(f) == p.get(f))
    };
    let mut names: Vec<String> = Vec::new();
    for (k, p) in &now {
        if k.is_empty() || p.get("hasInstallScript").and_then(|h| h.as_bool()) != Some(true) {
            continue;
        }
        if before.is_some() && same(k, p) {
            continue;
        }
        let by_path = k
            .rsplit_once("node_modules/")
            .map_or(k.as_str(), |(_, n)| n);
        names.push(by_path.to_string());
        if let Some(n) = p.get("name").and_then(|n| n.as_str()) {
            names.push(n.to_string());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// How a failed `npm rebuild` starts its log, so that [`npm_ci`] can tell
/// it from a failing script of the project.
const REBUILD_FAILED: &str = "npm rebuild failed";

/// What [`run_scripts`] runs.
struct Scripts<'a> {
    /// Dependencies whose install scripts `npm rebuild` runs first.
    rebuild: Vec<String>,
    /// `skip_lifecycle` rules for the project's own scripts.
    rules: &'a [SkipLifecycle],
    /// Shown when one of the project's own scripts fails.
    after: &'static str,
}

impl<'a> Scripts<'a> {
    /// The project's own lifecycle after a clone, `skip_lifecycle` rules
    /// applied.
    fn own(rules: &'a [SkipLifecycle]) -> Scripts<'a> {
        Scripts {
            rebuild: Vec::new(),
            rules,
            after: " after clone",
        }
    }

    /// Everything an install without scripts left out: the dependencies'
    /// `rebuild` and the project's own lifecycle, with no rule (`npm ci`
    /// would run them all).
    fn install(rebuild: Vec<String>) -> Scripts<'static> {
        Scripts {
            rebuild,
            rules: &[],
            after: "",
        }
    }
}

/// Run what an install without scripts (`--ignore-scripts`) left out, as
/// that install would have: nothing when npm's `ignore-scripts` is set.
fn run_install_scripts(
    project: &Path,
    scripts: &Scripts,
    cache_repo: &Path,
    waited: &mut Duration,
) -> Result<(), String> {
    let own = fs::read(project.join("package.json"))
        .map(|p| own_lifecycle_scripts(&p))
        .unwrap_or_default();
    if (scripts.rebuild.is_empty() && own.is_empty()) || ignore_scripts(project) {
        return Ok(());
    }
    run_scripts(project, ".", scripts, cache_repo, waited).map(|_| ())
}

/// Whether npm's `ignore-scripts` is set for the project (in an `.npmrc`
/// or the environment): then `npm ci` runs no script, and neither does
/// fast-worktree in its place.
fn ignore_scripts(project: &Path) -> bool {
    Command::new("npm")
        .args(["config", "get", "ignore-scripts"])
        .current_dir(project)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
}

/// Run `npm rebuild` of `scripts.rebuild`, then the project's own
/// lifecycle scripts except those a rule skips, holding the repository's
/// [`lifecycle_lock`] (taken only when something runs; the wait is added
/// to `waited`). A run that finds the git config locked is tried again.
/// Returns a note per skipped script, and the scripts that ran.
fn run_scripts(
    project: &Path,
    dir: &str,
    scripts: &Scripts,
    cache_repo: &Path,
    waited: &mut Duration,
) -> Result<(Vec<String>, Vec<&'static str>), String> {
    let own = fs::read(project.join("package.json"))
        .map(|p| own_lifecycle_scripts(&p))
        .unwrap_or_default();
    let mut notes = Vec::new();
    let mut ran = Vec::new();
    let mut lock = None;
    let mut take = |waited: &mut Duration| {
        if lock.is_none() {
            let l = lifecycle_lock(cache_repo);
            *waited += l.waited;
            lock = Some(l);
        }
    };
    if !scripts.rebuild.is_empty() {
        take(waited);
        let out = output_retrying_config_lock(|| {
            let mut c = Command::new("npm");
            c.arg("rebuild")
                .args(&scripts.rebuild)
                .current_dir(project)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            c
        })
        .map_err(|e| format!("cannot run npm: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "{REBUILD_FAILED}:\n{}",
                util::tail(&String::from_utf8_lossy(&out.stderr), 30)
            ));
        }
    }
    for script in own {
        if scripts
            .rules
            .iter()
            .any(|r| r.applies_to(dir, script) && r.inputs_unchanged(project))
        {
            notes.push(format!("note   {dir}: {script} skipped, inputs unchanged"));
            continue;
        }
        take(waited);
        // `--ignore-scripts`: this script alone, not its `pre` and `post`
        // ones (`npm run install` would also run `preinstall` and
        // `postinstall`, which run on their own turn).
        let out = output_retrying_config_lock(|| {
            let mut c = Command::new("npm");
            c.args(["run", script, "--ignore-scripts"])
                .current_dir(project)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            c
        })
        .map_err(|e| format!("cannot run npm: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "npm run {script} failed{}:\n{}",
                scripts.after,
                util::tail(&String::from_utf8_lossy(&out.stderr), 30)
            ));
        }
        ran.push(script);
    }
    Ok((notes, ran))
}

/// `npm ci`. Without its scripts first, for every project at once, then the
/// scripts one at a time per repository ([`run_install_scripts`]); with
/// them, under the [`lifecycle_lock`], when they cannot wait
/// ([`scripts_can_wait`]) or `npm rebuild` failed. Its output is shown only
/// when it fails. Returns notes to print.
fn npm_ci(
    project: &Path,
    dir: &str,
    lockfile: &[u8],
    cache_repo: &Path,
    waited: &mut Duration,
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    if scripts_can_wait(lockfile) {
        let out = npm_output(project, &["ci", "--ignore-scripts"])
            .map_err(|e| format!("cannot run npm: {e}"))?;
        if !out.status.success() {
            return Err(npm_log(&out));
        }
        set_scripts_pending(project);
        let hidden = fs::read(project.join(HIDDEN_LOCKFILE)).unwrap_or_default();
        let scripts = Scripts::install(scripted_packages(&hidden, None));
        match run_install_scripts(project, &scripts, cache_repo, waited) {
            Ok(()) => {
                clear_scripts_pending(project);
                return Ok(notes);
            }
            Err(e) if e.starts_with(REBUILD_FAILED) => {
                notes.push(format!(
                    "note   {dir}: npm rebuild failed, running npm ci with its scripts"
                ));
            }
            Err(e) => return Err(e),
        }
    }
    let lock = lifecycle_lock(cache_repo);
    *waited += lock.waited;
    let out = npm_output(project, &["ci"]).map_err(|e| format!("cannot run npm: {e}"))?;
    if out.status.success() {
        // npm ci starts from an empty node_modules; this is for one that
        // did not.
        clear_scripts_pending(project);
        return Ok(notes);
    }
    Err(npm_log(&out))
}

/// The end of what npm printed, for a failure.
fn npm_log(out: &std::process::Output) -> String {
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    util::tail(&log, 30)
}

/// Whether the install scripts of the tree in `project` are still to run.
fn scripts_pending(project: &Path) -> bool {
    project.join(SCRIPTS_PENDING).symlink_metadata().is_ok()
}

fn set_scripts_pending(project: &Path) {
    let _ = fs::write(project.join(SCRIPTS_PENDING), b"");
}

fn clear_scripts_pending(project: &Path) {
    let _ = fs::remove_file(project.join(SCRIPTS_PENDING));
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
        save(
            &nm,
            &t.path().join("entry"),
            true,
            v1,
            "app",
            t.path(),
            &mut lines,
        );
        assert!(
            !lines.iter().any(|l| l.contains("package-lock.json")),
            "{lines:?}"
        );
        // Without the record it is still not saved.
        fs::remove_file(nm.join(".package-lock.json")).unwrap();
        let mut lines = Vec::new();
        save(
            &nm,
            &t.path().join("entry2"),
            true,
            v1,
            "app",
            t.path(),
            &mut lines,
        );
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
        let rules = std::slice::from_ref(&rule);
        let mut waited = Duration::ZERO;
        let (notes, ran) = run_scripts(
            p,
            "app",
            &Scripts::own(rules),
            &p.join("cache"),
            &mut waited,
        )
        .unwrap();
        assert!(ran.is_empty());
        assert_eq!(
            notes,
            vec!["note   app: postinstall skipped, inputs unchanged"]
        );
    }

    #[test]
    fn scripts_wait_unless_git_dependencies_links_or_version_1() {
        let lock = |pkgs: &str| format!(r#"{{"lockfileVersion":3,"packages":{{"":{{}}{pkgs}}}}}"#);
        assert!(scripts_can_wait(lock("").as_bytes()));
        assert!(scripts_can_wait(
            lock(r#","node_modules/a":{"version":"1.0.0","resolved":"https://r/a.tgz"}"#)
                .as_bytes()
        ));
        assert!(!scripts_can_wait(
            lock(r#","node_modules/a":{"resolved":"git+ssh://git@h/a.git#0123"}"#).as_bytes()
        ));
        assert!(!scripts_can_wait(
            lock(r#","node_modules/w":{"resolved":"packages/w","link":true}"#).as_bytes()
        ));
        assert!(!scripts_can_wait(
            br#"{"lockfileVersion":1,"dependencies":{"a":{}}}"#
        ));
        assert!(!scripts_can_wait(b"not json"));
    }

    #[test]
    fn scripted_packages_are_named_and_only_changed_ones_after_an_install() {
        let hidden = br#"{"packages":{
            "node_modules/plain":{"version":"1.0.0"},
            "node_modules/native":{"version":"2.0.0","hasInstallScript":true},
            "node_modules/a/node_modules/@s/deep":{"version":"1.0.0","hasInstallScript":true},
            "node_modules/alias":{"name":"real","version":"1.0.0","hasInstallScript":true}
        }}"#;
        assert_eq!(
            scripted_packages(hidden, None),
            vec!["@s/deep", "alias", "native", "real"]
        );
        let before = br#"{"packages":{
            "node_modules/native":{"version":"2.0.0","hasInstallScript":true},
            "node_modules/a/node_modules/@s/deep":{"version":"0.9.0","hasInstallScript":true}
        }}"#;
        assert_eq!(
            scripted_packages(hidden, Some(before)),
            vec!["@s/deep", "alias", "real"],
            "native is unchanged"
        );
        assert!(scripted_packages(b"{}", None).is_empty());
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
        fn fingerprint(&self, p: &Path) -> Option<String> {
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
    fn passed_over_donors_are_explained() {
        let worktrees = vec![wt("main"), wt("a"), wt("b"), wt("c"), wt("d")];
        let mut f = Fake::default();
        f.facts
            .insert(wt("main/app"), (true, true, false, Some("v20")));
        f.facts
            .insert(wt("a/app"), (true, false, true, Some("v20")));
        f.facts.insert(wt("b/app"), (true, true, true, Some("v22")));
        f.facts.insert(wt("c/app"), (true, true, true, None));
        let mut why = Vec::new();
        let got = explain_donor(&wt("d"), &worktrees, "app", "v20", &f, &mut why);
        assert_eq!(got, None);
        assert_eq!(
            why,
            vec![
                "worktree /r/main: node_modules does not match package-lock.json",
                "worktree /r/a: package-lock.json (or another key file) differs",
                "worktree /r/b: key_commands print v22, not v20",
                "worktree /r/c: key_commands failed there",
            ]
        );
    }

    #[test]
    fn fingerprints_are_the_outputs_or_their_hash() {
        let v = |o: &[&str]| fingerprint(&o.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(v(&["v20.13.1\n"]), "v20.13.1", "the default: node -v");
        assert_eq!(v(&["v20.13.1", "10.9.2"]), "v20.13.1-10.9.2");
        assert_eq!(v(&[]), "none");
        let long = v(&["Flutter 3.24.0 • channel stable"]);
        assert!(long.starts_with('h') && long.len() == 13, "{long}");
        assert_eq!(long, v(&["Flutter 3.24.0 • channel stable"]));
        assert_ne!(v(&["a/b"]), "a/b", "never a path separator");
        assert_eq!(
            cache_key(b"{}", &v(&["v20.11.1"])),
            cache_key(b"{}", "v20.11.1")
        );
    }

    #[test]
    fn a_donor_node_modules_is_in_its_worktree() {
        let nm = Path::new("/r/wt/packages/web/node_modules");
        assert_eq!(donor_root(nm, "packages/web"), Path::new("/r/wt"));
        assert_eq!(
            donor_root(Path::new("/r/wt/node_modules"), "."),
            Path::new("/r/wt")
        );
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
