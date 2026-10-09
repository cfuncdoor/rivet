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
