# PLAN.md — rivet milestone checklist

Current phase: **M5** — registry.
M4 acceptance passed (see below). Do not start M6+ until M5 acceptance passes.

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

## M3 — Profiles, features, tests [DONE]
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

## M4 — Resolver + lockfile + git deps [DONE]
- [x] `rivet-git`: system-git shell-out only; bare mirror + immutable
  checkouts under `~/.rivet/git`; shallow/partial with fallback;
  tag/branch/rev resolve; tree-hash verify; moved-tag error;
  subdir/submodules; hooks disabled; https/ssh/file only;
  `GIT_TERMINAL_PROMPT=0`; list/prune/fetch machinery; temp-bare-repo tests
  (no network — `file://` URLs only)
- [x] `rivet-resolver`: semver requirements, resolution over
  path/registry-stub/git candidates honoring C1 features; deterministic
  lockfile per C2 with checksums/SHAs; `--locked/--frozen/--offline`
  enforcement as library functions; semver-compatible selection; yank
  awareness in the index client stub
- [x] CLI integration: build-from-lock (opt-in per scope, patch overlay,
  target redirection for immutable checkouts), `update [-p] [--precise]`
  with movement report + moved-tag abort, `git list|prune|fetch`, `vendor`
  (+offline consumption via tree hash), `dev`+`patch`, publish-with-patch
  warning, `--locked/--frozen/--offline` on build/test/run/compile-commands
- [x] Integration tests (`tests/m4.rs`, local bare repos): tag end-to-end,
  branch update, `--precise` pin, moved-tag error, `--locked` error,
  offline rebuild from vendor, subdir deps, submodules, prune, patch, `dev`,
  fetch warm-up

## M5 — Registry [DONE]
- [x] Index client: sparse-HTTPS and local-dir reads (M4-compatible
  JSON-lines), yank filtering, version listing
- [x] Tarball fetch (`file://` + `https`), SHA-256 verify always, safe
  extract (traversal/symlink rejection) into `<cache>/registry/<hash>/`
- [x] `publish` / `login` / `search` (+ `install`): token store, dry-run
  packaging, index search, bin copy to `~/.rivet/bin`
- [x] Registry materialize wired into locked builds (checksum-verified,
  offline + vendor paths); per-dep `registry` URLs locked (no more
  `"default"` hardcode)
- [x] Hermetic tests (`tests/m5.rs`, local `file://` registry): end-to-end,
  checksum mismatch, yank skip, publish dry-run, login/search, offline
  vendor rebuild, install
- [x] New deps justified (`ureq` rustls, `tar` + `flate2`; D41/D45)

## M6 — Adapters (CMake/Meson/system libs) [DONE]
- [x] `crates/rivet-adapters`: CMake adapter (system `cmake` + `cmake
  --build`, include/lib discovery preferring static), Meson/Make
  best-effort, `pkg-config`/`system` probing (no sources required)
- [x] Adapter output model (`include_dirs`, `lib_dirs`, `libs`
  File/Name, `defines`) + source/spec/toolchain fingerprinting
- [x] Schemaless foreign deps via inline `package` block; `system` locks a
  system source and probes at build; `system`+sources rejected at parse
- [x] Hermetic tests + `examples/cmake-mini` (offline) and
  `examples/cmake-zlib` (real zlib via git, network) demos

## M7 — Build scripts, artifact cache, cross-compile [DONE]
- [x] `rivet-buildscript`: `build.rivet.cpp` compiled with the detected
  toolchain, run pre-build (cwd = package, secrets scrubbed),
  `rivet:` directives (include/lib/libdir/define/rerun-if-changed/env),
  rerun gating with persisted outputs (D51)
- [x] Artifact cache in `rivet-cache` (content key, quarantine on corrupt,
  `Cached` restores with state; D52)
- [x] `--target` triple parsing + selection (native detect, cross via
  clang, fingerprint-suffixed; D53); PCH/modules = actionable errors (D54)
- [x] `examples/buildscript-demo` hermetic demo; `tests/m7.rs` (script
  rerun, cache hit after wipe, native/bogus targets)
- [x] LTO/sanitizers already shipped in M3; shared/vendor already in M4

## M5/M6/M7 shared contracts (frozen)

- **C4 — Registry manifest surface.** `DependencyDetail.registry:
  Option<String>` (alternate registry URL; absent = default registry) is the
  only registry surface; no other manifest changes for M5. The M4
  `RegistryIndex` dir format stays valid (M5 extends, never breaks).
- **C5 — Adapter manifest surface.** `DependencyDetail.adapter:
  Option<AdapterSpec{tool, targets, options}>` + existing `system: bool`
  are the only adapter surfaces. `tool ∈ {cmake, meson, make, pkg-config,
  system}` is validated at parse time. Adapters apply to git/path deps
  (foreign sources) or with `system: true` (no sources). No other manifest
  changes for M6.
- **C6 — File ownership (no cross-edits).** M5 owns ONLY
  `crates/rivet-registry/**`. M6 owns ONLY `crates/rivet-adapters/**` +
  `examples/cmake-*` (new files only). M7 owns ONLY
  `crates/rivet-buildscript/**`, `crates/rivet-toolchain/**` (selection +
  flags only), `crates/rivet-cache/**`, `examples/buildscript-*` (new
  files only). NOBODY touches: `crates/rivet-manifest`, `crates/rivet-graph`,
  `crates/rivet-cli`, `crates/rivet-git`, `crates/rivet-resolver`, docs
  (`*.md`), `.github/`, `Cargo.toml` files outside their crates, `Cargo.lock`
  (plain `cargo -p` runs may extend it — expected; never hand-edit, never
  `cargo update`, never commit). The coordinator wires graph+CLI afterwards
  and returns API summaries for DECISIONS.md.

## M8 — Polish [IN PROGRESS]
- [x] shell completions, `tree`/`fmt` commands, installers (`install.sh`,
      `install.ps1`), `README.md`, milestone scrub
- [x] no-op rebuild optimization (toolchain cache + link-digest reuse);
      200-file bench: release Fresh ~0.4s wall (~0.3s in-rivet), single-TU
      recompile+relink ~2.3s; debug Fresh ~3.4s warm wall (was ~15s)
- [x] GH Actions matrix (win/linux × msvc/gcc/clang): `RIVET_CXX` pins
      the driver per job (D60), `-v` log greps the expected driver,
      acceptance steps run under `shell: bash` on both OSes
- [ ] 200-file benchmark vs CMake+Ninja (SKIPPED for now — corpus
      generator kept at `examples/synth-200/gen.sh` for later)
- [ ] docs/ book
