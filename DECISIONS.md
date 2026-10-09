# DECISIONS.md — rivet decision log

Record every non-trivial/ambiguous choice here. Rule: when ambiguous, choose the
simpler option that preserves Cargo's mental model, then continue.

## M1

- **D1: JSONC via lex-strip + serde_json (no jsonc-parser crate).**
  A ~100-line comment/trailing-comma stripper that replaces stripped chars with
  spaces (newlines preserved) keeps line/column mapping exact with zero new
  dependencies. A dedicated crate would add audit surface for no benefit.
- **D2: clap 4 with `derive` for CLI parsing.**
  Non-trivial dep, justified: CLI has 15+ subcommands with flags; hand-rolling
  with lexopt saves ~500KB binary but costs maintainability and completions
  (M8 needs `clap_complete`). clap is the Cargo-standard choice.
- **D3: serde + serde_json for manifest model.**
  Justified: typed validated model with precise errors; the de-facto standard,
  small, no unsafe concerns beyond its audit history.
- **D4: thiserror in libraries, anyhow only in rivet-cli.**
  Follows AGENTS.md; library errors are typed/actionable, CLI wraps with context.
- **D5: which for compiler lookup.**
  Justified: correct PATH lookup on Windows (PATHEXT, `cl.exe`) vs hand-rolled
  search. Small, single-purpose.
- **D6: walkdir for source discovery.**
  Justified: correct recursive `src/`/`tests/` discovery with symlink-loop
  protection; std::fs::read_dir recursion is error-prone on Windows junctions.
- **D7: tempfile (dev-dependency) for tests.**
  Justified: hermetic integration tests that run real C++ compilers in temp dirs.
- **D8: Root package + workspace.**
  Root `Cargo.toml` is both `[package] rivet` (thin `src/main.rs` wrapper) and
  `[workspace]`. This makes `cargo install --path .` work while keeping the
  8-crate layout. Alternative (virtual workspace) would break that acceptance
  requirement.
- **D9: MSVC detection order: PATH → vswhere → well-known VS paths → clang → gcc.**
  Never silently fall back: at `-v` log each candidate tried; on failure emit
  actionable message (install VS Build Tools / LLVM / gcc). `cl` invocations
  always carry the captured `vcvars` env so builds work outside dev prompts.
- **D10: M1 build is single-translation-unit compile+link, no incremental cache.**
  Full graph/depfiles/content-hashing is M2. M1 `rivet-graph::build` compiles
  `src/main.cpp` → `target/<profile>/{deps,build}/obj` → links exe. Layout
  already matches the M2 target tree so outputs are forward-compatible.
- **D11: Stub crates for M4–M7 (resolver/git/registry/cache) ship typed “not yet
  implemented” errors in M1.**
  Keeps public API surface final and independently testable without pulling
  network/tar/fetch deps early. Each stub documents its milestone.
- **D12: `dev` profile builds into `target/debug/` (not `target/dev/`).**
  AGENTS.md says `target/<profile>/…`; Cargo's mental model maps profile
  `dev` → directory `debug`. Cargo compatibility wins over the literal
  reading; `Finished dev profile in …` still prints the profile name.

## M2

- **D13: `sha2` for content hashing.**
  Justified: the spec mandates content-hash rebuilds; SHA-256 is the standard
  choice (also used for future checksum verification in M5). Hex via
  `format!("{:x}")`, no `hex` crate needed.
- **D14: Hand-rolled parallel scheduler (work queue + scoped threads), no rayon.**
  Compile tasks are independent within a package (link/archive run after, in
  topo order), so a ~70-line FIFO queue with a poison flag for fail-fast is
  sufficient. rayon would pull in 6+ transitive deps for no structural win.
  Jobs default to `std::thread::available_parallelism()`, overridable with
  `-j/--jobs`; `--keep-going` disables fail-fast. Static work-stealing is
  deferred to M8 if the 200-file benchmark needs it.
- **D15: Always-hash (no mtime fast path) for staleness.**
  Every incremental check hashes source + recorded header contents. This is
  slower than a stat fast-path but trivially correct, and fast enough until
  the M8 benchmark. A `size+mtime` shortcut is explicitly deferred, not
  forgotten.
- **D16: Depfiles via `-MMD -MF` (GCC/Clang) and `/showIncludes` + `VSLANG=1033` (MSVC-like).**
  `-MMD` excludes system headers (toolchain upgrades are covered by the
  fingerprint instead). MSVC `/showIncludes` notes are locale-dependent, so
  every MSVC-spawned command sets `VSLANG=1033` to force the English
  `Note: including file:` prefix. clang-cl follows the MSVC path.
- **D17: Bin/lib source partition rule.**
  Helpers are `src/**/*.cpp` minus all target roots. A lib (explicit, or
  `src/lib.cpp`) owns the helpers; bins then compile only their entry file
  and link the archive. Without a lib, each bin compiles entry + all helpers
  (multi-file bin). `include/` headers imply a header-only lib only when the
  package has no bins at all — otherwise a bin package with headers would
  lose its helpers to a phantom lib. Leftover `src/` sources without bins
  form a static lib. Explicit directory `bin` entries keep M1 semantics
  (whole directory, no merging). Bin root == lib root is an error, not a
  silent pick.
- **D18: `static` + `header-only` libs in M2; `shared` is an actionable error (M7).**
  Shared needs an export/SONAME story that belongs with cross-compile and
  adapters. Static archives (`.lib`/`.a`) plus header-only include
  propagation cover the M2 acceptance (`lib + bin targets`).
- **D19: Only `include/` propagates across path deps (public vs private).**
  `src/` headers are private to their package by convention. A path dep
  contributes its archive (if any) + its `include/` dir, transitively.
  Static link order is reversed-topo (dependents first), correct for GNU `ld`
  and harmless for MSVC.
- **D20: Virtual workspace manifests (`workspace` without `package`).**
  `package` becomes optional; a virtual root must not define `bin`/`lib`.
  Inside a workspace, bare `build` covers the whole workspace (Cargo
  behavior); `-p` narrows, `--bin` selects. Each package keeps its own
  `target/` dir (a shared target dir waits for the M7 artifact cache).
- **D21: Member globs without a `glob` crate.**
  Only `*` (one segment), `**` (any depth), and `?` are supported — enough
  for `crates/*` and friends, ~40 lines, zero new deps. Unmatched globs and
  members without `rivet.json` are errors (likely typos), not warnings.
- **D22: M1 `build_exe_command` removed in favor of split compile/link/archive.**
  Pre-1.0 API cleanup with no external users; the M1 integration tests drive
  the CLI binary and are unaffected.
