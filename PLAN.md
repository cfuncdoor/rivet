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

## M3 — Profiles, features, tests [IN PROGRESS — owner: M3 agent]
- [x] `rivet test`: discovers `tests/*.cpp` (stem-named binaries), links vs the
      package lib + dep archives, runs each, reports `test <name> ... ok|FAILED`
      + `test result:` summary, nonzero exit on failure; `-p` selects
- [x] Profiles: `opt`/`debug`/`lto`/`sanitizers` honored in flags + fingerprint
      (MSVC `/O2`+`/GL`+`/LTCG`, `/Zi`, `/fsanitize=address`; GNU `-O/-g/-flto`,
      `-fsanitize=`); ASan run-tested where the local toolchain supports it
- [x] Features: `Dependency.optional` + `dep:<name>` activation,
      `--features`/`--all-features`/`--no-default-features`, unknown-feature
      errors, fingerprint inclusion; unification = union of requested features
      per package (see contract below)
- [x] `rivet compile-commands`: emits `target/<prof>/compile_commands.json`
      (`directory` + `arguments` array + `file` per TU, all targets)
- [x] `examples/` corpus gains tests + a features demo; CI builds them
- [x] Unit + integration tests (real compiler, temp dirs); docs updated

## M3/M4 shared contracts (frozen — both agents conform, neither changes them)

- **C1 — Optional deps + features.** `DependencyDetail.optional: bool`
  (default false; foundation already in `rivet-manifest`). A feature enables
  optional deps via `dep:<name>` entries in the `features` table. Enabled
  features per package = union of: `default` (unless `--no-default-features`),
  `--features` requests, all (if `--all-features`). Unknown feature names and
  `dep:` pointing at nonexistent/non-optional deps are errors. Enabled
  features join the build fingerprint. Activation helper lives in
  `rivet-graph` (`activated_features` + active-dep filtering) so the
  lock-based M4 path reuses it.
- **C2 — Lockfile schema** (`rivet.lock.json`, strict JSON, sorted keys,
  stable pretty formatting, `$schema`-style `version: 1`):
  `{"version":1,"packages":[{"name","version","source":{"kind":"path"|"registry"|"git",
  ...},"features":[...],"checksum":"sha256:..."(registry only)}]}`,
  sorted by name. Path source: `{"kind":"path","path":"<as-written>"}`.
  Git source: `{"kind":"git","url","ref":{"tag"|"branch"|"rev","value"},"sha","tree"}`.
  Registry source: `{"kind":"registry","registry":"<url-or-default>","version","checksum"}`.
  Defined + written by `rivet-resolver` (M4 crates agent); read by the CLI
  integration later.
- **C3 — File ownership (no cross-edits).** M3 agent owns: `crates/rivet-cli`,
  `crates/rivet-graph`, `crates/rivet-toolchain` (flags only),
  `examples/`, docs M3 sections, `.github/workflows/ci.yml`. M4 crates agent
  owns ONLY: `crates/rivet-git/**`, `crates/rivet-resolver/**` (code + own
  Cargo.tomls + crate tests). Neither touches `Cargo.lock` deliberately
  (M4 agent's `cargo` runs may extend it with `semver` — expected).
  CLI integration of M4 (`update`, `git …`, `vendor`, `dev`, build-from-lock)
  is done by the coordinator after both land.

## M4 — Resolver + lockfile + git deps [IN PROGRESS — crates: M4 agent; CLI integration: coordinator]
- [ ] `rivet-git` (M4 agent, self-contained): system-git shell-out only;
  bare mirror + immutable checkouts under `~/.rivet/git`; shallow/partial
  with fallback; tag/branch/rev resolve; tree-hash verify; moved-tag error;
  subdir/submodules; hooks disabled; https/ssh/file only;
  `GIT_TERMINAL_PROMPT=0`; list/prune/fetch machinery; temp-bare-repo tests
  (no network — `file://` URLs only)
- [ ] `rivet-resolver` (M4 agent, self-contained): semver requirements,
  resolution over path/registry-stub/git candidates honoring C1 features;
  deterministic lockfile per C2 with checksums/SHAs; `--locked/--frozen/
  --offline` enforcement as library functions; semver-compatible selection;
  yank awareness in the index client stub
- [ ] CLI integration (coordinator, after both agents): build-from-lock,
  `update [-p] [--precise]`, `git list|prune|fetch`, `vendor`, `dev`+`patch`,
  publish-with-patch warning, `--locked/--frozen/--offline` wiring
- [ ] Integration tests with local bare repos: moved tag, force-pushed
  branch, subdir deps, submodules, offline rebuild, prune, patch, `dev`

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
