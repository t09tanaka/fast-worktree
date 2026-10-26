# monorepo

A small monorepo for trying fast-worktree: two npm projects with one tiny
dependency each, and a Dart package without dependencies.

- `web` has a `postinstall` that writes `generated.txt` outside
  `node_modules`, the kind of script fast-worktree runs again after a
  clone.
- `api` has a `postinstall` that generates code into `node_modules` from
  `schema.json` and keeps a copy of the schema there. `.fast-worktree.toml`
  skips it after a clone while the schema is unchanged.
- `mobile` is a Dart package: `flutter pub get` (or `dart pub get`) works
  offline because it has no dependencies.
- `.worktreeinclude` lists `.env`, which `.gitignore` ignores.
- `.fast-worktree.toml` also has one hook per stage. It has no
  `[[clone]]`, so the projects are found from their lockfiles.
- `mobile/ios` has a `Podfile.lock` and `fake-pod.sh`, an offline
  stand-in for `pod install`, for trying a `[[clone]]` without a preset:
  `fast-worktree config init` writes the presets for the three projects,
  and `cat pods.toml >> .fast-worktree.toml` adds `mobile/ios/Pods`.

fast-worktree works on a repository, so copy this directory out, commit it,
and clone it (the clone gives it an `origin` to start branches from):

```sh
cp -R examples/monorepo /tmp/fw-upstream
git -C /tmp/fw-upstream init -q -b main
git -C /tmp/fw-upstream add -A
git -C /tmp/fw-upstream commit -q -m init
git clone -q /tmp/fw-upstream /tmp/fw-monorepo
cd /tmp/fw-monorepo
echo "API_TOKEN=dev-only" > .env

fast-worktree add ../fw-a -b feature/a     # npm ci for both projects, pub get
fast-worktree add ../fw-b -b feature/b     # cloned from the cache (macOS)
cat ../fw-b/.env ../fw-b/web/generated.txt

# A new lockfile: the old cache entry is cloned and updated (macOS).
(cd web && npm install --package-lock-only --no-audit --no-fund ms@2.1.2)
git commit -qam "web: ms 2.1.2"
fast-worktree add ../fw-c -b feature/c HEAD

fast-worktree cache ls

# List the targets, and add mobile/ios/Pods (see the README).
fast-worktree config init
cat pods.toml >> .fast-worktree.toml
fast-worktree config check
git commit -qam "[[clone]] entries"
fast-worktree plan
fast-worktree add ../fw-d -b feature/d HEAD     # pod install (the stand-in)
fast-worktree add ../fw-e -b feature/e HEAD     # Pods cloned from the cache (macOS)

fast-worktree remove ../fw-e --delete-branch
fast-worktree remove ../fw-d --delete-branch
fast-worktree remove ../fw-c --delete-branch
fast-worktree remove ../fw-b --delete-branch
fast-worktree remove ../fw-a --delete-branch
```

The first `npm ci` downloads two packages from the npm registry, and the
`npm install` one more. Without `flutter` or `dart`, `mobile` is skipped
with a note.
