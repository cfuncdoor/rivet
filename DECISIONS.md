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

## M3

- **D23: Profile flags — MSVC `/GL`+`/LTCG`, `/fsanitize=address`;
  GNU/Clang `-flto`, `-fsanitize=<csv>; all in the fingerprint.**
  `CompileOptions` gains `lto: bool`, `sanitizers: Vec<String>`,
  `features: Vec<String>` (sorted, fingerprint-only). Compile side rides in
  `push_common_args`; link side in `link_exe_command` (MSVC `/LTCG` goes
  after `/link`, `/fsanitize=` stays a cl-level flag). `clang-cl` follows
  the MSVC mapping (it is `is_msvc_like`). MSVC supports only `address` —
  anything else fails in `validate_options` with an actionable error;
  GNU/Clang names pass through (the compiler reports unknowns). Command
  builders stay infallible, so `build_unit`/`is_fresh_unit` (and
  `compile_commands_for`) call `validate_options` first — misconfiguration
  fails fast even on a fresh tree.
  (PLAN.md M3 wrote `/fsanitize:address` with a colon; the real MSVC flag
  uses `=`, which is what we emit.)
- **D24: Features — union semantics with lenient per-package activation.**
  `activated_features` (strict) errors on unknown requests and on `dep:`
  entries naming missing/non-optional deps — but only entries of *enabled*
  features are validated, so a disabled feature never breaks the build.
  Command-line requests span packages with different tables, so
  `workspace::resolve_units_with_features` uses `activated_features_lenient`
  (requests filtered to each package's table = union of requested features
  per package) plus a scope-wide check: a request unknown to *every*
  reached package is still a typo error (Cargo's "none of the selected
  packages contains these features" spirit). Same-package feature refs
  enable transitively; `other/feat` cross-package entries are ignored here
  and left for the M4 resolver. Optional *path* deps outside the active set
  are filtered out of the build; optional git/registry deps still hit the
  M4/M5 milestone errors (M4 wires those flags). `--features` is
  comma-splitting + repeatable (clap `value_delimiter`). Enabled features
  join the fingerprint, so toggling a feature rebuilds dependents.
- **D25: `rivet test` — synthesized `test-<stem>` targets in the shared
  state, sequential runs.**
  Discovery (`rivet-graph::test`) scans top-level `tests/*.cpp` only
  (`is_cpp_source`, sorted by stem; duplicate stems are an error; missing
  `tests/` = no tests). Each file becomes a `TargetBuild` (tag
  `test-<stem>`, objects `build/test-<stem>`, output
  `target/<prof>/tests/<stem>[.exe]`) appended to the normal plan and
  driven through `build_unit` — one state file, lib archived before test
  links, normal bins link first. `devDependencies` stay unwired (M4).
  The CLI builds lib+bins, then tests, then runs each binary sequentially
  (cwd = package dir, output captured): `test <stem> ... ok|FAILED`
  (+ output on failure), `test result: ok. N passed; M failed`, nonzero
  exit on any failure. Supports `-p/--release/-j/--keep-going` + feature
  flags; all tests run even after a failure (failures aggregate).
- **D26: `compile_commands.json` — `compile_only_args` per TU, per-unit
  files + workspace merge.**
  Entries `{directory, arguments[], file}` with `arguments` = driver + full
  compile flags + source (dep-tracking flags excluded — IDEs don't need
  them; paths via `to_string_lossy`). Covers bins + lib + tests. One file
  per unit at `target/<prof>/compile_commands.json`; multi-unit scopes
  additionally get a merged, file-sorted copy at the scope root's
  `target/<prof>/`. Needs a toolchain for flag mapping but compiles
  nothing.
- **D27: MSVC ASan builds and links, but running needs the runtime DLL on
  PATH — so no sanitizer example.**
  Verified empirically: `sanitizers:["address"]` compiles (`/fsanitize=address`)
  and links on MSVC 14.51, but the exe exits `0xC0000135`
  (`STATUS_DLL_NOT_FOUND`) unless the `clang_rt.asan_dynamic-x86_64.dll`
  directory (next to `cl.exe`) is on `PATH`; with it on `PATH` the binary
  runs clean (exit 0). That is stock MSVC ASan behavior (environment setup
  like `vcvars`, not something `rivet run` should paper over in M3), so per
  the M3 brief there is no `examples/*san*`: sanitizer coverage is
  flag-string unit tests + an MSVC rejection test + a Linux-gated
  build-and-run integration test (GCC ASan needs no PATH games).
- **D28: Examples + CI.**
  `examples/libhello` gains `tests/{greeting,answer}.cpp` (plain
  return-code tests, no framework); new `examples/features/{app,extra}`
  demo (optional path dep, `default: []`, `extra: ["dep:extra"]`) where
  `extra/target/` existing after a build proves activation. CI builds the
  libhello suite (`build`+`test`+`run`) and exercises the features matrix
  (`build`, `--features extra`, `--all-features`, `--no-default-features`,
  `run`, `compile-commands`).

## M4

- **D29: Same URL at different refs must unify to one SHA (unify-or-error).**
  If two packages request the same git URL with different refs, both refs are
  resolved and the SHAs compared: identical SHAs unify silently (one source),
  divergent SHAs are `SameUrlDifferentRefs` spelling both refs and SHAs with
  a "unify on one ref" hint. The `db/<name>-<urlhash>/` + per-SHA checkout
  layout *could* host divergent SHAs, but building one repo twice at two
  commits is a typo magnet, so rivet refuses it (Cargo-like strictness).
- **D30: Shallow/partial fetch first, two-level fallback.**
  Every fetch tries `--filter=blob:none` for the exact ref, then the same
  refetch without the filter, then a full `fetch origin`. Rationale: partial
  clones halve mirror traffic on modern servers, while `file://` test mirrors
  and old gits that reject `--filter` fall through transparently. Resolution
  always ends at `rev-parse`/`cat-file` in the local mirror, so fallback is
  invisible to callers.
- **D31: Cache-dir URL hash is FNV-1a-64 hex, not SHA-256.**
  `rivet-git` stays dependency-free (no `sha2` for a directory suffix). The
  full URL is recorded in `db/<dir>/rivet-url.txt`, so a hypothetical
  collision surfaces as a URL mismatch, never silent confusion.
- **D32: Registry index stub = JSON-lines `<index>/<name>`.**
  Lines `{"version","checksum","yanked","deps":[{"name","req","optional"}]}`,
  checksums stored verbatim into the lockfile, unknown fields ignored so M5
  can extend the format. Yanked versions are skipped; a lock pinning a
  yanked version is an explicit `YankedPinned` error ("run `rivet update`"),
  never a silent keep-or-skip. Optional *index* deps are skipped in M4 (real
  registry feature tables arrive with M5).
- **D33: Lockfile bytes = C2 example field order, packages by name.**
  `version` first, then `packages` (C2's literal order), each package in
  `{name,version,source,features,checksum?}` order, features sorted; pretty
  JSON + trailing newline. "Sorted keys" is realized as sorted packages /
  sorted features + fixed field order (deterministic: resolve-twice yields
  identical bytes). `GitLockRef` writes canonical `{"kind","value"}`.
  `read` also accepts the single-key form (`{"tag":"v1"}`).
- **D34: Feature semantics for M4 (C1 edge cases).**
  Root: `default` (unless `--no-default-features`) + `--features`, or all
  with `--all-features`; transitive deps: `default` (or all). `--features`
  does not propagate to transitive deps (no `pkg/feat` syntax in M4 —
  deferred to M5). The *whole* feature table of every resolved manifest is
  validated eagerly (bad `dep:` / dangling chained refs error even when the
  feature is disabled); `dep:` must name an existing `optional = true`
  entry in `dependencies` (dev/build deps don't count in M4). Registry
  packages record `features: []` in the stub. Lockfile `name` = dep key and
  must equal the child manifest's `package.name` (`NameMismatch` otherwise).
  Bare `git` URL without `tag`/`branch`/`rev` = remote default (`HEAD`).
  Registry is always `"default"` (manifests cannot name a registry yet).
  Frozen = offline resolve + `check_locked` (no separate flag).
- **D35: New deps justified.**
  `semver 1.0` (caret reqs + max-compatible selection — hand-rolling SemVer
  is a bug farm), `serde`/`serde_json` in `rivet-resolver` (already pinned
  via `rivet-manifest`; lockfile I/O needs them). `tempfile 3.0` dev-only
  for hermetic tests. No git2/gitoxide (AGENTS.md forbids; system `git`
  only). `protocol.file.allow=always` is set on every invocation so
  `file://` submodules work in hermetic tests; network submodules are still
  gated by the URL allowlist before init.
- **D36: Lockfile flow is opt-in per scope (path-only scopes never lock).**
  `build`/`test`/`run`/`compile-commands` first try plain resolution; only
  `GitDep`/`RegistryDep` failures enter the lock flow (reuse → re-resolve →
  rewrite unless `--locked`). Path-only projects therefore never see a
  `rivet.lock.json`, preserving M1–M3 behavior byte-for-byte, and stay fully
  offline without flags.
- **D37: Locked-materialized units redirect builds under the scope target.**
  Immutable checkouts (shared across projects) and vendor copies must never
  gain a `target/` tree, so locked git units build into
  `<scope>/target/<prof>/locked/<dep>/` (own objects, state, archives).
  Path deps keep building in their own `target/` (M2 behavior — user-owned
  dirs). Archive/include lookups follow the same rule via
  `Unit.target_dir_override`.
- **D38: `update` locks the world (`--all-features`) and is per-seed merged.**
  Workspace members resolve independently and merge (same name must agree;
  features union) so one member's optional never starves another's lock.
  `update -p` restores non-selected entries (branches elsewhere don't move);
  `--precise` keeps the manifest ref and overrides SHA+tree (verified via
  `tree_hash`, so unknown SHAs fail). Moved tags abort the update before any
  write; `head_count_between` messages are best-effort (`(? commits)` offline).
- **D39: Patch = local paths only; builds substitute, update resolves base.**
  The overlay order per edge is own-table patch → root patch → lock. Patch
  entries with non-path sources are an error. `update` strips patched keys
  before resolving (with a warning) and keeps their old lock entries; nested
  (non-seed) patches are not traversed at update time — documented
  limitation until M5 source replacement. `dev` clones `--local`,
  auto-patches, and warns about `publish` (which itself warns when patch is
  active).
- **D40: `vendor` = full copies + `rivet-vendor.json`, consumed by tree hash.**
  Git checkouts are copied with `.git` (tree verification needs objects);
  path deps skip `target/`. Builds prefer the cache and fall back to vendor
  only on cache-miss failures, accepting a vendored tree iff its
  `HEAD^{tree}` equals the locked tree — content-addressed, so a mutated
  vendor fails loudly (`TreeMismatch` semantics) instead of silently.
  Registry packages cannot vend in M4 (fetching is M5) — actionable error.

## M5

- **D41: Registry transport = `ureq` 2.12 (`default-features = false`, `tls`).**
  Minimal sync rustls client with bundled Mozilla roots and always-on cert
  verification (no insecure knob, no OpenSSL — satisfies AGENTS.md
  `ureq`/rustls-no-OpenSSL). One 120 s-timeout agent built per request
  (no global state, no connection pool). Bodies capped (`MAX_INDEX_BYTES`
  8 MiB, `MAX_TARBALL_BYTES` 256 MiB). `file://` reads local disk
  (allowed even offline — it is not network); `https://` refused under
  `offline`; all other schemes rejected without touching the network.
  Anything heavier (`reqwest`/tokio/`curl`) is unjustified for index +
  tarball GETs and one publish PUT.
- **D42: Sparse index URL layout `<registry-url>/index/<name>`, tarballs `<registry-url>/tarballs/<name>-<version>.tar.gz`.**
  Local-dir `<index-dir>/<name>` stays primary (M4-compatible JSON-lines
  `{"version","checksum","yanked","deps"}`, extended with optional
  `"description"`; unknown fields ignored both directions). `file://`
  bases map to `<root>/index/<name>` and `<root>/tarballs/…` on disk so
  the sparse layout is hermetically testable. Package names gated to
  `[A-Za-z0-9_-]+` so index/tarball lookups can never escape. Lock
  sentinel `"default"` is never a URL — `sparse()`/`tarball_url_for()` /
  `materialize()` reject it actionably; the CLI resolves it to a concrete
  `https://…` URL first.
- **D43: Tarball layout + determinism rules.**
  Cache: `<cache>/registry/<registry-hash>/<name>-<version>/` where hash
  = first 16 hex of SHA-256 over the normalized registry URL (full URL
  recorded in `.rivet-source-url`; reuse keyed by `.rivet-extract-ok`
  holding the verified checksum). Source root = the single top-level
  dir when the package dir holds exactly one dir (what `package_source_dir`
  emits: `<name>-<version>/…`), else the package dir itself. Packaging:
  entries sorted by archive path, file `0644` / dir `0755`, uid/gid 0,
  mtime 0, gzip level 6; any `target` or `.git` component excluded;
  symlinks rejected at package time. Same dir packaged twice = identical
  bytes. Extraction rejects absolute/`..`/empty paths, non-file/dir/link
  types, and absolute/escaping links, then re-validates the tree
  (same discipline as `rivet-git` checkout validation).
- **D44: Credentials at `~/.rivet/credentials` (JSON map URL → token).**
  `login` creates parent dirs, pretty-prints + trailing newline,
  overwrites per-URL. Unix chmods `0600` best-effort (warn, never fail);
  Windows has no std chmod so login warns that the store inherits
  directory ACLs. Missing file = `None` (never logged in); corrupt JSON
  = actionable `Login` error (`delete + rivet login again`). Publish
  wire format: `PUT <base>/publish/<name>/<version>`, body = gzip tarball,
  `Content-Type: application/gzip`, `Authorization: Bearer <token>`,
  `X-Rivet-Checksum: sha256:…`, optional `X-Rivet-Description` /
  `X-Rivet-Deps` JSON. Publishing requires `https://` (never `file://` /
  `http://` — tokens must not travel in the clear) and a non-empty token
  (`AuthMissing` → `rivet login`).
- **D45: New deps justified (`tar` 0.4, `flate2` 1.0, `ureq` 2.12).**
  `tar` + `flate2` = gzip-tar pack/extract (AGENTS.md pre-approved;
  hand-rolling tar/gzip is a bug farm). `ureq` = minimal rustls HTTPS
  (see D41; only candidate that is sync, OpenSSL-free, and small).
  Reused (already pinned in workspace): `semver` 1.0, `sha2` 0.10,
  `serde` / `serde_json`, `walkdir` 2.5, `thiserror` 2.0 only,
  `tempfile` 3.0 dev-only. MSRV stays 1.74.

## M6

- **D46: CMake invocation is `cmake -S src -B build -DCMAKE_BUILD_TYPE=<type> -DCMAKE_INSTALL_PREFIX=<build>/install [-DCMAKE_CXX_COMPILER=<tc.cxx> [-DCMAKE_C_COMPILER=<sibling>]] [-Dk=v …]` then `cmake --build <build> --config <type> [--target t …] [--parallel N]`.**
  `--config` is always passed (required by multi-config generators like Visual Studio, ignored by single-config ones); one `--target` flag per requested target. `options` become `-Dk=v` with a user-supplied `-D` prefix tolerated, never doubled. The toolchain's C++ driver is forwarded as `CMAKE_CXX_COMPILER` unless the spec sets it explicitly (explicit wins, forwarded exactly once); a sibling C compiler (`clang++`→`clang`, `*g++*`→`*gcc*`, `cl`→itself) is forwarded only when the file exists, else CMake's own discovery runs. Toolchain `vcvars` env overlays every adapter subprocess. Only the `cmake` binary executes — nothing else from the foreign source. `cmake` 3.13+ required (`-S`/`-B`); 3.16+ recommended.
- **D47: Adapter output discovery prefers static and never silently substitutes.**
  Build-dir scan accepts `*.lib/*.a/*.so/*.dll/*.dylib` + versioned `.so.*`; link order is pure-static archives, then Windows import libs (same-stem `.dll` next door), then shared objects. Non-empty `targets` filter by normalized stem (strip `lib` prefix, extensions, `.so` versions; ASCII case-insensitive) and a filter matching nothing is `NoLibraries` — never "build everything instead". Include dirs are heuristic in fixed priority: `<src>/include`, then `<src>` itself only with top-level headers (covers zlib's root `zlib.h`), then `<build>` only with headers outside `CMakeFiles` (covers generated `zconf.h`). File symlinks are skipped (no double-report of versioned `.so`), directory symlinks never followed.
- **D48: System probing order is pkg-config-then-dir-search for `system`, strict pkg-config for `pkg-config`.**
  `probe_installed("system")` runs `pkg-config --print-errors --cflags/--libs <name>` when the binary exists and falls back to the well-known-dir search when pkg-config is absent *or* does not know the package; `pkg-config` tool never falls back (unknown package = `SystemNotFound` naming the package + `PKG_CONFIG_PATH` hint). The only code ever executed is `pkg-config` itself. Dir search matches `<name>.h` or a `<name>/` subdir with headers in include roots, and `lib<name>.a/.so/.dylib`, `<name>.lib`, `lib<name>.lib`, `<name>.dll`, `lib<name>.dll`, `lib<name>.so.*` in lib roots (top level only). Header-only and link-only partial hits are returned, not errors; total miss names the package, lists searched roots, and gives the `lib<name>-dev`/brew/vcpkg install hint. Roots are `INCLUDE`/`LIB` env first, then Unix `/usr/include|lib…`, `/usr/local/…`, `/opt/local|homebrew`, or Windows Strawberry/MSYS2/MinGW defaults.
- **D49: Skip-gating policy — missing optional tools skip, never fail.**
  Tests requiring `cmake`/`pkg-config`/`make`/`meson` check presence via the crate's own `tool_available` and return early with a notice when absent; pure-logic tests always run. Real-build tests use tempdir fixtures so they need no network and no system packages. `make` builds in-source by nature, so the integrator hands it a writable scratch copy for immutable checkouts and fingerprints before building.
- **D50: New deps: none.**
  `rivet-adapters` adds no runtime dependencies (shells out to `cmake`/`meson`/`make`/`pkg-config`; PATH lookup and hashing are hand-rolled to avoid `which`/`sha2`). Dev-only `tempfile 3.0` for hermetic tests (already the workspace standard, D7). Schemaless foreign deps (no `rivet.json`) resolve via the inline `package` block (name must equal the dep key, version required); `system: true` locks a `system` source entry and probes at build time; `system` with `path`/`git` is a parse error (probe-only means sourceless).

## M7

- **D51: Build-script directive set + sandbox limits.**
  Convention: `<package-dir>/build.rivet.cpp`, auto-detected; absent = no
  script. Compiled with the detected toolchain at C++17 minimum with the
  package `include/` dir on the path; output `build-script[.exe]` under
  `target/<prof>/build/script`. Stdout directives, all `rivet:<key> <value>`
  (space or `=` separator): `include <dir>`, `lib <file|-lname>`
  (verbatim, in order), `libdir <dir>`, `define <K[=V]>`,
  `rerun-if-changed <path>`, `rerun-if-env <VAR>` (bare name). Relative
  paths resolve against the package dir; repeats dedupe first-wins (`-I`
  order is semantics). Unknown keys are an error naming the line number
  and content; non-`rivet:` stdout plus stderr pass to the build log.
  Rerun rule: script-bytes hash vs snapshot, content hashes of watched
  files (missing↔present counts), watched env values; outputs persist in
  `target/<prof>/incremental/build-script.json` so fresh builds never
  re-execute. Sandbox, stated honestly: the script runs as the current
  user with cwd = package dir, stdin null, and `*_TOKEN`/`*_KEY` env vars
  stripped; there is NO network or filesystem isolation on stock OSes, so
  only run scripts from sources you trust (same model as Cargo `build.rs`).
- **D52: Artifact cache key layout + invalidation rule.**
  Key = `sha256("package\\0version\\0<sorted features>\\0<toolchain
  fingerprint>\\0profile\\0<sorted flags>\\0<inputs hash>")`, 64 hex chars;
  the inputs hash covers sources only (headers verified post-lookup
  against the sidecar — a wipe-safe two-phase hit). Entry layout
  `<root>/artifacts/<key>/{files…, rivet-artifact.json}` plus a graph
  sidecar `build-state.json` (output mapping + TU/link state snapshot);
  corrupt entries quarantine + miss, never crash. Invalidation: any
  dimension change is a miss; header drift fails the sidecar verify and
  rebuilds precisely. Remote cache is out of scope (hook point documented
  in `rivet-cache`). The CLI prints `Cached` on restore hits.
- **D53: Target-triple support matrix.**
  Parser accepts `native` + `arch-vendor-os[-env]` with arch/OS/env
  aliases normalized. Native arch+OS (vendor/env ignored) → normal
  detection; anything else → `clang++ --target=<triple>` when clang is
  installed, else actionable `NoCrossToolchain`. The triple lives on
  `CxxToolchain.target`: driver flags append `--target=` (compile+link,
  never `ar`), and `fingerprint()` appends `|target=<triple>` only when
  set — native `None` keeps the historic format byte-for-byte, so native
  builds never rebuild spuriously.
- **D54: PCH + C++20-modules verdict: actionable errors, not implemented.**
  PCH needs ordered per-TU setup (`/Yc` creator + `/Yu` consumers) the
  uniform per-TU flag model cannot express; modules need per-toolchain
  dependency scanning first. Both surface typed errors prescribing plain
  `include/` + `#include`. No manifest surface exists for either (nothing
  to request them with), so the errors are currently unreachable from
  builds — the APIs wait for a future manifest story.
- **D55: New deps justified: `sha2 0.10` in `rivet-buildscript` +
  `rivet-cache` (nothing else).**
  `sha2` was already in the workspace (via `rivet-graph`) and pinned in
  `Cargo.lock`; adding it to these two crates reuses the audited primitive
  for script-hashing, inputs-hashing, and key derivation instead of a
  second hasher. No `hex` crate (formatting via `format!("{:x}")`), no
  network / archive / git crates. `tempfile 3.0` remains dev-only.
- **D56: Mirrors are full clones; checkouts verify materialization (amends D30).**
  Partial mirrors (`blob:none`) plus `clone --local` checkouts silently
  produce file-less trees on common git builds (observed: checkout exits 0
  with every file `D`; partial fetches never backfill because
  `partialclonefilter` persists). So mirrors clone full, and every checkout
  verifies `git status --porcelain` clean: reuse of a dirty checkout heals
  by rebuild, fresh checkouts repair once via mirror re-clone, else hard
  error. The bandwidth savings were not worth silent corruption.
- **D57: Bare `.dll` files never link; linker flags go after `/link`.**
  MSVC cannot link a `.dll` directly (LNK1107) — only import `.lib`s link,
  so discovery drops bare DLLs (their dirs still surface for runtime
  lookup). Separately, `/LIBPATH` and bare lib names are linker inputs
  and must follow `/link` on the `cl` line (`D9002`/`D9024` otherwise).
  Both found wiring the real-zlib demo.
