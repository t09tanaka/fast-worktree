//! Does an installed `node_modules` match a `package-lock.json`?
//!
//! npm writes `node_modules/.package-lock.json` when it installs: a record
//! of the tree actually on disk. Comparing it with the lockfile tells
//! whether the installed tree is the one the lockfile asks for, without
//! walking `node_modules`. A checkout whose lockfile changed (a pull) but
//! that was not reinstalled has the same lockfile as a fresh worktree and
//! an old tree; this is what catches it.
//!
//! The rules, per entry of `packages` (the `""` root entry is skipped):
//! - in the tree but not in the lockfile: `extra`
//! - in both, with a different `version`; a different `integrity` or
//!   `resolved` when both have one (a git dependency whose `#<commit>`
//!   moved keeps its version and has no integrity); or `link` on one side
//!   only: `diff`
//! - in the lockfile but not in the tree, unless `optional`, `devOptional`
//!   or `peer` (npm may legitimately leave those out): `missing`
//!
//! Field tests follow JavaScript truthiness, as npm's own reader does.

use serde_json::{Map, Value};

/// `Ok(())` when the tree matches the lockfile, otherwise up to three
/// reasons such as `diff node_modules/foo`.
pub fn tree_matches(lockfile: &[u8], hidden: &[u8]) -> Result<(), Vec<String>> {
    let parse = |b: &[u8]| -> Result<Map<String, Value>, Vec<String>> {
        let v: Value = serde_json::from_slice(b).map_err(|e| vec![format!("parse: {e}")])?;
        Ok(match v.get("packages") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        })
    };
    let lock = parse(lockfile)?;
    let tree = parse(hidden)?;
    let mut why = Vec::new();
    for (k, v) in &tree {
        if k.is_empty() {
            continue;
        }
        match lock.get(k) {
            None => why.push(format!("extra {k}")),
            Some(l) => {
                let both_and_differ = |f: &str| {
                    let (a, b) = (l.get(f), v.get(f));
                    truthy(a) && truthy(b) && a != b
                };
                if l.get("version") != v.get("version")
                    || both_and_differ("integrity")
                    || both_and_differ("resolved")
                    || truthy(l.get("link")) != truthy(v.get("link"))
                {
                    why.push(format!("diff {k}"));
                }
            }
        }
    }
    for (k, l) in &lock {
        if k.is_empty() || truthy(tree.get(k)) {
            continue;
        }
        if !(truthy(l.get("optional")) || truthy(l.get("devOptional")) || truthy(l.get("peer"))) {
            why.push(format!("missing {k}"));
        }
    }
    if why.is_empty() {
        Ok(())
    } else {
        why.truncate(3);
        Err(why)
    }
}

/// JavaScript truthiness of an optional JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(lock: Value, tree: Value) -> Result<(), Vec<String>> {
        tree_matches(lock.to_string().as_bytes(), tree.to_string().as_bytes())
    }

    fn pkgs(p: Value) -> Value {
        json!({ "name": "x", "lockfileVersion": 3, "packages": p })
    }

    #[test]
    fn identical_trees_match() {
        let p = pkgs(json!({
            "": { "name": "x" },
            "node_modules/a": { "version": "1.0.0", "integrity": "sha512-a" },
        }));
        assert_eq!(check(p.clone(), p), Ok(()));
    }

    #[test]
    fn root_entry_is_ignored() {
        let lock = pkgs(json!({ "": { "name": "x", "version": "1.0.0" } }));
        let tree = pkgs(json!({ "": { "name": "x", "version": "2.0.0" } }));
        assert_eq!(check(lock, tree), Ok(()));
    }

    #[test]
    fn version_change_is_a_diff() {
        let lock = pkgs(json!({ "node_modules/a": { "version": "1.0.1" } }));
        let tree = pkgs(json!({ "node_modules/a": { "version": "1.0.0" } }));
        assert_eq!(check(lock, tree), Err(vec!["diff node_modules/a".into()]));
    }

    #[test]
    fn integrity_is_compared_only_when_both_have_one() {
        let lock = pkgs(json!({ "node_modules/a": { "version": "1", "integrity": "sha512-x" } }));
        let same_v = pkgs(json!({ "node_modules/a": { "version": "1" } }));
        assert_eq!(check(lock.clone(), same_v), Ok(()));
        let empty = pkgs(json!({ "node_modules/a": { "version": "1", "integrity": "" } }));
        assert_eq!(check(lock.clone(), empty), Ok(()), "empty string is falsy");
        let other = pkgs(json!({ "node_modules/a": { "version": "1", "integrity": "sha512-y" } }));
        assert_eq!(check(lock, other), Err(vec!["diff node_modules/a".into()]));
    }

    #[test]
    fn a_git_dependency_whose_commit_moved_is_a_diff() {
        let at = |sha: &str| {
            pkgs(json!({ "node_modules/a": {
                "version": "1.0.0",
                "resolved": format!("git+ssh://git@example.com/a.git#{sha}"),
            } }))
        };
        assert_eq!(check(at("aaa"), at("aaa")), Ok(()));
        assert_eq!(
            check(at("bbb"), at("aaa")),
            Err(vec!["diff node_modules/a".into()])
        );
        // Compared only when both have one, like integrity.
        let bare = pkgs(json!({ "node_modules/a": { "version": "1.0.0" } }));
        assert_eq!(check(at("bbb"), bare), Ok(()));
    }

    #[test]
    fn a_link_is_compared_by_its_target_and_its_flag() {
        let link = |to: &str| pkgs(json!({ "node_modules/a": { "resolved": to, "link": true } }));
        assert_eq!(check(link("packages/a"), link("packages/a")), Ok(()));
        assert_eq!(
            check(link("packages/b"), link("packages/a")),
            Err(vec!["diff node_modules/a".into()])
        );
        let not_link = pkgs(json!({ "node_modules/a": { "resolved": "packages/a" } }));
        assert_eq!(
            check(link("packages/a"), not_link),
            Err(vec!["diff node_modules/a".into()])
        );
    }

    #[test]
    fn missing_version_on_both_sides_is_equal() {
        let lock = pkgs(json!({ "node_modules/a": { "link": true } }));
        let tree = pkgs(json!({ "node_modules/a": { "link": true } }));
        assert_eq!(check(lock, tree), Ok(()));
        let tree = pkgs(json!({ "node_modules/a": { "version": "1" } }));
        let lock = pkgs(json!({ "node_modules/a": { "link": true } }));
        assert!(check(lock, tree).is_err());
    }

    #[test]
    fn extra_entry_in_the_tree() {
        let lock = pkgs(json!({}));
        let tree = pkgs(json!({ "node_modules/a": { "version": "1" } }));
        assert_eq!(check(lock, tree), Err(vec!["extra node_modules/a".into()]));
    }

    #[test]
    fn missing_entry_unless_optional_dev_optional_or_peer() {
        let tree = pkgs(json!({}));
        let lock = pkgs(json!({ "node_modules/a": { "version": "1" } }));
        assert_eq!(
            check(lock, tree.clone()),
            Err(vec!["missing node_modules/a".into()])
        );
        for flag in ["optional", "devOptional", "peer"] {
            let lock = pkgs(json!({ "node_modules/a": { "version": "1", flag: true } }));
            assert_eq!(check(lock, tree.clone()), Ok(()), "{flag}");
            let lock = pkgs(json!({ "node_modules/a": { "version": "1", flag: false } }));
            assert!(check(lock, tree.clone()).is_err(), "{flag}: false");
        }
    }

    #[test]
    fn reasons_are_capped_at_three() {
        let lock = pkgs(json!({}));
        let tree = pkgs(json!({
            "node_modules/a": {}, "node_modules/b": {},
            "node_modules/c": {}, "node_modules/d": {},
        }));
        assert_eq!(check(lock, tree).unwrap_err().len(), 3);
    }

    #[test]
    fn missing_packages_key_is_an_empty_tree() {
        let v1 = json!({ "lockfileVersion": 1, "dependencies": { "a": { "version": "1" } } });
        assert_eq!(check(v1.clone(), v1), Ok(()));
        let tree = pkgs(json!({ "node_modules/a": { "version": "1" } }));
        let v1 = json!({ "lockfileVersion": 1 });
        assert!(check(v1, tree).is_err());
    }

    #[test]
    fn unparsable_input_does_not_match() {
        assert!(tree_matches(b"{", b"{}").is_err());
        assert!(tree_matches(b"{}", b"nope").is_err());
    }

    #[test]
    fn truthiness_follows_javascript() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&json!(null))));
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!(""))));
        assert!(truthy(Some(&json!(1))));
        assert!(truthy(Some(&json!("x"))));
        assert!(truthy(Some(&json!({}))));
        assert!(truthy(Some(&json!([]))));
    }
}
