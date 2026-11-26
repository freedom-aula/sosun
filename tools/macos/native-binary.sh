# Shared by native-setup.sh and run-native.sh, from the workspace root.
# Cargo resolves environment overrides and local/global build.target-dir config.
# plutil is part of macOS and decodes JSON strings without losing path quoting.
native_binary() {
  local target_dir
  if ! target_dir=$(set -o pipefail
    cargo metadata --format-version 1 --no-deps --offline |
      /usr/bin/plutil -extract target_directory raw -expect string -o - -
  ); then
    echo "Cannot resolve Cargo's target directory. Check the Cargo error above and your Cargo configuration/toolchain." >&2
    return 1
  fi
  if [[ -z "$target_dir" || "$target_dir" != /* ]]; then
    echo "Cargo metadata did not provide an absolute target directory." >&2
    return 1
  fi
  printf '%s/x86_64-apple-darwin/release/victory\n' "$target_dir"
}
