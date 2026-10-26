//! Small helpers shared by the commands.

use std::env;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub fn home() -> PathBuf {
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Write to stdout, ignoring errors. `print!` panics when stdout is a pipe
/// whose reader has gone (EPIPE); `check` promises to exit 0 whatever
/// happens to its output.
pub fn say(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes()).and_then(|()| out.flush());
}

/// [`say`] for stderr.
pub fn warn(s: &str) {
    use std::io::Write;
    let _ = std::io::stderr().lock().write_all(s.as_bytes());
}

/// `print!` through [`say`]: a reader that has gone (`fast-worktree pool
/// status | head -1`) ends the output, not the command.
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::util::say(&format!($($arg)*))
    };
}

/// `println!` through [`say`].
macro_rules! outln {
    () => {
        $crate::util::say("\n")
    };
    ($($arg:tt)*) => {
        $crate::util::say(&format!("{}\n", format_args!($($arg)*)))
    };
}

/// `eprintln!` through [`warn`].
macro_rules! errln {
    () => {
        $crate::util::warn("\n")
    };
    ($($arg:tt)*) => {
        $crate::util::warn(&format!("{}\n", format_args!($($arg)*)))
    };
}
pub(crate) use {errln, out, outln};

/// `1.2s`, the format every timing line uses.
pub fn elapsed(start: Instant) -> String {
    format!("{:.1}s", start.elapsed().as_secs_f64())
}

/// Show a path with `$HOME` abbreviated to `~`.
pub fn tilde(path: &Path) -> String {
    tilde_with(path, &home())
}

fn tilde_with(path: &Path, home: &Path) -> String {
    if home != Path::new("/") {
        if let Ok(rest) = path.strip_prefix(home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

/// Find an executable on `PATH`, then in `~/.cargo/bin` (where `cargo
/// install` puts tools even when that directory is not on `PATH`).
pub fn find_tool(name: &str) -> Option<PathBuf> {
    find_tool_in(name, env::var_os("PATH").as_deref(), &home())
}

fn find_tool_in(name: &str, path: Option<&std::ffi::OsStr>, home: &Path) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let on_path = path
        .into_iter()
        .flat_map(env::split_paths)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name));
    let cargo = std::iter::once(home.join(".cargo/bin").join(name));
    on_path.chain(cargo).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The last `n` lines of `s`.
pub fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn tilde_abbreviates_home_only_as_a_path_prefix() {
        let home = Path::new("/Users/me");
        assert_eq!(tilde_with(Path::new("/Users/me/a/b"), home), "~/a/b");
        assert_eq!(tilde_with(Path::new("/Users/me"), home), "~");
        assert_eq!(
            tilde_with(Path::new("/Users/meow/a"), home),
            "/Users/meow/a"
        );
        assert_eq!(tilde_with(Path::new("/x"), Path::new("/")), "/x");
    }

    #[test]
    fn find_tool_prefers_path_then_cargo_bin() {
        let t = tempfile::tempdir().unwrap();
        let bin = t.path().join("bin");
        let cargo = t.path().join(".cargo/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&cargo).unwrap();
        let mk = |p: &Path| {
            fs::write(p, "#!/bin/sh\n").unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        };
        mk(&cargo.join("tool"));
        let path = std::ffi::OsString::from(bin.as_os_str());
        assert_eq!(
            find_tool_in("tool", Some(&path), t.path()),
            Some(cargo.join("tool"))
        );
        mk(&bin.join("tool"));
        assert_eq!(
            find_tool_in("tool", Some(&path), t.path()),
            Some(bin.join("tool"))
        );
        assert_eq!(find_tool_in("nope", Some(&path), t.path()), None);
        // Not executable: ignored.
        fs::write(bin.join("plain"), "").unwrap();
        assert_eq!(find_tool_in("plain", Some(&path), t.path()), None);
    }

    #[test]
    fn tail_keeps_last_lines() {
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a", 5), "a");
    }
}
