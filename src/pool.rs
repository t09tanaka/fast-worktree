//! The pool: worktrees made ahead of time, with their dependencies
//! installed, that `add` takes instead of creating one.
//!
//! Spares live in `<cache>/<repo>/pool/wt-<unix seconds>-<pid>-<n>`. Each is
//! `git worktree add --detach` of origin's default branch, then `install`,
//! then `git worktree lock --reason "fast-worktree pool"`, built under a
//! hidden name (`.new-<id>`) and renamed into place only when ready.
//!
//! Taking one is a rename to a hidden name of our own (`.claimed-<pid>-<id>`):
//! the rename either succeeds or finds the spare gone, so two `add`s never
//! take the same one. Then `git worktree repair` (git still records the old
//! path), `unlock`, and `git worktree move` to where the worktree belongs.
//! If the move fails (another file system, say), the spare is put back.
//!
//! `fill` holds `pool/fill.lock` (`flock`) while it works, so only one runs
//! per repository; a second one sees the lock and exits.
//!
//! The pool is off unless `[pool] size` is set, in the main checkout's
//! `.fast-worktree.toml` or in the user's `config.toml`.
//!
//! A spare is made in the pool and used somewhere else, so whatever its
//! install recorded by absolute path points into the pool. A `node_modules`
//! with a symlink whose target is absolute is therefore not kept in a spare
//! (the `add` that takes it installs that project again), and after the
//! move the projects' own install steps run again where the worktree now is
//! ([`crate::ecosystem::Ecosystem::rerun_own_scripts`]).

use crate::cmd::install;
use crate::ecosystem::{self, Mode};
use crate::error::{bail, Error, Result};
use crate::hooks::{self, Config};
use crate::{cache, clone, git, paths, pending, trash, util};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const LOCK_REASON: &str = "fast-worktree pool";
/// Spares are named `wt-<unix seconds>-<pid>-<n>`.
pub const SPARE_PREFIX: &str = "wt-";
const BUILDING_PREFIX: &str = ".new-";
const CLAIMED_PREFIX: &str = ".claimed-";
const MISSES: &str = "misses";
const FILL_LOCK: &str = "fill.lock";
pub const FILL_LOG: &str = "fill.log";
/// `fill.log` is started afresh once it grows past this.
const LOG_LIMIT: u64 = 1 << 20;
/// Hidden entries left by a run that was killed are cleared by `pool
/// clear` once they are this old.
const LEFTOVER_SECS: u64 = 3_600;
/// How long `pool fill` lets the fetch of the default branch take.
const FETCH_LIMIT: Duration = Duration::from_secs(60);

/// The pool directory of the repository whose main checkout is `main`.
pub fn dir(main: &Path) -> PathBuf {
    paths::cache_root().join(paths::repo_key(main)).join("pool")
}

/// Whether `path` (a real path) is a spare, or one being made or taken.
pub fn is_pool_path(path: &Path) -> bool {
    let root = paths::cache_root();
    let root = root.canonicalize().unwrap_or(root);
    path.starts_with(&root)
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == "pool")
}

/// Where the size comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Repo,
    User,
    Default,
}

impl Source {
    fn describe(self) -> String {
        match self {
            Source::Repo => format!("[pool] in the main checkout's {}", hooks::FILE),
            Source::User => format!("[pool] in {}", util::tilde(&hooks::user_config_path())),
            Source::Default => "not set".to_string(),
        }
    }
}

/// The configured size: `[pool] size` in the main checkout's
/// `.fast-worktree.toml`, else in the user's config, else 0 (off). A file
/// that cannot be read is reported and counts as unset.
pub fn size(main: &Path) -> (u32, Source) {
    let repo = Config::load(main)
        .map_err(|e| eprintln!("fast-worktree: {e}"))
        .ok()
        .and_then(|c| c.pool)
        .and_then(|p| p.size);
    if let Some(n) = repo {
        return (n, Source::Repo);
    }
    match hooks::user_pool_size() {
        Ok(Some(n)) => (n, Source::User),
        Ok(None) => (0, Source::Default),
        Err(e) => {
            eprintln!("fast-worktree: {e}");
            (0, Source::Default)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spare {
    pub path: PathBuf,
    pub id: String,
    /// Unix seconds, from the id.
    pub created: u64,
}

fn created_of(id: &str) -> Option<u64> {
    id.strip_prefix(SPARE_PREFIX)?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// The ready spares, newest first.
pub fn spares(pool: &Path) -> Vec<Spare> {
    let mut v: Vec<Spare> = fs::read_dir(pool)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let id = e.file_name().to_string_lossy().into_owned();
            let created = created_of(&id)?;
            Some(Spare {
                path: e.path(),
                id,
                created,
            })
        })
        .collect();
    v.sort_by(|a, b| (b.created, &b.id).cmp(&(a.created, &a.id)));
    v
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{SPARE_PREFIX}{}-{}-{}",
        now_secs(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// What [`take`] got.
#[derive(Debug)]
pub enum Taken {
    /// The spare is now the worktree at the target; `commit` is the commit
    /// it was made at.
    Spare { commit: String },
    /// No spare was ready.
    Empty,
    /// A spare was found but could not be moved; it was put back.
    Failed(String),
}

/// Take a spare and move it to `target` (which must not exist). The
/// worktree there is detached at the spare's commit.
pub fn take(main: &Path, target: &Path) -> Taken {
    let pool = dir(main);
    for s in spares(&pool) {
        let claimed = pool.join(format!("{CLAIMED_PREFIX}{}-{}", std::process::id(), s.id));
        if fs::rename(&s.path, &claimed).is_err() {
            continue; // another `add` took it first
        }
        return match settle(main, &claimed, target) {
            Ok(commit) => Taken::Spare { commit },
            Err(why) => {
                put_back(main, &claimed, &s.path);
                Taken::Failed(why.to_string())
            }
        };
    }
    Taken::Empty
}

/// Tell git where the claimed spare is, unlock it and move it to `target`.
/// Returns its commit.
fn settle(main: &Path, claimed: &Path, target: &Path) -> Result<String> {
    let claimed_s = claimed.to_string_lossy();
    git::output(main, &["worktree", "repair", &claimed_s])?;
    let commit = git::output(claimed, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    git::output(main, &["worktree", "unlock", &claimed_s])?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Err(e) = git::output(
        main,
        &["worktree", "move", &claimed_s, &target.to_string_lossy()],
    ) {
        let _ = git::output(
            main,
            &["worktree", "lock", "--reason", LOCK_REASON, &claimed_s],
        );
        return Err(e);
    }
    Ok(commit)
}

/// Undo a claim: lock again and rename back.
fn put_back(main: &Path, claimed: &Path, spare: &Path) {
    let claimed_s = claimed.to_string_lossy();
    let _ = git::output(
        main,
        &["worktree", "lock", "--reason", LOCK_REASON, &claimed_s],
    );
    if fs::rename(claimed, spare).is_ok() {
        let _ = git::output(main, &["worktree", "repair", &spare.to_string_lossy()]);
    }
}

/// Note that an `add` found no spare.
pub fn record_miss(main: &Path) {
    let pool = dir(main);
    let _ = fs::create_dir_all(&pool);
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pool.join(MISSES))
    {
        let _ = writeln!(f, "{}", now_secs());
    }
}

/// How many times an `add` found no spare.
pub fn misses(pool: &Path) -> usize {
    fs::read_to_string(pool.join(MISSES))
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

/// Start `fast-worktree pool fill` in the background, detached, with its
/// output in `pool/fill.log`.
pub fn spawn_fill(main: &Path) {
    let pool = dir(main);
    if let Err(e) = try_spawn_fill(main, &pool) {
        println!("note   pool: cannot start `pool fill`: {e}");
    }
}

fn try_spawn_fill(main: &Path, pool: &Path) -> io::Result<()> {
    fs::create_dir_all(pool)?;
    let log_path = pool.join(FILL_LOG);
    let fresh = fs::metadata(&log_path).is_ok_and(|m| m.len() > LOG_LIMIT);
    let log = OpenOptions::new()
        .create(true)
        .append(!fresh)
        .write(true)
        .truncate(fresh)
        .open(&log_path)?;
    let exe = std::env::current_exe()?;
    let mut c = Command::new(exe);
    c.args(["pool", "fill"])
        .current_dir(main)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    pending::detach(&mut c);
    for v in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_PREFIX"] {
        c.env_remove(v);
    }
    c.spawn().map(|_| ())
}

/// An exclusive `flock` on `pool/fill.lock`, held while the value lives.
pub struct FillLock(#[allow(dead_code)] File);

/// Take the fill lock without waiting. `None` when another process has it.
pub fn try_lock(pool: &Path) -> Result<Option<FillLock>> {
    fs::create_dir_all(pool)?;
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(pool.join(FILL_LOCK))?;
    // SAFETY: a valid open descriptor; the lock goes with it when the file
    // is closed.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(FillLock(f)))
    } else {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(Error(format!("{FILL_LOCK}: {e}")))
        }
    }
}

/// The main checkout of the repository in the current directory.
fn main_here() -> Result<PathBuf> {
    install::main_checkout(&git::toplevel(Path::new("."))?)
}

fn stamp() -> String {
    cache::datetime(SystemTime::now())
}

/// Fetch origin's default branch and return `origin/<branch>`. A failed
/// fetch is reported and the ref as last fetched is used.
fn fetched_base(main: &Path) -> Result<String> {
    let base = git::origin_default_branch(main)?;
    if let Err(e) = git::fetch_quietly(main, &base, Some(FETCH_LIMIT)) {
        println!("note   pool: fetch failed, using origin/{base} as last fetched: {e}");
    }
    Ok(format!("origin/{base}"))
}

/// `pool fill`: make spares until there are `size`.
pub fn fill() -> Result<i32> {
    let main = main_here()?;
    let (size, _) = size(&main);
    if size == 0 {
        println!("the pool is off: set [pool] size in {}", hooks::FILE);
        return Ok(0);
    }
    let pool = dir(&main);
    let Some(_lock) = try_lock(&pool)? else {
        println!("{} fill  another fill is running", stamp());
        return Ok(0);
    };
    println!("{} fill  started", stamp());
    let base = fetched_base(&main)?;
    fill_locked(&main, &pool, size, &base)
}

fn fill_locked(main: &Path, pool: &Path, size: u32, base: &str) -> Result<i32> {
    while spares(pool).len() < size as usize {
        if let Err(e) = make_spare(main, pool, base) {
            println!("{} fill  failed: {e}", stamp());
            return Ok(1);
        }
    }
    println!(
        "{} fill  done: {} of {size} ready",
        stamp(),
        spares(pool).len()
    );
    Ok(0)
}

/// Make one spare at `base`.
fn make_spare(main: &Path, pool: &Path, base: &str) -> Result<()> {
    let start = Instant::now();
    let id = new_id();
    let building = pool.join(format!("{BUILDING_PREFIX}{id}"));
    let b = building.to_string_lossy().into_owned();
    // FAST_WORKTREE_SKIP: a post-checkout hook running fast-worktree stays
    // out of the way; the install below does the work.
    git::output_with_env(
        main,
        &["worktree", "add", "-q", "--detach", &b, base],
        &[("FAST_WORKTREE_SKIP", "1")],
    )?;
    let installed = Config::load(&building)
        .and_then(|cfg| install::install_deps(&building, &[], &cfg, Mode::Missing))
        .map(|i| i.ok);
    if !matches!(installed, Ok(true)) {
        discard(main, &building);
        match installed {
            Err(e) => bail!("installing into a new spare: {e}"),
            _ => bail!("installing into a new spare failed"),
        }
    }
    drop_absolute_symlinks(&building);
    git::output(main, &["worktree", "lock", "--reason", LOCK_REASON, &b])?;
    let ready = pool.join(&id);
    fs::rename(&building, &ready)?;
    git::output(main, &["worktree", "repair", &ready.to_string_lossy()])?;
    let short = git::output(&ready, &["rev-parse", "--short", "HEAD"])?;
    println!(
        "{} spare {id} at {} ready ({})",
        stamp(),
        short.trim(),
        util::elapsed(start)
    );
    Ok(())
}

/// Move away the `node_modules` of the spare at `root` that have a symlink
/// with an absolute target: it points into the pool, where the spare will
/// not be. The `add` that takes the spare installs those projects again.
fn drop_absolute_symlinks(root: &Path) {
    for p in ecosystem::discover(root).unwrap_or_default() {
        let nm = ecosystem::project_path(root, &p.dir).join("node_modules");
        if nm.is_dir() && clone::has_absolute_symlink(&nm) {
            let _ = trash::trash(&nm);
            println!(
                "{} note  {}: absolute symlink in node_modules, not kept in the spare",
                stamp(),
                p.dir
            );
        }
    }
}

/// Remove a spare (or what is left of one) and unregister it, and only it.
fn discard(main: &Path, path: &Path) {
    // It may have been renamed since git last saw it: tell git, so that
    // its entry can be found from where it is now.
    let _ = git::output(main, &["worktree", "repair", &path.to_string_lossy()]);
    let admin = git::admin_dir(path)
        .ok()
        .or_else(|| git::find_admin(main, path).ok().flatten());
    if trash::trash(path).is_err() {
        return;
    }
    match admin.map(|a| git::unregister(&a)) {
        Some(Ok(())) => {}
        Some(Err(e)) => println!("note   pool: {e}"),
        // A spare whose `git worktree add` never finished has no entry.
        None => {}
    }
}

/// Claim a spare and discard it. False when another process took it first.
fn claim_and_discard(main: &Path, pool: &Path, s: &Spare) -> bool {
    let claimed = pool.join(format!("{CLAIMED_PREFIX}{}-{}", std::process::id(), s.id));
    if fs::rename(&s.path, &claimed).is_err() {
        return false;
    }
    discard(main, &claimed);
    true
}

/// How many commits `commit` is behind `base`, if git can tell.
fn behind(main: &Path, commit: &str, base: &str) -> Option<u64> {
    git::output(main, &["rev-list", "--count", &format!("{commit}..{base}")])
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn head_of(spare: &Path) -> Option<String> {
    git::output(spare, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
}

/// `pool status`.
pub fn status() -> Result<i32> {
    let main = main_here()?;
    let (size, source) = size(&main);
    let pool = dir(&main);
    let ready = spares(&pool);
    if size == 0 {
        println!("pool   off (size {})", source.describe());
    } else {
        println!(
            "pool   {} of {size} ready (size from {})",
            ready.len(),
            source.describe()
        );
    }
    if !ready.is_empty() {
        let base = git::origin_default_branch(&main)
            .map(|b| format!("origin/{b}"))
            .ok();
        let rows: Vec<[String; 4]> = ready
            .iter()
            .map(|s| {
                let head = head_of(&s.path);
                let short = head
                    .as_deref()
                    .map(|h| h[..h.len().min(7)].to_string())
                    .unwrap_or_else(|| "?".into());
                let behind = match (&head, &base) {
                    (Some(h), Some(b)) => behind(&main, h, b)
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "?".into()),
                    _ => "?".into(),
                };
                let created = cache::datetime(UNIX_EPOCH + Duration::from_secs(s.created)) + " UTC";
                [s.id.clone(), short, behind, created]
            })
            .collect();
        let behind_head = match &base {
            Some(b) => format!("BEHIND {b}"),
            None => "BEHIND".to_string(),
        };
        print!(
            "{}",
            crate::cmd::cache::table(&["ID", "COMMIT", &behind_head, "CREATED"], &rows)
        );
    }
    let fill_running = matches!(try_lock(&pool), Ok(None));
    println!("misses {} (an add found no spare ready)", misses(&pool));
    if fill_running {
        println!(
            "fill   running (log: {})",
            util::tilde(&pool.join(FILL_LOG))
        );
    }
    Ok(0)
}

/// `pool refresh`: fetch, discard the spares behind origin's default
/// branch, and fill again.
pub fn refresh() -> Result<i32> {
    let main = main_here()?;
    let (size, _) = size(&main);
    let pool = dir(&main);
    let Some(_lock) = try_lock(&pool)? else {
        bail!("a fill is running; try again when it is done");
    };
    let base = fetched_base(&main)?;
    let mut discarded = 0;
    for s in spares(&pool) {
        let stale = match head_of(&s.path) {
            Some(h) => behind(&main, &h, &base) != Some(0),
            None => true,
        };
        if stale && claim_and_discard(&main, &pool, &s) {
            println!("drop   {} (behind {base})", s.id);
            discarded += 1;
        }
    }
    println!("dropped {discarded} spare(s) behind {base}");
    if size == 0 {
        println!("the pool is off: set [pool] size in {}", hooks::FILE);
        return Ok(0);
    }
    fill_locked(&main, &pool, size, &base)
}

/// `pool clear`: remove every spare, and what killed runs left behind.
pub fn clear() -> Result<i32> {
    let main = main_here()?;
    let pool = dir(&main);
    let mut n = 0;
    for s in spares(&pool) {
        if claim_and_discard(&main, &pool, &s) {
            n += 1;
        }
    }
    let now = now_secs();
    for e in fs::read_dir(&pool).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let rest = name.strip_prefix(BUILDING_PREFIX).or_else(|| {
            // `.claimed-<pid>-wt-...`
            name.strip_prefix(CLAIMED_PREFIX)
                .and_then(|r| r.split_once('-'))
                .map(|(_, id)| id)
        });
        if let Some(created) = rest.and_then(created_of) {
            if created + LEFTOVER_SECS < now {
                discard(&main, &e.path());
            }
        }
    }
    println!("removed {n} spare(s) (deleting in background)");
    if matches!(try_lock(&pool), Ok(None)) {
        println!("note   a fill is running and may add spares again");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spares_are_ready_dirs_newest_first() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        for d in [
            "wt-100-1-0",
            "wt-300-1-0",
            "wt-200-9-3",
            ".new-wt-400-1-0",
            ".claimed-5-wt-500-1-0",
            "wt-x",
        ] {
            fs::create_dir(p.join(d)).unwrap();
        }
        fs::write(p.join("wt-600-1-0"), "a file").unwrap();
        fs::write(p.join(MISSES), "1\n2\n").unwrap();
        let ids: Vec<String> = spares(p).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["wt-300-1-0", "wt-200-9-3", "wt-100-1-0"]);
        assert_eq!(spares(p)[0].created, 300);
        assert_eq!(misses(p), 2);
        assert_eq!(misses(&p.join("none")), 0);
    }

    #[test]
    fn ids_are_unique_and_dated() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert!(created_of(&a).is_some_and(|t| t + 5 > now_secs()));
    }

    #[test]
    fn the_fill_lock_is_exclusive() {
        let t = tempfile::tempdir().unwrap();
        let held = try_lock(t.path()).unwrap();
        assert!(held.is_some());
        // flock is per open file: a second open in this process conflicts.
        assert!(try_lock(t.path()).unwrap().is_none());
        drop(held);
        // A child another test is forking right now may hold a copy of the
        // descriptor until it execs.
        let start = Instant::now();
        while try_lock(t.path()).unwrap().is_none() {
            assert!(start.elapsed() < Duration::from_secs(5), "still locked");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
