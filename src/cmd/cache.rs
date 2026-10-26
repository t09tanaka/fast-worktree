//! `fast-worktree cache ls` / `cache prune`.

use crate::{cache, paths, util};
use std::time::{Duration, SystemTime};

pub fn ls() -> i32 {
    let root = paths::cache_root();
    let entries = cache::list(&root);
    if entries.is_empty() {
        println!("cache is empty ({})", util::tilde(&root));
        return 0;
    }
    let now = SystemTime::now();
    let rows: Vec<[String; 4]> = entries
        .iter()
        .map(|e| {
            let days = now
                .duration_since(e.last_used)
                .map(|d| d.as_secs() / 86_400)
                .unwrap_or(0);
            [
                e.repo.clone(),
                e.dir.clone(),
                e.key.clone(),
                format!("{} ({days}d ago)", cache::date(e.last_used)),
            ]
        })
        .collect();
    print!("{}", table(&["REPO", "DIR", "KEY", "LAST USED"], &rows));
    println!("{} in {}", count(entries.len()), util::tilde(&root));
    0
}

pub fn prune(older_than_days: Option<u64>) -> i32 {
    let days = older_than_days.unwrap_or(cache::TTL_DAYS);
    let root = paths::cache_root();
    let removed = cache::prune_all(&root, Duration::from_secs(days * 86_400));
    for p in &removed {
        let rel = p.strip_prefix(&root).unwrap_or(p);
        println!("pruned {}", rel.display());
    }
    println!(
        "pruned {} unused for more than {days} days (deleting in background)",
        count(removed.len())
    );
    0
}

/// `1 entry`, `2 entries`.
fn count(n: usize) -> String {
    format!("{n} {}", if n == 1 { "entry" } else { "entries" })
}

/// Left-aligned columns separated by two spaces.
fn table<const N: usize>(head: &[&str; N], rows: &[[String; N]]) -> String {
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
