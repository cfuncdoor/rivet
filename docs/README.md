# rivet documentation

Single-binary, Cargo-like build system and package manager for C/C++:
one declarative manifest, one lockfile, one command to build, run,
and test. No CMake/Make/Ninja/Python/Conan needed to *use* rivet
(CMake is only ever invoked as an *adapter* for foreign packages).

- [Manifest reference](manifest.md) — `rivet.json` fields, JSONC rules, schema
- [Commands](commands.md) — every subcommand and its flags
- [Dependencies](dependencies.md) — registry, path, and git deps; lockfile;
  offline/vendor/dev workflows
- [Builds](build.md) — profiles, features, build scripts, adapters,
  caching, targets, environment variables, troubleshooting

## Quickstart

Requirements: a C/C++ toolchain (MSVC, clang, gcc, or MinGW), system
`git` (only for git deps), and the `rivet` binary itself
([install](../README.md#install) or `cargo build --release`).

```sh
rivet new hello && cd hello
rivet build     # Compiling hello v0.1.0 ... Finished dev profile in 0.4s
rivet run       # builds if needed, then runs ./target/debug/hello
rivet test      # builds and runs tests/*.{c,cpp}
```

Conventions (zero config): `src/main.cpp` (or `src/main.c`) → binary,
`src/lib.cpp` (or `src/lib.c`) or `include/` → library,
`tests/*.{c,cpp}` → tests, `examples/` and `benches/` likewise. Public headers go in `include/`, private sources
in `src/`. Build output lands in `target/debug|release/`
(`dev` profile → `debug/`, Cargo convention); shared caches live in
`~/.rivet/`.

Output is Cargo-like (`Compiling`, `Fresh`/`Cached`, `Finished`),
honors `NO_COLOR`, and supports `--message-format=json` (one JSON
object per line) for IDEs. Compiler diagnostics pass through
unmodified; rivet's own errors are actionable (what/where/fix) and
never silently fall back — a decision is logged at `-v`.
