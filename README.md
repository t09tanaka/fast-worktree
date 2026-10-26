# fast-worktree

Create a git worktree with its npm dependencies in seconds, and remove it in
one.

```sh
fast-worktree add ../app-feature -b feature/x   # worktree + node_modules, ready to work
fast-worktree install                           # dependencies for an existing worktree
fast-worktree check --quiet                     # what is missing (for a post-checkout hook)
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
- **A cache keyed by what matters.** Every `npm ci` result whose installed
  tree matches the lockfile is kept under the lockfile's hash and the `node -v` the project resolves (native
  addons are built for one Node ABI). The next worktree with the same
  lockfile clones it.
- **Other worktrees as a source.** Without a cache entry, a registered
  worktree (the main checkout included) is cloned from when its lockfile is
  byte-identical, its installed tree matches that lockfile (compared with
  `node_modules/.package-lock.json`: versions, integrity, resolved sources
  and links, so a checkout that pulled but did not reinstall is not used,
  nor one whose git dependency moved to another commit), and its Node
  version is the same.
- **`npm ci` only when nothing matches.** Projects are handled in
  parallel.
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

Runs `git worktree add <path> -b <branch> <start-point>`, then `install` in
the new worktree, then the `post_add` hooks. Without `<start-point>`, the
new branch starts from origin's default branch, freshly fetched: it is read
from `refs/remotes/origin/HEAD` (set with `git remote set-head origin -a`
when missing), never assumed to be `main`.

```sh
fast-worktree install [--root <dir>] [<project-dir>...]
```

For every tracked `package-lock.json` (or only the given project
directories), makes `node_modules` present: from the cache, else from a
matching worktree, else with `npm ci --prefer-offline --no-audit
--no-fund`. A project whose `node_modules` has its
`node_modules/.package-lock.json` is skipped; a `node_modules` without one
(an install that did not finish) is moved to the trash and installed again.
Then the `post_add` hooks run.

```sh
fast-worktree check [--quiet]
```

Lists the projects without `node_modules/.package-lock.json`. It always
exits 0 and runs nothing but two quick `git` calls (no `node`, no
`npm`), so it fits in a `post-checkout` hook. `--quiet` prints only when something is missing.

```sh
fast-worktree remove <path> [--delete-branch]
```

Refuses (exit 1) the main checkout, a path that is not a worktree root, a
locked worktree, a worktree with another registered worktree inside it
(even in an ignored directory), a worktree with uncommitted or untracked
changes, and a worktree with submodules. Changes are what `git status
--porcelain --untracked-files=all --ignore-submodules=none` prints, so
`status.showUntrackedFiles=no` hides nothing; ignored files do not count,
and a `git status` that fails counts as changes. It also refuses, before
any hook runs, a worktree that cannot be renamed (its parent directory is
not writable, or it is a mount point).

Otherwise runs the `pre_remove` hooks, renames the worktree away, deletes
its entry under `.git/worktrees/` and deletes the files in the background.
It does not run `git worktree prune`, which would also unregister other
worktrees that are only out of sight, such as one on a volume that is not
mounted. If the rename still fails after the hooks ran, it says so and
exits 1; the worktree stays where it was, registered. `--delete-branch`
then runs `git branch -d`, which keeps a branch that has unmerged
commits.

### Example

[`examples/monorepo`](examples/monorepo) has two npm projects with one
small dependency each; `web` has a `postinstall` that writes a file outside
`node_modules`, and `.fast-worktree.toml` has a hook for each stage.
Committed as its own repository and cloned to `~/monorepo`, on an Apple
Silicon Mac:

```console
$ fast-worktree add ../monorepo-a -b feature/a
Preparing worktree (new branch 'feature/a')
branch 'feature/a' set up to track 'origin/main'.
HEAD is now at 34296c0 init
worktree added in 0.0s
npm ci api (0.4s)
npm ci web (0.4s)
deps done in 0.4s
hook   post_add: echo "ready: $(basename "$FAST_WORKTREE_PATH")"
ready: monorepo-a
```

The second worktree clones both projects from the cache. `web`'s own
`postinstall` runs again in the clone:

```console
$ fast-worktree add ../monorepo-b -b feature/b
Preparing worktree (new branch 'feature/b')
branch 'feature/b' set up to track 'origin/main'.
HEAD is now at 34296c0 init
worktree added in 0.0s
clone  api from ~/Library/Caches/fast-worktree/monorepo-7ccfcacd/api/263f7cb8e0876026-v20.13.1 (0.1s)
clone  web from ~/Library/Caches/fast-worktree/monorepo-7ccfcacd/web/02ed81ddfaf462f9-v20.13.1 (0.3s)
deps done in 0.3s
hook   post_add: echo "ready: $(basename "$FAST_WORKTREE_PATH")"
ready: monorepo-b
$ cat ../monorepo-b/web/generated.txt
one day is 86400000 ms
```

`check` and `install` in `monorepo-b` after deleting `api/node_modules`,
then `remove` from the main checkout:

```console
$ fast-worktree check
missing deps: api
run `fast-worktree install` to install them
$ fast-worktree install
clone  api from ~/Library/Caches/fast-worktree/monorepo-7ccfcacd/api/263f7cb8e0876026-v20.13.1 (0.1s)
skip   web (node_modules already present)
deps done in 0.1s
hook   post_add: echo "ready: $(basename "$FAST_WORKTREE_PATH")"
ready: monorepo-b
$ fast-worktree remove ../monorepo-b --delete-branch
hook   pre_remove: echo "bye: $(basename "$FAST_WORKTREE_PATH")"
bye: monorepo-b
removed ~/monorepo-b (moved to ~/Library/Caches/fast-worktree/trash/1793010441-6470-0709015000-monorepo-b, deleting in background)
deleted branch feature/b
```

The dependencies here are tiny; the clone stays one syscall per project
however large `node_modules` is, while `npm ci` grows with it.

## Hooks: `.fast-worktree.toml`

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
```

- `run` is executed with `sh -c` in the worktree root, with
  `FAST_WORKTREE_PATH` (the worktree's absolute path) and
  `FAST_WORKTREE_MAIN` (the main checkout's absolute path) set.
- A failing `post_add` hook makes the command exit non-zero. A failing
  `pre_remove` hook is reported and the removal goes on.
- No file, no hooks. Unknown keys are warned about and ignored.

## Where things live

- Cache: `~/Library/Caches/fast-worktree` on macOS, else
  `$XDG_CACHE_HOME/fast-worktree` (default `~/.cache/fast-worktree`).
  `FAST_WORKTREE_CACHE` overrides it. Entries are
  `<repo>-<hash>/<project>/<lockfile sha256, 16 digits>-<node -v>`; the
  repository part is the main checkout's name plus a hash of its real path,
  so two checkouts with the same name never share entries. Entries unused
  for 7 days are removed. Clones share blocks with their source, so the
  cache costs almost no disk while worktrees made from it exist.
- Trash: `<cache>/trash`. When the trash is on another volume than the
  directory being deleted, that directory is renamed to a hidden
  `.fast-worktree-trash-*` sibling instead, and a note in the trash records
  where. Each run deletes again whatever an interrupted background deletion
  left in the trash or in a sibling it has a note for.

## What is safe to clone

A cloned `node_modules` is the same tree `npm ci` would produce for that
lockfile and Node version. Dependencies' install scripts wrote their output
inside `node_modules`, so the clone carries it. After cloning:

- `node_modules/.cache` is deleted (tool caches often embed absolute
  paths).
- If the tree contains a symlink with an absolute target, which could point
  into the source worktree, the clone is dropped and `npm ci` runs instead.
- The project's own `preinstall`, `install`, `postinstall` and `prepare`
  scripts, which may write outside `node_modules` (git hooks setup, code
  generation), are run again if its `package.json` has them.

## Platforms

Cloning needs macOS `clonefile(2)`. On other platforms the clone stages are
skipped and every project gets `npm ci`; `add`, `check` and `remove` work
the same.

## Not yet

Planned for later versions: a pool of pre-made worktrees, entry points for
editor and agent worktree hooks, incremental installs, copying ignored
files listed in `.worktreeinclude`, and ecosystems besides npm.

## License

MIT
