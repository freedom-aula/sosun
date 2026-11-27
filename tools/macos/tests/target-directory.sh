#!/bin/bash
# Game-free regressions using real Cargo configuration and stock macOS Bash 3.2.
# Run: /bin/bash tools/macos/tests/target-directory.sh
set -euo pipefail
scripts=$(cd "$(dirname "$0")/.." && pwd)
sysroot=$(rustc --print sysroot)
real_cargo="$sysroot/bin/cargo"
root=$(mktemp -d "${TMPDIR:-/tmp}/victory scripts.XXXXXX")
root=$(cd "$root" && pwd -P)
trap 'rm -rf "$root"' EXIT

fail() { echo "FAIL: $*" >&2; cat "$root/output" >&2; exit 1; }
contains() { grep -F -- "$1" "$root/output" >/dev/null || fail "missing diagnostic: $1"; }

# Each test runs in a fresh subshell and fresh workspace. Do not read or change
# the user's Cargo config, compile anything, or invoke a real victory binary.
prepare() {
  local variable
  for variable in $(compgen -e); do
    case "$variable" in CARGO_*|RUST*|VICTORY_*) unset "$variable" ;; esac
  done
  repo="$root/$1/workspace with spaces"
  mkdir -p "$repo/tools/macos" "$repo/src" "$root/$1/home/.cargo" "$root/$1/bin"
  cp "$scripts/"*.sh "$repo/tools/macos/"
  export HOME="$root/$1/home" CARGO_HOME="$root/$1/home/.cargo"
  export PATH="$root/$1/bin:$sysroot/bin:/usr/bin:/bin:/usr/sbin:/sbin"
  export RECORD="$root/$1/invocation"
  bin_dir="$root/$1/bin"
  default="$repo/target"
  shared="$root/$1/shared \"quoted\" target"
  input="$root/$1/harmless input.txt"
  printf 'not a game\n' > "$input"
  printf '[package]\nname = "fixture"\nversion = "0.1.0"\nedition = "2021"\n[workspace]\n' > "$repo/Cargo.toml"
  printf 'fn main() {}\n' > "$repo/src/main.rs"
  cd "$root"
}

fixture() {
  mkdir -p "$1/x86_64-apple-darwin/release"
  cat > "$1/x86_64-apple-darwin/release/victory" <<'FIXTURE'
#!/bin/bash
printf '%s\n' "$0" "$@" > "$RECORD"
exit "${CHILD_STATUS:-0}"
FIXTURE
  chmod +x "$1/x86_64-apple-darwin/release/victory"
}

configure() {
  mkdir -p "$1"
  printf "[build]\ntarget-dir = '%s'\n" "$2" > "$1/config.toml"
}

run_script() {
  rc=0
  /bin/bash "$repo/tools/macos/$1" "${@:2}" > "$root/output" 2>&1 || rc=$?
}

launch() {
  local target="$1"
  shift
  if [[ "$target" != "$default" ]]; then fixture "$default"; fi
  fixture "$target"
  run_script run-native.sh --console series-x "$@" "$input"
  [[ "$rc" == "${CHILD_STATUS:-0}" ]] || fail "child status: $rc"
  printf '%s\n' "$target/x86_64-apple-darwin/release/victory" run --execute --console series-x "$@" --unimplemented zero "$input" > "$root/expected"
  cmp "$RECORD" "$root/expected" || fail "wrong executable or arguments"
}

reject_launch() {
  fixture "$default" # A stale default must never conceal an error.
  run_script run-native.sh --console series-s "$input"
  [[ "$rc" != 0 && ! -e "$RECORD" ]] || fail "launched despite resolution failure"
}

test_default() { launch "$default"; }
test_invalid_run_options() {
  printf 'native_binary() { touch "$RECORD"; return 1; }\n' > "$repo/tools/macos/native-binary.sh"
  local value option
  for value in abc -1 '' 18446744073709551616; do
    run_script run-native.sh --seconds "$value" "$input"
    [[ "$rc" == 2 ]] || fail "expected status 2 for --seconds=$value, got $rc"
    contains '--seconds '
  done
  for option in --seconds --console; do
    run_script run-native.sh "$option"
    [[ "$rc" == 2 ]] || fail "expected status 2 for missing $option value, got $rc"
    contains "$option"
  done
  [[ ! -e "$RECORD" && ! -e "$repo/runs" ]] || fail "invalid option reached launcher setup"
}
test_decimal_seconds() {
  fixture "$default"
  # Give the timer a bounded child to stop and record its requested duration.
  printf '#!/bin/bash\nexec /bin/sleep 2\n' > "$default/x86_64-apple-darwin/release/victory"
  printf '#!/bin/bash\necho "$1" > "$RECORD"\n' > "$bin_dir/sleep"
  chmod +x "$bin_dir/sleep"
  run_script run-native.sh --seconds 08 "$input"
  [[ "$rc" == 143 && -f "$RECORD" && $(cat "$RECORD") == 8 ]] || fail "expected an 8-second timer"
  run_script run-native.sh --seconds=09 "$input"
  [[ "$rc" == 143 && $(cat "$RECORD") == 9 ]] || fail "expected a 9-second timer"
}
test_same_second_logs() {
  printf '#!/bin/bash\necho 20260101-120000\n' > "$bin_dir/date"
  chmod +x "$bin_dir/date"
  launch "$default"
  local first=("$repo"/runs/*/victory.log)
  echo 'first run marker' >> "${first[0]}"
  launch "$default"
  local logs=("$repo"/runs/*/victory.log)
  [[ ${#logs[@]} == 2 ]] || fail "two runs in the same second shared a log"
  grep -Fxq 'first run marker' "${first[0]}" || fail "first log was overwritten"
}
test_global() { configure "$CARGO_HOME" "$shared"; launch "$shared"; }
test_home_global() { configure "$CARGO_HOME" "$shared"; unset CARGO_HOME; launch "$shared"; }
test_global_without_default() {
  configure "$CARGO_HOME" "$shared"
  fixture "$shared"
  run_script run-native.sh --console series-s "$input"
  [[ "$rc" == 0 && -f "$RECORD" ]] || fail "configured executable not launched"
}
test_environment() {
  configure "$CARGO_HOME" "$root/global decoy"
  configure "$repo/.cargo" 'project decoy'
  export CARGO_TARGET_DIR="$shared" CHILD_STATUS=37
  launch "$shared" --headless
}
test_build_environment() { export CARGO_BUILD_TARGET_DIR="$shared"; launch "$shared"; }
test_relative_environment() { export CARGO_TARGET_DIR='relative target'; launch "$repo/relative target"; }
test_project() {
  configure "$CARGO_HOME" "$shared"
  configure "$repo/.cargo" 'relative target'
  launch "$repo/relative target"
}
test_missing() {
  configure "$CARGO_HOME" "$shared"
  reject_launch
  contains "$shared/x86_64-apple-darwin/release/victory"
  contains 'native-setup.sh'
}
test_not_executable() {
  configure "$CARGO_HOME" "$shared"
  fixture "$shared"
  chmod -x "$shared/x86_64-apple-darwin/release/victory"
  reject_launch
  contains "$shared/x86_64-apple-darwin/release/victory"
  contains 'native-setup.sh'
}
test_invalid_config() {
  printf '[build\n' > "$CARGO_HOME/config.toml"
  reject_launch
  contains Cargo
  contains config.toml
}
test_bad_metadata() {
  local body
  for body in 'exit 19' 'echo invalid-json' \
    "echo '{\"target_directory\": \"\"}'" \
    "echo '{\"target_directory\": 42}'" \
    "echo '{\"target_directory\": \"/tmp\"}'; exit 19" \
    "echo '{\"target_directory\": \"relative\"}'"; do
    printf '#!/bin/bash\n%s\n' "$body" > "$bin_dir/cargo"
    chmod +x "$bin_dir/cargo"
    reject_launch
    contains Cargo
  done
}

prepare_setup() {
  # Stub prerequisites and the expensive build, but keep real Cargo metadata.
  export TEST_SYSROOT="$root/fake toolchain" REAL_CARGO="$real_cargo"
  mkdir -p "$TEST_SYSROOT/bin"
  cat > "$bin_dir/rustup" <<'STUB'
#!/bin/bash
if [[ "$1" == run ]]; then echo "$TEST_SYSROOT"; fi
STUB
  cat > "$bin_dir/uname" <<'STUB'
#!/bin/bash
if [[ "$1" == -m ]]; then echo arm64; else echo Darwin; fi
STUB
  printf '#!/bin/bash\nexit 0\n' > "$bin_dir/arch"
  cp "$bin_dir/arch" "$bin_dir/xcode-select"
  cat > "$TEST_SYSROOT/bin/cargo" <<'STUB'
#!/bin/bash
if [[ "$1" == build ]]; then
  printf '%s\n' "$@" > "$RECORD"
  exit "${BUILD_STATUS:-0}"
fi
exec "$REAL_CARGO" "$@"
STUB
  chmod +x "$bin_dir/"* "$TEST_SYSROOT/bin/cargo"
}
test_setup() {
  prepare_setup
  configure "$CARGO_HOME" "$shared"
  run_script native-setup.sh
  [[ "$rc" == 0 ]] || fail "setup failed"
  contains "built $shared/x86_64-apple-darwin/release/victory"
  printf '%s\n' build --release --target x86_64-apple-darwin -p victory > "$root/expected"
  cmp "$RECORD" "$root/expected" || fail "build arguments changed"
  launch "$shared"
}
test_setup_sysroot_failure() {
  prepare_setup
  printf '#!/bin/bash\nif [[ "$1" == run ]]; then exit 47; fi\n' > "$bin_dir/rustup"
  cp "$TEST_SYSROOT/bin/cargo" "$bin_dir/cargo"
  run_script native-setup.sh
  [[ "$rc" == 47 && ! -e "$RECORD" ]] || fail "setup continued after sysroot lookup failed (status $rc)"
}
test_setup_metadata_failure() {
  prepare_setup
  printf '[build\n' > "$CARGO_HOME/config.toml"
  run_script native-setup.sh
  [[ "$rc" != 0 && ! -e "$RECORD" ]] || fail "build ran despite invalid config"
  contains Cargo
}
test_setup_build_failure() {
  prepare_setup
  export BUILD_STATUS=41
  run_script native-setup.sh
  [[ "$rc" == 41 ]] || fail "setup concealed build failure"
  if grep -F 'built ' "$root/output"; then fail "setup claimed success"; fi
}

# rustup is installed but not on PATH: right after the rustup.rs installer, or
# Homebrew's keg-only formula. A fake Homebrew prefix keeps a real one out.
rustup_off_path() {
  export HOMEBREW_PREFIX="$HOME/homebrew"
  mkdir -p "$1"
  if [[ -e "$bin_dir/rustup" ]]; then mv "$bin_dir/rustup" "$1/rustup"; fi
}
setup_with_rustup_in() {
  prepare_setup
  rustup_off_path "$1"
  configure "$CARGO_HOME" "$shared"
  run_script native-setup.sh
  [[ "$rc" == 0 ]] || fail "setup failed"
  contains "rustup is in $1 but not on your PATH"
  contains "built $shared/x86_64-apple-darwin/release/victory"
}
test_setup_rustup_in_cargo_home() { setup_with_rustup_in "$CARGO_HOME/bin"; }
test_setup_rustup_in_homebrew() { setup_with_rustup_in "$HOME/homebrew/opt/rustup/bin"; }
test_setup_without_rustup() {
  prepare_setup
  rustup_off_path "$HOME/empty"
  rm "$HOME/empty/rustup"
  run_script native-setup.sh
  [[ "$rc" != 0 && ! -e "$RECORD" ]] || fail "setup went on without rustup"
  contains 'rustup is missing'
}
# The launcher finds rustup's cargo when nothing else puts cargo on PATH.
test_launch_rustup_in_cargo_home() {
  export PATH="$bin_dir:/usr/bin:/bin:/usr/sbin:/sbin"
  rustup_off_path "$CARGO_HOME/bin"
  printf '#!/bin/bash\nexit 0\n' > "$CARGO_HOME/bin/rustup"
  printf '#!/bin/bash\nPATH="%s/bin:$PATH" exec "%s" "$@"\n' "$sysroot" "$real_cargo" > "$CARGO_HOME/bin/cargo"
  chmod +x "$CARGO_HOME/bin/"*
  launch "$default"
}
test_launch_without_cargo() {
  export PATH="$bin_dir:/usr/bin:/bin:/usr/sbin:/sbin"
  rustup_off_path "$HOME/empty"
  reject_launch
  contains 'Cargo is not on your PATH'
  contains 'https://rustup.rs'
}

# Optional names allow the central regression to be checked against old scripts.
if [[ $# == 0 ]]; then
  set -- default invalid_run_options decimal_seconds same_second_logs global home_global global_without_default environment \
    build_environment relative_environment project missing not_executable invalid_config \
    bad_metadata setup setup_sysroot_failure setup_metadata_failure setup_build_failure \
    setup_rustup_in_cargo_home setup_rustup_in_homebrew setup_without_rustup \
    launch_rustup_in_cargo_home launch_without_cargo
fi
for test in "$@"; do
  (prepare "$test"; "test_$test")
  echo "PASS: $test"
done
