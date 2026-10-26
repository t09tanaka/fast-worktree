//! The dependency cache.
//!
//! An entry is a directory `<cache>/<repo>/<project>/<key>` holding one
//! installed `node_modules` (or, for a `[[clone]]` without a preset, its
//! `path`, under `<cache>/<repo>/<path>/<key>`). Next to it, `<key>.ok`
//! records that the tree was checked for absolute symlinks when it was
//! stored, so cloning it does not walk every file again, and `<key>.src`
//! the worktree it was made in, for `rewrite_paths`. These live outside the
//! entry so that they are never cloned into a worktree. They are written
//! before the entry is renamed into place, under a lock ([`STORE_LOCK`])
//! that a second run storing the same entry waits for, so that it finds
//! the entry there and leaves the first run's records alone.
//!
//! An entry's modification time is its "last used": it is set when the
//! entry is stored and each time it is cloned.

use crate::{clone, paths, trash};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Entries unused for this long are removed by `install` and by `cache
/// prune` without `--older-than`.
pub const TTL_DAYS: u64 = 7;

const MARKER_SUFFIX: &str = ".ok";
const SOURCE_SUFFIX: &str = ".src";
const TMP_INFIX: &str = ".tmp-";

/// The lock, in a project's directory of the cache, held from the check
/// that an entry is not there to its rename into place, and while records
/// without an entry are removed: so that no run writes or removes the
/// records of an entry that another run has just put there. Its name
/// starts with a dot and it is a file: never taken for an entry.
const STORE_LOCK: &str = ".store.lock";

/// An exclusive `flock` on the [`STORE_LOCK`] of `dir`, held while the
/// value lives. `None` when it cannot be taken (a file system without
/// locks), and then nothing waits.
fn store_lock(dir: &Path) -> Option<fs::File> {
    use std::os::unix::io::AsRawFd;
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(STORE_LOCK))
        .ok()?;
    loop {
        // SAFETY: a valid open descriptor; the lock goes when it is closed.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Some(f);
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
}

/// Where the "checked" marker of `entry` lives.
pub fn marker(entry: &Path) -> PathBuf {
    let mut s = entry.as_os_str().to_owned();
    s.push(MARKER_SUFFIX);
    PathBuf::from(s)
}

/// Whether `entry` was checked for absolute symlinks when it was stored.
pub fn is_checked(entry: &Path) -> bool {
    marker(entry).is_file()
}

/// Record that `entry` holds no absolute symlink.
pub fn mark_checked(entry: &Path) {
    let _ = fs::write(marker(entry), b"");
}

/// Where the record of the worktree `entry` was made in lives.
fn source_record(entry: &Path) -> PathBuf {
    let mut s = entry.as_os_str().to_owned();
    s.push(SOURCE_SUFFIX);
    PathBuf::from(s)
}

/// Record that `entry` was made in the worktree `root`.
pub fn record_source(entry: &Path, root: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let _ = fs::write(source_record(entry), root.as_os_str().as_bytes());
}

/// The worktree `entry` was made in, when that was recorded.
pub fn source(entry: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = fs::read(source_record(entry)).ok()?;
    (!bytes.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
}

/// A directory name that is an entry, not a marker, a half-made entry, or
/// a spare of the pool (which lives in `<repo>/pool`, where a project
/// directory named `pool` would keep its entries too).
fn is_entry_name(name: &str) -> bool {
    !name.ends_with(MARKER_SUFFIX)
        && !name.ends_with(SOURCE_SUFFIX)
        && !name.contains(TMP_INFIX)
        && !name.starts_with('.')
        && !name.starts_with(crate::pool::SPARE_PREFIX)
}

/// Clone `node_modules` into the cache at `entry`, unless it is there
/// already. Built under a temporary name and renamed, so a concurrent run
/// never sees a half-made entry. `checked` says the tree was already found
/// free of absolute symlinks (or need not be); otherwise it is checked
/// here, once, and a tree that has one is not cached (it may point into its
/// worktree). `made_in` is the worktree it was made in.
pub fn store(
    node_modules: &Path,
    entry: &Path,
    checked: bool,
    made_in: &Path,
) -> Result<(), String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if entry.exists() {
        return Ok(());
    }
    if !checked && clone::has_absolute_symlink(node_modules) {
        return Err("absolute symlink in node_modules".into());
    }
    let parent = entry.parent().ok_or("no parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let name = entry.file_name().ok_or("no name")?.to_string_lossy();
    let tmp = parent.join(format!(
        "{name}{TMP_INFIX}{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    clone::clone_dir(node_modules, &tmp).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(tmp.join(".cache"));
    touch(&tmp);
    let _lock = store_lock(parent);
    // The records first: an entry is never seen without them. Under the
    // lock, a run that comes second finds the entry here and writes none.
    if entry.exists() {
        let _ = trash::trash(&tmp);
        return Ok(());
    }
    mark_checked(entry);
    record_source(entry, made_in);
    if fs::rename(&tmp, entry).is_err() {
        let _ = trash::trash(&tmp);
        forget_records(entry);
    }
    Ok(())
}

/// Remove the records of `entry` when there is no such entry: a rename
/// that did not happen.
fn forget_records(entry: &Path) {
    if !entry.exists() {
        let _ = fs::remove_file(marker(entry));
        let _ = fs::remove_file(source_record(entry));
    }
}

/// Move the directory `dir` into the cache as `entry`, by rename, unless
/// there is an entry already. `made_in` is the worktree it comes from. An
/// error when it cannot be renamed there (another volume), in which case it
/// stays where it is.
///
/// Everything is done to it before the rename (the records next to the
/// entry, its `.cache` removed, its time of last use), so that another
/// process that finds the entry finds it complete.
pub fn adopt(dir: &Path, entry: &Path, made_in: &Path) -> Result<bool, String> {
    if entry.exists() {
        return Ok(false);
    }
    let parent = entry.parent().ok_or("no parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(dir.join(".cache"));
    let _lock = store_lock(parent);
    // As in `store`: another run may have put it there since.
    if entry.exists() {
        return Ok(false);
    }
    mark_checked(entry);
    record_source(entry, made_in);
    touch(dir);
    if let Err(e) = fs::rename(dir, entry) {
        forget_records(entry);
        return Err(e.to_string());
    }
    Ok(true)
}

/// The most recently used entry in `project_dir` (one project of one
/// repository) for this Node version: the base for an incremental install
/// when no entry matches the lockfile. `except` (the entry that matches,
/// when it could not be used) is never chosen.
pub fn latest(project_dir: &Path, node_version: &str, except: &Path) -> Option<PathBuf> {
    let suffix = format!("-{}", node_version.trim());
    fs::read_dir(project_dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            is_entry_name(&n) && n.ends_with(&suffix) && e.path() != except && e.path().is_dir()
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// Remove the entries in `project_dir` not used for longer than `ttl`, with
/// their markers, and markers left without an entry. Returns the removed
/// entries.
pub fn prune(project_dir: &Path, ttl: Duration) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(project_dir) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut removed = Vec::new();
    let mut markers = Vec::new();
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_dir() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(MARKER_SUFFIX) || name.ends_with(SOURCE_SUFFIX) {
                markers.push(e.path());
            }
            continue;
        }
        if !is_entry_name(&e.file_name().to_string_lossy()) {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        if age > ttl && trash::trash(&e.path()).is_ok() {
            removed.push(e.path());
        }
    }
    let _lock = (!markers.is_empty()).then(|| store_lock(project_dir));
    for m in markers {
        let s = m.to_string_lossy();
        let entry = Path::new(
            s.strip_suffix(MARKER_SUFFIX)
                .or_else(|| s.strip_suffix(SOURCE_SUFFIX))
                .unwrap_or(&s),
        );
        if !entry.is_dir() {
            let _ = fs::remove_file(&m);
        }
    }
    removed
}

/// Set a directory's modification time to now: the cache's "last used".
pub fn touch(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: a valid NUL-terminated path; a null `times` means "now".
        unsafe {
            libc::utimes(c.as_ptr(), std::ptr::null());
        }
    }
}

/// One cache entry, for `cache ls`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `<name>-<hash>` of the repository's main checkout.
    pub repo: String,
    /// The project directory (`.` for the repository root).
    pub dir: String,
    /// `<lockfile sha256, 16 digits>-<node -v>`.
    pub key: String,
    pub path: PathBuf,
    pub last_used: SystemTime,
}

/// The directories in `p`, by name.
fn dirs(p: &Path) -> Vec<(String, PathBuf)> {
    fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect()
}

/// Whether a directory in the cache root is a repository's, as
/// [`paths::repo_key`] names them: `<name>-<8 hex digits>`.
pub fn is_repo_name(name: &str) -> bool {
    name.rsplit_once('-').is_some_and(|(n, h)| {
        !n.is_empty() && h.len() == 8 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Whether an entry's name is a key, as [`crate::ecosystem::npm::cache_key`]
/// makes them, `<16 hex digits>-<node -v>`, and a `[[clone]]` without a
/// preset, `<16 hex digits>-<8 hex digits>`.
fn is_key_name(name: &str) -> bool {
    is_entry_name(name)
        && name.split_once('-').is_some_and(|(h, v)| {
            h.len() == 16
                && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                && !v.is_empty()
        })
}

/// Every entry under the cache root `root`, sorted by repository, project
/// and key. Directories of an older layout ([`legacy`]) are left out.
pub fn list(root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    for (repo, repo_path) in dirs(root) {
        if !is_repo_name(&repo) {
            continue;
        }
        for (slug, project_path) in dirs(&repo_path) {
            for (key, path) in dirs(&project_path) {
                if !is_key_name(&key) {
                    continue;
                }
                let last_used = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                out.push(Entry {
                    repo: repo.clone(),
                    dir: paths::project_dir(&slug),
                    key,
                    path,
                    last_used,
                });
            }
        }
    }
    out.sort_by(|a, b| (&a.repo, &a.dir, &a.key).cmp(&(&b.repo, &b.dir, &b.key)));
    out
}

/// A directory in the cache root that no version since 0.2.0 reads: one
/// that a repository's cache had before its name got the hash of the main
/// checkout's path (and whose entries for the root project sat right in
/// it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legacy {
    pub name: String,
    pub path: PathBuf,
    pub last_used: SystemTime,
}

/// The [`Legacy`] directories under the cache root `root`, by name.
pub fn legacy(root: &Path) -> Vec<Legacy> {
    let mut out: Vec<Legacy> = dirs(root)
        .into_iter()
        .filter(|(n, _)| n != "trash" && !n.starts_with('.') && !is_repo_name(n))
        .map(|(name, path)| Legacy {
            last_used: fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH),
            name,
            path,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Move every [`legacy`] directory under `root` to the trash. Returns the
/// ones moved.
pub fn prune_legacy(root: &Path) -> Vec<Legacy> {
    legacy(root)
        .into_iter()
        .filter(|l| trash::trash(&l.path).is_ok())
        .collect()
}

/// Remove every entry under `root` not used for longer than `ttl`.
pub fn prune_all(root: &Path, ttl: Duration) -> Vec<Entry> {
    let entries = list(root);
    let mut projects: Vec<PathBuf> = entries
        .iter()
        .filter_map(|e| e.path.parent().map(Path::to_path_buf))
        .collect();
    projects.dedup();
    let mut removed = Vec::new();
    for p in projects {
        removed.extend(prune(&p, ttl));
    }
    entries
        .into_iter()
        .filter(|e| removed.contains(&e.path))
        .collect()
}

/// `YYYY-MM-DD HH:MM` (UTC) of a time.
pub fn datetime(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = secs % 86_400;
    format!("{} {:02}:{:02}", date(t), s / 3_600, s % 3_600 / 60)
}

/// `YYYY-MM-DD` (UTC) of a time.
pub fn date(t: SystemTime) -> String {
    let days = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0) as i64;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_sit_next_to_the_entry() {
        assert_eq!(
            marker(Path::new("/c/r/app/0123456789abcdef-v20.13.1")),
            Path::new("/c/r/app/0123456789abcdef-v20.13.1.ok")
        );
        assert!(is_entry_name("0123456789abcdef-v20.13.1"));
        assert!(!is_entry_name("0123456789abcdef-v20.13.1.ok"));
        assert!(!is_entry_name("0123456789abcdef-v20.13.1.tmp-12-0"));
        assert!(!is_entry_name("wt-1793012885-1-0"), "a spare of the pool");
    }

    #[test]
    fn latest_is_the_most_recently_used_entry_of_that_node_version() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        for n in ["aaaa-v20", "bbbb-v20", "cccc-v22", "dddd-v20.tmp-1-0"] {
            fs::create_dir(p.join(n)).unwrap();
        }
        fs::write(p.join("eeee-v20"), "a file, not an entry").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        touch(&p.join("aaaa-v20"));
        std::thread::sleep(Duration::from_millis(20));
        touch(&p.join("cccc-v22"));
        touch(&p.join("dddd-v20.tmp-1-0"));
        let none = Path::new("");
        assert_eq!(latest(p, "v20\n", none), Some(p.join("aaaa-v20")));
        assert_eq!(
            latest(p, "v20", &p.join("aaaa-v20")),
            Some(p.join("bbbb-v20"))
        );
        assert_eq!(latest(p, "v22", none), Some(p.join("cccc-v22")));
        assert_eq!(latest(p, "v2", none), None, "the whole version must match");
        assert_eq!(latest(&p.join("nope"), "v20", none), None);
    }

    #[test]
    fn prune_removes_old_entries_with_their_markers() {
        let t = tempfile::tempdir().unwrap();
        let cache = t.path().join("cache");
        std::env::set_var("FAST_WORKTREE_CACHE", &cache);
        let proj = cache.join("repo/app");
        fs::create_dir_all(proj.join("old-v20.1")).unwrap();
        fs::create_dir_all(proj.join("new-v20.1")).unwrap();
        fs::create_dir_all(proj.join("wt-1-1-0")).unwrap();
        mark_checked(&proj.join("old-v20.1"));
        mark_checked(&proj.join("new-v20.1"));
        record_source(&proj.join("old-v20.1"), Path::new("/wt/a"));
        record_source(&proj.join("new-v20.1"), Path::new("/wt/b"));
        fs::write(proj.join("gone-v20.1.ok"), "").unwrap();
        fs::write(proj.join("gone-v20.1.src"), "/x").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        touch(&proj.join("new-v20.1"));
        let removed = prune(&proj, Duration::from_secs(1));
        assert_eq!(removed, vec![proj.join("old-v20.1")]);
        assert!(!proj.join("old-v20.1").exists());
        assert!(!proj.join("old-v20.1.ok").exists());
        assert!(!proj.join("gone-v20.1.ok").exists(), "orphan marker");
        assert!(!proj.join("gone-v20.1.src").exists(), "orphan record");
        assert!(!proj.join("old-v20.1.src").exists());
        assert_eq!(
            source(&proj.join("new-v20.1")),
            Some(PathBuf::from("/wt/b"))
        );
        assert_eq!(source(&proj.join("none")), None);
        assert!(proj.join("new-v20.1").exists());
        assert!(proj.join("wt-1-1-0").exists(), "spares are not entries");
        assert!(is_checked(&proj.join("new-v20.1")));
    }

    #[test]
    fn list_reads_repo_project_and_key() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let k1 = "0123456789abcdef-v20";
        let k2 = "fedcba9876543210-v20";
        for p in [
            format!("app-1234abcd/_root/{k1}"),
            format!("app-1234abcd/packages__web/{k2}"),
            format!("app-1234abcd/packages__web/{k2}.tmp-1-0"),
            "app-1234abcd/pool/wt-1-1-0".to_string(),
            "trash/123-1-1-wt".to_string(),
            // An older layout: no hash in the repository's name, the root
            // project's entries right in it.
            format!("app/{k1}/some-package"),
            format!("app/web/{k2}"),
        ] {
            fs::create_dir_all(root.join(p)).unwrap();
        }
        mark_checked(&root.join("app-1234abcd/_root").join(k1));
        let got: Vec<(String, String, String)> = list(root)
            .into_iter()
            .map(|e| (e.repo, e.dir, e.key))
            .collect();
        assert_eq!(
            got,
            vec![
                ("app-1234abcd".into(), ".".into(), k1.into()),
                ("app-1234abcd".into(), "packages/web".into(), k2.into()),
            ]
        );
        let old: Vec<String> = legacy(root).into_iter().map(|l| l.name).collect();
        assert_eq!(old, vec!["app".to_string()]);
        assert_eq!(prune_legacy(root).len(), 1);
        assert!(!root.join("app").exists());
        assert!(root.join("app-1234abcd").exists());
    }

    #[test]
    fn adopt_moves_a_directory_in_unless_the_entry_exists() {
        let t = tempfile::tempdir().unwrap();
        let wt = t.path().join("wt");
        fs::create_dir_all(wt.join("Pods/.cache")).unwrap();
        fs::write(wt.join("Pods/Manifest.lock"), "m").unwrap();
        let entry = t.path().join("c/repo/Pods/0123456789abcdef-01234567");
        assert_eq!(adopt(&wt.join("Pods"), &entry, &wt), Ok(true));
        assert!(!wt.join("Pods").exists());
        assert!(entry.join("Manifest.lock").is_file());
        assert!(!entry.join(".cache").exists());
        assert!(is_checked(&entry));
        assert_eq!(source(&entry), Some(wt.clone()));
        fs::create_dir_all(wt.join("Pods")).unwrap();
        assert_eq!(adopt(&wt.join("Pods"), &entry, &wt), Ok(false));
        assert!(wt.join("Pods").is_dir(), "left where it is");
        // A rename that fails leaves no records behind.
        let other = t.path().join("c/repo/Pods/fedcba9876543210-01234567");
        assert!(adopt(&wt.join("none"), &other, &wt).is_err());
        assert!(!marker(&other).exists() && source(&other).is_none());
    }

    #[test]
    fn an_adopted_entry_is_never_seen_without_its_records() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let t = tempfile::tempdir().unwrap();
        let entries: Vec<PathBuf> = (0..5)
            .map(|i| {
                t.path()
                    .join(format!("c/repo/Pods/012345678{i}abcdef-01234567"))
            })
            .collect();
        let done = Arc::new(AtomicBool::new(false));
        let seen_bare = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (entries, done, seen_bare) = (entries.clone(), done.clone(), seen_bare.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    // Records read after the entry is seen: they must be
                    // there already.
                    for entry in &entries {
                        if entry.is_dir() && (source(entry).is_none() || !is_checked(entry)) {
                            seen_bare.store(true, Ordering::Relaxed);
                        }
                    }
                }
            })
        };
        for (round, entry) in entries.iter().enumerate() {
            let wt = t.path().join(format!("wt{round}"));
            // A large `.cache`, so that whatever happens after the rename
            // takes long enough to be seen.
            fs::create_dir_all(wt.join("Pods/.cache")).unwrap();
            for i in 0..1500 {
                fs::write(wt.join(format!("Pods/.cache/{i}")), "x").unwrap();
            }
            assert_eq!(adopt(&wt.join("Pods"), entry, &wt), Ok(true));
            assert_eq!(source(entry), Some(wt.clone()));
            assert!(!entry.join(".cache").exists());
        }
        done.store(true, Ordering::Relaxed);
        watcher.join().unwrap();
        assert!(
            !seen_bare.load(Ordering::Relaxed),
            "an entry was there without its records"
        );
    }

    /// `n` directories `<base>/wt<i>/Pods`, each holding `who` = `i`.
    fn contenders(base: &Path, n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| {
                let wt = base.join(format!("wt{i}"));
                fs::create_dir_all(wt.join("Pods")).unwrap();
                fs::write(wt.join("Pods/who"), i.to_string()).unwrap();
                wt
            })
            .collect()
    }

    /// Run `put(i)` in `n` threads at once.
    fn at_once(n: usize, put: impl Fn(usize) + Sync) {
        let start = std::sync::Barrier::new(n);
        std::thread::scope(|s| {
            for i in 0..n {
                let (start, put) = (&start, &put);
                s.spawn(move || {
                    start.wait();
                    put(i);
                });
            }
        });
    }

    /// The worktree recorded for `entry` is the one its contents came from.
    #[track_caller]
    fn records_match(entry: &Path, wts: &[PathBuf]) {
        let who: usize = fs::read_to_string(entry.join("who"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(source(entry), Some(wts[who].clone()), "{}", entry.display());
        assert!(is_checked(entry));
    }

    #[test]
    fn the_records_of_an_entry_adopted_twice_at_once_are_the_winners() {
        let t = tempfile::tempdir().unwrap();
        for round in 0..40 {
            let base = t.path().join(format!("r{round}"));
            let wts = contenders(&base, 4);
            let entry = base.join("c/repo/Pods/0123456789abcdef-01234567");
            at_once(wts.len(), |i| {
                let _ = adopt(&wts[i].join("Pods"), &entry, &wts[i]);
            });
            records_match(&entry, &wts);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_records_of_an_entry_stored_twice_at_once_are_the_winners() {
        let t = tempfile::tempdir().unwrap();
        // The runs that come second move their copies into the trash.
        std::env::set_var("FAST_WORKTREE_CACHE", t.path().join("cache"));
        for round in 0..40 {
            let base = t.path().join(format!("r{round}"));
            let wts = contenders(&base, 4);
            let entry = base.join("c/repo/Pods/0123456789abcdef-01234567");
            at_once(wts.len(), |i| {
                store(&wts[i].join("Pods"), &entry, true, &wts[i]).unwrap();
            });
            records_match(&entry, &wts);
        }
    }

    #[test]
    fn repository_and_key_names() {
        assert!(is_repo_name("app-1234abcd"));
        assert!(is_repo_name("my-app-0000ffff"));
        assert!(!is_repo_name("app"));
        assert!(!is_repo_name("my-app"));
        assert!(!is_repo_name("app-1234ABCD"));
        assert!(!is_repo_name("-1234abcd"));
        assert!(is_key_name("0123456789abcdef-v20.13.1"));
        assert!(!is_key_name("0123456789abcdef-v20.13.1.ok"));
        assert!(!is_key_name("ansi-colors"));
        assert!(!is_key_name("0123456789abcdef"));
        assert!(is_key_name("0123456789abcdef-01234567"));
        assert!(!is_key_name("0123456789abcdef-01234567.src"));
    }

    #[test]
    fn dates_are_utc_calendar_days() {
        let at = |s: u64| date(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01");
        assert_eq!(at(951_782_400), "2000-02-29");
        assert_eq!(at(1_793_010_441), "2026-10-26");
        assert_eq!(at(1_709_251_199), "2024-02-29");
        let dt = |s: u64| datetime(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(dt(1_709_251_199), "2024-02-29 23:59");
        assert_eq!(dt(0), "1970-01-01 00:00");
    }
}
