#!/bin/sh
# Builds VIctory for Windows (gnullvm) on macOS and runs its tests under Wine.
# Usage: tools/macos/test-wine.sh [cargo test arguments]
# WINE names the wine binary. Cargo splits the runner on spaces, so the
# runner is a wrapper script in a temporary folder without spaces.
set -e
cd "$(dirname "$0")/../.."
ROOT="$PWD"
WINE="${WINE:-$(command -v wine || true)}"
[ -x "$WINE" ] || { echo "set WINE to a wine binary" >&2; exit 1; }
TARGET=x86_64-pc-windows-gnullvm
mkdir -p target/macos
# A wrapper rather than a symlink: some Wine builds find their loader next
# to the path they were started from.
LINK="$(mktemp -d /tmp/victory-wine.XXXXXX)/wine"
printf '#!/bin/sh\nexec "%s" "$@"\n' "$WINE" > "$LINK"
chmod +x "$LINK"
export WINEPREFIX="${WINEPREFIX:-$ROOT/target/wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUNNER="$LINK"
cargo build --quiet --manifest-path tests/fixtures/hle-probe/Cargo.toml \
    --target-dir target/fixtures --target "$TARGET"
VICTORY_HLE_PROBE="Z:$(printf '%s' "$ROOT" | tr / '\\')\\target\\fixtures\\$TARGET\\debug\\hle_probe.dll"
export VICTORY_HLE_PROBE
cargo test --target "$TARGET" "$@"
