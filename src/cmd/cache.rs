//! `fast-worktree cache ls` / `cache prune`.

use crate::util::{out, outln};
use crate::{cache, paths, util};
use std::time::{Duration, SystemTime};

pub fn ls() -> i32 {
    let root = paths::cache_root();
    let entries = cache::list(&root);
    let legacy = cache::legacy(&root);
    if entries.is_empty() {
        outln!("cache is empty ({})", util::tilde(&root));
    } else {
        let rows: Vec<[String; 4]> = entries
            .iter()
            .map(|e| {
                [
                    e.repo.clone(),
                    e.dir.clone(),
                    e.key.clone(),
                    last_used(e.last_used),
                ]
            })
            .collect();
        out!("{}", table(&["REPO", "DIR", "KEY", "LAST USED"], &rows));
        outln!("{} in {}", count(entries.len()), util::tilde(&root));
    }
    if !legacy.is_empty() {
        let rows: Vec<[String; 2]> = legacy
            .iter()
            .map(|l| [l.name.clone(), last_used(l.last_used)])
            .collect();
        outln!();
        out!("{}", table(&["LEGACY", "LAST USED"], &rows));
        outln!(
            "{} of an older layout, not used any more; \
             `fast-worktree cache prune --legacy` removes them",
            directories(legacy.len())
        );
    }
    0
}

/// `2026-10-26 (0d ago)`.
fn last_used(t: SystemTime) -> String {
    let days = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    format!("{} ({days}d ago)", cache::date(t))
}

pub fn prune(older_than_days: Option<u64>) -> i32 {
    let days = older_than_days.unwrap_or(cache::TTL_DAYS);
    let root = paths::cache_root();
    let removed = cache::prune_all(&root, Duration::from_secs(days * 86_400));
    for e in &removed {
        outln!("pruned {}  {}  {}", e.repo, e.dir, e.key);
    }
    outln!(
        "pruned {} unused for more than {days} days (deleting in background)",
        count(removed.len())
    );
    0
}

/// `cache prune --legacy`: move the directories of an older layout to the
/// trash.
pub fn prune_legacy() -> i32 {
    let root = paths::cache_root();
    let removed = cache::prune_legacy(&root);
    for l in &removed {
        outln!("pruned {} (older layout)", l.name);
    }
    outln!(
        "pruned {} of an older layout (deleting in background)",
        directories(removed.len())
    );
    0
}

/// `1 entry`, `2 entries`.
fn count(n: usize) -> String {
    format!("{n} {}", if n == 1 { "entry" } else { "entries" })
}

/// `1 directory`, `2 directories`.
fn directories(n: usize) -> String {
    format!("{n} {}", if n == 1 { "directory" } else { "directories" })
}

/// Left-aligned columns separated by two spaces.
pub fn table<const N: usize>(head: &[&str; N], rows: &[[String; N]]) -> String {
    let mut width = head.map(str::len);
    for r in rows {
        for (w, c) in width.iter_mut().zip(r) {
            *w = (*w).max(c.len());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i + 1 == N {
                s.push_str(c);
            } else {
                s.push_str(&format!("{c:<w$}  ", w = width[i]));
            }
        }
        s.push('\n');
        s
    };
    let mut out = line(head.to_vec());
    for r in rows {
        out.push_str(&line(r.iter().map(String::as_str).collect()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns() {
        let rows = [
            ["a".to_string(), "long".to_string()],
            ["bbb".to_string(), "x".to_string()],
        ];
        assert_eq!(table(&["H", "I"], &rows), "H    I\na    long\nbbb  x\n");
    }
}
