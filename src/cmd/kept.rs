//! `refs/fast-worktree/kept/`: the commits `hook remove` keeps, and `kept
//! ls` / `kept prune`.
//!
//! One removal keeps its refs in a group of its own,
//! `refs/fast-worktree/kept/<name>-<YYYYMMDD>-<HHMMSS>` (UTC; `-2`, `-3`...
//! when that name is taken): `<group>/head` for the worktree's `HEAD`, and
//! `<group>/sub/<path>` for each checked-out submodule's. `prune` reads the
//! date from the group's name.

use super::cache::table;
use crate::error::Result;
use crate::git;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const KEPT_REFS: &str = "refs/fast-worktree/kept/";

/// How long `kept prune` keeps refs by default.
pub const PRUNE_DAYS: u64 = 30;

/// The refs one removal keeps, under a group that is made when the first
/// ref is added.
pub struct Group<'a> {
    main: &'a Path,
    name: String,
    base: Option<String>,
}

impl<'a> Group<'a> {
    /// A group for the worktree `name` of the repository at `main`.
    pub fn new(main: &'a Path, name: &str) -> Group<'a> {
        Group {
            main,
            name: name.to_string(),
            base: None,
        }
    }

    /// The group's ref prefix, chosen the first time: a name no ref uses.
    fn base(&mut self) -> Option<String> {
        if let Some(b) = &self.base {
            return Some(b.clone());
        }
        let time = compact_now();
        let mut stem = format!("{KEPT_REFS}{}-{time}", self.name);
        if !git::succeeds(self.main, &["check-ref-format", &format!("{stem}/head")]) {
            stem = format!("{KEPT_REFS}worktree-{time}");
        }
        for n in 1..10 {
            let b = if n == 1 {
                stem.clone()
            } else {
                format!("{stem}-{n}")
            };
            // A pattern matches the ref itself and every ref below it.
            let used = git::output(self.main, &["for-each-ref", "--format=%(refname)", &b])
                .map_or(true, |s| !s.trim().is_empty());
            if !used {
                self.base = Some(b.clone());
                return Some(b);
            }
        }
        None
    }

    /// Keep `sha` as `<group>/<leaf>`. Returns the ref.
    pub fn keep(&mut self, leaf: &str, sha: &str) -> std::result::Result<String, String> {
        let r = format!("{}/{leaf}", self.base().ok_or("no free ref name")?);
        if !git::succeeds(self.main, &["check-ref-format", &r]) {
            return Err(format!("{r} is not a valid ref name"));
        }
        // The empty old value: only when the ref does not exist yet.
        git::output(self.main, &["update-ref", &r, sha, ""])
            .map(|_| r)
            .map_err(|e| e.to_string())
    }

    /// Keep the commit `sha` of the repository whose git directory is
    /// `git_dir` (a submodule) as `<group>/<leaf>`, fetching it into the
    /// main repository first: the submodule's own git directory may go.
    pub fn fetch(
        &mut self,
        leaf: &str,
        git_dir: &Path,
        sha: &str,
    ) -> std::result::Result<String, String> {
        let r = format!("{}/{leaf}", self.base().ok_or("no free ref name")?);
        if !git::succeeds(self.main, &["check-ref-format", &r]) {
            return Err(format!("{r} is not a valid ref name"));
        }
        let src = git_dir.to_string_lossy();
        let fetch = |what: &str| {
            git::output(
                self.main,
                &[
                    "fetch",
                    "-q",
                    "--no-tags",
                    "--no-recurse-submodules",
                    "--no-write-fetch-head",
                    &src,
                    &format!("{what}:{r}"),
                ],
            )
        };
        // By commit, else (an old git that does not fetch commits by name)
        // its HEAD, which is that commit.
        let fetched = fetch(sha).or_else(|_| fetch("HEAD"));
        let got = git::output(self.main, &["rev-parse", "--verify", "-q", &r])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        match fetched {
            Ok(_) if got == sha => Ok(r),
            Ok(_) => {
                let _ = git::output(self.main, &["update-ref", "-d", &r]);
                Err(format!("fetched {got} instead of {sha}"))
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

/// `YYYYMMDD-HHMMSS` (UTC), for a ref name.
pub fn compact_now() -> String {
    compact(now_secs())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn compact(secs: u64) -> String {
    let t = UNIX_EPOCH + std::time::Duration::from_secs(secs);
    let s = secs % 86_400;
    format!(
        "{}-{:02}{:02}{:02}",
        crate::cache::date(t).replace('-', ""),
        s / 3_600,
        s % 3_600 / 60,
        s % 60
    )
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's
/// days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The time in a kept ref's name: the rightmost `-YYYYMMDD-HHMMSS` of the
/// part after `kept/` up to the next `/`, followed by nothing or by `-<n>`.
/// (`<name>` may hold digits and dashes of its own.)
pub fn kept_time(refname: &str) -> Option<u64> {
    let group = refname.strip_prefix(KEPT_REFS)?.split('/').next()?;
    let b = group.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    (0..b.len()).rev().find_map(|i| {
        if b.len() < i + 16 || b[i] != b'-' || b[i + 9] != b'-' {
            return None;
        }
        if !digits(i + 1..i + 9) || !digits(i + 10..i + 16) {
            return None;
        }
        let rest = &group[i + 16..];
        let suffix_ok = rest.is_empty()
            || rest
                .strip_prefix('-')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()));
        if !suffix_ok {
            return None;
        }
        let num = |r: std::ops::Range<usize>| group[r].parse::<i64>().ok();
        let (y, mo, d) = (num(i + 1..i + 5)?, num(i + 5..i + 7)?, num(i + 7..i + 9)?);
        let (h, mi, s) = (
            num(i + 10..i + 12)?,
            num(i + 12..i + 14)?,
            num(i + 14..i + 16)?,
        );
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
            return None;
        }
        let secs = days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60 + s;
        u64::try_from(secs).ok()
    })
}

/// The kept refs of the repository in the current directory, with their
/// commits.
fn kept_refs() -> Result<Vec<(String, String)>> {
    let out = git::output(
        Path::new("."),
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            KEPT_REFS,
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(r, o)| (r.to_string(), o.to_string()))
        .collect())
}

fn count(n: usize) -> String {
    format!("{n} {}", if n == 1 { "ref" } else { "refs" })
}

/// `kept ls`.
pub fn ls() -> Result<i32> {
    let refs = kept_refs()?;
    if refs.is_empty() {
        println!("no refs under {KEPT_REFS}");
        return Ok(0);
    }
    let now = now_secs();
    let rows: Vec<[String; 3]> = refs
        .iter()
        .map(|(r, o)| {
            let when = match kept_time(r) {
                Some(t) => {
                    let st = UNIX_EPOCH + std::time::Duration::from_secs(t);
                    format!(
                        "{} ({}d ago)",
                        crate::cache::datetime(st),
                        now.saturating_sub(t) / 86_400
                    )
                }
                None => "?".to_string(),
            };
            [r.clone(), o[..o.len().min(12)].to_string(), when]
        })
        .collect();
    print!("{}", table(&["REF", "COMMIT", "KEPT (UTC)"], &rows));
    println!(
        "{}; `git push --mirror` pushes them too. `fast-worktree kept prune` drops those older than {PRUNE_DAYS} days",
        count(refs.len())
    );
    Ok(0)
}

/// `kept prune [--older-than <days>]`: delete the kept refs whose name says
/// they are older than that. Refs whose name has no time are left alone.
pub fn prune(older_than_days: Option<u64>) -> Result<i32> {
    let days = older_than_days.unwrap_or(PRUNE_DAYS);
    let now = now_secs();
    let mut pruned = 0;
    let mut failed = 0;
    for (r, o) in kept_refs()? {
        let Some(t) = kept_time(&r) else {
            continue;
        };
        if now.saturating_sub(t) < days * 86_400 {
            continue;
        }
        match git::output(Path::new("."), &["update-ref", "-d", &r, &o]) {
            Ok(_) => {
                println!("pruned {r}");
                pruned += 1;
            }
            Err(e) => {
                eprintln!("fast-worktree: {e}");
                failed += 1;
            }
        }
    }
    println!("pruned {} kept for {days} days or more", count(pruned));
    Ok(i32::from(failed > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_time_is_read_from_the_right_of_the_group_name() {
        let t = kept_time("refs/fast-worktree/kept/a-20261026-120000/head").unwrap();
        assert_eq!(compact(t), "20261026-120000");
        let t2 = kept_time("refs/fast-worktree/kept/a-20261026-120000-3/sub/x/y").unwrap();
        assert_eq!(t2, t);
        // A name with a date of its own.
        let t3 = kept_time("refs/fast-worktree/kept/fix-20200101-000000-20261026-120000").unwrap();
        assert_eq!(t3, t);
        assert_eq!(kept_time("refs/fast-worktree/kept/plain/head"), None);
        assert_eq!(
            kept_time("refs/fast-worktree/kept/a-20261026-120000x"),
            None
        );
        assert_eq!(kept_time("refs/fast-worktree/kept/a-20261326-120000"), None);
        assert_eq!(kept_time("refs/heads/a-20261026-120000"), None);
        assert_eq!(
            kept_time("refs/fast-worktree/kept/x-19700101-000001"),
            Some(1)
        );
    }

    #[test]
    fn compact_times_round_trip() {
        for secs in [0, 951_782_400, 1_793_016_085, 4_102_444_799] {
            let c = compact(secs);
            let r = format!("{KEPT_REFS}n-{c}");
            assert_eq!(kept_time(&r), Some(secs), "{c}");
        }
        let now = compact_now();
        assert_eq!(now.len(), "20261026-120000".len(), "{now}");
    }
}
