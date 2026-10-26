# Changelog

All notable changes to fast-worktree are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/), and this project
adheres to [Semantic Versioning](https://semver.org/) (pre-1.0: a breaking
change bumps the minor version).

## [Unreleased]

## [0.2.0]

### Added

- Incremental npm installs: when no cache entry or worktree matches a
  changed lockfile, the project's most recently used cache entry for the
  same Node version is cloned and updated with `npm install
  --prefer-offline --no-audit --no-fund`. The result is used only if
  `package-lock.json` is unchanged afterwards and the installed tree
  matches it; otherwise the lockfile is restored and `npm ci` runs.
  Printed as `delta  <dir> from <entry>`.
- `[[skip_lifecycle]]` in `.fast-worktree.toml` (`dir`, `script`,
  `if_equal`): after a clone, a project's own lifecycle script is not run
  when every listed pair of files is byte-identical. A missing file is
  never equal.
- `.worktreeinclude` (`.gitignore` syntax, read from the main checkout):
  `add` copies the gitignored files it matches into the new worktree,
  without overwriting anything there.
- Dart and Flutter projects, one per tracked `pubspec.lock`: `flutter pub
  get --offline`, then online if that fails, or `dart pub get` without
  Flutter; a note when neither is installed.
- `check --post-checkout <prev> <new> <flag>` for a `post-checkout` hook:
  on a new worktree or clone it clones what the cache or other worktrees
  have and lists the rest; on other checkouts it only lists. Always exits
  0; does nothing with `CI` set or `FAST_WORKTREE_SKIP=1`. `add` sets
  `FAST_WORKTREE_SKIP=1` for its `git worktree add`.
- `install --changed`: also install again where the lockfile or the Node
  version changed since the last install (recorded in
  `node_modules/.fast-worktree-key` and `.dart_tool/fast-worktree-key`).
- `cache ls` and `cache prune [--older-than <days>]`.
- A pool of ready worktrees, off unless `[pool] size` is set in the main
  checkout's `.fast-worktree.toml` or in
  `$XDG_CONFIG_HOME/fast-worktree/config.toml` (default
  `~/.config/fast-worktree/config.toml`). Spares are made under
  `<cache>/<repo>/pool` with `git worktree add --detach` of origin's default
  branch, `install` and `git worktree lock`. `add` takes one by renaming
  it, moves it into place with `git worktree move`, switches to the new
  branch, installs only the projects whose lockfile changed, runs a
  project's own install scripts again where its tracked files changed, and
  starts `pool fill` in the background. Without a spare, or when the move
  fails, `add` creates the worktree as before and counts a miss.
- `pool status`, `pool fill`, `pool refresh` and `pool clear`.
- `hook create` and `hook remove` for Claude Code's `WorktreeCreate` and
  `WorktreeRemove` hooks, and `hook print-config` to print them for a
  settings file. `hook create` makes `<main>/.claude/worktrees/<name>` on
  branch `worktree-<name>` from origin's default branch, prints only the
  path on stdout, installs what can be cloned and leaves the rest to a
  background `install --changed`; it fails only when git does. `hook
  remove` deletes the worktree even with changes (kept in the trash for at
  least 24 hours, with the command to restore them) and deletes
  `worktree-<name>` only when its commit is on another ref.
- `wait [<path>]`: wait for the background install of `hook create`;
  `check` says when one is running.

### Changed

- Cache entries are checked for absolute symlinks once, when stored, and
  marked with `<entry>.ok`; clones from the cache no longer walk the tree.
  An install result with an absolute symlink is not cached.
- `check` names the ecosystem: `missing deps: api (npm)`.
- `remove` points to `pool clear` when asked to remove a spare.
- `cache ls` and `cache prune` leave the pool's spares alone.

### Fixed

- A project's own lifecycle script failing after a clone, or `npm ci`
  failing, no longer leaves a `node_modules` that the next `install`
  skips as present.
- `remove --delete-branch` refuses a branch that `git branch -d` would
  keep before removing anything, instead of removing the worktree and
  then failing.
- `.worktreeinclude` never copies another repository: registered
  worktrees and directories with their own `.git` are left out, and a
  matching directory with one inside (`.claude/` with
  `.claude/worktrees/<name>/`) is copied around it.
- `check` exits 0 when its stdout is a closed pipe, instead of panicking
  with exit code 101.
- `install --changed` installs again when a git dependency moved to
  another commit (the tree check now compares `resolved`).

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
