//! Copy-on-write directory clones.
//!
//! On macOS, `clonefile(2)` clones a whole directory tree in one syscall on
//! APFS. That single call is the point: cloning file by file (`cp -Rc`)
//! takes about as long as `npm ci` itself, while one call on the directory
//! is an order of magnitude faster. Other platforms have no equivalent, so
//! there the clone stages are skipped and `npm ci` is the only path.

use std::fs;
use std::io;
use std::path::Path;

/// Whether this platform can clone directories at all.
pub const SUPPORTED: bool = cfg!(target_os = "macos");

/// Clone the file or directory `src` to `dst` (which must not exist).
#[cfg(target_os = "macos")]
pub fn clone_dir(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let s = CString::new(src.as_os_str().as_bytes())?;
    let d = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both arguments are valid NUL-terminated paths that outlive
    // the call; clonefile does not retain them.
    let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn clone_dir(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "directory clones need macOS clonefile(2)",
    ))
}

/// Copy `src` (a file, symlink or directory) to `dst`, which must not
/// exist: one clone where the platform can, else an ordinary recursive copy.
/// Symlinks are copied as symlinks.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.symlink_metadata().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists", dst.display()),
        ));
    }
    let meta = src.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        return std::os::unix::fs::symlink(fs::read_link(src)?, dst);
    }
    if SUPPORTED && clone_dir(src, dst).is_ok() {
        return Ok(());
    }
    if meta.is_dir() {
        fs::create_dir(dst)?;
        fs::set_permissions(dst, meta.permissions())?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(src, dst).map(|_| ())
    }
}

/// Whether any symlink under `dir` (not followed) has an absolute target.
/// This walks every directory, so it runs once per cache entry: when the
/// entry is stored, not each time it is cloned.
pub fn has_absolute_symlink(dir: &Path) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                if fs::read_link(e.path()).is_ok_and(|t| t.is_absolute()) {
                    return true;
                }
            } else if ft.is_dir() {
                stack.push(e.path());
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn clones_a_tree_in_one_call() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("a/b/f"), "x").unwrap();
        std::os::unix::fs::symlink("../a", src.join("link")).unwrap();
        let dst = t.path().join("dst");
        clone_dir(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("a/b/f")).unwrap(), "x");
        assert_eq!(fs::read_link(dst.join("link")).unwrap(), Path::new("../a"));
        // The clone is independent of its source.
        fs::write(dst.join("a/b/f"), "y").unwrap();
        assert_eq!(fs::read_to_string(src.join("a/b/f")).unwrap(), "x");
        assert!(clone_dir(&src, &dst).is_err(), "destination exists");
    }

    #[test]
    fn copy_tree_copies_files_dirs_and_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        fs::create_dir_all(src.join("a")).unwrap();
        fs::write(src.join("a/f"), "x").unwrap();
        std::os::unix::fs::symlink("a/f", src.join("link")).unwrap();
        let dst = t.path().join("dst");
        copy_tree(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("a/f")).unwrap(), "x");
        assert_eq!(fs::read_link(dst.join("link")).unwrap(), Path::new("a/f"));
        copy_tree(&src.join("a/f"), &t.path().join("one")).unwrap();
        assert_eq!(fs::read_to_string(t.path().join("one")).unwrap(), "x");
        let l = t.path().join("l2");
        copy_tree(&src.join("link"), &l).unwrap();
        assert!(l.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(
            copy_tree(&src.join("a/f"), &t.path().join("one")).is_err(),
            "exists"
        );
    }

    #[test]
    fn finds_absolute_symlinks_without_following_links() {
        let t = tempfile::tempdir().unwrap();
        let nm = t.path().join("node_modules");
        fs::create_dir_all(nm.join("a/b")).unwrap();
        std::os::unix::fs::symlink("../a", nm.join("rel")).unwrap();
        assert!(!has_absolute_symlink(&nm));
        std::os::unix::fs::symlink("/tmp", nm.join("a/b/abs")).unwrap();
        assert!(has_absolute_symlink(&nm));
    }
}
