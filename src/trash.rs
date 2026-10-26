//! Fast deletion: rename into a trash directory on the same volume (a
//! constant-time metadata operation), then delete in a detached child.

use crate::paths;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Trash entries older than this are assumed to be orphaned (their deleter
/// was killed) and are deleted again by the next run.
const SWEEP_AFTER_SECS: u64 = 600;

/// Name prefix of the hidden sibling used when the trash directory is on
/// another volume.
const BESIDE_PREFIX: &str = ".fast-worktree-trash-";

/// Name prefix of a note in the trash directory that records where such a
/// sibling is.
const NOTE_PREFIX: &str = "beside-";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A name that is unique across threads and concurrent processes:
/// `<unix seconds>-<pid>-<counter><nanos>-<basename>`.
fn unique_name(path: &Path) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let base = path
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_else(|| "x".into());
    format!("{}-{}-{n}{nanos}-{base}", now_secs(), std::process::id())
}

/// Name prefix of trash entries that are kept for a while instead of being
/// deleted at once: `keep-<unix seconds until>-...`.
const KEEP_PREFIX: &str = "keep-";

/// Move `path` out of the way and delete it in the background. Returns
/// where it went. Falls back to a hidden sibling when the trash directory
/// is on another volume, so the rename stays cheap.
pub fn trash(path: &Path) -> io::Result<PathBuf> {
    let dest = move_aside(path)?;
    spawn_rm(&dest);
    Ok(dest)
}

/// Move `path` into the trash as [`trash`] does, but start no deleter:
/// the caller takes what it needs out of it first, then calls
/// [`spawn_rm`]. One left there is deleted by a later [`sweep`].
pub fn move_aside(path: &Path) -> io::Result<PathBuf> {
    move_away(path, &unique_name(path))
}

/// Move `path` into the trash and leave it there for at least `secs`
/// seconds: a later run deletes it once that time has passed. Returns where
/// it went.
pub fn keep(path: &Path, secs: u64) -> io::Result<PathBuf> {
    let name = format!("{KEEP_PREFIX}{}-{}", now_secs() + secs, unique_name(path));
    move_away(path, &name)
}

/// Rename `path` into the trash as `name` (or next to itself when the trash
/// is on another volume).
fn move_away(path: &Path, name: &str) -> io::Result<PathBuf> {
    let dir = paths::trash_dir();
    let dest = match fs::create_dir_all(&dir) {
        Ok(()) => {
            let d = dir.join(name);
            match fs::rename(path, &d) {
                Ok(()) => Some(d),
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => None,
                Err(e) => return Err(e),
            }
        }
        Err(_) => None,
    };
    let dest = match dest {
        Some(d) => d,
        None => beside(path, name, &dir)?,
    };
    Ok(dest)
}

/// Rename `path` to a hidden sibling, and leave a note in the trash
/// directory `dir` saying where it went, so that [`sweep`] finds it once
/// its name says it is stale: when its deleter was interrupted, or, for
/// [`keep`], when its time is up. (Without a trash directory there is
/// nowhere to leave the note.) Whether a deleter starts at once is up to
/// the caller: [`trash`] starts one, [`keep`] does not.
fn beside(path: &Path, name: &str, dir: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let d = parent.join(format!("{BESIDE_PREFIX}{name}"));
    fs::rename(path, &d)?;
    let real = d.canonicalize().unwrap_or_else(|_| d.clone());
    let _ = fs::write(
        dir.join(format!("{NOTE_PREFIX}{name}")),
        real.as_os_str().as_bytes(),
    );
    Ok(d)
}

/// Whether `path` can be renamed away at all: its parent directory is
/// writable and `path` is not a mount point (a rename never crosses
/// volumes). Checked before anything that cannot be undone, such as the
/// `pre_remove` hooks.
pub fn can_move(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let parent = path.parent().unwrap_or(Path::new("/"));
    let dev = |p: &Path| p.metadata().map(|m| m.dev());
    match (dev(path), dev(parent)) {
        (Ok(a), Ok(b)) if a != b => return Err(format!("{} is a mount point", path.display())),
        (Err(e), _) | (_, Err(e)) => return Err(e.to_string()),
        _ => {}
    }
    let c = std::ffi::CString::new(parent.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: a valid NUL-terminated path.
    if unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
        return Err(format!(
            "{} is not writable ({})",
            parent.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Start `rm -rf <path>` in its own process group with no stdio, and do
/// not wait for it: it outlives this process and ignores the terminal's
/// Ctrl-C.
pub fn spawn_rm(path: &Path) {
    let _ = Command::new("rm")
        .arg("-rf")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

/// Delete again whatever an earlier, interrupted deleter left in the trash,
/// and the hidden siblings that the trash's notes point to.
pub fn sweep() {
    for p in stale(&paths::trash_dir(), now_secs()) {
        spawn_rm(&p);
    }
}

/// What [`sweep`] deletes, for the trash directory `dir` at time `now`. A
/// note whose sibling is gone is removed on the way.
fn stale(dir: &Path, now: u64) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with(NOTE_PREFIX) {
            if is_stale(&name, now) {
                out.push(e.path());
            }
            continue;
        }
        let Ok(bytes) = fs::read(e.path()) else {
            continue;
        };
        let target = PathBuf::from(OsStr::from_bytes(&bytes));
        // Only a sibling named by us, and only once its name says stale.
        let Some(theirs) = target
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(BESIDE_PREFIX))
        else {
            continue;
        };
        if target.symlink_metadata().is_err() {
            let _ = fs::remove_file(e.path());
        } else if is_stale(theirs, now) {
            out.push(target);
        }
    }
    out
}

fn is_stale(name: &str, now: u64) -> bool {
    if let Some(rest) = name.strip_prefix(KEEP_PREFIX) {
        return match rest.split('-').next().and_then(|s| s.parse::<u64>().ok()) {
            Some(until) => until < now,
            None => false,
        };
    }
    match name.split('-').next().and_then(|s| s.parse::<u64>().ok()) {
        Some(t) => t + SWEEP_AFTER_SECS < now,
        // Not ours, or not named by us: leave it.
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_is_decided_by_the_name_timestamp() {
        assert!(is_stale("100-1-1-wt", 100 + SWEEP_AFTER_SECS + 1));
        assert!(!is_stale("100-1-1-wt", 100 + SWEEP_AFTER_SECS));
        assert!(!is_stale("readme", 1_000_000));
        assert!(!is_stale("keep-5000-100-1-1-wt", 4_999));
        assert!(!is_stale("keep-5000-100-1-1-wt", 5_000));
        assert!(is_stale("keep-5000-100-1-1-wt", 5_001));
        assert!(!is_stale("keep-x", 1_000_000));
    }

    #[test]
    fn a_sibling_is_noted_in_the_trash_and_swept_when_stale() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let dir = base.join("trash");
        fs::create_dir_all(&dir).unwrap();
        let wt = base.join("vol/wt");
        fs::create_dir_all(wt.join("sub")).unwrap();
        let d = beside(&wt, "100-1-1-wt", &dir).unwrap();
        assert_eq!(d, base.join("vol/.fast-worktree-trash-100-1-1-wt"));
        assert!(!wt.exists() && d.join("sub").is_dir());
        let note = dir.join("beside-100-1-1-wt");
        assert!(note.is_file());
        // Not stale yet: neither the sibling nor the note goes.
        assert!(stale(&dir, 100 + SWEEP_AFTER_SECS).is_empty());
        assert_eq!(stale(&dir, 100 + SWEEP_AFTER_SECS + 1), vec![d.clone()]);
        // Once the sibling is gone, so is the note.
        fs::remove_dir_all(&d).unwrap();
        assert!(stale(&dir, 100 + SWEEP_AFTER_SECS + 1).is_empty());
        assert!(!note.exists());
    }

    #[test]
    fn a_note_never_points_sweep_at_something_not_named_by_us() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("trash");
        fs::create_dir_all(&dir).unwrap();
        let other = t.path().join("project");
        fs::create_dir_all(&other).unwrap();
        fs::write(dir.join("beside-100-1-1-x"), other.as_os_str().as_bytes()).unwrap();
        assert!(stale(&dir, u64::MAX / 2).is_empty());
        assert!(other.is_dir());
    }

    #[test]
    fn can_move_needs_a_writable_parent() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let wt = t.path().join("p/wt");
        fs::create_dir_all(&wt).unwrap();
        assert_eq!(can_move(&wt), Ok(()));
        assert!(can_move(&t.path().join("p/missing")).is_err());
        let p = t.path().join("p");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o555)).unwrap();
        // SAFETY: no arguments.
        let root = unsafe { libc::geteuid() } == 0;
        let r = can_move(&wt);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        if !root {
            assert!(r.unwrap_err().contains("not writable"));
        }
    }

    #[test]
    fn unique_names_differ() {
        let p = Path::new("/a/wt");
        let a = unique_name(p);
        let b = unique_name(p);
        assert_ne!(a, b);
        assert!(a.ends_with("-wt"));
    }
}
