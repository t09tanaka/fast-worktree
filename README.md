# fast-worktree

Create a git worktree with its dependencies in seconds, and remove it in
one.

```sh
fast-worktree add ../app-feature -b feature/x   # worktree + node_modules, ready to work
fast-worktree install                           # dependencies for an existing worktree
fast-worktree check --quiet                     # what is missing
fast-worktree remove ../app-feature             # gone at once, deleted in the background
```

## What & why

A worktree per task keeps parallel work apart, but each new worktree starts
without `node_modules`. In a monorepo with several `package-lock.json`
files that means several `npm ci` runs before anything else can happen,
and removing the worktree again means deleting tens of thousands of files.

`fast-worktree` makes both ends cheap:

- **Clone, don't install.** On macOS (APFS), a whole `node_modules`
  directory is cloned copy-on-write with one `clonefile(2)` call. Cloning
  file by file (`cp -Rc`) is about as slow as `npm ci`; one call on the
  directory is what makes it fast.
- **A cache keyed by what matters.** Every install result whose tree
  matches the lockfile is kept under the lockfile's hash and the `node -v`
  the project resolves (native addons are built for one Node ABI). The
  next worktree with the same lockfile clones it.
- **Other worktrees as a source.** Without a cache entry, a registered
  worktree (the main checkout included) is cloned from when its lockfile is
  byte-identical, its installed tree matches that lockfile (compared with
  `node_modules/.package-lock.json`: versions, integrity, resolved sources
  and links, so a checkout that pulled but did not reinstall is not used,
  nor one whose git dependency moved to another commit), and its Node
  version is the same.
- **Only the difference when the lockfile changed.** Without either, the
  project's most recently used cache entry (an older lockfile, same Node
  version) is cloned and `npm install` brings it up to date. The result is
  kept only if `package-lock.json` is byte-identical afterwards and the
  tree matches it; otherwise it is thrown away and `npm ci` runs.
- **`npm ci` only when nothing matches.** Projects are handled in
  parallel.
- **Flutter and Dart too.** A tracked `pubspec.lock` is a project: `flutter
  pub get --offline` (then online), or `dart pub get` without Flutter.
- **Ignored files come along.** `.env` and other gitignored files listed
  in `.worktreeinclude` are copied into each new worktree.
- **Removal by rename.** `remove` renames the worktree into a trash
  directory on the same volume, removes its own entry from git (and no
  other), and deletes the files in a detached background process.

## Install

```sh
cargo install fast-worktree
```

Or download a binary from the
[releases](https://github.com/t09tanaka/fast-worktree/releases).

## Usage

```sh
fast-worktree add <path> -b <branch> [<start-point>]
```

Runs `git worktree add <path> -b <branch> <start-point>`, copies the files
`.worktreeinclude` selects, then runs `install` in the new worktree, then
the `post_add` hooks. Without `<start-point>`, the new branch starts from
origin's default branch, freshly fetched: it is read from
`refs/remotes/origin/HEAD` (set with `git remote set-head origin -a` when
missing), never assumed to be `main`. `git worktree add` runs with
`FAST_WORKTREE_SKIP=1`, so a [post-checkout hook](#post-checkout-hook) stays
out of the way.

```sh
fast-worktree install [--root <dir>] [--changed] [<project-dir>...]
```

For every tracked `package-lock.json` and `pubspec.lock` (or only the
given project directories), makes the dependencies present. npm projects:
from the cache, else from a matching worktree, else incrementally from the
latest cache entry, else with `npm ci --prefer-offline --no-audit
--no-fund`. Dart projects: `pub get` as above; with neither `flutter` nor
`dart` installed, a note and nothing else. A project whose dependencies
are present is skipped; for npm that means `node_modules` has its
`node_modules/.package-lock.json`, and a `node_modules` without one (an
install that did not finish) is moved to the trash and installed again.
With `--changed`, a project is installed again when its lockfile changed
since fast-worktree last installed it (for npm: unless the installed tree
already matches the new lockfile) or, for npm, when the project now
resolves another Node version. Then the `post_add` hooks run.

```sh
fast-worktree check [--quiet]
```

Lists the projects without `node_modules/.package-lock.json` (npm) or
`.dart_tool/package_config.json` (pub). It always exits 0 and runs nothing
but two quick `git` calls. `--quiet` prints only when something is missing.

```sh
fast-worktree check --post-checkout <prev> <new> <flag>
```

For a `post-checkout` hook; see [below](#post-checkout-hook).

```sh
fast-worktree remove <path> [--delete-branch]
```

Refuses (exit 1) the main checkout, a path that is not a worktree root, a
locked worktree, a worktree with another registered worktree inside it
(even in an ignored directory), a worktree with uncommitted or untracked
changes, and a worktree with submodules. Changes are what `git status
--porcelain --untracked-files=all --ignore-submodules=none` prints, so
`status.showUntrackedFiles=no` hides nothing; ignored files do not count,
and a `git status` that fails counts as changes. With `--delete-branch`, it
also refuses a branch that `git branch -d` would keep: one not merged into
its upstream (or into `HEAD` when it has none). And it refuses a worktree
that cannot be renamed (its parent directory is not writable, or it is a
mount point). All of this is decided before any hook runs and before
anything is removed.

Otherwise runs the `pre_remove` hooks, renames the worktree away, deletes
its entry under `.git/worktrees/`, deletes the files in the background
and, with `--delete-branch`, runs `git branch -d`. It does not run `git
worktree prune`, which would also unregister other worktrees that are only
out of sight, such as one on a volume that is not mounted. If the rename
still fails after the hooks ran, it says so and exits 1; the worktree stays
where it was, registered.

```sh
fast-worktree cache ls
fast-worktree cache prune [--older-than <days>]
```

`ls` lists the cache entries with their repository, project directory, key
and last use. `prune` moves the entries not used for `<days>` (default 7)
to the trash.

### Example

[`examples/monorepo`](examples/monorepo) has two npm projects with one
small dependency each and a Dart package without dependencies. `web` has
a `postinstall` that writes a file outside `node_modules`; `api` has one
that generates code into `node_modules`, which `.fast-worktree.toml` skips
when its input is unchanged. `.worktreeinclude` lists `.env`. Committed as
its own repository and cloned to `~/monorepo`, with a `.env` in the main
checkout, on an Apple Silicon Mac:

```console
$ fast-worktree add ../monorepo-a -b feature/a
Preparing worktree (new branch 'feature/a')
branch 'feature/a' set up to track 'origin/main'.
HEAD is now at e949c07 init
worktree added in 0.0s
copy   .env
npm ci api (0.5s)
pub get mobile (flutter, 0.5s)
npm ci web (0.5s)
deps done in 0.5s
hook   post_add: echo "ready: $(basename "$FAST_WORKTREE_PATH")"
ready: monorepo-a
```

The second worktree clones both npm projects from the cache. `web`'s own
`postinstall` runs again in the clone; `api`'s is skipped, because the
schema it generates from is the one the cloned `node_modules` was
generated from:

```console
$ fast-worktree add ../monorepo-b -b feature/b
Preparing worktree (new branch 'feature/b')
branch 'feature/b' set up to track 'origin/main'.
HEAD is now at e949c07 init
worktree added in 0.0s
copy   .env
note   api: postinstall skipped, inputs unchanged
clone  api from ~/Library/Caches/fast-worktree/monorepo-fc34b8e7/api/263f7cb8e0876026-v20.13.1 (0.1s)
pub get mobile (flutter, 0.3s)
clone  web from ~/Library/Caches/fast-worktree/monorepo-fc34b8e7/web/02ed81ddfaf462f9-v20.13.1 (0.3s)
deps done in 0.4s
hook   post_add: echo "ready: $(basename "$FAST_WORKTREE_PATH")"
ready: monorepo-b
$ cat ../monorepo-b/.env
API_TOKEN=dev-only
$ cat ../monorepo-b/web/generated.txt
one day is 86400000 ms
$ cat ../monorepo-b/api/node_modules/.codegen/index.js
module.exports = ["id","name"];
```

After a dependency of `web` changes, its new lockfile has no cache entry
and no worktree has it installed, so the old entry is cloned and updated
with `npm install`:

```console
$ (cd web && npm install --package-lock-only --no-audit --no-fund ms@2.1.2)
$ git commit -qam "web: ms 2.1.2"
$ fast-worktree add ../monorepo-c -b feature/c HEAD
Preparing worktree (new branch 'feature/c')
HEAD is now at 4cf433c web: ms 2.1.2
worktree added in 0.0s
copy   .env
note   api: postinstall skipped, inputs unchanged
clone  api from ~/Library/Caches/fast-worktree/monorepo-fc34b8e7/api/263f7cb8e0876026-v20.13.1 (0.1s)
pub get mobile (flutter, 0.3s)
delta  web from ~/Library/Caches/fast-worktree/monorepo-fc34b8e7/web/02ed81ddfaf462f9-v20.13.1 (0.5s)
deps done in 0.5s
hook   post_add: echo "ready: $(basename "$FAST_WORKTREE_PATH")"
ready: monorepo-c
```

With the [post-checkout hook](#post-checkout-hook) installed, a plain `git
worktree add` gets what can be cloned, and a `git switch` lists what is
missing:

```console
$ git worktree add -q ../monorepo-d -b feature/d HEAD
note   api: postinstall skipped, inputs unchanged
clone  api from ~/Library/Caches/fast-worktree/monorepo-fc34b8e7/api/263f7cb8e0876026-v20.13.1 (0.1s)
clone  web from ~/Library/Caches/fast-worktree/monorepo-fc34b8e7/web/a1fd1fb4cf620116-v20.13.1 (0.3s)
missing deps: mobile (pub)
run `fast-worktree install` to install them
$ git -C ../monorepo-d switch -q -c feature/e
missing deps: mobile (pub)
run `fast-worktree install` to install them
```

The cache, and `remove`:

```console
$ fast-worktree cache ls
REPO               DIR  KEY                        LAST USED
monorepo-fc34b8e7  api  263f7cb8e0876026-v20.13.1  2026-10-26 (0d ago)
monorepo-fc34b8e7  web  02ed81ddfaf462f9-v20.13.1  2026-10-26 (0d ago)
monorepo-fc34b8e7  web  a1fd1fb4cf620116-v20.13.1  2026-10-26 (0d ago)
3 entries in ~/Library/Caches/fast-worktree
$ git -C ../monorepo-a commit -q --allow-empty -m "work in progress"
$ fast-worktree remove ../monorepo-a --delete-branch
fast-worktree: branch feature/a is not merged into origin/main, so `git branch -d` would keep it; nothing was removed. Merge it first, or run remove without --delete-branch
$ fast-worktree remove ../monorepo-b --delete-branch
hook   pre_remove: echo "bye: $(basename "$FAST_WORKTREE_PATH")"
bye: monorepo-b
removed ~/monorepo-b (moved to ~/Library/Caches/fast-worktree/trash/1793012885-22119-0303526000-monorepo-b, deleting in background)
deleted branch feature/b
```

The dependencies here are tiny; the clone stays one syscall per project
however large `node_modules` is, while `npm ci` grows with it.

## Settings: `.fast-worktree.toml`

fast-worktree knows nothing about the tools a repository uses. Anything
repository-specific goes in an optional `.fast-worktree.toml` at the
worktree root:

```toml
[[post_add]]            # after add / install, in the worktree root, in order
run = "make setup"
requires = "make"       # optional: skipped silently when not on PATH or in ~/.cargo/bin

[[pre_remove]]          # after remove's safety checks, before the worktree is moved
run = 'docker compose -p "$(basename "$FAST_WORKTREE_PATH")" down -v --remove-orphans'
requires = "docker"

[[skip_lifecycle]]      # after a clone, do not run this script of this project...
dir = "api"             # (relative to the worktree root; default ".")
script = "postinstall"
# ...when every pair of files (relative to the project) is byte-identical
if_equal = [["schema.json", "node_modules/.codegen/schema.json"]]
```

- `run` is executed with `sh -c` in the worktree root, with
  `FAST_WORKTREE_PATH` (the worktree's absolute path) and
  `FAST_WORKTREE_MAIN` (the main checkout's absolute path) set.
- A failing `post_add` hook makes the command exit non-zero. A failing
  `pre_remove` hook is reported and the removal goes on.
- `skip_lifecycle` is for a project's own script that generates into
  `node_modules` and records its input there, as code generators commonly
  do: when the input is unchanged, the cloned `node_modules` already holds
  the output. A file that does not exist is never equal, so the script
  runs. It applies only where fast-worktree runs the script itself, after a
  clone; `npm ci` and `npm install` run scripts on their own.
- No file, no hooks. Unknown keys are warned about and ignored.

## Ignored files: `.worktreeinclude`

A `.worktreeinclude` in the main checkout's root lists, in `.gitignore`
syntax, files to copy from the main checkout into each worktree `add`
creates. It is the format Claude Code reads for its own worktrees:

```text
.env
.env.local
config/secrets.json
```

Only files that match a pattern **and** are gitignored in the main
checkout are copied, so tracked files and untracked files that git does
not ignore never are. Nothing that exists in the new worktree is
overwritten. A directory that git ignores as a whole (such as
`node_modules/`) is looked into only when a pattern can reach it: the
directory itself matches, an anchored pattern starts with its path (write
`vendor/**/config.json`, not `**/config.json`), or, for a pattern starting
with `**/` (or without a slash), the first name after `**/` is one of the
directory's names. Another repository is never copied: a registered
worktree, or a directory with a `.git` of its own, is left out. A matching
directory with one inside (such as `.claude/` holding
`.claude/worktrees/<name>/`) is copied entry by entry around it. Copies are
clones on macOS.

## Post-checkout hook

`check --post-checkout` takes the three arguments git passes to a
`post-checkout` hook. Add it to the repository's hook (`.git/hooks/post-checkout`,
or the file your hook manager runs):

```sh
#!/bin/sh
command -v fast-worktree >/dev/null 2>&1 && fast-worktree check --post-checkout "$@" || true
```

Where fast-worktree is not installed, the hook does nothing.

- After `git worktree add` or `git clone` (the previous commit is all zeros
  and the flag is 1), the dependencies that the cache or another worktree
  can provide are cloned in, with the project's own lifecycle scripts as
  after any clone (so it runs `node -v` and those scripts). Nothing that
  downloads or resolves runs: projects that would need `npm ci`, `npm
  install` or `pub get` are listed as missing. (A fresh `git clone` has no
  cache entries or other worktrees of its own yet, so there it only
  lists.)
- On any other checkout, such as `git switch`, it lists what is missing,
  like `check --quiet`.
- It always exits 0, also when its output goes to a pipe that is already
  closed. With the environment variable `CI` set, or
  `FAST_WORKTREE_SKIP=1`, it does nothing.

## Where things live

- Cache: `~/Library/Caches/fast-worktree` on macOS, else
  `$XDG_CACHE_HOME/fast-worktree` (default `~/.cache/fast-worktree`).
  `FAST_WORKTREE_CACHE` overrides it. Entries are
  `<repo>-<hash>/<project>/<lockfile sha256, 16 digits>-<node -v>`; the
  repository part is the main checkout's name plus a hash of its real path,
  so two checkouts with the same name never share entries. Next to an
  entry, `<entry>.ok` records that it was checked when stored (below).
  Only a tree whose `.package-lock.json` matches the lockfile is stored.
  Entries unused for 7 days are removed; `cache prune` does it on demand.
  Clones share blocks with their source, so the cache costs almost no disk
  while worktrees made from it exist.
- `node_modules/.fast-worktree-key` and `.dart_tool/fast-worktree-key`
  record what an install was made from, for `install --changed`. A tree
  without one is compared with its lockfile.
- Trash: `<cache>/trash`. When the trash is on another volume than the
  directory being deleted, that directory is renamed to a hidden
  `.fast-worktree-trash-*` sibling instead, and a note in the trash records
  where. Each run deletes again whatever an interrupted background deletion
  left in the trash or in a sibling it has a note for.

## What is safe to clone

A cloned `node_modules` is the same tree an install would produce for that
lockfile and Node version. Dependencies' install scripts wrote their output
inside `node_modules`, so the clone carries it. After cloning:

- `node_modules/.cache` is deleted (tool caches often embed absolute
  paths).
- A tree with a symlink whose target is absolute, which could point into
  the worktree it came from, is not used: a clone from another worktree is
  checked and dropped, and an install result is checked before it is
  stored in the cache, and then not stored. The check walks every file, so
  for the cache it runs once per entry, when the entry is stored, not on
  each clone.
- The project's own `preinstall`, `install`, `postinstall` and `prepare`
  scripts, which may write outside `node_modules` (git hooks setup, code
  generation), are run again if its `package.json` has them, unless a
  `skip_lifecycle` rule applies. If one fails, the clone is removed, so the
  next `install` does not take a broken tree for an installed one.

## Platforms

Cloning needs macOS `clonefile(2)`. On other platforms the clone and delta
stages are skipped and every npm project gets `npm ci`; the post-checkout
hook only lists. `add`, `check`, `remove`, `.worktreeinclude`, Dart
projects and the cache commands work the same.

## Not yet

Planned for later versions: a pool of pre-made worktrees, and entry points
for editor and agent worktree hooks.

## License

MIT
