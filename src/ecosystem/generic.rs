//! A `[[clone]]` without a preset: a directory (`path`) that is reused
//! whole, such as CocoaPods' `Pods` or a build cache, decided by the
//! contents of its `key` files and the output of its `key_commands`.
//!
//! `path` is made present by the first of these that applies:
//!
//! 1. **Cache.** `<cache>/<repo>/<path>/<key>` holds the directory from an
//!    earlier install, or from a worktree that `remove` deleted ([`reclaim`]).
//! 2. **Another worktree** whose `path` was made from the same key (its
//!    recorded key is ours), or, without a record, whose `key` files are
//!    byte-identical, whose `key_commands` print the same there and where
//!    `verify` passes.
//! 3. `install`, run in `dir`. Its result is stored in the cache.
//!
//! A clone is one `clonefile(2)` call (macOS only; elsewhere only `install`
//! runs). After it, `verify` must pass in `path`, else the clone is thrown
//! away and the next source is tried; with `rewrite_paths`, the worktree it
//! was made in is replaced with this one in its text files; then the
//! `after` commands run. The key is `<16 hex digits of the key files>-<8
//! hex digits of the key commands' output>`, and is written to
//! `<path>/.fast-worktree-key` once all of that succeeded, so that
//! `install`, `check` and `remove` can tell what the directory was made
//! from. What fails on the way (the install, `verify`, an `after`) moves
//! `path` away: a directory without that record is not taken as installed,
//! lent to another worktree or kept in the cache, unless `verify` vouches
//! for it.
//!
//! Nor is it fast-worktree's: a `path` there without a record (a build's
//! output, a hand-made `Pods`) is left as it is by `install`, and made
//! again only with `--changed`. What is made again (that, or a `path`
//! recorded from another key) is moved aside, into the trash, until its
//! replacement is complete, and put back when nothing replaces it.
//!
//! `verify` runs in a copy of this worktree's own: after a clone, after an
//! install, and in a `path` without a record. It never runs in a cache
//! entry, nor in another worktree whose recorded key is ours.
//!
//! Unlike `node_modules`, the directory is not checked for symlinks with an
//! absolute target: one that points into the worktree it was made in
//! points there in a clone too. `rewrite_paths` leaves symlinks alone.

use super::{target, Ctx, Mode, Plan, Preset, Report, Target};
use crate::paths::project_slug;
use crate::{cache, clone, trash, util};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the directory was made from, inside it.
pub const KEY_FILE: &str = ".fast-worktree-key";

pub struct Generic;

impl Preset for Generic {
    fn name(&self) -> &'static str {
        "clone"
    }

    fn manifest(&self) -> &'static str {
        ""
    }

    fn managed(&self) -> &'static str {
        ""
    }

    fn is_installed(&self, root: &Path, t: &Target) -> bool {
        is_installed(root, t)
    }

    fn is_unrecorded(&self, root: &Path, t: &Target) -> bool {
        is_unrecorded(root, t)
    }

    fn install(&self, ctx: &Ctx, t: &Target, mode: Mode) -> Result<Report, String> {
        install(ctx, t, mode)
    }

    fn plan(&self, ctx: &Ctx, t: &Target, mode: Mode, fresh: bool) -> Plan {
        plan(ctx, t, mode, fresh)
    }
}

/// Whether `path` is there, in the worktree, and made from the key files
/// as they are now: its recorded key starts with their digest. Cheap, so
/// the `key_commands` are not run and `verify` is not either: a directory
/// without a record is not installed until `install` has seen `verify`
/// pass in it.
pub fn is_installed(root: &Path, t: &Target) -> bool {
    let path = root.join(&t.path);
    if target::inside(root, &t.path).is_err() || !path.is_dir() {
        return false;
    }
    let Some(k) = recorded(&path) else {
        return false;
    };
    super::key_digest(root, &t.key)
        .is_ok_and(|d| k.split('-').next() == Some(super::short(&d, 16).as_str()))
}

/// The key of `t` in the worktree `root`.
pub fn key(root: &Path, main: &Path, t: &Target) -> Result<String, String> {
    let files = super::key_digest(root, &t.key)?;
    let outputs = outputs(root, main, t)?;
    Ok(format!(
        "{}-{}",
        super::short(&files, 16),
        super::short(&super::outputs_digest(&outputs), 8)
    ))
}

fn outputs(root: &Path, main: &Path, t: &Target) -> Result<Vec<String>, String> {
    super::command_outputs(
        &super::project_path(root, &t.dir),
        &t.key_commands,
        root,
        main,
    )
}

/// The cache entry of `t` for `key`.
pub fn entry(cache_repo: &Path, t: &Target, key: &str) -> PathBuf {
    cache_repo.join(project_slug(&t.path)).join(key)
}

fn recorded(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join(KEY_FILE))
        .ok()
        .map(|k| k.trim().to_string())
}

fn write_key(dir: &Path, key: &str) {
    let _ = fs::write(dir.join(KEY_FILE), format!("{key}\n"));
}

/// Move `path` away, when there is something there.
fn discard(path: &Path) {
    if path.symlink_metadata().is_ok() {
        let _ = trash::trash(path);
    }
}

/// What the `path` already in this worktree is to an install with `key`.
enum Standing {
    /// Made from `key`.
    Current,
    /// Without a record, and `verify` passes in it: it is taken as made
    /// from `key`, and recorded so.
    Verified,
    /// Made by fast-worktree from another key: to be made again, and why.
    Stale(String),
    /// Without a record of its key, and no `verify` vouches for it: not
    /// known to be fast-worktree's (made by hand, by a build, or left by an
    /// install that was killed). Left as it is, unless `--changed` asks for
    /// it to be made again; and why.
    Unrecorded(String),
}

fn standing(ctx: &Ctx, t: &Target, path: &Path, key: &str) -> Standing {
    match recorded(path) {
        Some(k) if k == key => Standing::Current,
        Some(_) => Standing::Stale("its key changed".into()),
        None => match &t.verify {
            Some(v) if super::succeeds(path, v, &ctx.root, &ctx.main) => Standing::Verified,
            Some(_) => Standing::Unrecorded("no record of its key, and verify fails".into()),
            None => Standing::Unrecorded("no record of its key".into()),
        },
    }
}

/// Whether `path` is a directory in the worktree without a record of its
/// key: one that `install` leaves as it is (unless `verify` vouches for
/// it). Cheap, for `check`.
pub fn is_unrecorded(root: &Path, t: &Target) -> bool {
    let path = root.join(&t.path);
    target::inside(root, &t.path).is_ok()
        && path.is_dir()
        && path.join(KEY_FILE).symlink_metadata().is_err()
}

/// What was in `path` before it is made again, moved into the trash (kept
/// there for a day, should this process die) until the new one is
/// complete: deleted then, else put back.
struct Aside {
    at: PathBuf,
    why: String,
}

/// How long what was moved aside stays in the trash at most.
const ASIDE_SECS: u64 = 86_400;

impl Aside {
    /// The new `path` is complete: delete the old one.
    fn drop_it(self) {
        trash::spawn_rm(&self.at);
    }

    /// Nothing replaced it: put it back at `path`. Returns a line to print
    /// when that could not be done.
    fn put_back(self, ctx: &Ctx, t: &Target, path: &Path) -> Option<String> {
        let back = target::inside(&ctx.root, &t.path).and_then(|()| {
            if path.symlink_metadata().is_ok() {
                return Err(format!("{} is there again", t.path));
            }
            fs::rename(&self.at, path).map_err(|e| e.to_string())
        });
        back.err().map(|e| {
            format!(
                "note   {}: could not put it back ({e}); it is in {} for a day",
                t.name,
                util::tilde(&self.at)
            )
        })
    }
}

/// Where [`make`] got `path` from.
enum Made {
    /// A line to print.
    From(String),
    /// Nothing could make it: no source to clone, and no `install` (or only
    /// clones were allowed).
    Nothing,
}

/// Why the worktree `w` cannot lend its `path` to a worktree whose key is
/// `key`, or `None` when it can: its recorded key is `key`, or, without a
/// record, its key files and `key_commands` agree and `verify` passes.
/// Nothing runs there when it has a record.
fn not_a_donor(ctx: &Ctx, t: &Target, w: &Path, ours: &[String], key: &str) -> Option<String> {
    let name = util::tilde(w);
    if let Err(why) = target::real_rel(w, &t.path) {
        return Some(format!("worktree {name}: {} {why}", t.path));
    }
    let p = w.join(&t.path);
    if !p.is_dir() {
        return Some(format!("worktree {name}: no {}", t.path));
    }
    match recorded(&p) {
        Some(k) if k == key => return None,
        Some(_) => {
            return Some(format!(
                "worktree {name}: its {} was made from other key files or tools",
                t.path
            ))
        }
        None => {}
    }
    if let Err(k) = super::same_key_files(w, &ctx.root, &t.key) {
        return Some(format!("worktree {name}: {k} differs"));
    }
    if !t.key_commands.is_empty() && outputs(w, &ctx.main, t).ok().as_deref() != Some(ours) {
        return Some(format!(
            "worktree {name}: key_commands print something else there"
        ));
    }
    match &t.verify {
        Some(v) if super::succeeds(&p, v, w, &ctx.main) => None,
        Some(_) => Some(format!("worktree {name}: verify failed there")),
        None => Some(format!(
            "worktree {name}: no record of what its {} was made from, and no verify",
            t.path
        )),
    }
}

/// Clone `src` (made in the worktree `made_in`) to `path`, then `verify`
/// and `rewrite_paths`. On error nothing is left at `path`. The key is not
/// written: [`finish`] does, after `after`.
fn clone_in(ctx: &Ctx, t: &Target, src: &Path, made_in: Option<PathBuf>) -> Result<(), String> {
    target::inside(&ctx.root, &t.path)?;
    let path = ctx.root.join(&t.path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    clone::clone_dir(src, &path).map_err(|e| format!("clone failed ({e})"))?;
    let _ = fs::remove_dir_all(path.join(".cache"));
    if let Some(v) = &t.verify {
        if !super::succeeds(&path, v, &ctx.root, &ctx.main) {
            discard(&path);
            return Err("verify failed after the clone".into());
        }
    }
    if let (true, Some(from)) = (t.rewrite_paths, made_in) {
        if let Err(e) = target::inside(&ctx.root, &t.path) {
            discard(&path);
            return Err(e);
        }
        super::rewrite_tree(&path, &[from], &ctx.root);
    }
    Ok(())
}

/// The `after` commands, then the key: `path` is complete. When an `after`
/// fails, `path` is moved away.
fn finish(ctx: &Ctx, t: &Target, path: &Path, key: &str) -> Result<(), String> {
    let cwd = super::project_path(&ctx.root, &t.dir);
    for a in &t.after {
        if let Err(e) = super::run_quietly(&cwd, a, &ctx.root, &ctx.main) {
            discard(path);
            return Err(e);
        }
    }
    target::inside(&ctx.root, &t.path)?;
    write_key(path, key);
    Ok(())
}

fn install(ctx: &Ctx, t: &Target, mode: Mode) -> Result<Report, String> {
    let start = Instant::now();
    let name = t.name.as_str();
    target::inside(&ctx.root, &t.path)?;
    let path = ctx.root.join(&t.path);
    let mut lines = Vec::new();
    let key = key(&ctx.root, &ctx.main, t)?;
    let mut aside = None;
    if path.symlink_metadata().is_ok() {
        let up_to_date = if mode.reinstalls_changed() {
            "up to date"
        } else {
            "already present"
        };
        let why = match standing(ctx, t, &path, &key) {
            Standing::Current => {
                return Ok(Report::kept(format!("skip   {name} ({up_to_date})")));
            }
            Standing::Verified => {
                write_key(&path, &key);
                return Ok(Report::kept(format!(
                    "skip   {name} ({up_to_date}, verify passes)"
                )));
            }
            Standing::Unrecorded(why) if !mode.reinstalls_changed() => {
                return Ok(Report::kept(format!(
                    "skip   {name} ({why}: not made by fast-worktree, left as it is; \
                     `fast-worktree install --changed {name}` makes it again)"
                )));
            }
            Standing::Stale(why) | Standing::Unrecorded(why) => why,
        };
        // Out of the way until the new one is complete, and put back when
        // nothing replaces it.
        target::inside(&ctx.root, &t.path)?;
        let at = trash::keep(&path, ASIDE_SECS)
            .map_err(|e| format!("cannot move {} away: {e}", t.path))?;
        aside = Some(Aside { at, why });
    }
    let made = make(ctx, t, mode, &path, &key, start, &mut lines);
    let Some(aside) = aside else {
        return match made? {
            Made::From(line) => {
                lines.push(line);
                Ok(Report::done(lines.join("\n")))
            }
            Made::Nothing if mode.clone_only() => Ok(Report::missing(lines.join("\n"))),
            Made::Nothing => {
                lines.push(format!(
                    "note   {name}: nothing to clone it from and no install to make it"
                ));
                Ok(Report::done(lines.join("\n")))
            }
        };
    };
    let why = aside.why.clone();
    match made {
        Ok(Made::From(line)) => {
            aside.drop_it();
            lines.push(format!("note   {name}: {why}, made again"));
            lines.push(line);
            Ok(Report::done(lines.join("\n")))
        }
        Ok(Made::Nothing) => {
            lines.extend(aside.put_back(ctx, t, &path));
            // The install that runs later makes it again.
            if mode.clone_only() && t.install.is_some() {
                return Ok(Report::missing(lines.join("\n")));
            }
            lines.push(format!(
                "note   {name}: {why}, but nothing to clone it from and no install to make it again: left as it is"
            ));
            Ok(Report::kept(lines.join("\n")))
        }
        Err(e) => {
            let mut e = e;
            if let Some(l) = aside.put_back(ctx, t, &path) {
                e.push('\n');
                e.push_str(&l);
            }
            Err(e)
        }
    }
}

/// Make `path` (not there now) from the first source that works: the
/// cache, another worktree, then `install` (unless `mode` allows only
/// clones). `lines` gets the sources that were tried and not used. On
/// error nothing is left at `path`.
fn make(
    ctx: &Ctx,
    t: &Target,
    mode: Mode,
    path: &Path,
    key: &str,
    start: Instant,
    lines: &mut Vec<String>,
) -> Result<Made, String> {
    let name = t.name.as_str();
    let entry = entry(&ctx.cache_repo, t, key);
    if clone::SUPPORTED {
        if entry.is_dir() {
            cache::touch(&entry);
            match clone_in(ctx, t, &entry, cache::source(&entry)) {
                Ok(()) => {
                    finish(ctx, t, path, key)?;
                    return Ok(Made::From(format!(
                        "clone  {name} from {} ({})",
                        util::tilde(&entry),
                        util::elapsed(start)
                    )));
                }
                Err(why) => lines.push(format!("note   {name}: {why}, not used")),
            }
        }
        let ours = outputs(&ctx.root, &ctx.main, t)?;
        for w in ctx.worktrees.iter().filter(|w| **w != ctx.root) {
            if not_a_donor(ctx, t, w, &ours, key).is_some() {
                continue;
            }
            let src = w.join(&t.path);
            match clone_in(ctx, t, &src, Some(w.clone())) {
                Ok(()) => {
                    finish(ctx, t, path, key)?;
                    return Ok(Made::From(format!(
                        "clone  {name} from {} ({})",
                        util::tilde(&src),
                        util::elapsed(start)
                    )));
                }
                Err(why) => lines.push(format!("note   {name}: {why}, not used")),
            }
        }
    }
    if mode.clone_only() {
        return Ok(Made::Nothing);
    }
    let Some(cmd) = &t.install else {
        return Ok(Made::Nothing);
    };
    let cwd = super::project_path(&ctx.root, &t.dir);
    // What a failed install leaves is not an install.
    let made = super::run_quietly(&cwd, cmd, &ctx.root, &ctx.main)
        .and_then(|()| target::inside(&ctx.root, &t.path))
        .and_then(|()| {
            if path.is_dir() {
                Ok(())
            } else {
                Err(format!("`{cmd}` did not make {}", t.path))
            }
        })
        .and_then(|()| match &t.verify {
            Some(v) => super::run_quietly(path, v, &ctx.root, &ctx.main)
                .map_err(|e| format!("verify failed after the install: {e}")),
            None => Ok(()),
        });
    if let Err(e) = made {
        if target::inside(&ctx.root, &t.path).is_ok() {
            discard(path);
        }
        return Err(e);
    }
    if clone::SUPPORTED {
        // Not checked for absolute symlinks (see the module's notes). The
        // key goes in when the entry is cloned.
        if let Err(e) = cache::store(path, &entry, true, &ctx.root) {
            lines.push(format!("note   {name}: cache not saved: {e}"));
        }
        if let Some(dir) = entry.parent() {
            cache::prune(dir, Duration::from_secs(cache::TTL_DAYS * 86_400));
        }
    }
    finish(ctx, t, path, key)?;
    Ok(Made::From(format!(
        "install {name} ({cmd}, {})",
        util::elapsed(start)
    )))
}

fn plan(ctx: &Ctx, t: &Target, mode: Mode, fresh: bool) -> Plan {
    let path = ctx.root.join(&t.path);
    let mut why = Vec::new();
    if let Err(e) = target::inside(&ctx.root, &t.path) {
        return Plan::new(format!("nothing: {e}"), why);
    }
    let key = match key(&ctx.root, &ctx.main, t) {
        Ok(k) => k,
        Err(e) => return Plan::new(format!("nothing: {e}"), why),
    };
    // What is there, made again only when something replaces it.
    let mut stale = false;
    if !fresh && path.symlink_metadata().is_ok() {
        match standing(ctx, t, &path, &key) {
            Standing::Current if mode.reinstalls_changed() => {
                return Plan::new("present (up to date)", why)
            }
            Standing::Current => return Plan::new("present", why),
            Standing::Verified => return Plan::new("present (verify passes)", why),
            Standing::Unrecorded(no) if !mode.reinstalls_changed() => {
                return Plan::new(
                    format!("present ({no}: not made by fast-worktree, left as it is)"),
                    why,
                )
            }
            Standing::Stale(no) | Standing::Unrecorded(no) => {
                why.push(format!("present: {no}"));
                stale = true;
            }
        }
    }
    // A clone is checked with verify once it is made, in this worktree.
    let checked = if t.verify.is_some() {
        " (verify after the clone)"
    } else {
        ""
    };
    let install = || match &t.install {
        Some(c) => format!("install ({c})"),
        None if stale => "nothing (no install): the one there is left as it is".to_string(),
        None => "nothing (no install)".to_string(),
    };
    if !clone::SUPPORTED {
        why.push("clone: needs macOS clonefile(2)".to_string());
    } else {
        let entry = entry(&ctx.cache_repo, t, &key);
        if entry.is_dir() {
            return Plan::new(format!("cache {}{checked}", util::tilde(&entry)), why);
        }
        why.push(format!("cache: no entry {key}"));
        let ours = match outputs(&ctx.root, &ctx.main, t) {
            Ok(o) => o,
            Err(e) => return Plan::new(format!("nothing: {e}"), why),
        };
        for w in ctx.worktrees.iter().filter(|w| fresh || **w != ctx.root) {
            match not_a_donor(ctx, t, w, &ours, &key) {
                Some(no) => why.push(no),
                None => {
                    return Plan::new(
                        format!("worktree {}{checked}", util::tilde(&w.join(&t.path))),
                        why,
                    )
                }
            }
        }
    }
    if mode.clone_only() {
        if let Some(c) = &t.install {
            return Plan::new(format!("later, in the background ({c})"), why);
        }
    }
    Plan::new(install(), why)
}

/// Before the worktree that was at `made_in` is deleted, from where it is
/// now (`at`: moved into the trash, so that nothing here runs unless it is
/// surely going), move the `path` of each of `targets` without a preset
/// into the cache, where the next worktree with the same key clones it:
/// when it is a directory in the worktree (not through a symlink), was made
/// from the key files it has now (its recorded key) or passes `verify`, and
/// the cache has no entry for that key yet. A rename on the same volume
/// costs next to nothing; one that fails (another volume) leaves it to be
/// deleted with the worktree. Only where directories can be cloned.
/// Returns lines to print.
pub fn reclaim(
    at: &Path,
    made_in: &Path,
    main: &Path,
    cache_repo: &Path,
    targets: &[Target],
) -> Vec<String> {
    let mut lines = Vec::new();
    if !clone::SUPPORTED {
        return lines;
    }
    for t in targets.iter().filter(|t| !t.has_preset()) {
        let path = at.join(&t.path);
        if !path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let name = &t.name;
        if let Err(e) = target::inside(at, &t.path) {
            lines.push(format!("note   {name}: not kept in the cache: {e}"));
            continue;
        }
        let key = match key(at, main, t) {
            Ok(k) => k,
            Err(e) => {
                lines.push(format!("note   {name}: not kept in the cache: {e}"));
                continue;
            }
        };
        let made_from_these = recorded(&path).as_deref() == Some(key.as_str());
        if !made_from_these {
            let verified = t
                .verify
                .as_ref()
                .is_some_and(|v| super::succeeds(&path, v, at, main));
            if !verified {
                lines.push(format!(
                    "note   {name}: not kept in the cache: not made from its key files as they are{}",
                    if t.verify.is_some() {
                        ", and verify failed"
                    } else {
                        ""
                    }
                ));
                continue;
            }
            write_key(&path, &key);
        }
        let entry = entry(cache_repo, t, &key);
        if let Err(e) = target::inside(at, &t.path) {
            lines.push(format!("note   {name}: not kept in the cache: {e}"));
            continue;
        }
        match cache::adopt(&path, &entry, made_in) {
            Ok(true) => lines.push(format!("cache  {name} kept as {}", util::tilde(&entry))),
            Ok(false) => {}
            Err(e) => lines.push(format!("note   {name}: not kept in the cache ({e})")),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecosystem::target;
    use crate::hooks;

    fn target(toml: &str) -> Target {
        let cfg = hooks::parse(&format!("[[clone]]\n{toml}\n")).unwrap().0;
        target::resolve(&cfg.clone).unwrap().remove(0)
    }

    #[test]
    fn the_key_is_the_files_and_the_command_output() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        fs::create_dir_all(root.join("ios")).unwrap();
        fs::write(root.join("ios/Podfile.lock"), "a").unwrap();
        let a = target("path = \"ios/Pods\"\nkey = [\"ios/Podfile.lock\"]");
        let k = key(root, root, &a).unwrap();
        assert_eq!(k.len(), 16 + 1 + 8, "{k}");
        let b = target(
            "path = \"ios/Pods\"\nkey = [\"ios/Podfile.lock\"]\nkey_commands = [\"echo 1.2\"]",
        );
        let kb = key(root, root, &b).unwrap();
        assert_eq!(k[..16], kb[..16], "same files");
        assert_ne!(k[17..], kb[17..], "another tool");
        fs::write(root.join("ios/Podfile.lock"), "b").unwrap();
        assert_ne!(key(root, root, &a).unwrap()[..16], k[..16]);
        let c = target("path = \"ios/Pods\"\nkey = [\"ios/none\"]");
        assert!(key(root, root, &c).unwrap_err().contains("ios/none"));
        let d = target(
            "path = \"ios/Pods\"\nkey = [\"ios/Podfile.lock\"]\nkey_commands = [\"exit 3\"]",
        );
        assert!(key(root, root, &d).unwrap_err().contains("`exit 3` failed"));
    }

    #[test]
    fn installed_means_recorded_from_the_key_files_as_they_are() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("wt");
        fs::create_dir_all(root.join("ios/Pods")).unwrap();
        fs::write(root.join("ios/Podfile.lock"), "a").unwrap();
        let pods = target("path = \"ios/Pods\"\nkey = [\"ios/Podfile.lock\"]");
        // Half made: no record.
        assert!(!is_installed(&root, &pods));
        let k = key(&root, &root, &pods).unwrap();
        write_key(&root.join("ios/Pods"), &k);
        assert!(is_installed(&root, &pods));
        // The key files changed since.
        fs::write(root.join("ios/Podfile.lock"), "b").unwrap();
        assert!(!is_installed(&root, &pods));
        // Through a symlink out of the worktree: not this worktree's.
        let out = t.path().join("outside/Pods");
        fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(t.path().join("outside"), root.join("link")).unwrap();
        let linked = target("path = \"link/Pods\"\nkey = [\"ios/Podfile.lock\"]");
        write_key(&out, &key(&root, &root, &linked).unwrap());
        assert!(!is_installed(&root, &linked));
    }

    /// A worktree `wt` with `k` and a symlink `link` to `outside`, which
    /// holds `Pods/important.txt`.
    fn with_a_way_out() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let root = base.join("wt");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("k"), "a").unwrap();
        let outside = base.join("outside");
        fs::create_dir_all(outside.join("Pods")).unwrap();
        fs::write(outside.join("Pods/important.txt"), "not ours").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        (t, root, outside)
    }

    #[test]
    fn install_checks_where_path_is_right_before_it_moves_it() {
        // As if the symlink appeared after the targets were checked.
        let (t, root, outside) = with_a_way_out();
        let ctx = Ctx {
            root: root.clone(),
            main: root.clone(),
            worktrees: vec![root.clone()],
            cache_repo: t.path().join("cache/repo-01234567"),
            skip_lifecycle: Vec::new(),
        };
        let pods = target("path = \"link/Pods\"\nkey = [\"k\"]\nverify = \"false\"");
        for mode in [Mode::Missing, Mode::Changed, Mode::ChangedCloneOnly] {
            let e = install(&ctx, &pods, mode).unwrap_err();
            assert!(e.contains("outside the worktree"), "{e}");
            assert!(outside.join("Pods/important.txt").is_file());
        }
        let p = plan(&ctx, &pods, Mode::Changed, false);
        assert!(p.from.starts_with("nothing: path `link/Pods`"), "{p:?}");
        assert!(!is_installed(&root, &pods));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reclaim_leaves_what_is_out_of_the_worktree() {
        let (t, root, outside) = with_a_way_out();
        let cache_repo = t.path().join("cache/repo-01234567");
        let pods = target("path = \"link/Pods\"\nkey = [\"k\"]\nverify = \"true\"");
        let lines = reclaim(&root, &root, &root, &cache_repo, &[pods]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("outside the worktree"), "{lines:?}");
        assert!(outside.join("Pods/important.txt").is_file());
        assert!(!cache_repo.exists());
    }

    #[test]
    fn the_command_output_is_taken_in_dir() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        fs::create_dir_all(root.join("ios/sub")).unwrap();
        fs::write(root.join("k"), "a").unwrap();
        let in_ios = target("path = \"ios/Pods\"\nkey = [\"k\"]\nkey_commands = [\"pwd -P\"]");
        let in_sub = target(
            "path = \"ios/Pods\"\nkey = [\"k\"]\nkey_commands = [\"pwd -P\"]\ndir = \"ios/sub\"",
        );
        assert_ne!(key(root, root, &in_ios), key(root, root, &in_sub));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reclaim_moves_what_matches_its_key_into_the_cache() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("wt");
        let cache_repo = t.path().join("cache/repo-01234567");
        fs::create_dir_all(root.join("ios/Pods")).unwrap();
        fs::write(root.join("ios/Podfile.lock"), "a").unwrap();
        let pods = target("path = \"ios/Pods\"\nkey = [\"ios/Podfile.lock\"]");
        let npm = target::Target::with_defaults(&crate::ecosystem::npm::Npm, "ios");
        let both = [pods.clone(), npm];
        // No record, no verify: not known to be made from these key files.
        let lines = reclaim(&root, &root, &root, &cache_repo, &both);
        assert!(lines[0].contains("not kept in the cache"), "{lines:?}");
        assert!(root.join("ios/Pods").is_dir());
        let k = key(&root, &root, &pods).unwrap();
        write_key(&root.join("ios/Pods"), &k);
        let lines = reclaim(&root, &root, &root, &cache_repo, &both);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("cache  ios/Pods kept as "),
            "{lines:?}"
        );
        let e = entry(&cache_repo, &pods, &k);
        assert!(e.join(KEY_FILE).is_file());
        assert!(!root.join("ios/Pods").exists());
        assert_eq!(cache::source(&e), Some(root.clone()));
        // An entry with that key is there now: the next one stays.
        fs::create_dir_all(root.join("ios/Pods")).unwrap();
        write_key(&root.join("ios/Pods"), &k);
        assert!(reclaim(&root, &root, &root, &cache_repo, &both).is_empty());
        assert!(root.join("ios/Pods").is_dir());
        // verify vouches for a directory without a record.
        let verified =
            target("path = \"ios/Pods\"\nkey = [\"ios/Podfile.lock\"]\nverify = \"test -f ok\"");
        fs::write(root.join("ios/Podfile.lock"), "b").unwrap();
        fs::remove_file(root.join("ios/Pods").join(KEY_FILE)).unwrap();
        let lines = reclaim(
            &root,
            &root,
            &root,
            &cache_repo,
            std::slice::from_ref(&verified),
        );
        assert!(lines[0].ends_with(", and verify failed"), "{lines:?}");
        fs::write(root.join("ios/Pods/ok"), "").unwrap();
        let lines = reclaim(
            &root,
            &root,
            &root,
            &cache_repo,
            std::slice::from_ref(&verified),
        );
        assert!(
            lines[0].starts_with("cache  ios/Pods kept as "),
            "{lines:?}"
        );
    }
}
