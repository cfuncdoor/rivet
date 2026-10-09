# PLAN.md — rivet milestone checklist

Current phase: **M3** — profiles, features, tests.
M2 acceptance passed (see below). Do not start M4+ until M3 acceptance passes.

## M1 — Manifest + CLI skeleton + toolchain detection [DONE]
- [x] Workspace scaffold (8 crates) + root `rivet` binary (`cargo install --path .`)
- [x] `rivet-manifest`: JSONC lex-strip (line/col preserving) + typed model + validation
- [x] `rivet.schema.json` shipped + `$schema` reference
- [x] `rivet-toolchain`: detect MSVC (vswhere/vcvars), clang, gcc, clang-cl, MinGW
- [x] `rivet-graph`: minimal compile+link for single-file bin
- [x] `rivet-cli`: `new/init/build/run/toolchains/--version`, Cargo-like output,
      `NO_COLOR`/TTY, `-v`, `--message-format=json`
- [x] `rivet new hello && rivet build && rivet run` on Windows (MSVC verified);
      Linux (gcc/clang) covered by the same detection path + CI matrix
- [x] Unit tests per crate + integration tests (real C++ compiler, temp dirs)
- [x] Gate: `cargo test --workspace`, `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `examples/hello` corpus builds in CI

## M2 — Build graph [DONE]
- [x] Multi-file, incremental, depfile tracking (`-MMD/-MF`, `/showIncludes` + `VSLANG=1033`)
- [x] Content-hash rebuilds (always-hash, SHA-256), parallel scheduler (jobs=cores, `-j`, `--keep-going`)
- [x] lib + bin targets (static + header-only; shared → M7 error), workspaces, path deps (topo order, dedupe, cycle errors)
- [x] Test: touching one header rebuilds only dependents (`header_touch_rebuilds_only_dependents` + mtime-only touch stays `Fresh`)
- [x] Gate green + `examples/{hello,libhello,workspace}` corpus builds in CI

## M3 — Profiles, features, tests [NOT STARTED]
- [ ] `rivet test` discovers+runs `tests/`, reports pass/fail + exit codes
- [ ] Examples, `compile_commands.json`
- [ ] C++ `examples/` corpus builds+tests in CI via rivet

## M4 — Resolver + lockfile + git deps [NOT STARTED]
- [ ] Deterministic sorted lockfile, `--locked/--frozen/--offline`
- [ ] Git subsystem: bare mirror + immutable checkouts, shallow/partial,
      tree-hash verify, moved-tag error, subdir/submodules, prune, patch, `dev`
- [ ] Tests use local bare repos in tempdirs (no network)

## M5 — Registry [NOT STARTED]
- [ ] JSON-lines index, fetch, checksum verify, publish/login/search, yank
- [ ] Local test registry in CI, SemVer-compatible resolution

## M6 — Adapters (CMake/Meson/system libs) [NOT STARTED]
- [ ] Demo: depend on real CMake-only lib (e.g. zlib via git) from a rivet package

## M7 — Build scripts, artifact cache, cross-compile [NOT STARTED]
- [ ] `build.rivet.cpp` directives, artifact cache, `vendor`, `--target`,
      sanitizers, LTO, PCH, (stretch) C++20 modules

## M8 — Polish [NOT STARTED]
- [ ] docs/ book, shell completions, installers + release binaries,
      GH Actions matrix (win/linux × msvc/gcc/clang), 200-file benchmark vs CMake+Ninja
