# Changelog

All notable changes to fast-worktree are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/), and this project
adheres to [Semantic Versioning](https://semver.org/) (pre-1.0: a breaking
change bumps the minor version).

## [0.3.0] - 2026-10-27

### Added

- `[[clone]]` in `.fast-worktree.toml`: one kind of entry for what each
  worktree gets. `preset = "npm"` and `preset = "dart"` with a `dir` fill
  in what a detected project gets (`path`, `key`, `key_commands`, the
  install and its checks); any of `key`, `key_commands`, `install`,
  `verify`, `rewrite_paths` and `after` replaces a preset's default. A
  `[[clone]]` without a preset names a directory (`path`) and the files
  whose contents decide whether a copy can be reused (`key`), and is made
  present from the cache, else from a registered worktree whose `path`
  records the same key (or, without that record, whose key files are
  byte-identical, whose `key_commands` print the same and where `verify`
  passes), else by its `install`, whose result is cached. After a clone,
  `verify` must pass, `rewrite_paths` replaces the source worktree's path
  with the new one in its text files, and `after` runs. The key is
  recorded in `<path>/.fast-worktree-key` only once all of that succeeded,
  and only a `path` with that record (or where `verify` passes) counts as
  installed, for `check`, `install` and as a source; when the install,
  `verify` or an `after` fails, `path` is moved away. A `path` that is
  there without that record and that `verify` does not vouch for (a
  build's output, a hand-made `Pods`) is not fast-worktree's: `install`
  leaves it as it is and says so, `check` lists it apart without asking
  for an install, and only `install --changed` makes it again. A `path`
  that is made again (that one, or one recorded from other key files) is
  moved away only once its replacement is complete, and put back when
  nothing can replace it (no cache entry, no worktree, no `install`) or
  the replacement fails; the Bundler preset's directory in the worktree
  is handled the same way. Targets with a preset are installed before
  those without.
- fast-worktree touches only what is its own: a `[[clone]]` (or a project
  found by its lockfile) whose `dir`, `path` or `key` leads out of the
  worktree through a symlink, whose `path` holds tracked files, or whose
  `path` holds another target's `path` or `dir`, is refused with the
  reason, and `path` is looked at again right before it is moved away,
  cloned into, rewritten or handed to the cache. A `[[clone]]` that is
  refused stops the command; a project found by its lockfile (without any
  `[[clone]]`) is left out with a warning on stderr, and `add`, `hook
  create`, `check` and `install` go on with the others.
- When a `.fast-worktree.toml` has at least one `[[clone]]`, only what the
  entries list is handled; without any, the tracked lockfiles are detected
  as before, with the same cache keys.
- `remove`, and `hook remove` for a worktree it does not keep, move the
  directory of each `[[clone]]` without a preset into the cache once the
  worktree has been moved into the trash (a rename), when it matches its
  key or passes `verify` and the cache has no entry for that key, so the
  next worktree clones it. A removal that stops before that leaves the
  worktree whole.
- `config init [--dry-run]` adds a `[[clone]]` for each tracked lockfile
  that has none, keeping the rest of the file and its comments.
- `config init` stops, writing nothing, when a `[[clone]]` does not
  resolve, rather than add a second entry for its project.
- `config check` exits 1 when a tracked lockfile is in no `[[clone]]`, a
  `[[clone]]` names a file that does not exist or is not tracked, or the
  file has a mistake: it does not parse, an unknown key or section (such
  as `[[clones]]`), an `[[ignore]]` without a `reason`, an unknown preset,
  a `path` that does not fit its preset, or a target fast-worktree refuses
  to touch (above). It runs a few `git ls-files`, for pre-commit hooks and
  CI. Without `[[clone]]` it does not compare the lockfiles but reports
  the rest.
- `plan [<path>]` lists, for each target, where its files would come
  from (a cache entry, a worktree, an incremental install, an install)
  and why the sources before it are not used. It installs nothing, but
  runs the `key_commands` and `verify` (never in a cache entry: a clone is
  checked once it is made, and `plan` says so), which, like `install` and
  `after`, are the repository's own commands, run by the post-checkout
  hook too and trusted as `post_add` hooks are.
- A Bundler preset, `preset = "bundler"`, and a tracked `Gemfile.lock` is
  a project for auto-detection, `config init` and `config check`. The
  directory Bundler installs into (`BUNDLE_PATH` from the project's
  `.bundle/config`, the environment or `~/.bundle/config`, else
  `<dir>/vendor/bundle`) is cloned from the cache or a worktree with the
  same `Gemfile.lock`, `ruby -v` and `RUBY_PLATFORM`, then checked with
  `bundle check --dry-run` (a clone it turns down is thrown away); else
  `BUNDLE_PATH=<path> bundle install` runs and its result is cached. A
  `BUNDLE_PATH` outside the worktree (or through a symlink out of it), or
  one that holds tracked files, is never cloned or moved away: `bundle
  check --dry-run`, else `bundle install`. Every check has `--dry-run`, so
  none writes `Gemfile.lock`.
- `[[ignore]]` in `.fast-worktree.toml` (`dir`, an optional `preset`, and
  a required `reason`) leaves a project out on purpose: auto-detection
  skips it, `config init` does not add it, and `config check` does not
  report its lockfile as unlisted. `config check` reports an `[[ignore]]`
  whose directory has no tracked lockfile as missing, and one that a
  `[[clone]]` also lists as invalid.

### Changed

- `remove` no longer stops at a `.fast-worktree.toml` it cannot read: it
  says so, runs no `pre_remove` hook, keeps nothing in the cache, and
  removes the worktree after its usual checks, as `hook remove` does
  (which no longer runs hooks from an empty config in its place). The
  warning says first that no `pre_remove` hook was run, and names the
  file it could not read.

### Fixed

- Two runs storing the same cache entry at once could leave it with the
  records (`<entry>.src`, `<entry>.ok`) of the one whose copy was not
  kept. The check that the entry is not there, the records and the rename
  into place are now done under a lock in the project's directory of the
  cache.
- The post-checkout hook (`check --post-checkout`) cloned the detected
  projects in a new worktree whose `.fast-worktree.toml` it could not read;
  it now says so and does nothing, as plain `check` does.
- `wait` could report "the background install stopped before it
  finished" for an install that was running: the marker was rewritten in
  place when the background process put its pid in it, and a marker read
  empty in that moment looked interrupted. The marker and the result file
  are now written to a temporary file and renamed over the old one.

## [0.2.1] - 2026-10-27

### Changed

- `npm ci` no longer holds the repository's lifecycle lock, so the
  projects of a first install download at once instead of one after
  another. It runs with `--ignore-scripts`; then, holding the lock, `npm
  rebuild <names>` runs the install scripts of the dependencies that have
  one and `npm run <script>` the project's own `preinstall`, `install`,
  `postinstall` and `prepare`, in npm's order. The tree is the one `npm
  ci` makes, `node_modules/.package-lock.json` and build output included.
  The incremental install does the same with `npm install
  --ignore-scripts`, rebuilding only the dependencies that are new or
  changed. Until the scripts are done, the tree carries
  `node_modules/.fast-worktree-scripts-pending` and does not count as
  installed. A lockfile with a git dependency or a link (workspaces), and
  a project whose `npm rebuild` fails, are installed with their scripts
  under the lock as before; with npm's `ignore-scripts` set, no script
  runs.
- The time a step waited for the lifecycle lock is printed apart from the
  time it took: `clone  . from … (0.3s, waited 4.1s for lifecycle lock)`.
  Before, a clone of a few kilobytes could show the minute it waited for
  other projects' `npm ci`.
- `add` runs `git worktree add` with `--quiet`: git's `Updating files: N%`
  progress and its "Preparing worktree" lines no longer fill the output.
  Its errors are still printed.
- `remove --delete-branch` counts a branch as merged when its commit is
  reachable from its upstream, `origin/<default>` or the local
  `<default>` branch, and deletes it with `git branch -D` if it has not
  moved since. Before, a branch without an upstream was compared with the
  main checkout's `HEAD`, so a merged branch was kept when the main
  checkout was behind or on another branch. An unmerged branch still stops
  `remove` before anything is removed.
- `cache prune` prints each entry as `cache ls` names it (`.` for the root
  project, not `_root`).

### Added

- `install root` installs the project at the repository root, and a
  project can be named by the last part of its directory (`lambda` for
  `provision/lambda`) when no other project's ends the same way; otherwise
  the error lists them. An unknown project's error lists the projects.
- `cache ls` lists the directories of the layout before 0.2.0 (a
  repository's name without the hash) apart, under `LEGACY`, instead of
  as entries with shifted columns, and `cache prune --legacy` moves them to
  the trash.

### Fixed

- The project's own `install` script, after a clone, no longer runs its
  `preinstall` and `postinstall` a second time (`npm run` is called with
  `--ignore-scripts`, which runs the named script alone).

## [0.2.0] - 2026-10-27

The first published release. It includes everything listed under
[0.1.0](#010---2026-10-27), which was not published on its own; the
changes and fixes below are relative to it.

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
  branch, installs only the projects whose lockfile changed, runs the
  other projects' own install steps again where the worktree now is, and
  when it is done starts `pool fill` in the background, in a session of
  its own (after a background install of `hook create`, when there is
  one). Without a spare, or when the move fails, `add` creates the
  worktree as before, counts a miss, and starts the refill the same way.
  The repository's own git hooks do not run when a spare is made; they run
  on the `git switch` in the worktree where it ends up.
- `pool status`, `pool fill`, `pool refresh` and `pool clear`.
- `hook create` and `hook remove` for Claude Code's `WorktreeCreate` and
  `WorktreeRemove` hooks, and `hook print-config` to print them for a
  settings file. `hook create` makes `<main>/.claude/worktrees/<name>` on
  branch `worktree-<name>` from origin's default branch, prints only the
  path on stdout, installs what can be cloned and leaves the rest to a
  background `install --changed`, in a session of its own; it fails only
  when git does. It always starts from origin's default branch (Claude
  Code's `worktree.baseRef` is not read). `hook remove` deletes the
  worktree even with changes and deletes `worktree-<name>` only when its
  commit is on another ref. It keeps the worktree in the trash for at least
  24 hours, with the command to restore it, when it has uncommitted or
  untracked changes (whatever `status.showUntrackedFiles` says), when it
  has submodules checked out (their git directories are moved into them
  first, so unpushed submodule commits go with it) or when another
  registered worktree is inside it (that one stays registered where it is now). Any
  checked-out submodule counts, whatever the layout of its git directory.
  If a submodule's git directory cannot be moved, the ones already moved
  are put back. Commits that would be lost are kept in a group of refs per
  removal, `refs/fast-worktree/kept/<name>-<date>-<time>/`: `head` for a
  kept worktree's `HEAD` that is not on a branch that stays (and for a
  detached `HEAD` on no other ref), `sub/<path>` for each checked-out
  submodule's `HEAD` of a kept worktree, fetched into the main repository.
  A detached worktree inside gets its own group. It stops a background
  install still running in the worktree first. It removes only the
  worktree's own entry from git, also when the path is already gone, and
  never runs `git worktree prune`. It refuses, before the `pre_remove`
  hooks, a worktree the user locked, one that cannot be renamed away, and
  one with a submodule git directory that cannot go with it.
- Fetches of `hook create` and `pool fill` never wait for a prompt:
  `GIT_TERMINAL_PROMPT=0`, and `-o BatchMode=yes` added to the user's
  `GIT_SSH_COMMAND` or `core.sshCommand` (or `ssh`). `hook create` gives
  its fetch at most 5 seconds and `pool fill` 60; after that, or when the
  fetch fails, they go on from origin's default branch as last fetched.
- A spare from the pool keeps no `node_modules` with an absolute symlink,
  and after it is moved into place its npm projects' own install scripts
  run again there, and in the ignored files Flutter generates in its Dart
  projects (`.dart_tool/`, `ios/Flutter/`, `*/flutter/ephemeral/`,
  `macos/Flutter/ephemeral/`, `.flutter-plugins*`) the path where it was
  made is replaced with the new one (text files only).
- The projects' own lifecycle runs (`npm ci`, `npm install`, and the
  scripts run again after a clone or a move) take a lock per repository,
  `<cache>/<repo>/lifecycle.lock`, so that only one runs at a time, and
  one that fails because git could not lock a config file is run again
  (at most twice).
  `pool clear` and a spare that cannot be used remove only their own
  entries from git.
- `wait [<path>]`: wait for the background install of `hook create`;
  `check` says when one is running.
- `kept ls` and `kept prune [--older-than <days>]` (default 30, going by
  the date in the ref's name) for `refs/fast-worktree/kept/`. `git push
  --mirror` pushes those refs; the README says so.
- `install` leaves alone a `node_modules` that yarn (`.yarn-integrity`,
  `.yarn-state.yml`), pnpm (`.modules.yaml`) or bun (`.bun`, or no
  `.package-lock.json` next to `bun.lock` / `bun.lockb`) made, with a note,
  instead of moving it away and running `npm ci`.

### Changed

- The fetch of `add` (without a start point) runs like those of `hook
  create` and `pool fill`: it never waits for a prompt, and it is stopped
  after `--fetch-timeout <secs>` (default 30). When it fails or is
  stopped, the branch starts from origin's default branch as last fetched,
  with a note on stderr; only when there is no such ref does `add` fail.
  Before, a credential helper that waited without a terminal held `add` up
  for good.
- Cache entries are checked for absolute symlinks once, when stored, and
  marked with `<entry>.ok`; clones from the cache no longer walk the tree.
  An install result with an absolute symlink is not cached.
- `check` names the ecosystem: `missing deps: api (npm)`.
- `remove` points to `pool clear` when asked to remove a spare.
- `remove` stops a background install of `hook create` still running in
  the worktree before moving it, as `hook remove` does.
- The tree check compares a registry tarball's `resolved` without its
  scheme and host, so a tree installed through a registry mirror matches;
  git (`git+...`, `#<commit>`), `file:` and link sources are compared
  whole.
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
- No command panics (exit code 101) any more when its stdout or stderr is
  a pipe whose reader has gone, as in `fast-worktree pool status | head
  -1`: what cannot be written is dropped and the command carries on.
  `check` exits 0 then, as always.
- `wait` no longer reports a background install that finished between
  its look at the marker and its check of the process as interrupted.
- `install --changed` installs again when a git dependency moved to
  another commit (the tree check now compares `resolved`).
- `remove` finds the entry of a worktree whose `gitdir` file is relative
  (`worktree.useRelativePaths`).
- A tree installed from a `lockfileVersion` 1 lockfile, which has no
  `packages` to compare, is cached when npm recorded it in
  `node_modules/.package-lock.json`.

## [0.1.0] - 2026-10-27

Not published on its own: it is part of 0.2.0, the first published
release.

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
