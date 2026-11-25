#!/bin/zsh
# Apple's newer command-line tools can be ARM-only. Run host tools natively
# even when configure runs under Rosetta; CC/CXX still target x86_64.
NATIVE_TOOLS="$HOME/Library/Caches/victory-macos/target/native-tools"
mkdir -p "$NATIVE_TOOLS"
for native_tool in clang clang++ cc c++ gcc g++ cpp ar as ld nm ranlib strip make libtool lipo otool install_name_tool dsymutil xcrun m4 c89 c99 yacc lex gperf; do
  [[ -x /usr/bin/$native_tool ]] || continue
  printf '#!/bin/sh\nexec /usr/bin/arch -arm64 /usr/bin/%s "$@"\n' "$native_tool" > "$NATIVE_TOOLS/$native_tool"
  chmod +x "$NATIVE_TOOLS/$native_tool"
done

# Flex otherwise invokes its compiled-in absolute /usr/bin/m4 path.
export M4="$NATIVE_TOOLS/m4"
