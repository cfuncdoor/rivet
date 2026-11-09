
<div align="center">

# rivet

**Cargo for C and C++.**<br>
One binary, one manifest, one lockfile.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE.md)
![Rust 1.74+](https://img.shields.io/badge/rust-1.74%2B-orange.svg)
![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20windows-lightgrey.svg)

</div>

rivet is a single-binary build system and package manager for C and C++. You describe a package in `rivet.json`, and one command builds, runs, or tests it. No CMake, Make, Ninja, Python, or Conan.

```console
$ rivet new hello && cd hello
$ rivet add fmt
$ rivet run
   Compiling fmt v11.0.2
   Compiling hello v0.1.0
    Finished dev in 1.2s
     Running target/dev/hello
Hello, world!
```

## Contents

- [Install](#install)
- [Project layout](#project-layout)
- [Manifest](#manifest)
- [Commands](#commands)
- [How it works](#how-it-works)
- [Environment](#environment)
- [Repository layout](#repository-layout)
- [Docs](#docs)

## Install

Grab a prebuilt static binary from the [releases](../../releases), or use an install script.

```sh
# Linux (x86_64, aarch64)
curl -fsSL https://raw.githubusercontent.com/cfuncdoor/rivet/main/install.sh | sh
```

```powershell
# Windows (x86_64)
irm https://raw.githubusercontent.com/cfuncdoor/rivet/main/install.ps1 | iex
```

Both scripts install to `~/.rivet/bin` and tell you if it isn't on your `PATH`.

| Variable | Default | Meaning |
|---|---|---|
| `RIVET_VERSION` | `latest` | Release tag to install |
| `DEST` | `~/.rivet/bin` | Install directory |
| `RIVET_REPO` | `cfuncdoor/rivet` | Install from a fork |

To build from source you only need Rust 1.74 or newer:

```sh
cargo install --path .
```

To compile C/C++ you need a toolchain (MSVC, clang, or gcc, auto-detected). The system `git` binary is only needed for git dependencies.

## Project layout

Nothing to configure if you follow the conventions.

```
myapp/
├── rivet.json          manifest
├── rivet.lock.json     lockfile (generated)
├── build.rivet.cpp     optional pre-build script
├── include/            library headers
├── src/
│   ├── main.cpp        binary   (or main.c)
│   └── lib.cpp         library  (or lib.c)
└── tests/
    └── *.cpp           tests    (or *.c)
```

## Manifest

`rivet.json` is JSON with comments. Add the `$schema` line and your editor gets completion and validation.

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

**Language standards:** `c++11` through `c++26`, and `c11`, `c17`, `c23`.

**Top-level keys:** `package`, `dependencies`, `devDependencies`, `buildDependencies`, `features`, `target`, `bin`, `lib`, `profile`, `workspace`, `patch`.

### Dependency sources

| Source | Example |
|---|---|
| Registry | `"fmt": "11"` or `{ "version": "11", "registry": "..." }` |
| Path | `{ "path": "../mylib" }` |
| Git | `{ "git": "<url>", "tag" \| "branch" \| "rev": "...", "subdir": "...", "submodules": true }` |
| System library | `{ "system": true }` |
| Foreign build | `{ "git": "<url>", "adapter": { "tool": "cmake", "targets": ["foo"], "options": { "FOO_SHARED": "OFF" } } }` |

Adapters support `cmake`, `meson`, `make`, `pkg-config`, and `system`. Any dependency can be marked `"optional": true` and enabled through `features`.

### Libraries, binaries, profiles

```jsonc
{
  "lib": { "type": "static" },                  // static | shared | header-only
  "bin": [{ "name": "tool", "path": "src/tool.cpp" }],
  "profile": {
    "dev":     { "opt": 0, "debug": true, "sanitizers": ["address"] },
    "release": { "opt": 3, "lto": true }
  },
  "workspace": { "members": ["crates/*"] }
}
```

## Commands

| Command | What it does |
|---|---|
| `rivet new` / `init` | Scaffold a package |
| `rivet build [--release] [-p pkg] [--features f]` | Compile and link, incrementally and in parallel |
| `rivet run [-- args]` | Build and run a binary |
| `rivet test` | Build and run `tests/` |
| `rivet add <spec>` | Add a dependency: `name[@v]`, `owner/repo[@ref]`, or a git URL. The build is checked and the edit reverted on failure |
| `rivet remove <dep>` | Remove a dependency |
| `rivet update [-p pkg]` | Re-resolve git deps and rewrite the lockfile |
| `rivet tree` | Print the dependency tree |
| `rivet vendor` | Copy all sources into `vendor/` for offline builds |
| `rivet dev <dep>` | Make a writable clone of a git dep and add a `patch` for it |
| `rivet publish` / `login` / `search` | Registry workflows |
| `rivet git list` / `prune` / `fetch` | Inspect the shared git cache |
| `rivet fmt` | Run clang-format |
| `rivet compile-commands` | Write `compile_commands.json` for clangd |
| `rivet completions` | Generate shell completions |
| `rivet toolchains` | Show detected C/C++ toolchains |

Output looks like Cargo's (`Compiling`, `Fresh`/`Cached`, `Finished`), respects `NO_COLOR`, and `--message-format=json` gives machine-readable output for IDEs. `--locked`, `--frozen`, and `--offline` work as they do in Cargo.

## How it works

**Resolution.** Produces `rivet.lock.json` (strict JSON, sorted keys). Registry versions are pinned with checksums, git refs with a commit SHA and tree hash, and path deps by version.

**Git deps.** Stored once in `~/.rivet/git`: one bare mirror per URL, immutable per-SHA checkouts, tree hash verified on every use.

**Builds.** Headers are tracked through compiler depfiles. Rebuilds are decided by content hash, never by mtime alone. Targets link in dependency order. Foreign sources go through the CMake, Meson, and system-lib adapters. A package's `build.rivet.cpp` can emit include paths, defines, and link flags.

**Cache.** Built artifacts are keyed by content in `~/.rivet/artifacts` and restored as `Cached` instead of being recompiled.

## Environment

| Variable | Effect |
|---|---|
| `RIVET_CXX` | Pin the compiler driver, by path or `PATH` name (`cl`, `clang-cl`, `clang++`, `g++`, ...). Errors out instead of falling back. Unset or empty means auto-detect. |
| `RIVET_HOME` | Cache root. Default `~/.rivet`. Handy for tests and CI isolation. |
| `RIVET_NO_TOOLCHAIN_CACHE` | Set to any non-empty value to skip the `~/.rivet/toolchain.json` detection cache. |

## Repository layout

rivet is a Cargo workspace. Each concern is its own crate.

| Crate | Responsibility |
|---|---|
| `rivet-cli` | Command line, output formatting |
| `rivet-manifest` | `rivet.json` parsing and validation |
| `rivet-resolver` | Dependency resolution and lockfile |
| `rivet-git` | Git mirrors, checkouts, tree hashing |
| `rivet-toolchain` | Compiler detection and invocation |
| `rivet-graph` | Build graph, depfiles, incremental rebuilds |
| `rivet-cache` | Content-keyed artifact cache |
| `rivet-registry` | Registry client (`publish`, `login`, `search`) |
| `rivet-adapters` | CMake, Meson, make, pkg-config, system libs |
| `rivet-buildscript` | `build.rivet.cpp` pre-build scripts |

The release profile is size-optimized with LTO and stripped, which keeps the static binary small.

## Docs

- [`ARCHITECTURE.md`](ARCHITECTURE.md): workspace layout, build and lock flows
- [`PLAN.md`](PLAN.md): milestone checklist and frozen contracts
- [`DECISIONS.md`](DECISIONS.md): every non-trivial choice, with the reasoning
- [`rivet.schema.json`](rivet.schema.json): manifest JSON Schema
- [`examples/`](examples/): real C/C++ projects that rivet builds in CI

## License

[MIT](LICENSE.md)
