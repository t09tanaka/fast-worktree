//! Copy-on-write directory clones.
//!
//! On macOS, `clonefile(2)` clones a whole directory tree in one syscall on
//! APFS. That single call is the point: cloning file by file (`cp -Rc`)
//! takes about as long as `npm ci` itself, while one call on the directory
//! is an order of magnitude faster. Other platforms have no equivalent, so
//! there the clone stages are skipped and `npm ci` is the only path.

use std::io;
use std::path::Path;

/// Whether this platform can clone directories at all.
pub const SUPPORTED: bool = cfg!(target_os = "macos");

/// Clone the directory `src` to `dst` (which must not exist).
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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::fs;

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
}
