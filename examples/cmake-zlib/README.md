# cmake-zlib — real CMake-only git dep (needs network)

A rivet app with a git dependency on zlib (`tag v1.3.1`) built via
`"adapter": {"tool": "cmake"}` with default targets: the adapter builds
zlib's default target and links the static archive first (static is
preferred when both variants are produced). Consumers get zlib's source
root (for `zlib.h`) plus the build dir (for generated `zconf.h`) on the
include path.

Needs network (clones github.com/madler/zlib); hermetic test suites skip
it. For an offline build, see `examples/cmake-mini`.
