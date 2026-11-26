#!/bin/bash
# Runs the game natively and keeps a log you can attach to a tester report.
# Usage: tools/macos/run-native.sh [--headless] [--seconds N] <game folder or .exe>
#   --headless   no window; frames go to an offscreen layer
#   --seconds N  stop the run after N seconds (default: run until you close it)
# Extra VICTORY_* variables in your environment are passed through
# (see docs/native-host.md, "Bring-up diagnostics").
set -uo pipefail
cd "$(dirname "$0")/../.."

headless=()
seconds=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --headless) headless=(--headless); shift ;;
    --seconds) seconds="$2"; shift 2 ;;
    -h|--help) sed -n 2,8p "$0"; exit 0 ;;
    *) break ;;
  esac
done
[[ $# -eq 1 ]] || { sed -n 2,8p "$0"; exit 2; }

target="$1"
if [[ -d "$target" ]]; then
  exe=$(find "$target" -maxdepth 1 -iname 'game_*master.exe' | head -1)
  [[ -n "$exe" ]] || { echo "No game_*master.exe found in $target"; exit 2; }
else
  exe="$target"
fi
[[ -f "$exe" ]] || { echo "Not a file: $exe"; exit 2; }

bin=target/x86_64-apple-darwin/release/victory
[[ -x "$bin" ]] || { echo "victory is not built yet: run tools/macos/native-setup.sh first"; exit 2; }

run_dir="runs/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$run_dir"
log="$run_dir/victory.log"
{
  echo "# $(sw_vers -productName) $(sw_vers -productVersion), $(sysctl -n machdep.cpu.brand_string), $(( $(sysctl -n hw.memsize) / 1073741824 )) GB"
  echo "# commit $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
} > "$log"

export VICTORY_GUESS_GRAPHICS="${VICTORY_GUESS_GRAPHICS:-1}"
# There is no live keyboard or gamepad yet: press Enter (the pad's A) every 2 s
# from 4 s to 80 s so the game gets past its "press A" start screen.
# shortcut: scripted presses, until real input reaches GameInput.
export VICTORY_KEYS="${VICTORY_KEYS:-$(seq 4 2 80 | sed 's/$/:Enter/' | paste -sd, -)}"
echo "Running $exe"
echo "Log: $log"

"$bin" run --execute "${headless[@]}" --unimplemented zero "$exe" >> "$log" 2>&1 &
pid=$!
if (( seconds > 0 )); then
  ( sleep "$seconds"; kill "$pid" 2>/dev/null ) &
fi
wait "$pid"
rc=$?

echo
echo "Exit code $rc. Last lines of the log:"
sed 's/\x1b\[[0-9;]*m//g' "$log" | tail -15 | cut -c1-200
echo
echo "To report: open a Tester report issue and attach $log"
