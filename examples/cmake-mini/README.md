# cmake-mini — hermetic CMake-adapter demo (offline)

A rivet app (`cmake-mini-app`) with a `path` dependency on `mini/`, a
vendored 3-file CMake-only library (`CMakeLists.txt`, `include/mini/mini.h`,
`src/mini.cpp`) built via `"adapter": {"tool": "cmake"}`.

The adapter configures `mini/` with
`cmake -S mini -B <build> -DCMAKE_BUILD_TYPE=… -DBUILD_SHARED_LIBS=OFF`,
builds `--target mini`, and exposes `mini/include` plus the produced
static archive to the app build. Everything is vendored, so this demo
builds offline.
