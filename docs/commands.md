# Commands

Global flags: `-v` (verbose: toolchain candidates, full commands,
decisions), `--message-format=json`, `--color=auto|always|never`
(`NO_COLOR` honored).

| Command | What it does |
|---|---|
| `rivet new <name>` / `rivet init [--name n]` | scaffold a package (`rivet.json` + `src/main.cpp`) |
| `rivet build` | compile + link the scope (incremental, parallel) |
| `rivet run` | build if needed, run the binary (forwards trailing args) |
| `rivet test` | build and run `tests/*.cpp` |
| `rivet clean [-p pkg]` | remove `target/` output |
| `rivet toolchains` | list detected toolchains, marking the default |
| `rivet update [-p pkg] [--precise v]` | re-resolve (branches move; tags/revs pin), rewrite lockfile |
| `rivet add <dep>` / `rivet remove <dep>` | edit `dependencies` (+ refetch) |
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

## `rivet update` and network flags

- `rivet update` re-resolves everything in scope (`-p` narrows,
  `--precise` pins one dep). Git branches advance; tags and revs never
  move (a moved tag is an error, not an update).
- `--locked`: CI mode — fail if resolution differs from the lockfile.
- `--frozen`: `--locked` plus no network (cache/vendor only).
- `--offline`: resolve from cache/vendor only, lockfile may still update
  from cached data.
