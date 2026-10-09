# ARCHITECTURE.md — rivet

Cargo-like build system + package manager for C++. Single static binary `rivet`.
Tool itself is Rust (stable, edition 2021+), built with Cargo.

## Workspace layout (Cargo workspace, small crates)

- `crates/rivet-manifest` — JSONC → typed model, validation with line:column + JSON path.
- `crates/rivet-resolver` — SemVer + PubGrub-style resolution → `rivet.lock.json` (M4+; stub in M1).
- `crates/rivet-git` — git dep subsystem: shell-out to system `git` only (M4+; stub in M1).
- `crates/rivet-toolchain` — detect MSVC (vswhere / vcvars env capture), clang, gcc,
  clang-cl, MinGW; archiver; linker. One flag-normalization abstraction.
- `crates/rivet-graph` — build graph of compile/link/archive steps, depfiles,
  content-hash incremental rebuilds, parallel scheduler (minimal in M1, full in M2).
- `crates/rivet-cache` — global cache in `~/.rivet` (stub in M1).
- `crates/rivet-registry` — index format, fetch, checksum verify (M5+; stub in M1).
- `crates/rivet-cli` — arg parsing (clap), colored Cargo-like output, progress,
  `--message-format=json`, `-v`. All user-facing commands live here.
- Root package `rivet` (`src/main.rs`) is a thin binary wrapper around
  `rivet-cli` so `cargo install --path .` works.

Keep crates independently testable. Typed + documented public APIs.
`thiserror` in libraries, `anyhow` only in `rivet-cli`.

## Build flow (M1)

```text
rivet new hello        → rivet-cli::new (scaffold rivet.json + src/main.cpp)
rivet build            → manifest::load (JSONC) → discover targets (conventions)
                       → toolchain::detect → graph::build (compile+link)
                       → target/<profile>/hello(.exe)
rivet run              → build + exec binary, passthrough stdout/exit code
```

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
