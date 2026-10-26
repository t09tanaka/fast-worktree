//! Presets: what makes a [`Target`] present. Each preset of [`presets`]
//! knows one kind of project (marked by a tracked lockfile), the defaults a
//! `[[clone]]` of it gets, and how to install it; [`generic::Generic`] does
//! the same for a `[[clone]]` without a preset, from what that `[[clone]]`
//! writes out.
//!
//! Another preset is a module with a type that implements [`Preset`], plus
//! a line in [`presets`]: auto-detection, `[[clone]] preset = "..."`,
//! `config init`, `config check` and `plan` pick it up from there.

pub mod bundler;
pub mod dart;
pub mod generic;
pub mod npm;
pub mod target;

pub use target::Target;

use crate::hooks::SkipLifecycle;
use crate::paths::hex;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What an install needs to know about its surroundings.
pub struct Ctx {
    /// The worktree being installed into (real path).
    pub root: PathBuf,
    /// The main checkout (real path), for `FAST_WORKTREE_MAIN`.
    pub main: PathBuf,
    /// Every registered worktree (real paths), the main checkout first.
    /// Candidates to clone from.
    pub worktrees: Vec<PathBuf>,
    /// This repository's directory in the cache.
    pub cache_repo: PathBuf,
    /// `skip_lifecycle` rules from `.fast-worktree.toml`.
    pub skip_lifecycle: Vec<SkipLifecycle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Make missing dependencies present by any means; leave present ones.
    Missing,
    /// Like `Missing`, and also install again where the lockfile or the
    /// Node version changed since the last install.
    Changed,
    /// Only what a clone (cache or another worktree) can provide: nothing
    /// that downloads or builds. For the `post-checkout` hook.
    CloneOnly,
    /// `Changed` limited to what `CloneOnly` may do: an outdated tree is
    /// moved away and replaced by a clone when there is one, else reported
    /// missing. For `hook create` with a spare from the pool.
    ChangedCloneOnly,
}

impl Mode {
    /// Whether present dependencies are checked against the lockfile.
    pub fn reinstalls_changed(self) -> bool {
        matches!(self, Mode::Changed | Mode::ChangedCloneOnly)
    }

    /// Whether only clones are allowed: nothing that downloads or builds.
    pub fn clone_only(self) -> bool {
        matches!(self, Mode::CloneOnly | Mode::ChangedCloneOnly)
    }
}

/// The result of one project's install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Lines to print.
    pub lines: String,
    /// `CloneOnly` found nothing to clone: the dependencies are still
    /// missing.
    pub missing: bool,
    /// The dependencies already there were left as they were.
    pub kept: bool,
}

impl Report {
    pub fn done(lines: impl Into<String>) -> Report {
        Report {
            lines: lines.into(),
            missing: false,
            kept: false,
        }
    }

    /// Nothing was installed: what was there is still current.
    pub fn kept(lines: impl Into<String>) -> Report {
        Report {
            kept: true,
            ..Report::done(lines)
        }
    }

    /// A clone-only install could not make the dependencies present.
    pub fn missing(lines: impl Into<String>) -> Report {
        Report {
            missing: true,
            ..Report::done(lines)
        }
    }
}

/// What `plan` says about one target: where it would come from, and why
/// the sources tried before that one are not used. Finding out moves,
/// clones and installs nothing, but runs the `key_commands` and `verify`
/// (in this worktree, and in another without a record of its key), which
/// the repository writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// `cache <entry>`, `worktree <path>`, `delta from <entry>`, `install
    /// (<command>)`, `present (...)`, ...
    pub from: String,
    /// One line per source passed over, such as `cache: no entry <key>`.
    pub why_not: Vec<String>,
}

impl Plan {
    pub fn new(from: impl Into<String>, why_not: Vec<String>) -> Plan {
        Plan {
            from: from.into(),
            why_not,
        }
    }
}

/// A kind of target: what `[[clone]] preset = "<name>"` selects, what
/// auto-detection finds by its lockfile, and what makes it present.
pub trait Preset: Sync {
    /// The name in `preset = "..."`, such as `npm`.
    fn name(&self) -> &'static str;

    /// Shown next to a target, `api (npm)`: the name, unless it differs.
    fn label(&self) -> &'static str {
        self.name()
    }

    /// The lockfile whose tracked copies mark a project directory, and the
    /// default `key`. Empty for [`generic::Generic`], which has neither.
    fn manifest(&self) -> &'static str;

    /// The directory under the project that it provides: the default
    /// `path`, such as `node_modules`.
    fn managed(&self) -> &'static str;

    /// The default `key_commands`.
    fn default_key_commands(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether the target looks present, for `check`. Must be cheap: no
    /// subprocesses.
    fn is_installed(&self, root: &Path, t: &Target) -> bool;

    /// Whether the target is not installed but its directory is there
    /// without a record of what it was made from: something
    /// [`Preset::install`] leaves as it is, unless `--changed` asks for it
    /// to be made again. For `check`, which then does not ask for an
    /// install. Cheap, as [`Preset::is_installed`].
    fn is_unrecorded(&self, _root: &Path, _t: &Target) -> bool {
        false
    }

    /// Make `t` present in `ctx.root`. Returns what to print, or on
    /// failure the log to show.
    fn install(&self, ctx: &Ctx, t: &Target, mode: Mode) -> std::result::Result<Report, String>;

    /// What [`Preset::install`] would do, without installing anything. It
    /// may run `key_commands` and `verify` (never in a cache entry), which
    /// are the repository's commands: they should only look. With `fresh`, for
    /// a new worktree with the files of `ctx.root`: nothing is present yet,
    /// and `ctx.root` is one of the worktrees to clone from.
    fn plan(&self, ctx: &Ctx, t: &Target, mode: Mode, fresh: bool) -> Plan;

    /// Run the project's own install steps again on dependencies that are
    /// already present, because the worktree moved (a spare from the pool:
    /// what those steps wrote by absolute path points into the pool) and
    /// its tracked files may have changed. `cache_repo` is the
    /// repository's directory in the cache (for [`lifecycle_lock`]).
    /// Returns notes to print, with the time waited for that lock. On
    /// failure the dependencies are moved away, so the next `install`
    /// installs them again.
    fn rerun_own_scripts(
        &self,
        _root: &Path,
        _t: &Target,
        _rules: &[SkipLifecycle],
        _cache_repo: &Path,
    ) -> std::result::Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    /// The worktree `root` moved from one of `from` (a spare of the pool):
    /// replace those paths with `root` in the files of `t` that record it.
    /// Returns how many files were rewritten. By default every text file
    /// under `path`, when `rewrite_paths` is on.
    fn rewrite_moved(&self, root: &Path, t: &Target, from: &[PathBuf]) -> usize {
        if t.rewrite_paths && target::inside(root, &t.path).is_ok() {
            rewrite_tree(&root.join(&t.path), from, root)
        } else {
            0
        }
    }
}

/// The presets. A new one is one more line here.
pub fn presets() -> &'static [&'static dyn Preset] {
    &[&npm::Npm, &dart::Dart, &bundler::Bundler]
}

/// The preset that `[[clone]] preset = "<name>"` names.
pub fn preset(name: &str) -> Option<&'static dyn Preset> {
    presets().iter().copied().find(|p| p.name() == name)
}

/// `sh -c <cmd>` in `cwd`, with `FAST_WORKTREE_PATH` (`root`) and
/// `FAST_WORKTREE_MAIN` set, as for the hooks. Nothing on stdin.
pub fn sh(cwd: &Path, cmd: &str, root: &Path, main: &Path) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env("FAST_WORKTREE_PATH", root)
        .env("FAST_WORKTREE_MAIN", main);
    c
}

/// Run `cmd` ([`sh`]). Its output is shown only when it fails.
pub fn run_quietly(
    cwd: &Path,
    cmd: &str,
    root: &Path,
    main: &Path,
) -> std::result::Result<(), String> {
    let out = sh(cwd, cmd, root, main)
        .output()
        .map_err(|e| format!("cannot run `{cmd}`: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Err(format!(
        "`{cmd}` failed ({}):\n{}",
        out.status,
        crate::util::tail(&log, 30)
    ))
}

/// Whether `cmd` ([`sh`]) exits 0, with its output discarded.
pub fn succeeds(cwd: &Path, cmd: &str, root: &Path, main: &Path) -> bool {
    sh(cwd, cmd, root, main)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The trimmed stdout of each of `cmds`, run in `cwd` ([`sh`]). One that
/// fails is an error.
pub fn command_outputs(
    cwd: &Path,
    cmds: &[String],
    root: &Path,
    main: &Path,
) -> std::result::Result<Vec<String>, String> {
    cmds.iter()
        .map(|cmd| {
            let out = sh(cwd, cmd, root, main)
                .output()
                .map_err(|e| format!("cannot run `{cmd}`: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "`{cmd}` failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
        .collect()
}

/// The sha256 of the files `key` (relative to `root`), each with its path
/// and length, so that no two lists hash alike. A file that cannot be read
/// is an error.
pub fn key_digest(root: &Path, key: &[String]) -> std::result::Result<[u8; 32], String> {
    let mut h = Sha256::new();
    for k in key {
        let bytes = fs::read(root.join(k)).map_err(|e| format!("key file {k}: {e}"))?;
        h.update(k.as_bytes());
        h.update([0]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(h.finalize().into())
}

/// The sha256 of command outputs, each one ended by a NUL.
pub fn outputs_digest(outputs: &[String]) -> [u8; 32] {
    let mut h = Sha256::new();
    for o in outputs {
        h.update(o.as_bytes());
        h.update([0]);
    }
    h.finalize().into()
}

/// The first `n` hex digits of a digest.
pub fn short(digest: &[u8], n: usize) -> String {
    hex(digest)[..n].to_string()
}

/// Whether every file of `key` is byte-identical under the roots `a` and
/// `b`. Returns the first that is not (or that one of them lacks).
pub fn same_key_files<'k>(
    a: &Path,
    b: &Path,
    key: &'k [String],
) -> std::result::Result<(), &'k str> {
    for k in key {
        match (fs::read(a.join(k)), fs::read(b.join(k))) {
            (Ok(x), Ok(y)) if x == y => {}
            _ => return Err(k),
        }
    }
    Ok(())
}

/// In every regular text file under `dir` (symlinks and files with a NUL
/// byte are left alone), replace each of `from` with `to`. Returns how
/// many files were rewritten.
pub fn rewrite_tree(dir: &Path, from: &[PathBuf], to: &Path) -> usize {
    let to = to.as_os_str().as_encoded_bytes();
    let mut from: Vec<Vec<u8>> = from
        .iter()
        .map(|p| p.as_os_str().as_encoded_bytes().to_vec())
        .filter(|p| !p.is_empty() && p.as_slice() != to)
        .collect();
    // Longest first: one may end with another (`/private/var/...` and
    // `/var/...`).
    from.sort_by_key(|p| std::cmp::Reverse(p.len()));
    from.dedup();
    if from.is_empty() {
        return 0;
    }
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() && rewrite_file(&e.path(), &from, to) {
                n += 1;
            }
        }
    }
    n
}

/// Replace each of `from` with `to` in `file`, unless it has a NUL byte
/// (binary). Whether it was rewritten.
pub fn rewrite_file(file: &Path, from: &[Vec<u8>], to: &[u8]) -> bool {
    let Ok(bytes) = fs::read(file) else {
        return false;
    };
    if bytes.contains(&0) {
        return false;
    }
    let mut text = bytes.clone();
    for f in from {
        text = replace_all(&text, f, to);
    }
    text != bytes && fs::write(file, &text).is_ok()
}

/// `haystack` with every `needle` replaced by `with`.
pub fn replace_all(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// The lock file, in the repository's directory in the cache, that every
/// run of a project's own lifecycle holds.
pub const LIFECYCLE_LOCK: &str = "lifecycle.lock";

/// An exclusive `flock` on `<cache_repo>/lifecycle.lock`, held while the
/// value lives.
pub struct LifecycleLock {
    _file: Option<fs::File>,
    /// How long taking it waited for another holder.
    pub waited: Duration,
}

/// Wait until no other run of install scripts in this repository is going
/// on (in another process, or for another project in this one), and hold
/// the lock. Those scripts (a project's own `prepare`, a dependency's
/// `postinstall`) often write the repository's shared `.git/config` (`git
/// config core.hooksPath ...`), which git lets only one writer hold at a
/// time. A lock that cannot be taken (a file system without locks, say) is
/// not waited for.
pub fn lifecycle_lock(cache_repo: &Path) -> LifecycleLock {
    let start = Instant::now();
    let file = fs::create_dir_all(cache_repo).and_then(|()| {
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(cache_repo.join(LIFECYCLE_LOCK))
    });
    let held = |file| LifecycleLock {
        _file: file,
        waited: start.elapsed(),
    };
    let Ok(f) = file else {
        return held(None);
    };
    loop {
        // SAFETY: a valid open descriptor; the lock goes when it is closed.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return held(Some(f));
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return held(None);
        }
    }
}

/// The time a step took, without what it waited for the lifecycle lock,
/// and that wait when there was one: `1.2s` or `1.2s, waited 4.1s for
/// lifecycle lock`.
pub fn timing(start: Instant, waited: Duration) -> String {
    let took = start.elapsed().saturating_sub(waited);
    if waited < Duration::from_millis(50) {
        return format!("{:.1}s", took.as_secs_f64());
    }
    format!(
        "{:.1}s, waited {:.1}s for lifecycle lock",
        took.as_secs_f64(),
        waited.as_secs_f64()
    )
}

/// What git prints when another process holds a config file it is about
/// to write.
const CONFIG_LOCKED: &str = "could not lock config file";

/// How many more times a run that found the git config locked is tried.
pub const CONFIG_LOCK_RETRIES: u32 = 2;

/// Run the command `make` builds, and again (after a short wait, at most
/// [`CONFIG_LOCK_RETRIES`] more times) while it fails because git found a
/// config file locked: something outside fast-worktree writing it at that
/// moment.
pub fn output_retrying_config_lock(
    mut make: impl FnMut() -> Command,
) -> io::Result<std::process::Output> {
    let mut attempt = 0;
    loop {
        let out = make().output()?;
        let locked = !out.status.success()
            && (contains(&out.stderr, CONFIG_LOCKED) || contains(&out.stdout, CONFIG_LOCKED));
        if !locked || attempt == CONFIG_LOCK_RETRIES {
            return Ok(out);
        }
        attempt += 1;
        std::thread::sleep(Duration::from_millis(300 * u64::from(attempt)));
    }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// `root/<dir>`, with `.` meaning `root` itself.
pub fn project_path(root: &Path, dir: &str) -> PathBuf {
    if dir == "." {
        root.to_path_buf()
    } else {
        root.join(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_all_replaces_every_occurrence() {
        assert_eq!(replace_all(b"a/x b/x", b"/x", b"/yy"), b"a/yy b/yy");
        assert_eq!(replace_all(b"none", b"/x", b"/y"), b"none");
        assert_eq!(replace_all(b"", b"/x", b"/y"), b"");
        assert_eq!(replace_all(b"abc", b"", b"/y"), b"abc");
    }

    #[test]
    fn key_digests_tell_lists_apart() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        fs::write(p.join("a"), "1").unwrap();
        fs::write(p.join("b"), "2").unwrap();
        fs::write(p.join("ab"), "").unwrap();
        let d = |k: &[&str]| key_digest(p, &k.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(d(&["a", "b"]), d(&["a", "b"]));
        assert_ne!(d(&["a", "b"]), d(&["b", "a"]));
        assert_ne!(d(&["a"]), d(&["ab"]));
        assert!(d(&["missing"])
            .unwrap_err()
            .starts_with("key file missing: "));
        assert_ne!(
            outputs_digest(&["ab".into()]),
            outputs_digest(&["a".into(), "b".into()])
        );
        assert_eq!(short(&[0xab, 0xcd, 0xef], 4), "abcd");
    }

    #[test]
    fn same_key_files_names_the_first_that_differs() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        for d in [&a, &b] {
            fs::create_dir_all(d.join("s")).unwrap();
            fs::write(d.join("s/k"), "1").unwrap();
            fs::write(d.join("k2"), d.to_string_lossy().as_bytes()).unwrap();
        }
        let key = vec!["s/k".to_string(), "k2".to_string(), "none".to_string()];
        assert_eq!(same_key_files(&a, &b, &key[..1]), Ok(()));
        assert_eq!(same_key_files(&a, &b, &key), Err("k2"));
        assert_eq!(
            same_key_files(&a, &b, &key[2..]),
            Err("none"),
            "missing on both sides"
        );
    }

    #[test]
    fn rewrite_tree_rewrites_text_files_only() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("Pods");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a.txt"), "at /old/wt/x and /old/wt").unwrap();
        fs::write(d.join("sub/b"), "none here").unwrap();
        fs::write(d.join("bin"), b"/old/wt\0").unwrap();
        fs::write(t.path().join("outside"), "/old/wt").unwrap();
        std::os::unix::fs::symlink(t.path().join("outside"), d.join("link")).unwrap();
        let n = rewrite_tree(&d, &[PathBuf::from("/old/wt")], Path::new("/new/place"));
        assert_eq!(n, 1);
        assert_eq!(
            fs::read_to_string(d.join("a.txt")).unwrap(),
            "at /new/place/x and /new/place"
        );
        assert_eq!(fs::read(d.join("bin")).unwrap(), b"/old/wt\0");
        assert_eq!(
            fs::read_to_string(t.path().join("outside")).unwrap(),
            "/old/wt",
            "a symlink's target is not followed"
        );
        let same = rewrite_tree(&d, &[PathBuf::from("/new/place")], Path::new("/new/place"));
        assert_eq!(same, 0);
    }
}
