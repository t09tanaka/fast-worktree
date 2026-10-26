//! The dependency cache.
//!
//! An entry is a directory `<cache>/<repo>/<project>/<key>` holding one
//! installed `node_modules`. Next to it, `<key>.ok` records that the tree
//! was checked for absolute symlinks when it was stored, so cloning it does
//! not walk every file again. The marker lives outside the entry so that it
//! is never cloned into a worktree.
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
const TMP_INFIX: &str = ".tmp-";

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

/// A directory name that is an entry, not a marker or a half-made entry.
fn is_entry_name(name: &str) -> bool {
    !name.ends_with(MARKER_SUFFIX) && !name.contains(TMP_INFIX) && !name.starts_with('.')
}

/// Clone `node_modules` into the cache at `entry`, unless it is there
/// already. Built under a temporary name and renamed, so a concurrent run
/// never sees a half-made entry. `checked` says the tree was already found
/// free of absolute symlinks; otherwise it is checked here, once, and a
/// tree that has one is not cached (it may point into its worktree).
pub fn store(node_modules: &Path, entry: &Path, checked: bool) -> Result<(), String> {
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
    if fs::rename(&tmp, entry).is_ok() {
        mark_checked(entry);
    } else {
        let _ = trash::trash(&tmp);
    }
    Ok(())
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
            if e.file_name().to_string_lossy().ends_with(MARKER_SUFFIX) {
                markers.push(e.path());
            }
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
    for m in markers {
        let s = m.to_string_lossy();
        let entry = Path::new(s.strip_suffix(MARKER_SUFFIX).unwrap_or(&s));
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

/// Every entry under the cache root `root`, sorted by repository, project
/// and key.
pub fn list(root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let dirs = |p: &Path| -> Vec<(String, PathBuf)> {
        fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect()
    };
    for (repo, repo_path) in dirs(root) {
        if repo == "trash" {
            continue;
        }
        for (slug, project_path) in dirs(&repo_path) {
            for (key, path) in dirs(&project_path) {
                if !is_entry_name(&key) {
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

/// Remove every entry under `root` not used for longer than `ttl`.
pub fn prune_all(root: &Path, ttl: Duration) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let mut projects: Vec<PathBuf> = list(root)
        .into_iter()
        .filter_map(|e| e.path.parent().map(Path::to_path_buf))
        .collect();
    projects.dedup();
    for p in projects {
        removed.extend(prune(&p, ttl));
    }
    removed
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
        mark_checked(&proj.join("old-v20.1"));
        mark_checked(&proj.join("new-v20.1"));
        fs::write(proj.join("gone-v20.1.ok"), "").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        touch(&proj.join("new-v20.1"));
        let removed = prune(&proj, Duration::from_secs(1));
        assert_eq!(removed, vec![proj.join("old-v20.1")]);
        assert!(!proj.join("old-v20.1").exists());
        assert!(!proj.join("old-v20.1.ok").exists());
        assert!(!proj.join("gone-v20.1.ok").exists(), "orphan marker");
        assert!(proj.join("new-v20.1").exists());
        assert!(is_checked(&proj.join("new-v20.1")));
    }

    #[test]
    fn list_reads_repo_project_and_key() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        for p in [
            "app-1234abcd/_root/k1-v20",
            "app-1234abcd/packages__web/k2-v20",
            "app-1234abcd/packages__web/k3-v20.tmp-1-0",
            "trash/123-1-1-wt",
        ] {
            fs::create_dir_all(root.join(p)).unwrap();
        }
        mark_checked(&root.join("app-1234abcd/_root/k1-v20"));
        let got: Vec<(String, String, String)> = list(root)
            .into_iter()
            .map(|e| (e.repo, e.dir, e.key))
            .collect();
        assert_eq!(
            got,
            vec![
                ("app-1234abcd".into(), ".".into(), "k1-v20".into()),
                (
                    "app-1234abcd".into(),
                    "packages/web".into(),
                    "k2-v20".into()
                ),
            ]
        );
    }

    #[test]
    fn dates_are_utc_calendar_days() {
        let at = |s: u64| date(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01");
        assert_eq!(at(951_782_400), "2000-02-29");
        assert_eq!(at(1_793_010_441), "2026-10-26");
        assert_eq!(at(1_709_251_199), "2024-02-29");
    }
}
