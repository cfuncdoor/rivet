# Commands

Global flags: `-v` (verbose: toolchain candidates, full commands,
decisions), `--message-format=json`, `--color=auto|always|never`
(`NO_COLOR` honored).

| Command | What it does |
|---|---|
| `rivet new <name> [--lib\|--shared]` / `rivet init [--name n] [--lib\|--shared]` | scaffold a binary, static lib (`src/lib.cpp` + `include/<name>.h`), or shared lib |
| `rivet build` | compile + link the scope (incremental, parallel) |
| `rivet run` | build if needed, run the binary (forwards trailing args) |
| `rivet test` | build and run `tests/*.{c,cpp}` |
| `rivet clean [-p pkg]` | remove `target/` output |
| `rivet toolchains` | list detected toolchains, marking the default |
| `rivet update [-p pkg] [--precise v]` | re-resolve (branches move; tags/revs pin), rewrite lockfile |
| `rivet add <spec>` / `rivet remove <dep>` | edit `dependencies`, re-resolve, revert on failure |
| `rivet tree` | print the dependency tree |
| `rivet fmt [--check]` | format sources (clang-format delegate) |
| `rivet compile-commands` | emit `compile_commands.json` for Clangd |
| `rivet completions <shell>` | shell completions |
| `rivet install` | build and copy binaries to `~/.rivet/bin` |
| `rivet publish [--dry-run] [--registry URL]` | upload to a registry |
| `rivet login --registry URL --token T` | store a registry token |
| `rivet search <query>` | search the registry index |
| `rivet vendor [--dry-run]` | copy all sources into `vendor/` for offline builds |
| `rivet git list\|prune\|fetch` | inspect/clean/update the shared git cache |
| `rivet dev <dep>` | writable clone in `./deps-dev/` + automatic `patch` entry |

## Build-like flags (`build`, `run`, `test`, `compile-commands`)

`--release` (release profile), `-p/--package`, `--bin`,
`--features f` (repeatable, comma-separated), `--all-features`,
`--no-default-features`, `--target TRIPLE` (`native` = host default),
`-j/--jobs N` (`0` = cores), `--keep-going`, `--offline`, `--locked`
(error if the lockfile would change), `--frozen` (offline + locked).

## `rivet add` and `rivet remove`

`add` specs (a `@ref` splits at the last `@` only when it follows the
last `/`, so `user@host` URLs survive):

- `name` / `name@req` — registry dep (`"name": "req"`; bare `name`
  resolves `*`, i.e. latest, at update; needs `RIVET_REGISTRY` unless
  the index copy is warm).
- `owner/repo[@ref]` — GitHub shorthand (`repo` key,
  `https://github.com/owner/repo.git`).
- `<url>[@ref]` — any `://` URL, or scp-like `user@host:path`
  (keyed by the last path segment).

Git refs resolve against the shared mirror: exact tag, then
`v`-prefixed tag (`@1.91.4` finds `v1.91.4`), then branch; 40-hex
resolves as a SHA pin; no ref tracks the default branch (`HEAD`).
The materialized checkout is inspected: own `rivet.json` needs
nothing; CMake/Meson/Makefile sources gain an inline `package` block
plus the matching `adapter` (like the zlib example); trees with
compilable top-level sources but no build system get the `sources`
adapter (they compile with the program); anything else warns loudly
(resolves and builds vacuously — vendor the sources with your own
manifest, or set the entry's `adapter`).

`add` writes the manifest entry, then re-resolves just that dep
(`update -p` semantics); on failure both manifest and lockfile are
restored, so a failed `add` never leaves a broken tree. `remove`
deletes the key from every dep table and drops its lock entry without
network — run `rivet update` to prune orphaned transitive entries.
Manifest edits are serde rewrites (formatting normalized, like `dev`).

## `rivet update` and network flags

- `rivet update` re-resolves everything in scope (`-p` narrows,
  `--precise` pins one dep). Git branches advance; tags and revs never
  move (a moved tag is an error, not an update).
- `--locked`: CI mode — fail if resolution differs from the lockfile.
- `--frozen`: `--locked` plus no network (cache/vendor only).
- `--offline`: resolve from cache/vendor only, lockfile may still update
  from cached data.
