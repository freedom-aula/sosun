#!/bin/sh
# Builds the native host's stand-in title (tests/fixtures/native-probe) with
# the same clang and lld as the gnullvm cross build (gnullvm-cc.sh).
# Usage: tools/macos/build-native-probe.sh [output.exe]
# The default output is target/fixtures/native-probe.exe. Prints the path.
set -e
cd "$(dirname "$0")/../.."
SRC=tests/fixtures/native-probe
OUT="${1:-target/fixtures/native-probe.exe}"
LLVM="${LLVM_PREFIX:-/opt/homebrew/opt/llvm}"
WORK="$(dirname "$OUT")/native-probe-libs"
mkdir -p "$WORK"
for def in xgameruntime victory-probe-host; do
    "$LLVM/bin/llvm-dlltool" -m i386:x86-64 -d "$SRC/$def.def" -l "$WORK/lib$def.a"
done
# No CRT: the probe's entry point is probe_main, which the host calls itself.
# --dynamicbase gives it base relocations; the preferred base is above 4 GiB
# like a console title's.
tools/macos/gnullvm-cc.sh -O1 -ffreestanding -fno-builtin -fno-stack-protector \
    -fno-asynchronous-unwind-tables -fms-extensions -fno-emulated-tls -nostdlib \
    -Wl,--entry,probe_main -Wl,--subsystem,console -Wl,--dynamicbase \
    -Wl,--image-base,0x140000000 -Wl,--export-all-symbols \
    -L"$WORK" "$SRC/probe.c" -lxgameruntime -lvictory-probe-host -lkernel32 -o "$OUT"
echo "$OUT"
