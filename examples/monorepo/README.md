# monorepo

A small npm monorepo for trying fast-worktree: two projects with one tiny
dependency each. `web` has a `postinstall` that writes `generated.txt`
outside `node_modules`, the kind of script fast-worktree runs again after a
clone. `.fast-worktree.toml` has one hook per stage.

fast-worktree works on a repository, so copy this directory out, commit it,
and clone it (the clone gives it an `origin` to start branches from):

```sh
cp -R examples/monorepo /tmp/fw-upstream
git -C /tmp/fw-upstream init -q -b main
git -C /tmp/fw-upstream add -A
git -C /tmp/fw-upstream commit -q -m init
git clone -q /tmp/fw-upstream /tmp/fw-monorepo
cd /tmp/fw-monorepo

fast-worktree add ../fw-a -b feature/a     # npm ci for both projects
fast-worktree add ../fw-b -b feature/b     # cloned from the cache (macOS)
cat ../fw-b/web/generated.txt
fast-worktree remove ../fw-b --delete-branch
fast-worktree remove ../fw-a --delete-branch
```

The first `npm ci` downloads two packages from the npm registry.
