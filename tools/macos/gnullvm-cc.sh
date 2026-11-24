#!/bin/sh
# Linker for x86_64-pc-windows-gnullvm on macOS. That target keeps Rust's
# thread-locals in static TLS as the MSVC target does, which VIctory's loader
# relies on. Homebrew has no llvm-mingw, so this drives Homebrew clang and lld
# against the Homebrew mingw-w64 runtime, and MinGW's libgcc_eh stands in for
# LLVM's libunwind (both export the same _Unwind_* functions).
M="${MINGW_PREFIX:-/opt/homebrew/opt/mingw-w64/toolchain-x86_64}"
GCC_LIB="$(ls -d "$M"/lib/gcc/x86_64-w64-mingw32/* | tail -n 1)"
LLVM="${LLVM_PREFIX:-/opt/homebrew/opt/llvm}"
LLD="${LLD_PREFIX:-/opt/homebrew/opt/lld}"
args=""
for a in "$@"; do
    case "$a" in
        -lunwind) set -- "$@" -lgcc_eh -lgcc ;;
        *) set -- "$@" "$a" ;;
    esac
    shift
done
exec "$LLVM/bin/clang" --target=x86_64-w64-windows-gnu -Qunused-arguments \
    --sysroot="$M/x86_64-w64-mingw32" -fuse-ld="$LLD/bin/ld.lld" \
    -L"$GCC_LIB" -L"$M/x86_64-w64-mingw32/lib" "$@"
