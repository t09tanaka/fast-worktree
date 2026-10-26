//! Fast deletion: rename into a trash directory on the same volume (a
//! constant-time metadata operation), then delete in a detached child.

use crate::paths;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Trash entries older than this are assumed to be orphaned (their deleter
/// was killed) and are deleted again by the next run.
const SWEEP_AFTER_SECS: u64 = 600;

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

/// Move `path` out of the way and delete it in the background. Returns
/// where it went. Falls back to a hidden sibling when the trash directory
/// is on another volume, so the rename stays cheap.
pub fn trash(path: &Path) -> io::Result<PathBuf> {
    let name = unique_name(path);
    let dir = paths::trash_dir();
    let dest = match fs::create_dir_all(&dir) {
        Ok(()) => {
            let d = dir.join(&name);
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
        None => {
            let parent = path.parent().unwrap_or(Path::new("."));
            let d = parent.join(format!(".fast-worktree-trash-{name}"));
            fs::rename(path, &d)?;
            d
        }
    };
    spawn_rm(&dest);
    Ok(dest)
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

/// Delete again whatever an earlier, interrupted deleter left in the trash.
pub fn sweep() {
    let Ok(entries) = fs::read_dir(paths::trash_dir()) else {
        return;
    };
    let now = now_secs();
    for e in entries.flatten() {
        let name = e.file_name();
        if is_stale(&name.to_string_lossy(), now) {
            spawn_rm(&e.path());
        }
    }
}

fn is_stale(name: &str, now: u64) -> bool {
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
