#!/usr/bin/env bash
# Generate a synthetic N-TU benchmark corpus (default 200): each TU sums a
# vector (unique function `tuNNN`), `main` sums them all. Emits both a
# rivet package (`rivet.json`) and a CMake project (`CMakeLists.txt`) over
# the SAME sources so `rivet build` and CMake+Ninja compare apples to
# apples. Usage: gen.sh <out-dir> [count]
set -euo pipefail

out="${1:?usage: gen.sh <out-dir> [count]}"
count="${2:-200}"
mkdir -p "$out/src"

i=0
decls=""
calls=""
srcs=""
while [ "$i" -lt "$count" ]; do
    n=$(printf '%03d' "$i")
    cat > "$out/src/tu$n.cpp" <<EOF
#include <vector>
int tu$n() { std::vector<int> v(100, $i); int s = 0; for (int x : v) s += x; return s; }
EOF
    decls="${decls}int tu$n();
"
    calls="${calls}  s += tu$n();
"
    srcs="${srcs}  src/tu$n.cpp
"
    i=$((i + 1))
done

cat > "$out/src/main.cpp" <<EOF
#include <cstdio>
${decls}int main() {
  long s = 0;
${calls}  printf("synth%ld\n", s);
  return 0;
}
EOF

cat > "$out/rivet.json" <<'EOF'
{"package": {"name": "synth", "version": "0.1.0"}}
EOF

{
    echo 'cmake_minimum_required(VERSION 3.16)'
    echo 'project(synth CXX)'
    echo 'add_executable(synth'
    printf '%s' "$srcs"
    echo '  src/main.cpp)'
} > "$out/CMakeLists.txt"

echo "generated $count TUs in $out"
