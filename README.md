# rivet — Cargo for C/C++

rivet is a single-binary build system and package manager that gives C
and C++ the Cargo experience: one declarative manifest, one lockfile,
and one command to build, run, and test.

```sh
rivet new hello && cd hello
rivet build
rivet run
```

## Install

Download a prebuilt static binary (see [releases](../../releases)) or run
one of the install scripts:

```sh
# Linux
curl -fsSL https://raw.githubusercontent.com/cfuncdoor/rivet/main/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/cfuncdoor/rivet/main/install.ps1 | iex
```

Or build from source (needs a Rust toolchain; no other build tools):

```sh
cargo install --path .
```

Requirements at build time: a C/C++ toolchain (MSVC, clang, or gcc — auto
detected) and, only for git dependencies, the system `git` binary. No
CMake, Make, Ninja, Python, or Conan.

## Manifest

Packages declare themselves in `rivet.json` (JSON with comments):

```jsonc
{
  "$schema": "https://raw.githubusercontent.com/cfuncdoor/rivet/main/rivet.schema.json",
  "package": { "name": "myapp", "version": "0.1.0", "standard": "c++20" },
  "dependencies": {
    "fmt": "11",                                    // registry, semver
    "mylib": { "path": "../mylib" },                // local path
    "zlib": { "git": "https://github.com/madler/zlib.git", "tag": "v1.3.1" }
  }
}
```

Conventions need no configuration: `src/main.cpp` (or `src/main.c`) is
the binary, `src/lib.cpp` (or `src/lib.c`) or `include/` the library,
`tests/*.{c,cpp}` tests, `build.rivet.cpp` an optional pre-build script.

## Everyday commands

| Command | Does |
|---|---|
| `rivet new/init` | scaffold a package |
| `rivet build [--release] [-p pkg] [--features f]` | compile + link (incremental, parallel) |
| `rivet run [-- args]` | build and run a binary |
| `rivet test` | build and run `tests/` |
| `rivet update [-p pkg]` | re-resolve git deps, rewrite the lockfile |
| `rivet add <spec>` / `remove <dep>` | add (`name[@v]`, `owner/repo[@ref]`, git URL; verified + reverted on failure) / remove |
| `rivet publish/login/search` | registry workflows |
| `rivet git list/prune/fetch` | inspect the shared git cache |
| `rivet vendor` | copy all sources into `vendor/` for offline builds |
| `rivet dev <dep>` | writable clone of a git dep + auto `patch` |
| `rivet tree` | dependency tree |
| `rivet fmt` / `compile-commands` / `completions` | clang-format delegate, Clangd DB, shell completions |
| `rivet toolchains` | detected C/C++ toolchains |

Output is Cargo-like (`Compiling`, `Fresh`/`Cached`, `Finished`), honors
`NO_COLOR`, and supports `--message-format=json` for IDEs.

## How it works

- **Resolution** produces `rivet.lock.json` (strict JSON, sorted keys):
  registry versions with checksums, git refs pinned to SHAs with tree
  hashes, path deps by version. `--locked` / `--frozen` / `--offline`
  behave like Cargo's.
- **Git deps** live once in `~/.rivet/git` (one bare mirror per URL,
  immutable per-SHA checkouts, tree-hash verified every use).
- **Builds** track headers via compiler depfiles, rebuild on content
  hashes (never mtimes alone), and link in dependency order. Foreign
  sources build through adapters (CMake/Meson/system libs); per-package
  `build.rivet.cpp` scripts can emit includes, defines, and link flags.
- **Cache**: built artifacts are content-keyed in `~/.rivet/artifacts`
  and restored with a `Cached` note instead of recompiling.

## Environment

| Variable | Effect |
|---|---|
| `RIVET_CXX` | Pin the compiler driver (path or `PATH` name: `cl`, `clang-cl`, `clang++`, `g++`, …). Fails actionably instead of falling back. Unset (or empty) means auto-detection. |
| `RIVET_HOME` | Override the cache root (default `~/.rivet`). Useful for tests and CI isolation. |
| `RIVET_NO_TOOLCHAIN_CACHE` | Set (non-empty) to skip the `~/.rivet/toolchain.json` detection cache. |

## Docs

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — workspace layout, build/lock flows
- [`PLAN.md`](PLAN.md) — milestone checklist and frozen contracts
- [`DECISIONS.md`](DECISIONS.md) — every non-trivial choice, with rationale
- [`rivet.schema.json`](rivet.schema.json) — manifest JSON Schema
- [`examples/`](examples/) — real C/C++ projects rivet builds in CI
