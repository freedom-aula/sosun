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

# Optional names allow the central regression to be checked against old scripts.
if [[ $# == 0 ]]; then
  set -- default global home_global global_without_default environment \
    build_environment relative_environment project missing not_executable invalid_config \
    bad_metadata setup setup_metadata_failure setup_build_failure
fi
for test in "$@"; do
  (prepare "$test"; "test_$test")
  echo "PASS: $test"
done
