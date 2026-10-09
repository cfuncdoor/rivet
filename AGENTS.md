# AGENTS.md — rivet

Cargo-like build system + package manager for C++. The tool itself is Rust (stable, edition 2021+), built with Cargo. Single static binary `rivet`, no bootstrap problem.

## Workspace layout (Cargo workspace, small crates)

- `crates/rivet-manifest`, `rivet-resolver`, `rivet-git`, `rivet-toolchain`, `rivet-graph`, `rivet-cache`, `rivet-registry`, `rivet-cli`
- Keep crates independently testable. Typed + documented public APIs.
- `examples/` = C++ corpus rivet must build in CI (hello, lib+bin workspace, git dep, CMake-only dep e.g. zlib, 200-file synthetic). `docs/`, `ARCHITECTURE.md`, `PLAN.md`, `DECISIONS.md` at root.
- Current phase: **M1** — manifest + CLI skeleton + toolchain detection. `rivet new hello && rivet build && rivet run` must work for single-file bin on Windows + Linux. Do not start M2+ until M1 acceptance passes.

## Build / test / verify

- `cargo test --workspace` — every crate has unit tests; integration tests run real C++ compilers in temp dirs (no network — use local bare repos in tempdirs for git tests).
- Focused: `cargo test -p <crate> [test_name] -- --nocapture`
- Gate before milestone done: `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings` must pass, CI green.
- Release profile: `opt-level="z"` or `3` + `lto=true`, `codegen-units=1`, `panic="abort"`, `strip=true`. Targets: `x86_64-pc-windows-msvc` (+crt-static), `x86_64-unknown-linux-musl`.

## Manifest / lockfile (JSONC — no TOML anywhere)

- Manifest `rivet.json` is JSONC (`//`, `/* */`, trailing commas). Strip via small lex pass before `serde_json`, preserving line/column. Errors must report `line:column` + JSON path (e.g. `dependencies.fmt.git`).
- Lock `rivet.lock.json`: strict JSON, sorted keys, stable formatting. Ship + reference `rivet.schema.json` via `$schema`.
- Conventions (zero config): `src/main.cpp`→bin, `src/lib.cpp` or `include/`→lib, `tests/*.cpp`→tests, `examples/`, `benches/`. Public `include/`, private `src/`. Output `target/<profile>/{deps,build,incremental}`. Cache `~/.rivet/`.

## Hard implementation constraints

- Deps: small + pinned (`serde`+`serde_json`, `semver`, `sha2`, `clap` or `lexopt`, `anyhow`/`thiserror`, `walkdir`, `tar`+`flate2`, `ureq`/rustls-no-OpenSSL, `which`, `tempfile` for tests). Commit `Cargo.lock`, run `cargo deny`/`audit` in CI. Justify every non-trivial crate in `DECISIONS.md`.
- `thiserror` in libraries, `anyhow` only in `rivet-cli`. `Result` everywhere; no `unwrap`/`expect` outside tests.
- `Path`/`PathBuf` + `OsString` for fs/process args (Windows paths aren't UTF-8). No global mutable state.
- No `unsafe` except small documented platform module (vswhere/registry/process) with `// SAFETY:` per block.
- Runtime deps: only system `git` binary. No CMake/Make/Ninja/Python/Conan (CMake only as M6 adapter for foreign packages). Deterministic builds: same inputs → same outputs.
- Security: always verify checksums; no code exec at resolve time (build scripts `build.rivet.cpp` are the only exec point); reject path traversal + escaping symlinks in archives/tarballs/checkouts; pin HTTPS; no telemetry.
- Errors: actionable (what/where/fix). Never silently fall back; log decision at `-v`. Compiler errors pass through unmodified. CLI mimics Cargo (`Compiling x v1.0`, `Finished dev in 2.3s`); respect `NO_COLOR`, detect TTY; support `--message-format=json`, `-v`.

## Git subsystem (`rivet-git`) — do not deviate without DECISIONS.md entry

- Shell out to system `git` via `std::process::Command`. Never `git2`/`gitoxide` (no libgit2/OpenSSL). Honors SSH/credential helpers/`~/.gitconfig`. Fail with actionable message if `git` missing. Set `GIT_TERMINAL_PROMPT=0` unless `--interactive`; never prompt mid-build.
- Layout under `~/.rivet/git/`: `db/<name>-<urlhash>/` (one bare mirror per URL), `checkouts/<name>-<urlhash>/<short-sha>/` (immutable, worktree/archive export, never edited). Prefer shallow/partial (`--filter=blob:none`, fetch by SHA/tag), fallback full.
- Lock per git dep: URL + requested ref + full SHA + tree hash; builds use locked SHA. Branches move only on `rivet update`. Moved tag = error. Verify tree hash after checkout. `tag`/`branch`/`rev` mutually exclusive. Disable hooks at checkout (`core.hooksPath` → empty dir). Only `https`/`ssh`/`file` URLs (submodules same).
- Flags: `--locked` (error if lock changes), `--frozen` (+no network), `--offline` (cache only). `rivet dev <name>` → writable clone in `./deps-dev/` + auto `patch`; warn on patch during `publish`.

## Process rules

- Maintain `ARCHITECTURE.md`, `PLAN.md` (milestone checklist), `DECISIONS.md` as you go. Ambiguous choice → simpler Cargo-like option, record in `DECISIONS.md`, continue. Ask nothing unless truly blocked.
- Small commits, conventional messages. One milestone = one branch/PR series; merge only when acceptance tests pass. Write tests as you go. End of milestone: print what works / stubbed / next.
- Toolchain abstraction (MSVC via vswhere/vcvars, clang, gcc, clang-cl, MinGW) normalizes opt/debug/lto/sanitizers/std/defines/includes/pic/warnings; ABI/compiler/std/stdlib are part of fingerprint. Header tracking via depfiles (`-MMD/-MF`, `/showIncludes`), content-hash rebuilds, parallel jobs=cores.
