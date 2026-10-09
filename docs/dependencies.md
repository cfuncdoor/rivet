# Dependencies

Three sources — registry, path, git — resolved to the strict-JSON
`rivet.lock.json` (registry checksums, git URL + ref + full SHA + tree
hash, path versions). `rivet tree` shows the result; `rivet update`
re-resolves it.

## Registry

`"fmt": "11"` resolves a semver requirement against the registry
index (sparse or `file://`), downloads the tarball, verifies its
checksum, and extracts it with path-traversal/symlink rejection.
`registry:` per-dep overrides the default registry (C4).
`rivet publish` / `login` / `search` manage the registry workflow;
publishing with active `patch` entries warns.

## Path and workspaces

`{ "path": "../utils" }` builds the local package in topological
order with the rest of the scope. `workspace.members` groups them;
`-p` selects. No version skew is possible — the sources *are* the dep.

## Git

`{ "git": "<https|ssh|file>://…", "tag"|"branch"|"rev": … }`
(`tag`/`branch`/`rev` are mutually exclusive).rivet shells out to the
system `git` (SSH, credential helpers, and `~/.gitconfig` keep
working); `GIT_TERMINAL_PROMPT=0` unless `--interactive`, so builds
never hang on a prompt. Missing `git` is a hard error with install
instructions.

Layout under `~/.rivet/git/`: one bare mirror per URL in `db/`, and
immutable per-SHA checkouts in `checkouts/` (verified clean before
reuse, healed by rebuild, repaired once via mirror re-clone — a
partial-mirror design was abandoned for silent corruption, D56).
Builds use the locked SHA; only `rivet update` advances branches.
`rivet git list` shows pins, `prune` reclaims, `fetch` refreshes
mirrors. `vendor` copies everything into `vendor/` for fully offline
builds; `dev <name>` materializes a writable clone plus `patch`.

Transfers prefer shallow/partial fetches with full-clone fallback,
disable checkout hooks, and verify the tree hash after every
materialization.
