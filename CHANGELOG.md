# Changelog

All notable changes to fast-worktree are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/), and this project
adheres to [Semantic Versioning](https://semver.org/) (pre-1.0: a breaking
change bumps the minor version).

## [Unreleased]

## [0.1.0]

### Added

- `add <path> -b <branch> [<start-point>]`: `git worktree add`, then
  `install`, then the `post_add` hooks. Without a start point, the branch
  starts from origin's default branch, fetched first.
- `install [--root <dir>] [<project-dir>...]`: for every tracked
  `package-lock.json`, make `node_modules` present by cloning a cache entry
  (keyed by the lockfile's hash and `node -v`), else cloning a matching
  worktree, else `npm ci`. Projects run in parallel. On macOS, clones are
  one `clonefile(2)` call per directory; elsewhere only `npm ci` is used.
  A tree matches a lockfile when `node_modules/.package-lock.json` agrees
  on every entry's version, integrity, resolved source and link; only such
  a tree is cached or cloned from. A `node_modules` without
  `.package-lock.json` is installed again.
- `check [--quiet]`: list projects without
  `node_modules/.package-lock.json`. Always exits 0.
- `remove <path> [--delete-branch]`: refuse the main checkout, non-roots,
  locked worktrees, worktrees with another worktree inside, dirty ones
  (untracked files count whatever `status.showUntrackedFiles` says, and a
  failing `git status` counts as dirty), submodule worktrees, and ones that
  cannot be renamed; otherwise run the `pre_remove` hooks, rename the
  worktree into the trash, delete its own entry under `.git/worktrees/`
  (never `git worktree prune`, which would also drop worktrees on an
  unmounted volume) and delete the files in the background.
- `.fast-worktree.toml` with `post_add` and `pre_remove` hooks (`run`,
  optional `requires`), given `FAST_WORKTREE_PATH` and
  `FAST_WORKTREE_MAIN`.
