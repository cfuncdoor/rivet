# Builds

## Freshness model

Rebuilds are **content-hash** based, never mtime based: touching a
file without changing it stays `Fresh`; changing a header rebuilds
exactly the TUs that include it (depfiles: `-MMD -MF` on GCC/Clang,
`/showIncludes` on MSVC). Per-build hashing is memoized (shared
headers hash once) and fresh targets reuse their link digests, so a
200-file no-op is well under a second. The toolchain (compiler, std,
opt/debug/lto/sanitizers, features, `--target`) is part of the
fingerprint — toggling any of it rebuilds.

## Profiles and features

`profile.dev` / `profile.release` carry `opt`, `debug`, `lto`, and
`sanitizers`; `rivet build --release` selects the latter. Features
(`features` table + `optional` deps, C1) are activated by `--features`,
`--all-features`, or suppressed with `--no-default-features`, and join
the fingerprint.

## Build scripts

A `build.rivet.cpp` beside the manifest compiles with the package
toolchain and runs **before** the build — the only code-execution
point at build time (resolution never executes anything). It emits
`rivet:` directives — `include`, `lib`, `libdir`, `define`,
`rerun-if-changed`, `rerun-if-env` — which feed the compile/link and
are persisted so fresh builds never re-execute (D51).

## Foreign packages (adapters)

Non-rivet sources build through adapters — `cmake`, `meson`, `make`,
`sources`, `pkg-config`, or `system` probes — into scratch dirs (never
into immutable checkouts), gated by source+spec+toolchain fingerprints.
The `sources` adapter compiles a schemaless tree's top-level C++
sources with the rivet toolchain into a static archive (subdirectories
are ignored; flags follow the profile like native builds).
Adapter outputs (includes, archives, bare names, defines) feed
dependents like native targets. Bare `.dll`s never link (only import
libs do); on MSVC, linker inputs follow `/link` (D57).

## Caching and targets

Whole-package artifacts cache content-keyed under
`~/.rivet/artifacts` and restore with a `Cached` note (target-wipe
safe, D52). Native toolchain detection caches in
`~/.rivet/toolchain.json`, validated by driver existence plus a live
version probe (D58). `--target TRIPLE` cross-compiles via
`clang --target=` (native `ar` still archives); the triple is recorded
in the fingerprint.

## Environment

`RIVET_CXX` pins the compiler driver (path or `PATH` name — `cl`,
`clang++`, `g++`, …; fails actionably, never falls back).
`RIVET_HOME` overrides the cache root. `RIVET_NO_TOOLCHAIN_CACHE`
(non-empty) skips detection caching.

## Troubleshooting

- `no C++ toolchain found`: install VS Build Tools / `build-essential`
  / `clang`; `rivet toolchains -v` shows every candidate tried.
- `RIVET_CXX=…: not a file…`: fix the path/name or unset it.
- Link errors after switching toolchains/flags: fingerprints normally
  cover this — `rivet clean` resets to a known state.
- Network-gated failures under `--offline`/`--frozen`: `vendor` the
  sources or drop the flag; git auth issues surface from `git` itself
  (credential helpers work: rivet never intercepts them).
