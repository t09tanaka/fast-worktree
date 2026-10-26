//! A `[[clone]]` without a preset: a directory (`path`) that is reused
//! whole, such as CocoaPods' `Pods` or a build cache, decided by the
//! contents of its `key` files and the output of its `key_commands`.
//!
//! `path` is made present by the first of these that applies:
//!
//! 1. **Cache.** `<cache>/<repo>/<path>/<key>` holds the directory from an
//!    earlier install, or from a worktree that `remove` deleted ([`reclaim`]).
//! 2. **Another worktree** whose `key` files are byte-identical, whose
//!    `key_commands` print the same there, and where `verify` passes.
//! 3. `install`, run in `dir`. Its result is stored in the cache.
//!
//! A clone is one `clonefile(2)` call (macOS only; elsewhere only `install`
//! runs). After it, `verify` must pass in `path`, else the clone is thrown
//! away and the next source is tried; with `rewrite_paths`, the worktree it
//! was made in is replaced with this one in its text files; then the
//! `after` commands run. The key is `<16 hex digits of the key files>-<8
//! hex digits of the key commands' output>`, and is written to
//! `<path>/.fast-worktree-key`, so that `install --changed` and `remove`
//! can tell what the directory was made from.
//!
//! Unlike `node_modules`, the directory is not checked for symlinks with an
//! absolute target: one that points into the worktree it was made in
//! points there in a clone too. `rewrite_paths` leaves symlinks alone.

use super::{Ctx, Mode, Plan, Preset, Report, Target};
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
        root.join(&t.path).is_dir()
    }

    fn install(&self, ctx: &Ctx, t: &Target, mode: Mode) -> Result<Report, String> {
        install(ctx, t, mode)
    }

    fn plan(&self, ctx: &Ctx, t: &Target, mode: Mode, fresh: bool) -> Plan {
        plan(ctx, t, mode, fresh)
    }
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

/// Why the worktree `w` cannot lend its `path`, or `None` when it can.
fn not_a_donor(ctx: &Ctx, t: &Target, w: &Path, ours: &[String]) -> Option<String> {
    let name = util::tilde(w);
    let p = w.join(&t.path);
    if !p.is_dir() {
        return Some(format!("worktree {name}: no {}", t.path));
    }
    if let Err(k) = super::same_key_files(w, &ctx.root, &t.key) {
        return Some(format!("worktree {name}: {k} differs"));
    }
    if !t.key_commands.is_empty() && outputs(w, &ctx.main, t).ok().as_deref() != Some(ours) {
        return Some(format!(
            "worktree {name}: key_commands print something else there"
        ));
    }
    if let Some(v) = &t.verify {
        if !super::succeeds(&p, v, w, &ctx.main) {
            return Some(format!("worktree {name}: verify failed there"));
        }
    }
    None
}

/// Clone `src` (made in the worktree `made_in`) to `path`, then `verify`,
/// `rewrite_paths`, the key and `after`. On error nothing is left at
/// `path`.
fn clone_in(
    ctx: &Ctx,
    t: &Target,
    src: &Path,
    made_in: Option<PathBuf>,
    key: &str,
) -> Result<(), String> {
    let path = ctx.root.join(&t.path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    clone::clone_dir(src, &path).map_err(|e| format!("clone failed ({e})"))?;
    let _ = fs::remove_dir_all(path.join(".cache"));
    if let Some(v) = &t.verify {
        if !super::succeeds(&path, v, &ctx.root, &ctx.main) {
            let _ = trash::trash(&path);
            return Err("verify failed after the clone".into());
        }
    }
    if let (true, Some(from)) = (t.rewrite_paths, made_in) {
        super::rewrite_tree(&path, &[from], &ctx.root);
    }
    write_key(&path, key);
    Ok(())
}

fn run_after(ctx: &Ctx, t: &Target) -> Result<(), String> {
    let cwd = super::project_path(&ctx.root, &t.dir);
    for a in &t.after {
        super::run_quietly(&cwd, a, &ctx.root, &ctx.main)?;
    }
    Ok(())
}

fn install(ctx: &Ctx, t: &Target, mode: Mode) -> Result<Report, String> {
    let start = Instant::now();
    let name = t.name.as_str();
    let path = ctx.root.join(&t.path);
    let mut lines = Vec::new();
    let present = path.symlink_metadata().is_ok();
    if present && !mode.reinstalls_changed() {
        return Ok(Report::kept(format!("skip   {name} (already present)")));
    }
    let key = key(&ctx.root, &ctx.main, t)?;
    if present {
        match recorded(&path) {
            Some(k) if k == key => {
                return Ok(Report::kept(format!("skip   {name} (up to date)")));
            }
            // Not made by fast-worktree: kept while it passes `verify`.
            None if t
                .verify
                .as_ref()
                .is_none_or(|v| super::succeeds(&path, v, &ctx.root, &ctx.main)) =>
            {
                return Ok(Report::kept(format!("skip   {name} (already present)")));
            }
            _ => {}
        }
        trash::trash(&path).map_err(|e| format!("cannot move {} away: {e}", t.path))?;
        lines.push(format!("note   {name}: its key changed, making it again"));
    }
    let entry = entry(&ctx.cache_repo, t, &key);
    if clone::SUPPORTED {
        if entry.is_dir() {
            cache::touch(&entry);
            match clone_in(ctx, t, &entry, cache::source(&entry), &key) {
                Ok(()) => {
                    run_after(ctx, t)?;
                    lines.push(format!(
                        "clone  {name} from {} ({})",
                        util::tilde(&entry),
                        util::elapsed(start)
                    ));
                    return Ok(Report::done(lines.join("\n")));
                }
                Err(why) => lines.push(format!("note   {name}: {why}, not used")),
            }
        }
        let ours = outputs(&ctx.root, &ctx.main, t)?;
        for w in ctx.worktrees.iter().filter(|w| **w != ctx.root) {
            if not_a_donor(ctx, t, w, &ours).is_some() {
                continue;
            }
            let src = w.join(&t.path);
            match clone_in(ctx, t, &src, Some(w.clone()), &key) {
                Ok(()) => {
                    run_after(ctx, t)?;
                    lines.push(format!(
                        "clone  {name} from {} ({})",
                        util::tilde(&src),
                        util::elapsed(start)
                    ));
                    return Ok(Report::done(lines.join("\n")));
                }
                Err(why) => lines.push(format!("note   {name}: {why}, not used")),
            }
        }
    }
    if mode.clone_only() {
        return Ok(Report::missing(lines.join("\n")));
    }
    let Some(cmd) = &t.install else {
        lines.push(format!(
            "note   {name}: nothing to clone it from and no install to make it"
        ));
        return Ok(Report::done(lines.join("\n")));
    };
    let cwd = super::project_path(&ctx.root, &t.dir);
    super::run_quietly(&cwd, cmd, &ctx.root, &ctx.main)?;
    if !path.is_dir() {
        return Err(format!("`{cmd}` did not make {}", t.path));
    }
    if let Some(v) = &t.verify {
        if let Err(e) = super::run_quietly(&path, v, &ctx.root, &ctx.main) {
            let _ = trash::trash(&path);
            return Err(format!("verify failed after the install: {e}"));
        }
    }
    write_key(&path, &key);
    if clone::SUPPORTED {
        // Not checked for absolute symlinks (see the module's notes).
        if let Err(e) = cache::store(&path, &entry, true, &ctx.root) {
            lines.push(format!("note   {name}: cache not saved: {e}"));
        }
        if let Some(dir) = entry.parent() {
            cache::prune(dir, Duration::from_secs(cache::TTL_DAYS * 86_400));
        }
    }
    run_after(ctx, t)?;
    lines.push(format!("install {name} ({cmd}, {})", util::elapsed(start)));
    Ok(Report::done(lines.join("\n")))
}

fn plan(ctx: &Ctx, t: &Target, mode: Mode, fresh: bool) -> Plan {
    let path = ctx.root.join(&t.path);
    let mut why = Vec::new();
    let present = !fresh && path.symlink_metadata().is_ok();
    if present && !mode.reinstalls_changed() {
        return Plan::new("present", why);
    }
    let key = match key(&ctx.root, &ctx.main, t) {
        Ok(k) => k,
        Err(e) => return Plan::new(format!("nothing: {e}"), why),
    };
    if present {
        match recorded(&path) {
            Some(k) if k == key => return Plan::new("present (up to date)", why),
            None if t
                .verify
                .as_ref()
                .is_none_or(|v| super::succeeds(&path, v, &ctx.root, &ctx.main)) =>
            {
                return Plan::new("present", why);
            }
            _ => why.push("present: made from other key files".to_string()),
        }
    }
    let install = || match &t.install {
        Some(c) => format!("install ({c})"),
        None => "nothing (no install)".to_string(),
    };
    if !clone::SUPPORTED {
        why.push("clone: needs macOS clonefile(2)".to_string());
    } else {
        let entry = entry(&ctx.cache_repo, t, &key);
        if !entry.is_dir() {
            why.push(format!("cache: no entry {key}"));
        } else if let Some(v) = t
            .verify
            .as_ref()
            .filter(|v| !super::succeeds(&entry, v, &ctx.root, &ctx.main))
        {
            why.push(format!(
                "cache {}: verify `{v}` failed",
                util::tilde(&entry)
            ));
        } else {
            return Plan::new(format!("cache {}", util::tilde(&entry)), why);
        }
        let ours = match outputs(&ctx.root, &ctx.main, t) {
            Ok(o) => o,
            Err(e) => return Plan::new(format!("nothing: {e}"), why),
        };
        for w in ctx.worktrees.iter().filter(|w| fresh || **w != ctx.root) {
            match not_a_donor(ctx, t, w, &ours) {
                Some(no) => why.push(no),
                None => {
                    return Plan::new(format!("worktree {}", util::tilde(&w.join(&t.path))), why)
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

/// Before the worktree `root` is deleted, move the `path` of each of
/// `targets` without a preset into the cache, where the next worktree with
/// the same key clones it: when it is there, was made from the key files
/// it has now (its recorded key) or passes `verify`, and the cache has no
/// entry for that key yet. A rename on the same volume costs next to
/// nothing; one that fails (another volume) leaves it to be deleted with
/// the worktree. Only where directories can be cloned. Returns lines to
/// print.
pub fn reclaim(root: &Path, main: &Path, cache_repo: &Path, targets: &[Target]) -> Vec<String> {
    let mut lines = Vec::new();
    if !clone::SUPPORTED {
        return lines;
    }
    for t in targets.iter().filter(|t| !t.has_preset()) {
        let path = root.join(&t.path);
        if !path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let name = &t.name;
        let key = match key(root, main, t) {
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
                .is_some_and(|v| super::succeeds(&path, v, root, main));
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
        match cache::adopt(&path, &entry, root) {
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
        let lines = reclaim(&root, &root, &cache_repo, &both);
        assert!(lines[0].contains("not kept in the cache"), "{lines:?}");
        assert!(root.join("ios/Pods").is_dir());
        let k = key(&root, &root, &pods).unwrap();
        write_key(&root.join("ios/Pods"), &k);
        let lines = reclaim(&root, &root, &cache_repo, &both);
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
        assert!(reclaim(&root, &root, &cache_repo, &both).is_empty());
        assert!(root.join("ios/Pods").is_dir());
        // verify vouches for a directory without a record.
        let verified =
            target("path = \"ios/Pods\"\nkey = [\"ios/Podfile.lock\"]\nverify = \"test -f ok\"");
        fs::write(root.join("ios/Podfile.lock"), "b").unwrap();
        fs::remove_file(root.join("ios/Pods").join(KEY_FILE)).unwrap();
        let lines = reclaim(&root, &root, &cache_repo, std::slice::from_ref(&verified));
        assert!(lines[0].ends_with(", and verify failed"), "{lines:?}");
        fs::write(root.join("ios/Pods/ok"), "").unwrap();
        let lines = reclaim(&root, &root, &cache_repo, std::slice::from_ref(&verified));
        assert!(
            lines[0].starts_with("cache  ios/Pods kept as "),
            "{lines:?}"
        );
    }
}
