//! `fast-worktree plan [<path>]`: for each target, where its files would
//! come from (a cache entry, another worktree, an incremental install, an
//! install) and why the sources before that one are not used. It moves,
//! clones and installs nothing, but it runs the `key_commands` (`node -v`
//! for npm) and `verify`, here and in another worktree whose `path` has no
//! record of its key. Those are the repository's own shell commands, as
//! `install` and `after` are (which the `post-checkout` hook runs too),
//! trusted as the `post_add` hooks are. `verify` never runs in a cache
//! entry: a clone is checked once it is made, which `plan` says.
//!
//! With `<path>`, for that worktree as it is: what `install` would do
//! there. Without, for a new worktree with the files of the current one:
//! nothing is present yet, and the current worktree is one to clone from.

use super::install;
use crate::ecosystem::{target, Mode};
use crate::error::Result;
use crate::hooks::Config;
use crate::util::outln;
use crate::{git, util};
use std::path::Path;

pub fn run(path: Option<&Path>) -> Result<i32> {
    let fresh = path.is_none();
    let root = git::toplevel(path.unwrap_or(Path::new(".")))?;
    let root = root.canonicalize().unwrap_or(root);
    let cfg = Config::load(&root)?;
    let targets = target::targets(&root, &cfg)?;
    let ctx = install::context(&root, &cfg)?;
    if fresh {
        outln!(
            "plan for a new worktree with the files of {}",
            util::tilde(&root)
        );
    } else {
        outln!("plan for {}", util::tilde(&root));
    }
    if targets.is_empty() {
        outln!("nothing to install: no lockfile tracked and no [[clone]]");
        return Ok(0);
    }
    for t in &targets {
        let plan = t.preset.plan(&ctx, t, Mode::Missing, fresh);
        outln!("{}", t.label());
        outln!("  from  {}", plan.from);
        for w in &plan.why_not {
            outln!("  not   {w}");
        }
    }
    Ok(0)
}
