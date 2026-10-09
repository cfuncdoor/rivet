# ARCHITECTURE.md — rivet

Cargo-like build system + package manager for C++. Single static binary `rivet`.
Tool itself is Rust (stable, edition 2021+), built with Cargo.

## Workspace layout (Cargo workspace, small crates)

- `crates/rivet-manifest` — JSONC → typed model, validation with line:column + JSON path.
- `crates/rivet-resolver` — SemVer + PubGrub-style resolution → `rivet.lock.json` (M4+; stub in M1).
- `crates/rivet-git` — git dep subsystem: shell-out to system `git` only (M4+; stub in M1).
- `crates/rivet-toolchain` — detect MSVC (vswhere / vcvars env capture), clang, gcc,
  clang-cl, MinGW; archiver; linker. One flag-normalization abstraction.
- `crates/rivet-graph` — build graph: per-TU compile/link/archive steps,
  depfiles, SHA-256 content-hash incremental rebuilds, parallel scheduler,
  workspaces + path deps with topological order (full since M2), feature
  activation + optional-dep filtering (M3: `features.rs`), test discovery
  + test-target planning + execution (`test.rs`), compilation databases
  (`compile_commands.rs`).
- `crates/rivet-cache` — global cache in `~/.rivet` (root helper done in M1;
  artifact cache in M7).
- `crates/rivet-registry` — index format, fetch, checksum verify (M5+; stub in M1).
- `crates/rivet-cli` — arg parsing (clap), colored Cargo-like output, progress,
  `--message-format=json`, `-v`. All user-facing commands live here.
- Root package `rivet` (`src/main.rs`) is a thin binary wrapper around
  `rivet-cli` so `cargo install --path .` works.

Keep crates independently testable. Typed + documented public APIs.
`thiserror` in libraries, `anyhow` only in `rivet-cli`.

## Build flow (M3)

```text
rivet new hello        → rivet-cli::new (scaffold rivet.json + src/main.cpp)
rivet build [--features f] → scope: nearest manifest, walk up to workspace root
                        → workspace::resolve_units_with_features (members + -p,
                           lenient per-package activation, optional path deps
                           filtered unless enabled; registry/git → milestone
                           errors)
                        → per unit in order: plan_with (bin/lib partition, D17)
                           → is_fresh_unit? Fresh : Compiling + build_unit
                             (parallel compiles w/ depfiles → archive → link,
                              state recorded in incremental/rivet-state.json)
                        → Finished dev in 2.3s
rivet run [-p pkg] [--bin name] [--features f] [-- args] → build scope + exec one binary
rivet test [-p pkg] [--release] [--features f]
                       → per unit: normal build (lib+bins) with Fresh/Compiling
                         lines, then test::build_tests (tests/*.cpp → tag
                         test-<stem> targets appended to the plan, same state
                         file) → run each target/<prof>/tests/<stem> binary
                         sequentially, captured output
                       → test <name> ... ok|FAILED (+ output) per test,
                         `test result: ok. N passed; M failed`, nonzero exit
                         on any failure
rivet compile-commands [-p pkg] → per unit: plan (no build) + test discovery
                         → target/<prof>/compile_commands.json
                           ({directory, arguments[], file} per TU, bins+lib+
                           tests); multi-unit scopes also get a merged copy at
                           the scope root's target/<prof>/
```

Per-package state (`target/debug|release/`): `build/<tag>/*.o[bj]` objects
(`test-<stem>` tags included), `build/<lib>.a|.lib` archives,
`deps/<tag>/*.d` GCC depfiles, `incremental/rivet-state.json` (fingerprint
+ source/dep SHA-256 + link digests), `<bin>[.exe]` outputs,
`tests/<stem>[.exe]` test binaries. Stale = missing object, fingerprint
change, source hash change, or any recorded header hash change
(always-hash, D15).

## Profiles / features (M3)

Manifest `profile.dev|release` carry `opt`/`debug`/`lto`/`sanitizers`
(parsed since M1, honored since M3). The toolchain maps them to flags
(D23); `CxxToolchain::validate_options` rejects sanitizers the toolchain
cannot honor (MSVC: only `address`). The fingerprint covers std, opt,
debug, defines, includes, lto, sanitizers, and the sorted enabled
features — toggling any of them rebuilds.

Features (contract C1): `features` table + `optional` path deps.
`activated_features` (strict, shared with the M4 resolver) computes the
union of `default` (unless `--no-default-features`), `--features`
requests, and all (under `--all-features`); `activated_features_lenient`
+ a scope-wide known-feature check give CLI union semantics (D24).
Resolution filters optional path deps no enabled feature activates;
enabled features join the fingerprint via `CompileOptions.features`.

Conventions (zero config): `src/main.cpp` → bin, `src/lib.cpp` or `include/` → lib,
`tests/*.cpp` → tests, `examples/`, `benches/`. Public `include/`, private `src/`.
Output `target/debug|release/{deps,build,incremental}` (`dev` profile → `debug/`,
Cargo convention; see DECISIONS.md D12). Cache `~/.rivet/`.

## Manifest / lockfile

- Manifest `rivet.json` is JSONC (`//`, `/* */`, trailing commas). Strip via small
  lex pass before `serde_json`, preserving line/column (stripped chars → spaces,
  newlines preserved). Errors report `line:column` + JSON path.
- Lock `rivet.lock.json`: strict JSON, sorted keys, stable formatting (M4+).
- Ship + reference `rivet.schema.json` via `$schema`.

## Toolchain abstraction

`CxxToolchain { kind, cxx: PathBuf, cc, archiver, version, msvc_env }`.
`detect()` order: `cl` in PATH → vswhere → well-known VS install paths →
`clang++`/`clang-cl` → `g++`. MSVC builds capture `vcvars64` env
(`INCLUDE`/`LIB`/`PATH`) and apply it to every `cl`/`link` invocation so
`cl` works without a pre-configured developer prompt. Flags normalized behind
`CompileOptions { std, opt, debug, defines, includes, pic, warnings }`.
ABI/compiler/std/stdlib are part of the build fingerprint.

## Errors / output

Actionable errors (what/where/fix). Never silently fall back; log decision at `-v`.
Compiler errors pass through unmodified. CLI mimics Cargo
(`Compiling x v1.0`, `Finished dev in 2.3s`); respect `NO_COLOR`, detect TTY;
support `--message-format=json`, `-v`.

## Constraints (from AGENTS.md)

- Deps: small + pinned, `Cargo.lock` committed, justify every non-trivial crate
  in `DECISIONS.md`.
- `Result` everywhere; no `unwrap`/`expect` outside tests.
- `Path`/`PathBuf` + `OsString` for fs/process args. No global mutable state.
- No `unsafe` except small documented platform module with `// SAFETY:` per block.
- Runtime deps: only system `git` binary. No CMake/Make/Ninja/Python/Conan.
- Security: verify checksums; no code exec at resolve time; reject path traversal.
