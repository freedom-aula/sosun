#!/bin/bash
# build-native-probe.sh names the cross tool it lacks before building anything.
# Run: /bin/bash tools/macos/tests/probe-prerequisites.sh
set -euo pipefail
script="$(cd "$(dirname "$0")/.." && pwd)/build-native-probe.sh"
root=$(mktemp -d "${TMPDIR:-/tmp}/victory probe.XXXXXX")
trap 'rm -rf "$root"' EXIT

# A stand-in for Homebrew's LLVM, lld and mingw-w64 without `missing`.
check() {
  local missing="$1" tools="$root/$2" rc=0 out
  mkdir -p "$tools/llvm/bin" "$tools/lld/bin" "$tools/mingw/x86_64-w64-mingw32"
  touch "$tools/llvm/bin/llvm-dlltool" "$tools/llvm/bin/clang" "$tools/lld/bin/ld.lld"
  rm -rf "${tools:?}/$missing"
  out=$(LLVM_PREFIX="$tools/llvm" LLD_PREFIX="$tools/lld" MINGW_PREFIX="$tools/mingw" \
    /bin/bash "$script" "$tools/out/probe.exe" 2>&1) || rc=$?
  if [ "$rc" -eq 0 ]; then
    echo "FAIL: without $missing the probe build succeeded" >&2
    exit 1
  fi
  case "$out" in
    *"$tools/$missing is missing"*"brew install llvm lld mingw-w64"*) ;;
    *) echo "FAIL: without $missing: $out" >&2; exit 1 ;;
  esac
  if [ -e "$tools/out" ]; then
    echo "FAIL: without $missing the build wrote $tools/out" >&2
    exit 1
  fi
}

check llvm/bin/llvm-dlltool dlltool
check llvm/bin/clang clang
check lld/bin/ld.lld lld
check mingw/x86_64-w64-mingw32 mingw
echo "probe prerequisites: ok"
