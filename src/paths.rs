//! Where the cache and the trash live.

use crate::util::home;
use sha2::{Digest, Sha256};
use std::env;
use std::path::{Path, PathBuf};

/// `FAST_WORKTREE_CACHE`, else `~/Library/Caches/fast-worktree` on macOS,
/// else `$XDG_CACHE_HOME/fast-worktree` (default `~/.cache/fast-worktree`).
pub fn cache_root() -> PathBuf {
    let var = |k: &str| env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(p) = var("FAST_WORKTREE_CACHE") {
        return p;
    }
    if cfg!(target_os = "macos") {
        return home().join("Library/Caches/fast-worktree");
    }
    var("XDG_CACHE_HOME")
        .unwrap_or_else(|| home().join(".cache"))
        .join("fast-worktree")
}

/// Removed worktrees and stale cache entries are renamed here, then deleted
/// in the background. It sits under the cache so that it is on the same
/// volume as the cache, and usually as the worktrees.
pub fn trash_dir() -> PathBuf {
    cache_root().join("trash")
}

/// The cache namespace of a repository: the main checkout's basename plus a
/// hash of its real path, so two clones named `app` never share entries.
pub fn repo_key(main_checkout: &Path) -> String {
    let real = main_checkout
        .canonicalize()
        .unwrap_or_else(|_| main_checkout.to_path_buf());
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let hash = Sha256::digest(real.as_os_str().as_encoded_bytes());
    format!("{name}-{}", &hex(&hash)[..8])
}

/// A project directory as one path component: `.` becomes `_root`, `/`
/// becomes `__`.
pub fn project_slug(dir: &str) -> String {
    if dir == "." || dir.is_empty() {
        "_root".to_string()
    } else {
        dir.replace('/', "__")
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_key_separates_same_named_checkouts() {
        let a = repo_key(Path::new("/nonexistent/one/app"));
        let b = repo_key(Path::new("/nonexistent/two/app"));
        assert!(a.starts_with("app-") && b.starts_with("app-"));
        assert_eq!(a.len(), "app-".len() + 8);
        assert_ne!(a, b);
        assert_eq!(a, repo_key(Path::new("/nonexistent/one/app")));
    }

    #[test]
    fn project_slug_is_one_component() {
        assert_eq!(project_slug("."), "_root");
        assert_eq!(project_slug("packages/web"), "packages__web");
        assert_eq!(project_slug("api"), "api");
    }
}
